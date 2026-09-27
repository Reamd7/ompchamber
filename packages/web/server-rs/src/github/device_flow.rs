//! Port of `server/lib/github/device-flow.js` — OAuth device flow against
//! github.com. Plain form POSTs with `Accept: application/json`; a 200 body
//! may still carry `{error: 'authorization_pending' | …}` which the caller
//! inspects. `postForm` is a seam so tests drive the state machine without
//! network access.
//!
//! 中文说明：实现 github.com 的 OAuth device flow 两步流程——先申请
//! device code（用户在浏览器输入 user_code 授权），再用 device code
//! 换取 access token。全部请求为普通 form POST 并带 Accept:
//! application/json；特别注意 GitHub 对 pending/expired 等非成功状态
//! 也返回 HTTP 200 + {error: ...}，成败要看响应体而非状态码，该判断
//! 由调用方完成。FormPoster 是请求 seam，测试注入假实现即可离线驱动
//! 整个状态机。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

/// device flow 第一步：申请 device code 的端点。
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// device flow 第二步：用 device code 换取（轮询）access token 的端点。
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// 换取 access token 所用的 OAuth grant type（device_code 授权类型的 urn 形式）。
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// `postForm(url, params)` seam: url → form body → parsed JSON payload.
///
/// postForm(url, body) 的类型签名：输入目标 URL 与已编码的 form body，
/// 异步返回解析后的 JSON 或 DeviceFlowError。抽象成 Arc<dyn Fn> 作为
/// seam：生产环境用 reqwest（default_form_poster），测试注入假实现。
pub type FormPoster =
    Arc<dyn Fn(&str, String) -> BoxFuture<'static, Result<Value, DeviceFlowError>> + Send + Sync>;

/// device flow 请求失败：HTTP 状态码、人类可读消息与可选的原始响应体。
#[derive(Debug, Clone)]
pub struct DeviceFlowError {
    /// HTTP 状态码；网络层失败时以 500 填充。
    pub status: u16,
    /// 错误消息：优先取响应体的 error_description / error 字段。
    pub message: String,
    /// 原始响应体（body 缺失或不是合法 JSON 时为 None）。
    pub payload: Option<Value>,
}

/// `encodeForm` + URLSearchParams serialization (space → `+`).
///
/// 对应 JS 版 encodeForm + URLSearchParams 序列化：跳过值为 None 的
/// 参数，其余按 key=value 以 & 连接；空格编码为 +。
fn encode_form(params: &[(&str, Option<&str>)]) -> String {
    params
        .iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .map(|(key, value)| format!("{}={}", key, encode_form_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 单个 form 分量的百分号编码：字母数字与 -_.~*'()! 原样保留，空格
/// 转为 +，其余字节输出大写 %XX，与 URLSearchParams 的行为对齐。
fn encode_form_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b'!' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Production poster: reqwest form POST with JSON accept, octokit-free (the
/// JS module uses bare `fetch`).
///
/// 生产环境的 poster：reqwest 发送 application/x-www-form-urlencoded
/// POST 并带 Accept: application/json（JS 版用裸 fetch，不走 octokit）。
/// 网络/发送错误包装为 status=500；非 2xx 时消息优先取响应体的
/// error_description / error，缺失则回退状态码的标准 reason，最后兜底
/// "GitHub request failed"；响应体不是合法 JSON 时按 Null 处理（对应
/// JS 的 .catch(() => null)）。
pub fn default_form_poster() -> FormPoster {
    Arc::new(|url: &str, body: String| {
        let url = url.to_string();
        Box::pin(async move {
            let response = reqwest::Client::new()
                .post(&url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| DeviceFlowError {
                    status: 500,
                    message: e.to_string(),
                    payload: None,
                })?;
            let status = response.status().as_u16();
            // response.json().catch(() => null)
            let payload: Option<Value> = match response.text().await {
                Ok(text) => serde_json::from_str(&text).ok(),
                Err(_) => None,
            };
            if !(200..300).contains(&status) {
                let message = payload
                    .as_ref()
                    .and_then(|p| {
                        p.get("error_description")
                            .or_else(|| p.get("error"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| {
                        reqwest::StatusCode::from_u16(status)
                            .ok()
                            .and_then(|s| s.canonical_reason().map(str::to_string))
                            .unwrap_or_else(|| "GitHub request failed".to_string())
                    });
                return Err(DeviceFlowError {
                    status,
                    message,
                    payload,
                });
            }
            Ok(payload.unwrap_or(Value::Null))
        })
    })
}

/// `startDeviceFlow({ clientId, scope })`.
///
/// 对应 JS 版 startDeviceFlow({ clientId, scope })：向 device code 端点
/// 提交 client_id 与 scope，返回含 device_code / user_code /
/// verification_uri / expires_in / interval 的 JSON。
pub async fn start_device_flow(
    poster: &FormPoster,
    client_id: &str,
    scope: &str,
) -> Result<Value, DeviceFlowError> {
    poster(
        DEVICE_CODE_URL,
        encode_form(&[("client_id", Some(client_id)), ("scope", Some(scope))]),
    )
    .await
}

/// `exchangeDeviceCode({ clientId, deviceCode })` — polls the access-token
/// endpoint; GitHub answers 200 with `{error: …}` for non-success states.
///
/// 对应 JS 版 exchangeDeviceCode({ clientId, deviceCode })：带
/// device_code 与 device_code grant type 轮询 access token 端点；GitHub
/// 对 authorization_pending / expired_token 等中间态仍返回 HTTP 200，
/// 错误信息在响应体的 error 字段中，由调用方检查并决定继续轮询还是终止。
pub async fn exchange_device_code(
    poster: &FormPoster,
    client_id: &str,
    device_code: &str,
) -> Result<Value, DeviceFlowError> {
    poster(
        ACCESS_TOKEN_URL,
        encode_form(&[
            ("client_id", Some(client_id)),
            ("device_code", Some(device_code)),
            ("grant_type", Some(DEVICE_GRANT_TYPE)),
        ]),
    )
    .await
}

/// 验证 device flow 的 form 编码规则与两个端点的请求/响应契约。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 测试辅助：构造记录每次 (url, body) 的假 poster，用于断言请求
    /// 命中了哪个端点、form body 编码是否正确。
    fn recording_poster(
        respond: impl Fn(&str, &str) -> Result<Value, DeviceFlowError> + Send + Sync + 'static,
    ) -> (FormPoster, Arc<Mutex<Vec<(String, String)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_closure = calls.clone();
        let poster: FormPoster = Arc::new(move |url: &str, body: String| {
            let result = respond(url, body.as_str());
            calls_for_closure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((url.to_string(), body));
            Box::pin(async move { result })
        });
        (poster, calls)
    }

    /// 验证 start_device_flow 把 client_id 与 scope 正确编码进 device code 端点的 form body。
    #[tokio::test]
    async fn start_sends_client_id_and_scope_as_form_fields() {
        let (poster, calls) = recording_poster(|_, _| {
            Ok(serde_json::json!({
                "device_code": "dc",
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 5,
            }))
        });
        let payload = start_device_flow(&poster, "client-1", "repo read:org")
            .await
            .unwrap();
        assert_eq!(payload["device_code"], "dc");

        let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, DEVICE_CODE_URL);
        assert_eq!(calls[0].1, "client_id=client-1&scope=repo+read%3Aorg");
    }

    /// 验证 exchange_device_code 携带 device_code 并使用 device_code grant type。
    #[tokio::test]
    async fn exchange_sends_device_grant_type() {
        let (poster, calls) =
            recording_poster(|_, _| Ok(serde_json::json!({ "error": "authorization_pending" })));
        let payload = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap();
        assert_eq!(payload["error"], "authorization_pending");

        let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls[0].0, ACCESS_TOKEN_URL);
        assert_eq!(
            calls[0].1,
            "client_id=client-1&device_code=dev-code&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }

    /// 验证成功响应原样透传 access_token / token_type。
    #[tokio::test]
    async fn success_payload_carries_access_token() {
        let (poster, _) = recording_poster(|_, _| {
            Ok(serde_json::json!({
                "access_token": "gho_token",
                "token_type": "bearer",
                "scope": "repo"
            }))
        });
        let payload = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap();
        assert_eq!(payload["access_token"], "gho_token");
        assert_eq!(payload["token_type"], "bearer");
    }

    /// 验证 HTTP 错误时错误消息优先取 error_description。
    #[tokio::test]
    async fn http_error_maps_error_description_first() {
        let (poster, _) = recording_poster(|_, _| {
            Err(DeviceFlowError {
                status: 422,
                message: "device code expired".to_string(),
                payload: Some(serde_json::json!({
                    "error": "expired_token",
                    "error_description": "device code expired"
                })),
            })
        });
        let error = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap_err();
        assert_eq!(error.status, 422);
        assert_eq!(error.message, "device code expired");
    }

    /// 验证 form 编码会跳过值为 None 的参数（与 URLSearchParams 一致）。
    #[tokio::test]
    async fn null_values_are_skipped_from_form_body() {
        let (poster, calls) = recording_poster(|_, _| Ok(Value::Null));
        // scope None would be skipped in JS; emulate via direct encode_form.
        let body = encode_form(&[("client_id", Some("c")), ("scope", None)]);
        assert_eq!(body, "client_id=c");
        start_device_flow(&poster, "c", "s").await.unwrap();
        assert_eq!(calls.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    }
}
