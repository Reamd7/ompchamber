//! Engine fetch seam shared by the three hub consumers ported in this module
//! (`session-assist`, `session-knowledge`, `context-obligatory`).
//!
//! Each JS module closes over its own `openCodeFetch(path, {directory,
//! method, body, query})` built from `buildOpenCodeUrl` +
//! `getOpenCodeAuthHeaders` + `AbortSignal.timeout`; the error strings
//! (`OpenCode ${method} ${path} failed with ${status}`, transport
//! `fetch failed`) reach route responses verbatim, so they are preserved here.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::engine::EngineState;

/// JS `error.message` of a failed engine call. `OpenCode … failed with …`
/// carries the upstream status; `fetch failed` is the undici transport error
/// (also used when the engine has no base URL yet, mirroring the JS fetch to
/// an unreachable URL).
#[derive(Debug, thiserror::Error)]
pub enum OpenCodeError {
    #[error("OpenCode {method} {path} failed with {status}")]
    Status {
        method: String,
        path: String,
        status: u16,
    },
    #[error("fetch failed")]
    Transport,
}

pub type OpenCodeFuture = Pin<Box<dyn Future<Output = Result<Value, OpenCodeError>> + Send>>;
/// `(path, directory, method, body)` — `path` may already carry a query
/// string; the implementation appends a non-empty `directory` like the JS
/// `URLSearchParams` dance.
pub type OpenCodeFetch =
    Arc<dyn Fn(&str, Option<&str>, &str, Option<&Value>) -> OpenCodeFuture + Send + Sync>;

/// Production fetch through the managed engine's HTTP client. JS:
/// `buildOpenCodeUrl` + `getOpenCodeAuthHeaders` + `AbortSignal.timeout`
/// followed by `response.json().catch(() => null)`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_js_encode_uri_component() {
        assert_eq!(encode_uri_component("ses_1"), "ses_1");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("x+y!~*'()"), "x%2By!~*'()");
    }
}
