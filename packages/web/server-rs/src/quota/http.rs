//! HTTP transport seam for quota providers.
//!
//! JS providers call the global `fetch` (optionally with
//! `AbortSignal.timeout` / `redirect: 'manual'`); tests stub it with canned
//! responses. The Rust port routes every outbound provider call through
//! [`HttpFetch`] so tests can do the same, with the production transport
//! backed by reqwest + rustls.
//!
//! Error mapping: a timed-out request surfaces as [`HttpError::Timeout`]
//! (providers translate that to "Request timed out"); anything else keeps the
//! transport's message like JS would keep `error.message`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

/// One outbound request, mirroring the `fetch` options the JS providers use.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout(ms)`.
    pub timeout_ms: Option<u64>,
    /// `redirect: 'manual'` (Ollama Cloud rejects redirects without
    /// forwarding credentials).
    pub redirect_manual: bool,
}

impl HttpRequest {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
            redirect_manual: false,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    pub fn json_body(mut self, body: &Value) -> Self {
        self.headers
            .push(("Content-Type".to_string(), "application/json".to_string()));
        self.body = Some(serde_json::to_vec(body).unwrap_or_default());
        self
    }

    pub fn raw_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = Some(body);
        self
    }

    pub fn form_body(mut self, body: String) -> Self {
        self.headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ));
        self.body = Some(body.into_bytes());
        self
    }

    pub fn timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn manual_redirect(mut self) -> Self {
        self.redirect_manual = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// JS `response.ok` (2xx).
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// JS `response.json()` — body parse failure is the caller's SyntaxError.
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// JS `response.text()`.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, Clone)]
pub enum HttpError {
    Timeout,
    Other(String),
}

impl HttpError {
    pub fn message(&self) -> String {
        match self {
            HttpError::Timeout => "The operation timed out".to_string(),
            HttpError::Other(message) => message.clone(),
        }
    }
}

pub type HttpFetch =
    Arc<dyn Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> + Send + Sync>;

/// reqwest client with the default fetch redirect policy (follow, up to 10).
static FOLLOW_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
/// `redirect: 'manual'`.
static MANUAL_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
});

fn collect_headers(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Production transport.
pub fn default_fetch() -> HttpFetch {
    Arc::new(|request: HttpRequest| {
        Box::pin(async move {
            let client = if request.redirect_manual {
                &*MANUAL_CLIENT
            } else {
                &*FOLLOW_CLIENT
            };
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|error| HttpError::Other(error.to_string()))?;
            let url = reqwest::Url::parse(&request.url)
                .map_err(|error| HttpError::Other(error.to_string()))?;
            let mut builder = client.request(method, url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            if let Some(timeout) = request.timeout_ms {
                builder = builder.timeout(Duration::from_millis(timeout));
            }
            match builder.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let headers = collect_headers(&response);
                    match response.bytes().await {
                        Ok(bytes) => Ok(HttpResponse {
                            status,
                            headers,
                            body: bytes.to_vec(),
                        }),
                        Err(error) => Err(map_reqwest_error(error)),
                    }
                }
                Err(error) => Err(map_reqwest_error(error)),
            }
        })
    })
}

fn map_reqwest_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Other(error.to_string())
    }
}
