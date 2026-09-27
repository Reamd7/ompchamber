/**
 * GitHub OAuth device flow 的裸 HTTP 封装。
 *
 * 只负责与 github.com 的 device/code 与 oauth/access_token 两个端点
 * 通信，不落盘、不轮询——等待用户授权的重试循环由路由层实现。
 */
/** 发起 device flow、返回 device_code/user_code/verification_uri 的端点。 */
const DEVICE_CODE_URL = 'https://github.com/login/device/code';
/** 用 device_code 换取 access token 的端点。 */
const ACCESS_TOKEN_URL = 'https://github.com/login/oauth/access_token';
/** device flow 专用的 OAuth grant_type 标识。 */
const DEVICE_GRANT_TYPE = 'urn:ietf:params:oauth:grant-type:device_code';

/** 把参数对象编码为 application/x-www-form-urlencoded 请求体，跳过 null/undefined 值。 */
const encodeForm = (params) => {
  const body = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value == null) continue;
    body.set(key, String(value));
  }
  return body.toString();
};

/**
 * 以表单 POST 请求 GitHub OAuth 端点并解析 JSON 响应。
 * 非 2xx 时抛出携带 status 与原始 payload 的 Error（消息依次取
 * error_description、error、statusText），供上层转成用户可读的提示。
 */
async function postForm(url, params) {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/x-www-form-urlencoded',
      Accept: 'application/json',
    },
    body: encodeForm(params),
  });

  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    const message = payload?.error_description || payload?.error || response.statusText;
    const error = new Error(message || 'GitHub request failed');
    error.status = response.status;
    error.payload = payload;
    throw error;
  }
  return payload;
}

/**
 * 发起 device flow 授权：向 DEVICE_CODE_URL 提交 client_id 与 scope，
 * 返回 device_code、user_code、verification_uri 与轮询 interval 等字段，
 * 前端据此引导用户完成设备验证。
 */
export async function startDeviceFlow({ clientId, scope }) {
  return postForm(DEVICE_CODE_URL, {
    client_id: clientId,
    scope,
  });
}

/**
 * 用 device_code 向 ACCESS_TOKEN_URL 轮询换取 token。GitHub 对未完成
 * 状态同样返回 200，payload 形如 { error: 'authorization_pending' 或
 * 'slow_down' 等 }，由调用方决定继续轮询还是报错；成功时含 access_token。
 */
export async function exchangeDeviceCode({ clientId, deviceCode }) {
  // GitHub returns 200 with {error: 'authorization_pending'|...} for non-success states.
  const payload = await postForm(ACCESS_TOKEN_URL, {
    client_id: clientId,
    device_code: deviceCode,
    grant_type: DEVICE_GRANT_TYPE,
  });
  return payload;
}
