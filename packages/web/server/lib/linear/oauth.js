/**
 * Linear OAuth 2.0 + PKCE 授权流程模块。
 *
 * 负责生成授权 URL、维护待完成授权（pending）状态、处理 Linear 回调以及交换/刷新/撤销
 * token。支持两种回调模式：直接使用配置的 redirect_uri，或经云端 broker 中转
 * （供桌面端等无公网回调地址的场景使用：/start 注册事务、/poll 轮询结果、/complete 确认）。
 */
import crypto from 'crypto';
import {
  getLinearClientId,
  getLinearClientSecret,
  getLinearBrokerUrl,
  getLinearRedirectUri,
  getLinearScopes,
} from './auth.js';
import { isPlainObject, isString, readFiniteNumber, readTrimmedString } from './parse.js';

/** Linear OAuth 用户授权页地址。 */
export const LINEAR_AUTHORIZE_URL = 'https://linear.app/oauth/authorize';
/** Linear OAuth token 端点（授权码交换与刷新均走此地址）。 */
export const LINEAR_TOKEN_URL = 'https://api.linear.app/oauth/token';
/** Linear OAuth token 撤销端点。 */
export const LINEAR_REVOKE_URL = 'https://api.linear.app/oauth/revoke';
/** 待完成授权（pending state）的存活时长（10 分钟），过期后回调将被拒绝。 */
export const PENDING_AUTHORIZATION_TTL_MS = 10 * 60_000;

/** 以 state 为键的待完成授权表（保存 code_verifier、redirect_uri、origin 与 broker 信息）。 */
const pendingByState = new Map();
/** 以 state 为键的进行中 broker 轮询 promise 表，避免同一 state 重复并发轮询。 */
const brokerPollsByState = new Map();

/**
 * Linear OAuth 流程统一错误类型：code 标识错误类别（如 UNKNOWN_STATE、MISSING_CODE、
 * LINEAR_CLIENT_ID_MISSING、LINEAR_BROKER_FAILED 或上游 error 字段的大写形式），
 * origin 属性（可选）记录发起端 desktop/web，供回调页决定是否唤起桌面端。
 */
export class LinearOAuthError extends Error {
  /**
   * @param {string} message 人类可读的错误信息
   * @param {string} [code] 机器可读错误码，默认 LINEAR_OAUTH_FAILED
   */
  constructor(message, code = 'LINEAR_OAUTH_FAILED') {
    super(message);
    this.name = 'LinearOAuthError';
    this.code = code;
  }
}

/**
 * 生成 PKCE 密钥对：verifier 为 32 字节随机数的 base64url，
 * challenge 为 verifier 的 SHA-256 哈希再 base64url（对应 S256 方法）。
 * @returns {{ verifier: string, challenge: string }} PKCE 密钥对
 */
export function createPkcePair() {
  const verifier = crypto.randomBytes(32).toString('base64url');
  const challenge = crypto.createHash('sha256').update(verifier).digest('base64url');
  return { verifier, challenge };
}

/** 清理 pendingByState 中已过期或非法的待完成授权条目，防止内存无限增长。 */
function pruneExpiredPending(now = Date.now()) {
  for (const [state, entry] of pendingByState.entries()) {
    if (!entry || entry.expiresAt <= now) {
      pendingByState.delete(state);
    }
  }
}

/**
 * 将 scope 规范化为逗号分隔字符串：字符串直接 trim，数组过滤空项后用逗号拼接，
 * 其余类型返回空字符串。
 */
function normalizeScope(scope) {
  if (isString(scope)) {
    return scope.trim();
  }
  if (Array.isArray(scope)) {
    return scope.filter((item) => isString(item) && item.trim()).join(',');
  }
  return '';
}

/**
 * 将 token 响应中的 expires_in（秒）换算为绝对过期时间戳（毫秒）；
 * 缺失或非正数时按 24 小时估算。
 */
function readExpiresAt(expiresIn, now = Date.now()) {
  const seconds = readFiniteNumber(expiresIn);
  if (seconds == null || seconds <= 0) {
    return now + 24 * 60 * 60 * 1000;
  }
  return now + Math.floor(seconds) * 1000;
}

/**
 * 校验并解析 Linear token 端点响应：响应为空、携带 error 字段或缺少 access_token
 * 时抛出 LinearOAuthError；成功时返回规范化令牌信息
 * （accessToken/refreshToken/tokenType/expiresAt/scope）。
 * @param {object} payload token 端点原始响应
 */
function parseTokenPayload(payload) {
  if (!isPlainObject(payload)) {
    throw new LinearOAuthError('Linear token response was empty');
  }
  if (readTrimmedString(payload.error)) {
    throw new LinearOAuthError(
      readTrimmedString(payload.error_description) || readTrimmedString(payload.error),
      readTrimmedString(payload.error).toUpperCase(),
    );
  }
  const accessToken = readTrimmedString(payload.access_token);
  if (!accessToken) {
    throw new LinearOAuthError('Linear token response was missing access_token');
  }
  return {
    accessToken,
    refreshToken: readTrimmedString(payload.refresh_token) || null,
    tokenType: readTrimmedString(payload.token_type) || 'bearer',
    expiresAt: readExpiresAt(payload.expires_in),
    scope: normalizeScope(payload.scope),
  };
}

/**
 * 以 application/x-www-form-urlencoded 形式向 Linear token 端点发起 POST 并解析响应。
 * 非 2xx 时抛出带 status 的 LinearOAuthError（优先使用响应中的 error_description），
 * 成功时经 parseTokenPayload 返回规范化令牌。
 * @param {string} url token 端点地址
 * @param {object} body 表单字段对象
 */
async function postForm(url, body) {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/x-www-form-urlencoded',
    },
    body: new URLSearchParams(body).toString(),
  });
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    const description = isPlainObject(payload)
      ? (readTrimmedString(payload.error_description) || readTrimmedString(payload.error))
      : '';
    const error = new LinearOAuthError(
      description || `Linear token request failed (${response.status})`,
      readTrimmedString(payload?.error).toUpperCase() || 'LINEAR_OAUTH_FAILED',
    );
    error.status = response.status;
    throw error;
  }
  return parseTokenPayload(payload);
}

/**
 * 读取并校验 broker 接口的 JSON 响应：非 2xx 抛出带 status 的 LINEAR_BROKER_FAILED
 * 错误（优先使用响应中的 error 字段），2xx 但内容不是对象同样报错；成功返回解析对象。
 * @param {Response} response fetch 响应对象
 * @param {string} fallbackMessage 失败时的兜底错误文案
 */
async function readJsonResponse(response, fallbackMessage) {
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    const message = isPlainObject(payload) && readTrimmedString(payload.error)
      ? readTrimmedString(payload.error)
      : `${fallbackMessage} (${response.status})`;
    const error = new LinearOAuthError(message, 'LINEAR_BROKER_FAILED');
    error.status = response.status;
    throw error;
  }
  if (!isPlainObject(payload)) {
    throw new LinearOAuthError(`${fallbackMessage}: invalid response`, 'LINEAR_BROKER_FAILED');
  }
  return payload;
}

/** 拼接 broker 的回调地址：brokerUrl 去除尾部斜杠后追加 /callback。 */
function brokerCallbackUrl(brokerUrl) {
  return `${brokerUrl.replace(/\/+$/, '')}/callback`;
}

/**
 * 向 broker 注册一次中转授权事务（POST /start，携带 state 与 claimSecret），
 * 并校验返回的 redirectUri 与预期回调地址完全一致（防止 broker 配置漂移导致授权落空）。
 * @returns {Promise<string>} broker 提供的回调地址
 */
async function registerBrokerTransaction({ brokerUrl, state, claimSecret }) {
  const response = await fetch(`${brokerUrl}/start`, {
    method: 'POST',
    headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
    body: JSON.stringify({ state, claimSecret }),
  });
  const payload = await readJsonResponse(response, 'Could not start Linear authorization broker');
  const redirectUri = readTrimmedString(payload.redirectUri);
  if (!redirectUri || redirectUri !== brokerCallbackUrl(brokerUrl)) {
    throw new LinearOAuthError('Linear authorization broker returned an unexpected callback URL', 'LINEAR_BROKER_FAILED');
  }
  return redirectUri;
}

/**
 * 发起一次 OAuth 授权：生成 PKCE 密钥对与随机 state，并确定回调模式——
 * 当配置的 redirect_uri 恰好是 broker 回调地址时走云端中转（先注册事务换取回调地址），
 * 否则直接使用配置的本机回调。最后组装 Linear 授权 URL，并把待完成状态写入内存表。
 * @param {{ origin?: string }} [options] origin 为 'desktop' 时标记为桌面端发起
 * @returns {Promise<{ authorizationUrl: string, expiresIn: number, scope: string }>} 授权 URL 与有效期、scope
 * @throws {LinearOAuthError} 未配置 client id 时抛出 LINEAR_CLIENT_ID_MISSING
 */
export async function startAuthorization({ origin } = {}) {
  const clientId = getLinearClientId();
  if (!clientId) {
    throw new LinearOAuthError(
      'Linear OAuth client not configured. Set OPENCHAMBER_LINEAR_CLIENT_ID.',
      'LINEAR_CLIENT_ID_MISSING',
    );
  }

  pruneExpiredPending();
  const { verifier, challenge } = createPkcePair();
  const state = crypto.randomBytes(32).toString('base64url');
  const brokerUrl = getLinearBrokerUrl();
  const configuredRedirectUri = getLinearRedirectUri();
  const usesBroker = configuredRedirectUri === brokerCallbackUrl(brokerUrl);
  const claimSecret = usesBroker ? crypto.randomBytes(32).toString('base64url') : null;
  const redirectUri = usesBroker
    ? await registerBrokerTransaction({ brokerUrl, state, claimSecret })
    : configuredRedirectUri;
  const scope = getLinearScopes();
  pendingByState.set(state, {
    codeVerifier: verifier,
    redirectUri,
    origin: origin === 'desktop' ? 'desktop' : 'web',
    broker: usesBroker ? { url: brokerUrl, claimSecret } : null,
    expiresAt: Date.now() + PENDING_AUTHORIZATION_TTL_MS,
  });

  const url = new URL(LINEAR_AUTHORIZE_URL);
  url.searchParams.set('response_type', 'code');
  url.searchParams.set('client_id', clientId);
  url.searchParams.set('redirect_uri', redirectUri);
  url.searchParams.set('scope', scope);
  url.searchParams.set('state', state);
  url.searchParams.set('code_challenge', challenge);
  url.searchParams.set('code_challenge_method', 'S256');
  url.searchParams.set('actor', 'user');
  url.searchParams.set('prompt', 'consent');

  return {
    authorizationUrl: url.toString(),
    expiresIn: Math.floor(PENDING_AUTHORIZATION_TTL_MS / 1000),
    scope,
  };
}

/**
 * 轮询单个 broker 事务（POST /poll）：202 表示尚未完成，返回 null；
 * status 为 complete 时用返回的 code 走 consumeAuthorizationCallback 交换令牌，
 * 并附上 brokerReceipt（state/url/claimSecret）供后续确认；
 * status 为 failed 时以错误参数走回调失败路径；其余状态视为异常抛 LINEAR_BROKER_FAILED。
 */
async function pollBrokerState(state, pending) {
  const response = await fetch(`${pending.broker.url}/poll`, {
    method: 'POST',
    headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
    body: JSON.stringify({ state, claimSecret: pending.broker.claimSecret }),
  });
  if (response.status === 202) {
    return null;
  }
  const payload = await readJsonResponse(response, 'Could not read Linear authorization result');
  const status = readTrimmedString(payload.status);
  if (status === 'complete') {
    const result = await consumeAuthorizationCallback({ code: payload.code, state });
    return {
      ...result,
      brokerReceipt: { state, ...pending.broker },
    };
  }
  if (status === 'failed') {
    return consumeAuthorizationCallback({
      state,
      error: payload.error,
      errorDescription: payload.errorDescription,
    });
  }
  throw new LinearOAuthError('Linear authorization broker returned an unexpected result', 'LINEAR_BROKER_FAILED');
}

/**
 * 确认 broker 事务完成（POST /complete），让 broker 侧清理对应 state。
 * receipt 字段不完整直接返回 false；请求失败抛 LINEAR_BROKER_FAILED，成功返回 true。
 * @param {{ url: string, state: string, claimSecret: string }} receipt 轮询结果携带的回执
 * @returns {Promise<boolean>}
 */
export async function completeAuthorizationBroker(receipt) {
  if (!receipt?.url || !receipt?.state || !receipt?.claimSecret) return false;
  const response = await fetch(`${receipt.url}/complete`, {
    method: 'POST',
    headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
    body: JSON.stringify({ state: receipt.state, claimSecret: receipt.claimSecret }),
  });
  if (!response.ok) {
    throw new LinearOAuthError(`Could not acknowledge Linear authorization result (${response.status})`, 'LINEAR_BROKER_FAILED');
  }
  return true;
}

/**
 * 遍历所有 broker 模式的待完成授权并逐个轮询（同一 state 复用进行中的 promise 去重，
 * 完成后从表中移除），返回第一个已完成的结果；没有进行中的授权或均未完成时返回 null。
 * @returns {Promise<object | null>} 授权结果（含 brokerReceipt）或 null
 */
export async function pollAuthorizationBroker() {
  pruneExpiredPending();
  for (const [state, pending] of pendingByState.entries()) {
    if (!pending?.broker) continue;
    let poll = brokerPollsByState.get(state);
    if (!poll) {
      poll = pollBrokerState(state, pending).finally(() => brokerPollsByState.delete(state));
      brokerPollsByState.set(state, poll);
    }
    const result = await poll;
    if (result) return result;
  }
  return null;
}

/** 构造携带 origin（desktop/web）的 LinearOAuthError；origin 供回调页决定是否唤起桌面端。 */
function failAuthorization(message, code, origin) {
  const error = new LinearOAuthError(message, code);
  if (origin) {
    error.origin = origin;
  }
  return error;
}

/**
 * 消费 Linear OAuth 回调：校验 error/code/state，并用授权码 + PKCE code_verifier
 * 向 token 端点交换令牌；成功后清除对应 pending 并返回令牌与发起端 origin。
 * 回调带 error、缺少 code 或 state 未知/过期时，抛出对应错误码的 LinearOAuthError
 * （UNKNOWN_STATE/MISSING_CODE/上游 error 大写码）。
 * @param {{ code?: string, state?: string, error?: string, errorDescription?: string }} params 回调查询参数
 * @returns {Promise<object>} 规范化令牌信息（附加 origin）
 */
export async function consumeAuthorizationCallback({ code, state, error, errorDescription }) {
  pruneExpiredPending();
  const pending = readTrimmedString(state) ? pendingByState.get(state) : null;

  if (readTrimmedString(error)) {
    if (readTrimmedString(state)) pendingByState.delete(state);
    throw failAuthorization(
      readTrimmedString(errorDescription) || readTrimmedString(error),
      readTrimmedString(error).toUpperCase(),
      pending?.origin,
    );
  }
  if (!readTrimmedString(code)) {
    if (readTrimmedString(state)) pendingByState.delete(state);
    throw failAuthorization(
      'Linear did not return an authorization code.',
      'MISSING_CODE',
      pending?.origin,
    );
  }
  if (!pending?.codeVerifier) {
    throw failAuthorization(
      'This authorization session has expired or is unknown to the running app. Return to OpenChamber and click Connect again.',
      'UNKNOWN_STATE',
    );
  }

  const body = {
    grant_type: 'authorization_code',
    code: code.trim(),
    redirect_uri: pending.redirectUri,
    client_id: getLinearClientId(),
    code_verifier: pending.codeVerifier,
  };
  const clientSecret = getLinearClientSecret();
  if (clientSecret) {
    body.client_secret = clientSecret;
  }

  try {
    const tokens = await postForm(LINEAR_TOKEN_URL, body);
    pendingByState.delete(state);
    return {
      ...tokens,
      origin: pending.origin,
    };
  } catch (caught) {
    if (caught instanceof Error) {
      caught.origin = pending.origin;
    }
    throw caught;
  }
}

/**
 * 用 refresh token 向 Linear 换取新的 access token（请求带 client_id，
 * 配置了 client secret 时一并附带）。
 * @param {string} refreshToken 刷新令牌
 * @returns {Promise<object>} 规范化的新令牌信息
 * @throws {LinearOAuthError} refreshToken 缺失时抛 MISSING_REFRESH_TOKEN
 */
export async function refreshAccessToken(refreshToken) {
  const token = readTrimmedString(refreshToken);
  if (!token) {
    throw new LinearOAuthError('refresh_token is required', 'MISSING_REFRESH_TOKEN');
  }
  const body = {
    grant_type: 'refresh_token',
    refresh_token: token,
    client_id: getLinearClientId(),
  };
  const clientSecret = getLinearClientSecret();
  if (clientSecret) {
    body.client_secret = clientSecret;
  }
  return postForm(LINEAR_TOKEN_URL, body);
}

/**
 * 向 Linear 撤销指定 token（token_type_hint 仅接受 access_token 或 refresh_token）。
 * 令牌为空、网络失败或响应非 200 时返回 false——尽力而为，绝不抛错，避免阻断断开流程。
 * @param {string} token 待撤销的令牌
 * @param {string} [tokenTypeHint] 令牌类型提示
 * @returns {Promise<boolean>} 是否撤销成功
 */
export async function revokeToken(token, tokenTypeHint) {
  const value = readTrimmedString(token);
  if (!value) {
    return false;
  }
  const body = { token: value };
  if (tokenTypeHint === 'access_token' || tokenTypeHint === 'refresh_token') {
    body.token_type_hint = tokenTypeHint;
  }
  try {
    const response = await fetch(LINEAR_REVOKE_URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams(body).toString(),
    });
    return response.status === 200;
  } catch {
    return false;
  }
}

/** 测试辅助：清空全部待完成授权与进行中的 broker 轮询，保证测试用例之间互不干扰。 */
export function clearPendingAuthorizationsForTests() {
  pendingByState.clear();
  brokerPollsByState.clear();
}
