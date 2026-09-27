/**
 * Web UI 认证模块。
 *
 * 以工厂函数 createUiAuth 构建完整的 UI 会话认证栈：密码登录（scrypt 校验 +
 * 登录限流与锁定）、JWT 会话 cookie（HttpOnly / SameSite=Strict）、受信任设备
 * 长会话、passkey（WebAuthn）注册/认证/吊销、短时效 URL token（供 SSE/WebSocket
 * 等不便携带 cookie 的只读端点使用）、client bearer token 认证旁路，以及全局
 * 登出（轮换 JWT secret 使所有既有凭据失效）。未配置密码时返回接口形状一致的
 * 禁用态桩实现。限流与 URL token 状态均为进程内内存态，重启即清空。
 */
import crypto from 'crypto';
import { SignJWT, jwtVerify } from 'jose';
import fs from 'fs';
import path from 'path';
import os from 'os';
import { createUiPasskeys } from './ui-passkeys.js';

/** 会话 cookie 名称。 */
const SESSION_COOKIE_NAME = 'oc_ui_session';
/** 普通会话有效期：12 小时。 */
const SESSION_TTL_MS = 12 * 60 * 60 * 1000;
/** 受信任设备会话有效期：7 天。 */
const TRUSTED_DEVICE_SESSION_TTL_MS = 7 * 24 * 60 * 60 * 1000;
/** URL token 有效期：60 秒（短时效，降低经 URL/日志泄漏后的可利用窗口）。 */
const URL_AUTH_TOKEN_TTL_MS = 60 * 1000;
/** 本模块签发的 URL token 固定前缀，用于快速识别与拒绝其他 token。 */
const URL_AUTH_TOKEN_PREFIX = 'oc_url_';

/** 登录失败次数的统计窗口：5 分钟。 */
const RATE_LIMIT_WINDOW_MS = 5 * 60 * 1000;
/** 统计窗口内允许的最大失败次数（可由环境变量 OMPCHAMBER_RATE_LIMIT_MAX_ATTEMPTS 覆盖）。 */
const RATE_LIMIT_MAX_ATTEMPTS = Number(process.env.OMPCHAMBER_RATE_LIMIT_MAX_ATTEMPTS) || 10;
/** 连续失败触发锁定后的锁定期：15 分钟。 */
const RATE_LIMIT_LOCKOUT_MS = 15 * 60 * 1000;
/** 过期限流记录的周期清理间隔：1 小时。 */
const RATE_LIMIT_CLEANUP_MS = 60 * 60 * 1000;
/** 无法识别来源 IP 的请求使用的更严格上限（可由环境变量覆盖），防御匿名代理刷密码。 */
const RATE_LIMIT_NO_IP_MAX_ATTEMPTS = Number(process.env.OMPCHAMBER_RATE_LIMIT_NO_IP_MAX_ATTEMPTS) || 3;

/** 登录失败记录表：key（IP 或哨兵）→ { count, lastAttempt, lockedUntil }，进程内存态。 */
const loginRateLimiter = new Map();
/** 周期清理定时器句柄（unref，不阻止进程退出）。 */
let rateLimitCleanupTimer = null;

/** 每 key 的互斥锁表：以 Promise 链串行化同一 key 的限流状态读写，避免并发竞态。 */
const rateLimitLocks = new Map();

/**
 * 提取客户端 IP：优先取 x-forwarded-for 首段（反代场景），其次 req.ip /
 * connection.remoteAddress；统一剥离 IPv4-mapped IPv6 前缀（::ffff:）。
 * @param {object} req Express 请求对象
 * @returns {string|null} 取不到 IP 时返回 null
 */
const getClientIp = (req) => {
  const forwarded = req.headers['x-forwarded-for'];
  if (typeof forwarded === 'string') {
    const ip = forwarded.split(',')[0].trim();
    if (ip.startsWith('::ffff:')) {
      return ip.substring(7);
    }
    return ip;
  }

  const ip = req.ip || req.connection?.remoteAddress;
  if (ip) {
    if (ip.startsWith('::ffff:')) {
      return ip.substring(7);
    }
    return ip;
  }
  return null;
};

/**
 * 生成限流 key：能取到 IP 用 IP 本身，否则用固定哨兵 'rate-limit:no-ip'
 * （配合更小的次数上限）。
 */
const getRateLimitKey = (req) => {
  const ip = getClientIp(req);
  if (ip) return ip;
  return 'rate-limit:no-ip';
};

/**
 * 返回该 key 对应的限流配置：哨兵 key 用更小的 maxAttempts，其余用默认值。
 */
const getRateLimitConfig = (key) => {
  if (key === 'rate-limit:no-ip') {
    return {
      maxAttempts: RATE_LIMIT_NO_IP_MAX_ATTEMPTS,
      windowMs: RATE_LIMIT_WINDOW_MS
    };
  }
  return {
    maxAttempts: RATE_LIMIT_MAX_ATTEMPTS,
    windowMs: RATE_LIMIT_WINDOW_MS
  };
};

/**
 * 获取指定 key 的串行锁：以 Promise 链排队等待，锁完成后自动从表中移除。
 * 防止并发请求同时读改同一限流记录导致计数丢失。
 */
const acquireRateLimitLock = async (key) => {
  const prev = rateLimitLocks.get(key) || Promise.resolve();
  const curr = prev.then(() => rateLimitLocks.delete(key));
  rateLimitLocks.set(key, curr);
  await curr;
};

/**
 * 检查本次登录尝试是否被允许（只读，不累加计数）。
 * 逻辑：锁定期内直接拒绝并返回 retryAfter；锁定已过期则删除记录；记录超出
 * 统计窗口视为重新开始；计数达到上限则写入新的锁定时间。
 * 所有 Map 操作异常均降级为放行（fail-open）并记日志，保证认证可用性优先。
 * @param {object} req Express 请求对象
 * @returns {Promise<{allowed: boolean, limit: number, remaining: number, reset: number, retryAfter?: number, locked?: boolean}>}
 */
const checkRateLimit = async (req) => {
  const key = getRateLimitKey(req);
  await acquireRateLimitLock(key);

  const now = Date.now();
  const { maxAttempts } = getRateLimitConfig(key);

  let record;
  try {
    record = loginRateLimiter.get(key);
  } catch (err) {
    console.error('[RateLimit] Failed to get record', { key, error: err.message });
    return {
      allowed: true,
      limit: maxAttempts,
      remaining: maxAttempts,
      reset: Math.ceil((now + RATE_LIMIT_WINDOW_MS) / 1000)
    };
  }

  if (record?.lockedUntil && now < record.lockedUntil) {
    return {
      allowed: false,
      retryAfter: Math.ceil((record.lockedUntil - now) / 1000),
      locked: true,
      limit: maxAttempts,
      remaining: 0,
      reset: Math.ceil(record.lockedUntil / 1000)
    };
  }

  if (record?.lockedUntil && now >= record.lockedUntil) {
    try {
      loginRateLimiter.delete(key);
    } catch (err) {
      console.error('[RateLimit] Failed to delete expired record', { key, error: err.message });
    }
  }

  if (!record || now - record.lastAttempt > RATE_LIMIT_WINDOW_MS) {
    return {
      allowed: true,
      limit: maxAttempts,
      remaining: maxAttempts,
      reset: Math.ceil((now + RATE_LIMIT_WINDOW_MS) / 1000)
    };
  }

  if (record.count >= maxAttempts) {
    const lockedUntil = now + RATE_LIMIT_LOCKOUT_MS;
    try {
      loginRateLimiter.set(key, { count: record.count + 1, lastAttempt: now, lockedUntil });
    } catch (err) {
      console.error('[RateLimit] Failed to set lockout', { key, error: err.message });
    }
    return {
      allowed: false,
      retryAfter: Math.ceil(RATE_LIMIT_LOCKOUT_MS / 1000),
      locked: true,
      limit: maxAttempts,
      remaining: 0,
      reset: Math.ceil(lockedUntil / 1000)
    };
  }

  const remaining = maxAttempts - record.count;
  const reset = Math.ceil((record.lastAttempt + RATE_LIMIT_WINDOW_MS) / 1000);
  return {
    allowed: true,
    limit: maxAttempts,
    remaining,
    reset
  };
};

/**
 * 记录一次登录失败：无记录或已超出统计窗口则重置为 1，否则累加计数。
 * 是否触发锁定由下一次 checkRateLimit 判定。
 */
const recordFailedAttempt = async (req) => {
  const key = getRateLimitKey(req);
  await acquireRateLimitLock(key);

  const now = Date.now();
  const { maxAttempts } = getRateLimitConfig(key);
  const record = loginRateLimiter.get(key);

  if (!record || now - record.lastAttempt > RATE_LIMIT_WINDOW_MS) {
    try {
      loginRateLimiter.set(key, { count: 1, lastAttempt: now });
    } catch (err) {
      console.error('[RateLimit] Failed to record attempt', { key, error: err.message });
    }
  } else {
    const newCount = record.count + 1;
    try {
      loginRateLimiter.set(key, { count: newCount, lastAttempt: now });
    } catch (err) {
      console.error('[RateLimit] Failed to record attempt', { key, error: err.message });
    }
  }
};

/**
 * 清除该 key 的失败记录（登录成功后调用），同时解除尚未到期的锁定状态。
 */
const clearRateLimit = async (req) => {
  const key = getRateLimitKey(req);
  await acquireRateLimitLock(key);

  try {
    loginRateLimiter.delete(key);
  } catch (err) {
    console.error('[RateLimit] Failed to clear', { key, error: err.message });
  }
};

/**
 * 清理过期或陈旧的限流记录：锁定已到期、或最后尝试时间超过清理时长的条目被删除，
 * 防止 Map 无限增长。单条删除失败仅记日志不影响其余条目。
 */
const cleanupRateLimitRecords = () => {
  const now = Date.now();
  for (const [key, record] of loginRateLimiter.entries()) {
    const isExpired = record.lockedUntil && now >= record.lockedUntil;
    const isStale = now - record.lastAttempt > RATE_LIMIT_CLEANUP_MS;
    if (isExpired || isStale) {
      try {
        loginRateLimiter.delete(key);
      } catch (err) {
        console.error('[RateLimit] Cleanup failed', { key, error: err.message });
      }
    }
  }
};

/**
 * 启动周期清理定时器（幂等：已有定时器则跳过；unref 避免阻止进程退出）。
 */
const startRateLimitCleanup = () => {
  if (!rateLimitCleanupTimer) {
    rateLimitCleanupTimer = setInterval(cleanupRateLimitRecords, RATE_LIMIT_CLEANUP_MS);
    if (rateLimitCleanupTimer && typeof rateLimitCleanupTimer.unref === 'function') {
      rateLimitCleanupTimer.unref();
    }
  }
};

/**
 * 停止清理定时器并置空句柄（dispose 时调用）。
 */
const stopRateLimitCleanup = () => {
  if (rateLimitCleanupTimer) {
    clearInterval(rateLimitCleanupTimer);
    rateLimitCleanupTimer = null;
  }
};

/**
 * 判断请求是否经 HTTPS 到达：req.secure 或 x-forwarded-proto 首段为 https
 * （反向代理场景）。决定会话 cookie 是否附加 Secure 属性。
 */
const isSecureRequest = (req) => {
  if (req.secure) {
    return true;
  }
  const forwardedProto = req.headers['x-forwarded-proto'];
  if (typeof forwardedProto === 'string') {
    const firstProto = forwardedProto.split(',')[0]?.trim().toLowerCase();
    return firstProto === 'https';
  }
  return false;
};

/**
 * 解析 Cookie 请求头为普通对象：按 ';' 分段，'=' 只切第一处（值本身可含 '='），
 * 值经 decodeURIComponent 还原，解码失败退回原始字符串。
 * @param {string} cookieHeader Cookie 头原文
 * @returns {object} cookie 名到值的映射；入参无效返回空对象
 */
const parseCookies = (cookieHeader) => {
  if (!cookieHeader || typeof cookieHeader !== 'string') {
    return {};
  }

  return cookieHeader.split(';').reduce((acc, segment) => {
    const [name, ...rest] = segment.split('=');
    if (!name) {
      return acc;
    }
    const key = name.trim();
    if (!key) {
      return acc;
    }
    const value = rest.join('=').trim();
    try {
      acc[key] = decodeURIComponent(value || '');
    } catch {
      acc[key] = value || '';
    }
    return acc;
  }, {});
};

/**
 * 从 Authorization 头提取 Bearer token（"Bearer" 前缀大小写不敏感；
 * 头为数组时取第一个）。无有效 token 返回 null。
 */
const getBearerTokenFromRequest = (req) => {
  const header = req?.headers?.authorization;
  const value = Array.isArray(header) ? header[0] : header;
  if (typeof value === 'string') {
    const match = value.match(/^Bearer\s+(.+)$/i);
    const token = match?.[1]?.trim() || '';
    if (token) return token;
  }
  return null;
};

/**
 * 从请求中提取 URL token（query 参数 oc_url_token）：优先取解析后的 req.query
 * （数组取首个），退回从原始 req.url 解析；无有效值返回 null。
 */
const getUrlAuthTokenFromRequest = (req) => {
  const queryToken = req?.query?.oc_url_token;
  let token = Array.isArray(queryToken) ? queryToken[0] : queryToken;
  if (typeof token !== 'string' && typeof req?.url === 'string') {
    try {
      token = new URL(req.url, 'http://localhost').searchParams.get('oc_url_token') || undefined;
    } catch {
      token = undefined;
    }
  }
  return typeof token === 'string' && token.trim() ? token.trim() : null;
};

/**
 * 解析请求 pathname：优先 originalUrl/url 经 URL 解析；解析失败再退回 Express
 * 的 baseUrl+path 拼接（压缩重复斜杠）；最终兜底 req.path，均无则空串。
 * 用于 URL token 的路径白名单判定。
 */
const getRequestPathname = (req) => {
  const rawUrl = req?.originalUrl || req?.url;
  if (typeof rawUrl === 'string' && rawUrl) {
    try {
      return new URL(rawUrl, 'http://localhost').pathname;
    } catch {
      // Fall through to Express' derived path fields.
    }
  }
  if (typeof req?.baseUrl === 'string' && req.baseUrl && typeof req?.path === 'string' && req.path) {
    return `${req.baseUrl}${req.path}`.replace(/\/+/g, '/');
  }
  if (typeof req?.path === 'string' && req.path) return req.path;
  return '';
};

/**
 * 判断请求是否为 WebSocket 升级请求（Upgrade 头等于 websocket，大小写不敏感）。
 */
const isWebSocketUpgrade = (req) => {
  const upgrade = req?.headers?.upgrade;
  const upgradeValue = Array.isArray(upgrade) ? upgrade[0] : upgrade;
  return String(upgradeValue || '').toLowerCase() === 'websocket';
};

/**
 * 判定 pathname 是否属于允许 URL token 认证的只读 HTTP 端点白名单：
 * 事件流（SSE）、文件 serve、preview proxy、项目图标等。
 */
const isUrlAuthReadableHttpPath = (pathname) => {
  return pathname === '/api/event'
    || pathname === '/api/global/event'
    || pathname === '/api/ompchamber/events'
    || pathname === '/api/ompchamber/realtime-proxy/sse'
    || pathname === '/api/notifications/stream'
    || pathname === '/api/fs/raw'
    || pathname === '/api/fs/serve'
    || pathname.startsWith('/api/fs/serve/')
    || pathname.startsWith('/api/preview/proxy/')
    || /^\/api\/projects\/[^/]+\/icon$/.test(pathname);
};

/**
 * 判定 pathname 是否属于允许 URL token 认证的 WebSocket 端点白名单：
 * 事件 WS、realtime proxy、终端、听写及 preview proxy 前缀。
 */
const isUrlAuthWebSocketPath = (pathname) => {
  return pathname === '/api/event/ws'
    || pathname === '/api/global/event/ws'
    || pathname === '/api/ompchamber/realtime-proxy/ws'
    || pathname === '/api/terminal/ws'
    || pathname === '/api/dictation/ws'
    || pathname.startsWith('/api/preview/proxy/');
};

/**
 * 判定该请求是否允许使用 URL token 认证：WS 升级请求限 WS 白名单路径；
 * 普通请求限 GET 且在只读路径白名单内。收紧范围可防止 token 经 URL 泄漏后
 * 被用于执行写操作或访问任意端点。
 */
const canUseUrlAuthTokenForRequest = (req) => {
  const method = typeof req?.method === 'string' ? req.method.toUpperCase() : 'GET';
  const pathname = getRequestPathname(req);
  if (isWebSocketUpgrade(req)) {
    return isUrlAuthWebSocketPath(pathname);
  }
  return method === 'GET' && isUrlAuthReadableHttpPath(pathname);
};

/**
 * 拼装 Set-Cookie 头值：固定 Path=/、HttpOnly、SameSite=Strict，
 * 可选 Max-Age/Expires/Secure。maxAge 为 0 时 Expires 置为 Unix 纪元以立即清除。
 * @param {object} options name/value/maxAge（秒）/secure
 * @returns {string} 完整的 Set-Cookie 头值
 */
const buildCookie = ({
  name,
  value,
  maxAge,
  secure,
}) => {
  const attributes = [
    `${name}=${value}`,
    'Path=/',
    'HttpOnly',
    'SameSite=Strict',
  ];

  if (typeof maxAge === 'number') {
    attributes.push(`Max-Age=${Math.max(0, Math.floor(maxAge))}`);
  }

  const expires = maxAge === 0
    ? 'Thu, 01 Jan 1970 00:00:00 GMT'
    : new Date(Date.now() + maxAge * 1000).toUTCString();

  attributes.push(`Expires=${expires}`);

  if (secure) {
    attributes.push('Secure');
  }

  return attributes.join('; ');
};

/**
 * 归一化密码候选值：Unicode NFC 归一 + 去首尾空白；非字符串返回空串。
 * 保证存储与比对使用同一形态，避免组合字符差异导致校验失败。
 */
const normalizePassword = (candidate) => {
  if (typeof candidate !== 'string') {
    return '';
  }
  return candidate.normalize().trim();
};

/**
 * 判定请求体是否声明“信任此设备”（严格等于布尔 true，用于发放长时会话）。
 */
const isTrustedDeviceRequest = (value) => value === true;

/**
 * 数据目录：环境变量 OMPCHAMBER_DATA_DIR 指定，否则 ~/.config/ompchamber。
 * 用于存放 jwt-secret 等凭据材料。
 */
const OMPCHAMBER_DATA_DIR = process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber');
/** JWT 签名密钥文件路径（数据目录下的 jwt-secret）。 */
const JWT_SECRET_FILE = path.join(OMPCHAMBER_DATA_DIR, 'jwt-secret');

/**
 * 获取或创建 JWT 签名密钥：优先环境变量 OPENCODE_JWT_SECRET；其次读取已持久化
 * 的密钥文件；否则生成 32 字节随机 hex 并以 0o600 权限落盘（写盘失败仅告警，
 * 仍返回新密钥，代价是重启后旧会话失效）。
 * @returns {Uint8Array} 用于 HMAC 签名的密钥字节
 */
function getOrCreateJwtSecret() {
  const envSecret = process.env.OPENCODE_JWT_SECRET;
  if (envSecret) {
    return new TextEncoder().encode(envSecret);
  }

  try {
    if (fs.existsSync(JWT_SECRET_FILE)) {
      return new TextEncoder().encode(fs.readFileSync(JWT_SECRET_FILE, 'utf8').trim());
    }
  } catch (e) {
    console.warn('[JWT] Failed to read secret file:', e.message);
  }

  const secret = crypto.randomBytes(32).toString('hex');
  try {
    fs.mkdirSync(OMPCHAMBER_DATA_DIR, { recursive: true });
    fs.writeFileSync(JWT_SECRET_FILE, secret, { mode: 0o600 });
    console.log('[JWT] Generated and persisted new secret to', JWT_SECRET_FILE);
  } catch (e) {
    console.warn('[JWT] Failed to persist secret:', e.message);
  }

  return new TextEncoder().encode(secret);
}

/**
 * 持久化新的 JWT 密钥（全局登出时轮换用）：若密钥来自环境变量则无法轮换，
 * 抛出 statusCode=400 的错误；否则写盘保存并以 0o600 权限限权。
 * @param {string} secret 新密钥（hex 字符串）
 * @returns {Uint8Array} 新密钥字节
 * @throws {Error} OPENCODE_JWT_SECRET 已设置时抛出（附 statusCode 400）
 */
function persistJwtSecret(secret) {
  if (process.env.OPENCODE_JWT_SECRET) {
    const error = new Error('Global sign-out is unavailable while OPENCODE_JWT_SECRET is set');
    error.statusCode = 400;
    throw error;
  }

  fs.mkdirSync(OMPCHAMBER_DATA_DIR, { recursive: true });
  fs.writeFileSync(JWT_SECRET_FILE, secret, { mode: 0o600 });
  return new TextEncoder().encode(secret);
}

/**
 * 创建 UI 认证实例。
 * @param {object} options password UI 密码（为空时构建禁用态桩实现）；
 *   cookieName 会话 cookie 名；sessionTtlMs 会话有效期；readSettingsFromDiskMigrated
 *   读磁盘设置（passkey 控制器使用）；clientAuthController client bearer token
 *   认证控制器（可选）；requireClientAuth 是否强制 client 认证（禁用态下仍生效）。
 * @returns {object} 认证对象：enabled 标志、requireAuth/requireSessionAuth/
 *   resolveAuthContext 中间件与全部 session/passkey/reset 路由处理器、
 *   ensureSessionToken 及 dispose。
 */
export const createUiAuth = ({
  password,
  cookieName = SESSION_COOKIE_NAME,
  sessionTtlMs = SESSION_TTL_MS,
  readSettingsFromDiskMigrated,
  clientAuthController = null,
  requireClientAuth = false,
} = {}) => {
  // 归一化后的密码（NFC + trim）；为空表示未启用密码认证。
  const normalizedPassword = normalizePassword(password);
  // URL token 表：token → { sessionToken, expiresAt }，进程内存态，随扫除清理。
  const urlAuthTokens = new Map();

  /**
   * 扫除已过期的 URL token（惰性触发：每次签发新 token 前调用）。
   */
  const sweepUrlAuthTokens = () => {
    const now = Date.now();
    for (const [token, entry] of urlAuthTokens.entries()) {
      if (!entry || entry.expiresAt <= now) {
        urlAuthTokens.delete(token);
      }
    }
  };

  /**
   * 为已认证会话签发短时效 URL token（前缀 oc_url_ + 24 字节随机 base64url），
   * 供无法携带 cookie 的只读 GET/SSE/WS 端点使用。
   * @param {string} sessionToken 关联的会话标识
   * @returns {{token: string, expiresAt: number}}
   */
  const issueUrlAuthTokenForSession = (sessionToken) => {
    sweepUrlAuthTokens();
    const token = `${URL_AUTH_TOKEN_PREFIX}${crypto.randomBytes(24).toString('base64url')}`;
    const expiresAt = Date.now() + URL_AUTH_TOKEN_TTL_MS;
    urlAuthTokens.set(token, { sessionToken, expiresAt });
    return { token, expiresAt };
  };

  /**
   * 校验请求中的 URL token：请求不在白名单（方法/路径）、前缀不符、不存在或已过期
   * 均返回 null（过期条目顺手删除）；通过则返回 { ok, sessionToken }
   * （关联会话缺失时使用哨兵 'url:authenticated'）。
   */
  const authenticateUrlAuthToken = (req) => {
    if (!canUseUrlAuthTokenForRequest(req)) return null;
    const token = getUrlAuthTokenFromRequest(req);
    if (!token || !token.startsWith(URL_AUTH_TOKEN_PREFIX)) return null;
    const entry = urlAuthTokens.get(token);
    if (!entry || entry.expiresAt <= Date.now()) {
      urlAuthTokens.delete(token);
      return null;
    }
    return { ok: true, sessionToken: entry.sessionToken || 'url:authenticated' };
  };

  /**
   * client 认证入口：先尝试 URL token（allowUrlToken 可关），再尝试 Authorization
   * Bearer，均委托 clientAuthController.authenticateBearerToken；未配置控制器、
   * 无 token、校验失败或抛错一律返回 null（不向外抛错）。
   * @param {object} req Express 请求
   * @param {object} opts allowUrlToken 是否允许 URL token 旁路
   * @returns {Promise<object|null>} 认证结果或 null
   */
  const authenticateClientRequest = async (req, { allowUrlToken = true } = {}) => {
    if (allowUrlToken) {
      const urlAuth = authenticateUrlAuthToken(req);
      if (urlAuth) return urlAuth;
    }
    const token = getBearerTokenFromRequest(req);
    if (!token || typeof clientAuthController?.authenticateBearerToken !== 'function') {
      return null;
    }
    try {
      const result = await clientAuthController.authenticateBearerToken(token, req);
      if (result?.ok) {
        return result;
      }
      return null;
    } catch {
      return null;
    }
  };

  /**
   * 从 client 认证结果提取会话标识：已带 client:/url: 前缀原样返回，
   * 其余补 client: 前缀，无任何可用值时兜底 'client:authenticated'。
   */
  const clientSessionToken = (clientAuth) => {
    const raw = clientAuth?.sessionToken || clientAuth?.clientId || clientAuth?.id;
    if (typeof raw === 'string' && (raw.startsWith('client:') || raw.startsWith('url:'))) return raw;
    return typeof raw === 'string' && raw.length > 0 ? `client:${raw}` : 'client:authenticated';
  };

  /**
   * 从 client 认证结果提取 clientId（多字段兜底），并剥掉 client: 前缀；
   * 无可用值返回 null。
   */
  const clientAuthClientId = (clientAuth) => {
    const raw = clientAuth?.client?.id || clientAuth?.clientId || clientAuth?.id || clientAuth?.sessionToken;
    if (typeof raw !== 'string' || raw.length === 0) return null;
    return raw.startsWith('client:') ? raw.slice('client:'.length) : raw;
  };

  /**
   * 将 client 认证结果组装为统一的认证上下文
   * { type: 'client', token, clientId, client }，与会话上下文同构。
   */
  const clientAuthContext = (clientAuth) => ({
    type: 'client',
    token: clientSessionToken(clientAuth),
    clientId: clientAuthClientId(clientAuth),
    client: clientAuth?.client || null,
  });

  // 未配置密码的禁用态分支：返回接口形状一致的桩实现（认证写操作一律 400）。
  if (!normalizedPassword) {
    /**
     * 写会话 cookie（禁用态）：按 HTTPS 与 TTL 组装 Set-Cookie 头。
     */
    const setSessionCookie = (req, res, token, ttlMs = sessionTtlMs) => {
      const secure = isSecureRequest(req);
      const maxAgeSeconds = Math.floor(ttlMs / 1000);
      const header = buildCookie({
        name: cookieName,
        value: encodeURIComponent(token),
        maxAge: maxAgeSeconds,
        secure,
      });
      res.setHeader('Set-Cookie', header);
    };

    /**
     * 确保存在会话 token（禁用态）：cookie 已有则复用，否则签发随机 token 并写 cookie。
     */
    const ensureSessionToken = async (req, res) => {
      const cookies = parseCookies(req.headers.cookie);
      if (cookies[cookieName]) {
        return cookies[cookieName];
      }
      const token = crypto.randomBytes(32).toString('base64url');
      setSessionCookie(req, res, token, sessionTtlMs);
      return token;
    };

    /**
     * 通用认证中间件（禁用态）：仅当要求 client 认证时校验 URL token/Bearer，
     * 其余直接放行；OPTIONS 预检放行。
     */
    const requireAuth = async (req, res, next) => {
      if (!requireClientAuth) {
        return next();
      }
      if (req.method === 'OPTIONS') {
        return next();
      }
      const clientAuth = await authenticateClientRequest(req);
      if (clientAuth) {
        return next();
      }
      return res.status(401).json({ error: 'Client authentication required', locked: true, clientAuthRequired: true });
    };

    /**
     * 会话认证中间件（禁用态）：要求 client 认证时一律 401，否则放行；OPTIONS 放行。
     */
    const requireSessionAuth = async (req, res, next) => {
      if (!requireClientAuth) {
        return next();
      }
      if (req.method === 'OPTIONS') {
        return next();
      }
      return res.status(401).json({ error: 'UI session authentication required', locked: true });
    };

    /**
     * 解析认证上下文（禁用态）：cookie 会话 > client 认证 > 自动补发匿名会话 token；
     * 要求 client 认证且都不满足时返回 null。
     */
    const resolveAuthContext = async (req, res, { allowClientAuth = true, allowUrlToken = true } = {}) => {
      const cookies = parseCookies(req.headers.cookie);
      if (cookies[cookieName]) {
        return { type: 'session', token: cookies[cookieName] };
      }
      if (allowClientAuth) {
        const clientAuth = await authenticateClientRequest(req, { allowUrlToken });
        if (clientAuth) return clientAuthContext(clientAuth);
      }
      if (!requireClientAuth) {
        const token = await ensureSessionToken(req, res);
        return { type: 'session', token };
      }
      return null;
    };

    // 禁用态桩对象：保持与启用态一致的接口形状。
    return {
      enabled: false,
      requireAuth,
      requireSessionAuth,
      resolveAuthContext,
      /** 会话状态查询（禁用态）：client 认证通过即 client scope，否则按配置返回已认证或 401。 */
      handleSessionStatus: async (req, res) => {
        if (requireClientAuth) {
          const clientAuth = await authenticateClientRequest(req);
          if (clientAuth) {
            return res.json({ authenticated: true, disabled: true, scope: 'client' });
          }
          return res.status(401).json({ authenticated: false, locked: true, clientAuthRequired: true });
        }
        res.json({ authenticated: true, disabled: true });
      },
      /** 密码登录（禁用态）：未配置密码，恒 400。 */
      handleSessionCreate: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** URL token 签发（禁用态）：client 认证或匿名会话换发短时效 token；响应 no-store。 */
      handleUrlAuthToken: async (req, res) => {
        const clientAuth = await authenticateClientRequest(req, { allowUrlToken: false });
        if (clientAuth) {
          res.setHeader('Cache-Control', 'no-store');
          return res.json(issueUrlAuthTokenForSession(clientSessionToken(clientAuth)));
        }
        if (requireClientAuth) {
          return res.status(401).json({ error: 'Client authentication required', locked: true, clientAuthRequired: true });
        }
        const sessionToken = await ensureSessionToken(req, res);
        res.setHeader('Cache-Control', 'no-store');
        return res.json(issueUrlAuthTokenForSession(sessionToken));
      },
      /** passkey 状态（禁用态）：恒报未启用。 */
      handlePasskeyStatus: (_req, res) => {
        res.json({ enabled: false, hasPasskeys: false, passkeyCount: 0, rpID: null });
      },
      /** passkey 注册 options（禁用态）：恒 400。 */
      handlePasskeyRegistrationOptions: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** passkey 注册校验（禁用态）：恒 400。 */
      handlePasskeyRegistrationVerify: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** passkey 认证 options（禁用态）：恒 400。 */
      handlePasskeyAuthenticationOptions: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** passkey 认证校验（禁用态）：恒 400。 */
      handlePasskeyAuthenticationVerify: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** passkey 列表（禁用态）：恒空列表。 */
      handlePasskeyList: (_req, res) => {
        res.json({ passkeys: [] });
      },
      /** passkey 吊销（禁用态）：恒 400。 */
      handlePasskeyRevoke: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** 重置认证（禁用态）：恒 400。 */
      handleResetAuth: (_req, res) => {
        res.status(400).json({ error: 'UI password not configured' });
      },
      /** 会话 token（禁用态）：client 认证优先，否则补发匿名会话 token。 */
      ensureSessionToken: async (req, res) => {
        const clientAuth = await authenticateClientRequest(req);
        if (clientAuth) return clientSessionToken(clientAuth);
        return ensureSessionToken(req, res);
      },
      /** 清理（禁用态）：无资源需要释放。 */
      dispose: () => {

      },
    };
  }

  // scrypt 盐：实例创建时随机生成，仅存内存（密码哈希不落盘）。
  const salt = crypto.randomBytes(16);
  // 期望的 scrypt 哈希（64 字节），verifyPassword 用 timingSafeEqual 比对。
  const expectedHash = crypto.scryptSync(normalizedPassword, salt, 64);
  // 当前 JWT 签名密钥（可被 rotateJwtSecret 轮换）。
  let jwtSecret = getOrCreateJwtSecret();
  // 密码绑定值：HMAC(jwtSecret, password)。passkey 凭据以此绑定，轮换密钥即失效。
  let passwordBinding = crypto.createHmac('sha256', jwtSecret).update(normalizedPassword).digest('hex');
  /**
   * 依据“是否信任此设备”选择会话 TTL：信任 7 天，否则用默认（12 小时）。
   */
  const resolveSessionTtlMs = (trustDevice) => (trustDevice ? TRUSTED_DEVICE_SESSION_TTL_MS : sessionTtlMs);
  // passkey（WebAuthn）控制器：注册/认证/列表/吊销均委托它完成。
  let passkeyController = createUiPasskeys({
    passwordBinding,
    readSettingsFromDiskMigrated,
  });

  /**
   * 重建 passkey 控制器：密钥轮换后 passwordBinding 随之改变，需以新绑定值
   * 重新初始化（旧绑定下注册的凭据自然失效，实现“全局登出”）。
   */
  const rebuildPasskeyController = () => {
    passkeyController.dispose();
    passwordBinding = crypto.createHmac('sha256', jwtSecret).update(normalizedPassword).digest('hex');
    passkeyController = createUiPasskeys({
      passwordBinding,
      readSettingsFromDiskMigrated,
    });
  };

  /**
   * 轮换 JWT 密钥并作废全部既有凭据：持久化新密钥、清空 URL token 表、
   * 重建 passkey 控制器。之后所有旧会话 JWT 与 URL token 均校验失败。
   */
  const rotateJwtSecret = () => {
    const nextSecret = crypto.randomBytes(32).toString('hex');
    jwtSecret = persistJwtSecret(nextSecret);
    urlAuthTokens.clear();
    rebuildPasskeyController();
  };

  /**
   * 从会话 cookie 中提取 token；无对应 cookie 返回 null。
   */
  const getTokenFromRequest = (req) => {
    const cookies = parseCookies(req.headers.cookie);
    if (cookies[cookieName]) {
      return cookies[cookieName];
    }
    return null;
  };

  /**
   * 写会话 cookie：token 经 URL 编码，按请求是否 HTTPS 加 Secure，
   * TTL 换算为 Max-Age/Expires。
   */
  const setSessionCookie = (req, res, token, ttlMs) => {
    const secure = isSecureRequest(req);
    const maxAgeSeconds = Math.floor(ttlMs / 1000);
    const header = buildCookie({
      name: cookieName,
      value: encodeURIComponent(token),
      maxAge: maxAgeSeconds,
      secure,
    });
    res.setHeader('Set-Cookie', header);
  };

  /**
   * 清除会话 cookie（Max-Age=0 且 Expires 置为 Unix 纪元）。
   */
  const clearSessionCookie = (req, res) => {
    const secure = isSecureRequest(req);
    const header = buildCookie({
      name: cookieName,
      value: '',
      maxAge: 0,
      secure,
    });
    res.setHeader('Set-Cookie', header);
  };

  /**
   * 校验密码候选值：scrypt(候选, salt) 与期望哈希做 timingSafeEqual（防时序侧信道）；
   * 空值、归一化后为空或计算异常一律返回 false。
   */
  const verifyPassword = (candidate) => {
    if (!candidate) {
      return false;
    }
    const normalizedCandidate = normalizePassword(candidate);
    if (!normalizedCandidate) {
      return false;
    }
    try {
      const candidateHash = crypto.scryptSync(normalizedCandidate, salt, 64);
      return crypto.timingSafeEqual(candidateHash, expectedHash);
    } catch {
      return false;
    }
  };

  /**
   * 校验 JWT 会话 token 的签名与有效期；空 token 或校验失败返回 false（不抛错）。
   */
  const isSessionValid = async (token) => {
    if (!token) {
      return false;
    }
    try {
      await jwtVerify(token, jwtSecret);
      return true;
    } catch {
      return false;
    }
  };

  /**
   * 签发 HS256 JWT 会话（payload 仅 type: 'ui-session'）并写入 cookie；
   * trustDevice 决定有效期长短。
   * @returns {Promise<string>} 签发的 token
   */
  const issueSession = async (req, res, { trustDevice = false } = {}) => {
    const ttlMs = resolveSessionTtlMs(trustDevice);
    const token = await new SignJWT({ type: 'ui-session' })
      .setProtectedHeader({ alg: 'HS256' })
      .setIssuedAt()
      .setExpirationTime(ttlMs / 1000 + 's')
      .sign(jwtSecret);
    setSessionCookie(req, res, token, ttlMs);
    return token;
  };

  // 启用密码认证：同样需要限流记录的周期清理。
  startRateLimitCleanup();

  /**
   * 统一 401 响应：Accept 含 JSON 或 /api 路径返回 JSON（locked:true），
   * 其余返回纯文本，兼顾 API 客户端与浏览器跳转登录页。
   */
  const respondUnauthorized = (req, res) => {
    res.status(401);
    const acceptsJson = req.headers.accept?.includes('application/json');
    if (acceptsJson || req.path?.startsWith('/api')) {
      res.json({ error: 'UI authentication required', locked: true });
    } else {
      res.type('text/plain').send('Authentication required');
    }
  };

  /**
   * 通用认证中间件：OPTIONS 预检放行；会话 cookie 有效或 client 认证
   * （URL token/Bearer）通过则放行；否则清除 cookie 并返回 401。
   */
  const requireAuth = async (req, res, next) => {
    if (req.method === 'OPTIONS') {
      return next();
    }
    const token = getTokenFromRequest(req);
    if (await isSessionValid(token)) {
      return next();
    }
    const clientAuth = await authenticateClientRequest(req);
    if (clientAuth) {
      return next();
    }
    clearSessionCookie(req, res);
    return respondUnauthorized(req, res);
  };

  /**
   * 仅认会话 cookie 的中间件（client bearer/URL token 不放行）；
   * OPTIONS 放行，失败清 cookie 并 401。
   */
  const requireSessionAuth = async (req, res, next) => {
    if (req.method === 'OPTIONS') {
      return next();
    }
    const token = getTokenFromRequest(req);
    if (await isSessionValid(token)) {
      return next();
    }
    clearSessionCookie(req, res);
    return respondUnauthorized(req, res);
  };

  /**
   * 会话状态查询：请求显式携带 Bearer 时只依据 bearer 判定（避免回退到
   * 环境会话 cookie 掩盖已吊销的 token）；否则依次尝试会话 cookie 与 client 认证。
   */
  const handleSessionStatus = async (req, res) => {
    // An explicit bearer credential decides the answer on its own. Native
    // clients probe with the token their runtime transport will actually use;
    // falling back to the ambient session cookie here masked revoked tokens
    // (cookie said "authenticated", every bearer-only API call then 401'd).
    const authorization = req.headers?.authorization;
    const hasBearer = typeof authorization === 'string' && authorization.toLowerCase().startsWith('bearer ');
    if (hasBearer) {
      const clientAuth = await authenticateClientRequest(req, { allowUrlToken: false });
      if (clientAuth) {
        res.json({ authenticated: true, scope: 'client' });
        return;
      }
      res.status(401).json({ authenticated: false, locked: true });
      return;
    }
    const token = getTokenFromRequest(req);
    if (await isSessionValid(token)) {
      res.json({ authenticated: true });
      return;
    }
    const clientAuth = await authenticateClientRequest(req);
    if (clientAuth) {
      res.json({ authenticated: true, scope: 'client' });
      return;
    }
    clearSessionCookie(req, res);
    res.status(401).json({ authenticated: false, locked: true });
  };

  /**
   * 解析当前请求可用的会话标识：有效会话 cookie 的 token，
   * 或 client 认证派生的 client:/url: 标识；均失败返回 null。
   */
  const resolveAuthenticatedSessionToken = async (req, { allowUrlToken = true } = {}) => {
    const token = getTokenFromRequest(req);
    if (await isSessionValid(token)) {
      return token;
    }
    const clientAuth = await authenticateClientRequest(req, { allowUrlToken });
    return clientAuth ? clientSessionToken(clientAuth) : null;
  };

  /**
   * 解析认证上下文：有效会话 cookie → { type: 'session', token }；
   * allowClientAuth 时尝试 client 认证 → { type: 'client', ... }；均失败返回 null。
   */
  const resolveAuthContext = async (req, _res, { allowClientAuth = true, allowUrlToken = true } = {}) => {
    const token = getTokenFromRequest(req);
    if (await isSessionValid(token)) {
      return { type: 'session', token };
    }
    if (!allowClientAuth) return null;
    const clientAuth = await authenticateClientRequest(req, { allowUrlToken });
    return clientAuth ? clientAuthContext(clientAuth) : null;
  };

  /**
   * 签发 URL token：要求请求已通过会话或 client bearer 认证
   * （不允许用 URL token 换新 URL token），响应附加 no-store。
   */
  const handleUrlAuthToken = async (req, res) => {
    const sessionToken = await resolveAuthenticatedSessionToken(req, { allowUrlToken: false });
    if (!sessionToken) {
      clearSessionCookie(req, res);
      return respondUnauthorized(req, res);
    }
    res.setHeader('Cache-Control', 'no-store');
    return res.json(issueUrlAuthTokenForSession(sessionToken));
  };

  /**
   * 密码登录：先过限流（响应携带 X-RateLimit-* 头，超限返回 429 + Retry-After）；
   * 密码错误记录失败次数并 401；成功后清除计数、按“信任设备”签发会话，
   * 可选同步签发 client token（clientAuthController.createClient，透传设备信息）。
   */
  const handleSessionCreate = async (req, res) => {
    const rateLimitResult = await checkRateLimit(req);

    res.setHeader('X-RateLimit-Limit', rateLimitResult.limit);
    res.setHeader('X-RateLimit-Remaining', rateLimitResult.remaining);
    res.setHeader('X-RateLimit-Reset', rateLimitResult.reset);

    if (!rateLimitResult.allowed) {
      res.setHeader('Retry-After', rateLimitResult.retryAfter);
      res.status(429).json({ 
        error: 'Too many login attempts, please try again later',
        retryAfter: rateLimitResult.retryAfter 
      });
      return;
    }

    const candidate = typeof req.body?.password === 'string' ? req.body.password : '';
    if (!verifyPassword(candidate)) {
      await recordFailedAttempt(req);
      clearSessionCookie(req, res);
      res.status(401).json({ error: 'Invalid credentials' });
      return;
    }

    await clearRateLimit(req);

    const trustDevice = isTrustedDeviceRequest(req.body?.trustDevice);
    const ttlMs = resolveSessionTtlMs(trustDevice);
    await issueSession(req, res, { trustDevice });
    let clientTokenResult = null;
    if (req.body?.issueClientToken === true && typeof clientAuthController?.createClient === 'function') {
      clientTokenResult = await clientAuthController.createClient({
        fallbackLabel: req.body?.clientLabel,
        expiresAt: new Date(Date.now() + ttlMs).toISOString(),
        clientKind: req.body?.clientKind,
        dedupeKey: req.body?.dedupeKey,
        authMethod: 'password',
        deviceName: req.body?.deviceName,
        devicePlatform: req.body?.devicePlatform,
        deviceModel: req.body?.deviceModel,
        appVersion: req.body?.appVersion,
      });
    }
    res.setHeader('Cache-Control', 'no-store');
    res.json({
      authenticated: true,
      ...(clientTokenResult?.token ? { clientToken: clientTokenResult.token, client: clientTokenResult.client } : {}),
    });
  };

  /**
   * passkey 错误响应：优先使用 error.statusCode，缺省 400。
   */
  const respondPasskeyError = (res, error) => {
    const statusCode = typeof error?.statusCode === 'number' ? error.statusCode : 400;
    res.status(statusCode).json({ error: error?.message || 'Passkey request failed' });
  };

  /**
   * passkey 状态查询：是否启用、已注册数量与 rpID。
   */
  const handlePasskeyStatus = (req, res) => {
    try {
      res.json(passkeyController.getStatus(req));
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 发起 passkey 注册：body.label 作为凭据标签，返回 WebAuthn 注册 options。
   */
  const handlePasskeyRegistrationOptions = async (req, res) => {
    try {
      const label = typeof req.body?.label === 'string' ? req.body.label : '';
      const options = await passkeyController.beginRegistration(req, { label });
      res.json(options);
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 完成 passkey 注册：校验客户端注册响应并持久化新凭据。
   */
  const handlePasskeyRegistrationVerify = async (req, res) => {
    try {
      const result = await passkeyController.finishRegistration(req.body);
      res.json(result);
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 发起 passkey 认证：返回 WebAuthn 断言请求 options。
   */
  const handlePasskeyAuthenticationOptions = async (req, res) => {
    try {
      const options = await passkeyController.beginAuthentication(req);
      res.json(options);
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 完成 passkey 认证：校验断言后按“信任设备”签发会话，
   * 可选同步签发 client token（authMethod: 'passkey'）。
   */
  const handlePasskeyAuthenticationVerify = async (req, res) => {
    try {
      await passkeyController.finishAuthentication(req.body);
      const trustDevice = isTrustedDeviceRequest(req.body?.trustDevice);
      const ttlMs = resolveSessionTtlMs(trustDevice);
      await issueSession(req, res, { trustDevice });
      let clientTokenResult = null;
      if (req.body?.issueClientToken === true && typeof clientAuthController?.createClient === 'function') {
        clientTokenResult = await clientAuthController.createClient({
          fallbackLabel: req.body?.clientLabel,
          expiresAt: new Date(Date.now() + ttlMs).toISOString(),
          clientKind: req.body?.clientKind,
          dedupeKey: req.body?.dedupeKey,
          authMethod: 'passkey',
          deviceName: req.body?.deviceName,
          devicePlatform: req.body?.devicePlatform,
          deviceModel: req.body?.deviceModel,
          appVersion: req.body?.appVersion,
        });
      }
      res.json({
        authenticated: true,
        ...(clientTokenResult?.token ? { clientToken: clientTokenResult.token, client: clientTokenResult.client } : {}),
      });
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 列出当前已注册的 passkey 凭据。
   */
  const handlePasskeyList = (req, res) => {
    try {
      res.json({ passkeys: passkeyController.listPasskeys(req) });
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 按 params.id 吊销指定的 passkey 凭据。
   */
  const handlePasskeyRevoke = (req, res) => {
    try {
      const result = passkeyController.revokePasskey(req, req.params?.id);
      res.json(result);
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 重置认证（全局登出）：清空全部 passkey、轮换 JWT 密钥作废所有会话与
   * URL token、清除会话 cookie，并报告清理数量。
   */
  const handleResetAuth = (req, res) => {
    try {
      const passkeyResult = passkeyController.clearAllPasskeys();
      rotateJwtSecret();
      clearSessionCookie(req, res);
      res.json({
        cleared: true,
        clearedPasskeys: passkeyResult.clearedCount,
        signedOutEverywhere: true,
      });
    } catch (error) {
      respondPasskeyError(res, error);
    }
  };

  /**
   * 释放资源：清空限流记录表、停止周期清理定时器、销毁 passkey 控制器。
   */
  const dispose = () => {
    loginRateLimiter.clear();
    if (rateLimitCleanupTimer) {
      clearInterval(rateLimitCleanupTimer);
      rateLimitCleanupTimer = null;
    }
    passkeyController.dispose();
  };

  // 启用态：暴露中间件与全部认证路由处理器。
  return {
    enabled: true,
    requireAuth,
    requireSessionAuth,
    resolveAuthContext,
    handleSessionStatus,
    handleSessionCreate,
    handleUrlAuthToken,
    handlePasskeyStatus,
    handlePasskeyRegistrationOptions,
    handlePasskeyRegistrationVerify,
    handlePasskeyAuthenticationOptions,
    handlePasskeyAuthenticationVerify,
    handlePasskeyList,
    handlePasskeyRevoke,
    handleResetAuth,
    ensureSessionToken: async (req, _res) => {
      return resolveAuthenticatedSessionToken(req);
    },
    dispose,
  };
};
