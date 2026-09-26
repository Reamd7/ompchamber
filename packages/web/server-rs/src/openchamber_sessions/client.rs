//! Port of the `server/lib/opencode/local-engine-client.js` subset the
//! OMPChamber session routes consume, plus the raw-`fetch` calls the routes
//! make directly (`createSession`, `runPromptAsync`, `fetchJson`).
//!
//! The JS client wraps every SDK call in the `{ data, error, response }`
//! convention and **never rejects for HTTP outcomes** (transport failures are
//! captured into `error`); [`EngineReply`] mirrors that shape and the
//! [`EngineClient`] trait is the DI seam the JS tests replace with mocks.
//! The fetch-based helpers keep their throwing semantics (`Result`).

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::engine::EngineState;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `error` half of the `{ data, error, response }` convention.
///
/// routes.js never reads this half (only `data.id`/`data` arrays gate
/// behavior), so the fields carry the convention shape for the future
/// shared local-engine-client module rather than route logic.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EngineCallError {
    pub name: String,
    pub message: String,
}

/// The SDK result convention: `data` is the parsed body of a 2xx response,
/// `error` carries the engine's error envelope otherwise, `status` is the
/// HTTP status when a response arrived. As in the JS client, only `data`
/// drives route behavior; the other halves document the wire convention.
#[derive(Debug, Clone)]
pub struct EngineReply {
    pub data: Option<Value>,
    #[allow(dead_code)]
    pub error: Option<EngineCallError>,
    #[allow(dead_code)]
    pub status: Option<u16>,
}

impl EngineReply {
    /// A 2xx reply carrying the parsed body.
    pub fn ok(data: Value) -> Self {
        Self {
            data: Some(data),
            error: None,
            status: Some(200),
        }
    }

    fn transport_error(message: String) -> Self {
        Self {
            data: None,
            error: Some(EngineCallError {
                name: "UnknownError".to_string(),
                message,
            }),
            status: None,
        }
    }
}

/// Parameters of `client.session.command` minus `sessionID` (the SDK sends
/// the rest as the request body).
#[derive(Debug, Clone)]
pub struct SessionCommandParams {
    pub directory: String,
    pub command: String,
    pub arguments: String,
    pub agent: Option<String>,
    /// `"providerID/modelID"`.
    pub model: String,
    pub variant: Option<String>,
}

/// The engine seam used by the session routes: the local-engine-client
/// subset plus the raw fetch helpers. `Err` from the SDK-style methods
/// mirrors a rejecting mock (the HTTP implementation never rejects, exactly
/// like the JS client's try/catch).
pub trait EngineClient: Send + Sync {
    fn session_fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    fn session_messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: u32,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    fn session_command(
        &self,
        session_id: &str,
        params: &SessionCommandParams,
    ) -> BoxFut<'_, Result<EngineReply, String>>;
    fn command_list(&self, directory: &str) -> BoxFut<'_, Result<EngineReply, String>>;
    /// `fetchJson(url, authHeaders, fallback, directory)` — resolves to the
    /// fallback on any non-2xx or unparseable response.
    fn fetch_json(&self, path: &str, directory: &str, fallback: Value) -> BoxFut<'_, Value>;
    /// `createSession` — throws `session create failed (status): body` /
    /// `failed to create session`.
    fn create_session(
        &self,
        directory: &str,
        title: Option<&str>,
    ) -> BoxFut<'_, Result<String, String>>;
    /// `runPromptAsync` — throws `prompt_async failed (status): body`.
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>>;
}

/// `encodeURIComponent` (RFC 3986 unreserved + JS literal extras).
pub fn encode_uri_component(value: &str) -> String {
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
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `URLSearchParams.set('directory', …).toString()` — form-urlencoding
/// (spaces become `+`, slashes `%2F`).
fn directory_query(directory: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("directory", directory)
        .finish()
}

/// HTTP-backed client over the managed engine (`createLocalEngineClient({
/// baseUrl, headers: authHeaders })` + the routes' raw fetch calls). The
/// client is constructed without a `directory`, so SDK calls carry the
/// directory in the body/query and no `x-opencode-directory` header; the
/// raw fetch helpers set it percent-encoded like the official SDK.
pub struct HttpEngineClient {
    engine: Arc<EngineState>,
}

impl HttpEngineClient {
    pub fn new(engine: Arc<EngineState>) -> Self {
        Self { engine }
    }

    fn base(&self) -> Result<String, String> {
        let base = self.engine.base_url().unwrap_or_default();
        let trimmed = base.strip_suffix('/').unwrap_or(&base).to_string();
        if trimmed.is_empty() {
            return Err("engine unavailable".to_string());
        }
        Ok(trimmed)
    }

    fn auth(&self) -> Option<String> {
        self.engine.auth_header()
    }

    /// One SDK-convention request: parse the text body, map non-2xx into the
    /// error envelope, capture transport failures.
    async fn sdk_request(
        &self,
        method: reqwest::Method,
        url: String,
        pathname: &str,
        body: Option<Value>,
    ) -> Result<EngineReply, String> {
        let mut request = self.engine.http().request(method, url);
        if let Some(auth) = self.auth() {
            request = request.header("authorization", auth);
        }
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .json(&payload);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Ok(EngineReply::transport_error(error.to_string())),
        };
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload: Option<Value> = if text.is_empty() {
            None
        } else {
            match serde_json::from_str(&text) {
                Ok(value) => Some(value),
                Err(_) => Some(serde_json::json!({ "message": text })),
            }
        };
        if (200..300).contains(&status) {
            return Ok(EngineReply {
                data: payload,
                error: None,
                status: Some(status),
            });
        }
        let message = payload
            .as_ref()
            .and_then(|p| p.get("data").and_then(|d| d.get("message")))
            .or_else(|| payload.as_ref().and_then(|p| p.get("message")))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{pathname} failed with {status}"));
        let name = payload
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("UnknownError")
            .to_string();
        Ok(EngineReply {
            data: None,
            error: Some(EngineCallError { name, message }),
            status: Some(status),
        })
    }

    /// The raw-fetch header set: auth + percent-encoded directory + accept
    /// (+ content-type when a body is sent).
    fn raw_fetch_request(
        &self,
        method: reqwest::Method,
        url: &str,
        directory: &str,
        body: Option<&Value>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .engine
            .http()
            .request(method, url)
            .header("accept", "application/json")
            .header("x-opencode-directory", encode_uri_component(directory));
        if let Some(auth) = self.auth() {
            request = request.header("authorization", auth);
        }
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .json(&payload);
        }
        request
    }
}

impl EngineClient for HttpEngineClient {
    fn session_fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!("{base}/session/{}/fork", encode_uri_component(session_id)),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        let mut body = serde_json::Map::new();
        body.insert(
            "directory".to_string(),
            Value::String(directory.to_string()),
        );
        if let Some(message_id) = message_id {
            body.insert(
                "messageID".to_string(),
                Value::String(message_id.to_string()),
            );
        }
        let body = Value::Object(body);
        Box::pin(self.sdk_request(reqwest::Method::POST, url, "/session/fork", Some(body)))
    }

    fn session_messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: u32,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/message?{}&limit={limit}",
                encode_uri_component(session_id),
                directory_query(directory),
            ),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        Box::pin(self.sdk_request(reqwest::Method::GET, url, "/session/message", None))
    }

    fn session_command(
        &self,
        session_id: &str,
        params: &SessionCommandParams,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/command",
                encode_uri_component(session_id)
            ),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        let mut body = serde_json::Map::new();
        body.insert(
            "directory".to_string(),
            Value::String(params.directory.clone()),
        );
        body.insert("command".to_string(), Value::String(params.command.clone()));
        body.insert(
            "arguments".to_string(),
            Value::String(params.arguments.clone()),
        );
        if let Some(agent) = &params.agent {
            body.insert("agent".to_string(), Value::String(agent.clone()));
        }
        body.insert("model".to_string(), Value::String(params.model.clone()));
        if let Some(variant) = &params.variant {
            body.insert("variant".to_string(), Value::String(variant.clone()));
        }
        let body = Value::Object(body);
        Box::pin(self.sdk_request(reqwest::Method::POST, url, "/session/command", Some(body)))
    }

    fn command_list(&self, directory: &str) -> BoxFut<'_, Result<EngineReply, String>> {
        let url = match self.base() {
            Ok(base) => format!("{base}/command?{}", directory_query(directory)),
            Err(error) => return Box::pin(async move { Ok(EngineReply::transport_error(error)) }),
        };
        Box::pin(self.sdk_request(reqwest::Method::GET, url, "/command", None))
    }

    fn fetch_json(&self, path: &str, directory: &str, fallback: Value) -> BoxFut<'_, Value> {
        let directory = directory.to_string();
        let url = match self.base() {
            Ok(base) => format!("{base}{path}?{}", directory_query(&directory)),
            Err(_) => return Box::pin(async move { fallback }),
        };
        Box::pin(async move {
            let request = self.raw_fetch_request(reqwest::Method::GET, &url, &directory, None);
            let Ok(response) = request.send().await else {
                return fallback;
            };
            if !response.status().is_success() {
                return fallback;
            }
            response.json::<Value>().await.unwrap_or(fallback)
        })
    }

    fn create_session(
        &self,
        directory: &str,
        title: Option<&str>,
    ) -> BoxFut<'_, Result<String, String>> {
        let directory = directory.to_string();
        let title = title.map(String::from);
        let url = match self.base() {
            Ok(base) => format!("{base}/session?{}", directory_query(&directory)),
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let mut body = serde_json::Map::new();
        body.insert("directory".to_string(), Value::String(directory.clone()));
        if let Some(title) = &title {
            body.insert("title".to_string(), Value::String(title.clone()));
        }
        let body = Value::Object(body);
        Box::pin(async move {
            let request =
                self.raw_fetch_request(reqwest::Method::POST, &url, &directory, Some(&body));
            let response = request
                .send()
                .await
                .map_err(|_| "session create failed".to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                let detail = if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                };
                return Err(format!("session create failed ({status}){detail}"));
            }
            let payload: Option<Value> = serde_json::from_str(&text).ok();
            let id = payload
                .as_ref()
                .and_then(|p| p.get("id"))
                .or_else(|| {
                    payload
                        .as_ref()
                        .and_then(|p| p.get("data").and_then(|d| d.get("id")))
                })
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(String::from);
            id.ok_or_else(|| "failed to create session".to_string())
        })
    }

    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>> {
        let directory = directory.to_string();
        let url = match self.base() {
            Ok(base) => format!(
                "{base}/session/{}/prompt_async?{}",
                encode_uri_component(session_id),
                directory_query(&directory),
            ),
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let payload = payload.clone();
        Box::pin(async move {
            let request =
                self.raw_fetch_request(reqwest::Method::POST, &url, &directory, Some(&payload));
            let response = request
                .send()
                .await
                .map_err(|_| "prompt_async failed".to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                let detail = if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                };
                return Err(format!("prompt_async failed ({status}){detail}"));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_js_encode_uri_component() {
        assert_eq!(encode_uri_component("/repo/app"), "%2Frepo%2Fapp");
        assert_eq!(
            encode_uri_component("/home/user/Masaüstü/projeler"),
            "%2Fhome%2Fuser%2FMasa%C3%BCst%C3%BC%2Fprojeler"
        );
        assert_eq!(encode_uri_component("a b"), "a%20b");
        assert_eq!(encode_uri_component("ses_123"), "ses_123");
    }
}
