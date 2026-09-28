/**
 * npm registry 客户端：查询 registry.npmjs.org 上的包元数据（最新版本、
 * 版本列表、dist-tags），带模块级 TTL 缓存与并发去重（in-flight 复用），
 * 供插件市场等功能使用。所有查询均不抛出网络异常——错误统一封装为
 * { ok: false, status, error } 结构返回，status 为 HTTP 状态码或 'network'。
 */
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

/** 查询结果的缓存有效期（毫秒），默认 1 小时。 */
const NPM_CACHE_TTL_MS = 3_600_000;
/** 单次 registry 请求的超时时间（毫秒），经 AbortSignal.timeout 生效。 */
const NPM_FETCH_TIMEOUT_MS = 5_000;
/** npm registry 的固定基址。 */
const NPM_REGISTRY_BASE = 'https://registry.npmjs.org';

/**
 * 中文补充（结果类型总览）：NpmPackagePayload 为成功负载（ok: true，
 * 含 latest / versions / distTags）；NpmLookupError 为失败负载（ok: false，
 * status 为数字或 'network'）；NpmLookupResult 为二者之并；
 * NpmInfoOptions 携带 forceRefresh；CacheEntry 为带时间戳的缓存条目。
 */
/**
 * @typedef {Object} NpmPackagePayload
 * @property {true} ok
 * @property {string|null} latest
 * @property {string[]} versions
 * @property {Record<string, string>} distTags
 *
 * @typedef {Object} NpmLookupError
 * @property {false} ok
 * @property {number|'network'} status
 * @property {string} error
 *
 * @typedef {NpmPackagePayload | NpmLookupError} NpmLookupResult
 * @typedef {{ forceRefresh?: boolean }} NpmInfoOptions
 * @typedef {{ fetchedAt: number, payload: NpmLookupResult }} CacheEntry
 */

/** 以包名为键的 TTL 缓存：只缓存确定性结果（成功与 404），瞬态网络错误不入缓存。 */
/** @type {Map<string, CacheEntry>} */
const _cache = new Map();

/** 以包名为键的在途请求表：并发的同名查询复用同一个 Promise，避免重复发起请求。 */
/** @type {Map<string, Promise<NpmLookupResult>>} */
const _inFlight = new Map();

/** 惰性计算并缓存的 User-Agent 字符串（ompchamber-server/<version>）。 */
/** @type {string | null} */
let _userAgent = null;

/**
 * 计算仓库根 package.json 的绝对路径。
 * 本文件位于 packages/web/server/lib/opencode/，因此向上回溯五级目录。
 */
function _getPackageJsonPath() {
  const __dirname = path.dirname(fileURLToPath(import.meta.url));
  return path.resolve(__dirname, '..', '..', '..', '..', '..', 'package.json');
}

/**
 * 读取根 package.json 的 version 并拼接 User-Agent；读取或解析失败时
 * 回退为 'ompchamber-server/dev'。结果缓存在模块级 _userAgent 中。
 */
function _getUserAgent() {
  if (_userAgent) return _userAgent;

  try {
    const pkg = JSON.parse(fs.readFileSync(_getPackageJsonPath(), 'utf8'));
    _userAgent = `ompchamber-server/${typeof pkg.version === 'string' ? pkg.version : '0.0.0'}`;
  } catch {
    _userAgent = 'ompchamber-server/dev';
  }

  return _userAgent;
}

/**
 * 把包名编码进 registry URL 路径：encodeURIComponent 后仅把开头的 %40
 * 还原为 @，使 scoped 包（@scope/name）呈现为 @scope%2Fname 的规范形式。
 */
function encodeName(name) {
  return encodeURIComponent(name).replace(/^%40/, '@');
}

/**
 * 从 registry 响应中提取 dist-tags：仅保留值为字符串的键值对；
 * 非对象输入（含数组）返回空对象。
 */
function parseDistTags(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return {};
  }

  return Object.fromEntries(
    Object.entries(value)
      .filter((entry) => typeof entry[1] === 'string'),
  );
}

/**
 * 从 registry 响应中提取版本号列表（versions 对象的键）；
 * 非对象输入（含数组）返回空数组。
 */
function parseVersions(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return [];
  }

  return Object.keys(value);
}

/**
 * 把结果写入 TTL 缓存。只缓存确定性结果：成功（ok: true）与 404；
 * 5xx、网络错误等瞬态失败不缓存，以便下次调用立即重试。
 */
function cacheResult(name, payload) {
  if (payload.ok || payload.status === 404) {
    _cache.set(name, { fetchedAt: Date.now(), payload });
  }
}

/**
 * 中文补充：直接向 npm registry 发起一次无缓存的包元数据查询。成功返回
 * { ok: true, latest, versions, distTags }；404 映射为 'Package not found'；
 * 其它非 2xx 返回 registry 状态码错误；任何网络或超时异常都被捕获并折叠为
 * { ok: false, status: 'network', error }，绝不 reject。
 */
/**
 * Fetch package metadata directly from the npm registry.
 *
 * @param {string} name npm package name
 * @returns {Promise<NpmLookupResult>}
 */
export async function lookupNpmPackage(name) {
  try {
    const response = await fetch(`${NPM_REGISTRY_BASE}/${encodeName(name)}`, {
      headers: {
        'User-Agent': _getUserAgent(),
        Accept: 'application/json',
      },
      signal: AbortSignal.timeout(NPM_FETCH_TIMEOUT_MS),
    });

    if (response.ok) {
      const data = await response.json();
      const distTags = parseDistTags(data?.['dist-tags']);
      return {
        ok: true,
        latest: distTags.latest ?? null,
        versions: parseVersions(data?.versions),
        distTags,
      };
    }

    if (response.status === 404) {
      return { ok: false, status: 404, error: 'Package not found' };
    }

    return { ok: false, status: response.status, error: `Registry returned ${response.status}` };
  } catch (error) {
    return { ok: false, status: 'network', error: String(error?.message ?? error) };
  }
}

/**
 * 中文补充：带 TTL 缓存与并发去重的包元数据查询（业务侧首选入口）。
 * 命中未过期缓存直接返回；forceRefresh 强制穿透缓存；同名包的并发请求
 * 复用同一在途 Promise；完成后按 cacheResult 规则落缓存。
 */
/**
 * Fetch package metadata with TTL cache and in-flight request deduplication.
 *
 * @param {string} name npm package name
 * @param {NpmInfoOptions} [options]
 * @returns {Promise<NpmLookupResult>}
 */
export async function getNpmInfo(name, options = {}) {
  const { forceRefresh = false } = options;
  const cached = _cache.get(name);
  if (cached && !forceRefresh && Date.now() - cached.fetchedAt < NPM_CACHE_TTL_MS) {
    return cached.payload;
  }

  const existing = _inFlight.get(name);
  if (existing && !forceRefresh) {
    return existing;
  }

  const lookup = (async () => {
    const result = await lookupNpmPackage(name);
    cacheResult(name, result);
    return result;
  })();

  _inFlight.set(name, lookup);
  try {
    return await lookup;
  } finally {
    if (_inFlight.get(name) === lookup) {
      _inFlight.delete(name);
    }
  }
}

/**
 * 清空 TTL 缓存与在途请求表，主要供测试在用例间隔离模块状态。
 */
export function clearCache() {
  _cache.clear();
  _inFlight.clear();
}
