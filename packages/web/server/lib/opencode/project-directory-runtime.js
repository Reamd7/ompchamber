/**
 * 项目目录解析运行时：为各 OpenCode 代理路由确定“当前工作目录”。
 * 候选优先级：请求头 x-opencode-directory（可带 uri 编码标记）→
 * query 的 directory 参数 → 设置中的 lastDirectory（UI 正在浏览的
 * 目录）→ 活动项目路径。候选会经过规范化、存在性与目录类型校验，
 * 并借助 realpath 缓存解析符号链接得到规范目录。
 */
import { createRealpathCache } from '../path-realpath-cache.js';

// 中文补充：浏览器传输层会显式 percent-encode 目录提示并打上标记；
// 只解码带标记的值，直连 API 客户端发来的字面百分号序列得以保留。
// Browser transport percent-encodes directory hints and marks them explicitly.
// Only marked values are decoded so literal percent sequences from direct API
// clients are preserved.
const safeDecodeMarkedURIComponent = (value, encoding) => {
  if (encoding !== 'uri') return value;
  try { return decodeURIComponent(value); } catch { return value; }
};

/**
 * 创建项目目录解析运行时。
 *
 * @param {object} dependencies
 * @param {object} dependencies.fsPromises fs.promises 兼容对象
 * @param {object} dependencies.path node:path 兼容对象
 * @param {Function} dependencies.normalizeDirectoryPath 目录字符串规范化
 * @param {Function} dependencies.readSettingsFromDiskMigrated 读取设置（含迁移）
 * @param {Function} [dependencies.getReadSettingsFromDiskMigrated] 返回缓存版读取函数（优先使用）
 * @param {Function} dependencies.sanitizeProjects 清洗项目列表（路径规范化与去重）
 * @returns {{ resolveDirectoryCandidate: Function, validateDirectoryPath: Function,
 *   resolveProjectDirectory: Function, resolveOptionalProjectDirectory: Function }}
 */
export const createProjectDirectoryRuntime = (dependencies) => {
  const {
    fsPromises,
    path,
    normalizeDirectoryPath,
    readSettingsFromDiskMigrated,
    getReadSettingsFromDiskMigrated,
    sanitizeProjects,
  } = dependencies;
  // realpath 缓存：绑定 fsPromises.realpath，避免对同一目录重复解析符号链接。
  const realpathCache = createRealpathCache({
    realpath: fsPromises.realpath.bind(fsPromises),
  });

  /**
   * 把候选目录字符串整理成绝对路径：trim → normalizeDirectoryPath 规范化
   * → path.resolve 补全。非字符串或空白输入返回 null。
   */
  const resolveDirectoryCandidate = (value) => {
    if (typeof value !== 'string') {
      return null;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      return null;
    }
    const normalized = normalizeDirectoryPath(trimmed);
    return path.resolve(normalized);
  };

  /**
   * 校验候选目录并解析符号链接。
   *
   * 成功返回 { ok: true, directory（realpath 规范目录）, requestedDirectory
   * （调用方请求的先验路径）}——后者供文件树等“用户可见路径空间”的
   * 路由使用：项目根本身是符号链接时，规范 directory 可能不再包含客户端
   * 发来的路径。失败按情形映射为可读信息：缺少参数 / 非目录 / ENOENT
   * （不存在）/ EACCES（无权限）/ 其它未知错误。
   */
  const validateDirectoryPath = async (candidate) => {
    const resolved = resolveDirectoryCandidate(candidate);
    if (!resolved) {
      return { ok: false, error: 'Directory parameter is required' };
    }
    try {
      const stats = await fsPromises.stat(resolved);
      if (!stats.isDirectory()) {
        return { ok: false, error: 'Specified path is not a directory' };
      }
      const realPath = await realpathCache.resolve(resolved);
      // `requestedDirectory` is the pre-realpath candidate the caller asked
      // for. Callers that address files in the user-visible path space (the
      // file tree, the read-family FS routes) need it when the project root
      // is itself a symlink and the canonical `directory` no longer contains
      // the paths the client sends.
      return { ok: true, directory: realPath, requestedDirectory: resolved };
    } catch (error) {
      const err = error;
      if (err && typeof err === 'object' && err.code === 'ENOENT') {
        return { ok: false, error: 'Directory not found' };
      }
      if (err && typeof err === 'object' && err.code === 'EACCES') {
        return { ok: false, error: 'Access to directory denied' };
      }
      return { ok: false, error: 'Failed to validate directory' };
    }
  };

  /**
   * 从请求解析出当前项目目录（必填语义，失败时返回 error）。
   *
   * 候选顺序：x-opencode-directory 请求头（经标记解码）→
   * query.directory → 设置中的 lastDirectory（UI 当前浏览目录，优先于
   * activeProjectId——用户可能已从侧栏最后点击的项目导航离开，用过期的
   * 项目作用域会导致 400）→ 活动项目（无活动 id 时取第一个项目）。
   * 任一候选校验失败则继续尝试下一个；全部失败返回最后一个错误。
   */
  const resolveProjectDirectory = async (req) => {
    const rawHeaderDirectory = typeof req.get === 'function' ? req.get('x-opencode-directory') : null;
    const headerEncoding = typeof req.get === 'function' ? req.get('x-opencode-directory-encoding') : null;
    const headerDirectory = rawHeaderDirectory ? safeDecodeMarkedURIComponent(rawHeaderDirectory, headerEncoding) : null;
    const queryDirectory = Array.isArray(req.query?.directory)
      ? req.query.directory[0]
      : req.query?.directory;
    const requested = [headerDirectory, queryDirectory].filter(Boolean);

    if (requested.length > 0) {
      let lastError = null;
      for (const candidate of requested) {
        const validated = await validateDirectoryPath(candidate);
        if (validated.ok) {
          return { directory: validated.directory, requestedDirectory: validated.requestedDirectory, error: null };
        }
        lastError = validated.error;
      }
      return { directory: null, requestedDirectory: null, error: lastError };
    }

    const readSettings = typeof getReadSettingsFromDiskMigrated === 'function'
      ? getReadSettingsFromDiskMigrated()
      : readSettingsFromDiskMigrated;
    const settings = await readSettings();

    // `lastDirectory` reflects the directory the UI is currently browsing —
    // useDirectoryStore.setDirectory() persists it on every navigation.
    // Prefer it over activeProjectId, because the user may have navigated
    // away from the project that was last "clicked" in the sidebar (e.g. via
    // `go to parent`, directory picker, or a deep link), leaving
    // activeProjectId stale. Fetches scoped to the stale project would 400
    // with "Path is outside of active workspace".
    if (typeof settings.lastDirectory === 'string' && settings.lastDirectory.trim()) {
      const validated = await validateDirectoryPath(settings.lastDirectory);
      if (validated.ok) {
        return { directory: validated.directory, requestedDirectory: validated.requestedDirectory, error: null };
      }
    }

    const projects = sanitizeProjects(settings.projects) || [];
    if (projects.length === 0) {
      return { directory: null, requestedDirectory: null, error: 'Directory parameter or active project is required' };
    }

    const activeId = typeof settings.activeProjectId === 'string' ? settings.activeProjectId : '';
    const active = projects.find((project) => project.id === activeId) || projects[0];
    if (!active || !active.path) {
      return { directory: null, requestedDirectory: null, error: 'Directory parameter or active project is required' };
    }

    const validated = await validateDirectoryPath(active.path);
    if (!validated.ok) {
      return { directory: null, requestedDirectory: null, error: validated.error };
    }

    return { directory: validated.directory, requestedDirectory: validated.requestedDirectory, error: null };
  };

  /**
   * 可选语义的目录解析：仅当请求头或 query 显式携带 directory 时才解析；
   * 未携带返回全 null 的成功结果（不回退到设置中的目录），适合目录
   * 作用域可选的路由（如全局会话列表）。
   */
  const resolveOptionalProjectDirectory = async (req) => {
    const rawHeaderDirectory = typeof req.get === 'function' ? req.get('x-opencode-directory') : null;
    const headerEncoding = typeof req.get === 'function' ? req.get('x-opencode-directory-encoding') : null;
    const headerDirectory = rawHeaderDirectory ? safeDecodeMarkedURIComponent(rawHeaderDirectory, headerEncoding) : null;
    const queryDirectory = Array.isArray(req.query?.directory)
      ? req.query.directory[0]
      : req.query?.directory;
    const requested = [headerDirectory, queryDirectory].filter(Boolean);

    if (requested.length === 0) {
      return { directory: null, requestedDirectory: null, error: null };
    }

    let lastError = null;
    for (const candidate of requested) {
      const validated = await validateDirectoryPath(candidate);
      if (validated.ok) {
        return { directory: validated.directory, requestedDirectory: validated.requestedDirectory, error: null };
      }
      lastError = validated.error;
    }
    return { directory: null, requestedDirectory: null, error: lastError };
  };

  return {
    resolveDirectoryCandidate,
    validateDirectoryPath,
    resolveProjectDirectory,
    resolveOptionalProjectDirectory,
  };
};
