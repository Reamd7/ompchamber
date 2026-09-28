/**
 * Linear GraphQL API 客户端模块。
 *
 * 封装对 Linear GraphQL 端点的请求（bearer token 认证、错误统一归一为 LinearApiError）、
 * viewer 身份解析，以及 access token 的自动刷新：token 过期时用 refresh token 换新
 * （同一 workspace 的并发刷新合并为一次），刷新失败则清除本地凭据。
 */
import {
  getLinearAuth,
  getLinearAuthByWorkspaceId,
  setLinearAuth,
  clearLinearAuth,
  isLinearAccessTokenStale,
} from './auth.js';
import { refreshAccessToken } from './oauth.js';
import { isPlainObject, readTrimmedString } from './parse.js';

/** Linear GraphQL API 端点地址。 */
const LINEAR_GRAPHQL_URL = 'https://api.linear.app/graphql';
/** 查询当前登录用户（viewer）及其所属组织的 GraphQL 查询。 */
const VIEWER_QUERY = '{ viewer { id name displayName email avatarUrl } organization { id name urlKey } }';
/** 响应中文件 URL 的有效期（秒），经 public-file-urls-expire-in 请求头传给 Linear。 */
// Linear file URLs in GraphQL need this header or the browser cannot load
// uploads.linear.app images (comment screenshots, description images).
const LINEAR_PUBLIC_FILE_URL_TTL_SECONDS = '3600';

/**
 * Linear API 请求统一错误类型：status 为映射后的 HTTP 状态码
 * （用户输入问题 400、上游异常 502、token 失效 401）；
 * userError 为 true 表示错误由调用方输入引起，路由层据此返回 400。
 */
export class LinearApiError extends Error {
  /**
   * @param {string} message 人类可读的错误信息
   * @param {number} status 映射后的 HTTP 状态码
   * @param {{ userError?: boolean }} options userError 为 true 时标记为用户输入错误
   */
  constructor(message, status, options = {}) {
    super(message);
    this.name = 'LinearApiError';
    this.status = status;
    this.userError = options.userError === true;
  }
}

/**
 * 从 GraphQL 错误响应中提取可展示的错误信息：优先 extensions.userPresentableMessage，
 * 其次校验约束（constraints）文案，最后回退原始 message；再依据 extensions.code 与
 * 文案特征（entity not found、argument validation 等）判断是否用户输入错误。
 * @returns {{ message: string, userError: boolean, status: number }} 用户错误映射 400，其余 502
 */
function readGraphqlError(payload) {
  const errors = Array.isArray(payload.errors) ? payload.errors : [];
  const first = errors.length > 0 && isPlainObject(errors[0]) ? errors[0] : null;
  if (!first) {
    return { message: '', userError: false, status: 502 };
  }
  const extensions = isPlainObject(first.extensions) ? first.extensions : null;
  const presentable = extensions ? readTrimmedString(extensions.userPresentableMessage) : '';
  let constraint = '';
  const validationErrors = extensions && Array.isArray(extensions.validationErrors)
    ? extensions.validationErrors
    : [];
  for (const entry of validationErrors) {
    if (!isPlainObject(entry) || !isPlainObject(entry.constraints)) continue;
    for (const value of Object.values(entry.constraints)) {
      const text = readTrimmedString(value);
      if (text) {
        constraint = text;
        break;
      }
    }
    if (constraint) break;
  }
  const message = presentable || constraint || readTrimmedString(first.message);
  const code = extensions ? readTrimmedString(extensions.code) : '';
  const userError = extensions?.userError === true
    || code === 'INVALID_INPUT'
    || code === 'INPUT_ERROR'
    || /^entity not found/i.test(message)
    || /^argument validation/i.test(message);
  return {
    message,
    userError,
    status: userError ? 400 : 502,
  };
}

/**
 * 从 viewer 查询响应中解析用户与组织身份：缺少有效 viewer.id 时返回 null；
 * 组织缺少 id 或 name 时 organization 置为 null。
 * @returns {{ user: object, organization: object | null } | null}
 */
function readIdentity(payload) {
  const data = isPlainObject(payload) ? payload.data : null;
  const viewer = isPlainObject(data) ? data.viewer : null;
  if (!isPlainObject(viewer) || !readTrimmedString(viewer.id)) {
    return null;
  }
  const organization = isPlainObject(data) ? data.organization : null;
  const organizationId = isPlainObject(organization) ? readTrimmedString(organization.id) : '';
  const organizationName = isPlainObject(organization) ? readTrimmedString(organization.name) : '';
  return {
    user: {
      id: viewer.id.trim(),
      name: readTrimmedString(viewer.name) || null,
      displayName: readTrimmedString(viewer.displayName) || null,
      email: readTrimmedString(viewer.email) || null,
      avatarUrl: readTrimmedString(viewer.avatarUrl) || null,
    },
    organization: organizationId && organizationName
      ? {
        id: organizationId,
        name: organizationName,
        urlKey: readTrimmedString(organization.urlKey) || null,
      }
      : null,
  };
}

/**
 * 执行一次 Linear GraphQL 请求（POST + bearer token）。
 * 请求头附带 public-file-urls-expire-in，使响应中的文件 URL 在有效期内可被浏览器直接加载。
 * 无 token 抛 401；HTTP 401、非 2xx、响应非 JSON、缺少 data 分别抛对应 LinearApiError，
 * data 缺失时经 readGraphqlError 归一错误信息与状态码。
 * @param {string} accessToken Linear access token
 * @param {string} query GraphQL 查询字符串
 * @param {object} [variables] 可选的查询变量
 * @returns {Promise<object>} 响应中的 data 对象
 */
export async function fetchLinearGraphql(accessToken, query, variables) {
  const token = readTrimmedString(accessToken);
  if (!token) {
    throw new LinearApiError('Linear is not connected', 401);
  }

  const body = { query };
  if (isPlainObject(variables)) {
    body.variables = variables;
  }

  const response = await fetch(LINEAR_GRAPHQL_URL, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
      'public-file-urls-expire-in': LINEAR_PUBLIC_FILE_URL_TTL_SECONDS,
    },
    body: JSON.stringify(body),
  });
  const payload = await response.json().catch(() => null);
  if (response.status === 401) {
    throw new LinearApiError('Linear token expired or revoked', 401);
  }
  if (!response.ok) {
    throw new LinearApiError(`Linear GraphQL request failed (${response.status})`, response.status);
  }
  if (!isPlainObject(payload)) {
    throw new LinearApiError('Linear GraphQL response was not JSON', 502);
  }
  const data = isPlainObject(payload.data) ? payload.data : null;
  if (!data) {
    const graphqlError = readGraphqlError(payload);
    throw new LinearApiError(
      graphqlError.message || 'Linear GraphQL response did not include data',
      graphqlError.status,
      { userError: graphqlError.userError },
    );
  }
  return data;
}

/**
 * 拉取当前 token 对应的 Linear 用户与组织身份（执行 VIEWER_QUERY）。
 * @param {string} accessToken Linear access token
 * @returns {Promise<{ user: object, organization: object | null }>}
 * @throws {LinearApiError} 响应缺少有效 viewer 时抛 502
 */
export async function fetchLinearIdentity(accessToken) {
  const data = await fetchLinearGraphql(accessToken, VIEWER_QUERY);
  const identity = readIdentity({ data });
  if (!identity) {
    throw new LinearApiError('Linear GraphQL response did not include a viewer', 502);
  }
  return identity;
}

/** 以 workspaceId 为键的进行中 token 刷新 promise 表，合并同一 workspace 的并发刷新。 */
const inFlightRefreshByWorkspace = new Map();

/**
 * 用授权条目中的 refresh token 刷新令牌，并把新 token 原位写回对应 workspace
 * （activate: false，不改变当前激活状态），返回新的 access token。
 * @param {object} auth 现有授权条目
 * @returns {Promise<string>} 刷新后的 access token
 */
async function refreshWorkspaceAuth(auth) {
  const tokens = await refreshAccessToken(auth.refreshToken);
  const next = setLinearAuth({
    accessToken: tokens.accessToken,
    refreshToken: tokens.refreshToken || auth.refreshToken,
    tokenType: tokens.tokenType,
    expiresAt: tokens.expiresAt,
    scope: tokens.scope || auth.scope,
    user: auth.user,
    organization: auth.organization,
    workspaceId: auth.workspaceId,
  }, { activate: false });
  return next.accessToken;
}

/**
 * 获取指定 workspace（缺省为当前激活项）的有效 access token：
 * 未过期直接返回；已过期且持有 refresh token 时执行刷新（并发去重），
 * 刷新遇到 INVALID_GRANT、400 或 401 则清除该 workspace 凭据并返回 null；
 * 已过期但无 refresh token 的条目同样直接清除并返回 null。
 * @param {string} [workspaceId] 可选的 workspace 标识
 * @returns {Promise<string | null>} 有效 token，或未连接/刷新失败时的 null
 */
export async function getValidLinearAccessToken(workspaceId) {
  const auth = workspaceId
    ? getLinearAuthByWorkspaceId(workspaceId)
    : getLinearAuth();
  if (!auth?.accessToken) {
    return null;
  }
  if (!isLinearAccessTokenStale(auth.expiresAt)) {
    return auth.accessToken;
  }
  if (!auth.refreshToken) {
    clearLinearAuth(auth.workspaceId);
    return null;
  }
  const key = auth.workspaceId;
  const pending = inFlightRefreshByWorkspace.get(key);
  if (pending) {
    return pending;
  }
  const promise = refreshWorkspaceAuth(auth)
    .catch((error) => {
      if (error?.code === 'INVALID_GRANT' || error?.status === 400 || error?.status === 401) {
        clearLinearAuth(auth.workspaceId);
        return null;
      }
      throw error;
    })
    .finally(() => {
      inFlightRefreshByWorkspace.delete(key);
    });
  inFlightRefreshByWorkspace.set(key, promise);
  return promise;
}
