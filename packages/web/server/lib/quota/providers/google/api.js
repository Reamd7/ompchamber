/**
 * Google Provider - API
 *
 * API calls for Google quota providers.
 * @module quota/providers/google/api
 */
/**
 * Google 配额相关的 HTTP API 调用：OAuth refresh token 换 access token、
 * retrieveUserQuota 配额桶查询与 fetchAvailableModels 模型列表。所有函数
 * 都把网络/HTTP 失败归一化为 null（不抛错），由调用方决定如何降级。
 */

/** 主端点：Google Cloud Code 内部 API 的正式域名。 */
const GOOGLE_PRIMARY_ENDPOINT = 'https://cloudcode-pa.googleapis.com';

/** fetchAvailableModels 的候选端点（sandbox/autopush 优先，主端点兜底）。 */
const GOOGLE_ENDPOINTS = [
  'https://daily-cloudcode-pa.sandbox.googleapis.com',
  'https://autopush-cloudcode-pa.sandbox.googleapis.com',
  GOOGLE_PRIMARY_ENDPOINT
];

/** 伪装成官方客户端的固定请求头，与 Antigravity/Gemini 扩展一致。 */
const GOOGLE_HEADERS = {
  'User-Agent': 'antigravity/1.11.5 windows/amd64',
  'X-Goog-Api-Client': 'google-cloud-sdk vscode_cloudshelleditor/0.1',
  'Client-Metadata':
    '{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}'
};

/**
 * 用 refresh token 通过 Google OAuth token 端点换取新的 access token。
 * @param {string} refreshToken OAuth refresh token
 * @param {string} clientId 来源对应的 OAuth client_id
 * @param {string} clientSecret 来源对应的 OAuth client_secret
 * @returns {Promise<string|null>} 新的 access token；HTTP 非 2xx 或响应
 *   缺少 access_token 字段时返回 null（不抛错）
 */
export const refreshGoogleAccessToken = async (refreshToken, clientId, clientSecret) => {
  const response = await fetch('https://oauth2.googleapis.com/token', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      client_id: clientId,
      client_secret: clientSecret,
      refresh_token: refreshToken,
      grant_type: 'refresh_token'
    })
  });

  if (!response.ok) {
    return null;
  }

  const data = await response.json();
  return typeof data?.access_token === 'string' ? data.access_token : null;
};

/**
 * 查询当前用户的配额桶（v1internal:retrieveUserQuota）。携带 project 时
 * 限定到指定 Google Cloud 项目；15 秒超时。仅 gemini 来源使用。
 * @param {string} accessToken Bearer access token
 * @param {string|null} projectId 项目 ID（空则不带 project 字段）
 * @returns {Promise<object|null>} 解析后的 JSON（含 buckets 数组）；
 *   请求失败或非 2xx 时返回 null
 */
export const fetchGoogleQuotaBuckets = async (accessToken, projectId) => {
  const body = projectId ? { project: projectId } : {};

  try {
    const response = await fetch(`${GOOGLE_PRIMARY_ENDPOINT}/v1internal:retrieveUserQuota`, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${accessToken}`,
        'Content-Type': 'application/json'
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(15000)
    });

    if (!response.ok) {
      return null;
    }

    return await response.json();
  } catch {
    return null;
  }
};

/**
 * 拉取可用模型及各自的配额信息（v1internal:fetchAvailableModels）。按
 * GOOGLE_ENDPOINTS 顺序逐个尝试，每个端点 15 秒超时，首个成功的响应即
 * 返回；全部失败返回 null。
 * @param {string} accessToken Bearer access token
 * @param {string|null} projectId 项目 ID（空则不带 project 字段）
 * @returns {Promise<object|null>} 解析后的 JSON（含 models 映射）
 */
export const fetchGoogleModels = async (accessToken, projectId) => {
  const body = projectId ? { project: projectId } : {};

  for (const endpoint of GOOGLE_ENDPOINTS) {
    try {
      const response = await fetch(`${endpoint}/v1internal:fetchAvailableModels`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${accessToken}`,
          'Content-Type': 'application/json',
          ...GOOGLE_HEADERS
        },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(15000)
      });

      if (response.ok) {
        return await response.json();
      }
    } catch {
      continue;
    }
  }

  return null;
};
