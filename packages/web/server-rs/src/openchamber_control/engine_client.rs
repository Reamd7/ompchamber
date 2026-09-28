//! Port of the `server/lib/opencode/local-engine-client.js` surface the
//! control service uses, plus the `getClient` preamble from
//! `openchamber-control/service.js` (`waitForOpenCodeReady(10_000, 250)`
//! then a client over `buildOpenCodeUrl('/', '')` with the OpenCode auth
//! headers).
//!
//! Same result convention as the JS client: every call resolves to
//! `{ data, error?, response }` and never throws for HTTP outcomes —
//! transport failures become `error: { name: 'UnknownError', message }`.
//! The trait additionally distinguishes a thrown double (JS tests mock
//! rejections, e.g. the failing per-directory status lookup).
//! （中文说明）本文件移植 JS `server/lib/opencode/local-engine-client.js`
//! 中控制服务用到的接口面，外加 `openchamber-control/service.js` 里
//! `getClient` 的前置逻辑：先 `waitForOpenCodeReady(10_000, 250)`，再用
//! OpenCode 鉴权头构造基于 `buildOpenCodeUrl('/', '')` 的客户端。结果约定
//! 与 JS 客户端一致：每次调用都解析为 `{ data, error?, response }`，
//! HTTP 层面的失败不抛异常——传输错误折叠为
//! `error: { name: 'UnknownError', message }`；trait 额外区分"抛出的异常"
//! （JS 测试会 mock 拒绝，例如按目录查状态失败的用例）。

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::engine::EngineState;

use super::error::ControlError;

/// 装箱的共享 future：trait 对象方法签名统一使用的返回值类型别名。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `error` half of the client result convention.
/// 对应 JS 结果约定中 `error` 的 `{ name, message }` 形状。
#[derive(Debug, Clone)]
pub struct EngineCallError {
    /// 错误名（JS 侧的 `error.name`，无法识别时为 `UnknownError`）。
    pub name: String,
    /// 错误消息文本。
    pub message: String,
}

/// `{ data, error?, response }` — `data` is `None` for both JSON `null`
/// and the error path (the service layer checks shape, not presence).
/// 对应 JS 结果约定中的 `{ data, error?, response }`：`data` 在 JSON 为
/// `null` 与出错路径下均为 `None`（服务层只看形状，不看是否存在）。
#[derive(Debug, Clone)]
pub struct EngineResponse {
    /// 响应体解析出的 JSON；空体或解析失败为 `None`。
    pub data: Option<Value>,
    /// 出错时的错误半边；成功路径为 `None`。
    pub error: Option<EngineCallError>,
}

/// 成功路径的便捷构造。
impl EngineResponse {
    /// 构造一个无错误的响应。
    fn ok(data: Option<Value>) -> Self {
        Self { data, error: None }
    }
}

/// The local-engine-client surface consumed by the control service. An
/// `Err` is a thrown exception (test doubles may reject, like the JS
/// suite's `mockRejectedValueOnce`; the honest client folds every
/// failure into `error` and always resolves).
/// 控制服务消费的本地 engine 客户端接口面；`Err` 表示抛出的异常
///（测试替身可能拒绝，对应 JS 套件的 `mockRejectedValueOnce`；诚实客户端
/// 把一切失败折叠进 `error` 并总是成功返回）。
pub trait EngineClient: Send + Sync {
    /// `client.session.list(params)` — `GET /session?directory=…`.
    /// 目录为 `None` 时不带查询参数；HTTP 失败折叠为 `error` 而非 `Err`。
    fn session_list<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.session.status({ directory })` — `GET /session/status?directory=…`.
    /// 不带目录时查询 engine 的默认目录；支撑 `session.status` 动作。
    fn session_status<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.session.messages({ sessionID, directory, limit })` —
    /// `GET /session/:id/message?directory=…&limit=…`.
    /// `session_id` 先做 URL 编码再拼进路径；`limit` 缺省由 engine 决定。
    fn session_messages<'a>(
        &'a self,
        session_id: &'a str,
        directory: Option<&'a str>,
        limit: Option<u64>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.experimental.session.list({})` — `GET /experimental/session`.
    /// 走 engine 的实验性会话列表端点，供需要全量会话的场景使用。
    fn experimental_session_list<'a>(&'a self) -> BoxFut<'a, Result<EngineResponse, String>>;
}

/// `createClient({ baseUrl: buildOpenCodeUrl('/', ''), headers:
/// getOpenCodeAuthHeaders() })` — the honest engine-backed client over
/// the shared reqwest handle.
/// 诚实实现：经共享 reqwest 句柄访问本地 engine，等价于 JS 的
/// `createClient({ baseUrl, headers: getOpenCodeAuthHeaders() })`。
pub struct HttpEngineClient {
    /// 共享的 engine 状态：提供 HTTP 句柄与鉴权头。
    engine: Arc<EngineState>,
    /// engine 基础 URL（已去尾斜杠），请求路径直接拼接其后。
    base_url: String,
}

/// 构造器与 GET 请求实现。
impl HttpEngineClient {
    /// 以 engine 状态与基础 URL 构造客户端。
    pub fn new(engine: Arc<EngineState>, base_url: String) -> Self {
        Self { engine, base_url }
    }

    /// `request(method, pathname, { query })` for GETs: no body, auth
    /// header attached, `{data, error}` convention preserved.
    /// 查询对为空值时直接省略该参数；非 2xx 时优先从响应体的
    /// `data.message` 或 `message` 提取错误消息，取不到则回退
    /// "GET {path} failed with {status}"；传输失败折叠为 `UnknownError`。
    /// 注意：绝不记录鉴权头或带凭据的 URL。
    async fn get(&self, pathname: &str, query: &[(&str, Option<String>)]) -> EngineResponse {
        let mut url = format!("{}{}", self.base_url, pathname);
        let pairs: Vec<String> = query
            .iter()
            .filter_map(|(key, value)| {
                value
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .map(|v| format!("{}={}", key, urlencode(v)))
            })
            .collect();
        if !pairs.is_empty() {
            url.push('?');
            url.push_str(&pairs.join("&"));
        }

        let mut request = self.engine.http().get(&url);
        if let Some(auth) = self.engine.auth_header() {
            request = request.header("authorization", auth);
        }
        // NEVER log the auth header or the URL with credentials.
        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let text = response.text().await.unwrap_or_default();
                if !(200..300).contains(&status) {
                    let payload: Option<Value> = if text.is_empty() {
                        None
                    } else {
                        serde_json::from_str(&text).ok()
                    };
                    let message = payload
                        .as_ref()
                        .and_then(|p| p.get("data").and_then(|d| d.get("message")))
                        .and_then(Value::as_str)
                        .or_else(|| {
                            payload
                                .as_ref()
                                .and_then(|p| p.get("message"))
                                .and_then(Value::as_str)
                        })
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("GET {pathname} failed with {status}"));
                    return EngineResponse {
                        data: None,
                        error: Some(EngineCallError {
                            name: payload
                                .as_ref()
                                .and_then(|p| p.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("UnknownError")
                                .to_string(),
                            message,
                        }),
                    };
                }
                let data: Option<Value> = if text.is_empty() {
                    None
                } else {
                    serde_json::from_str(&text).ok()
                };
                EngineResponse::ok(data)
            }
            Err(error) => EngineResponse {
                data: None,
                error: Some(EngineCallError {
                    name: "UnknownError".to_string(),
                    message: error.to_string(),
                }),
            },
        }
    }
}

/// `encodeURIComponent` for query values.
/// 字符集与 JS `encodeURIComponent` 逐字节对齐：保留
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )`，其余字节编码为大写 `%HH`。
fn urlencode(value: &str) -> String {
    // 大写十六进制表，与 encodeURIComponent 的 %HH 输出一致。
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// 四个 trait 方法都委托给 `get`，按各自端点拼接路径与查询参数；
/// 诚实实现永远返回 `Ok`（失败折叠在 `EngineResponse::error` 里）。
impl EngineClient for HttpEngineClient {
    /// `GET /session?directory=…`；directory 缺省时省略查询参数。
    fn session_list<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>> {
        let query = [("directory", directory.map(str::to_string))];
        Box::pin(async move { Ok(self.get("/session", &query).await) })
    }

    /// `GET /session/status?directory=…`。
    fn session_status<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>> {
        let query = [("directory", directory.map(str::to_string))];
        Box::pin(async move { Ok(self.get("/session/status", &query).await) })
    }

    /// `session_id` 先 URL 编码再拼进 `GET /session/:id/message`，
    /// directory 与 limit 一并透传。
    fn session_messages<'a>(
        &'a self,
        session_id: &'a str,
        directory: Option<&'a str>,
        limit: Option<u64>,
    ) -> BoxFut<'a, Result<EngineResponse, String>> {
        let path = format!("/session/{}/message", urlencode(session_id));
        let query = [
            ("directory", directory.map(str::to_string)),
            ("limit", limit.map(|value| value.to_string())),
        ];
        Box::pin(async move { Ok(self.get(&path, &query).await) })
    }

    /// `GET /experimental/session`，无查询参数。
    fn experimental_session_list<'a>(&'a self) -> BoxFut<'a, Result<EngineResponse, String>> {
        Box::pin(async move { Ok(self.get("/experimental/session", &[]).await) })
    }
}
/// `getClient()` — wait for the engine, then hand back a client. Mirrors
/// `waitForOpenCodeReady(10_000, 250)` throwing `'OpenCode port is not
/// available'` / `'Timed out waiting for OpenCode to become ready'`, both
/// surfacing as 500 control errors through `asControlError`.
/// 客户端工厂闭包类型：等待 engine 就绪后交回一个客户端；两条失败路径
///（端口不可用 / 等待超时）都以 500 控制错误暴露。
pub type ClientFactory =
    Arc<dyn Fn() -> BoxFut<'static, Result<Arc<dyn EngineClient>, ControlError>> + Send + Sync>;

/// 对应 JS 的 `getClient()`：取 engine 基础 URL（缺失或为空报
/// "OpenCode port is not available"），等待就绪（10 秒超时，超时报
/// "Timed out waiting for OpenCode to become ready"），成功则返回基于该
/// URL 的 `HttpEngineClient`。
pub fn engine_client_factory(engine: Arc<EngineState>) -> ClientFactory {
    Arc::new(move || {
        let engine = Arc::clone(&engine);
        Box::pin(async move {
            let Some(base_url) = engine.base_url() else {
                return Err(ControlError::internal("OpenCode port is not available"));
            };
            let base = base_url.trim_end_matches('/');
            if base.is_empty() {
                return Err(ControlError::internal("OpenCode port is not available"));
            }
            if engine.wait_ready(Duration::from_secs(10)).await.is_err() {
                return Err(ControlError::internal(
                    "Timed out waiting for OpenCode to become ready",
                ));
            }
            Ok(
                Arc::new(HttpEngineClient::new(Arc::clone(&engine), base.to_string()))
                    as Arc<dyn EngineClient>,
            )
        })
    })
}

/// URL 编码与客户端工厂行为的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：`urlencode` 与 JS `encodeURIComponent` 输出一致（空格、路径
    /// 分隔符、多字节字符均按预期转义）。
    #[test]
    fn urlencode_matches_encodeuricomponent() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("ses_1"), "ses_1");
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    /// 验证：engine 无有效 base URL 时工厂立即报 "OpenCode port is not
    /// available"，不进入就绪等待。
    #[tokio::test]
    async fn factory_reports_missing_engine_immediately() {
        // An engine state with no base URL answers like a server whose
        // OpenCode never booted: `state.openCodePort` is unset.
        let engine = EngineState::external(String::new(), None);
        // base_url() is Some("") here; the empty base must be rejected.
        let factory = engine_client_factory(engine);
        let error = factory()
            .await
            .err()
            .expect("empty base url must not produce a client");
        assert_eq!(error.message, "OpenCode port is not available");
    }

    /// 验证：外部 engine 就绪时工厂产出可用客户端，且传输失败折叠为
    /// `UnknownError` 而不是抛出。
    #[tokio::test]
    async fn factory_builds_a_client_for_a_ready_external_engine() {
        let engine = EngineState::external("http://127.0.0.1:4096".to_string(), None);
        let factory = engine_client_factory(engine);
        let client = factory().await.ok().expect("client");
        // Trait object sanity: the experimental listing folds transport
        // failure into the error half instead of throwing.
        let response = client
            .experimental_session_list()
            .await
            .expect("honest client never throws");
        assert!(response.data.is_none());
        assert_eq!(
            response.error.expect("transport error").name,
            "UnknownError"
        );
    }
}
