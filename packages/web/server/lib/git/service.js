/**
 * Git 服务层（Node/express web server 的参考实现）：
 * 基于 simple-git 与裸 git CLI，为 OMPChamber 提供全部 Git 能力——
 * 状态/差异/日志查询、暂存与提交、分支与远端管理、stash、merge/rebase、
 * 以及 OpenCode 风格 worktree 的创建与 bootstrap 状态机。
 *
 * 结构概览：
 * - git binary 解析（Windows 安装路径探测）与 simple-git 实例工厂；
 * - SSH agent（含 gpg-agent）环境注入与仓库身份/签名配置；
 * - index 变更队列：同一仓库的写操作串行化，规避 index.lock 竞态；
 * - worktree 管理：名称候选、目录解析、快速创建（后台挂接）、bootstrap 状态轮询；
 * - integrate 流程：把 worktree 分支上的提交 cherry-pick 搬运到目标分支，
 *   支持计划计算、冲突详情、中止与续跑。
 */
import simpleGit from 'simple-git';
import fs from 'fs';
import path from 'path';
import os from 'os';
import { execFile } from 'child_process';
import { promisify } from 'util';
import { createRequire } from 'module';

/** fs 的 promise 化 API 别名。 */
const fsp = fs.promises;
/** ESM 内构造的 CommonJS require（createRequire），供按需加载 CJS 依赖。 */
const require = createRequire(import.meta.url);
/** promisify 的 execFile：直接执行外部命令（git、gpgconf、hook、启动脚本等）。 */
const execFileAsync = promisify(execFile);
/** gpgconf 候选命令：PATH 中的 gpgconf 与 macOS Homebrew 常见安装路径。 */
const gpgconfCandidates = ['gpgconf', '/opt/homebrew/bin/gpgconf', '/usr/local/bin/gpgconf'];
/** 已解析的 Windows git 绝对路径缓存；null 表示尚未解析（非 Windows 平台不使用）。 */
let resolvedGitBinary = null;
/** 目录 → worktree bootstrap 状态表（status/phase/error/updatedAt）。 */
const worktreeBootstrapState = new Map();
/** 目录 → 进行中的 bootstrap 任务 Promise 表（删除 worktree 前先等待对应任务结束）。 */
const activeWorktreeBootstrapTasks = new Map();
/** remote 存在性缓存：key 为目录与 remote 名，value 为 { exists, checkedAt }。 */
const remoteExistenceCache = new Map();
/** simple-git 视为"安全"的自定义 binary 路径白名单正则（可选盘符 + 受限字符集）。 */
const SIMPLE_GIT_SAFE_BINARY_PATTERN = /^([a-z]:)?([a-z0-9/.\\_~-]+)$/i;
/** simple-git 对不安全 binary 路径输出的固定告警文案，用于精准过滤。 */
const SIMPLE_GIT_UNSAFE_BINARY_WARNING = 'Invalid value supplied for custom binary, restricted characters must be removed';
/** remote 存在性缓存的存活时间（毫秒）。 */
const REMOTE_EXISTENCE_CACHE_TTL_MS = 30_000;
/** 各仓库的 index 变更串行队列表：key 为 repo root，value 为链式 Promise 尾部。 */
const gitIndexMutationQueues = new Map();

/** bootstrap 状态值：进行中。 */
const WORKTREE_BOOTSTRAP_PENDING = 'pending';
/** bootstrap 状态值：全部阶段完成。 */
const WORKTREE_BOOTSTRAP_READY = 'ready';
/** bootstrap 状态值：失败（error 字段记录原因）。 */
const WORKTREE_BOOTSTRAP_FAILED = 'failed';
/** bootstrap 阶段值：目录已创建、git 挂接进行中。 */
const WORKTREE_BOOTSTRAP_PHASE_DIRECTORY_CREATED = 'directory-created';
/** bootstrap 阶段值：文件填充与 post-checkout 完成。 */
const WORKTREE_BOOTSTRAP_PHASE_GIT_READY = 'git-ready';
/** bootstrap 阶段值：启动脚本执行完毕。 */
const WORKTREE_BOOTSTRAP_PHASE_SETUP_READY = 'setup-ready';
/** git 全零 null ref（40 个 '0'）：post-checkout hook 的"无前驱 HEAD"参数。 */
const GIT_NULL_REF = '0'.repeat(40);
/** index.lock 冲突后第一次重试的等待毫秒数。 */
const WORKTREE_INDEX_LOCK_RETRY_DELAY_MS = 250;
/** 判定 index.lock 为陈旧锁前，观察锁文件是否变化的等待毫秒数。 */
const WORKTREE_INDEX_LOCK_STALE_DELAY_MS = 750;

// Patch stdout that clients parse must come from git's own diff engine: a
// user's `diff.external` / GIT_EXTERNAL_DIFF (difftastic, delta, …) replaces
// the patch with a human-facing format no patch parser can read, so the diff
// surfaces would render an empty frame instead of failing.
/** （中文补充）所有 patch 输出统一附加该参数以禁用外部 diff 工具，保证输出可被解析（见上方英文说明）。 */
const NO_EXT_DIFF = '--no-ext-diff';

/** 把目录归一为 bootstrap 状态表的 key（绝对路径）；无效输入返回空串。 */
const toBootstrapStateKey = (directory) => {
  const normalized = normalizeDirectoryPath(directory);
  if (!normalized) {
    return '';
  }
  return path.resolve(normalized);
};

/** 构造一条 bootstrap 状态记录（纯对象）：status/phase/error（空白归一为 null）/updatedAt。 */
const createWorktreeBootstrapState = (status, phase, error = null) => ({
  status,
  phase,
  error: typeof error === 'string' && error.trim().length > 0 ? error.trim() : null,
  updatedAt: Date.now(),
});

/** 写入/更新目录的 bootstrap 状态（附时间戳）；目录无效返回 null 且不写表。 */
const setWorktreeBootstrapState = (directory, status, phase, error = null) => {
  const key = toBootstrapStateKey(directory);
  if (!key) {
    return null;
  }
  const state = createWorktreeBootstrapState(status, phase, error);
  worktreeBootstrapState.set(key, state);
  return state;
};

/** 删除目录的 bootstrap 状态登记（worktree 移除或无需跟踪时调用）。 */
const clearWorktreeBootstrapState = (directory) => {
  const key = toBootstrapStateKey(directory);
  if (!key) {
    return;
  }
  worktreeBootstrapState.delete(key);
};

/** 登记目录正在运行的 bootstrap Promise，settle 后自动移除登记；返回原 task 便于链式调用。 */
const trackWorktreeBootstrapTask = (directory, task) => {
  const key = toBootstrapStateKey(directory);
  if (!key) {
    return task;
  }

  activeWorktreeBootstrapTasks.set(key, task);
  const clearTask = () => {
    if (activeWorktreeBootstrapTasks.get(key) === task) {
      activeWorktreeBootstrapTasks.delete(key);
    }
  };
  void task.then(clearTask, clearTask);
  return task;
};

/** 循环等待目录上已登记的 bootstrap 任务全部结束（容忍期间新任务入表）；任务自身的失败被吞掉。 */
const waitForActiveWorktreeBootstrap = async (directory) => {
  const key = toBootstrapStateKey(directory);
  if (!key) {
    return;
  }

  while (true) {
    const task = activeWorktreeBootstrapTasks.get(key);
    if (!task) {
      return;
    }
    await task.catch(() => undefined);
  }
};

/**
 * 判断候选是否为可执行文件：Windows 按扩展名（.exe/.cmd/.bat/.com 或无
 * 扩展名）；POSIX 用 access(X_OK)。stat/access 异常一律按不可执行处理。
 */
const isExecutableFile = (candidate) => {
  if (typeof candidate !== 'string' || candidate.trim().length === 0) {
    return false;
  }
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isFile()) {
      return false;
    }
    if (process.platform === 'win32') {
      const ext = path.extname(candidate).toLowerCase();
      return ext.length === 0 || ext === '.exe' || ext === '.cmd' || ext === '.bat' || ext === '.com';
    }
    fs.accessSync(candidate, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
};

/**
 * 归一 git 可执行候选路径：trim；Windows 的 .cmd/.bat/.com 包装器优先替换为
 * 同名 .exe（simple-git 无法直接 spawn 包装器，且替换目标必须真实存在）；
 * 其余原样返回，非字符串返回 null。
 */
const normalizeGitExecutableCandidate = (candidate) => {
  if (typeof candidate !== 'string') {
    return null;
  }
  const trimmed = candidate.trim();
  if (!trimmed) {
    return null;
  }

  const ext = path.extname(trimmed).toLowerCase();
  if (ext === '.cmd' || ext === '.bat' || ext === '.com') {
    const exeCandidate = trimmed.slice(0, -ext.length) + '.exe';
    if (isExecutableFile(exeCandidate)) {
      return exeCandidate;
    }
  }

  return trimmed;
};

/** 判断 binary 路径是否匹配 simple-git 的安全字符白名单正则。 */
const isSafeSimpleGitBinary = (candidate) => (
  typeof candidate === 'string' && SIMPLE_GIT_SAFE_BINARY_PATTERN.test(candidate)
);

/**
 * 构造 simple-git 实例：需要放行不安全自定义 binary 时临时接管 console.warn，
 * 吞掉 simple-git 的固定告警文案（其余告警照常输出），构造完成后恢复。
 */
const createSimpleGit = (options) => {
  if (!options?.unsafe?.allowUnsafeCustomBinary) {
    return simpleGit(options);
  }

  const originalWarn = console.warn;
  console.warn = (...args) => {
    if (String(args[0] || '').includes(SIMPLE_GIT_UNSAFE_BINARY_WARNING)) {
      return;
    }
    originalWarn(...args);
  };

  try {
    return simpleGit(options);
  } finally {
    console.warn = originalWarn;
  }
};

/** 按 PATH 各分段（去重、跳过空段）拼出 binaryName 的候选绝对路径列表。 */
const listPathExecutableCandidates = (binaryName) => {
  const currentPath = process.env.PATH || '';
  const seen = new Set();
  const matches = [];
  for (const segment of currentPath.split(path.delimiter)) {
    const dir = typeof segment === 'string' ? segment.trim() : '';
    if (!dir || seen.has(dir)) {
      continue;
    }
    seen.add(dir);
    matches.push(path.join(dir, binaryName));
  }
  return matches;
};

/** 列举 Windows 常见 Git 安装位置（Program Files 等目录下的 cmd/bin/mingw64）中的 git.exe 候选。 */
const listWindowsGitInstallCandidates = () => {
  const roots = [
    process.env.ProgramFiles,
    process.env['ProgramFiles(x86)'],
    process.env.LocalAppData,
  ]
    .map((value) => (typeof value === 'string' ? value.trim() : ''))
    .filter(Boolean);

  const candidates = [];
  for (const root of roots) {
    candidates.push(path.join(root, 'Git', 'cmd', 'git.exe'));
    candidates.push(path.join(root, 'Git', 'bin', 'git.exe'));
    candidates.push(path.join(root, 'Git', 'mingw64', 'bin', 'git.exe'));
    candidates.push(path.join(root, 'Programs', 'Git', 'cmd', 'git.exe'));
    candidates.push(path.join(root, 'Programs', 'Git', 'bin', 'git.exe'));
  }
  return candidates;
};

/**
 * 解析应使用的 git 命令：非 Windows 直接 'git'；Windows 依次尝试
 * GIT_BINARY / OMPCHAMBER_GIT_BINARY 环境变量 → PATH 中的 git（此时直接用
 * 'git'）→ 常见安装目录的 git.exe（优先路径字符安全的 .exe）。
 * 结果缓存于 resolvedGitBinary，进程内只解析一次。
 */
const resolveGitBinary = () => {
  if (process.platform !== 'win32') {
    return 'git';
  }
  if (resolvedGitBinary) {
    return resolvedGitBinary;
  }

  const explicit = [process.env.GIT_BINARY, process.env.OMPCHAMBER_GIT_BINARY]
    .map((value) => (typeof value === 'string' ? value.trim() : ''))
    .filter(Boolean);
  for (const candidate of explicit) {
    const normalized = normalizeGitExecutableCandidate(candidate);
    if (isExecutableFile(normalized)) {
      resolvedGitBinary = normalized;
      return resolvedGitBinary;
    }
  }

  const pathDiscovered = [
    ...listPathExecutableCandidates('git.exe'),
    ...listPathExecutableCandidates('git'),
  ]
    .map(normalizeGitExecutableCandidate)
    .filter(Boolean)
    .filter((candidate) => isExecutableFile(candidate));
  if (pathDiscovered.length > 0) {
    resolvedGitBinary = 'git';
    return resolvedGitBinary;
  }

  const discovered = [
    ...listWindowsGitInstallCandidates(),
  ]
    .map(normalizeGitExecutableCandidate)
    .filter(Boolean)
    .filter((candidate) => isExecutableFile(candidate));

  const preferredExe = discovered.find((candidate) => isSafeSimpleGitBinary(candidate) && candidate.toLowerCase().endsWith('.exe'))
    || discovered.find((candidate) => candidate.toLowerCase().endsWith('.exe'));
  resolvedGitBinary = preferredExe || discovered[0] || 'git.exe';
  return resolvedGitBinary;
};

/** 取当前平台应使用的 git 命令（resolveGitBinary 的直通别名）。 */
const getGitBinary = () => resolveGitBinary();

/**
 * Escape an SSH key path for use in core.sshCommand.
 * Handles Windows/Unix differences and prevents command injection.
 */
/**
 * （中文补充）转义 SSH 私钥路径供 core.sshCommand 使用：Windows 转为 MSYS
 * 风格的 /c/... 路径；含 shell 元字符（引号、$、分号、竖线等）直接抛错防
 * 命令注入；结果用单引号包裹（Unix 内嵌单引号额外转义）。
 */
function escapeSshKeyPath(sshKeyPath) {
  const isWindows = process.platform === 'win32';
  
  // Normalize path first on Windows (convert backslashes to forward slashes)
  let normalizedPath = sshKeyPath;
  if (isWindows) {
    normalizedPath = sshKeyPath.replace(/\\/g, '/');
  }
  
  // Validate: reject paths with characters that could enable injection
  // Allow only alphanumeric, path separators, dots, dashes, underscores, spaces, and colons (for Windows drives)
  // Note: backslash is not in this list since we've already normalized Windows paths
  const dangerousChars = /[`$!"';&|<>(){}[\]*?#~]/;
  if (dangerousChars.test(normalizedPath)) {
    throw new Error(`SSH key path contains invalid characters: ${sshKeyPath}`);
  }

  if (isWindows) {
    // On Windows, Git (via MSYS/MinGW) expects Unix-style paths
    // Convert "C:/path" to "/c/path" for MSYS compatibility
    let unixPath = normalizedPath;
    const driveMatch = unixPath.match(/^([A-Za-z]):\//);
    if (driveMatch) {
      unixPath = `/${driveMatch[1].toLowerCase()}${unixPath.slice(2)}`;
    }
    
    // Use single quotes for the path (prevents shell interpretation)
    return `'${unixPath}'`;
  } else {
    // On Unix, use single quotes and escape any single quotes in the path
    // Single quotes prevent all shell interpretation except for single quotes themselves
    const escaped = normalizedPath.replace(/'/g, "'\\''");
    return `'${escaped}'`;
  }
}

/**
 * Build the SSH command string for git config
 */
/** （中文补充）拼装 core.sshCommand 命令串：ssh -i 转义后的密钥路径 -o IdentitiesOnly=yes，强制只用指定密钥。 */
function buildSshCommand(sshKeyPath) {
  const escapedPath = escapeSshKeyPath(sshKeyPath);
  return `ssh -i ${escapedPath} -o IdentitiesOnly=yes`;
}

/** 判断路径是否为 Unix domain socket（stat.isSocket）；异常按否处理。 */
const isSocketPath = async (candidate) => {
  if (!candidate || typeof candidate !== 'string') {
    return false;
  }
  try {
    const stat = await fsp.stat(candidate);
    return typeof stat.isSocket === 'function' && stat.isSocket();
  } catch {
    return false;
  }
};

/**
 * 解析 SSH_AUTH_SOCK：优先沿用现有环境变量；否则尝试 ~/.gnupg/S.gpg-agent.ssh
 * 与 `gpgconf --list-dirs agent-ssh-socket`（必要时 --launch gpg-agent 后重查）。
 * Windows 或全部失败返回 null。
 */
const resolveSshAuthSock = async () => {
  const existing = (process.env.SSH_AUTH_SOCK || '').trim();
  if (existing) {
    return existing;
  }

  if (process.platform === 'win32') {
    return null;
  }

  const gpgSock = path.join(os.homedir(), '.gnupg', 'S.gpg-agent.ssh');
  if (await isSocketPath(gpgSock)) {
    return gpgSock;
  }

  const runGpgconf = async (args) => {
    for (const candidate of gpgconfCandidates) {
      try {
        const { stdout } = await execFileAsync(candidate, args);
        return String(stdout || '');
      } catch {
        continue;
      }
    }
    return '';
  };

  const candidate = (await runGpgconf(['--list-dirs', 'agent-ssh-socket'])).trim();
  if (candidate && await isSocketPath(candidate)) {
    return candidate;
  }

  if (candidate) {
    await runGpgconf(['--launch', 'gpg-agent']);
    const retried = (await runGpgconf(['--list-dirs', 'agent-ssh-socket'])).trim();
    if (retried && await isSocketPath(retried)) {
      return retried;
    }
  }

  return null;
};

/** 构造 git 子进程环境：复制 process.env；SSH_AUTH_SOCK 缺失时补上 gpg-agent 解析结果。 */
const buildGitEnv = async () => {
  const env = { ...process.env };
  if (!env.SSH_AUTH_SOCK || !env.SSH_AUTH_SOCK.trim()) {
    const resolved = await resolveSshAuthSock();
    if (resolved) {
      env.SSH_AUTH_SOCK = resolved;
    }
  }
  return env;
};

/**
 * 核心工厂：为目录构造 simple-git 实例。显式绑定 baseDir（防继承
 * process.cwd() 导致误判非仓库）、注入 SSH agent 环境变量、使用平台解析的
 * git binary；自定义 binary 或显式允许时设置 unsafe 选项（并屏蔽相应告警）。
 * @param {string} directory 工作目录（支持 ~ 前缀）
 * @param {{allowUnsafeSshCommand?: boolean}} [options] 放行 core.sshCommand 中的特殊字符
 * @throws 目录为空时抛 'Git directory is required'
 */
const createGit = async (directory, { allowUnsafeSshCommand = false } = {}) => {
  const env = await buildGitEnv();
  const spawnOptions = { windowsHide: true };
  const binary = getGitBinary();
  const hasCustomBinary = typeof binary === 'string' && binary.trim() && binary !== 'git' && binary !== 'git.exe';
  const unsafe = hasCustomBinary || allowUnsafeSshCommand
    ? {
      ...(hasCustomBinary && { allowUnsafeCustomBinary: true }),
      ...(allowUnsafeSshCommand && { allowUnsafeSshCommand: true }),
    }
    : undefined;
  // Always pin simple-git to an explicit working directory. Omitting baseDir
  // makes simple-git use process.cwd(), which breaks when the OMPChamber
  // server was launched from a neutral directory (e.g. $HOME) and the opened
  // project lives elsewhere — session/project discovery then sees spurious
  // "not a git repository" errors and can abort enumeration.
  const baseDir = normalizeDirectoryPath(directory);
  if (typeof baseDir !== 'string' || !baseDir.trim()) {
    throw new Error('Git directory is required');
  }
  return createSimpleGit({
    baseDir,
    env,
    spawnOptions,
    binary,
    unsafe,
  });
};

// Global config reads do not need a repository; use the home directory as a
// stable baseDir so we never accidentally inherit process.cwd().
/** （中文补充）读全局配置用的 git 实例：以 home 为稳定 baseDir，避免继承 process.cwd()（见上方英文说明）。 */
const createGitForGlobalConfig = async () => createGit(os.homedir());

/** 归一目录字符串：trim、展开 ~ 与 ~/ 为 home 目录；非字符串原样返回。 */
const normalizeDirectoryPath = (value) => {
  if (typeof value !== 'string') {
    return value;
  }

  const trimmed = value.trim();
  if (!trimmed) {
    return trimmed;
  }

  if (trimmed === '~') {
    return os.homedir();
  }

  if (trimmed.startsWith('~/') || trimmed.startsWith('~\\')) {
    return path.join(os.homedir(), trimmed.slice(2));
  }

  return trimmed;
};

/** normalizeDirectoryPath 之后再把反斜杠统一为正斜杠（便于与 git 输出比较）。 */
const normalizePath = (value) => {
  const normalized = normalizeDirectoryPath(value);
  if (typeof normalized !== 'string') {
    return normalized;
  }
  return normalized.replace(/\\/g, '/');
};

/** 计算目录的 index 变更队列 key（绝对路径）；无效输入返回空串。 */
const getGitIndexMutationQueueKey = (directory) => {
  const normalized = normalizeDirectoryPath(directory);
  if (!normalized) {
    return '';
  }
  return path.resolve(normalized);
};

/**
 * 把 task 串行排入仓库的 index 变更队列：key 优先取 repo root（linked worktree
 * 与主仓库共享同一队列），解析失败退化为目录路径。同队列内的 stage/commit/
 * apply 等写操作依次执行，避免并发触发 index.lock；队列尾部空闲后自动清表。
 * @returns task 的结果；task 抛错正常向调用方传播且不打断后续排队任务
 */
const withGitIndexMutationQueue = async (directory, task) => {
  let key = getGitIndexMutationQueueKey(directory);
  try {
    const directoryPath = normalizeDirectoryPath(directory);
    if (directoryPath) {
      const git = await createGit(directoryPath);
      key = await resolveGitRepositoryRoot(directoryPath, git);
    }
  } catch {
    // Fall back to the normalized directory key when the repo root is unavailable.
  }
  if (!key) {
    return task();
  }

  const previous = gitIndexMutationQueues.get(key) || Promise.resolve();
  const current = previous.catch(() => {}).then(task);
  const tail = current.catch(() => {});
  gitIndexMutationQueues.set(key, tail);

  try {
    return await current;
  } finally {
    if (gitIndexMutationQueues.get(key) === tail) {
      gitIndexMutationQueues.delete(key);
    }
  }
};

/** 把单路径或路径数组归一为 trim、去空、去重后的字符串数组。 */
const normalizeFilePathList = (paths) => Array.from(new Set(
  (Array.isArray(paths) ? paths : [paths])
    .map((value) => String(value || '').trim())
    .filter(Boolean)
));

/** 路径穿越防护：每个文件 resolve 后必须等于仓库根或位于其下，否则抛 "Path is outside repository"。 */
const validateRepositoryFilePaths = (directoryPath, filePaths) => {
  const repoRoot = path.resolve(directoryPath);

  for (const filePath of filePaths) {
    const absoluteTarget = path.resolve(repoRoot, filePath);
    if (!absoluteTarget.startsWith(repoRoot + path.sep) && absoluteTarget !== repoRoot) {
      throw new Error(`Path is outside repository: ${filePath}`);
    }
  }
};

/** 把路径分隔符统一为 git 使用的正斜杠形式。 */
const toGitPath = (value) => value.replace(/\\/g, '/');

/** 判断 target 等于 root 或位于 root 目录树内（基于 path.relative 结果）。 */
const isInsideOrSameDirectory = (root, target) => {
  const relative = path.relative(root, target);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
};

/** 用 rev-parse --show-toplevel 解析仓库顶层绝对路径；git 给相对路径时按目录 resolve。 */
const resolveGitRepositoryRoot = async (directoryPath, git) => {
  const topLevel = await git.raw(['rev-parse', '--show-toplevel']);
  const normalizedTopLevel = topLevel.trim();
  return path.isAbsolute(normalizedTopLevel)
    ? path.resolve(normalizedTopLevel)
    : path.resolve(directoryPath, normalizedTopLevel);
};

/**
 * 建立仓库操作上下文：规范化目录、以该目录为 baseDir 建 git 实例、解析仓库根；
 * 目录不在仓库顶层时再以 repoRoot 为 baseDir 建主实例（多数命令需在根执行）。
 * @returns {{directoryPath: string, directoryGit: object, repoRoot: string, git: object}}
 * @throws 目录为空抛 'Git directory is required'；不在仓库内由 rev-parse 抛错
 */
const createRepositoryGitContext = async (directory) => {
  const directoryPath = normalizeDirectoryPath(directory);
  if (typeof directoryPath !== 'string' || !directoryPath.trim()) {
    throw new Error('Git directory is required');
  }
  const directoryGit = await createGit(directoryPath);
  const repoRoot = await resolveGitRepositoryRoot(directoryPath, directoryGit);
  const git = path.resolve(directoryPath) === repoRoot ? directoryGit : await createGit(repoRoot);
  return { directoryPath, directoryGit, repoRoot, git };
};

/**
 * Absolute repository root for a directory anywhere inside it. Callers that key
 * persisted data by repository need this so two directories in the same
 * repository do not address different records.
 */
/**
 * （中文补充）返回目录所在仓库的绝对根路径（见上方英文说明：按仓库键控
 * 持久化数据时必须使用，同仓库的两个子目录才能命中同一条记录）。
 */
export async function getRepositoryRoot(directory) {
  const { repoRoot } = await createRepositoryGitContext(directory);
  return repoRoot;
}

/** 解析 .git 内部条目（MERGE_MSG、rebase-merge 等）的绝对路径，兼容 linked worktree 的 .git 文件布局。 */
const resolveGitInternalPath = async (repoRoot, git, gitPath) => {
  const resolved = await git.raw(['rev-parse', '--git-path', gitPath]);
  return path.resolve(repoRoot, resolved.trim());
};

/**
 * 把用户提供的文件路径解析为仓库文件上下文：依次尝试相对 repoRoot 与相对
 * directoryPath 的绝对路径，候选须在仓库内且满足之一——工作区存在（文件或
 * 符号链接）、index 中存在、HEAD 中存在。
 * @returns {{absolutePath: string, repoPath: string, repoRoot: string, isSymbolicLink: boolean}}
 * @throws 无候选满足时抛 "Invalid file path"
 */
const resolveGitFileContext = async (directoryPath, git, filePath, repoRootOverride = null) => {
  const repoRoot = repoRootOverride || await resolveGitRepositoryRoot(directoryPath, git);
  const candidates = Array.from(new Set([
    path.resolve(repoRoot, filePath),
    path.resolve(directoryPath, filePath),
  ]));

  for (const absolutePath of candidates) {
    if (!isInsideOrSameDirectory(repoRoot, absolutePath)) {
      continue;
    }

    const repoPath = toGitPath(path.relative(repoRoot, absolutePath));
    const worktreeEntry = await fsp.lstat(absolutePath).catch(() => null);
    const isSymbolicLink = worktreeEntry?.isSymbolicLink() ?? false;
    const existsInWorktree = worktreeEntry?.isFile() || isSymbolicLink;
    const existsInIndex = await git.raw(['cat-file', '-e', `:${repoPath}`]).then(() => true).catch(() => false);
    const existsInHead = await git.raw(['cat-file', '-e', `HEAD:${repoPath}`]).then(() => true).catch(() => false);

    if (existsInWorktree || existsInIndex || existsInHead) {
      return {
        absolutePath,
        repoPath,
        repoRoot,
        isSymbolicLink,
      };
    }
  }

  throw new Error('Invalid file path');
};

/** 剥掉分支 ref 的 refs/heads/、heads/、refs/ 前缀返回裸分支名；空值原样返回。 */
const cleanBranchName = (branch) => {
  if (!branch) {
    return branch;
  }
  if (branch.startsWith('refs/heads/')) {
    return branch.substring('refs/heads/'.length);
  }
  if (branch.startsWith('heads/')) {
    return branch.substring('heads/'.length);
  }
  if (branch.startsWith('refs/')) {
    return branch.substring('refs/'.length);
  }
  return branch;
};

/** 随机 worktree 命名词库：形容词部分。 */
const OPENCODE_ADJECTIVES = [
  'brave',
  'calm',
  'clever',
  'cosmic',
  'crisp',
  'curious',
  'eager',
  'gentle',
  'glowing',
  'happy',
  'hidden',
  'jolly',
  'kind',
  'lucky',
  'mighty',
  'misty',
  'neon',
  'nimble',
  'playful',
  'proud',
  'quick',
  'quiet',
  'shiny',
  'silent',
  'stellar',
  'sunny',
  'swift',
  'tidy',
  'witty',
];

/** 随机 worktree 命名词库：名词部分。 */
const OPENCODE_NOUNS = [
  'cabin',
  'cactus',
  'canyon',
  'circuit',
  'comet',
  'eagle',
  'engine',
  'falcon',
  'forest',
  'garden',
  'harbor',
  'island',
  'knight',
  'lagoon',
  'meadow',
  'moon',
  'mountain',
  'nebula',
  'orchid',
  'otter',
  'panda',
  'pixel',
  'planet',
  'river',
  'rocket',
  'sailor',
  'squid',
  'star',
  'tiger',
  'wizard',
  'wolf',
];

/** worktree 名称候选生成上限（全部撞名则报错）。 */
const OPENCODE_WORKTREE_ATTEMPTS = 26;

/** 返回 OpenCode 数据目录（XDG_DATA_HOME，缺省 ~/.local/share 下的 opencode）。 */
const getOpenCodeDataPath = () => {
  const xdgDataHome = process.env.XDG_DATA_HOME || path.join(os.homedir(), '.local', 'share');
  return path.join(xdgDataHome, 'opencode');
};

/** 等概率随机取数组中的一个元素。 */
const pickRandom = (values) => values[Math.floor(Math.random() * values.length)];

/** 生成 "形容词-名词" 形式的 OpenCode 随机名（如 brave-falcon）。 */
const generateOpenCodeRandomName = () => `${pickRandom(OPENCODE_ADJECTIVES)}-${pickRandom(OPENCODE_NOUNS)}`;

/** 生成 worktree 名称 slug：去 refs 前缀、空白与路径分隔符转连字符、非法字符压缩为单连字符、去首尾连字符、限 80 字符。 */
const slugWorktreeName = (value) => {
  return String(value || '')
    .trim()
    .replace(/^refs\/heads\//, '')
    .replace(/^heads\//, '')
    .replace(/\s+/g, '-')
    .replace(/^\/+|\/+$/g, '')
    .split('/').join('-')
    .replace(/[^A-Za-z0-9._-]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-+/, '')
    .replace(/-+$/, '')
    .slice(0, 80);
};

/**
 * 解析 worktree list --porcelain 输出为条目数组：每条含 worktree 路径、head、
 * branchRef 与去前缀的 branch；空行结束当前条目，未知行忽略。
 */
const parseWorktreePorcelain = (raw) => {
  const lines = String(raw || '').split('\n').map((line) => line.trim());
  const entries = [];
  let current = null;

  for (const line of lines) {
    if (!line) {
      if (current?.worktree) {
        entries.push(current);
      }
      current = null;
      continue;
    }

    if (line.startsWith('worktree ')) {
      if (current?.worktree) {
        entries.push(current);
      }
      current = { worktree: line.substring('worktree '.length).trim() };
      continue;
    }

    if (!current) {
      continue;
    }

    if (line.startsWith('HEAD ')) {
      current.head = line.substring('HEAD '.length).trim();
      continue;
    }

    if (line.startsWith('branch ')) {
      const branchRef = line.substring('branch '.length).trim();
      current.branchRef = branchRef;
      current.branch = cleanBranchName(branchRef);
    }
  }

  if (current?.worktree) {
    entries.push(current);
  }

  return entries;
};

/**
 * 计算用于等值比较的规范路径：resolve + realpath（解析符号链接，失败回退）
 * + normalize；Windows 再转小写（大小写不敏感文件系统）。
 */
const canonicalPath = async (input) => {
  const absolutePath = path.resolve(input);
  const realPath = await fsp.realpath(absolutePath).catch(() => absolutePath);
  const normalized = path.normalize(realPath);
  return process.platform === 'win32' ? normalized.toLowerCase() : normalized;
};

/** 异步判断路径是否存在（stat 成功即存在）。 */
const checkPathExists = async (targetPath) => {
  try {
    await fsp.stat(targetPath);
    return true;
  } catch {
    return false;
  }
};

/** 归一 worktree 起点 ref：空值回退 'HEAD'，其余 trim。 */
const normalizeStartRef = (value) => {
  const trimmed = String(value || '').trim();
  if (!trimmed) {
    return 'HEAD';
  }
  return trimmed;
};

/** 校验字符串是否为 7-40 位十六进制 commit hash。 */
function isValidCommitHash(hash) {
  return typeof hash === 'string' && /^[0-9a-fA-F]{7,40}$/.test(hash);
}

/**
 * 把 "远程/分支"（含 refs/remotes/、remotes/ 前缀形式）解析为 remote 与 branch。
 * @returns {{remote, branch, remoteRef, fullRef}|null} 拆不出非空 remote/branch 时为 null
 */
const parseRemoteBranchRef = (value) => {
  const trimmed = String(value || '').trim();
  if (!trimmed) {
    return null;
  }

  if (trimmed.startsWith('refs/remotes/')) {
    const rest = trimmed.substring('refs/remotes/'.length);
    const slashIndex = rest.indexOf('/');
    if (slashIndex <= 0 || slashIndex === rest.length - 1) {
      return null;
    }
    return {
      remote: rest.slice(0, slashIndex),
      branch: rest.slice(slashIndex + 1),
      remoteRef: rest,
      fullRef: `refs/remotes/${rest}`,
    };
  }

  if (trimmed.startsWith('remotes/')) {
    return parseRemoteBranchRef(`refs/${trimmed}`);
  }

  const slashIndex = trimmed.indexOf('/');
  if (slashIndex <= 0 || slashIndex === trimmed.length - 1) {
    return null;
  }

  return {
    remote: trimmed.slice(0, slashIndex),
    branch: trimmed.slice(slashIndex + 1),
    remoteRef: trimmed,
    fullRef: `refs/remotes/${trimmed}`,
  };
};

/**
 * 判断输入是否指向"仅存在于远端的分支"：可解析为 远程/分支 且不存在同名本地
 * 分支时返回解析结果（显式 refs/remotes/、remotes/ 前缀总按远端处理）；
 * 其余返回 null。用于 worktree 创建时推断 upstream 起点。
 */
const resolveRemoteBranchRef = async (primaryWorktree, value) => {
  const raw = String(value || '').trim();
  const parsed = parseRemoteBranchRef(raw);
  if (!parsed) {
    return null;
  }

  if (raw.startsWith('refs/remotes/') || raw.startsWith('remotes/')) {
    return parsed;
  }

  const localRef = `refs/heads/${raw}`;
  const localExists = await runGitCommand(primaryWorktree, ['show-ref', '--verify', '--quiet', localRef]);
  if (localExists.success) {
    return null;
  }

  return parsed;
};

/** 归一 upstream 配置：remote 与 branch 均非空才返回 { remote, branch, full: '远程/分支' }，否则 null。 */
const normalizeUpstreamTarget = (remote, branch) => {
  const remoteName = String(remote || '').trim();
  const branchName = String(branch || '').trim();
  if (!remoteName || !branchName) {
    return null;
  }
  return {
    remote: remoteName,
    branch: branchName,
    full: `${remoteName}/${branchName}`,
  };
};

/**
 * 聚合错误对象的全部可用文本（stderr、stdout、message，最后退回 String(error)）。
 * Bun + simple-git 场景下 fatal 信息常只出现在 message/toString 中，不能漏。
 */
const parseGitErrorText = (error) => {
  const stderr = typeof error?.stderr === 'string' ? error.stderr : '';
  const stdout = typeof error?.stdout === 'string' ? error.stdout : '';
  const message = typeof error?.message === 'string' ? error.message : '';
  // Some runtimes (notably Bun + simple-git GitError) surface the fatal text
  // primarily via message/toString; keep String(error) as a last resort so
  // "not a git repository" matching never misses and aborts callers.
  const fallback = !message && error != null ? String(error) : '';
  return [stderr, stdout, message, fallback]
    .map((chunk) => String(chunk || '').trim())
    .filter(Boolean)
    .join('\n')
    .trim();
};

/** 解析 rev-list --left-right --count 的两列输出为 { ahead, behind }；任一非数字返回 null。 */
const parseAheadBehindCounts = (value) => {
  const [aheadRaw, behindRaw] = String(value || '').trim().split(/\s+/);
  const ahead = parseInt(aheadRaw, 10);
  const behind = parseInt(behindRaw, 10);
  if (!Number.isFinite(ahead) || !Number.isFinite(behind)) {
    return null;
  }
  return { ahead, behind };
};

/** 生成 remote 存在性缓存的 key：目录绝对路径与 remote 名以 \0 字符拼接。 */
const getRemoteExistenceCacheKey = (directory, remoteName) => {
  const normalizedDirectory = normalizeDirectoryPath(directory) || '';
  return `${path.resolve(normalizedDirectory)}\0${remoteName}`;
};

/** 判断仓库是否配置了某 remote（remote get-url 有输出即存在），结果按 30 秒 TTL 缓存。 */
const hasRemote = async (git, directory, remoteName) => {
  const remote = String(remoteName || '').trim();
  if (!remote) {
    return false;
  }

  const key = getRemoteExistenceCacheKey(directory, remote);
  const cached = remoteExistenceCache.get(key);
  if (cached && Date.now() - cached.checkedAt < REMOTE_EXISTENCE_CACHE_TTL_MS) {
    return cached.exists;
  }

  const exists = await git
    .raw(['remote', 'get-url', remote])
    .then((value) => String(value || '').trim().length > 0)
    .catch(() => false);

  remoteExistenceCache.set(key, { exists, checkedAt: Date.now() });
  return exists;
};

/**
 * 把 options 归一为 git 参数数组：数组逐项 trim 去空；对象按键展开——
 * 值 true/undefined 只保留键、false 跳过、其它追加 String(value)。
 */
const buildRawGitOptions = (raw) => {
  if (Array.isArray(raw)) {
    return raw.map((value) => String(value || '').trim()).filter(Boolean);
  }

  if (!raw || typeof raw !== 'object') {
    return [];
  }

  return Object.entries(raw).flatMap(([key, value]) => {
    const option = String(key || '').trim();
    if (!option || value === false) {
      return [];
    }
    if (value === true || value == null) {
      return [option];
    }
    return [option, String(value)];
  });
};

/**
 * 比较 HEAD 与 refs/remotes/远程/分支 的领先/落后提交数（rev-list --left-right --count）。
 * @returns {{remote, branch, ahead, behind}|null} 远端 ref 不存在或计数不可解析时为 null
 */
const getRemoteBranchComparison = async (git, remoteName, branchName) => {
  const remote = String(remoteName || '').trim();
  const branch = String(branchName || '').trim();
  if (!remote || !branch) {
    return null;
  }

  const remoteRef = `refs/remotes/${remote}/${branch}`;
  const exists = await git
    .raw(['rev-parse', '--verify', remoteRef])
    .then((value) => String(value || '').trim())
    .catch(() => '');
  if (!exists) {
    return null;
  }

  const countsRaw = await git
    .raw(['rev-list', '--left-right', '--count', `HEAD...${remoteRef}`])
    .then((value) => String(value || '').trim())
    .catch(() => '');
  const counts = parseAheadBehindCounts(countsRaw);
  if (!counts) {
    return null;
  }

  return {
    remote,
    branch,
    ahead: counts.ahead,
    behind: counts.behind,
  };
};

/** 判断错误文本是否含 "not a git repository"（把 GitError 归一为可识别的非仓库错误）。 */
const isNotGitRepositoryError = (error) => {
  const text = parseGitErrorText(error);
  return /not a git repository/i.test(text);
};

// A directory that no longer exists (e.g. a worktree deleted while something
// was still polling its status) is an expected, benign condition — not a fault
// to scream about. simple-git throws "Cannot use simple-git on a directory that
// does not exist"; the underlying fs errors are ENOENT/ENOTDIR.
/** （中文补充）判断错误是否表示目录已不存在（ENOENT/ENOTDIR 或对应文案，见上方英文说明）。 */
const isMissingDirectoryError = (error) => {
  const code = error?.code;
  if (code === 'ENOENT' || code === 'ENOTDIR') {
    return true;
  }
  const text = parseGitErrorText(error);
  return /directory that does not exist|does not exist|no such file or directory/i.test(text);
};

/**
 * 直接 spawn git 子进程执行命令（不经 simple-git，保留精确 exit code 与大输出）。
 * 失败不抛错：返回 { success:false, exitCode, stdout, stderr, message }；
 * maxBuffer 20MB，注入 buildGitEnv 环境变量。
 */
const runGitCommand = async (cwd, args) => {
  try {
    const { stdout, stderr } = await execFileAsync(getGitBinary(), args, {
      cwd,
      env: await buildGitEnv(),
      windowsHide: true,
      maxBuffer: 20 * 1024 * 1024,
    });
    return {
      success: true,
      exitCode: 0,
      stdout: String(stdout || ''),
      stderr: String(stderr || ''),
    };
  } catch (error) {
    return {
      success: false,
      exitCode: typeof error?.code === 'number' ? error.code : 1,
      stdout: String(error?.stdout || ''),
      stderr: String(error?.stderr || ''),
      message: parseGitErrorText(error),
    };
  }
};

/**
 * 在候选路径中定位 commit 里真实存在的一个：对每个候选用 ls-tree 并行检查
 * 父提交与提交本身的树对象，命中即返回。
 * @throws 全部候选都不存在时抛 "Invalid file path"
 */
const resolveGitCommitFilePath = async (repoRoot, hash, candidates) => {
  for (const candidate of candidates) {
    const [originalTreeResult, modifiedTreeResult] = await Promise.all([
      runGitCommand(repoRoot, ['ls-tree', '--name-only', `${hash}^`, '--', candidate]),
      runGitCommand(repoRoot, ['ls-tree', '--name-only', hash, '--', candidate]),
    ]);

    if ((originalTreeResult.success && originalTreeResult.stdout.trim()) || (modifiedTreeResult.success && modifiedTreeResult.stdout.trim())) {
      return candidate;
    }
  }

  throw new Error('Invalid file path');
};

/** runGitCommand 的抛错版本：失败抛 message（空则用 fallbackMessage），成功返回完整结果。 */
const runGitCommandOrThrow = async (cwd, args, fallbackMessage) => {
  const result = await runGitCommand(cwd, args);
  if (!result.success) {
    throw new Error(result.message || fallbackMessage || 'Git command failed');
  }
  return result;
};

/** 等待指定毫秒数的 Promise 包装。 */
const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

/** 判断命令结果是否为 index.lock 冲突（File exists / another git process）。 */
const isIndexLockError = (result) => {
  const message = [result?.message, result?.stderr, result?.stdout].filter(Boolean).join('\n');
  return /index\.lock['"]?: File exists|another git process seems to be running/i.test(message);
};

/** 解析 worktree 的 index.lock 绝对路径（rev-parse --git-path）；失败或空输出返回 null。 */
const getWorktreeIndexLockPath = async (directory) => {
  const result = await runGitCommand(directory, ['rev-parse', '--git-path', 'index.lock']);
  if (!result.success) {
    return null;
  }
  const value = String(result.stdout || '').trim();
  return value ? (path.isAbsolute(value) ? value : path.resolve(directory, value)) : null;
};

/** 取文件稳定身份（dev:ino:size:mtimeMs），用于判断锁文件在等待期间是否被替换；ENOENT 返回 null。 */
const getFileIdentity = async (filePath) => {
  try {
    const stat = await fsp.stat(filePath);
    return `${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeMs}`;
  } catch (error) {
    if (error?.code === 'ENOENT') {
      return null;
    }
    throw error;
  }
};

// OMPChamber places managed worktrees under a deep data-dir path
// (`<XDG_DATA_HOME>/opencode/worktree/<40-char project id>/<name>/`). On
// Windows that prefix plus a deeply nested repo file routinely exceeds
// MAX_PATH (260). Git can check those paths out when core.longpaths is
// enabled; without it, `git reset --hard` during bootstrap fails with
// "Filename too long" and leaves a half-populated worktree (issue #2746).
/** （中文补充）填充 worktree 的 reset 参数组合，见上方英文说明（core.longpaths 兜底 Windows MAX_PATH 问题）。 */
const WORKTREE_POPULATE_RESET_ARGS = ['-c', 'core.longpaths=true', 'reset', '--hard'];

/** 判断错误文本是否为 Windows 的 "Filename too long"。 */
const isFilenameTooLongError = (message) => /file ?name too long/i.test(String(message || ''));

/** 美化填充失败信息：路径超长错误追加原因说明与修复建议（开系统长路径或缩短仓库路径）。 */
const formatWorktreePopulateError = (message) => {
  const text = String(message || '').trim() || 'Failed to populate worktree';
  if (!isFilenameTooLongError(text)) {
    return text;
  }
  return [
    text,
    'The worktree checkout path exceeds this system\'s path-length limit.',
    'OMPChamber enables Git `core.longpaths` for worktree population; if this still fails on Windows, enable OS long paths (LongPathsEnabled) or open the repository from a shorter absolute path.',
  ].join('\n');
};

/**
 * 确保仓库 core.longpaths=true（写入经 common git dir 对全部 linked worktree
 * 生效）；已开启直接返回；写失败不致命——填充时仍会带 -c core.longpaths=true 兜底。
 */
export const ensureWorktreeLongpaths = async (directory) => {
  const current = await runGitCommand(directory, ['config', '--get', 'core.longpaths']);
  if (String(current.stdout || '').trim().toLowerCase() === 'true') {
    return;
  }
  // Local config is shared across linked worktrees via the common git dir, so
  // subsequent OMPChamber and CLI git operations in this repo also get long
  // path support. Failures here are non-fatal: populate still passes
  // `-c core.longpaths=true` on reset.
  await runGitCommand(directory, ['config', 'core.longpaths', 'true']);
};

/**
 * 填充 worktree（-c core.longpaths=true reset --hard）并处理 index.lock 冲突：
 * 先直接执行；锁冲突则等待后重试；仍冲突则记录锁文件身份（dev:ino:mtime）
 * 再等一次，身份未变才判定为陈旧锁并删除后做最终重试；其余错误包装后抛出。
 */
export const populateWorktreeWithLockRecovery = async (directory) => {
  await ensureWorktreeLongpaths(directory);

  let result = await runGitCommand(directory, WORKTREE_POPULATE_RESET_ARGS);
  if (result.success) {
    return;
  }
  if (!isIndexLockError(result)) {
    throw new Error(formatWorktreePopulateError(result.message));
  }

  await wait(WORKTREE_INDEX_LOCK_RETRY_DELAY_MS);
  result = await runGitCommand(directory, WORKTREE_POPULATE_RESET_ARGS);
  if (result.success) {
    return;
  }
  if (!isIndexLockError(result)) {
    throw new Error(formatWorktreePopulateError(result.message));
  }

  const lockPath = await getWorktreeIndexLockPath(directory);
  const identity = lockPath ? await getFileIdentity(lockPath) : null;
  await wait(WORKTREE_INDEX_LOCK_STALE_DELAY_MS);

  result = await runGitCommand(directory, WORKTREE_POPULATE_RESET_ARGS);
  if (result.success) {
    return;
  }
  if (!isIndexLockError(result) || !lockPath || !identity || await getFileIdentity(lockPath) !== identity) {
    throw new Error(formatWorktreePopulateError(result.message));
  }

  await fsp.unlink(lockPath).catch((error) => {
    if (error?.code !== 'ENOENT') {
      throw error;
    }
  });
  const finalResult = await runGitCommand(directory, WORKTREE_POPULATE_RESET_ARGS);
  if (!finalResult.success) {
    throw new Error(formatWorktreePopulateError(finalResult.message || 'Failed to populate worktree'));
  }
};

// Worktrees are created with `git worktree add --no-checkout` and populated
// with `git reset --hard`, neither of which runs git's post-checkout hook —
// git only runs it for checkouts, clone, and worktree add *without*
// --no-checkout. Invoke the hook explicitly after population to restore git's
// checkout semantics: git passes the previous HEAD (null ref for a brand-new
// worktree), the new HEAD, and flag 1 for a branch checkout, and runs the hook
// from the worktree top-level.
/**
 * （中文补充）见上方英文说明：显式执行 post-checkout hook，参数对齐 git 语义
 * （前驱 HEAD 用全零 null ref、新 HEAD、flag 1），失败仅告警不阻断 bootstrap。
 */
const runPostCheckoutHook = async (directory) => {
  let hookDirectory = null;
  try {
    const result = await runGitCommand(directory, ['rev-parse', '--git-path', 'hooks']);
    if (!result.success) return;
    hookDirectory = normalizeDirectoryPath(String(result.stdout || '').trim());
  } catch {
    return;
  }
  if (!hookDirectory) return;

  const hookPath = path.join(hookDirectory, 'post-checkout');
  try {
    const stat = await fsp.stat(hookPath);
    if (!stat.isFile()) return;
    if (process.platform !== 'win32') {
      await fsp.access(hookPath, fs.constants.X_OK);
    }
  } catch {
    // Missing or non-executable hooks are skipped, matching git.
    return;
  }

  const [headResult, gitDirResult] = await Promise.all([
    runGitCommand(directory, ['rev-parse', 'HEAD']),
    runGitCommand(directory, ['rev-parse', '--absolute-git-dir']),
  ]);
  if (!headResult.success || !gitDirResult.success) return;
  const head = String(headResult.stdout || '').trim();
  const gitDir = String(gitDirResult.stdout || '').trim();
  if (!head || !gitDir) return;

  try {
    await execFileAsync(hookPath, [GIT_NULL_REF, head, '1'], {
      cwd: directory,
      env: {
        ...(await buildGitEnv()),
        GIT_DIR: gitDir,
        GIT_WORK_TREE: path.resolve(directory),
      },
      windowsHide: true,
    });
  } catch (error) {
    // A failing hook must not fail worktree creation or session bootstrap:
    // warn and continue.
    console.warn(`[GitService] post-checkout hook failed in worktree ${directory}: ${error instanceof Error ? error.message : String(error)}`);
  }
};

/**
 * 从 .git 目录路径反推主 worktree 根：根/.git 形式去尾；linked worktree 的
 * 根/.git/worktrees/名称 形式截到 worktrees marker 前；其余返回 null。
 */
const derivePrimaryWorktreeRootFromGitDir = (gitDir) => {
  const normalized = normalizePath(gitDir);
  if (!normalized) return null;
  if (normalized.endsWith('/.git')) {
    return normalized.slice(0, -'/.git'.length) || null;
  }
  const marker = '/.git/worktrees/';
  const markerIndex = normalized.indexOf(marker);
  if (markerIndex > 0) {
    return normalized.slice(0, markerIndex) || null;
  }
  return null;
};

/**
 * 解析目录所在仓库的主 worktree 根目录：优先用 --absolute-git-dir 推导，
 * 再用 --git-common-dir（相对路径时按目录 resolve）；全部失败回退输入目录。
 * @returns {{root: string}}
 */
export async function resolvePrimaryWorktreeRoot(directory) {
  const result = await runGitCommand(directory, ['rev-parse', '--absolute-git-dir', '--git-common-dir']);
  if (!result.success) {
    return { root: directory };
  }
  const lines = String(result.stdout || '')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
  const absoluteGitDir = normalizePath(lines[0] || '');
  const rootFromAbsoluteGitDir = derivePrimaryWorktreeRootFromGitDir(absoluteGitDir);
  if (rootFromAbsoluteGitDir) {
    return { root: rootFromAbsoluteGitDir };
  }
  const rawCommonDir = normalizePath(lines[1] || '');
  if (rawCommonDir) {
    const commonDir = path.isAbsolute(rawCommonDir)
      ? rawCommonDir
      : path.resolve(directory, rawCommonDir);
    const rootFromCommonDir = derivePrimaryWorktreeRootFromGitDir(commonDir);
    if (rootFromCommonDir) {
      return { root: rootFromCommonDir };
    }
  }
  return { root: directory };
}

/** 解析目录所在 worktree 的顶层目录（--show-toplevel）；失败回退为输入目录。 */
export async function resolveWorktreeTopLevel(directory) {
  const result = await runGitCommand(directory, ['rev-parse', '--show-toplevel']);
  if (!result.success) {
    return { root: directory };
  }
  const root = normalizePath(String(result.stdout || '').trim());
  return { root: root || directory };
}

/**
 * 批量取 commit 摘要（show -s，TAB 分隔的 hash/short/subject 自定义格式）；
 * 空 SHA 列表返回空结果；SHA 不合法（非 4-64 位十六进制）抛 "Invalid commit SHA"。
 */
export async function getCommitSummaries(directory, shas) {
  const commits = Array.isArray(shas)
    ? shas.map((sha) => String(sha || '').trim()).filter(Boolean)
    : [];
  if (commits.length === 0) {
    return { commits: [] };
  }
  if (commits.some((sha) => !/^[0-9a-fA-F]{4,64}$/.test(sha))) {
    throw new Error('Invalid commit SHA');
  }
  const result = await runGitCommandOrThrow(
    directory,
    ['show', '-s', '--format=%H%x09%h%x09%s', ...commits, '--'],
    'Failed to get commit summaries'
  );
  const parsed = String(result.stdout || '')
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      const [sha, short, subject] = line.split('\t');
      return { sha: sha || '', short: short || '', subject: subject || '' };
    })
    .filter((entry) => entry.sha && entry.short);
  return { commits: parsed };
}

/** 把多行输出按行拆分、逐行 trim、去空行。 */
const trimGitLines = (value) => String(value || '')
  .split(/\r?\n/)
  .map((line) => line.trim())
  .filter(Boolean);

/** 提取命令结果 stdout 的 trim 文本，空安全。 */
const gitStdoutText = (result) => String(result?.stdout || '').trim();
/** 提取命令结果 stderr（退而取 message）的 trim 文本，空安全。 */
const gitStderrText = (result) => String(result?.stderr || result?.message || '').trim();

/** 校验 integrate 用分支名：非空、不以 '-' 开头、不含 NUL；返回 trim 值，不合法抛错。 */
const normalizeIntegrateBranch = (value, fieldName) => {
  const branch = String(value || '').trim();
  if (!branch) {
    throw new Error(`${fieldName} is required`);
  }
  if (branch.startsWith('-') || branch.includes('\0')) {
    throw new Error(`Invalid ${fieldName}`);
  }
  return branch;
};

/** 校验 integrate 用 commit SHA（4-64 位十六进制），返回 trim 后的值，否则抛错。 */
const normalizeIntegrateSha = (value) => {
  const sha = String(value || '').trim();
  if (!/^[0-9a-fA-F]{4,64}$/.test(sha)) {
    throw new Error('Invalid commit SHA');
  }
  return sha;
};

/** 校验 integrate 用路径：必填（fieldName 用于报错），归一 ~ 后 resolve 为绝对路径。 */
const normalizeIntegratePath = (value, fieldName) => {
  const target = normalizeDirectoryPath(value);
  if (!target) {
    throw new Error(`${fieldName} is required`);
  }
  return path.resolve(target);
};

/** 判断命令结果是否成功（result.success 为真）。 */
const runGitOk = (result) => Boolean(result?.success);

/** 解析 worktree list --porcelain 为 { path, branchRef } 精简条目（失败抛错）。 */
const listGitWorktreesForIntegrate = async (repoRoot) => {
  const out = await runGitCommandOrThrow(repoRoot, ['worktree', 'list', '--porcelain'], 'Failed to list git worktrees');
  const entries = [];
  let current = null;
  for (const line of String(out.stdout || '').split(/\r?\n/)) {
    if (line.startsWith('worktree ')) {
      if (current) entries.push(current);
      current = { path: line.slice('worktree '.length).trim(), branchRef: null };
      continue;
    }
    if (!current) continue;
    if (line.startsWith('branch ')) {
      current.branchRef = line.slice('branch '.length).trim();
    }
  }
  if (current) entries.push(current);
  return entries.filter((entry) => Boolean(entry.path));
};

/**
 * 确保 integrate 目标分支本地可用：HEAD 原样返回；已有本地分支直接用；
 * remotes/远程/分支 或仅 origin 上存在时用 --track 建本地跟踪分支；
 * 都不存在时原样返回交给后续 git 命令报错。
 */
const ensureLocalIntegrateBranch = async (repoRoot, candidate) => {
  const raw = normalizeIntegrateBranch(candidate, 'targetBranch');
  if (raw === 'HEAD') {
    return 'HEAD';
  }

  const hasLocal = await runGitCommand(repoRoot, ['show-ref', '--verify', '--quiet', `refs/heads/${raw}`]);
  if (runGitOk(hasLocal)) {
    return raw;
  }

  if (raw.startsWith('remotes/')) {
    const remoteRef = raw.slice('remotes/'.length);
    const parts = remoteRef.split('/');
    const remote = normalizeIntegrateBranch(parts[0] || 'origin', 'remote');
    const name = normalizeIntegrateBranch(parts.slice(1).join('/'), 'branch');
    await runGitCommandOrThrow(repoRoot, ['branch', '--track', name, `${remote}/${name}`], 'Failed to track remote branch');
    return name;
  }

  const remoteCheck = await runGitCommand(repoRoot, ['show-ref', '--verify', '--quiet', `refs/remotes/origin/${raw}`]);
  if (runGitOk(remoteCheck)) {
    await runGitCommandOrThrow(repoRoot, ['branch', '--track', raw, `origin/${raw}`], 'Failed to track remote branch');
    return raw;
  }

  return raw;
};

/**
 * 计算 integrate 计划：用 git cherry 找出 target 尚不包含的 source 提交（'+' 行），
 * 配合 rev-list --reverse 得到按时间正序的待搬运 SHA 列表；
 * 任一分支为 HEAD 时返回空提交列表的 noop 计划。
 */
export async function computeIntegratePlan(input = {}) {
  const repoRoot = normalizeIntegratePath(input.repoRoot, 'repoRoot');
  const sourceBranch = normalizeIntegrateBranch(input.sourceBranch, 'sourceBranch');
  const targetBranchRaw = normalizeIntegrateBranch(input.targetBranch, 'targetBranch');
  if (sourceBranch === 'HEAD' || targetBranchRaw === 'HEAD') {
    return { repoRoot, sourceBranch, targetBranch: targetBranchRaw, commits: [] };
  }

  const targetBranch = await ensureLocalIntegrateBranch(repoRoot, targetBranchRaw);
  const cherry = await runGitCommandOrThrow(repoRoot, ['cherry', targetBranch, sourceBranch], 'Failed to compute cherry commits');
  const plus = new Set();
  for (const line of trimGitLines(cherry.stdout)) {
    const match = line.match(/^\+\s+([0-9a-f]{7,40})\b/i);
    if (match) {
      plus.add(match[1]);
    }
  }

  const revList = await runGitCommandOrThrow(repoRoot, ['rev-list', '--reverse', `${targetBranch}..${sourceBranch}`], 'Failed to list commits');
  const commits = trimGitLines(revList.stdout).filter((sha) => plus.has(sha));
  return { repoRoot, sourceBranch, targetBranch, commits };
}

/** 在 ~/.config/ompchamber/tmp 下 mkdtemp 建临时目录并 worktree add --force 到 target 分支；失败清理后抛错。 */
const createIntegrateTempWorktree = async (repoRoot, targetBranch) => {
  const tmpParent = path.join(os.homedir(), '.config', 'ompchamber', 'tmp');
  await fsp.mkdir(tmpParent, { recursive: true });
  const tmpDir = await fsp.mkdtemp(path.join(tmpParent, 'oc-integrate-'));
  try {
    await runGitCommandOrThrow(repoRoot, ['worktree', 'add', '--force', tmpDir, targetBranch], 'Failed to create temp worktree');
    return tmpDir;
  } catch (error) {
    await fsp.rm(tmpDir, { recursive: true, force: true }).catch(() => undefined);
    throw error;
  }
};

/** 强制移除 integrate 临时 worktree 并 prune 登记记录；所有失败吞掉（尽力清理）。 */
const removeIntegrateTempWorktree = async (repoRoot, tmpDir) => {
  await runGitCommand(repoRoot, ['worktree', 'remove', '--force', tmpDir]).catch(() => undefined);
  await runGitCommand(repoRoot, ['worktree', 'prune']).catch(() => undefined);
};

/** 若 target 分支配置了 upstream，则 fetch 后 merge --ff-only 快进临时 worktree；无 upstream 静默跳过。 */
const maybeFastForwardIntegrateUpstream = async (tmpDir) => {
  const upstream = await runGitCommand(tmpDir, ['rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{u}']);
  const upstreamRef = gitStdoutText(upstream);
  if (!upstreamRef) {
    return;
  }
  await runGitCommand(tmpDir, ['fetch']);
  const ff = await runGitCommand(tmpDir, ['merge', '--ff-only', upstreamRef]);
  if (!runGitOk(ff)) {
    throw new Error(gitStderrText(ff) || 'Fast-forward failed');
  }
};

/**
 * 并行采集 integrate 冲突现场：porcelain 状态、未合并文件、完整 diff、
 * CHERRY_PICK_HEAD 的提交元信息与 patch 内容，供前端展示与继续决策。
 */
export async function getIntegrateConflictDetails(tmpDir) {
  const target = normalizeIntegratePath(tmpDir, 'tempWorktreePath');
  const [status, unmerged, diff, meta, patch] = await Promise.all([
    runGitCommand(target, ['status', '--porcelain']),
    runGitCommand(target, ['diff', '--name-only', '--diff-filter=U']),
    runGitCommand(target, ['diff', NO_EXT_DIFF]),
    runGitCommand(target, ['show', '--no-patch', '--pretty=fuller', 'CHERRY_PICK_HEAD']),
    runGitCommand(target, ['show', NO_EXT_DIFF, 'CHERRY_PICK_HEAD']),
  ]);

  return {
    statusPorcelain: String(status.stdout || ''),
    unmergedFiles: trimGitLines(unmerged.stdout),
    diff: String(diff.stdout || diff.stderr || ''),
    currentPatchMeta: String(meta.stdout || meta.stderr || ''),
    currentPatch: String(patch.stdout || patch.stderr || ''),
  };
}

/** 判断 integrate 临时 worktree 是否处于 cherry-pick 进行中（CHERRY_PICK_HEAD 可解析）。 */
export async function isCherryPickInProgress(tmpDir) {
  const target = normalizeIntegratePath(tmpDir, 'tempWorktreePath');
  const head = await runGitCommand(target, ['rev-parse', '--verify', '--quiet', 'CHERRY_PICK_HEAD']);
  return { inProgress: runGitOk(head) };
}

/**
 * 找出需要同步的 worktree：检出 target 分支、不在排除名单（临时 worktree）中，
 * 且工作区干净（status --porcelain 为空）的目录列表。
 */
const computeCleanIntegrateWorktreesToSync = async ({ repoRoot, targetBranch, excludePaths }) => {
  const targetRef = `refs/heads/${targetBranch}`;
  const exclude = new Set(excludePaths);
  const entries = await listGitWorktreesForIntegrate(repoRoot);
  const candidates = entries
    .filter((entry) => entry.branchRef === targetRef)
    .map((entry) => entry.path)
    .filter((candidate) => candidate && !exclude.has(candidate));

  const clean = [];
  for (const candidate of candidates) {
    const status = await runGitCommand(candidate, ['status', '--porcelain']);
    if (!gitStdoutText(status)) {
      clean.push(candidate);
    }
  }
  return clean;
};

/** 对列出的每个 worktree 执行 reset --hard 同步到新提交；单个失败忽略继续。 */
const syncCleanIntegrateTargetWorktrees = async (paths) => {
  for (const target of paths) {
    await runGitCommand(target, ['reset', '--hard']).catch(() => undefined);
  }
};

/** 校验并归一外部传入的 integrate 计划（repoRoot、双分支、SHA 列表），不合法即抛错。 */
const normalizeIntegratePlan = async (plan = {}) => {
  const repoRoot = normalizeIntegratePath(plan.repoRoot, 'repoRoot');
  const sourceBranch = normalizeIntegrateBranch(plan.sourceBranch, 'sourceBranch');
  const targetBranch = normalizeIntegrateBranch(plan.targetBranch, 'targetBranch');
  const commits = Array.isArray(plan.commits) ? plan.commits.map(normalizeIntegrateSha) : [];
  return { repoRoot, sourceBranch, targetBranch, commits };
};

/** 校验并归一外部传入的 integrate 状态对象（路径、分支、剩余/当前提交列表）。 */
const normalizeIntegrateState = (state = {}) => ({
  repoRoot: normalizeIntegratePath(state.repoRoot, 'repoRoot'),
  tempWorktreePath: normalizeIntegratePath(state.tempWorktreePath, 'tempWorktreePath'),
  sourceBranch: normalizeIntegrateBranch(state.sourceBranch, 'sourceBranch'),
  targetBranch: normalizeIntegrateBranch(state.targetBranch, 'targetBranch'),
  cleanTargetWorktrees: Array.isArray(state.cleanTargetWorktrees)
    ? state.cleanTargetWorktrees.map((entry) => normalizeIntegratePath(entry, 'cleanTargetWorktree'))
    : [],
  remainingCommits: Array.isArray(state.remainingCommits) ? state.remainingCommits.map(normalizeIntegrateSha) : [],
  currentCommit: normalizeIntegrateSha(state.currentCommit),
});

/**
 * 执行 integrate 主流程：建临时 worktree（检出 target 分支）→ 快进 upstream →
 * 校验工作区干净 → 逐个 cherry-pick 计划提交；遇冲突返回
 * { kind:'conflict', state, details }（state 保存进度，供 continue/abort 使用）；
 * 全部成功后清理临时 worktree、把其它检出 target 分支的干净 worktree
 * reset --hard 同步，返回 success。空计划返回 noop；非冲突异常也先清理再抛。
 */
export async function integrateWorktreeCommits(inputPlan = {}) {
  const plan = await normalizeIntegratePlan(inputPlan);
  if (plan.commits.length === 0) {
    return { kind: 'noop', reason: 'No commits to move' };
  }

  const tmpDir = await createIntegrateTempWorktree(plan.repoRoot, plan.targetBranch);
  let cleanTargetWorktrees = [];
  let remaining = [];
  try {
    await maybeFastForwardIntegrateUpstream(tmpDir);

    const clean = await runGitCommand(tmpDir, ['status', '--porcelain']);
    if (gitStdoutText(clean)) {
      throw new Error('Target branch has local changes; abort integration and retry');
    }

    cleanTargetWorktrees = await computeCleanIntegrateWorktreesToSync({
      repoRoot: plan.repoRoot,
      targetBranch: plan.targetBranch,
      excludePaths: [tmpDir],
    }).catch(() => []);

    remaining = [...plan.commits];
    while (remaining.length > 0) {
      const sha = remaining[0];
      const pick = await runGitCommand(tmpDir, ['cherry-pick', sha]);
      if (runGitOk(pick)) {
        remaining.shift();
        continue;
      }

      const unmerged = await runGitCommand(tmpDir, ['diff', '--name-only', '--diff-filter=U']);
      const unmergedFiles = trimGitLines(unmerged.stdout);
      if (unmergedFiles.length > 0) {
        const details = await getIntegrateConflictDetails(tmpDir);
        return {
          kind: 'conflict',
          state: {
            repoRoot: plan.repoRoot,
            tempWorktreePath: tmpDir,
            sourceBranch: plan.sourceBranch,
            targetBranch: plan.targetBranch,
            cleanTargetWorktrees,
            remainingCommits: remaining,
            currentCommit: sha,
          },
          details,
        };
      }

      throw new Error(gitStderrText(pick) || 'Cherry-pick failed');
    }

    await removeIntegrateTempWorktree(plan.repoRoot, tmpDir);
    await syncCleanIntegrateTargetWorktrees(cleanTargetWorktrees).catch(() => undefined);
    return { kind: 'success', moved: plan.commits.length };
  } catch (error) {
    await removeIntegrateTempWorktree(plan.repoRoot, tmpDir).catch(() => undefined);
    throw error;
  }
}

/** 放弃 integrate：cherry-pick --abort（尽力而为）后强制移除临时 worktree。 */
export async function abortIntegrate(stateInput = {}) {
  const state = normalizeIntegrateState(stateInput);
  await runGitCommand(state.tempWorktreePath, ['cherry-pick', '--abort']).catch(() => undefined);
  await removeIntegrateTempWorktree(state.repoRoot, state.tempWorktreePath);
  return { success: true };
}

/**
 * 继续 integrate：cherry-pick --continue；仍有未合并文件则返回 conflict 状态与
 * 详情，否则继续逐个 pick 剩余提交；全部完成后清理临时 worktree、同步目标
 * worktree，返回 { kind: 'success', moved }。
 */
export async function continueIntegrate(stateInput = {}) {
  const state = normalizeIntegrateState(stateInput);
  const cont = await runGitCommand(state.tempWorktreePath, ['cherry-pick', '--continue']);
  if (!runGitOk(cont)) {
    const unmerged = await runGitCommand(state.tempWorktreePath, ['diff', '--name-only', '--diff-filter=U']);
    if (trimGitLines(unmerged.stdout).length > 0) {
      const details = await getIntegrateConflictDetails(state.tempWorktreePath);
      return { kind: 'conflict', state, details };
    }
    throw new Error(gitStderrText(cont) || 'Cherry-pick continue failed');
  }

  const remaining = [...state.remainingCommits];
  if (remaining.length > 0 && remaining[0] === state.currentCommit) {
    remaining.shift();
  }

  const still = [...remaining];
  while (still.length > 0) {
    const sha = still[0];
    const pick = await runGitCommand(state.tempWorktreePath, ['cherry-pick', sha]);
    if (runGitOk(pick)) {
      still.shift();
      continue;
    }
    const unmerged = await runGitCommand(state.tempWorktreePath, ['diff', '--name-only', '--diff-filter=U']);
    if (trimGitLines(unmerged.stdout).length > 0) {
      const details = await getIntegrateConflictDetails(state.tempWorktreePath);
      return {
        kind: 'conflict',
        state: {
          ...state,
          remainingCommits: still,
          currentCommit: sha,
        },
        details,
      };
    }
    throw new Error(gitStderrText(pick) || 'Cherry-pick failed');
  }

  await removeIntegrateTempWorktree(state.repoRoot, state.tempWorktreePath);
  await syncCleanIntegrateTargetWorktrees(state.cleanTargetWorktrees).catch(() => undefined);
  return { kind: 'success', moved: state.remainingCommits.length };
}

/**
 * 确保主 worktree 的 .git/opencode 文件中有项目 ID：已有则直接返回；否则取
 * 仓库全部根提交中字典序最小的 SHA 作为稳定 ID 写回（写失败不影响返回）。
 * @throws 无法派生（仓库没有任何根提交）时抛错
 */
const ensureOpenCodeProjectId = async (primaryWorktree) => {
  const gitDir = path.join(primaryWorktree, '.git');
  const idFile = path.join(gitDir, 'opencode');
  const existing = await fsp.readFile(idFile, 'utf8').then((value) => value.trim()).catch(() => '');
  if (existing) {
    return existing;
  }

  const rootsResult = await runGitCommandOrThrow(
    primaryWorktree,
    ['rev-list', '--max-parents=0', '--all'],
    'Failed to resolve repository roots'
  );

  const roots = rootsResult.stdout
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .sort((a, b) => a.localeCompare(b));

  const projectId = roots[0] || '';
  if (!projectId) {
    throw new Error('Failed to derive OpenCode project ID');
  }

  await fsp.mkdir(gitDir, { recursive: true }).catch(() => undefined);
  await fsp.writeFile(idFile, projectId, 'utf8').catch(() => undefined);

  return projectId;
};

/**
 * 解析 worktree 项目上下文：目录顶层（sandbox）、主 worktree（--git-common-dir
 * 的父目录）、OpenCode 项目 ID（ensureOpenCodeProjectId），以及托管 worktree
 * 根目录（OpenCode 数据目录/worktree/项目ID）。
 */
const resolveWorktreeProjectContext = async (directory) => {
  const directoryPath = normalizeDirectoryPath(directory);
  if (!directoryPath) {
    throw new Error('Directory is required');
  }

  const topResult = await runGitCommandOrThrow(
    directoryPath,
    ['rev-parse', '--show-toplevel'],
    'Failed to resolve git top-level directory'
  );
  const sandbox = path.resolve(directoryPath, topResult.stdout.trim());

  const commonResult = await runGitCommandOrThrow(
    sandbox,
    ['rev-parse', '--git-common-dir'],
    'Failed to resolve git common directory'
  );
  const commonDir = path.resolve(sandbox, commonResult.stdout.trim());
  const primaryWorktree = path.dirname(commonDir);
  const projectID = await ensureOpenCodeProjectId(primaryWorktree);
  const worktreeRoot = path.join(getOpenCodeDataPath(), 'worktree', projectID);

  return {
    projectID,
    sandbox,
    primaryWorktree,
    worktreeRoot,
  };
};

/** 列出仓库全部 worktree 条目（worktree list --porcelain 的完整解析；失败抛错）。 */
const listWorktreeEntries = async (directory) => {
  const rawResult = await runGitCommandOrThrow(
    directory,
    ['worktree', 'list', '--porcelain'],
    'Failed to list git worktrees'
  );
  return parseWorktreePorcelain(rawResult.stdout);
};

/** 生成至多 26 个名称候选：首个为 slug 后的基名，其余为 基名-随机名；无基名则全随机。 */
const resolveWorktreeNameCandidates = (baseName) => {
  const normalizedBase = slugWorktreeName(baseName || '');
  if (!normalizedBase) {
    return Array.from({ length: OPENCODE_WORKTREE_ATTEMPTS }, () => generateOpenCodeRandomName());
  }
  return Array.from({ length: OPENCODE_WORKTREE_ATTEMPTS }, (_, index) => {
    if (index === 0) {
      return normalizedBase;
    }
    return `${normalizedBase}-${generateOpenCodeRandomName()}`;
  });
};

/**
 * 从名称候选中确定可用的 worktree 目录与分支：目录已存在则换下一个候选；
 * 未显式指定分支名时还要求 ompchamber/名称 分支也不存在；
 * 全部候选撞名抛 "Failed to generate a unique worktree name"。
 */
const resolveCandidateDirectory = async (worktreeRoot, preferredName, explicitBranchName, primaryWorktree) => {
  const candidates = resolveWorktreeNameCandidates(preferredName);

  for (const name of candidates) {
    const directory = path.join(worktreeRoot, name);
    if (await checkPathExists(directory)) {
      continue;
    }

    if (explicitBranchName) {
      return { name, directory, branch: explicitBranchName };
    }

    const branch = `ompchamber/${name}`;
    const branchRef = `refs/heads/${branch}`;
    const branchExists = await runGitCommand(primaryWorktree, ['show-ref', '--verify', '--quiet', branchRef]);
    if (branchExists.success) {
      continue;
    }

    return { name, directory, branch };
  }

  throw new Error('Failed to generate a unique worktree name');
};

/**
 * existing 模式解析要检出的分支：本地已存在则直接检出（不新建）；否则按
 * 远端分支处理——远端跟踪 ref 不存在先 fetch 再校验，本地分支名取用户首选
 * 或远端分支名。
 * @returns {{localBranch: string, checkoutRef: string, createLocalBranch: boolean, remoteRef: object|null}}
 * @throws existingBranch 为空或本地/远端都找不到时抛错
 */
const resolveBranchForExistingMode = async (primaryWorktree, existingBranch, preferredBranchName) => {
  const requested = String(existingBranch || '').trim();
  if (!requested) {
    throw new Error('existingBranch is required in existing mode');
  }

  const normalizedLocal = cleanBranchName(requested);
  const localRef = `refs/heads/${normalizedLocal}`;
  const localExists = await runGitCommand(primaryWorktree, ['show-ref', '--verify', '--quiet', localRef]);
  if (localExists.success) {
    return {
      localBranch: normalizedLocal,
      checkoutRef: normalizedLocal,
      createLocalBranch: false,
      remoteRef: null,
    };
  }

  const remoteRef = parseRemoteBranchRef(requested);
  if (!remoteRef) {
    throw new Error(`Branch not found: ${requested}`);
  }

  const remoteExists = await runGitCommand(primaryWorktree, ['show-ref', '--verify', '--quiet', remoteRef.fullRef]);
  if (!remoteExists.success) {
    await fetchRemoteBranchRef(primaryWorktree, remoteRef.remote, remoteRef.branch).catch(() => undefined);
    const recheck = await runGitCommand(primaryWorktree, ['show-ref', '--verify', '--quiet', remoteRef.fullRef]);
    if (!recheck.success) {
      throw new Error(`Remote branch not found: ${requested}`);
    }
  }

  const localBranch = cleanBranchName(preferredBranchName || remoteRef.branch || requested);
  if (!localBranch) {
    throw new Error('Failed to resolve local branch name for existing branch worktree');
  }

  return {
    localBranch,
    checkoutRef: remoteRef.remoteRef,
    createLocalBranch: true,
    remoteRef,
  };
};

/** 查找某本地分支当前被哪个 worktree 检出（branchRef 全量或裸分支名匹配）；未被检出返回 null。 */
const findBranchInUse = async (primaryWorktree, localBranchName) => {
  if (!localBranchName) {
    return null;
  }
  const entries = await listWorktreeEntries(primaryWorktree);
  const targetRef = `refs/heads/${localBranchName}`;
  const targetClean = cleanBranchName(targetRef);
  return entries.find((entry) => {
    const entryRef = String(entry.branchRef || '').trim();
    const entryClean = cleanBranchName(entryRef || entry.branch || '');
    return entryRef === targetRef || entryClean === targetClean;
  }) || null;
};

/**
 * 在 worktree 目录执行启动命令：Windows 经 cmd /c、其余经 bash -lc（登录
 * shell 以加载用户环境）；命令为空直接成功。永不抛错——返回
 * { success, stdout, stderr, message? }。
 */
const runWorktreeStartCommand = async (directory, command) => {
  const text = String(command || '').trim();
  if (!text) {
    return { success: true };
  }

  if (process.platform === 'win32') {
    const result = await execFileAsync('cmd', ['/c', text], {
      cwd: directory,
      env: await buildGitEnv(),
      windowsHide: true,
      maxBuffer: 20 * 1024 * 1024,
    }).then(({ stdout, stderr }) => ({ success: true, stdout, stderr })).catch((error) => ({
      success: false,
      stdout: error?.stdout,
      stderr: error?.stderr,
      message: parseGitErrorText(error),
    }));
    return result;
  }

  const result = await execFileAsync('bash', ['-lc', text], {
    cwd: directory,
    env: await buildGitEnv(),
    maxBuffer: 20 * 1024 * 1024,
  }).then(({ stdout, stderr }) => ({ success: true, stdout, stderr })).catch((error) => ({
    success: false,
    stdout: error?.stdout,
    stderr: error?.stderr,
    message: parseGitErrorText(error),
  }));
  return result;
};

/** 读取 OpenCode 项目存储（storage/project/项目ID.json）中的 commands.start；读取或解析失败返回空串。 */
const loadProjectStartCommand = async (projectID) => {
  const storagePath = path.join(getOpenCodeDataPath(), 'storage', 'project', `${projectID}.json`);
  try {
    const raw = await fsp.readFile(storagePath, 'utf8');
    const parsed = JSON.parse(raw);
    const start = typeof parsed?.commands?.start === 'string' ? parsed.commands.start.trim() : '';
    return start || '';
  } catch {
    return '';
  }
};

// OpenCode owns its own project/sandbox registry. It records a worktree as a
// sandbox itself when an instance boots for that directory, and filters entries
// whose directory no longer exists when reading them back. OMPChamber used to
// write that state directly into OpenCode's storage JSON and SQLite database,
// behind the back of the running process: the row changed but the server was
// never told, so a worktree created while OpenCode was running stayed unknown
// to it until a restart. Registration is not ours to perform.

/** 判断目录是否已挂接为 git worktree（rev-parse --is-inside-work-tree 为 true）；异常按否处理。 */
const isAttachedGitWorktreeDirectory = async (directory) => {
  try {
    const result = await runGitCommand(directory, ['rev-parse', '--is-inside-work-tree']);
    return result.success && String(result.stdout || '').trim() === 'true';
  } catch {
    return false;
  }
};

/**
 * 快速创建失败后的兜底清理：仅当候选目录位于托管 worktree 根之内、还不是已
 * 挂接的 git worktree、且为空目录时才删除；否则不动（避免误删用户数据）。
 */
const cleanupFailedFastWorktreeCreate = async (context, candidate) => {
  const candidateDirectory = path.resolve(candidate.directory);
  const worktreeRoot = path.resolve(context.worktreeRoot);
  const isInsideWorktreeRoot = isInsideOrSameDirectory(worktreeRoot, candidateDirectory) && candidateDirectory !== worktreeRoot;
  const isAttached = await isAttachedGitWorktreeDirectory(candidateDirectory);

  if (!isInsideWorktreeRoot || isAttached) {
    return;
  }

  try {
    const entries = await fsp.readdir(candidateDirectory);
    if (entries.length === 0) {
      await fsp.rmdir(candidateDirectory);
    }
  } catch (error) {
    if (!['ENOENT', 'ENOTEMPTY', 'EEXIST'].includes(error?.code)) {
      console.warn('Failed to clean up empty worktree directory after creation failure:', error instanceof Error ? error.message : String(error));
    }
  }
};

/** 依序执行启动脚本：先 OpenCode 项目级 commands.start，成功后再执行用户附加的 startCommand；失败仅告警不阻塞。 */
const runWorktreeStartScripts = async (directory, projectID, startCommand) => {
  const projectStart = await loadProjectStartCommand(projectID);
  if (projectStart) {
    const projectResult = await runWorktreeStartCommand(directory, projectStart);
    if (!projectResult.success) {
      console.warn('Worktree project start command failed:', projectResult.message || projectResult.stderr || projectResult.stdout);
      return;
    }
  }

  const extraCommand = String(startCommand || '').trim();
  if (!extraCommand) {
    return;
  }
  const extraResult = await runWorktreeStartCommand(directory, extraCommand);
  if (!extraResult.success) {
    console.warn('Worktree start command failed:', extraResult.message || extraResult.stderr || extraResult.stdout);
  }
};

/**
 * 排队执行 worktree bootstrap（下一 tick 异步开始）：填充工作区文件 → 执行
 * post-checkout hook → 按需配置 upstream（失败仅告警）→ 置 git-ready → 执行
 * 启动脚本 → 置 ready；任一步失败置 failed 并记录错误。
 * 任务登记进 active 表，供 removeWorktree 等待、避免删除与创建竞态。
 */
const queueWorktreeBootstrap = (args) => {
  const {
    directory,
    projectID,
    primaryWorktree,
    localBranch,
    setUpstream,
    upstreamRemote,
    upstreamBranch,
    ensureRemoteName,
    ensureRemoteUrl,
    startCommand,
  } = args;
  const task = new Promise((resolve) => setTimeout(resolve, 0))
    .then(async () => {
      await populateWorktreeWithLockRecovery(directory);
      await runPostCheckoutHook(directory);
      if (setUpstream) {
        await applyUpstreamConfiguration({
          primaryWorktree,
          worktreeDirectory: directory,
          localBranch,
          setUpstream,
          upstreamRemote,
          upstreamBranch,
          ensureRemoteName,
          ensureRemoteUrl,
        }).catch((error) => {
          console.warn('Worktree upstream configuration failed:', error instanceof Error ? error.message : String(error));
        });
      }
      setWorktreeBootstrapState(
        directory,
        WORKTREE_BOOTSTRAP_PENDING,
        WORKTREE_BOOTSTRAP_PHASE_GIT_READY
      );
      await runWorktreeStartScripts(directory, projectID, startCommand).catch((error) => {
        console.warn('Worktree start script task failed:', error instanceof Error ? error.message : String(error));
      });
      setWorktreeBootstrapState(
        directory,
        WORKTREE_BOOTSTRAP_READY,
        WORKTREE_BOOTSTRAP_PHASE_SETUP_READY
      );
    })
    .catch((error) => {
      setWorktreeBootstrapState(
        directory,
        WORKTREE_BOOTSTRAP_FAILED,
        WORKTREE_BOOTSTRAP_PHASE_DIRECTORY_CREATED,
        error instanceof Error ? error.message : String(error)
      );
      console.warn('Worktree bootstrap task failed:', error instanceof Error ? error.message : String(error));
    });

  trackWorktreeBootstrapTask(directory, task);
};

/** 确保 remote 存在且指向给定 URL：不存在则 add、URL 不一致则 set-url；名称或 URL 为空跳过。 */
const ensureRemoteWithUrl = async (primaryWorktree, remoteName, remoteUrl) => {
  const name = String(remoteName || '').trim();
  const url = String(remoteUrl || '').trim();
  if (!name || !url) {
    return;
  }

  const getUrl = await runGitCommand(primaryWorktree, ['remote', 'get-url', name]);
  if (getUrl.success) {
    const currentUrl = String(getUrl.stdout || '').trim();
    if (currentUrl !== url) {
      await runGitCommandOrThrow(primaryWorktree, ['remote', 'set-url', name, url], 'Failed to update git remote URL');
    }
    return;
  }

  await runGitCommandOrThrow(primaryWorktree, ['remote', 'add', name, url], 'Failed to add git remote');
};

/** 用显式 refspec 拉取远端分支到 refs/remotes；失败抛错。 */
const fetchRemoteBranchRef = async (primaryWorktree, remoteName, branchName) => {
  const remote = String(remoteName || '').trim();
  const branch = String(branchName || '').trim();
  if (!remote || !branch) {
    return;
  }

  const refspec = `+refs/heads/${branch}:refs/remotes/${remote}/${branch}`;
  await runGitCommandOrThrow(
    primaryWorktree,
    ['fetch', remote, refspec],
    `Failed to fetch ${remote}/${branch}`
  );
};

/**
 * Shared existing-mode resolver for validate + create.
 * Provisioned remotes (`ensureRemoteName`/`ensureRemoteUrl`) are used for fork
 * PR heads; other existing branches keep the local / already-fetched remote path.
 *
 * @param {'validate'|'create'} intent
 */
/**
 * （中文补充）validate 与 create 共用的 existing 模式解析器（见上方英文说明）：
 * 预置 remote（fork PR 场景）走 ensure + fetch 路径，其余沿用本地/已 fetch
 * 的远端分支；intent='validate' 只做连通性检查，'create' 才真正建 remote 并 fetch。
 */
const resolveExistingWorktreeSource = async (primaryWorktree, input = {}, intent = 'create') => {
  const preferredBranchName = cleanBranchName(String(input?.branchName || '').trim());
  const ensureRemoteName = String(input?.ensureRemoteName || '').trim();
  const ensureRemoteUrl = String(input?.ensureRemoteUrl || '').trim();
  const requestedExistingBranch = String(input?.existingBranch || '').trim();
  const wantUpstream = Boolean(input?.setUpstream);
  const explicitUpstreamRemote = String(input?.upstreamRemote || '').trim();
  const explicitUpstreamBranch = String(input?.upstreamBranch || '').trim();
  const parsedExistingRemote = await resolveRemoteBranchRef(primaryWorktree, requestedExistingBranch);

  if (
    parsedExistingRemote
    && ensureRemoteName
    && ensureRemoteUrl
    && parsedExistingRemote.remote === ensureRemoteName
  ) {
    if (intent === 'validate') {
      const lsRemote = await runGitCommand(
        primaryWorktree,
        ['ls-remote', '--heads', ensureRemoteUrl, `refs/heads/${parsedExistingRemote.branch}`]
      );
      if (!lsRemote.success) {
        throw new Error(
          `Unable to reach remote ${ensureRemoteName} (${ensureRemoteUrl}). `
          + 'Check network access and credentials for that repository.'
        );
      }
      if (!String(lsRemote.stdout || '').trim()) {
        throw new Error(`Remote branch not found: ${parsedExistingRemote.remoteRef}`);
      }
    } else {
      await ensureRemoteWithUrl(primaryWorktree, ensureRemoteName, ensureRemoteUrl);
      try {
        await fetchRemoteBranchRef(
          primaryWorktree,
          parsedExistingRemote.remote,
          parsedExistingRemote.branch
        );
      } catch (error) {
        const detail = error instanceof Error ? error.message : String(error);
        throw new Error(
          `Unable to fetch ${parsedExistingRemote.remote}/${parsedExistingRemote.branch} `
          + `from ${ensureRemoteUrl}. ${detail}`
        );
      }
    }

    const localBranch = cleanBranchName(preferredBranchName || parsedExistingRemote.branch);
    return {
      localBranch,
      checkoutRef: parsedExistingRemote.remoteRef,
      createLocalBranch: true,
      setUpstream: wantUpstream,
      upstream: {
        remote: explicitUpstreamRemote || parsedExistingRemote.remote,
        branch: explicitUpstreamBranch || parsedExistingRemote.branch,
      },
    };
  }

  if (!requestedExistingBranch) {
    throw new Error('existingBranch is required in existing mode');
  }

  const resolved = await resolveBranchForExistingMode(
    primaryWorktree,
    requestedExistingBranch,
    preferredBranchName
  );
  const upstream = resolved.remoteRef
    ? {
        remote: explicitUpstreamRemote || resolved.remoteRef.remote,
        branch: explicitUpstreamBranch || resolved.remoteRef.branch,
      }
    : (explicitUpstreamRemote && explicitUpstreamBranch
      ? { remote: explicitUpstreamRemote, branch: explicitUpstreamBranch }
      : null);

  return {
    localBranch: resolved.localBranch,
    checkoutRef: resolved.checkoutRef,
    createLocalBranch: resolved.createLocalBranch,
    setUpstream: wantUpstream && Boolean(upstream),
    upstream,
  };
};

/**
 * 用 ls-remote --heads 检查远端分支是否存在（提供 remoteUrl 时直接查 URL）。
 * @returns {{success: boolean, found: boolean}} success=false 表示远端不可达
 */
const checkRemoteBranchExists = async (primaryWorktree, remoteName, branchName, remoteUrl = '') => {
  const remote = String(remoteName || '').trim();
  const branch = String(branchName || '').trim();
  const url = String(remoteUrl || '').trim();
  if (!remote || !branch) {
    return { success: false, found: false };
  }

  const target = url || remote;
  const lsRemote = await runGitCommand(
    primaryWorktree,
    ['ls-remote', '--heads', target, `refs/heads/${branch}`]
  );
  if (!lsRemote.success) {
    return { success: false, found: false };
  }

  return {
    success: true,
    found: Boolean(String(lsRemote.stdout || '').trim()),
  };
};

/**
 * 为 worktree 的本地分支配置 upstream：先确保预置 remote 存在、fetch 目标
 * 分支（失败则放弃配置，避免跟踪从未 fetch 的 ref），再 --set-upstream-to
 * 指向 remote/branch。setUpstream 为假时直接跳过。
 */
const applyUpstreamConfiguration = async (args) => {
  const {
    primaryWorktree,
    worktreeDirectory,
    localBranch,
    setUpstream,
    upstreamRemote,
    upstreamBranch,
    ensureRemoteName,
    ensureRemoteUrl,
  } = args;

  if (!setUpstream) {
    return;
  }

  if (ensureRemoteName && ensureRemoteUrl) {
    await ensureRemoteWithUrl(primaryWorktree, ensureRemoteName, ensureRemoteUrl);
  }

  const upstream = normalizeUpstreamTarget(upstreamRemote, upstreamBranch);
  if (!upstream || !localBranch) {
    return;
  }

  try {
    await fetchRemoteBranchRef(primaryWorktree, upstream.remote, upstream.branch);
  } catch {
    // Fetch failed: leave tracking unset. Do not write branch.*.remote/merge
    // pointing at a ref that was never fetched.
    return;
  }

  await runGitCommandOrThrow(
    worktreeDirectory,
    ['branch', `--set-upstream-to=${upstream.full}`, localBranch],
    `Failed to set upstream to ${upstream.full}`
  );
};

/** 判断目录存在且位于 git 仓库内（rev-parse --git-dir 成功即真）。 */
export async function isGitRepository(directory) {
  const directoryPath = normalizeDirectoryPath(directory);
  if (!directoryPath || !fs.existsSync(directoryPath)) {
    return false;
  }

  const result = await runGitCommand(directoryPath, ['rev-parse', '--git-dir']);
  return result.success;
}

/** 读取全局身份（user.name/user.email/core.sshCommand）；异常归一为 null 字段并记日志。 */
export async function getGlobalIdentity() {
  const git = await createGitForGlobalConfig();

  try {
    const userName = await git.getConfig('user.name', 'global').catch(() => null);
    const userEmail = await git.getConfig('user.email', 'global').catch(() => null);
    const sshCommand = await git.getConfig('core.sshCommand', 'global').catch(() => null);

    return {
      userName: userName?.value || null,
      userEmail: userEmail?.value || null,
      sshCommand: sshCommand?.value || null
    };
  } catch (error) {
    console.error('Failed to get global Git identity:', error);
    return {
      userName: null,
      userEmail: null,
      sshCommand: null
    };
  }
}

/** 读取 remote（默认 origin）的 URL；不存在或失败返回 null。 */
export async function getRemoteUrl(directory, remoteName = 'origin') {
  const git = await createGit(directory);

  try {
    const url = await git.remote(['get-url', remoteName]);
    return url?.trim() || null;
  } catch {
    return null;
  }
}

/** 读取当前生效身份：先查仓库 --local 配置，miss 时回退 --global；异常归一为 null 字段。 */
export async function getCurrentIdentity(directory) {
  const git = await createGit(directory);

  try {

    const userName = await git.getConfig('user.name', 'local').catch(() =>
      git.getConfig('user.name', 'global')
    );

    const userEmail = await git.getConfig('user.email', 'local').catch(() =>
      git.getConfig('user.email', 'global')
    );

    const sshCommand = await git.getConfig('core.sshCommand', 'local').catch(() =>
      git.getConfig('core.sshCommand', 'global')
    );

    return {
      userName: userName?.value || null,
      userEmail: userEmail?.value || null,
      sshCommand: sshCommand?.value || null
    };
  } catch (error) {
    console.error('Failed to get current Git identity:', error);
    return {
      userName: null,
      userEmail: null,
      sshCommand: null
    };
  }
}

/** 判断仓库是否配置了 --local 级别的 user.name 或 user.email。 */
export async function hasLocalIdentity(directory) {
  const git = await createGit(directory);

  try {
    const localName = await git.getConfig('user.name', 'local').catch(() => null);
    const localEmail = await git.getConfig('user.email', 'local').catch(() => null);
    return Boolean(localName?.value || localEmail?.value);
  } catch {
    return false;
  }
}

/**
 * 写入仓库本地身份：user.name/email；authType='ssh' 时写 core.sshCommand
 * （指定密钥 + IdentitiesOnly）并清 credential.helper，'token' 时反向操作
 * （启用 credential.helper=store 并清 sshCommand）；signCommits 时配置 SSH
 * 签名（gpg.format=ssh + user.signingkey + commit.gpgsign）。失败记日志后抛出。
 */
export async function setLocalIdentity(directory, profile) {
  const git = await createGit(directory, { allowUnsafeSshCommand: true });

  try {

    await git.addConfig('user.name', profile.userName, false, 'local');
    await git.addConfig('user.email', profile.userEmail, false, 'local');

    const authType = profile.authType || 'ssh';

    if (authType === 'ssh' && profile.sshKey) {
      await git.raw([
        'config',
        '--local',
        'core.sshCommand',
        buildSshCommand(profile.sshKey)
      ]);
      await git.raw(['config', '--local', '--unset', 'credential.helper']).catch(() => {});
    } else if (authType === 'token' && profile.host) {
      await git.addConfig(
        'credential.helper',
        'store',
        false,
        'local'
      );
      await git.raw(['config', '--local', '--unset', 'core.sshCommand']).catch(() => {});
    }

    if (profile.signCommits === true && typeof profile.signingKey === 'string' && profile.signingKey.trim()) {
      await git.addConfig('gpg.format', 'ssh', false, 'local');
      await git.addConfig('user.signingkey', profile.signingKey.trim(), false, 'local');
      await git.addConfig('commit.gpgsign', 'true', false, 'local');
    }

    return true;
  } catch (error) {
    console.error('Failed to set Git identity:', error);
    throw error;
  }
}

/**
 * 读取仓库状态：当前分支、跟踪分支与 ahead/behind、upstream remote 对比、
 * 文件列表（-uall 逐个列出未跟踪文件）、每文件 diff 统计（staged+working
 * 的 numstat 累加；新文件 git 不计行数，改为读文件按行估算，上限 200 个/1MB），
 * 以及进行中的 merge/rebase 摘要。无跟踪分支时按 origin/HEAD → main/master
 * 候选计算"未发布提交数"。light 模式跳过统计类开销供轮询使用。
 * 非仓库/目录消失错误统一改写为标准 fatal 文案再抛，供上层优雅处理而非 500。
 */
export async function getStatus(directory, options = {}) {
  const lightMode = options.mode === 'light';
  const normalizedDirectory = normalizeDirectoryPath(directory);
  if (typeof normalizedDirectory !== 'string' || !normalizedDirectory.trim()) {
    throw new Error('directory is required');
  }

  try {
    // Prefer an explicit non-repo check before simple-git status so a missing
    // repository never depends on process.cwd() or an opaque GitError shape.
    if (!(await isGitRepository(normalizedDirectory))) {
      throw new Error('fatal: not a git repository (or any of the parent directories): .git');
    }

    const { directoryPath, repoRoot, git } = await createRepositoryGitContext(normalizedDirectory);

    // Use -uall to show all untracked files individually, not just directories
    const status = await git.status(['-uall']);

    // Light mode: skip numstat + new-file line counting for faster response
    const [stagedStatsRaw, workingStatsRaw] = lightMode
      ? ['', '']
      : await Promise.all([
          git.raw(['diff', '--cached', '--numstat']).catch(() => ''),
          git.raw(['diff', '--numstat']).catch(() => ''),
        ]);

    const diffStatsMap = new Map();

    const accumulateStats = (raw) => {
      if (!raw) return;
      raw
        .split('\n')
        .map((line) => line.trim())
        .filter(Boolean)
        .forEach((line) => {
          const parts = line.split('\t');
          if (parts.length < 3) {
            return;
          }
          const [insertionsRaw, deletionsRaw, ...pathParts] = parts;
          const path = pathParts.join('\t');
          if (!path) {
            return;
          }
          const insertions = insertionsRaw === '-' ? 0 : parseInt(insertionsRaw, 10) || 0;
          const deletions = deletionsRaw === '-' ? 0 : parseInt(deletionsRaw, 10) || 0;

          const existing = diffStatsMap.get(path) || { insertions: 0, deletions: 0 };
          diffStatsMap.set(path, {
            insertions: existing.insertions + insertions,
            deletions: existing.deletions + deletions,
          });
        });
    };

    accumulateStats(stagedStatsRaw);
    accumulateStats(workingStatsRaw);

    const diffStats = Object.fromEntries(diffStatsMap.entries());

    const MAX_NEW_FILE_STATS = 200;
    const MAX_NEW_FILE_STAT_SIZE = 1024 * 1024;
    const newFileStats = [];

    if (!lightMode) {
      for (const file of status.files) {
        if (newFileStats.length >= MAX_NEW_FILE_STATS) {
          break;
        }

        const working = (file.working_dir || '').trim();
        const indexStatus = (file.index || '').trim();
        const statusCode = working || indexStatus;

        if (statusCode !== '?' && statusCode !== 'A') {
          continue;
        }

        const existing = diffStats[file.path];
        if (existing && existing.insertions > 0) {
          continue;
        }

        const absolutePath = path.join(repoRoot, file.path);

        try {
          const stat = await fsp.stat(absolutePath);
          if (!stat.isFile() || stat.size > MAX_NEW_FILE_STAT_SIZE) {
            continue;
          }

          const buffer = await fsp.readFile(absolutePath);
          if (buffer.indexOf(0) !== -1) {
            newFileStats.push({
              path: file.path,
              insertions: existing?.insertions ?? 0,
              deletions: existing?.deletions ?? 0,
            });
            continue;
          }

          const normalized = buffer.toString('utf8').replace(/\r\n/g, '\n');
          if (!normalized.length) {
            newFileStats.push({
              path: file.path,
              insertions: 0,
              deletions: 0,
            });
            continue;
          }

          const segments = normalized.split('\n');
          if (normalized.endsWith('\n')) {
            segments.pop();
          }

          const lineCount = segments.length;
          newFileStats.push({
            path: file.path,
            insertions: lineCount,
            deletions: 0,
          });
        } catch (error) {
          if (error?.code !== 'ENOENT') {
            console.warn('Failed to estimate diff stats for new file', file.path, error);
          }
        }
      }
    }

    for (const entry of newFileStats) {
      diffStats[entry.path] = {
        insertions: entry.insertions,
        deletions: entry.deletions,
      };
    }

    const selectBaseRefForUnpublished = async () => {
      const candidates = [];

      const originHead = await git
        .raw(['symbolic-ref', '-q', 'refs/remotes/origin/HEAD'])
        .then((value) => String(value || '').trim())
        .catch(() => '');

      if (originHead) {
        // "refs/remotes/origin/main" -> "origin/main"
        candidates.push(originHead.replace(/^refs\/remotes\//, ''));
      }

      candidates.push('origin/main', 'origin/master', 'main', 'master');

      for (const ref of candidates) {
        const exists = await git
          .raw(['rev-parse', '--verify', ref])
          .then((value) => String(value || '').trim())
          .catch(() => '');
        if (exists) return ref;
      }

      return null;
    };

    let tracking = status.tracking || null;
    let ahead = status.ahead;
    let behind = status.behind;
    let upstreamComparison;

    // When no upstream is configured (common for new worktree branches), Git doesn't report ahead/behind.
    // We still want to show the number of unpublished commits to the user.
    // Light mode skips this — the basic ahead/behind from git status is sufficient for polling.
    if (!lightMode && !tracking && status.current) {
      const baseRef = await selectBaseRefForUnpublished();
      if (baseRef) {
        const countRaw = await git
          .raw(['rev-list', '--count', `${baseRef}..HEAD`])
          .then((value) => String(value || '').trim())
          .catch(() => '');
        const count = parseInt(countRaw, 10);
        if (Number.isFinite(count)) {
          ahead = count;
          behind = 0;
        }
      }
    }

    if (
      !lightMode
      && status.current
      && (!tracking || !tracking.startsWith('upstream/'))
      && await hasRemote(git, directoryPath, 'upstream')
    ) {
      upstreamComparison = await getRemoteBranchComparison(git, 'upstream', status.current);
    }

    // Check for in-progress operations
    let mergeInProgress = null;
    let rebaseInProgress = null;

    try {
      // Check MERGE_HEAD for merge in progress
      const mergeHeadExists = await git
        .raw(['rev-parse', '--verify', '--quiet', 'MERGE_HEAD'])
        .then(() => true)
        .catch(() => false);
      
      if (mergeHeadExists) {
        const mergeHead = await git.raw(['rev-parse', 'MERGE_HEAD']).catch(() => '');
        const headSha = mergeHead.trim().slice(0, 7);
        // Only set mergeInProgress if we actually have a valid head SHA
        if (headSha) {
          const mergeMsgPath = await resolveGitInternalPath(repoRoot, git, 'MERGE_MSG').catch(() => '');
          const mergeMsg = mergeMsgPath ? await fsp.readFile(mergeMsgPath, 'utf8').catch(() => '') : '';
          mergeInProgress = {
            head: headSha,
            message: mergeMsg.split('\n')[0] || '',
          };
        }
      }
    } catch {
      // ignore
    }

    try {
      // Check for rebase in progress (.git/rebase-merge or .git/rebase-apply)
      const rebaseMergePath = await resolveGitInternalPath(repoRoot, git, 'rebase-merge').catch(() => '');
      const rebaseApplyPath = await resolveGitInternalPath(repoRoot, git, 'rebase-apply').catch(() => '');
      const rebaseMergeExists = rebaseMergePath ? await fsp.stat(rebaseMergePath).then(() => true).catch(() => false) : false;
      const rebaseApplyExists = rebaseApplyPath ? await fsp.stat(rebaseApplyPath).then(() => true).catch(() => false) : false;
      
      if (rebaseMergeExists || rebaseApplyExists) {
        const rebasePath = rebaseMergeExists ? rebaseMergePath : rebaseApplyPath;
        const headName = await fsp.readFile(path.join(rebasePath, 'head-name'), 'utf8').catch(() => '');
        const onto = await fsp.readFile(path.join(rebasePath, 'onto'), 'utf8').catch(() => '');
        
        const headNameTrimmed = headName.trim().replace('refs/heads/', '');
        const ontoTrimmed = onto.trim().slice(0, 7);
        
        // Only set rebaseInProgress if we have valid data
        if (headNameTrimmed || ontoTrimmed) {
          rebaseInProgress = {
            headName: headNameTrimmed,
            onto: ontoTrimmed,
          };
        }
      }
    } catch {
      // ignore
    }

    return {
      current: status.current,
      tracking,
      ahead,
      behind,
      upstreamComparison,
      files: status.files.map((f) => ({
        path: f.path,
        index: f.index,
        working_dir: f.working_dir,
      })),
      isClean: status.isClean(),
      diffStats: lightMode ? undefined : diffStats,
      mergeInProgress,
      rebaseInProgress,
    };
  } catch (error) {
    if (isNotGitRepositoryError(error) || isMissingDirectoryError(error)) {
      // Re-throw a plain Error so route/session callers can match reliably and
      // continue enumerating other projects instead of treating GitError as 500.
      throw new Error('fatal: not a git repository (or any of the parent directories): .git');
    }
    console.error('Failed to get Git status:', error);
    throw error;
  }
}

/**
 * 获取 diff 文本（工作区或 --cached 暂存区）：可指定单文件与上下文行数；
 * 未跟踪文件回退 `diff --no-index /dev/null 文件`（exit 1 视为有差异而非错误），
 * 符号链接手工构造 120000 模式的 patch；staged 或已跟踪文件的空输出即真无差异。
 */
export async function getDiff(directory, { path: filePath, staged = false, contextLines = 3 } = {}) {
  const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);

  try {
    const args = ['diff', '--no-color', NO_EXT_DIFF];
    const fileContext = filePath ? await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot) : null;

    if (typeof contextLines === 'number' && !Number.isNaN(contextLines)) {
      args.push(`-U${Math.max(0, contextLines)}`);
    }

    if (staged) {
      args.push('--cached');
    }

    if (fileContext) {
      args.push('--', fileContext.repoPath);
    }

    const diff = await git.raw(args);
    if (diff && diff.trim().length > 0) {
      return diff;
    }

    if (staged) {
      return diff;
    }

    if (!fileContext) {
      return diff;
    }

    try {
      await git.raw(['ls-files', '--error-unmatch', '--', fileContext.repoPath]);
      return diff;
    } catch {
      if (fileContext.isSymbolicLink) {
        const target = await fsp.readlink(fileContext.absolutePath);
        return [
          `diff --git a/${fileContext.repoPath} b/${fileContext.repoPath}`,
          'new file mode 120000',
          '--- /dev/null',
          `+++ b/${fileContext.repoPath}`,
          '@@ -0,0 +1 @@',
          `+${target}`,
          '\\ No newline at end of file',
          '',
        ].join('\n');
      }

      const noIndexArgs = ['diff', '--no-color', NO_EXT_DIFF];
      if (typeof contextLines === 'number' && !Number.isNaN(contextLines)) {
        noIndexArgs.push(`-U${Math.max(0, contextLines)}`);
      }
      noIndexArgs.push('--no-index', '--', '/dev/null', fileContext.repoPath);
      try {
        const noIndexDiff = await git.raw(noIndexArgs);
        return noIndexDiff;
      } catch (noIndexError) {
        // git diff --no-index returns exit code 1 when differences exist (not a real error)
        if (noIndexError.exitCode === 1 && noIndexError.message) {
          return noIndexError.message;
        }
        throw noIndexError;
      }
    }
  } catch (error) {
    console.error('Failed to get Git diff:', error);
    throw error;
  }
}

/**
 * Individual untracked file paths, honoring ignore rules.
 *
 * Deliberately not `--directory`: collapsed directory entries end in a slash
 * and are not valid inputs to the per-file diff helpers, so a caller would
 * silently lose every file inside a new directory. Listing files costs more
 * entries but each one is usable.
 *
 * Callers that only need this list should not pay for `getStatus`, which also
 * computes ahead/behind, diff stats, and merge state — an order of magnitude
 * more work for an answer they throw away.
 */
/**
 * （中文补充）列出未跟踪文件的逐个路径（见上方英文说明：遵守 ignore 规则，
 * 刻意不用 --directory 折叠目录，保证每个条目都能直接喂给按文件的 diff 接口）。
 */
export async function listUntrackedPaths(directory) {
  const { repoRoot } = await createRepositoryGitContext(directory);
  const result = await runGitCommand(repoRoot, [
    'ls-files',
    '--others',
    '--exclude-standard',
  ]);
  if (!result.success) return [];
  return String(result.stdout || '')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
}

/**
 * Diffs for untracked files, produced against an empty tree.
 *
 * `getDiff` re-resolves the repository context on every call, which costs an
 * extra `rev-parse` per file; a walkthrough of a branch with thirty new files
 * pays that thirty times. This resolves once and reuses it, with a bounded pool
 * so a repository full of new files cannot flood the process table.
 *
 * Returns one entry per input path, in order; unreadable paths yield `''`
 * rather than failing the batch.
 */
/**
 * （中文补充）并发批量生成未跟踪文件对空树的 diff（见上方英文说明：
 * 单次解析仓库上下文复用 + 受限并发池，逐路径返回、读不到的给空串）。
 */
export async function getUntrackedDiffs(directory, filePaths = [], { concurrency = 8, contextLines = 3 } = {}) {
  const paths = (Array.isArray(filePaths) ? filePaths : []).filter((value) => typeof value === 'string' && value);
  if (paths.length === 0) return [];

  const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
  const results = new Array(paths.length).fill('');
  let cursor = 0;

  const worker = async () => {
    while (cursor < paths.length) {
      const index = cursor++;
      try {
        const fileContext = await resolveGitFileContext(directoryPath, directoryGit, paths[index], repoRoot);
        const args = ['diff', '--no-color', NO_EXT_DIFF];
        if (typeof contextLines === 'number' && !Number.isNaN(contextLines)) {
          args.push(`-U${Math.max(0, contextLines)}`);
        }
        args.push('--no-index', '--', '/dev/null', fileContext.repoPath);
        try {
          results[index] = await git.raw(args);
        } catch (error) {
          // `git diff --no-index` exits 1 whenever there are differences, which
          // for a new file is always.
          results[index] = error?.exitCode === 1 && error?.message ? error.message : '';
        }
      } catch {
        results[index] = '';
      }
    }
  };

  await Promise.all(Array.from({ length: Math.min(concurrency, paths.length) }, worker));
  return results;
}

/** 判断 ref 是否解析到某个 commit（rev-parse --verify --quiet，异常按否处理）。 */
const refResolvesToCommit = async (git, ref) => git
  .raw(['rev-parse', '--verify', '--quiet', `${ref}^{commit}`])
  .then((value) => Boolean(String(value || '').trim()))
  .catch(() => false);

/**
 * The branch list includes remote-only branches that `ls-remote` reported but
 * the repository never fetched (#2098), so a comparison can name a ref that does
 * not exist locally. Say that plainly instead of letting git's "ambiguous
 * argument" surface as an opaque failure.
 */
/**
 * （中文补充）校验范围比较用到的每个 ref 都能在本地解析到 commit，
 * 否则抛出明确的"先 fetch 再比较"提示（见上方英文说明）。
 */
async function assertRangeRefsResolve(git, refs) {
  for (const ref of refs) {
    if (!(await refResolvesToCommit(git, ref))) {
      throw new Error(`Ref "${ref}" is not available locally. Fetch it before comparing.`);
    }
  }
}

/**
 * 计算 base...head 三点 diff：base 优先用 origin/&lt;base&gt;（本地基线过期时
 * 已合并提交不重现），本地分支不存在时再在任意 remote 的同名分支中找；
 * ref 不可解析会抛出"先 fetch"错误；可限定单文件与上下文行数。
 */
export async function getRangeDiff(directory, { base, head, path: filePath, contextLines = 3 } = {}) {
  const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
  const baseRef = typeof base === 'string' ? base.trim() : '';
  const headRef = typeof head === 'string' ? head.trim() : '';
  if (!baseRef || !headRef) {
    throw new Error('base and head are required');
  }

  // Prefer remote-tracking base ref so merged commits don't reappear
  // when local base branch is stale (common when user stays on feature branch).
  let resolvedBase = baseRef;
  const originCandidate = `refs/remotes/origin/${baseRef}`;
  try {
    const verified = await git.raw(['rev-parse', '--verify', originCandidate]);
    if (verified && verified.trim()) {
      resolvedBase = `origin/${baseRef}`;
    }
  } catch {
    // ignore
  }

  // Not every repository has an `origin`. When the base names a branch that
  // exists only on another remote, a bare name does not resolve — git looks in
  // refs/heads, not across remotes — and the diff fails with "ambiguous
  // argument". Fall back to whichever remote actually carries it.
  if (resolvedBase === baseRef && !/[*?[\]^~:\\]/.test(baseRef)) {
    const resolvesLocally = await git
      .raw(['rev-parse', '--verify', `refs/heads/${baseRef}`])
      .then((value) => Boolean(String(value || '').trim()))
      .catch(() => false);

    if (!resolvesLocally) {
      const remoteMatch = await git
        .raw(['for-each-ref', '--count=1', '--format=%(refname:short)', `refs/remotes/*/${baseRef}`])
        .then((value) => String(value || '').trim())
        .catch(() => '');
      if (remoteMatch) {
        resolvedBase = remoteMatch;
      }
    }
  }

  await assertRangeRefsResolve(git, [resolvedBase, headRef]);

  const args = ['diff', '--no-color', NO_EXT_DIFF];
  if (typeof contextLines === 'number' && !Number.isNaN(contextLines)) {
    args.push(`-U${Math.max(0, contextLines)}`);
  }
  args.push(`${resolvedBase}...${headRef}`);
  if (filePath) {
    const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
    args.push('--', fileContext.repoPath);
  }
  const diff = await git.raw(args);
  return diff;
}

/** 匹配分支 reflog 中 "branch: Created from 来源" 记录的正则。 */
const BRANCH_CREATION_SOURCE_RE = /^branch: Created from (.+)$/;

/**
 * Parse a branch reflog (`git reflog show --format=%gs <branch>`) and return the
 * ref the branch was created from, when that source is itself a named ref.
 *
 * Returns null when the branch was created from `HEAD` (bare, as `git switch -c`
 * / `git checkout -b` without an explicit start point record) or a raw commit
 * (detached start): the original branch name is not recorded anywhere in that
 * case, and guessing a base from commit topology would be a heuristic, not an
 * answer. Callers should ask the user to pick a base instead.
 */
/**
 * （中文补充）解析 reflog 文本，提取分支创建时的具名来源；来源为裸 HEAD /
 * HEAD@{...} / 裸 commit hash 时返回 null（见上方英文说明），调用方应让用户自选基线。
 */
export function parseBranchCreationSource(reflogText) {
  const lines = String(reflogText || '')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
  // Reflog lists newest entries first; the creation entry is the oldest one.
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const match = lines[index].match(BRANCH_CREATION_SOURCE_RE);
    if (!match) continue;
    const source = match[1].trim();
    // Bare `HEAD` (`git switch -c` from the current branch) and `HEAD@{...}`
    // (detached start) both lack a named source; a raw commit hash does too.
    if (!source || /^HEAD(@|$)/.test(source) || /^[0-9a-f]{7,40}$/i.test(source)) {
      return null;
    }
    return source;
  }
  return null;
}

/**
 * Resolve the branch the given branch was created from, from its reflog.
 * Returns { base: null } when git has no authoritative record (clone, detached
 * start, reflog expired) — callers must not fall back to main/master.
 */
/**
 * （中文补充）从 reflog 解析分支的创建来源分支（见上方英文说明）；
 * git 无权威记录（clone、detached 起点、reflog 过期）时返回 { base: null }。
 */
export async function getBranchBase(directory, branch) {
  const branchName = String(branch || '').trim();
  if (!branchName) {
    throw new Error('branch is required');
  }

  const { git } = await createRepositoryGitContext(directory);

  let reflog = '';
  try {
    reflog = await git.raw(['reflog', 'show', '--format=%gs', branchName]);
  } catch {
    return { base: null };
  }

  const source = parseBranchCreationSource(reflog);
  if (!source || source === branchName) {
    return { base: null };
  }

  const resolves = await git
    .raw(['rev-parse', '--verify', '--quiet', source])
    .then((value) => Boolean(String(value || '').trim()))
    .catch(() => false);
  if (!resolves) {
    return { base: null };
  }

  return { base: source };
}

/**
 * 列出 base...head 范围内变更的文件：--name-status -z -C（带复制检测），
 * R/C 条目取第二个路径 token（目标路径），其余取第一个；返回 [{ path, status }]。
 * base 同样优先解析为 origin/&lt;base&gt;。
 */
export async function getRangeFiles(directory, { base, head } = {}) {
  const { git } = await createRepositoryGitContext(directory);
  const baseRef = typeof base === 'string' ? base.trim() : '';
  const headRef = typeof head === 'string' ? head.trim() : '';
  if (!baseRef || !headRef) {
    throw new Error('base and head are required');
  }

  let resolvedBase = baseRef;
  const originCandidate = `refs/remotes/origin/${baseRef}`;
  try {
    const verified = await git.raw(['rev-parse', '--verify', originCandidate]);
    if (verified && verified.trim()) {
      resolvedBase = `origin/${baseRef}`;
    }
  } catch {
    // ignore
  }

  await assertRangeRefsResolve(git, [resolvedBase, headRef]);

  // `-C` (copy detection among changed files only, so cheap) makes copies
  // surface as C entries instead of plain additions; rename detection is on
  // by default.
  const raw = await git.raw(['diff', '--name-status', '-z', '-C', `${resolvedBase}...${headRef}`]);
  // -z format: STATUS\0PATH\0[ORIG\0] repeated. For rename/copy entries
  // (`R100`, `C75`) the first path token is the ORIGINAL path and the second
  // is the DESTINATION — the diff (and the UI) must address the destination.
  const tokens = String(raw || '').split('\0');
  const files = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const status = (tokens[index] || '').trim();
    if (!status) continue;
    const isRenameOrCopy = status.startsWith('R') || status.startsWith('C');
    const path = isRenameOrCopy ? (tokens[index + 2] || '').trim() : (tokens[index + 1] || '').trim();
    index += isRenameOrCopy ? 2 : 1;
    if (path) {
      files.push({ path, status: status.charAt(0) });
    }
  }
  return files;
}

/** 按图片处理的文件扩展名列表。 */
const IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'ico', 'bmp', 'avif'];

/** 二进制嗅探读取的头部字节数（8KB）。 */
const BINARY_SNIFF_BYTES = 8192;

/** 按扩展名判断文件是否为图片。 */
function isImageFile(filePath) {
  const ext = filePath.split('.').pop()?.toLowerCase();
  return IMAGE_EXTENSIONS.includes(ext || '');
}

/** 扩展名到图片 MIME 的映射；未知扩展名返回 application/octet-stream。 */
function getImageMimeType(filePath) {
  const ext = filePath.split('.').pop()?.toLowerCase();
  const mimeMap = {
    'png': 'image/png',
    'jpg': 'image/jpeg',
    'jpeg': 'image/jpeg',
    'gif': 'image/gif',
    'svg': 'image/svg+xml',
    'webp': 'image/webp',
    'ico': 'image/x-icon',
    'bmp': 'image/bmp',
    'avif': 'image/avif',
  };
  return mimeMap[ext] || 'application/octet-stream';
}

/** 从 numstat 首行判断二进制：新增或删除列出现 '-' 即二进制；空输出为否。 */
const parseIsBinaryFromNumstat = (raw) => {
  const text = String(raw || '').trim();
  if (!text) {
    return false;
  }

  // Expected format: <added>\t<deleted>\t<path>
  const firstLine = text.split('\n').map((line) => line.trim()).find(Boolean) || '';
  const [added, deleted] = firstLine.split('\t');
  return added === '-' || deleted === '-';
};

/** 从 name-status 行提取路径：R/C 状态行为 "原路径\t目标路径"，取目标路径。 */
const extractGitStatusPath = (status, pathPart) => {
  if ((status === 'R' || status === 'C') && pathPart.includes('\t')) {
    return pathPart.split('\t').pop() || pathPart;
  }
  return pathPart;
};

/** 把 numstat 的重命名路径还原为纯目标路径：支持 {a => b} 大括号简写与 old => new 全量形式。 */
const extractGitNumstatDestinationPath = (filePath) => {
  if (!filePath.includes(' => ')) {
    return filePath;
  }

  const braceMatch = filePath.match(/^(.*)\{([^{}]*)\s=>\s([^{}]*)\}(.*)$/);
  if (braceMatch) {
    const [, prefix, , destination, suffix] = braceMatch;
    return `${prefix}${destination}${suffix}`.replace(/\/+/g, '/');
  }

  return filePath.split(' => ').pop()?.trim() || filePath;
};

/** 二进制嗅探：读文件前 8KB，含 NUL 字节即判为二进制；读取失败按非二进制处理。 */
const looksBinaryBySniff = async (absolutePath) => {
  try {
    const handle = await fsp.open(absolutePath, 'r');
    try {
      const buffer = Buffer.alloc(BINARY_SNIFF_BYTES);
      const { bytesRead } = await handle.read(buffer, 0, BINARY_SNIFF_BYTES, 0);
      if (bytesRead <= 0) {
        return false;
      }
      return buffer.subarray(0, bytesRead).includes(0);
    } finally {
      await handle.close();
    }
  } catch {
    return false;
  }
};

/**
 * 判断文件的 diff 是否二进制：先用 numstat 的 '-' 标记判定；未跟踪文件 diff
 * 输出为空，再用 --no-index 对 /dev/null 的 numstat/错误文案判定。
 */
const isBinaryDiff = async (directoryPath, filePath, staged) => {
  // Fast path: ask git for numstat. For binary, it returns "-\t-\t<path>".
  const args = ['diff', '--numstat'];
  if (staged) {
    args.push('--cached');
  }
  args.push('--', filePath);

  const result = await runGitCommand(directoryPath, args);
  if (parseIsBinaryFromNumstat(result.stdout)) {
    return true;
  }

  // Fallback for untracked files (diff output is empty): use --no-index against /dev/null
  if (!staged) {
    const tracked = await runGitCommand(directoryPath, ['ls-files', '--error-unmatch', '--', filePath]).then((r) => r.success);
    if (!tracked) {
      const noIndex = await runGitCommand(directoryPath, ['diff', '--no-index', '--numstat', '--', '/dev/null', filePath]);
      if (parseIsBinaryFromNumstat(noIndex.stdout) || parseIsBinaryFromNumstat(noIndex.stderr) || parseIsBinaryFromNumstat(noIndex.message)) {
        return true;
      }
      const text = `${noIndex.stdout || ''}\n${noIndex.stderr || ''}\n${noIndex.message || ''}`.toLowerCase();
      if (text.includes('binary files') || text.includes('git binary patch')) {
        return true;
      }
    }
  }

  return false;
};

/**
 * 取单文件两侧内容供前端渲染：文本返回 original（HEAD 或 index 中版本）与
 * modified（工作区/暂存版本）字符串；图片（按扩展名）返回 data URL；
 * 二进制（8KB NUL 嗅探或 git numstat 判定）只置 isBinary 标志；
 * 符号链接的 modified 取链接目标。统一把 CRLF 归一为 LF。
 */
export async function getFileDiff(directory, { path: filePath, staged = false } = {}) {
  if (!directory || !filePath) {
    throw new Error('directory and path are required for getFileDiff');
  }

  const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
  const isImage = isImageFile(filePath);
  const mimeType = isImage ? getImageMimeType(filePath) : null;
  const { absolutePath, repoPath, isSymbolicLink } = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);

  if (!isImage && !isSymbolicLink) {
    const isBinaryBySniff = await looksBinaryBySniff(absolutePath);
    const isBinary = isBinaryBySniff || (await isBinaryDiff(repoRoot, repoPath, staged));
    if (isBinary) {
      return {
        original: '',
        modified: '',
        path: filePath,
        isBinary: true,
      };
    }
  }

  let original = '';
  try {
    if (isImage) {
      // For images, use git show with raw output and convert to base64
      try {
        const { stdout } = await execFileAsync(getGitBinary(), ['show', `HEAD:${repoPath}`], {
          cwd: repoRoot,
          encoding: 'buffer',
          windowsHide: true,
          maxBuffer: 50 * 1024 * 1024, // 50MB max
        });
        if (stdout && stdout.length > 0) {
          original = `data:${mimeType};base64,${stdout.toString('base64')}`;
        }
      } catch {
        original = '';
      }
    } else {
      original = await git.show([`HEAD:${repoPath}`]);
    }
  } catch {
    original = '';
  }

  let modified = '';
  try {
    if (staged) {
      if (isImage) {
        const { stdout } = await execFileAsync(getGitBinary(), ['show', `:${repoPath}`], {
          cwd: repoRoot,
          encoding: 'buffer',
          windowsHide: true,
          maxBuffer: 50 * 1024 * 1024,
        });
        if (stdout && stdout.length > 0) {
          modified = `data:${mimeType};base64,${stdout.toString('base64')}`;
        }
      } else {
        modified = await git.show([`:${repoPath}`]);
      }
    } else {
      if (isSymbolicLink) {
        modified = await fsp.readlink(absolutePath);
      } else {
        const stat = await fsp.stat(absolutePath);
        if (!stat.isFile()) {
          return {
            original: typeof original === 'string' ? original.replace(/\r\n/g, '\n') : original,
            modified: '',
            path: filePath,
            isBinary: false,
          };
        }
        if (isImage) {
          // For images, read as binary and convert to data URL
          const buffer = await fsp.readFile(absolutePath);
          modified = `data:${mimeType};base64,${buffer.toString('base64')}`;
        } else {
          modified = await fsp.readFile(absolutePath, 'utf8');
        }
      }
    }
  } catch (error) {
    if (error && typeof error === 'object' && error.code === 'ENOENT') {
      modified = '';
    } else {
      console.error('Failed to read modified file contents for diff:', error);
      throw error;
    }
  }

  return {
    original: typeof original === 'string' ? original.replace(/\r\n/g, '\n') : original,
    modified: typeof modified === 'string' ? modified.replace(/\r\n/g, '\n') : modified,
    path: filePath,
    isBinary: false,
  };
}

/**
 * 还原单个文件（index 变更队列内）：未跟踪文件用 `git clean -f -d`（失败退化为
 * 直接 rm）；已跟踪文件 scope='all' 时先 restore --staged（老 git 回退
 * reset HEAD）再 restore 工作区（回退 checkout --）；scope='working' 只动工作区。
 */
export async function revertFile(directory, filePath, options = {}) {
  return withGitIndexMutationQueue(directory, async () => {
    const scope = options?.scope === 'working' ? 'working' : 'all';
    const directoryPath = normalizeDirectoryPath(directory);
    const directoryGit = await createGit(directoryPath);
    const repoRoot = await resolveGitRepositoryRoot(directoryPath, directoryGit);
    const { absolutePath, repoPath } = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
    const git = await createGit(repoRoot);

    const isTracked = await git
      .raw(['ls-files', '--error-unmatch', '--', repoPath])
      .then(() => true)
      .catch(() => false);

    if (!isTracked) {
      try {
        await git.raw(['clean', '-f', '-d', '--', repoPath]);
        return;
      } catch (cleanError) {
        try {
          await fsp.rm(absolutePath, { recursive: true, force: true });
          return;
        } catch (fsError) {
          if (fsError && typeof fsError === 'object' && fsError.code === 'ENOENT') {
            return;
          }
          console.error('Failed to remove untracked file during revert:', fsError);
          throw fsError;
        }
      }
    }

    if (scope === 'all') {
      try {
        await git.raw(['restore', '--staged', '--', repoPath]);
      } catch (error) {
        await git.raw(['reset', 'HEAD', '--', repoPath]).catch(() => {});
      }
    }

    try {
      await git.raw(['restore', '--', repoPath]);
    } catch (error) {
      try {
        await git.raw(['checkout', '--', repoPath]);
      } catch (fallbackError) {
        console.error('Failed to revert git file:', fallbackError);
        throw fallbackError;
      }
    }
  });
}

/** hunk 操作到 git apply 参数的映射：stage=--cached、unstage=--cached --reverse、discard=--reverse。 */
const HUNK_ACTION_FLAGS = {
  stage: ['--cached'],
  unstage: ['--cached', '--reverse'],
  discard: ['--reverse'],
};

/**
 * 解析 diff 头行（---/+++）中的路径 token：支持 C 风格双引号转义形式
 * （按 JSON.parse 解码，失败退化为去引号）与 tab 附加字段（截到第一列）；
 * /dev/null 或空值返回 null。
 */
const parsePatchPathToken = (line) => {
  const value = String(line || '').replace(/^(?:-{3}|\+{3})\s+/, '');
  if (!value || value === '/dev/null') {
    return null;
  }

  if (value.startsWith('"')) {
    let token = '"';
    let escaped = false;
    for (let index = 1; index < value.length; index += 1) {
      const char = value[index];
      token += char;
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '"') {
        break;
      }
    }

    try {
      return JSON.parse(token);
    } catch {
      return token.slice(1, token.endsWith('"') ? -1 : undefined);
    }
  }

  return value.split('\t', 1)[0] || null;
};

/** 归一 patch 路径：剥掉 a/ 或 b/ 前缀；/dev/null 与空值返回 null。 */
const normalizePatchTargetPath = (value) => {
  if (!value || value === '/dev/null') {
    return null;
  }
  return value.replace(/^[ab]\//, '');
};

/** 从 patch 的 ---/+++ 头里提取首个真实目标路径（经 token 解析与 a/ b/ 前缀归一）。 */
const extractPatchTargetPath = (patch) => {
  const matches = [...patch.matchAll(/^(?:-{3}|\+{3})\s+.+$/gm)];
  const realTargets = matches
    .map((match) => normalizePatchTargetPath(parsePatchPathToken(match[0])))
    .filter(Boolean);
  return realTargets[0] || null;
};

/** 把 patch 文本写入系统临时目录中的唯一文件并返回路径（applyHunk 用后即删）。 */
const writeTempPatchFile = async (patch) => {
  const tmpDir = os.tmpdir();
  const tmpPath = path.join(tmpDir, `ompchamber-hunk-${Date.now()}-${Math.random().toString(36).slice(2)}.patch`);
  await fsp.writeFile(tmpPath, patch, 'utf8');
  return tmpPath;
};

/**
 * 按动作应用单个 hunk：stage/unstage/discard 映射到 git apply 参数组合；
 * 校验 patch 含 @@ 头、目标路径与请求文件一致，先 --check 预检（失败提示
 * "Hunk 不再适用"）再真正应用；patch 落临时文件、用完即删。
 * 全程在 index 变更队列内串行。
 */
export async function applyHunk(directory, filePath, options = {}) {
  const action = options?.action;
  if (!action || !HUNK_ACTION_FLAGS[action]) {
    throw new Error('Invalid hunk action');
  }
  const patch = typeof options?.patch === 'string' ? options.patch : '';
  if (!patch.trim()) {
    throw new Error('patch is required to apply a hunk');
  }
  if (!/^@@\s/m.test(patch)) {
    throw new Error('patch does not contain a hunk header');
  }

  return withGitIndexMutationQueue(directory, async () => {
    const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
    const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
    validateRepositoryFilePaths(repoRoot, [fileContext.repoPath]);

    const targetPath = extractPatchTargetPath(patch);
    if (targetPath && targetPath !== fileContext.repoPath && targetPath !== filePath) {
      throw new Error('patch target path does not match the requested file');
    }

    const flags = HUNK_ACTION_FLAGS[action];
    let tmpPath = null;
    try {
      tmpPath = await writeTempPatchFile(patch);

      try {
        await git.raw(['apply', ...flags, '--check', tmpPath]);
      } catch (checkError) {
        const text = parseGitErrorText(checkError);
        throw new Error(
          text
            ? `Hunk no longer applies — refresh and try again.\n${text}`
            : 'Hunk no longer applies — refresh and try again.'
        );
      }

      await git.raw(['apply', ...flags, tmpPath]);
    } finally {
      if (tmpPath) {
        await fsp.rm(tmpPath, { force: true }).catch(() => {});
      }
    }
  });
}

/** 逐文件收集工作区 diff（跳过失败与空结果），返回 [{ path, diff }] 列表。 */
export async function collectDiffs(directory, files = []) {
  const results = [];
  for (const filePath of files) {
    try {
      const diff = await getDiff(directory, { path: filePath });
      if (diff && diff.trim().length > 0) {
        results.push({ path: filePath, diff });
      }
    } catch (error) {
      console.error(`Failed to diff ${filePath}:`, error);
    }
  }
  return results;
}

/**
 * 拉取并合并：rebase=true 时附加 --rebase 选项；只给 remote 未给 branch 时
 * 读取当前分支补全参数（simple-git 需两者同给才会传 remote）。
 * 返回 simple-git 的 summary/files/insertions/deletions。
 */
export async function pull(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);
  const pullOptions = options.rebase === true
    ? { ...(options.options && typeof options.options === 'object' && !Array.isArray(options.options) ? options.options : {}), '--rebase': null }
    : options.options || {};

  try {
    const remote = String(options.remote || '').trim();
    const requestedBranch = String(options.branch || '').trim();
    let branch = requestedBranch;

    if (remote && !branch) {
      // simple-git only includes the remote when both remote and branch are provided.
      // Resolve the current branch so selecting a remote in the UI really runs `git pull <remote> <branch>`.
      const status = await git.status();
      branch = String(status.current || '').trim();
    }

    const result = await git.pull(
      remote || 'origin',
      branch || undefined,
      pullOptions
    );

    return {
      success: true,
      summary: result.summary,
      files: result.files,
      insertions: result.insertions,
      deletions: result.deletions
    };
  } catch (error) {
    console.error('Failed to pull:', error);
    throw error;
  }
}

/** 列出 stash：自定义 %x1f 分隔格式解析出 ref/message/relativeTime/hash。 */
export async function listStashes(directory) {
  const { git } = await createRepositoryGitContext(directory);
  const output = await git.raw(['stash', 'list', '--format=%gd%x1f%gs%x1f%cr%x1f%H']);
  return String(output || '')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [ref = '', message = '', relativeTime = '', hash = ''] = line.split('\x1f');
      return { ref, message, relativeTime, hash };
    })
    .filter((entry) => entry.ref);
}

/** 并发（4 路）统计每个 stash 涉及的文件数（stash show --name-only 行数）；单个失败计 0。 */
export async function countStashFiles(directory, refs = []) {
  const { git } = await createRepositoryGitContext(directory);
  const uniqueRefs = Array.from(new Set((Array.isArray(refs) ? refs : []).map((ref) => String(ref || '').trim()).filter(Boolean)));
  const counts = {};
  const concurrency = 4;
  let cursor = 0;

  const worker = async () => {
    while (cursor < uniqueRefs.length) {
      const ref = uniqueRefs[cursor++];
      if (!ref) continue;
      try {
        const names = await git.raw(['stash', 'show', '--name-only', ref]);
        counts[ref] = String(names || '').split('\n').map((line) => line.trim()).filter(Boolean).length;
      } catch {
        counts[ref] = 0;
      }
    }
  };

  await Promise.all(Array.from({ length: Math.min(concurrency, uniqueRefs.length) }, () => worker()));
  return counts;
}
/**
 * 创建 stash（--include-untracked），message 缺省为带时间戳的默认值；
 * 无本地改动时 created=false。
 */
export async function stashPush(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);
  const message = typeof options.message === 'string' && options.message.trim()
    ? options.message.trim()
    : `OMPChamber stash ${new Date().toISOString()}`;
  const output = await git.raw(['stash', 'push', '--include-untracked', '-m', message]);
  return {
    success: true,
    created: !/no local changes/i.test(String(output || '')),
    message,
    output: String(output || '').trim(),
  };
}

/** 应用指定 stash：优先 --index 忠实恢复暂存/未暂存划分，失败回退普通 apply；默认 stash@{0}。 */
export async function stashApply(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);
  const ref = typeof options.ref === 'string' && options.ref.trim() ? options.ref.trim() : 'stash@{0}';
  // Prefer --index so the staged/unstaged split captured in the stash is restored
  // faithfully. Fall back to a plain apply when the index can't be reinstated
  // cleanly (e.g. conflicts), which is the prior behavior.
  await git.raw(['stash', 'apply', '--index', ref]).catch(async () => {
    await git.raw(['stash', 'apply', ref]);
  });
  return { success: true, ref };
}

/** 删除指定 stash（默认 stash@{0}）。 */
export async function stashDrop(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);
  const ref = typeof options.ref === 'string' && options.ref.trim() ? options.ref.trim() : 'stash@{0}';
  await git.raw(['stash', 'drop', ref]);
  return { success: true, ref };
}

/** 应用并删除指定 stash（等价 stash apply + stash drop；默认 stash@{0}）。 */
export async function stashPop(directory, options = {}) {
  const ref = typeof options.ref === 'string' && options.ref.trim() ? options.ref.trim() : 'stash@{0}';
  await stashApply(directory, { ref });
  await stashDrop(directory, { ref });
  return { success: true, ref };
}

/**
 * 推送（含多层 upstream 兜底）：无 remote/branch 时先裸 push，报缺 upstream
 * 则自动选 origin（或第一个 remote）+ 当前分支带 --set-upstream 重试；
 * 指定 remote 未指定 branch 时，当前分支无跟踪则直接带 --set-upstream 发布；
 * 普通路径失败若疑似缺 upstream 也会补 --set-upstream 再试一次。
 * 错误消息优先取嵌套 git 错误的 message/stderr。
 */
export async function push(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  const describePushError = (error) => {
    const fromNestedGit = error?.git && typeof error.git === 'object'
      ? [error.git.message, error.git.stderr, error.git.stdout]
      : [];
    const candidates = [
      error?.message,
      error?.stderr,
      error?.stdout,
      ...fromNestedGit,
    ]
      .map((value) => String(value || '').trim())
      .filter(Boolean);

    return candidates[0] || 'Failed to push to remote';
  };

  const buildUpstreamOptions = (raw) => {
    if (Array.isArray(raw)) {
      return raw.includes('--set-upstream') ? raw : [...raw, '--set-upstream'];
    }

    if (raw && typeof raw === 'object') {
      return { ...raw, '--set-upstream': null };
    }

    return ['--set-upstream'];
  };

  const looksLikeMissingUpstream = (error) => {
    const message = String(error?.message || error?.stderr || '').toLowerCase();
    return (
      message.includes('has no upstream') ||
      message.includes('no upstream') ||
      message.includes('set-upstream') ||
      message.includes('set upstream') ||
      (message.includes('upstream') && message.includes('push') && message.includes('-u'))
    );
  };

  const normalizePushResult = (result) => {
    return {
      success: true,
      pushed: result.pushed,
      repo: result.repo,
      ref: result.ref,
    };
  };

  const remote = String(options.remote || '').trim();

  if (!remote && !options.branch) {
    try {
      await git.push();
      return {
        success: true,
        pushed: [],
        repo: directory,
        ref: null,
      };
    } catch (error) {
      if (!looksLikeMissingUpstream(error)) {
        const message = describePushError(error);
        console.error('Failed to push:', error);
        throw new Error(message);
      }

      try {
        const status = await git.status();
        const branch = status.current;
        const remotes = await git.getRemotes(true);
        const fallbackRemote = remotes.find((entry) => entry.name === 'origin')?.name || remotes[0]?.name;
        if (!branch || !fallbackRemote) {
          const message = describePushError(error);
          throw new Error(message);
        }

        const result = await git.push(fallbackRemote, branch, buildUpstreamOptions(options.options));
        return normalizePushResult(result);
      } catch (fallbackError) {
        const message = describePushError(fallbackError);
        console.error('Failed to push (including upstream fallback):', fallbackError);
        throw new Error(message);
      }
    }
  }

  const remoteName = remote || 'origin';

  // If caller didn't specify a branch, this is the common "Push"/"Commit & Push" path.
  // When there's no upstream yet (typical for freshly-created worktree branches), publish it on first push.
  if (!options.branch) {
    try {
      const status = await git.status();
      if (status.current && !status.tracking) {
        const result = await git.push(remoteName, status.current, buildUpstreamOptions(options.options));
        return normalizePushResult(result);
      }
    } catch (error) {
      // If we can't read status, fall back to the regular push path below.
      console.warn('Failed to read git status before push:', error);
    }
  }

  try {
    const result = await git.push(remoteName, options.branch, options.options || {});
    return normalizePushResult(result);
  } catch (error) {
    // Last-resort fallback: retry with upstream if the error suggests it's missing.
    if (!looksLikeMissingUpstream(error)) {
      const message = describePushError(error);
      console.error('Failed to push:', error);
      throw new Error(message);
    }

    try {
      const status = await git.status();
      const branch = options.branch || status.current;
      if (!branch) {
        console.error('Failed to push: missing branch name for upstream setup:', error);
        throw error;
      }

      const result = await git.push(remoteName, branch, buildUpstreamOptions(options.options));
      return normalizePushResult(result);
    } catch (fallbackError) {
      const message = describePushError(fallbackError);
      console.error('Failed to push (including upstream fallback):', fallbackError);
      throw new Error(message);
    }
  }
}

/** 删除远端分支：向 remote（默认 origin）push 空 ref（:branch）；分支名剥掉 refs/heads/ 前缀。 */
export async function deleteRemoteBranch(directory, options = {}) {
  const { branch, remote } = options;
  if (!branch) {
    throw new Error('branch is required to delete remote branch');
  }

  const { git } = await createRepositoryGitContext(directory);
  const targetBranch = branch.startsWith('refs/heads/')
    ? branch.substring('refs/heads/'.length)
    : branch;
  const remoteName = remote || 'origin';

  try {
    await git.push(remoteName, `:${targetBranch}`);
    return { success: true };
  } catch (error) {
    console.error('Failed to delete remote branch:', error);
    throw error;
  }
}

/**
 * 拉取远端对象：仅指定 remote 时用 raw 命令保留 `git fetch &lt;remote&gt;` 语义
 * （simple-git 会丢掉无 branch 的 remote 参数）；否则走 simple-git.fetch。
 */
export async function fetch(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const remote = String(options.remote || '').trim();
    const branch = String(options.branch || '').trim();
    const fetchOptions = options.options || {};

    if (remote && !branch) {
      // simple-git drops the remote when branch is omitted, so use raw to preserve `git fetch <remote>`.
      await git.raw(['fetch', ...buildRawGitOptions(fetchOptions), remote]);
    } else {
      await git.fetch(
        remote || 'origin',
        branch || undefined,
        fetchOptions
      );
    }

    return { success: true };
  } catch (error) {
    console.error('Failed to fetch:', error);
    throw error;
  }
}

/** 暂存单个文件（stageFiles 的单文件便捷封装）。 */
export async function stageFile(directory, filePath) {
  await stageFiles(directory, [filePath]);
}

/**
 * 批量暂存：路径归一、穿越校验、解析为仓库相对路径后在 index 变更队列内
 * `git add`。整批 pathspec 失配时逐路径重试并跳过已达目标状态的路径
 * （快速连续勾选时 UI 可能请求暂存刚被上一波操作处理完的文件，如已删除文件）。
 */
export async function stageFiles(directory, paths) {
  if (!directory) {
    throw new Error('directory and path are required for stageFile');
  }

  const filePaths = normalizeFilePathList(paths);
  if (filePaths.length === 0) {
    throw new Error('directory and path are required for stageFile');
  }
  validateRepositoryFilePaths(normalizeDirectoryPath(directory), filePaths);

  await withGitIndexMutationQueue(directory, async () => {
    const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
    const repoPaths = Array.from(new Set(await Promise.all(filePaths.map(async (filePath) => {
      const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
      return fileContext.repoPath;
    }))));
    validateRepositoryFilePaths(repoRoot, repoPaths);
    await git.raw(['add', '--', ...repoPaths]).catch(async (error) => {
      const gitErrorText = parseGitErrorText(error);
      const isPathspecError = gitErrorText.includes('pathspec') && gitErrorText.includes('did not match any files');
      if (!isPathspecError) {
        throw error;
      }

      // During rapid stage/unstage toggling the optimistic UI can request staging a
      // path that a prior queued mutation already staged (most visibly a deletion,
      // whose file is gone from the working tree). `git add` aborts the whole batch
      // on a single unmatched pathspec, so retry per-path and skip the ones already
      // in their target state rather than failing the entire "stage all".
      for (const repoPath of repoPaths) {
        await git.raw(['add', '--', repoPath]).catch((perPathError) => {
          const perPathText = parseGitErrorText(perPathError);
          const perPathIsPathspecError =
            perPathText.includes('pathspec') && perPathText.includes('did not match any files');
          if (!perPathIsPathspecError) {
            throw perPathError;
          }
        });
      }
    });
  });
}

/** 取消暂存单个文件（unstageFiles 的单文件便捷封装）。 */
export async function unstageFile(directory, filePath) {
  await unstageFiles(directory, [filePath]);
}

/**
 * 批量取消暂存：路径归一/校验后 `git restore --staged`（老 git 回退 reset HEAD），
 * 在 index 变更队列内串行执行。
 */
export async function unstageFiles(directory, paths) {
  if (!directory) {
    throw new Error('directory and path are required for unstageFile');
  }

  const filePaths = normalizeFilePathList(paths);
  if (filePaths.length === 0) {
    throw new Error('directory and path are required for unstageFile');
  }
  validateRepositoryFilePaths(normalizeDirectoryPath(directory), filePaths);

  await withGitIndexMutationQueue(directory, async () => {
    const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
    const repoPaths = Array.from(new Set(await Promise.all(filePaths.map(async (filePath) => {
      const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
      return fileContext.repoPath;
    }))));
    validateRepositoryFilePaths(repoRoot, repoPaths);
    await git.raw(['restore', '--staged', '--', ...repoPaths]).catch(async () => {
      await git.raw(['reset', 'HEAD', '--', ...repoPaths]);
    });
  });
}

/**
 * 提交（在 index 变更队列内串行）：addAll 全量提交；或按 files 选择性提交——
 * 给了 stageFiles 时仅提交选中文件（临时 unstage 其它已暂存文件、提交后恢复）；
 * 已全暂存的文件跳过 add；pathspec 失配（选择已过期）时回退为提交当前暂存区。
 */
export async function commit(directory, message, options = {}) {
  return withGitIndexMutationQueue(directory, async () => {
    const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);
    let temporarilyUnstagedFiles = [];

    try {
      const requestedFiles = Array.isArray(options.files)
        ? options.files
          .map((value) => String(value || '').trim())
          .filter(Boolean)
        : [];
      const requestedStageFiles = Array.isArray(options.stageFiles)
        ? options.stageFiles
          .map((value) => String(value || '').trim())
          .filter(Boolean)
        : null;
      let filesToCommit = [];
      let commitFromIndexOnly = false;

      if (options.addAll) {
        await git.add('.');
      } else if (requestedFiles.length > 0) {
        filesToCommit = Array.from(new Set(await Promise.all(requestedFiles.map(async (filePath) => {
          const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
          return fileContext.repoPath;
        }))));

        const stageFilesToCommit = requestedStageFiles
          ? Array.from(new Set(await Promise.all(requestedStageFiles.map(async (filePath) => {
            const fileContext = await resolveGitFileContext(directoryPath, directoryGit, filePath, repoRoot);
            return fileContext.repoPath;
          }))))
          : null;

        const status = await git.status();
        const fileStatusByPath = new Map(status.files.map((file) => [file.path, file]));
      filesToCommit = filesToCommit.filter((filePath) => fileStatusByPath.has(filePath));

        if (filesToCommit.length === 0) {
          throw new Error('No selected files are available to commit. Refresh git status and try again.');
        }

        if (requestedStageFiles) {
          commitFromIndexOnly = true;
          const selectedFileSet = new Set(filesToCommit);
          temporarilyUnstagedFiles = status.files
            .filter((file) => {
              const indexStatus = (file.index || '').trim();
              return indexStatus && indexStatus !== '?' && !selectedFileSet.has(file.path);
            })
            .map((file) => file.path);

          if (temporarilyUnstagedFiles.length > 0) {
            await git.raw(['restore', '--staged', '--', ...temporarilyUnstagedFiles]);
          }
        }

        const filesNeedingAdd = requestedStageFiles
          ? (stageFilesToCommit || []).filter((filePath) => fileStatusByPath.has(filePath))
          : filesToCommit.filter((filePath) => {
            const fileStatus = fileStatusByPath.get(filePath);
            if (!fileStatus) {
              return false;
            }

            const alreadyFullyStaged = fileStatus.index !== ' ' && fileStatus.working_dir === ' ';
            return !alreadyFullyStaged;
          });

        if (filesNeedingAdd.length > 0) {
          await git.raw(['add', '--', ...filesNeedingAdd]);
        }
      }

      const commitArgs =
        !commitFromIndexOnly && !options.addAll && filesToCommit.length > 0
          ? filesToCommit
          : undefined;

      let result;
      try {
        result = await git.commit(message, commitArgs);
      } catch (error) {
        const gitErrorText = parseGitErrorText(error);
        const isPathspecError = gitErrorText.includes('pathspec') && gitErrorText.includes('did not match any files');
        if (!isPathspecError || !commitArgs || commitArgs.length === 0) {
          throw error;
        }

        // Fallback for deleted/stale selections: commit currently staged changes.
        result = await git.commit(message);
      }

      if (temporarilyUnstagedFiles.length > 0) {
        await git.raw(['add', '--', ...temporarilyUnstagedFiles]).catch((restoreError) => {
          console.error('Failed to restore temporarily unstaged files:', restoreError);
        });
      }

      return {
        success: true,
        commit: result.commit,
        branch: result.branch,
        summary: result.summary
      };
    } catch (error) {
      if (temporarilyUnstagedFiles.length > 0) {
        await git.raw(['add', '--', ...temporarilyUnstagedFiles]).catch((restoreError) => {
          console.error('Failed to restore temporarily unstaged files after commit failure:', restoreError);
        });
      }
      console.error('Failed to commit:', error);
      throw error;
    }
  });
}

/**
 * 列出分支：本地全量 + 经 filterActiveRemoteBranches 过滤/补充的远端分支，
 * 附带每个 remote 的默认分支表；current 为当前分支。
 */
export async function getBranches(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const result = await git.branch();

    const allBranches = result.all;
    const remoteBranches = allBranches.filter(branch => branch.startsWith('remotes/'));
    const activeRemoteBranches = await filterActiveRemoteBranches(git, remoteBranches);
    const defaultBranches = await getRemoteDefaultBranches(git);

    const filteredAll = [
      ...allBranches.filter(branch => !branch.startsWith('remotes/')),
      ...activeRemoteBranches
    ];

    return {
      all: filteredAll,
      current: result.current,
      branches: result.branches,
      defaultBranches,
    };
  } catch (error) {
    console.error('Failed to get branches:', error);
    throw error;
  }
}

/**
 * Counts locally unpushed commits for a small caller-supplied set of local
 * branches. This deliberately reads only local refs: the branch picker calls
 * it when opened, never polls, and never fetches a remote behind the user's
 * back. Unknown, remote, and upstream-less branches are omitted.
 */
/**
 * （中文补充）统计少量本地分支的未推送提交数：只读本地与 upstream ref，
 * 不主动 fetch（详见上方英文说明）；无 upstream 或未知分支不出现在结果里。
 */
export async function getUnpushedBranchCounts(directory, branchNames) {
  const { git } = await createRepositoryGitContext(directory);
  const requested = [...new Set(Array.isArray(branchNames) ? branchNames : [])]
    .filter((name) => typeof name === 'string' && name.length > 0)
    .slice(0, 5);
  if (requested.length === 0) return { counts: {} };

  const local = new Set((await git.branchLocal()).all);
  const counts = {};
  await Promise.all(requested.map(async (branch) => {
    if (!local.has(branch)) return;
    const upstream = await git.raw(['rev-parse', '--abbrev-ref', '--symbolic-full-name', `${branch}@{upstream}`])
      .then((value) => value.trim())
      .catch(() => '');
    if (!upstream) return;
    const count = await git.raw(['rev-list', '--count', `${upstream}..${branch}`])
      .then((value) => Number.parseInt(value.trim(), 10))
      .catch(() => 0);
    if (Number.isFinite(count) && count > 0) counts[branch] = count;
  }));
  return { counts };
}

/**
 * 解析每个 remote 的默认分支：优先读 refs/remotes/&lt;remote&gt;/HEAD 符号引用；
 * 缺失的 remote 再用 `ls-remote --symref &lt;remote&gt; HEAD` 询问
 * （不可达则留空，不猜测 main/master）。
 */
async function getRemoteDefaultBranches(git) {
  let defaults = {};

  try {
    const refs = await git.raw([
      'for-each-ref',
      '--format=%(refname) %(symref)',
      'refs/remotes',
    ]);
    defaults = Object.fromEntries(
      refs.trim().split('\n').flatMap((line) => {
        const [ref, symbolicRef] = line.split(' ');
        const match = ref.match(/^refs\/remotes\/([^/]+)\/HEAD$/);
        const prefix = match ? `refs/remotes/${match[1]}/` : '';
        return match && typeof symbolicRef === 'string' && symbolicRef.startsWith(prefix)
          ? [[match[1], symbolicRef.slice(prefix.length)]]
          : [];
      })
    );
  } catch {
    defaults = {};
  }

  // `remote/HEAD` is written by clone and by `git remote set-head`; a remote
  // added by hand may never have one. Without this the caller falls back to
  // guessing main/master/develop, which is exactly the guess this data exists
  // to replace — so ask the remote itself, but only for the remotes that are
  // actually missing an answer.
  try {
    const remotes = await git.getRemotes();
    const missing = remotes.filter((remote) => remote?.name && !defaults[remote.name]);
    if (missing.length === 0) return defaults;

    const resolved = await Promise.all(missing.map(async (remote) => {
      try {
        const output = await git.raw(['ls-remote', '--symref', remote.name, 'HEAD']);
        const match = String(output || '').match(/^ref:\s+refs\/heads\/(.+?)\s+HEAD$/m);
        return match ? [remote.name, match[1]] : null;
      } catch {
        // Unreachable or refusing: no answer is better than a guessed one.
        return null;
      }
    }));

    for (const entry of resolved) {
      if (entry) defaults[entry[0]] = entry[1];
    }
  } catch {
    // Remote list unavailable; the local symrefs are still valid.
  }

  return defaults;
}

/**
 * 过滤仍活跃的远端分支：ls-remote 逐个 remote 查询实际存在的分支；
 * 不可达 remote 的本地跟踪分支全保留（离线不该显示成分支消失）；
 * 同时补上远端已有但本地从未 fetch 的分支（#2098）。整体失败退回原列表。
 */
async function filterActiveRemoteBranches(git, remoteBranches) {
  try {
    const remotes = await git.getRemotes();
    const branchesByRemote = new Map();

    // A remote that did not answer says nothing about its branches. Dropping
    // them would turn "we could not ask" into "these branches are gone", and
    // callers use this list to decide whether a base branch exists at all — so
    // offline would silently remove comparisons that work perfectly well
    // against the local remote-tracking refs.
    const unreachableRemotes = new Set();

    await Promise.all(remotes.map(async (remote) => {
      try {
        const lsRemoteResult = await git.raw(['ls-remote', '--heads', remote.name]);
        const actualRemoteBranches = new Set();
        const lines = lsRemoteResult.trim().split('\n');
        for (const line of lines) {
          if (line.includes('\trefs/heads/')) {
            const branchName = line.split('\t')[1].replace('refs/heads/', '');
            actualRemoteBranches.add(branchName);
          }
        }
        branchesByRemote.set(remote.name, actualRemoteBranches);
      } catch {
        unreachableRemotes.add(remote.name);
      }
    }));

    const activeBranches = remoteBranches.filter(remoteBranch => {
      const match = remoteBranch.match(/^remotes\/[^\/]+\/(.+)$/);
      if (!match) return false;
      const remoteName = remoteBranch.split('/')[1];
      const branchName = match[1];
      if (unreachableRemotes.has(remoteName)) return true;
      return branchesByRemote.get(remoteName)?.has(branchName) ?? false;
    });

    // A branch pushed to the remote that was never fetched locally has no
    // remote-tracking ref, so `git branch` never reports it — but ls-remote
    // just told us it exists. Add those so a freshly pushed branch shows up
    // without requiring a fetch first (#2098). Unreachable remotes have no
    // ls-remote data and therefore add nothing here; their local view above
    // is preserved unchanged.
    const seenBranches = new Set(activeBranches);
    for (const [remoteName, actualRemoteBranches] of branchesByRemote) {
      for (const branchName of actualRemoteBranches) {
        const qualifiedBranch = `remotes/${remoteName}/${branchName}`;
        if (!seenBranches.has(qualifiedBranch)) {
          seenBranches.add(qualifiedBranch);
          activeBranches.push(qualifiedBranch);
        }
      }
    }

    return activeBranches;
  } catch (error) {
    console.warn('Failed to filter active remote branches, returning all:', error.message);
    return remoteBranches;
  }
}

/** 从 startPoint（默认 HEAD）创建并切换到新分支。 */
export async function createBranch(directory, branchName, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    await git.checkoutBranch(branchName, options.startPoint || 'HEAD');
    return { success: true, branch: branchName };
  } catch (error) {
    console.error('Failed to create branch:', error);
    throw error;
  }
}

// Deliberately not `--quiet`: simple-git resolves a quiet non-zero exit as
// success, so the ref itself has to be echoed for the answer to mean anything.
/** 判断 ref 是否存在：show-ref --verify 有输出即存在（刻意不带 --quiet，见上方英文说明）。 */
const gitRefExists = async (git, ref) => {
  try {
    const output = await git.raw(['show-ref', '--verify', ref]);
    return String(output).trim().length > 0;
  } catch {
    return false;
  }
};

/**
 * The branch selector lists remote-tracking branches beside local ones, so
 * picking `origin/main` means "work on main", not "detach HEAD at the remote's
 * commit" — which is what a literal checkout of a remote-tracking ref does.
 * Resolve such a pick to the local branch, creating it with tracking when it
 * does not exist yet. Anything we cannot resolve is checked out as requested,
 * leaving git's own DWIM behavior intact.
 */
/**
 * （中文补充）见上方英文说明：把远端跟踪分支的选择解析为"检出本地同名分支"，
 * 本地缺 ref 时先 fetch 单个分支再建跟踪；无法解析时原样返回交由 git DWIM。
 */
const resolveBranchCheckoutTarget = async (git, branchName) => {
  const requested = String(branchName || '').trim();
  if (!requested) {
    throw new Error('Branch name is required');
  }

  const asRequested = { branch: requested, remoteRef: null };

  if (await gitRefExists(git, `refs/heads/${requested}`)) {
    return asRequested;
  }

  const remoteRef = requested.replace(/^remotes\//, '');
  const remotes = await git.getRemotes();
  const remote = remotes.find((entry) => entry?.name && remoteRef.startsWith(`${entry.name}/`));
  if (!remote) {
    return asRequested;
  }

  const localBranch = remoteRef.slice(remote.name.length + 1);
  // `origin/HEAD` names no branch of its own; it is a pointer to one.
  if (!localBranch || localBranch === 'HEAD') {
    return asRequested;
  }

  // The branch list also carries branches that only `ls-remote` knows about
  // (#2098): they exist on the remote but were never fetched, so there is no
  // remote-tracking ref and a literal checkout fails with a pathspec error.
  // Fetch the single branch first so the tracking ref exists, then fall through
  // to the normal create-with-tracking path.
  if (!(await gitRefExists(git, `refs/remotes/${remoteRef}`))) {
    try {
      await git.fetch(remote.name, localBranch);
    } catch (error) {
      throw new Error(`Failed to fetch ${localBranch} from ${remote.name}: ${error?.message || error}`);
    }
    if (!(await gitRefExists(git, `refs/remotes/${remoteRef}`))) {
      throw new Error(`Branch ${localBranch} no longer exists on remote ${remote.name}`);
    }
  }

  const localExists = await gitRefExists(git, `refs/heads/${localBranch}`);
  return { branch: localBranch, remoteRef: localExists ? null : remoteRef };
};

/**
 * 切换分支：经 resolveBranchCheckoutTarget 解析目标（选中远端跟踪分支时建同名
 * 本地分支并 --track），再 checkout。返回实际检出的分支名。
 */
export async function checkoutBranch(directory, branchName) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const target = await resolveBranchCheckoutTarget(git, branchName);
    if (target.remoteRef) {
      await git.raw(['checkout', '-b', target.branch, '--track', target.remoteRef]);
    } else {
      await git.checkout(target.branch);
    }
    return { success: true, branch: target.branch };
  } catch (error) {
    console.error('Failed to checkout branch:', error);
    throw error;
  }
}

/** 以 detached HEAD 检出指定 commit（hash 先经十六进制校验）。 */
export async function checkoutCommit(directory, hash) {
  if (!isValidCommitHash(hash)) {
    throw new Error('Invalid commit hash');
  }
  const { git } = await createRepositoryGitContext(directory);
  try {
    await git.checkout(hash);
    return { success: true };
  } catch (error) {
    console.error('Failed to checkout commit:', error);
    throw error;
  }
}

/**
 * cherry-pick 指定提交；冲突按错误文本识别（conflict / patch does not apply），
 * 返回 conflict 与冲突文件列表，其余错误抛出。
 */
export async function cherryPick(directory, hash) {
  if (!isValidCommitHash(hash)) {
    throw new Error('Invalid commit hash');
  }
  const { git } = await createRepositoryGitContext(directory);
  try {
    await git.raw(['cherry-pick', hash]);
    return { success: true, conflict: false };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict =
      errorMessage.includes('conflict') ||
      errorMessage.includes('patch does not apply');

    if (isConflict) {
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || [],
      };
    }

    console.error('Failed to cherry-pick:', error);
    throw error;
  }
}

/**
 * 反做某个提交（git revert --no-commit，改动进暂存区但不自动提交）；
 * 冲突按错误文本识别，返回 conflict 与冲突文件列表。
 */
export async function revertCommit(directory, hash) {
  if (!isValidCommitHash(hash)) {
    throw new Error('Invalid commit hash');
  }
  const { git } = await createRepositoryGitContext(directory);
  try {
    await git.raw(['revert', '--no-commit', hash]);
    return { success: true, conflict: false };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict =
      errorMessage.includes('conflict') ||
      errorMessage.includes('revert failed');

    if (isConflict) {
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || [],
      };
    }

    console.error('Failed to revert commit:', error);
    throw error;
  }
}

/**
 * 重置到指定 commit：mode 为 hard 且未显式 force 时，若工作区有未提交改动
 * 先抛错保护（提示 stash/commit 或 force）；随后 `git reset --<mode> <hash>`。
 */
export async function resetToCommit(directory, hash, mode, force = false) {
  if (!isValidCommitHash(hash)) {
    throw new Error('Invalid commit hash');
  }
  const { git } = await createRepositoryGitContext(directory);

  if (mode === 'hard' && !force) {
    const status = await git.status();
    const isDirty = !status.isClean();
    if (isDirty) {
      throw new Error('Cannot hard reset: uncommitted changes in working tree. Stash or commit first, or use force.');
    }
  }

  try {
    await git.raw(['reset', `--${mode}`, hash]);
    return { success: true };
  } catch (error) {
    console.error('Failed to reset to commit:', error);
    throw error;
  }
}

/**
 * 列出仓库全部 worktree（porcelain 解析为 head/name/branch/path）。
 * 目录不存在返回空；非 git 仓库视为无 worktree 返回空数组（可选功能，
 * 不让上层报 500）。
 */
export async function getWorktrees(directory) {
  const directoryPath = normalizeDirectoryPath(directory);
  if (!directoryPath || !fs.existsSync(directoryPath)) {
    return [];
  }
  try {
    const directoryGit = await createGit(directoryPath);
    const repoRoot = await resolveGitRepositoryRoot(directoryPath, directoryGit);
    const result = await runGitCommandOrThrow(
      repoRoot,
      ['worktree', 'list', '--porcelain'],
      'Failed to list git worktrees'
    );
    return parseWorktreePorcelain(result.stdout).map((entry) => ({
      head: entry.head || '',
      name: path.basename(entry.worktree || ''),
      branch: entry.branch || '',
      path: entry.worktree,
    }));
  } catch (error) {
    // Worktrees are an optional feature. When the caller passes a directory
    // that is not inside any git repository (for example, the managed
    // OpenCode's working directory or an unconfigured project path), git
    // exits with "fatal: not a git repository ...". Treat that as an
    // authoritative empty result so the route handler can still respond
    // 200 [] and the desktop main.log stays free of noise.
    if (!isNotGitRepositoryError(error)) {
      console.warn('Failed to list worktrees, returning empty list:', error?.message || error);
    }
    return [];
  }
}

/**
 * 静态校验 worktree 创建请求（无副作用）：existing 模式校验分支可解析；
 * new 模式校验分支名未被占用、起点 ref 可达（远端分支用 ls-remote 确认）；
 * 统一校验分支未被其它 worktree 检出、ensureRemote 配对、upstream 完整性。
 * @returns {{ok: boolean, errors: Array<{code: string, message: string}>, resolved: object}}
 * 校验失败也返回结构化结果而非抛错
 */
export async function validateWorktreeCreate(directory, input = {}) {
  const mode = input?.mode === 'existing' ? 'existing' : 'new';
  const errors = [];

  try {
    const context = await resolveWorktreeProjectContext(directory);
    const preferredBranchName = cleanBranchName(String(input?.branchName || '').trim());
    const startRef = normalizeStartRef(input?.startRef);
    const ensureRemoteName = String(input?.ensureRemoteName || '').trim();
    const ensureRemoteUrl = String(input?.ensureRemoteUrl || '').trim();

    let localBranch = '';
    let inferredUpstream = null;

    if (mode === 'existing') {
      try {
        const resolved = await resolveExistingWorktreeSource(context.primaryWorktree, input, 'validate');
        localBranch = resolved.localBranch || '';
        if (resolved.upstream) {
          inferredUpstream = {
            remote: resolved.upstream.remote,
            branch: resolved.upstream.branch,
          };
        }
      } catch (error) {
        errors.push({
          code: 'branch_not_found',
          message: error instanceof Error ? error.message : 'Existing branch not found',
        });
      }
    } else {
      if (preferredBranchName) {
        const exists = await runGitCommand(context.primaryWorktree, ['show-ref', '--verify', '--quiet', `refs/heads/${preferredBranchName}`]);
        if (exists.success) {
          errors.push({
            code: 'branch_exists',
            message: `Branch already exists: ${preferredBranchName}`,
          });
        }
        localBranch = preferredBranchName;
      }

      const parsedRemoteRef = await resolveRemoteBranchRef(context.primaryWorktree, startRef);
      if (startRef && startRef !== 'HEAD') {
        if (parsedRemoteRef && ensureRemoteName && ensureRemoteUrl && ensureRemoteName === parsedRemoteRef.remote) {
          const remoteCheck = await checkRemoteBranchExists(
            context.primaryWorktree,
            parsedRemoteRef.remote,
            parsedRemoteRef.branch,
            ensureRemoteUrl
          );
          if (!remoteCheck.success) {
            errors.push({
              code: 'remote_unreachable',
              message: `Unable to query remote ${ensureRemoteName}`,
            });
          } else if (!remoteCheck.found) {
            errors.push({
              code: 'start_ref_not_found',
              message: `Remote branch not found: ${parsedRemoteRef.remoteRef}`,
            });
          }
        } else if (parsedRemoteRef) {
          const remoteCheck = await checkRemoteBranchExists(
            context.primaryWorktree,
            parsedRemoteRef.remote,
            parsedRemoteRef.branch
          );
          if (!remoteCheck.success) {
            errors.push({
              code: 'remote_unreachable',
              message: `Unable to query remote ${parsedRemoteRef.remote}`,
            });
          } else if (!remoteCheck.found) {
            errors.push({
              code: 'start_ref_not_found',
              message: `Remote branch not found: ${parsedRemoteRef.remoteRef}`,
            });
          }
        } else {
          const startRefExists = await runGitCommand(context.primaryWorktree, ['rev-parse', '--verify', '--quiet', startRef]);
          if (!startRefExists.success) {
            errors.push({
              code: 'start_ref_not_found',
              message: `Start ref not found: ${startRef}`,
            });
          }
        }
      }

      if (parsedRemoteRef) {
        inferredUpstream = {
          remote: parsedRemoteRef.remote,
          branch: parsedRemoteRef.branch,
        };
      }
    }

    if (localBranch) {
      const inUse = await findBranchInUse(context.primaryWorktree, localBranch);
      if (inUse) {
        errors.push({
          code: 'branch_in_use',
          message: `Branch is already checked out in ${inUse.worktree}`,
        });
      }
    }

    if ((ensureRemoteName && !ensureRemoteUrl) || (!ensureRemoteName && ensureRemoteUrl)) {
      errors.push({
        code: 'invalid_remote_config',
        message: 'Both ensureRemoteName and ensureRemoteUrl are required together',
      });
    }

    const shouldSetUpstream = Boolean(input?.setUpstream);
    if (shouldSetUpstream) {
      const upstreamRemote = String(input?.upstreamRemote || inferredUpstream?.remote || '').trim();
      const upstreamBranch = String(input?.upstreamBranch || inferredUpstream?.branch || '').trim();

      if (!upstreamRemote || !upstreamBranch) {
        errors.push({
          code: 'upstream_incomplete',
          message: 'upstreamRemote and upstreamBranch are required when setUpstream is true',
        });
      } else {
        const remoteExists = await runGitCommand(context.primaryWorktree, ['remote', 'get-url', upstreamRemote]);
        if (!remoteExists.success && (!ensureRemoteName || ensureRemoteName !== upstreamRemote)) {
          errors.push({
            code: 'remote_not_found',
            message: `Remote not found: ${upstreamRemote}`,
          });
        }
      }
    }

    return {
      ok: errors.length === 0,
      errors,
      resolved: {
        mode,
        localBranch: localBranch || null,
      },
    };
  } catch (error) {
    return {
      ok: false,
      errors: [{
        code: 'validation_failed',
        message: error instanceof Error ? error.message : 'Failed to validate worktree creation',
      }],
    };
  }
}

/** 快速创建前的预检：跑 validateWorktreeCreate，不通过则把全部错误拼成一条消息抛出。 */
const assertWorktreeCreatePreflight = async (directory, input = {}) => {
  const validation = await validateWorktreeCreate(directory, input);
  if (validation?.ok) {
    return;
  }

  const message = validation?.errors
    ?.map((error) => error?.message)
    .filter(Boolean)
    .join('\n') || 'Failed to validate worktree creation';
  throw new Error(message);
};

/** 预览将要创建的 worktree：解析名称/分支/目录候选，但不做任何 git 挂接操作（仅确保根目录存在）。 */
export async function previewWorktreeCreate(directory, input = {}) {
  const mode = input?.mode === 'existing' ? 'existing' : 'new';
  const context = await resolveWorktreeProjectContext(directory);
  await fsp.mkdir(context.worktreeRoot, { recursive: true });

  const preferredName = String(input?.worktreeName || input?.name || '').trim();
  const preferredBranchName = cleanBranchName(String(input?.branchName || '').trim());
  const candidate = await resolveCandidateDirectory(
    context.worktreeRoot,
    preferredName,
    mode === 'new' && preferredBranchName ? preferredBranchName : '',
    context.primaryWorktree
  );

  return {
    name: candidate.name,
    branch: mode === 'new' ? candidate.branch : preferredBranchName,
    path: candidate.directory,
  };
}

/**
 * 把 git worktree 真正挂接到候选目录（createWorktree 的核心实现）：
 * 解析 existing/new 模式的分支与检出 ref、校验分支未被其它 worktree 占用、
 * 必要时预置 remote 并 fetch 起点分支、`worktree add --no-checkout` 建骨架，
 * 再排队 bootstrap（填充文件、hook、upstream、启动脚本）。
 * 返回 head/名称/分支/路径与初始 bootstrap 状态。
 */
async function attachGitWorktreeToCandidate(context, candidate, input = {}) {
  const mode = input?.mode === 'existing' ? 'existing' : 'new';
  const startRef = normalizeStartRef(input?.startRef);
  let ensureRemoteName = String(input?.ensureRemoteName || '').trim();
  let ensureRemoteUrl = String(input?.ensureRemoteUrl || '').trim();

  let localBranch = '';
  let inferredUpstream = null;
  let shouldSetUpstream = Boolean(input?.setUpstream);
  const worktreeAddArgs = ['worktree', 'add', '--no-checkout'];

  if (mode === 'existing') {
    const resolved = await resolveExistingWorktreeSource(context.primaryWorktree, input, 'create');
    localBranch = resolved.localBranch;
    shouldSetUpstream = resolved.setUpstream;

    const inUse = await findBranchInUse(context.primaryWorktree, localBranch);
    if (inUse) {
      throw new Error(`Branch is already checked out in ${inUse.worktree}`);
    }

    if (resolved.createLocalBranch) {
      worktreeAddArgs.push('-b', localBranch);
    }
    worktreeAddArgs.push(candidate.directory, resolved.checkoutRef);

    if (resolved.upstream) {
      inferredUpstream = {
        remote: resolved.upstream.remote,
        branch: resolved.upstream.branch,
      };
    }
  } else {
    localBranch = candidate.branch;
    if (!localBranch) {
      throw new Error('Failed to resolve branch name for new worktree');
    }

    const branchExists = await runGitCommand(context.primaryWorktree, ['show-ref', '--verify', '--quiet', `refs/heads/${localBranch}`]);
    if (branchExists.success) {
      throw new Error(`Branch already exists: ${localBranch}`);
    }

    const inUse = await findBranchInUse(context.primaryWorktree, localBranch);
    if (inUse) {
      throw new Error(`Branch is already checked out in ${inUse.worktree}`);
    }

    worktreeAddArgs.push('-b', localBranch, candidate.directory);
    if (startRef && startRef !== 'HEAD') {
      worktreeAddArgs.push(startRef);
    }

    const parsedRemoteStartRef = await resolveRemoteBranchRef(context.primaryWorktree, startRef);
    if (parsedRemoteStartRef) {
      inferredUpstream = {
        remote: parsedRemoteStartRef.remote,
        branch: parsedRemoteStartRef.branch,
      };
    }
  }

  if (ensureRemoteName && ensureRemoteUrl) {
    await ensureRemoteWithUrl(context.primaryWorktree, ensureRemoteName, ensureRemoteUrl);
  }

  if (mode === 'new') {
    const parsedRemoteStartRef = await resolveRemoteBranchRef(context.primaryWorktree, startRef);
    if (parsedRemoteStartRef) {
      await fetchRemoteBranchRef(context.primaryWorktree, parsedRemoteStartRef.remote, parsedRemoteStartRef.branch);
    }
  }

  await runGitCommandOrThrow(context.primaryWorktree, worktreeAddArgs, 'Failed to create git worktree');

  const upstreamRemote = shouldSetUpstream
    ? String(inferredUpstream?.remote || input?.upstreamRemote || '').trim()
    : '';
  const upstreamBranch = shouldSetUpstream
    ? String(inferredUpstream?.branch || input?.upstreamBranch || '').trim()
    : '';

  const bootstrapStatus = setWorktreeBootstrapState(
    candidate.directory,
    WORKTREE_BOOTSTRAP_PENDING,
    WORKTREE_BOOTSTRAP_PHASE_DIRECTORY_CREATED
  );

  queueWorktreeBootstrap({
    directory: candidate.directory,
    projectID: context.projectID,
    primaryWorktree: context.primaryWorktree,
    localBranch,
    setUpstream: shouldSetUpstream,
    upstreamRemote,
    upstreamBranch,
    ensureRemoteName,
    ensureRemoteUrl,
    startCommand: input?.startCommand,
  });

  const headResult = await runGitCommand(candidate.directory, ['rev-parse', 'HEAD']);
  const head = String(headResult.stdout || '').trim();

  return {
    head,
    name: candidate.name,
    branch: localBranch,
    path: candidate.directory,
    directoryCreated: true,
    bootstrapStatus,
  };
}

/**
 * 创建 worktree（对 UI 暴露的主入口）。returnAfterDirectoryCreated=true 时走
 * "快速返回"路径：先跑校验、建目录并登记 pending 状态，git 挂接在后台进行
 * （失败标记 failed 并清理空目录）；否则同步执行完整挂接流程。
 */
export async function createWorktree(directory, input = {}) {
  const mode = input?.mode === 'existing' ? 'existing' : 'new';
  const context = await resolveWorktreeProjectContext(directory);

  if (input?.returnAfterDirectoryCreated === true) {
    await assertWorktreeCreatePreflight(directory, input);
  }

  await fsp.mkdir(context.worktreeRoot, { recursive: true });

  const preferredName = String(input?.worktreeName || input?.name || '').trim();
  const preferredBranchName = cleanBranchName(String(input?.branchName || '').trim());

  const candidate = await resolveCandidateDirectory(
    context.worktreeRoot,
    preferredName,
    mode === 'new' && preferredBranchName ? preferredBranchName : '',
    context.primaryWorktree
  );

  if (input?.returnAfterDirectoryCreated === true) {
    await fsp.mkdir(candidate.directory, { recursive: false });

    const bootstrapStatus = setWorktreeBootstrapState(
      candidate.directory,
      WORKTREE_BOOTSTRAP_PENDING,
      WORKTREE_BOOTSTRAP_PHASE_DIRECTORY_CREATED
    );
    const localBranch = mode === 'existing'
      ? cleanBranchName(String(input?.branchName || input?.existingBranch || candidate.branch || '').trim())
      : candidate.branch;

    const task = attachGitWorktreeToCandidate(context, candidate, input).catch(async (error) => {
      setWorktreeBootstrapState(
        candidate.directory,
        WORKTREE_BOOTSTRAP_FAILED,
        WORKTREE_BOOTSTRAP_PHASE_DIRECTORY_CREATED,
        error instanceof Error ? error.message : String(error)
      );
      await cleanupFailedFastWorktreeCreate(context, candidate);
      console.warn('Background worktree creation failed:', error instanceof Error ? error.message : String(error));
    });
    trackWorktreeBootstrapTask(candidate.directory, task);

    return {
      head: '',
      name: candidate.name,
      branch: localBranch,
      path: candidate.directory,
      directoryCreated: true,
      bootstrapStatus,
    };
  }

  return attachGitWorktreeToCandidate(context, candidate, input);
}

/**
 * 查询某 worktree 目录的 bootstrap 状态；从未登记过的目录返回
 * ready/setup-ready 默认值（视为无需 bootstrap）。
 */
export async function getWorktreeBootstrapStatus(directory) {
  const key = toBootstrapStateKey(directory);
  if (!key) {
    throw new Error('Worktree directory is required');
  }

  const current = worktreeBootstrapState.get(key);
  if (current) {
    return current;
  }

  return createWorktreeBootstrapState(
    WORKTREE_BOOTSTRAP_READY,
    WORKTREE_BOOTSTRAP_PHASE_SETUP_READY
  );
}

/**
 * 移除 worktree（先等待该目录的后台 bootstrap 任务结束，避免竞态）：
 * 禁止删除主 worktree；是 git 登记的 worktree 则 `worktree remove --force`
 * （可选连带删除本地分支）；已不在登记表中的"孤儿"托管目录（仍在 worktree
 * 根下）直接递归删除；最后清理 bootstrap 状态。
 */
export async function removeWorktree(directory, input = {}) {
  const targetDirectory = normalizeDirectoryPath(input?.directory);
  if (!targetDirectory) {
    throw new Error('Worktree directory is required');
  }

  await waitForActiveWorktreeBootstrap(targetDirectory);

  const context = await resolveWorktreeProjectContext(directory);
  const deleteLocalBranch = input?.deleteLocalBranch === true;

  const targetCanonical = await canonicalPath(targetDirectory);
  const primaryCanonical = await canonicalPath(context.primaryWorktree);
  if (targetCanonical === primaryCanonical) {
    throw new Error('Cannot remove the primary workspace');
  }
  const worktreeRootCanonical = await canonicalPath(context.worktreeRoot);

  const entries = await listWorktreeEntries(context.primaryWorktree);
  const matchedEntry = await (async () => {
    for (const entry of entries) {
      if (!entry?.worktree) {
        continue;
      }
      const entryCanonical = await canonicalPath(entry.worktree);
      if (entryCanonical === targetCanonical) {
        return entry;
      }
    }
    return null;
  })();

  if (!matchedEntry?.worktree) {
    const isManagedOrphan = targetCanonical !== worktreeRootCanonical
      && isInsideOrSameDirectory(worktreeRootCanonical, targetCanonical);

    const targetExists = await checkPathExists(targetDirectory);
    if (targetExists && isManagedOrphan) {
      await fsp.rm(targetDirectory, { recursive: true, force: true });
    }

    clearWorktreeBootstrapState(targetDirectory);

    return true;
  }

  await runGitCommandOrThrow(
    context.primaryWorktree,
    ['worktree', 'remove', '--force', matchedEntry.worktree],
    'Failed to remove git worktree'
  );

  if (deleteLocalBranch) {
    const branchName = cleanBranchName(String(matchedEntry.branchRef || matchedEntry.branch || '').trim());
    if (branchName) {
      await runGitCommandOrThrow(
        context.primaryWorktree,
        ['branch', '-D', branchName],
        `Failed to delete local branch ${branchName}`
      );
    }
  }

  clearWorktreeBootstrapState(matchedEntry.worktree);

  return true;
}

/** 删除本地分支（force=true 用 -D 强删，否则 -d 安全删除）；分支名自动剥掉 refs/heads/ 前缀。 */
export async function deleteBranch(directory, branch, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const branchName = branch.startsWith('refs/heads/')
      ? branch.substring('refs/heads/'.length)
      : branch;
    const args = ['branch', options.force ? '-D' : '-d', branchName];
    await git.raw(args);
    return { success: true };
  } catch (error) {
    console.error('Failed to delete branch:', error);
    throw error;
  }
}

/**
 * Resolve a log base ref using local-first semantics.
 *
 * - If `from` is falsy / whitespace → return undefined.
 * - If the local ref resolves → return it unchanged (caller's intent preserved).
 * - If the local ref is absent but `origin/<from>` exists → return `origin/<from>`
 *   (common when the user has never checked out the base branch locally).
 * - If neither resolves → return `from` unchanged so git surfaces a meaningful error.
 *
 * @param {string | undefined} from   - The raw `from` option value.
 * @param {(ref: string) => Promise<boolean>} checkRef - Returns true when the ref resolves.
 * @returns {Promise<string | undefined>}
 */
/**
 * （中文补充）日志基线 ref 的本地优先解析，规则见上方英文 JSDoc：
 * 空→undefined；本地可解析→原样；仅 origin 存在→origin/&lt;from&gt;；
 * 都不行→原样返回交由 git 报出有意义的错误。
 */
export async function resolveBaseRefForLog(from, checkRef) {
  const normalized = typeof from === 'string' ? from.trim() : undefined;
  if (!normalized) return undefined;

  if (await checkRef(normalized)) return normalized;

  const originRef = `refs/remotes/origin/${normalized}`;
  if (await checkRef(originRef)) return `origin/${normalized}`;

  return normalized;
}

/**
 * 读取提交历史。options.all 时走 --all --topo-order 的自解析路径（含 refs 与
 * shortstat 统计）；否则用 simple-git 的 log（from 经 resolveBaseRefForLog 本地
 * 优先解析）取主体，再用 `git log --shortstat` 补每个提交的文件数/增删行数与
 * 父提交，两路结果按 hash 合并返回 { all, latest, total }。可按单文件过滤。
 */
export async function getLog(directory, options = {}) {
  const { directoryPath, directoryGit, repoRoot, git } = await createRepositoryGitContext(directory);

  try {
    const maxCount = options.maxCount || 50;

    if (options.all) {
      const logArgs = [
        'log',
        `--max-count=${maxCount}`,
        '--all',
        '--topo-order',
        '--date=iso',
        '--pretty=format:%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%s%x1f%D',
        '--shortstat',
      ];

      const rawLog = await git.raw(logArgs);
      const records = rawLog
        .split('\x1e')
        .map((e) => e.trim())
        .filter(Boolean);

      const entries = [];
      for (const record of records) {
        const lines = record.split('\n').filter((l) => l.trim().length > 0);
        const header = lines.shift() || '';
        const [hash, parentsRaw, author_name, author_email, date, message, refsRaw] =
          header.split('\x1f');
        if (!hash) continue;

        const parents = parentsRaw ? parentsRaw.trim().split(' ').filter(Boolean) : [];
        const refs = refsRaw ? refsRaw.trim() : '';

        let filesChanged = 0;
        let insertions = 0;
        let deletions = 0;
        for (const line of lines) {
          const filesMatch = line.match(/(\d+)\s+files?\s+changed/);
          const insertMatch = line.match(/(\d+)\s+insertions?\(\+\)/);
          const deleteMatch = line.match(/(\d+)\s+deletions?\(-\)/);
          if (filesMatch) filesChanged = parseInt(filesMatch[1], 10);
          if (insertMatch) insertions = parseInt(insertMatch[1], 10);
          if (deleteMatch) deletions = parseInt(deleteMatch[1], 10);
        }

        entries.push({
          hash,
          date: date || '',
          message: message || '',
          refs,
          body: '',
          author_name: author_name || '',
          author_email: author_email || '',
          filesChanged,
          insertions,
          deletions,
          parents,
        });
      }

      return { all: entries, latest: entries[0] || null, total: entries.length };
    }

    const filePath = options.file
      ? (await resolveGitFileContext(directoryPath, directoryGit, options.file, repoRoot)).repoPath
      : undefined;

    // Prefer the local ref; fall back to origin/<from> only when the local ref
    // cannot be resolved (e.g. user has never checked out the base branch).
    const checkRef = async (ref) => {
      try {
        const out = await git.raw(['rev-parse', '--verify', ref]);
        return Boolean(out && out.trim());
      } catch {
        return false;
      }
    };
    const resolvedFrom = await resolveBaseRefForLog(options.from, checkRef);

    const baseLog = await git.log({
      maxCount,
      from: resolvedFrom,
      to: options.to,
      file: filePath
    });

    const logArgs = [
      'log',
      `--max-count=${maxCount}`,
      '--date=iso',
      '--pretty=format:%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%s',
      '--shortstat'
    ];

    if (resolvedFrom && options.to) {
      logArgs.push(`${resolvedFrom}..${options.to}`);
    } else if (resolvedFrom) {
      logArgs.push(`${resolvedFrom}..HEAD`);
    } else if (options.to) {
      logArgs.push(options.to);
    }

    if (filePath) {
      logArgs.push('--', filePath);
    }

    const rawLog = await git.raw(logArgs);
    const records = rawLog
      .split('\x1e')
      .map((entry) => entry.trim())
      .filter(Boolean);

    const statsMap = new Map();

    records.forEach((record) => {
      const lines = record.split('\n').filter((line) => line.trim().length > 0);
      const header = lines.shift() || '';
      const [hash, parentsRaw] = header.split('\x1f');
      const parents = parentsRaw ? parentsRaw.trim().split(' ').filter(Boolean) : [];
      if (!hash) {
        return;
      }

      let filesChanged = 0;
      let insertions = 0;
      let deletions = 0;

      lines.forEach((line) => {
        const filesMatch = line.match(/(\d+)\s+files?\s+changed/);
        const insertMatch = line.match(/(\d+)\s+insertions?\(\+\)/);
        const deleteMatch = line.match(/(\d+)\s+deletions?\(-\)/);

        if (filesMatch) {
          filesChanged = parseInt(filesMatch[1], 10);
        }
        if (insertMatch) {
          insertions = parseInt(insertMatch[1], 10);
        }
        if (deleteMatch) {
          deletions = parseInt(deleteMatch[1], 10);
        }
      });

      statsMap.set(hash, { filesChanged, insertions, deletions, parents });
    });

    const merged = baseLog.all.map((entry) => {
      const stats = statsMap.get(entry.hash) || { filesChanged: 0, insertions: 0, deletions: 0, parents: [] };
      return {
        hash: entry.hash,
        date: entry.date,
        message: entry.message,
        refs: entry.refs || '',
        body: entry.body || '',
        author_name: entry.author_name,
        author_email: entry.author_email,
        filesChanged: stats.filesChanged,
        insertions: stats.insertions,
        deletions: stats.deletions,
        parents: stats.parents || [],
      };
    });

    return {
      all: merged,
      latest: merged[0] || null,
      total: baseLog.total
    };
  } catch (error) {
    console.error('Failed to get log:', error);
    throw error;
  }
}

/** 判断目录是否为 linked worktree（--git-dir 与 --git-common-dir 不一致）；出错按否处理。 */
export async function isLinkedWorktree(directory) {
  const git = await createGit(directory);
  try {
    const [gitDir, gitCommonDir] = await Promise.all([
      git.raw(['rev-parse', '--git-dir']).then((output) => output.trim()),
      git.raw(['rev-parse', '--git-common-dir']).then((output) => output.trim())
    ]);
    return gitDir !== gitCommonDir;
  } catch (error) {
    console.error('Failed to determine worktree type:', error);
    return false;
  }
}

/**
 * 校验 worktree 目录与托管根的关系：解析两边的 canonical path，
 * 返回目录是否有效（是 git 仓库）、是否位于 worktreeRoot 之内及解析结果。
 */
export async function validateWorktreeDirectory(directory, worktreeRoot) {
  const directoryPath = normalizeDirectoryPath(directory);
  const rootPath = normalizeDirectoryPath(worktreeRoot);

  if (!directoryPath || !rootPath) {
    return {
      valid: false,
      insideWorktreeRoot: false,
      resolvedWorktreeRoot: null,
      resolvedCwd: null,
    };
  }

  const isRepo = await isGitRepository(directoryPath);
  if (!isRepo) {
    return {
      valid: false,
      insideWorktreeRoot: false,
      resolvedWorktreeRoot: null,
      resolvedCwd: null,
    };
  }

  const resolvedCwd = await canonicalPath(directoryPath);
  const resolvedRoot = await canonicalPath(rootPath);

  const inside = resolvedCwd.startsWith(resolvedRoot + path.sep) || resolvedCwd === resolvedRoot;

  return {
    valid: true,
    insideWorktreeRoot: inside,
    resolvedWorktreeRoot: resolvedRoot,
    resolvedCwd,
  };
}

/**
 * 汇总目录的 worktree 状态快照：托管 worktree 根（解析失败记 invalid）、
 * HEAD 状态（branch / detached / unborn，附分支名或短 hash），以及需要用户
 * 注意的原因（merge/rebase/cherry-pick/revert，通过探测 .git 内部标记文件）。
 * 目录无效或非仓库时返回 not-a-repo 的空状态。
 */
export async function canonicalizeWorktreeState(directory) {
  const directoryPath = normalizeDirectoryPath(directory);

  if (!directoryPath) {
    return {
      worktreeRoot: null,
      cwd: null,
      branch: null,
      headState: 'detached',
      worktreeStatus: 'not-a-repo',
      legacy: false,
      degraded: false,
      attentionReason: null,
    };
  }

  const isRepo = await isGitRepository(directoryPath);
  if (!isRepo) {
    return {
      worktreeRoot: null,
      cwd: null,
      branch: null,
      headState: 'detached',
      worktreeStatus: 'not-a-repo',
      legacy: false,
      degraded: false,
      attentionReason: null,
    };
  }

  const cwd = await canonicalPath(directoryPath);
  const git = await createGit(directoryPath);
  const repoRoot = await resolveGitRepositoryRoot(directoryPath, git).catch(() => directoryPath);

  let worktreeRoot = null;
  let worktreeStatus = 'ready';
  let headState = /** @type {'branch' | 'detached' | 'unborn'} */ ('branch');
  let branch = null;
  let attentionReason = /** @type {'merge' | 'rebase' | 'cherry-pick' | 'revert' | 'bisect' | null} */ (null);

  try {
    const context = await resolveWorktreeProjectContext(directoryPath);
    worktreeRoot = await canonicalPath(context.worktreeRoot);
  } catch {
    worktreeStatus = 'invalid';
  }

  try {
    const symbolicRef = await git.raw(['symbolic-ref', '-q', 'HEAD']).catch(() => '');
    if (symbolicRef.trim()) {
      headState = 'branch';
      branch = cleanBranchName(symbolicRef.trim());
    } else {
      const revParse = await git.raw(['rev-parse', 'HEAD']).catch(() => '');
      if (!revParse.trim()) {
        headState = 'unborn';
        branch = null;
      } else {
        headState = 'detached';
        branch = revParse.trim().slice(0, 7);
      }
    }
  } catch {
    headState = 'unborn';
    branch = null;
  }

  // Detect attention reasons from getStatus side-effects
  try {
    const status = await git.status(['-uall']);
    if (status.current && (await git.raw(['rev-parse', '--verify', 'MERGE_HEAD']).then(() => true).catch(() => false))) {
      attentionReason = 'merge';
    } else {
      const rebaseMergePath = await resolveGitInternalPath(repoRoot, git, 'rebase-merge').catch(() => '');
      const rebaseApplyPath = await resolveGitInternalPath(repoRoot, git, 'rebase-apply').catch(() => '');
      const rebaseMerge = rebaseMergePath ? await fsp.stat(rebaseMergePath).then(() => true).catch(() => false) : false;
      const rebaseApply = rebaseApplyPath ? await fsp.stat(rebaseApplyPath).then(() => true).catch(() => false) : false;
      if (rebaseMerge || rebaseApply) {
        attentionReason = 'rebase';
      } else if (status.conflicted && status.conflicted.length > 0) {
        const cherryPickHeadPath = await resolveGitInternalPath(repoRoot, git, 'CHERRY_PICK_HEAD').catch(() => '');
        const revertHeadPath = await resolveGitInternalPath(repoRoot, git, 'REVERT_HEAD').catch(() => '');
        const cherryPickHead = cherryPickHeadPath ? await fsp.stat(cherryPickHeadPath).then(() => true).catch(() => false) : false;
        const revertHead = revertHeadPath ? await fsp.stat(revertHeadPath).then(() => true).catch(() => false) : false;
        if (cherryPickHead) attentionReason = 'cherry-pick';
        else if (revertHead) attentionReason = 'revert';
      }
    }
  } catch {
    // Status check failed — ignore
  }

  return {
    worktreeRoot,
    cwd,
    branch,
    headState,
    worktreeStatus,
    legacy: false,
    degraded: false,
    attentionReason,
  };
}

/**
 * 列出某 commit 变更的文件：numstat 提供行数（'-' 记 0 并标记 isBinary），
 * name-status 补充精确变更类型（A/M/D/R/C，R/C 行取 tab 后的目标路径）；
 * 含 " => " 的重命名路径整串展示并记为 R。
 */
export async function getCommitFiles(directory, commitHash) {
  const { git } = await createRepositoryGitContext(directory);

  try {

    const numstatRaw = await git.raw([
      'show',
      '--numstat',
      '--format=',
      commitHash
    ]);

    const files = [];
    const lines = numstatRaw.trim().split('\n').filter(Boolean);

    for (const line of lines) {
      const parts = line.split('\t');
      if (parts.length < 3) continue;

      const [insertionsRaw, deletionsRaw, ...pathParts] = parts;
      const filePath = pathParts.join('\t');
      if (!filePath) continue;

      const insertions = insertionsRaw === '-' ? 0 : parseInt(insertionsRaw, 10) || 0;
      const deletions = deletionsRaw === '-' ? 0 : parseInt(deletionsRaw, 10) || 0;
      const isBinary = insertionsRaw === '-' && deletionsRaw === '-';

      let changeType = 'M';
      let displayPath = filePath;

      if (filePath.includes(' => ')) {
        changeType = 'R';

        const match = filePath.match(/(?:\{[^}]*\s=>\s[^}]*\}|.*\s=>\s.*)/);
        if (match) {
          displayPath = filePath;
        }
      }

      files.push({
        path: displayPath,
        insertions,
        deletions,
        isBinary,
        changeType
      });
    }

    const nameStatusRaw = await git.raw([
      'show',
      '--name-status',
      '--format=',
      commitHash
    ]).catch(() => '');

    const statusMap = new Map();
    const statusLines = nameStatusRaw.trim().split('\n').filter(Boolean);
    for (const line of statusLines) {
      const match = line.match(/^([AMDRC])\d*\t(.+)$/);
      if (match) {
        const [, status, pathPart] = match;
        statusMap.set(extractGitStatusPath(status, pathPart), status);
      }
    }

    for (const file of files) {
      const basePath = extractGitNumstatDestinationPath(file.path);

      const status = statusMap.get(basePath) || statusMap.get(file.path);
      if (status) {
        file.changeType = status;
      }
    }

    return { files };
  } catch (error) {
    console.error('Failed to get commit files:', error);
    throw error;
  }
}

/**
 * 重命名本地分支并迁移 upstream 跟踪：先读旧分支的 branch.<name>.remote/merge
 * 配置，`git branch -m` 后把 upstream 指到新名（merge ref 指向旧名时同步替换）；
 * set-upstream 失败则保留无跟踪状态而非写坏配置。
 */
export async function renameBranch(directory, oldName, newName) {
  const { git, repoRoot } = await createRepositoryGitContext(directory);

  try {
    const normalizedOldName = cleanBranchName(String(oldName || '').trim());
    const normalizedNewName = cleanBranchName(String(newName || '').trim());

    const previousRemote = await git
      .raw(['config', '--get', `branch.${normalizedOldName}.remote`])
      .then((value) => String(value || '').trim())
      .catch(() => '');
    const previousMerge = await git
      .raw(['config', '--get', `branch.${normalizedOldName}.merge`])
      .then((value) => String(value || '').trim())
      .catch(() => '');

    // Use git branch -m command to rename the branch
    await git.raw(['branch', '-m', oldName, newName]);

    if (previousRemote && previousMerge && normalizedNewName) {
      const previousMergeBranch = cleanBranchName(previousMerge);
      const nextMergeBranch =
        previousMergeBranch === normalizedOldName
          ? normalizedNewName
          : previousMergeBranch;
      const upstream = normalizeUpstreamTarget(previousRemote, nextMergeBranch);

      if (upstream) {
        try {
          await runGitCommandOrThrow(
            repoRoot,
            ['branch', `--set-upstream-to=${upstream.full}`, normalizedNewName],
            `Failed to set upstream to ${upstream.full}`
          );
        } catch {
          // Leave tracking unset rather than writing config for a missing ref.
        }
      }
    }

    return { success: true, branch: newName };
  } catch (error) {
    console.error('Failed to rename branch:', error);
    throw error;
  }
}

/** 列出全部 remote 的名称与 fetch/push URL；目录不是仓库时返回空数组而非抛错。 */
export async function getRemotes(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const remotes = await git.getRemotes(true);
    
    return remotes.map((remote) => ({
      name: remote.name,
      fetchUrl: remote.refs.fetch,
      pushUrl: remote.refs.push
    }));
  } catch (error) {
    if (isNotGitRepositoryError(error)) {
      return [];
    }
    console.error('Failed to get remotes:', error);
    throw error;
  }
}

/** 删除指定 remote；origin 不允许删除；remote 名缺失抛错。 */
export async function removeRemote(directory, options = {}) {
  const remoteName = String(options.remote || '').trim();
  if (!remoteName) {
    throw new Error('remote is required to remove a remote');
  }
  if (remoteName === 'origin') {
    throw new Error('Cannot remove origin remote');
  }

  const { git } = await createRepositoryGitContext(directory);

  try {
    await git.removeRemote(remoteName);
    return { success: true };
  } catch (error) {
    console.error('Failed to remove remote:', error);
    throw error;
  }
}

/**
 * 把当前分支 rebase 到 onto 指定基点（必填，缺失抛错）；冲突按错误文本识别
 * （conflict / could not apply / merge conflict），返回 conflict 与冲突文件列表。
 */
export async function rebase(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const { onto } = options;
    if (!onto) {
      throw new Error('onto parameter is required for rebase');
    }

    await git.rebase([onto]);

    return {
      success: true,
      conflict: false
    };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict = errorMessage.includes('conflict') || 
                       errorMessage.includes('could not apply') ||
                       errorMessage.includes('merge conflict');

    if (isConflict) {
      // Get list of conflicted files
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || []
      };
    }

    console.error('Failed to rebase:', error);
    throw error;
  }
}

/** 中止进行中的 rebase（git rebase --abort）。 */
export async function abortRebase(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    await git.rebase(['--abort']);
    return { success: true };
  } catch (error) {
    console.error('Failed to abort rebase:', error);
    throw error;
  }
}

/**
 * 合并指定分支（branch 必填）：按错误文本识别冲突（conflict / merge conflict /
 * automatic merge failed），冲突时返回 conflict 与冲突文件列表，其余错误抛出。
 */
export async function merge(directory, options = {}) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    const { branch } = options;
    if (!branch) {
      throw new Error('branch parameter is required for merge');
    }

    await git.merge([branch]);

    return {
      success: true,
      conflict: false
    };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict = errorMessage.includes('conflict') || 
                       errorMessage.includes('merge conflict') ||
                       errorMessage.includes('automatic merge failed');

    if (isConflict) {
      // Get list of conflicted files
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || []
      };
    }

    console.error('Failed to merge:', error);
    throw error;
  }
}

/** 中止进行中的合并（git merge --abort）。 */
export async function abortMerge(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    await git.merge(['--abort']);
    return { success: true };
  } catch (error) {
    console.error('Failed to abort merge:', error);
    throw error;
  }
}

/**
 * 继续 rebase：设置 GIT_EDITOR=true 防止编辑器阻塞（仅作用于本次命令）。
 * 错误分三类处理——冲突返回 conflict 与文件列表；"nothing to commit" 自动
 * --skip 跳过该提交；其余抛出。
 */
export async function continueRebase(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    // Set GIT_EDITOR to prevent editor prompts
    await git.env('GIT_EDITOR', 'true').rebase(['--continue']);
    return { success: true, conflict: false };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict = errorMessage.includes('conflict') || 
                       errorMessage.includes('needs merge') ||
                       errorMessage.includes('unmerged') ||
                       errorMessage.includes('fix conflicts');

    if (isConflict) {
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || []
      };
    }

    // Check for "nothing to commit" which means rebase step is complete
    if (errorMessage.includes('nothing to commit') || errorMessage.includes('no changes')) {
      // Skip this commit and continue
      try {
        await git.env('GIT_EDITOR', 'true').rebase(['--skip']);
        return { success: true, conflict: false };
      } catch {
        // If skip also fails, the rebase may be complete
        return { success: true, conflict: false };
      }
    }

    console.error('Failed to continue rebase:', error);
    throw error;
  }
}

/**
 * 继续合并：仍有未合并文件时返回 conflict 与文件列表；冲突已解完则用
 * --no-edit 提交合并结果；"nothing to commit" 视为合并已完成。
 */
export async function continueMerge(directory) {
  const { git } = await createRepositoryGitContext(directory);

  try {
    // Check if there are still unmerged files
    const status = await git.status();
    if (status.conflicted && status.conflicted.length > 0) {
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted
      };
    }

    // For merge, we commit after resolving conflicts
    // Use --no-edit to use the default merge commit message
    await git.env('GIT_EDITOR', 'true').commit([], { '--no-edit': null });
    return { success: true, conflict: false };
  } catch (error) {
    const errorMessage = String(error?.message || error || '').toLowerCase();
    const isConflict = errorMessage.includes('conflict') || 
                       errorMessage.includes('needs merge') ||
                       errorMessage.includes('unmerged') ||
                       errorMessage.includes('fix conflicts');

    if (isConflict) {
      const status = await git.status().catch(() => ({ conflicted: [] }));
      return {
        success: false,
        conflict: true,
        conflictFiles: status.conflicted || []
      };
    }

    // "nothing to commit" can happen if all conflicts resolved to one side
    if (errorMessage.includes('nothing to commit') || errorMessage.includes('no changes added')) {
      // The merge is effectively complete (all changes already committed or no changes needed)
      return { success: true, conflict: false };
    }

    console.error('Failed to continue merge:', error);
    throw error;
  }
}

/**
 * 采集仓库当前冲突现场：porcelain 状态、未合并文件、完整 diff，
 * 并根据 MERGE_HEAD / REBASE_HEAD 判断操作类型（merge/rebase）及头部信息。
 * 各子命令失败均降级为空字符串，不影响整体返回。
 */
export async function getConflictDetails(directory) {
  const { repoRoot, git } = await createRepositoryGitContext(directory);

  try {
    // Get git status --porcelain
    const statusPorcelain = await git.raw(['status', '--porcelain']).catch(() => '');

    // Get unmerged files
    const unmergedFilesRaw = await git.raw(['diff', '--name-only', '--diff-filter=U']).catch(() => '');
    const unmergedFiles = unmergedFilesRaw
      .split('\n')
      .map((line) => line.trim())
      .filter(Boolean);

    // Get current diff
    const diff = await git.raw(['diff', NO_EXT_DIFF]).catch(() => '');

    // Detect operation type and get head info
    let operation = 'merge';
    let headInfo = '';

    // Check for MERGE_HEAD (merge in progress)
    const mergeHeadExists = await git
      .raw(['rev-parse', '--verify', '--quiet', 'MERGE_HEAD'])
      .then(() => true)
      .catch(() => false);

    if (mergeHeadExists) {
      operation = 'merge';
      const mergeHead = await git.raw(['rev-parse', 'MERGE_HEAD']).catch(() => '');
      const mergeMsgPath = await resolveGitInternalPath(repoRoot, git, 'MERGE_MSG').catch(() => '');
      const mergeMsg = mergeMsgPath ? await fsp.readFile(mergeMsgPath, 'utf8').catch(() => '') : '';
      headInfo = `MERGE_HEAD: ${mergeHead.trim()}\n${mergeMsg}`;
    } else {
      // Check for REBASE_HEAD (rebase in progress)
      const rebaseHeadExists = await git
        .raw(['rev-parse', '--verify', '--quiet', 'REBASE_HEAD'])
        .then(() => true)
        .catch(() => false);

      if (rebaseHeadExists) {
        operation = 'rebase';
        const rebaseHead = await git.raw(['rev-parse', 'REBASE_HEAD']).catch(() => '');
        headInfo = `REBASE_HEAD: ${rebaseHead.trim()}`;
      }
    }

    return {
      statusPorcelain: statusPorcelain.trim(),
      unmergedFiles,
      diff: diff.trim(),
      headInfo: headInfo.trim(),
      operation,
    };
  } catch (error) {
    console.error('Failed to get conflict details:', error);
    throw error;
  }
}

/**
 * 读取某 commit 中单个文件的两侧内容（original = hash^ 版本，modified = hash 版本）。
 * 路径候选依次尝试相对 repoRoot 与相对 directoryPath 的形式，命中以父提交/提交
 * 的 `git show` 成功为准；全部 miss 时用 resolveGitCommitFilePath 兜底定位。
 * @returns {{original: string, modified: string, isBinary: boolean}} 二进制直接短路返回
 * @throws 参数缺失、路径无效或两侧都读不到时抛错
 */
export async function getCommitFileDiff(directory, hash, filePath, isBinary) {
  if (!directory || !hash || !filePath) {
    throw new Error('directory, hash, and path are required for getCommitFileDiff');
  }

  if (isBinary) {
    return { original: '', modified: '', isBinary: true };
  }

  const { directoryPath, repoRoot } = await createRepositoryGitContext(directory);
  const candidates = Array.from(new Set([
    toGitPath(path.relative(repoRoot, path.resolve(repoRoot, filePath))),
    toGitPath(path.relative(repoRoot, path.resolve(directoryPath, filePath))),
  ])).filter((candidate) => candidate && !candidate.startsWith('..') && !path.isAbsolute(candidate));

  let originalResult = null;
  let modifiedResult = null;

  for (const candidate of candidates) {
    const [candidateOriginalResult, candidateModifiedResult] = await Promise.all([
      runGitCommand(repoRoot, ['show', `${hash}^:${candidate}`]),
      runGitCommand(repoRoot, ['show', `${hash}:${candidate}`]),
    ]);

    if (candidateOriginalResult.success || candidateModifiedResult.success) {
      originalResult = candidateOriginalResult;
      modifiedResult = candidateModifiedResult;
      break;
    }
  }

  if (!originalResult || !modifiedResult) {
    const resolvedPath = await resolveGitCommitFilePath(repoRoot, hash, candidates);
    [originalResult, modifiedResult] = await Promise.all([
      runGitCommand(repoRoot, ['show', `${hash}^:${resolvedPath}`]),
      runGitCommand(repoRoot, ['show', `${hash}:${resolvedPath}`]),
    ]);
  }

  const original = originalResult.success ? originalResult.stdout : '';
  const modified = modifiedResult.success ? modifiedResult.stdout : '';

  if (!originalResult.success && !modifiedResult.success) {
    throw new Error(`Failed to read file content at commit ${hash}: ${originalResult.stderr || modifiedResult.stderr}`);
  }

  return { original, modified, isBinary: false };
}
