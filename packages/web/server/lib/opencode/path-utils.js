/**
 * Shared PATH heuristics and merge utilities for server and Electron runtimes.
 *
 * The heuristic decides whether the current process.env.PATH looks like it was
 * configured by the user (or their session manager) vs. a minimal system default.
 * When the PATH looks user-configured we keep it; otherwise we prefer the login
 * shell PATH which typically has the full toolchain.
 */

/**
 * 中文说明：服务端与 Electron 桌面端共享的 PATH 启发式与合并工具。
 * pathLooksUserConfigured 判断当前 process.env.PATH 是“用户（或其会话
 * 管理器）配置过”还是“最小系统默认值”：前者直接沿用；后者优先改用
 * 登录 shell 的 PATH（通常包含完整工具链）。
 */

/** 常见包管理器 / 工具链的安装前缀（Homebrew、snap 等）。 */
const TOOLCHAIN_SEGMENTS = [
  '/opt/homebrew/',
  '/opt/pkg/',
  '/opt/pmk/',
  '/snap/',
];

/** 用户 home 下常见的工具链目录名（.cargo、.nvm、node_modules 等）。 */
const TOOLCHAIN_BASENAMES = new Set([
  '.cargo',
  '.bun',
  '.nvm',
  '.pyenv',
  '.rbenv',
  '.sdkman',
  '.asdf',
  '.volta',
  '.fnm',
  '.local',
  '.opencode',
  'node_modules',
]);

/**
 * 中文补充：判断 PATH 字符串中是否至少存在一段能表明“该 PATH 由用户
 * 或会话管理器配置”的路径——home 下的路径、常见工具链前缀，或路径中
 * 含常见工具链目录名。
 */
/**
 * Returns true when `value` (a PATH string) contains at least one segment that
 * suggests the PATH was configured by the user or their session manager rather
 * than being a bare system default.
 *
 * @param {string} value  - The PATH string to inspect.
 * @param {string} home   - The user's home directory (os.homedir()).
 * @param {string} delim  - The PATH delimiter (':' on POSIX, ';' on Windows).
 */
export function pathLooksUserConfigured(value, home, delim) {
  if (typeof value !== 'string' || !value) {
    return false;
  }

  const normalizedHome = typeof home === 'string' ? home.replaceAll('\\', '/') : '';
  const homeWithSep = normalizedHome ? normalizedHome + '/' : '';

  return value.split(delim).some((segment) => {
    if (!segment) return false;
    const normalizedSegment = segment.replaceAll('\\', '/');

    // Any path under the user's home directory.
    if (normalizedHome && (normalizedSegment === normalizedHome || normalizedSegment.startsWith(homeWithSep))) {
      return true;
    }

    // Well-known package-manager / toolchain prefixes.
    if (TOOLCHAIN_SEGMENTS.some((prefix) => normalizedSegment.startsWith(prefix))) {
      return true;
    }

    // Well-known dot-directories inside home (e.g. ~/.cargo/bin).
    const parts = normalizedSegment.split('/').filter(Boolean);
    if (parts.some((part) => TOOLCHAIN_BASENAMES.has(part))) {
      return true;
    }

    return false;
  });
}

/**
 * 中文补充：合并两个 PATH 字符串并按段去重——保持 primary 的顺序优先，
 * 再追加 fallback 中尚未出现的段。
 */
/**
 * Merges two PATH strings, deduplicating segments while preserving the order of
 * `primary` and appending any segments from `fallback` that are not already
 * present.
 *
 * @param {string} primary  - The preferred PATH (e.g. user-configured or login shell).
 * @param {string} fallback - The secondary PATH to fill gaps from.
 * @param {string} delim    - The PATH delimiter.
 */
export function mergePathValues(primary, fallback, delim) {
  const seen = new Set();
  const result = [];

  // 逐段并入：跳过空段与已见过的段，保持首次出现的顺序。
  const addSegments = (value) => {
    if (typeof value !== 'string' || !value) return;
    for (const segment of value.split(delim)) {
      if (segment && !seen.has(segment)) {
        seen.add(segment);
        result.push(segment);
      }
    }
  };

  addSegments(primary);
  addSegments(fallback);

  return result.join(delim);
}
