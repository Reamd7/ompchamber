/**
 * Octokit 客户端工厂模块。
 *
 * 为所有 GitHub REST 调用提供统一的 fetch 实现：单请求 8 秒超时
 * （AbortSignal），避免慢连接吃光 PR 状态路由的整体时间预算；GET 请求
 * 附带 ETag 条件缓存，GitHub 返回 304 时不消耗 REST rate limit。
 * createOctokit 创建带该 fetch 的实例，getOctokitOrNull 按 gh CLI 与
 * 本地存储 token 的优先级取当前凭证创建实例。
 */
import { Octokit } from '@octokit/rest';
import { getGitHubAuth, isGhCliActive, isGhCliDisabled } from './auth.js';
import { getGhCliToken } from './gh-cli-credential.js';

/** 单个 GitHub 请求的超时时间：8 秒（原生 fetch 自身没有超时机制）。 */
// Per-request timeout for every GitHub call. Octokit v22 uses native fetch,
// which has no built-in timeout — without this, a stuck connection hangs until
// some outer bound (the PR-status route's 12s overall budget) fires, and a
// single slow request can eat the whole budget. Bounding each request lets the
// caller fail fast and fall back to cached state instead.
const OCTOKIT_REQUEST_TIMEOUT_MS = 8000;

/**
 * 带超时的 fetch 包装：调用方已提供 signal 时原样透传（尊重外部取消），
 * 否则附加 AbortSignal.timeout 的 8 秒超时，让上层能快速失败并回退缓存。
 */
const timeoutFetch = (url, options = {}) => {
  // Respect a caller-provided signal if present; otherwise attach our timeout.
  if (options.signal) {
    return fetch(url, options);
  }
  return fetch(url, { ...options, signal: AbortSignal.timeout(OCTOKIT_REQUEST_TIMEOUT_MS) });
};

/** ETag 条件缓存的最大条目数（按 LRU 淘汰）。 */
// Conditional-request cache for GET calls: GitHub serves 304 Not Modified for
// matching If-None-Match WITHOUT counting the request against the REST rate
// limit, so polling unchanged PRs/checks becomes rate-limit-free. Keyed by
// token+URL so different identities never share responses.
const ETAG_CACHE_MAX_ENTRIES = 300;
/** ETag 缓存：`token 换行 URL` → { etag, body, headers }，按身份隔离响应。 */
const etagCache = new Map();

/** 写入一条 ETag 缓存（delete + set 刷新 LRU 位置），超限时淘汰最旧条目。 */
const rememberEtag = (key, etag, body, headers) => {
  etagCache.delete(key);
  etagCache.set(key, { etag, body, headers });
  if (etagCache.size > ETAG_CACHE_MAX_ENTRIES) {
    const oldest = etagCache.keys().next().value;
    if (oldest !== undefined) {
      etagCache.delete(oldest);
    }
  }
};

/**
 * 构造带 ETag 条件请求的 fetch，仅对 GET 生效（其它方法直通 timeoutFetch）。
 * 命中缓存时附 If-None-Match 头；GitHub 回 304 则重放缓存的 200 响应
 * （不计 REST rate limit）；200 且带 ETag 时缓存 body 供下次重放。
 */
const createConditionalFetch = (token) => async (url, options = {}) => {
  const method = (options.method || 'GET').toUpperCase();
  if (method !== 'GET') {
    return timeoutFetch(url, options);
  }

  const cacheKey = `${token}\n${url}`;
  const cached = etagCache.get(cacheKey);
  const headers = { ...(options.headers || {}) };
  if (cached?.etag) {
    headers['if-none-match'] = cached.etag;
  }

  const response = await timeoutFetch(url, { ...options, headers });

  if (response.status === 304 && cached) {
    // Touch for LRU and replay the cached success response.
    rememberEtag(cacheKey, cached.etag, cached.body, cached.headers);
    return new Response(cached.body, { status: 200, headers: cached.headers });
  }

  if (response.ok) {
    const etag = response.headers.get('etag');
    if (etag) {
      const body = await response.arrayBuffer();
      rememberEtag(cacheKey, etag, body, response.headers);
      return new Response(body, { status: response.status, headers: response.headers });
    }
  }

  return response;
};

/** 创建带单请求超时与 ETag 重验证 fetch 的 Octokit 实例。 */
/** Create an Octokit instance with per-request timeout + ETag revalidation. */
export function createOctokit(token) {
  return new Octokit({ auth: token, request: { fetch: createConditionalFetch(token) } });
}

/**
 * 按当前可用凭证创建 Octokit 实例；无任何 token 时返回 null。
 * token 优先级：gh CLI 激活时 gh token 优先（未禁用且能取到），否则
 * 本地存储的 access token 优先、gh token 兜底。
 */
export function getOctokitOrNull() {
  const auth = getGitHubAuth();
  const ghToken = !isGhCliDisabled() ? getGhCliToken() : null;
  const token = isGhCliActive() ? ghToken || auth?.accessToken : auth?.accessToken || ghToken;
  if (!token) {
    return null;
  }
  return createOctokit(token);
}
