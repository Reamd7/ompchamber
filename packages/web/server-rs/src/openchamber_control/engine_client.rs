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

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::engine::EngineState;

use super::error::ControlError;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `error` half of the client result convention.
#[derive(Debug, Clone)]
pub struct EngineCallError {
    pub name: String,
    pub message: String,
}

/// `{ data, error?, response }` — `data` is `None` for both JSON `null`
/// and the error path (the service layer checks shape, not presence).
#[derive(Debug, Clone)]
pub struct EngineResponse {
    pub data: Option<Value>,
    pub error: Option<EngineCallError>,
}

impl EngineResponse {
    fn ok(data: Option<Value>) -> Self {
        Self { data, error: None }
    }
}

/// The local-engine-client surface consumed by the control service. An
/// `Err` is a thrown exception (test doubles may reject, like the JS
/// suite's `mockRejectedValueOnce`; the honest client folds every
/// failure into `error` and always resolves).
pub trait EngineClient: Send + Sync {
    /// `client.session.list(params)` — `GET /session?directory=…`.
    fn session_list<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.session.status({ directory })` — `GET /session/status?directory=…`.
    fn session_status<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.session.messages({ sessionID, directory, limit })` —
    /// `GET /session/:id/message?directory=…&limit=…`.
    fn session_messages<'a>(
        &'a self,
        session_id: &'a str,
        directory: Option<&'a str>,
        limit: Option<u64>,
    ) -> BoxFut<'a, Result<EngineResponse, String>>;
    /// `client.experimental.session.list({})` — `GET /experimental/session`.
    fn experimental_session_list<'a>(&'a self) -> BoxFut<'a, Result<EngineResponse, String>>;
}

/// `createClient({ baseUrl: buildOpenCodeUrl('/', ''), headers:
/// getOpenCodeAuthHeaders() })` — the honest engine-backed client over
/// the shared reqwest handle.
pub struct HttpEngineClient {
    engine: Arc<EngineState>,
    base_url: String,
}

impl HttpEngineClient {
    pub fn new(engine: Arc<EngineState>, base_url: String) -> Self {
        Self { engine, base_url }
    }

    /// `request(method, pathname, { query })` for GETs: no body, auth
    /// header attached, `{data, error}` convention preserved.
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
fn urlencode(value: &str) -> String {
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

impl EngineClient for HttpEngineClient {
    fn session_list<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>> {
        let query = [("directory", directory.map(str::to_string))];
        Box::pin(async move { Ok(self.get("/session", &query).await) })
    }

    fn session_status<'a>(
        &'a self,
        directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<EngineResponse, String>> {
        let query = [("directory", directory.map(str::to_string))];
        Box::pin(async move { Ok(self.get("/session/status", &query).await) })
    }

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

    fn experimental_session_list<'a>(&'a self) -> BoxFut<'a, Result<EngineResponse, String>> {
        Box::pin(async move { Ok(self.get("/experimental/session", &[]).await) })
    }
}
/// `getClient()` — wait for the engine, then hand back a client. Mirrors
/// `waitForOpenCodeReady(10_000, 250)` throwing `'OpenCode port is not
/// available'` / `'Timed out waiting for OpenCode to become ready'`, both
/// surfacing as 500 control errors through `asControlError`.
pub type ClientFactory =
    Arc<dyn Fn() -> BoxFut<'static, Result<Arc<dyn EngineClient>, ControlError>> + Send + Sync>;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_matches_encodeuricomponent() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("ses_1"), "ses_1");
        assert_eq!(urlencode("é"), "%C3%A9");
    }

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
