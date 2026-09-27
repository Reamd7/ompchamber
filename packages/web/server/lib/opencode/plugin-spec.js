/**
 * 插件来源标识（spec）的解析工具：把用户输入的插件标识拆解为“npm 包
 * （可选版本 / dist-tag）”与“本地路径”两类，供插件安装与更新流程决定
 * 是查询 npm registry 还是直接读取本地文件。全部为纯字符串解析，不做
 * 文件系统访问。
 */
import path from 'path';

/** 中文补充：解析成功的 npm spec —— { name, version }，version 为 null 表示未指定版本。 */
/**
 * @typedef {Object} ParsedNpmSpec
 * @property {string} name
 * @property {string|null} version
 */

/** 中文补充：解析失败的 npm spec —— malformed 恒为 true，raw 为原始输入的字符串形式。 */
/**
 * @typedef {Object} MalformedSpec
 * @property {true} malformed
 * @property {string} raw
 */

/** 中文补充：解析成功的路径 spec —— { absolutePath } 为规范化后的绝对路径。 */
/**
 * @typedef {Object} ParsedPathSpec
 * @property {string} absolutePath
 */

/**
 * 中文补充：把 npm 包标识字符串拆成包名 + 版本。支持 scoped 包
 * （@scope/name@version）与普通包（name@version）；版本可为精确版本、
 * 范围或 dist-tag。非字符串输入先经 String() 强转再判定为 malformed，
 * 避免异常输入直接抛错。
 */
/**
 * Parse an npm package spec string into name + version.
 * Handles scoped packages (`@scope/name[@version]`) and unscoped (`name[@version]`).
 * Non-string inputs are coerced via `String()` and returned as malformed.
 *
 * @param {unknown} spec
 * @returns {ParsedNpmSpec | MalformedSpec}
 */
export function parseNpmSpec(spec) {
  if (typeof spec !== 'string') {
    return { malformed: true, raw: String(spec) };
  }

  if (spec.startsWith('@')) {
    // scoped: '@scope/name' or '@scope/name@version'
    const slashIdx = spec.indexOf('/');
    if (slashIdx < 2) return { malformed: true, raw: spec }; // '@' or '@/foo'
    const afterSlash = spec.slice(slashIdx + 1);
    if (afterSlash === '') return { malformed: true, raw: spec }; // '@scope/'
    const atIdx = afterSlash.indexOf('@');
    if (atIdx === -1) return { name: spec, version: null };
    const namePart = spec.slice(0, slashIdx + 1 + atIdx); // '@scope/name'
    const versionPart = afterSlash.slice(atIdx + 1);
    if (versionPart === '') return { malformed: true, raw: spec }; // '@scope/foo@'
    return { name: namePart, version: versionPart };
  }

  // unscoped
  if (spec === '') return { malformed: true, raw: spec };
  const atIdx = spec.indexOf('@');
  if (atIdx === -1) return { name: spec, version: null };
  if (atIdx === 0) return { malformed: true, raw: spec }; // bare '@'
  const namePart = spec.slice(0, atIdx);
  const versionPart = spec.slice(atIdx + 1);
  if (versionPart === '') return { malformed: true, raw: spec }; // 'foo@'
  return { name: namePart, version: versionPart };
}

/**
 * 中文补充：判断版本串是否为“精确” semver（x.y.z，可带 pre-release 或
 * build 元数据后缀），即不含 ^ ~ > 等范围操作符、也不是 dist-tag。
 * 调用方以此区分精确版本与范围，选择不同的安装 / 更新策略。
 */
/**
 * Check whether a version string is an exact semver (no range operators).
 * Accepts optional pre-release (`-label`) or build metadata (`+label`) suffixes.
 *
 * @param {string} version
 * @returns {boolean}
 */
export function isExactSemver(version) {
  return /^\d+\.\d+\.\d+([-+][\w.-]+)?$/.test(version);
}

/**
 * 中文补充：判断插件标识是否更像本地路径而非 npm 包名：涵盖 Unix 绝对
 * 路径、./ 与 ../ 与 ~ 前缀，以及 Windows 绝对路径，避免把本地路径当作
 * 包名去查询 npm。
 */
/**
 * Check whether a plugin spec is path-like instead of an npm package spec.
 * Includes Windows absolute paths so local paths are never queried against npm.
 *
 * @param {string} spec
 * @returns {boolean}
 */
export function isPathSpec(spec) {
  return spec.startsWith('/')
    || spec.startsWith('./')
    || spec.startsWith('../')
    || spec.startsWith('~')
    || path.win32.isAbsolute(spec);
}

/**
 * 中文补充：把路径形式的插件标识解析为绝对路径：~ 与 ~/... 基于
 * homedir，./ 与 ../ 相对 cwd 解析，绝对路径直接返回（Windows 绝对
 * 路径原样保留），其余按相对 cwd 处理。纯函数，仅依赖 path.resolve。
 */
/**
 * Resolve a path-style plugin spec to an absolute path.
 * Supports `~` (home), `./`, `../` (relative to cwd), and absolute paths.
 * Pure — no filesystem access; uses only `path.resolve`.
 *
 * @param {string} spec
 * @param {{ homedir: string, cwd: string }} options
 * @returns {ParsedPathSpec}
 */
export function parsePathSpec(spec, { homedir, cwd }) {
  if (spec === '~') {
    return { absolutePath: path.resolve(homedir) };
  }
  if (spec.startsWith('~/')) {
    return { absolutePath: path.resolve(homedir, spec.slice(2)) };
  }
  if (spec.startsWith('./') || spec.startsWith('../')) {
    return { absolutePath: path.resolve(cwd, spec) };
  }
  if (path.win32.isAbsolute(spec)) {
    return { absolutePath: spec };
  }
  return { absolutePath: path.resolve(spec) };
}
