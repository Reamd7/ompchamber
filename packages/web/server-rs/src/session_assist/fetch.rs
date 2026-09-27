//! Engine fetch seam shared by the three hub consumers ported in this module
//! (`session-assist`, `session-knowledge`, `context-obligatory`).
//!
//! Each JS module closes over its own `openCodeFetch(path, {directory,
//! method, body, query})` built from `buildOpenCodeUrl` +
//! `getOpenCodeAuthHeaders` + `AbortSignal.timeout`; the error strings
//! (`OpenCode ${method} ${path} failed with ${status}`, transport
//! `fetch failed`) reach route responses verbatim, so they are preserved here.
//!
//! 中文说明：本模块是 `session_assist` 三个 hub 消费者（session-assist、
//! session-knowledge、context-obligatory）共享的引擎 HTTP 接缝。每个 JS
//! 模块原先各自闭包持有 `openCodeFetch`；由于错误文案会逐字透传到路由
//! 响应，此处原样保留（见 [`OpenCodeError`]）。

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::engine::EngineState;

/// JS `error.message` of a failed engine call. `OpenCode … failed with …`
/// carries the upstream status; `fetch failed` is the undici transport error
/// (also used when the engine has no base URL yet, mirroring the JS fetch to
/// an unreachable URL).
///
/// 中文说明：引擎调用失败时对应的 JS `error.message`。`Status` 变体携带
/// 上游 HTTP 状态码；`Transport` 变体对应 undici 传输错误——引擎尚无
/// base URL 时也用它，等价于 JS 请求一个不可达 URL。
#[derive(Debug, thiserror::Error)]
pub enum OpenCodeError {
    /// 上游返回非 2xx 状态码：方法、路径、状态码原样进入错误消息。
    #[error("OpenCode {method} {path} failed with {status}")]
    Status {
        /// 请求方法（如 `GET`/`POST`），仅用于渲染错误消息。
        method: String,
        /// 请求路径（可能已含 query string），仅用于渲染错误消息。
        path: String,
        /// 上游 HTTP 状态码。
        status: u16,
    },
    /// 传输层失败（连接错误/超时，或引擎尚无 base URL），消息恒为 `fetch failed`。
    #[error("fetch failed")]
    Transport,
}

/// 引擎调用的 boxed future：成功为解析后的 JSON（响应体非 JSON 时为
/// `Value::Null`），失败为 [`OpenCodeError`]。
pub type OpenCodeFuture = Pin<Box<dyn Future<Output = Result<Value, OpenCodeError>> + Send>>;
/// `(path, directory, method, body)` — `path` may already carry a query
/// string; the implementation appends a non-empty `directory` like the JS
/// `URLSearchParams` dance.
///
/// 中文说明：调用签名为 `(path, directory, method, body)`；`path` 可自带
/// query string，实现会像 JS 的 `URLSearchParams` 逻辑那样把非空
/// `directory` 追加到 URL 上。
pub type OpenCodeFetch =
    Arc<dyn Fn(&str, Option<&str>, &str, Option<&Value>) -> OpenCodeFuture + Send + Sync>;

/// Production fetch through the managed engine's HTTP client. JS:
/// `buildOpenCodeUrl` + `getOpenCodeAuthHeaders` + `AbortSignal.timeout`
/// followed by `response.json().catch(() => null)`.
///
/// 中文说明：走托管引擎的共享 HTTP client 发请求——带每请求超时与
/// bearer 授权头；非 2xx 映射为 [`OpenCodeError::Status`]，响应体解析
/// 失败时按 JS `.catch(() => null)` 语义返回 Null。
pub fn engine_fetch(engine: Arc<EngineState>, timeout_ms: u64) -> OpenCodeFetch {
    Arc::new(
        move |path: &str, directory: Option<&str>, method: &str, body: Option<&Value>| {
            let engine = Arc::clone(&engine);
            let path = path.to_string();
            let directory = directory.filter(|d| !d.is_empty()).map(str::to_string);
            let method = method.to_string();
            let body = body.cloned();
            Box::pin(async move {
                let Some(base) = engine.base_url() else {
                    return Err(OpenCodeError::Transport);
                };
                let mut url = format!("{base}{path}");
                if let Some(directory) = directory.as_deref() {
                    let separator = if url.contains('?') { '&' } else { '?' };
                    url = format!(
                        "{url}{separator}directory={}",
                        encode_uri_component(directory)
                    );
                }
                let http_method =
                    reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
                let mut request = engine
                    .http()
                    .request(http_method, &url)
                    .timeout(Duration::from_millis(timeout_ms))
                    .header("accept", "application/json");
                if let Some(auth) = engine.auth_header() {
                    request = request.header("authorization", auth);
                }
                if let Some(body) = body.as_ref() {
                    request = request
                        .header("content-type", "application/json")
                        .json(body);
                }
                let response = request.send().await.map_err(|_| OpenCodeError::Transport)?;
                let status = response.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(OpenCodeError::Status {
                        method,
                        path,
                        status,
                    });
                }
                // JS: response.json().catch(() => null).
                let text = response
                    .text()
                    .await
                    .map_err(|_| OpenCodeError::Transport)?;
                Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
            })
        },
    )
}

/// JS `encodeURIComponent` (RFC 3986 unreserved + JS extras literal). Local
/// copy of the helper `session_goal::runtime` keeps private.
///
/// 中文说明：等价于 JS 的 `encodeURIComponent`——除 RFC 3986 未保留字符
/// 及 JS 额外放行字符外全部做百分号编码；这是本地副本，与
/// `session_goal::runtime` 保持私有的实现一致。
pub(crate) fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(format!("%{byte:02X}").as_str());
        }
    }
    out
}

/// URL 编码辅助函数的行为测试。
#[cfg(test)]
mod tests {
    use super::*;

/// 契约：字母数字与 JS 放行的标点原样保留，空格、`/`、`+` 等被百分号编码。
    #[test]
    fn encodes_like_js_encode_uri_component() {
        assert_eq!(encode_uri_component("ses_1"), "ses_1");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("x+y!~*'()"), "x%2By!~*'()");
    }
}
