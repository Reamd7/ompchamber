// Lightweight, process-global GitHub rate-limit gate.
//
// Octokit is configured without the throttling plugin, so a primary or
// secondary rate limit surfaces as a thrown 403/429. Resolving PR status for
// many worktrees fans out dozens of calls; once GitHub starts limiting, every
// further call wastes a round-trip and the cache masks the failure. When we
// detect a rate-limit response we record a cooldown and skip GitHub work until
// it passes, so the burst stops and the reason is visible in the logs.
/**
 * 进程级 GitHub rate-limit 冷却门（中文模块说明）：
 * 检测到主/次级限流的 403/429 后记录一段全局冷却期，期间
 * isGitHubRateLimited 为真，调用方应跳过 GitHub 工作直到冷却结束，
 * 让突发调用停下来且原因可见于日志。
 */

/** 冷却期上限：15 分钟，防止异常的 reset 头导致长时间停摆。 */
const MAX_COOLDOWN_MS = 15 * 60 * 1000;
/** 响应头未给出可用 retry-after/reset 信息时的默认冷却：60 秒。 */
const DEFAULT_COOLDOWN_MS = 60 * 1000;

/** 冷却截止时间戳（ms epoch）；0 表示当前未被限流。 */
let rateLimitedUntil = 0;

/**
 * 兼容读取响应头：同时支持 Headers 实例（.get 方法）与普通对象两种
 * 形态；headers 缺失时返回 undefined。
 */
const headerValue = (headers, name) => {
  if (!headers) return undefined;
  // Octokit/fetch headers can be a plain object or a Headers instance.
  if (typeof headers.get === 'function') return headers.get(name);
  return headers[name];
};

/**
 * 从错误响应头解析建议的冷却毫秒数：优先 retry-after（秒），其次
 * x-ratelimit-reset 与当前时间之差；均不可用时返回 null。
 */
const parseRetryAfterMs = (error) => {
  const headers = error?.response?.headers;
  const retryAfter = headerValue(headers, 'retry-after');
  if (retryAfter !== undefined && retryAfter !== null) {
    const secs = Number(retryAfter);
    if (Number.isFinite(secs) && secs > 0) return secs * 1000;
  }
  const reset = headerValue(headers, 'x-ratelimit-reset');
  if (reset !== undefined && reset !== null) {
    const delta = Number(reset) * 1000 - Date.now();
    if (Number.isFinite(delta) && delta > 0) return delta;
  }
  return null;
};

/**
 * 判断一个 Octokit 错误是否为主/次级 rate limit：429 直接认定；
 * 403 需满足 x-ratelimit-remaining 为 0、带 retry-after 头、或错误
 * 消息含 "rate limit" 之一。
 */
/** True when an Octokit error represents a primary or secondary rate limit. */
export const isGitHubRateLimitError = (error) => {
  const status = error?.status ?? error?.response?.status;
  if (status === 429) return true;
  if (status !== 403) return false;
  const remaining = headerValue(error?.response?.headers, 'x-ratelimit-remaining');
  if (remaining === '0' || remaining === 0) return true;
  if (headerValue(error?.response?.headers, 'retry-after') != null) return true;
  const message = String(error?.message ?? '').toLowerCase();
  return message.includes('rate limit');
};

/**
 * 记录一次限流冷却：取响应头建议的时长（封顶 15 分钟、缺省 60 秒），
 * 仅当新的截止时间更晚时才更新并打印警告日志。
 */
/** Record a cooldown after a detected rate-limit response. */
export const noteGitHubRateLimit = (error) => {
  const retryMs = Math.min(parseRetryAfterMs(error) ?? DEFAULT_COOLDOWN_MS, MAX_COOLDOWN_MS);
  const until = Date.now() + retryMs;
  if (until > rateLimitedUntil) {
    rateLimitedUntil = until;
    console.warn(`[github] rate limited — pausing GitHub PR status calls for ~${Math.round(retryMs / 1000)}s`);
  }
};

/** 便捷组合：若错误确为 rate limit 则记录冷却；返回是否命中。 */
/** Convenience: note the error if it is a rate-limit error. Returns whether it was. */
export const noteIfGitHubRateLimit = (error) => {
  if (!isGitHubRateLimitError(error)) return false;
  noteGitHubRateLimit(error);
  return true;
};

/** 当前是否处于限流冷却期内（期间调用方应跳过 GitHub 请求）。 */
export const isGitHubRateLimited = () => Date.now() < rateLimitedUntil;
