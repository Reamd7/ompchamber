/**
 * FS 路由模块：向 Express app 注册 /api/fs/* 文件系统 HTTP 端点。
 *
 * 核心职责：
 * - 工作区路径围栏：路径先经 resolveWorkspacePathFromContext 系列校验，必须落在
 *   活动项目目录（含 worktree 与 symlink 原始路径空间）或用户配置根内，越界一律拒绝；
 *   工作区外文件需持有一把短期一次性 grant token（mintOutsideFileGrant）。
 * - 文件操作：home/stat/read/raw/serve/write/upload/delete/rename/mkdir/list。
 * - git 集成：仓库克隆、嵌套仓库发现（git-dirs）、gitignore 过滤、
 *   以及 git rev-parse 只读结果的 TTL+LRU 缓存。
 * - shell 执行：/api/fs/exec 在工作区目录内以用户 shell 运行命令并收集结果。
 */

import { createRealpathCache } from '../path-realpath-cache.js';
import nodeFsPromises from 'node:fs/promises';
import nodePath from 'node:path';

/** exec 任务在内存注册表中的存活时间（30 分钟），超时未更新即被惰性清理。 */
const EXEC_JOB_TTL_MS = 30 * 60 * 1000;
/** 工作区外文件访问授权（grant）的有效期（10 分钟），过期后 token 立即失效。 */
const OUTSIDE_FILE_GRANT_TTL_MS = 10 * 60 * 1000;

/** 模块级 grant 注册表：token 到 { canonicalPath, base, scopes, expiresAt } 的映射，跨请求共享。 */
const outsideFileGrants = new Map();

/** 清理 outsideFileGrants 中已过期或损坏的授权项；在每次铸造/校验 grant 前调用。 */
const pruneOutsideFileGrants = () => {
  const now = Date.now();
  for (const [token, grant] of outsideFileGrants.entries()) {
    if (!grant || grant.expiresAt <= now) {
      outsideFileGrants.delete(token);
    }
  }
};

/** 判断错误是否为操作系统级权限拒绝（EACCES/EPERM），用于把这类错误映射为 403 而非 500。 */
const isOsPermissionError = (error) => (
  error
  && typeof error === 'object'
  && (error.code === 'EACCES' || error.code === 'EPERM')
);

/** 以 403 响应 OS 权限拒绝，body 附带 reason: 'os-permission' 供客户端区分权限问题。 */
const sendOsPermissionDenied = (res, message) => (
  res.status(403).json({ error: message, reason: 'os-permission' })
);

/**
 * 为工作区外的单个文件铸造一次性访问授权（grant）。
 * 对目标路径 realpath 归一后要求是已存在的普通文件，再生成随机 token 存入
 * 模块级注册表（有效期见 OUTSIDE_FILE_GRANT_TTL_MS），并顺带清理过期授权。
 * @param {string} targetPath 目标文件路径。
 * @param {object} [options] 可选注入项：scopes 允许的范围（默认 stat/read/raw 全开）；
 *   fsPromises/path/crypto 供测试替换。
 * @returns {Promise<{path: string, outsideFileGrant: string, expiresAt: number}>}
 *   规范化路径、grant token 与过期时间戳。
 * @throws 路径为空、realpath/stat 失败或目标不是普通文件时抛错。
 */
export const mintOutsideFileGrant = async (targetPath, {
  scopes = ['stat', 'read', 'raw'],
  fsPromises = nodeFsPromises,
  path = nodePath,
  crypto = globalThis.crypto,
} = {}) => {
  const raw = typeof targetPath === 'string' ? targetPath.trim() : '';
  if (!raw) {
    throw new Error('Path is required');
  }
  const canonicalPath = await fsPromises.realpath(raw);
  const stats = await fsPromises.stat(canonicalPath);
  if (!stats.isFile()) {
    throw new Error('Outside file grants require a file path');
  }
  pruneOutsideFileGrants();
  const token = typeof crypto?.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  const normalizedScopes = new Set(
    (Array.isArray(scopes) ? scopes : [])
      .filter((scope) => typeof scope === 'string' && scope.trim())
      .map((scope) => scope.trim())
  );
  if (normalizedScopes.size === 0) {
    normalizedScopes.add('read');
  }
  const grant = {
    canonicalPath,
    base: path.dirname(canonicalPath),
    scopes: normalizedScopes,
    expiresAt: Date.now() + OUTSIDE_FILE_GRANT_TTL_MS,
  };
  outsideFileGrants.set(token, grant);
  return {
    path: canonicalPath,
    outsideFileGrant: token,
    expiresAt: grant.expiresAt,
  };
};

/**
 * 校验一次工作区外文件授权：token 必须存在、未过期、允许所请求的 scope，
 * 且请求路径 realpath 后与授权记录的 canonicalPath 完全一致（防止 token 被挪用）。
 * @returns {Promise<object>} 失败返回 { ok: false, error }；成功返回
 *   { ok: true, base, resolved, granted: true }。
 */
const resolveOutsideFileGrant = async ({ token, targetPath, scope, fsPromises }) => {
  pruneOutsideFileGrants();
  if (typeof token !== 'string' || !token.trim()) {
    return { ok: false, error: 'Outside workspace file access requires a grant' };
  }
  const grant = outsideFileGrants.get(token.trim());
  if (!grant) {
    return { ok: false, error: 'Outside workspace file grant is invalid or expired' };
  }
  if (!grant.scopes.has(scope)) {
    return { ok: false, error: 'Outside workspace file grant does not allow this operation' };
  }
  const canonicalPath = await fsPromises.realpath(targetPath);
  if (canonicalPath !== grant.canonicalPath) {
    return { ok: false, error: 'Outside workspace file grant does not match requested path' };
  }
  return { ok: true, base: grant.base, resolved: canonicalPath, granted: true };
};

/** 读取单条命令的执行超时（环境变量 OMPCHAMBER_FS_EXEC_TIMEOUT_MS，须为正数），默认 5 分钟。 */
const createCommandTimeoutMs = () => {
  const raw = Number(process.env.OMPCHAMBER_FS_EXEC_TIMEOUT_MS);
  if (Number.isFinite(raw) && raw > 0) return raw;
  return 5 * 60 * 1000;
};

// How long a cached git-read result stays fresh. The location of a repo's git
// directory is effectively static while the app runs, so a short TTL safely
// absorbs the burst of identical lookups a fresh client (e.g. right after a
// page reload) fires for every project. Set to 0 to disable caching.
/** 读取 git-read 缓存 TTL（OMPCHAMBER_GIT_READ_CACHE_TTL_MS），默认 30 秒，设为 0 表示禁用缓存。 */
const createGitReadCacheTtlMs = () => {
  const raw = Number(process.env.OMPCHAMBER_GIT_READ_CACHE_TTL_MS);
  if (Number.isFinite(raw) && raw >= 0) return raw;
  return 30 * 1000;
};

/** 读取 git check-ignore 子进程超时（OMPCHAMBER_GIT_CHECK_IGNORE_TIMEOUT_MS），默认 2.5 秒，0 表示不限时。 */
const createGitCheckIgnoreTimeoutMs = () => {
  const raw = Number(process.env.OMPCHAMBER_GIT_CHECK_IGNORE_TIMEOUT_MS);
  if (Number.isFinite(raw) && raw >= 0) return raw;
  return 2500;
};

/** 读取上传大小上限（OMPCHAMBER_FS_UPLOAD_MAX_BYTES，须为正整数），默认 100 MiB。 */
const createUploadMaxBytes = () => {
  const raw = Number(process.env.OMPCHAMBER_FS_UPLOAD_MAX_BYTES);
  if (Number.isFinite(raw) && raw > 0) return Math.floor(raw);
  return 100 * 1024 * 1024;
};

/** 扩展名到 Content-Type 的映射表（已冻结），供 /api/fs/serve 按扩展名推断 MIME。 */
const FILE_MIME_MAP = Object.freeze({
  '.html': 'text/html',
  '.htm': 'text/html',
  '.css': 'text/css',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.xml': 'application/xml',
  '.txt': 'text/plain',
  '.md': 'text/markdown',
  '.pdf': 'application/pdf',
  '.csv': 'text/csv',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.ttf': 'font/ttf',
  '.eot': 'application/vnd.ms-fontobject',
  '.mp3': 'audio/mpeg',
  '.mp4': 'video/mp4',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.svg': 'image/svg+xml',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.bmp': 'image/bmp',
  '.avif': 'image/avif',
});

/** /api/fs/serve 允许服务的最大文件字节数（100 MiB），超出返回 413。 */
const MAX_SERVE_BYTES = 100 * 1024 * 1024;

/**
 * 把请求体流式写入已打开的文件句柄，边写边累计字节数。
 * 超过 maxBytes 时先 resume 排空请求再抛出带 uploadTooLarge 标记的错误；
 * 句柄一次写回 0 字节视为写入失败并抛错。
 */
const streamUploadBody = async (req, handle, maxBytes) => {
  let received = 0;
  for await (const chunk of req) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    received += buffer.length;
    if (received > maxBytes) {
      req.resume?.();
      throw Object.assign(new Error('Upload exceeds the maximum allowed size'), { uploadTooLarge: true });
    }

    let offset = 0;
    while (offset < buffer.length) {
      const { bytesWritten } = await handle.write(buffer, offset, buffer.length - offset, null);
      if (!Number.isFinite(bytesWritten) || bytesWritten <= 0) {
        throw new Error('Failed to write upload');
      }
      offset += bytesWritten;
    }
  }
};

// Only deterministic, side-effect-free git plumbing path queries are cacheable.
// Anything outside this allowlist (including any non-git command) runs normally
// — we never cache arbitrary exec.
/** 把命令串归一（trim 并压缩内部空白），作为 git-read 缓存 key 的一部分；非字符串返回空串。 */
const normalizeCommand = (command) =>
  typeof command === 'string' ? command.trim().replace(/\s+/g, ' ') : '';

/**
 * 判断命令是否属于可缓存的 git 只读白名单：
 * 仅接受 git rev-parse 携带 --absolute-git-dir、--git-common-dir、--show-toplevel
 * 中 1 到 3 个组合的形式；其余命令（包括所有非 git 命令）一律不缓存、正常执行。
 */
const isCacheableGitReadCommand = (command) => {
  const normalized = normalizeCommand(command);
  return /^git rev-parse(?: --(?:absolute-git-dir|git-common-dir|show-toplevel)){1,3}$/.test(normalized);
};

// Dual-constraint bound per the project's caching policy (count + bytes). Git
// rev-parse outputs are tiny, so these ceilings are generous and only guard
// against pathological growth on long-lived, many-directory deployments.
/** git-read 缓存的最大条目数（LRU 双重上限之一）。 */
const GIT_READ_CACHE_MAX_ENTRIES = 500;
/** git-read 缓存的总字节数上限（约 1 MiB），与条目数共同约束内存占用。 */
const GIT_READ_CACHE_MAX_BYTES = 1024 * 1024;

/** 估算一条缓存项的内存占用：key 长度加上 stdout 与 stderr 的长度（按字符数近似）。 */
const gitReadEntryBytes = (key, result) =>
  key.length + (result?.stdout?.length || 0) + (result?.stderr?.length || 0);

/**
 * 词法级包含检查：resolvedPath 是否位于 rootPath（为空时回退到家目录）之内。
 * 只做 path.relative 判断、不解析 symlink —— 需要规范化围栏的调用方
 * 应先对两侧 realpath 再调用本函数。
 */
const isPathWithinRoot = (resolvedPath, rootPath, path, os) => {
  const resolvedRoot = path.resolve(rootPath || os.homedir());
  const relative = path.relative(resolvedRoot, resolvedPath);
  if (relative.startsWith('..') || path.isAbsolute(relative)) {
    return false;
  }
  return true;
};

/**
 * 把客户端传入的 targetPath 归一为绝对路径，并检查是否落在 baseDirectory
 * （活动项目目录）或 ompchamberUserConfigRoot（用户配置根）之内。
 * @returns {{ok: boolean, base?: string, resolved?: string, error?: string}}
 *   成功时给出围栏根 base 与绝对路径 resolved；失败时给出可直接作为
 *   400 响应正文的 error 文案。
 */
const resolveWorkspacePath = ({ targetPath, baseDirectory, path, os, normalizeDirectoryPath, ompchamberUserConfigRoot }) => {
  const normalized = normalizeDirectoryPath(targetPath);
  if (!normalized || typeof normalized !== 'string') {
    return { ok: false, error: 'Path is required' };
  }

  const resolved = path.resolve(normalized);
  const resolvedBase = path.resolve(baseDirectory || os.homedir());

  if (isPathWithinRoot(resolved, resolvedBase, path, os)) {
    return { ok: true, base: resolvedBase, resolved };
  }

  if (isPathWithinRoot(resolved, ompchamberUserConfigRoot, path, os)) {
    return { ok: true, base: path.resolve(ompchamberUserConfigRoot), resolved };
  }

  return { ok: false, error: 'Path is outside of active workspace' };
};

/**
 * 兜底路径解析：目标不在项目目录本身时，动态加载 ../git/index.js 的
 * getWorktrees，逐个检查是否落在某个 worktree 根目录内。
 * worktree 枚举失败只打 console.warn 并按越界处理，不向上抛错。
 */
const resolveWorkspacePathFromWorktrees = async ({ targetPath, baseDirectory, path, os, normalizeDirectoryPath }) => {
  const normalized = normalizeDirectoryPath(targetPath);
  if (!normalized || typeof normalized !== 'string') {
    return { ok: false, error: 'Path is required' };
  }

  const resolved = path.resolve(normalized);
  const resolvedBase = path.resolve(baseDirectory || os.homedir());

  try {
    const { getWorktrees } = await import('../git/index.js');
    const worktrees = await getWorktrees(resolvedBase);

    for (const worktree of worktrees) {
      const candidatePath = typeof worktree?.path === 'string'
        ? worktree.path
        : (typeof worktree?.worktree === 'string' ? worktree.worktree : '');
      const candidate = normalizeDirectoryPath(candidatePath);
      if (!candidate) {
        continue;
      }
      const candidateResolved = path.resolve(candidate);
      if (isPathWithinRoot(resolved, candidateResolved, path, os)) {
        return { ok: true, base: candidateResolved, resolved };
      }
    }
  } catch (error) {
    console.warn('Failed to resolve worktree roots:', error);
  }

  return { ok: false, error: 'Path is outside of active workspace' };
};

/**
 * 基于请求上下文的三级路径解析：
 * 1) 以 resolveProjectDirectory 解析出的活动项目目录做围栏；
 * 2) 若仅因越界失败且客户端请求的原始目录（可能是 symlink 路径空间）与规范化
 *    目录不同，则按原始目录词法重试，保证 symlink 下的路径仍可寻址；
 * 3) 仍失败则尝试各 worktree 根。
 * @returns 与 resolveWorkspacePath 相同的 { ok, base, resolved, error } 结构。
 */
const resolveWorkspacePathFromContext = async ({ req, targetPath, resolveProjectDirectory, path, os, normalizeDirectoryPath, ompchamberUserConfigRoot }) => {
  const resolvedProject = await resolveProjectDirectory(req);
  if (!resolvedProject.directory) {
    return { ok: false, error: resolvedProject.error || 'Active workspace is required' };
  }

  const resolved = resolveWorkspacePath({
    targetPath,
    baseDirectory: resolvedProject.directory,
    path,
    os,
    normalizeDirectoryPath,
    ompchamberUserConfigRoot,
  });
  if (resolved.ok || resolved.error !== 'Path is outside of active workspace') {
    return resolved;
  }

  // The active project directory is validated with fs.realpath, so the base is
  // canonical while the client (and the file tree) addresses files under the
  // user-visible root, which may itself be a symlink. Retry against the raw
  // directory the client asked for so those paths stay addressable. Symlink
  // resolution still happens afterwards, and the routes that need canonical
  // containment re-check it against this base.
  const requestedBase = resolvedProject.requestedDirectory;
  if (typeof requestedBase === 'string' && requestedBase && requestedBase !== resolvedProject.directory) {
    const lexical = resolveWorkspacePath({
      targetPath,
      baseDirectory: requestedBase,
      path,
      os,
      normalizeDirectoryPath,
      ompchamberUserConfigRoot,
    });
    if (lexical.ok) {
      return lexical;
    }
  }

  return resolveWorkspacePathFromWorktrees({
    targetPath,
    baseDirectory: resolvedProject.directory,
    path,
    os,
    normalizeDirectoryPath,
  });
};

// Nested repository discovery bounds: only shallow walks are useful for the
// Git tab's "pick a repository" picker, and deep/monorepo trees can explode
// otherwise. Directories deeper than maxDepth or beyond the visit cap are
// silently not searched.
/** 嵌套仓库搜索的最大深度（自根向下 3 层）。 */
const GIT_DIRS_MAX_DEPTH = 3;
/** 嵌套仓库搜索最多访问的目录数量，防止巨型 monorepo 拖垮扫描。 */
const GIT_DIRS_MAX_DIRS = 100;
/** 仓库扫描永远跳过的目录名（依赖目录与构建产物）。 */
const GIT_DIRS_SKIP_LIST = new Set(['node_modules', 'dist', 'build', '.venv', 'target', '.next']);

// Walks rootPath and returns every nested git repository path (a directory
// containing a `.git` entry — a directory, a worktree pointer file, or a
// symlink). A repository boundary stops descent: nested repos inside repos
// are not reported. The root itself, when it is a repo, yields no results.
/**
 * 自顶向下遍历 rootPath，收集所有嵌套 git 仓库的路径（即包含 .git 条目 ——
 * 目录、worktree 指针文件或 symlink —— 的目录）。遇到仓库边界即停止下钻，
 * 因此仓库内再嵌套的仓库不会被报告；根目录本身是仓库时返回空列表。
 * 子目录不可读时静默跳过，仅根目录不可读时向上抛错（由路由映射 403/404/500）。
 * @returns {Promise<string[]>} 仓库目录的绝对路径列表（子目录按名称排序遍历）。
 */
const findGitDirectories = async ({ rootPath, fsPromises, path: pathModule, maxDepth, maxDirs }) => {
  const results = [];
  let visited = 0;

  /** 单目录递归体：受 maxDirs 访问上限约束，识别 .git 边界并收集待下钻的子目录。 */
  const walk = async (dir, depth) => {
    if (visited >= maxDirs) {
      return;
    }

    let dirents;
    try {
      dirents = await fsPromises.readdir(dir, { withFileTypes: true });
    } catch (error) {
      // Unreadable subtree — skip it unless it is the root itself, which the
      // route maps to 403/404/500 through the shared error handling.
      if (dir === rootPath) {
        throw error;
      }
      return;
    }
    visited += 1;

    let isRepoBoundary = false;
    const subdirectories = [];
    for (const dirent of dirents) {
      if (dirent.name === '.git') {
        isRepoBoundary = true;
        continue;
      }
      if (!dirent.isDirectory() || dirent.isSymbolicLink()) {
        continue;
      }
      if (GIT_DIRS_SKIP_LIST.has(dirent.name)) {
        continue;
      }
      if (depth >= maxDepth) {
        continue;
      }
      subdirectories.push(dirent.name);
    }

    if (isRepoBoundary) {
      if (dir !== rootPath) {
        results.push(dir);
      }
      return;
    }

    subdirectories.sort();
    for (const name of subdirectories) {
      if (visited >= maxDirs) {
        break;
      }
      await walk(pathModule.join(dir, name), depth + 1);
    }
  };

  await walk(rootPath, 0);
  return results;
};

/**
 * 从 git 远程 URL 推断克隆目录名：去掉 query/fragment 与结尾斜杠后，
 * 取路径最后一段并剥掉 .git 后缀；无法推断时返回空串。
 */
const deriveCloneDirectoryName = (remoteUrl) => {
  const remote = typeof remoteUrl === 'string' ? remoteUrl.trim() : '';
  if (!remote) return '';
  const withoutQuery = remote.split(/[?#]/, 1)[0] || remote;
  const match = withoutQuery.match(/([^/:]+?)(?:\.git)?\/?$/);
  return match?.[1]?.trim() || '';
};

/**
 * 解析克隆要使用的 git 身份：id 为 'global' 时读取全局身份（要求 userName
 * 与 userEmail 齐全，并把 sshCommand 还原为 sshKey 路径），否则按 profile id
 * 查询；通过动态加载 ../git/index.js 完成，查不到时返回 null。
 */
const resolveCloneGitIdentity = async (gitIdentityId) => {
  const id = typeof gitIdentityId === 'string' ? gitIdentityId.trim() : '';
  if (!id) return null;
  const { getProfile, getGlobalIdentity } = await import('../git/index.js');
  if (id === 'global') {
    const globalIdentity = await getGlobalIdentity();
    if (!globalIdentity?.userName || !globalIdentity?.userEmail) return null;
    return {
      id: 'global',
      name: 'Global Identity',
      userName: globalIdentity.userName,
      userEmail: globalIdentity.userEmail,
      sshKey: globalIdentity.sshCommand ? globalIdentity.sshCommand.replace('ssh -i ', '') : null,
    };
  }
  return getProfile(id) || null;
};

/**
 * 把 SSH 私钥路径转义成可安全嵌入 git -c core.sshCommand 的单引号字面量。
 * 含 shell 元字符（反引号、美元、引号、管道等）的路径直接抛错；
 * Windows 上把反斜杠转为正斜杠，并把盘符路径改写成 Git Bash 风格（如 C:/k -> /c/k）。
 * @throws 路径包含危险字符时抛错。
 */
const escapeCloneSshKeyPath = (sshKeyPath) => {
  const raw = String(sshKeyPath || '').trim();
  if (!raw) return '';
  const normalized = process.platform === 'win32' ? raw.replace(/\\/g, '/') : raw;
  const dangerousChars = /[`$!"';&|<>(){}[\]*?#~]/;
  if (dangerousChars.test(normalized)) {
    throw new Error(`SSH key path contains invalid characters: ${raw}`);
  }
  if (process.platform === 'win32') {
    const driveMatch = normalized.match(/^([A-Za-z]):\//);
    const unixPath = driveMatch ? `/${driveMatch[1].toLowerCase()}${normalized.slice(2)}` : normalized;
    return `'${unixPath}'`;
  }
  return `'${normalized.replace(/'/g, "'\\''")}'`;
};

/**
 * 读取类端点（stat/read/raw）专用的路径解析：
 * 携带 allowOutsideWorkspace=true 时走 grant 校验分支（scope 决定允许的操作，
 * targetPath 会再次 realpath 并与授权路径比对）；否则回落到常规工作区围栏解析。
 */
const resolveReadPathFromContext = async ({ req, targetPath, scope, resolveProjectDirectory, path, os, fsPromises, normalizeDirectoryPath, ompchamberUserConfigRoot }) => {
  if (req.query?.allowOutsideWorkspace === 'true') {
    const normalized = normalizeDirectoryPath(targetPath);
    if (!normalized || typeof normalized !== 'string') {
      return { ok: false, error: 'Path is required' };
    }
    const resolved = path.resolve(normalized);
    return resolveOutsideFileGrant({
      token: req.query?.outsideFileGrant,
      targetPath: resolved,
      scope,
      fsPromises,
    });
  }

  return resolveWorkspacePathFromContext({
    req,
    targetPath,
    resolveProjectDirectory,
    path,
    os,
    normalizeDirectoryPath,
    ompchamberUserConfigRoot,
  });
};

/**
 * 在 resolvedCwd 目录中经 shell（shellFlag 形如 -c 或 /c）执行单条命令，
 * 用增强后的 PATH 环境变量 spawn 子进程，收集 stdout/stderr，
 * 超时（commandTimeoutMs）后 SIGKILL。返回 { command, success, exitCode,
 * stdout, stderr, error? }，永不 reject —— 所有失败路径都体现在结果对象里。
 */
const runCommandInDirectory = ({ shell, shellFlag, command, resolvedCwd, spawn, buildAugmentedPath, commandTimeoutMs }) => {
  return new Promise((resolve) => {
    let stdout = '';
    let stderr = '';
    let timedOut = false;

    const envPath = buildAugmentedPath();
    const execEnv = { ...process.env, PATH: envPath };

    const child = spawn(shell, [shellFlag, command], {
      cwd: resolvedCwd,
      env: execEnv,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    const timeout = setTimeout(() => {
      timedOut = true;
      try {
        child.kill('SIGKILL');
      } catch {
      }
    }, commandTimeoutMs);

    child.stdout?.on('data', (chunk) => {
      stdout += chunk.toString();
    });

    child.stderr?.on('data', (chunk) => {
      stderr += chunk.toString();
    });

    child.on('error', (error) => {
      clearTimeout(timeout);
      resolve({
        command,
        success: false,
        exitCode: undefined,
        stdout: stdout.trim(),
        stderr: stderr.trim(),
        error: (error && error.message) || 'Command execution failed',
      });
    });

    child.on('close', (code, signal) => {
      clearTimeout(timeout);
      const exitCode = typeof code === 'number' ? code : undefined;
      const base = {
        command,
        success: exitCode === 0 && !timedOut,
        exitCode,
        stdout: stdout.trim(),
        stderr: stderr.trim(),
      };

      if (timedOut) {
        resolve({
          ...base,
          success: false,
          error: `Command timed out after ${commandTimeoutMs}ms` + (signal ? ` (${signal})` : ''),
        });
        return;
      }

      resolve(base);
    });
  });
};

/**
 * 在 app 上注册全部 /api/fs/* 端点（home/mkdir/clone/stat/read/raw/serve/
 * write/upload/delete/rename/reveal/exec/list/git-dirs）。
 * @param {object} app Express 应用（测试中为路由注册表替身）。
 * @param {object} dependencies 注入依赖：os/path/fsPromises/spawn/crypto 等
 *   Node 模块（测试可替换）、platform 平台标识、normalizeDirectoryPath 与
 *   resolveProjectDirectory（活动项目目录解析）、buildAugmentedPath（增强 PATH）、
 *   resolveGitBinaryForSpawn（git 可执行名）、ompchamberUserConfigRoot（用户配置根）。
 */
export const registerFsRoutes = (app, dependencies) => {
  const {
    os,
    path,
    fsPromises,
    spawn,
    platform = process.platform,
    crypto,
    normalizeDirectoryPath,
    resolveProjectDirectory,
    buildAugmentedPath,
    resolveGitBinaryForSpawn,
    ompchamberUserConfigRoot,
  } = dependencies;
  /** realpath 结果缓存（见 path-realpath-cache 模块），加速 list 端点的重复路径解析。 */
  const realpathCache = createRealpathCache({
    realpath: fsPromises.realpath.bind(fsPromises),
  });

  /**
   * 以 detached 模式启动外部程序（如文件管理器）并在成功 spawn 后立即 unref，
   * 不等待其退出。同步抛错或触发 error 事件时以统一错误信息 reject。
   */
  const spawnDetached = (command, args) => new Promise((resolve, reject) => {
    let child;
    try {
      child = spawn(command, args, { windowsHide: true, stdio: 'ignore', detached: true });
    } catch (error) {
      reject(new Error('Failed to launch file browser', { cause: error }));
      return;
    }
    /** spawn 失败回调：解绑成功监听并以统一错误 reject。 */
    const onError = (error) => {
      child.removeListener('spawn', onSpawn);
      reject(new Error('Failed to launch file browser', { cause: error }));
    };
    /** spawn 成功回调：解绑错误监听、unref 子进程（不阻塞进程退出）并 resolve。 */
    const onSpawn = () => {
      child.removeListener('error', onError);
      child.unref();
      resolve();
    };
    child.once('error', onError);
    child.once('spawn', onSpawn);
  });

  /** exec 任务注册表：jobId 到任务对象（状态、命令、结果、时间戳）的映射，带 TTL 惰性清理。 */
  const execJobs = new Map();
  /** 单条命令的执行超时（毫秒），注册路由时读取一次。 */
  const commandTimeoutMs = createCommandTimeoutMs();
  /** git-read 缓存 TTL（毫秒），0 表示禁用缓存。 */
  const gitReadCacheTtlMs = createGitReadCacheTtlMs();
  /** git check-ignore 子进程超时（毫秒），0 表示不限时。 */
  const gitCheckIgnoreTimeoutMs = createGitCheckIgnoreTimeoutMs();
  /** git 只读命令的结果缓存：缓存 key 到 { result, at } 的 LRU Map。 */
  const gitReadCache = new Map();
  /** 在途 git-read 请求表：缓存 key 到进行中 Promise 的映射，用于合并并发相同请求。 */
  const inFlightGitReadCache = new Map();

  /** 清理超过 EXEC_JOB_TTL_MS 未更新的 exec 任务；条目缺失或非对象时直接删除。 */
  const pruneExecJobs = () => {
    const now = Date.now();
    for (const [jobId, job] of execJobs.entries()) {
      if (!job || typeof job !== 'object') {
        execJobs.delete(jobId);
        continue;
      }
      const updatedAt = typeof job.updatedAt === 'number' ? job.updatedAt : 0;
      if (updatedAt && now - updatedAt > EXEC_JOB_TTL_MS) {
        execJobs.delete(jobId);
      }
    }
  };

  /** 清理过期的 git-read 缓存项；TTL 为 0（禁用缓存）时直接返回。 */
  const pruneGitReadCache = () => {
    if (gitReadCacheTtlMs <= 0) {
      return;
    }
    const now = Date.now();
    for (const [key, entry] of gitReadCache.entries()) {
      if (!entry || now - entry.at > gitReadCacheTtlMs) {
        gitReadCache.delete(key);
      }
    }
  };

  // Insert with LRU (oldest-first) eviction enforcing both count and byte caps.
  // Map iteration order is insertion order, so deleting+re-setting a key moves
  // it to the most-recently-used position.
  /**
   * 写入一条 git-read 缓存：先 delete 再 set 把 key 挪到最新位置（LRU），
   * 随后从最旧端淘汰，直到同时满足条目数与总字节数双重上限。
   */
  const setGitReadCacheEntry = (key, result) => {
    gitReadCache.delete(key);
    gitReadCache.set(key, { result, at: Date.now() });

    let totalBytes = 0;
    for (const [k, entry] of gitReadCache) {
      totalBytes += gitReadEntryBytes(k, entry.result);
    }
    while (
      gitReadCache.size > GIT_READ_CACHE_MAX_ENTRIES ||
      (totalBytes > GIT_READ_CACHE_MAX_BYTES && gitReadCache.size > 1)
    ) {
      const oldest = gitReadCache.entries().next().value;
      if (!oldest) {
        break;
      }
      totalBytes -= gitReadEntryBytes(oldest[0], oldest[1].result);
      gitReadCache.delete(oldest[0]);
    }
  };

  // Runs a command, transparently serving/storing cacheable git-read results.
  // Non-cacheable commands always execute and are never stored.
  /**
   * 带缓存的命令执行：命令命中 git-read 白名单且缓存未过期时直接返回缓存结果
   * （并刷新 LRU 位置）；存在相同 key 的在途请求时复用同一个 Promise 防止惊群；
   * 其余情况真正执行，且只有成功的白名单结果才会写入缓存（失败可能是瞬时的）。
   */
  const runCommandWithGitReadCache = async ({ shell, shellFlag, command, resolvedCwd }) => {
    const cacheable = gitReadCacheTtlMs > 0 && isCacheableGitReadCommand(command);
    const cacheKey = cacheable ? `${resolvedCwd}${normalizeCommand(command)}` : null;

    if (cacheKey) {
      const cached = gitReadCache.get(cacheKey);
      if (cached && Date.now() - cached.at < gitReadCacheTtlMs) {
        // Refresh recency for LRU without altering the entry's age/TTL.
        gitReadCache.delete(cacheKey);
        gitReadCache.set(cacheKey, cached);
        return { ...cached.result, command };
      }
      if (cached) {
        gitReadCache.delete(cacheKey);
      }

      const inFlight = inFlightGitReadCache.get(cacheKey);
      if (inFlight) {
        const result = await inFlight;
        return { ...result, command };
      }
    }

    const runPromise = runCommandInDirectory({
      shell,
      shellFlag,
      command,
      resolvedCwd,
      spawn,
      buildAugmentedPath,
      commandTimeoutMs,
    }).then((result) => {
      // Only cache successful results — failures may be transient.
      if (cacheKey && result && result.success) {
        setGitReadCacheEntry(cacheKey, result);
      }
      return result;
    }).finally(() => {
      if (cacheKey && inFlightGitReadCache.get(cacheKey) === runPromise) {
        inFlightGitReadCache.delete(cacheKey);
      }
    });

    if (cacheKey) {
      inFlightGitReadCache.set(cacheKey, runPromise);
    }

    return runPromise;
  };

  /**
   * 顺序执行 job.commands 中的每条命令，逐条把结果累计进 job.results 并刷新
   * updatedAt；单条命令抛错只记录失败结果、不中断后续命令。全部执行完后
   * 汇总 success（所有命令成功才为 true）、置 status 为 done 并记录 finishedAt。
   */
  const runExecJob = async (job) => {
    job.status = 'running';
    job.updatedAt = Date.now();

    const results = [];
    for (const command of job.commands) {
      if (typeof command !== 'string' || !command.trim()) {
        results.push({ command, success: false, error: 'Invalid command' });
        continue;
      }

      try {
        const result = await runCommandWithGitReadCache({
          shell: job.shell,
          shellFlag: job.shellFlag,
          command,
          resolvedCwd: job.resolvedCwd,
        });
        results.push(result);
      } catch (error) {
        results.push({
          command,
          success: false,
          error: (error && error.message) || 'Command execution failed',
        });
      }

      job.results = results;
      job.updatedAt = Date.now();
    }

    job.results = results;
    job.success = results.every((r) => r.success);
    job.status = 'done';
    job.finishedAt = Date.now();
    job.updatedAt = Date.now();
  };

  /**
   * GET /api/fs/home — 返回当前用户家目录（os.homedir()），供客户端默认定位。
   * 家目录缺失或解析异常时返回 500。
   */
  app.get('/api/fs/home', (_req, res) => {
    try {
      const home = os.homedir();
      if (!home || typeof home !== 'string' || home.length === 0) {
        return res.status(500).json({ error: 'Failed to resolve home directory' });
      }
      return res.json({ home });
    } catch (error) {
      console.error('Failed to resolve home directory:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to resolve home directory' });
    }
  });

  /**
   * POST /api/fs/mkdir — 递归创建目录。body: { path, allowOutsideWorkspace }。
   * 路径必须通过工作区围栏校验；allowOutsideWorkspace（目录类越界）一律 403 拒绝。
   * OS 权限错误返回 403，其余失败返回 500。
   */
  app.post('/api/fs/mkdir', async (req, res) => {
    try {
      const { path: dirPath, allowOutsideWorkspace } = req.body ?? {};
      if (typeof dirPath !== 'string' || !dirPath.trim()) {
        return res.status(400).json({ error: 'Path is required' });
      }

      let resolvedPath = '';
      if (allowOutsideWorkspace) {
        console.warn('Rejected outside-workspace mkdir without trusted directory grant');
        return res.status(403).json({ error: 'Outside workspace directory creation requires a grant' });
      } else {
        const resolved = await resolveWorkspacePathFromContext({
          req,
          targetPath: dirPath,
          resolveProjectDirectory,
          path,
          os,
          normalizeDirectoryPath,
          ompchamberUserConfigRoot,
        });
        if (!resolved.ok) {
          return res.status(400).json({ error: resolved.error });
        }
        resolvedPath = resolved.resolved;
      }

      await fsPromises.mkdir(resolvedPath, { recursive: true });
      return res.json({ success: true, path: resolvedPath });
    } catch (error) {
      if (isOsPermissionError(error)) {
        return sendOsPermissionDenied(res, 'Access denied');
      }
      console.error('Failed to create directory:', error);
      return res.status(500).json({ error: error.message || 'Failed to create directory' });
    }
  });

  /**
   * POST /api/fs/clone — 克隆远程仓库。body: { remoteUrl, destinationPath, gitIdentityId }。
   * 目标可以是"父目录/"（从 URL 推断目录名）或"父目录/新名字"；目标已存在返回 409。
   * 指定 git 身份时通过 -c core.sshCommand 注入 SSH key，克隆成功后再把
   * userName/userEmail 写入新仓库的本地配置（失败仅警告）。git 子进程带
   * GIT_TERMINAL_PROMPT=0 禁止交互式凭据提示，失败返回 500 与合并后的输出。
   */
  app.post('/api/fs/clone', async (req, res) => {
    try {
      const { remoteUrl, destinationPath, gitIdentityId } = req.body ?? {};
      const remote = typeof remoteUrl === 'string' ? remoteUrl.trim() : '';
      const destination = typeof destinationPath === 'string' ? destinationPath.trim() : '';
      if (!remote) {
        return res.status(400).json({ error: 'Repository URL is required' });
      }
      if (!destination) {
        return res.status(400).json({ error: 'Destination path is required' });
      }

      let resolvedDestination = path.resolve(normalizeDirectoryPath(destination));
      let parentPath = path.dirname(resolvedDestination);
      let directoryName = path.basename(resolvedDestination);

      const cloneIntoDestinationDirectory = destination.endsWith('/') || destination.endsWith('\\');
      if (cloneIntoDestinationDirectory) {
        const inferredName = deriveCloneDirectoryName(remote);
        if (!inferredName) {
          return res.status(400).json({ error: 'Could not infer repository directory name from URL' });
        }
        parentPath = resolvedDestination;
        directoryName = inferredName;
        resolvedDestination = path.join(parentPath, directoryName);
      } else {
        try {
          const stat = await fsPromises.stat(resolvedDestination);
          if (stat.isDirectory()) {
            const inferredName = deriveCloneDirectoryName(remote);
            if (!inferredName) {
              return res.status(400).json({ error: 'Could not infer repository directory name from URL' });
            }
            parentPath = resolvedDestination;
            directoryName = inferredName;
            resolvedDestination = path.join(parentPath, directoryName);
          }
        } catch (error) {
          if (!error || error.code !== 'ENOENT') {
            throw error;
          }
        }
      }
      if (!directoryName || directoryName === '.' || directoryName === '..') {
        return res.status(400).json({ error: 'Destination path must include a directory name' });
      }

      const identity = await resolveCloneGitIdentity(gitIdentityId);
      const gitArgs = ['clone', '--', remote, directoryName];
      const sshKeyPath = typeof identity?.sshKey === 'string' ? identity.sshKey.trim() : '';
      if (sshKeyPath) {
        gitArgs.unshift(`core.sshCommand=ssh -i ${escapeCloneSshKeyPath(sshKeyPath)} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new`);
        gitArgs.unshift('-c');
      }

      await fsPromises.mkdir(parentPath, { recursive: true });
      try {
        await fsPromises.access(resolvedDestination);
        return res.status(409).json({ error: 'Destination path already exists' });
      } catch (error) {
        if (!error || error.code !== 'ENOENT') {
          throw error;
        }
      }

      const output = await new Promise((resolve, reject) => {
        const child = spawn(resolveGitBinaryForSpawn(), gitArgs, {
          cwd: parentPath,
          windowsHide: true,
          stdio: ['ignore', 'pipe', 'pipe'],
          env: {
            ...process.env,
            PATH: buildAugmentedPath ? buildAugmentedPath(process.env.PATH || '') : process.env.PATH,
            GIT_TERMINAL_PROMPT: '0',
          },
        });

        let stdout = '';
        let stderr = '';
        child.stdout.on('data', (data) => { stdout += data.toString(); });
        child.stderr.on('data', (data) => { stderr += data.toString(); });
        child.on('error', reject);
        child.on('close', (code) => {
          const combined = `${stdout}\n${stderr}`.trim();
          if (code === 0) {
            resolve(combined);
            return;
          }
          const message = combined || `git clone failed with exit code ${code}`;
          reject(new Error(message));
        });
      });

      if (identity?.userName && identity?.userEmail) {
        try {
          const { setLocalIdentity } = await import('../git/index.js');
          await setLocalIdentity(resolvedDestination, identity);
        } catch (error) {
          console.warn('Failed to apply git identity after clone:', error);
        }
      }

      return res.json({ success: true, path: resolvedDestination, output });
    } catch (error) {
      console.error('Failed to clone repository:', error);
      return res.status(500).json({ error: error.message || 'Failed to clone repository' });
    }
  });

  /**
   * GET /api/fs/stat — 返回文件元信息 { path, isFile, size, mtimeMs }。
   * query: path 必填；optional=true 时 ENOENT 返回 { exists: false } 而非 404；
   * allowOutsideWorkspace=true 时需携带覆盖 stat scope 的有效 grant。
   * 越界或目标不是普通文件返回 400，OS 权限错误返回 403。
   */
  app.get('/api/fs/stat', async (req, res) => {
    const filePath = typeof req.query.path === 'string' ? req.query.path.trim() : '';
    const optional = req.query.optional === 'true';
    if (!filePath) {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = await resolveReadPathFromContext({
        req,
        targetPath: filePath,
        scope: 'stat',
        resolveProjectDirectory,
        path,
        os,
        fsPromises,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        if (req.query?.allowOutsideWorkspace === 'true') {
          console.warn(`Rejected outside-workspace stat: ${resolved.error}`);
        }
        return res.status(400).json({ error: resolved.error });
      }

      const canonicalPath = await fsPromises.realpath(resolved.resolved);

      const stats = await fsPromises.stat(canonicalPath);
      if (!stats.isFile()) {
        return res.status(400).json({ error: 'Specified path is not a file' });
      }

      return res.json({ path: canonicalPath, isFile: true, size: stats.size, mtimeMs: stats.mtimeMs });
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        if (optional) {
          return res.json({ path: filePath, exists: false });
        }
        return res.status(404).json({ error: 'File not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to file denied');
      }
      console.error('Failed to stat file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to stat file' });
    }
  });

  /**
   * GET /api/fs/read — 以 UTF-8 文本读取整个文件并以 text/plain 返回。
   * stat 与 readFile 之间可能撞上并发写者的 O_TRUNC 窗口（stat 有大小却读到
   * 空内容），此时按退避节奏重试最多 3 次。optional=true 时 ENOENT 返回空串；
   * OS 权限错误返回 403，越界返回 400。
   */
  app.get('/api/fs/read', async (req, res) => {
    const filePath = typeof req.query.path === 'string' ? req.query.path.trim() : '';
    const optional = req.query.optional === 'true';
    if (!filePath) {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = await resolveReadPathFromContext({
        req,
        targetPath: filePath,
        scope: 'read',
        resolveProjectDirectory,
        path,
        os,
        fsPromises,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        if (req.query?.allowOutsideWorkspace === 'true') {
          console.warn(`Rejected outside-workspace read: ${resolved.error}`);
        }
        return res.status(400).json({ error: resolved.error });
      }

      const canonicalPath = await fsPromises.realpath(resolved.resolved);

      const stats = await fsPromises.stat(canonicalPath);
      if (!stats.isFile()) {
        return res.status(400).json({ error: 'Specified path is not a file' });
      }

      let content = await fsPromises.readFile(canonicalPath, 'utf8');
      // Retry empty reads — concurrent writer may have truncated the file
      // between our stat and read (O_TRUNC window). If the file existed with
      // content at stat time but we read nothing, the writer hasn't finished
      // writing yet.
      if (content.length === 0 && stats.size > 0) {
        for (let attempt = 0; attempt < 3; attempt++) {
          await new Promise((r) => setTimeout(r, 50 * (attempt + 1)));
          content = await fsPromises.readFile(canonicalPath, 'utf8');
          if (content.length > 0) break;
        }
        if (content.length === 0) {
          console.warn(`Read retry exhausted for ${canonicalPath}: stat reported ${stats.size} bytes but content is empty`);
        }
      }
      return res.type('text/plain').send(content);
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        if (optional) {
          return res.type('text/plain').send('');
        }
        return res.status(404).json({ error: 'File not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to file denied');
      }
      console.error('Failed to read file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to read file' });
    }
  });

  /**
   * GET /api/fs/raw — 以原始字节读取文件，按扩展名设置 MIME 类型。
   * query: path；download=true 时按 RFC 5987 设置 Content-Disposition
   * （非 ASCII 文件名使用 filename*=UTF-8 百分号编码，另给 ASCII 兜底的 filename=）。
   * 经 grant 访问工作区外文件时附加 Referrer-Policy: no-referrer；响应禁止缓存。
   */
  app.get('/api/fs/raw', async (req, res) => {
    const filePath = typeof req.query.path === 'string' ? req.query.path.trim() : '';
    if (!filePath) {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = await resolveReadPathFromContext({
        req,
        targetPath: filePath,
        scope: 'raw',
        resolveProjectDirectory,
        path,
        os,
        fsPromises,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        if (req.query?.allowOutsideWorkspace === 'true') {
          console.warn(`Rejected outside-workspace raw read: ${resolved.error}`);
        }
        return res.status(400).json({ error: resolved.error });
      }

      const canonicalPath = await fsPromises.realpath(resolved.resolved);

      const stats = await fsPromises.stat(canonicalPath);
      if (!stats.isFile()) {
        return res.status(400).json({ error: 'Specified path is not a file' });
      }

      const ext = path.extname(canonicalPath).toLowerCase();
      const mimeMap = {
        '.png': 'image/png',
        '.jpg': 'image/jpeg',
        '.jpeg': 'image/jpeg',
        '.gif': 'image/gif',
        '.svg': 'image/svg+xml',
        '.webp': 'image/webp',
        '.ico': 'image/x-icon',
        '.bmp': 'image/bmp',
        '.avif': 'image/avif',
        '.pdf': 'application/pdf',
      };
      const mimeType = mimeMap[ext] || 'application/octet-stream';

      const download = req.query.download === 'true';
      if (download) {
        const fileName = path.basename(canonicalPath);
        // RFC 5987: use filename*= for non-ASCII filenames, with ASCII-only
        // filename= as fallback for older clients.
        const asciiOnly = fileName.replace(/[^\u0000-\u007F]/g, '');
        const fallback = asciiOnly || 'file';
        // Percent-encode the raw UTF-8 bytes for filename*=
        const encoded = encodeURIComponent(fileName);
        res.setHeader('Content-Disposition', `attachment; filename="${fallback}"; filename*=UTF-8''${encoded}`);
      }

      const content = await fsPromises.readFile(canonicalPath);
      res.setHeader('Cache-Control', 'no-store');
      if (resolved.granted) {
        res.setHeader('Referrer-Policy', 'no-referrer');
      }
      return res.type(mimeType).send(content);
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'File not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to file denied');
      }
      console.error('Failed to read raw file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to read file' });
    }
  });

  /**
   * GET /api/fs/serve/<绝对路径> — 只读静态文件服务，按 FILE_MIME_MAP 推断 MIME，
   * 超过 MAX_SERVE_BYTES 返回 413；明确禁止 allowOutsideWorkspace（403）。
   * 响应统一带 Cache-Control: no-store 与 X-Content-Type-Options: nosniff。
   */
  app.get(/^\/api\/fs\/serve\/(.+)$/, async (req, res) => {
    const rawPath = req.params[0] || '';
    if (!rawPath) {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      if (req.query?.allowOutsideWorkspace === 'true') {
        return res.status(403).json({ error: 'allowOutsideWorkspace is not permitted for this endpoint' });
      }

      const filePath = path.resolve('/', rawPath);
      const resolved = await resolveReadPathFromContext({
        req,
        targetPath: filePath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        return res.status(400).json({ error: resolved.error });
      }

      const canonicalPath = await fsPromises.realpath(resolved.resolved);

      const stats = await fsPromises.stat(canonicalPath);
      if (!stats.isFile()) {
        return res.status(400).json({ error: 'Specified path is not a file' });
      }
      if (stats.size > MAX_SERVE_BYTES) {
        return res.status(413).json({ error: 'File too large to serve' });
      }

      const ext = path.extname(canonicalPath).toLowerCase();
      const mimeType = FILE_MIME_MAP[ext] || 'application/octet-stream';
      const content = await fsPromises.readFile(canonicalPath);
      res.setHeader('Cache-Control', 'no-store');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      return res.type(mimeType).send(content);
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'File not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to file denied');
      }
      console.error('Failed to serve file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to serve file' });
    }
  });

  /**
   * POST /api/fs/write — 写入文本文件。body: { path, content }。
   * realpath 后对规范化 base 复查围栏（防 symlink 逃逸）；目标不存在时
   * realpath 回退为词法路径。内容与现有文件完全相同时直接短路返回成功，
   * 避免无谓重写。真正写入走"临时文件 + rename"原子替换，避免并发读端
   * 看到 O_TRUNC 窗口中的空文件；临时文件清理失败静默忽略。
   */
  app.post('/api/fs/write', async (req, res) => {
    const { path: filePath, content } = req.body || {};
    if (!filePath || typeof filePath !== 'string') {
      return res.status(400).json({ error: 'Path is required' });
    }
    if (typeof content !== 'string') {
      return res.status(400).json({ error: 'Content is required' });
    }

    try {
      const resolved = await resolveWorkspacePathFromContext({
        req,
        targetPath: filePath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        return res.status(400).json({ error: resolved.error });
      }

      const writePath = await fsPromises.realpath(resolved.resolved).catch((error) => {
        if (error && typeof error === 'object' && error.code === 'ENOENT') {
          return resolved.resolved;
        }
        throw error;
      });
      const canonicalBase = await fsPromises.realpath(resolved.base).catch(() => path.resolve(resolved.base));
      if (!isPathWithinRoot(writePath, canonicalBase, path, os)) {
        return res.status(403).json({ error: 'Access denied' });
      }

      const existing = await fsPromises.readFile(writePath, 'utf8').catch(() => null);
      if (existing === content) {
        return res.json({ success: true, path: resolved.resolved });
      }

      await fsPromises.mkdir(path.dirname(writePath), { recursive: true });

      // Atomic write: write to temp then rename to avoid concurrent readers
      // seeing an empty file during the O_TRUNC window of direct writeFile.
      const tmp = `${writePath}.tmp-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      try {
        await fsPromises.writeFile(tmp, content, 'utf8');
        await fsPromises.rename(tmp, writePath);
      } catch (error) {
        await fsPromises.unlink(tmp).catch(() => {});
        throw error;
      }
      return res.json({ success: true, path: resolved.resolved });
    } catch (error) {
      const err = error;
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access denied');
      }
      console.error('Failed to write file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to write file' });
    }
  });

  /**
   * POST /api/fs/upload — 以 application/octet-stream 流式上传二进制文件。
   * query: path、overwrite。Content-Length 或实收字节数超限返回 413 并排空请求体；
   * 父目录 realpath 后做规范化围栏复查。先写入同目录 .upload-<uuid> 临时文件，
   * overwrite 时用 rename 覆盖，否则用硬链接提交（不会覆盖存在性检查之后
   * 新出现的同名文件），读端因此永远看不到半个文件。EEXIST、目录目标、
   * 父目录缺失分别映射 409、400、404。
   */
  app.post('/api/fs/upload', async (req, res) => {
    const filePath = typeof req.query?.path === 'string' ? req.query.path.trim() : '';
    const overwrite = req.query?.overwrite === 'true';
    if (!filePath) {
      return res.status(400).json({ error: 'Path is required' });
    }
    if (!String(req.headers?.['content-type'] || '').toLowerCase().startsWith('application/octet-stream')) {
      return res.status(415).json({ error: 'Content-Type must be application/octet-stream' });
    }

    const maxUploadBytes = createUploadMaxBytes();
    const declaredSize = Number(req.headers?.['content-length']);
    if (Number.isFinite(declaredSize) && declaredSize > maxUploadBytes) {
      req.resume?.();
      return res.status(413).json({ error: `File exceeds maximum size of ${maxUploadBytes} bytes` });
    }

    try {
      const resolved = await resolveWorkspacePathFromContext({
        req,
        targetPath: filePath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        return res.status(400).json({ error: resolved.error });
      }

      const canonicalBase = await fsPromises.realpath(resolved.base).catch(() => path.resolve(resolved.base));
      const requestedParent = path.dirname(resolved.resolved);
      const canonicalParent = await fsPromises.realpath(requestedParent);
      if (!isPathWithinRoot(canonicalParent, canonicalBase, path, os)) {
        return res.status(403).json({ error: 'Access denied' });
      }

      const existingPath = await fsPromises.realpath(resolved.resolved).catch((error) => {
        if (error && typeof error === 'object' && error.code === 'ENOENT') {
          return null;
        }
        throw error;
      });
      const writePath = existingPath || path.join(canonicalParent, path.basename(resolved.resolved));
      if (!isPathWithinRoot(writePath, canonicalBase, path, os)) {
        return res.status(403).json({ error: 'Access denied' });
      }

      if (existingPath) {
        const stats = await fsPromises.stat(existingPath);
        if (stats.isDirectory()) {
          return res.status(400).json({ error: 'Specified path is a directory' });
        }
        if (!overwrite) {
          req.resume?.();
          return res.status(409).json({ error: 'File already exists', reason: 'already-exists' });
        }
      }

      const tmp = `${writePath}.upload-${crypto.randomUUID()}`;
      let tempExists = false;
      try {
        const handle = await fsPromises.open(tmp, 'wx');
        tempExists = true;
        let streamError = null;
        try {
          await streamUploadBody(req, handle, maxUploadBytes);
        } catch (error) {
          streamError = error;
        }
        try {
          await handle.close();
        } catch (error) {
          if (!streamError) throw error;
        }
        if (streamError) throw streamError;

        if (overwrite) {
          await fsPromises.rename(tmp, writePath);
        } else {
          // A same-directory hard link commits without replacing a target that
          // appeared after the existence check. The temp file is already fully
          // flushed, so readers never observe a partial upload.
          await fsPromises.link(tmp, writePath);
          await fsPromises.unlink(tmp).catch(() => {});
        }
        tempExists = false;
      } catch (error) {
        if (tempExists) {
          await fsPromises.unlink(tmp).catch(() => {});
        }
        throw error;
      }

      return res.json({ success: true, path: resolved.resolved });
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'EEXIST') {
        return res.status(409).json({ error: 'File already exists', reason: 'already-exists' });
      }
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'Destination directory not found', reason: 'not-found' });
      }
      if (err && typeof err === 'object' && err.uploadTooLarge) {
        return res.status(413).json({ error: `File exceeds maximum size of ${maxUploadBytes} bytes` });
      }
      if (err && typeof err === 'object' && (err.code === 'EISDIR' || err.code === 'ENOTDIR')) {
        return res.status(400).json({ error: 'Specified path is a directory' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access denied');
      }
      console.error('Failed to upload file:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to upload file' });
    }
  });

  /**
   * POST /api/fs/delete — 递归删除文件或目录（rm 的 recursive + force）。
   * 路径必须通过工作区围栏校验；ENOENT 返回 404，OS 权限错误返回 403。
   */
  app.post('/api/fs/delete', async (req, res) => {
    const { path: targetPath } = req.body || {};
    if (!targetPath || typeof targetPath !== 'string') {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = await resolveWorkspacePathFromContext({
        req,
        targetPath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        return res.status(400).json({ error: resolved.error });
      }

      await fsPromises.rm(resolved.resolved, { recursive: true, force: true });
      return res.json({ success: true, path: resolved.resolved });
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'File or directory not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access denied');
      }
      console.error('Failed to delete path:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to delete path' });
    }
  });

  /**
   * POST /api/fs/rename — 移动或重命名。body: { oldPath, newPath }。
   * 两端分别做工作区围栏校验，且解析出的 base 必须一致（同一工作区根），
   * 否则返回 400；源不存在返回 404。
   */
  app.post('/api/fs/rename', async (req, res) => {
    const { oldPath, newPath } = req.body || {};
    if (!oldPath || typeof oldPath !== 'string') {
      return res.status(400).json({ error: 'oldPath is required' });
    }
    if (!newPath || typeof newPath !== 'string') {
      return res.status(400).json({ error: 'newPath is required' });
    }

    try {
      const resolvedOld = await resolveWorkspacePathFromContext({
        req,
        targetPath: oldPath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolvedOld.ok) {
        return res.status(400).json({ error: resolvedOld.error });
      }

      const resolvedNew = await resolveWorkspacePathFromContext({
        req,
        targetPath: newPath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolvedNew.ok) {
        return res.status(400).json({ error: resolvedNew.error });
      }

      if (resolvedOld.base !== resolvedNew.base) {
        return res.status(400).json({ error: 'Source and destination must share the same workspace root' });
      }

      await fsPromises.rename(resolvedOld.resolved, resolvedNew.resolved);
      return res.json({ success: true, path: resolvedNew.resolved });
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'Source path not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access denied');
      }
      console.error('Failed to rename path:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to rename path' });
    }
  });

  /**
   * POST /api/fs/reveal — 在系统文件管理器中显示路径。body: { path }。
   * darwin 用 open（文件加 -R 定位）；win32 经 PowerShell 启动 explorer.exe
   * （文件用 /select, 定位，路径中的单引号转义为两个单引号）；其余平台用
   * xdg-open 打开所在目录。路径不存在返回 404。
   */
  app.post('/api/fs/reveal', async (req, res) => {
    const { path: targetPath } = req.body || {};
    if (!targetPath || typeof targetPath !== 'string') {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = path.resolve(targetPath.trim());
      await fsPromises.access(resolved);

      if (platform === 'darwin') {
        const stat = await fsPromises.stat(resolved);
        if (stat.isDirectory()) {
          await spawnDetached('open', [resolved]);
        } else {
          await spawnDetached('open', ['-R', resolved]);
        }
      } else if (platform === 'win32') {
        const stat = await fsPromises.stat(resolved);
        const escapedPath = resolved.replace(/'/g, "''");
        const explorerArg = stat.isDirectory() ? escapedPath : `/select,${escapedPath}`;
        const command = `Start-Process -FilePath explorer.exe -ArgumentList '${explorerArg}'`;
        await new Promise((resolve, reject) => {
          const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
            windowsHide: true,
            stdio: 'ignore',
          });
          child.once('error', reject);
          child.once('exit', (code) => {
            if (code === 0) {
              resolve();
              return;
            }
            reject(new Error(`Explorer launch failed with code ${code ?? 'unknown'}`));
          });
        });
      } else {
        const stat = await fsPromises.stat(resolved);
        const dir = stat.isDirectory() ? resolved : path.dirname(resolved);
        await spawnDetached('xdg-open', [dir]);
      }

      return res.json({ success: true, path: resolved });
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return res.status(404).json({ error: 'Path not found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to path denied');
      }
      console.error('Failed to reveal path:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to reveal path' });
    }
  });

  /**
   * POST /api/fs/exec — 在工作区内的目录执行 shell 命令序列。
   * body: { commands: string[], cwd, background }；background 请求被 400 拒绝。
   * cwd 必须通过围栏校验且是目录。先创建 execJobs 任务对象再同步执行，
   * 返回 { jobId, status, success, results }；git rev-parse 类只读命令走
   * TTL+LRU 缓存（见 runCommandWithGitReadCache）。
   */
  app.post('/api/fs/exec', async (req, res) => {
    const { commands, cwd, background } = req.body || {};
    if (!Array.isArray(commands) || commands.length === 0) {
      return res.status(400).json({ error: 'Commands array is required' });
    }
    if (!cwd || typeof cwd !== 'string') {
      return res.status(400).json({ error: 'Working directory (cwd) is required' });
    }

    pruneExecJobs();
    pruneGitReadCache();

    try {
      if (background === true) {
        console.warn('Rejected background /api/fs/exec request');
        return res.status(400).json({ error: 'Background command execution is not allowed' });
      }
      const resolvedCwdCandidate = path.resolve(normalizeDirectoryPath(cwd));
      const resolvedForWorkspace = await resolveWorkspacePathFromContext({
        req,
        targetPath: resolvedCwdCandidate,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolvedForWorkspace.ok) {
        console.warn(`Rejected /api/fs/exec outside workspace: ${resolvedForWorkspace.error}`);
        return res.status(403).json({ error: resolvedForWorkspace.error });
      }
      const resolvedCwd = resolvedForWorkspace.resolved;
      const stats = await fsPromises.stat(resolvedCwd);
      if (!stats.isDirectory()) {
        return res.status(400).json({ error: 'Specified cwd is not a directory' });
      }

      const shell = process.env.SHELL || (process.platform === 'win32' ? 'cmd.exe' : '/bin/sh');
      const shellFlag = process.platform === 'win32' ? '/c' : '-c';

      const jobId = crypto.randomUUID();
      const job = {
        jobId,
        status: 'queued',
        success: null,
        commands,
        resolvedCwd,
        shell,
        shellFlag,
        results: [],
        startedAt: Date.now(),
        finishedAt: null,
        updatedAt: Date.now(),
      };

      execJobs.set(jobId, job);

      const isBackground = false;
      if (isBackground) {
        void runExecJob(job).catch((error) => {
          job.status = 'done';
          job.success = false;
          job.results = Array.isArray(job.results) ? job.results : [];
          job.results.push({
            command: '',
            success: false,
            error: (error && error.message) || 'Command execution failed',
          });
          job.finishedAt = Date.now();
          job.updatedAt = Date.now();
        });

        return res.status(202).json({
          jobId,
          status: 'running',
        });
      }

      await runExecJob(job);
      return res.json({
        jobId,
        status: job.status,
        success: job.success === true,
        results: job.results,
      });
    } catch (error) {
      console.error('Failed to execute commands:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to execute commands' });
    }
  });

  /**
   * GET /api/fs/exec/:jobId — 查询 exec 任务的状态与结果（后台任务轮询用）。
   * 每次查询刷新 job.updatedAt 以延续其 TTL；未知 jobId 返回 404。
   */
  app.get('/api/fs/exec/:jobId', (req, res) => {
    const jobId = typeof req.params?.jobId === 'string' ? req.params.jobId : '';
    if (!jobId) {
      return res.status(400).json({ error: 'Job id is required' });
    }

    pruneExecJobs();

    const job = execJobs.get(jobId);
    if (!job) {
      return res.status(404).json({ error: 'Job not found' });
    }

    job.updatedAt = Date.now();
    return res.json({
      jobId: job.jobId,
      status: job.status,
      success: job.success === true,
      results: Array.isArray(job.results) ? job.results : [],
    });
  });

  /**
   * GET /api/fs/list — 列出目录项 { name, path, isDirectory, isFile, isSymbolicLink }。
   * query: path（缺省家目录）、respectGitignore=true 时用 git check-ignore
   * 过滤被忽略条目（带超时，失败或超时静默降级为不过滤）。symlink 指向目录时
   * 补充 stat 判定 isDirectory。返回的条目路径保持调用方的逻辑路径空间
   * （realpath 只用于读取目录内容），否则通过 symlink 展开时文件树会被
   * UI 的围栏校验拒绝。.opencode/plans 目录不存在时返回空列表而非 404。
   */
  app.get('/api/fs/list', async (req, res) => {
    const rawPath = typeof req.query.path === 'string' && req.query.path.trim().length > 0
      ? req.query.path.trim()
      : os.homedir();
    const respectGitignore = req.query.respectGitignore === 'true';
    // Logical (requested) path stays in the caller's path space. Realpath is
    // only used to read directory contents — returning real paths for entries
    // breaks file-tree expansion when listing through a symlink, because the
    // UI rejects expanded paths that fall outside the workspace root.
    let requestedPath = '';
    let resolvedPath = '';

    /** 判断路径（统一斜杠并去掉结尾斜杠后）是否为 .opencode/plans 目录。 */
    const isPlansDirectory = (value) => {
      if (!value || typeof value !== 'string') return false;
      const normalized = value.replace(/\\/g, '/').replace(/\/+$/, '');
      return normalized.endsWith('/.opencode/plans') || normalized.endsWith('.opencode/plans');
    };

    try {
      requestedPath = path.resolve(normalizeDirectoryPath(rawPath));
      resolvedPath = await realpathCache.resolve(requestedPath);

      const stats = await fsPromises.stat(resolvedPath);
      if (!stats.isDirectory()) {
        return res.status(400).json({ error: 'Specified path is not a directory', reason: 'not-directory' });
      }

      const dirents = await fsPromises.readdir(resolvedPath, { withFileTypes: true });
      let ignoredPaths = new Set();
      if (respectGitignore) {
        try {
          const pathsToCheck = dirents.map((d) => d.name);
          if (pathsToCheck.length > 0) {
            try {
              const result = await new Promise((resolve) => {
                const child = spawn(resolveGitBinaryForSpawn(), ['check-ignore', '--', ...pathsToCheck], {
                  cwd: resolvedPath,
                  windowsHide: true,
                  stdio: ['ignore', 'pipe', 'pipe'],
                });

                let stdout = '';
                let settled = false;
                let timeout = null;
                /** 幂等收口：保证只结算一次，清掉超时定时器后以累计的 stdout resolve。 */
                const finish = (value) => {
                  if (settled) return;
                  settled = true;
                  if (timeout) clearTimeout(timeout);
                  resolve(value);
                };

                if (gitCheckIgnoreTimeoutMs > 0) {
                  timeout = setTimeout(() => {
                    try {
                      child.kill('SIGKILL');
                    } catch {
                    }
                    finish('');
                  }, gitCheckIgnoreTimeoutMs);
                }

                child.stdout.on('data', (data) => { stdout += data.toString(); });
                child.on('close', () => finish(stdout));
                child.on('error', () => finish(''));
              });

              result.split('\n').filter(Boolean).forEach((name) => {
                const fullPath = path.join(resolvedPath, name.trim());
                ignoredPaths.add(fullPath);
              });
            } catch {
            }
          }
        } catch {
        }
      }

      const entries = await Promise.all(
        dirents.map(async (dirent) => {
          const physicalEntryPath = path.join(resolvedPath, dirent.name);
          if (respectGitignore && ignoredPaths.has(physicalEntryPath)) {
            return null;
          }

          let isDirectory = dirent.isDirectory();
          const isSymbolicLink = dirent.isSymbolicLink();

          if (!isDirectory && isSymbolicLink) {
            try {
              const linkStats = await fsPromises.stat(physicalEntryPath);
              isDirectory = linkStats.isDirectory();
            } catch {
              isDirectory = false;
            }
          }

          return {
            name: dirent.name,
            path: path.join(requestedPath, dirent.name),
            isDirectory,
            isFile: dirent.isFile(),
            isSymbolicLink,
          };
        })
      );

      return res.json({
        path: requestedPath,
        entries: entries.filter(Boolean),
      });
    } catch (error) {
      const err = error;
      const code = err && typeof err === 'object' && 'code' in err ? err.code : undefined;
      const isPlansPath = code === 'ENOENT' && (
        isPlansDirectory(resolvedPath)
        || isPlansDirectory(requestedPath)
        || isPlansDirectory(rawPath)
      );
      if (code !== 'ENOENT') {
        console.error('Failed to list directory:', error);
      }
      if (code === 'ENOENT') {
        if (isPlansPath) {
          return res.json({ path: requestedPath || resolvedPath || rawPath, entries: [] });
        }
        return res.status(404).json({ error: 'Directory not found', reason: 'not-found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to directory denied');
      }
      return res.status(500).json({ error: (error && error.message) || 'Failed to list directory' });
    }
  });

  /**
   * GET /api/fs/git-dirs — 扫描工作区内（含用户配置根）的嵌套 git 仓库，
   * 供仓库选择器使用。query: path 必填且必须是目录；结果受 GIT_DIRS_* 的
   * 深度与访问数量上限约束。目录不存在返回 404，权限错误返回 403。
   */
  app.get('/api/fs/git-dirs', async (req, res) => {
    const rawPath = typeof req.query.path === 'string' && req.query.path.trim().length > 0
      ? req.query.path.trim()
      : '';
    if (!rawPath) {
      return res.status(400).json({ error: 'Path is required' });
    }

    try {
      const resolved = await resolveWorkspacePathFromContext({
        req,
        targetPath: rawPath,
        resolveProjectDirectory,
        path,
        os,
        normalizeDirectoryPath,
        ompchamberUserConfigRoot,
      });
      if (!resolved.ok) {
        return res.status(400).json({ error: resolved.error });
      }

      const stats = await fsPromises.stat(resolved.resolved);
      if (!stats.isDirectory()) {
        return res.status(400).json({ error: 'Specified path is not a directory', reason: 'not-directory' });
      }

      const repositories = await findGitDirectories({
        rootPath: resolved.resolved,
        fsPromises,
        path,
        maxDepth: GIT_DIRS_MAX_DEPTH,
        maxDirs: GIT_DIRS_MAX_DIRS,
      });

      return res.json({
        path: resolved.resolved,
        repositories: repositories.map((repoPath) => ({
          path: repoPath,
          name: path.basename(repoPath),
        })),
      });
    } catch (error) {
      const err = error;
      const code = err && typeof err === 'object' && 'code' in err ? err.code : undefined;
      if (code === 'ENOENT') {
        return res.status(404).json({ error: 'Directory not found', reason: 'not-found' });
      }
      if (isOsPermissionError(err)) {
        return sendOsPermissionDenied(res, 'Access to directory denied');
      }
      console.error('Failed to find git directories:', error);
      return res.status(500).json({ error: (error && error.message) || 'Failed to find git directories' });
    }
  });
};
