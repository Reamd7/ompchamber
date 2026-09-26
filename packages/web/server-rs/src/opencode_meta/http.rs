//! HTTP transport seam for the `opencode_meta` module (models.dev catalog +
//! npm registry lookups).
//!
//! JS precedent: both libraries call the global `fetch` with
//! `AbortSignal.timeout(ms)` and their vitest suites stub it with canned
//! responses. The Rust port routes every outbound call through [`HttpFetch`]
//! so tests inject the same fakes; the production transport is reqwest +
//! rustls.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;

/// One outbound request, mirroring the `fetch` options the JS uses.
#[derive(Debug, Clone)]
pub(crate) struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout(ms)`.
    pub timeout_ms: Option<u64>,
}

impl HttpRequest {
    pub(crate) fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        }
    }

    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub(crate) fn timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive header lookup (first match, like `headers.get`).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// JS `response.ok` (2xx).
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// JS `response.text()`.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Transport failure. `Timeout` preserves the JS distinction between
/// `TimeoutError`/`AbortError` (mapped to 504 by the routes) and every other
/// network failure.
#[derive(Debug, Clone)]
pub(crate) enum HttpError {
    Timeout,
    Other(String),
}

impl HttpError {
    pub(crate) fn message(&self) -> String {
        match self {
            HttpError::Timeout => "The operation timed out".to_string(),
            HttpError::Other(message) => message.clone(),
        }
    }

    /// JS `error.name === 'TimeoutError'` probe (models-metadata route's
    /// 504 mapping). Unreachable through the current fetch rotation — the
    /// proxy attempt's error always wins, exactly like the JS — but kept as
    #[allow(dead_code)]
    pub(crate) fn is_timeout(&self) -> bool {
        matches!(self, HttpError::Timeout)
    }
}

pub(crate) type HttpFetch =
    Arc<dyn Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> + Send + Sync>;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Production transport (direct, no proxy — the proxy retry path in
/// `models_metadata` builds dedicated proxied clients).
pub(crate) fn default_fetch() -> HttpFetch {
    Arc::new(|request: HttpRequest| {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|err| HttpError::Other(err.to_string()))?;
            let mut builder = CLIENT
                .request(method, &request.url)
                .headers(reqwest::header::HeaderMap::default());
            for (name, value) in &request.headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|err| HttpError::Other(err.to_string()))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|err| HttpError::Other(err.to_string()))?;
                builder = builder.header(name, value);
            }
            if let Some(bytes) = request.body {
                builder = builder.body(bytes);
            }
            if let Some(ms) = request.timeout_ms {
                builder = builder.timeout(Duration::from_millis(ms));
            }
            let response = builder.send().await.map_err(map_reqwest_error)?;
            let status = response.status().as_u16();
            let mut headers = Vec::new();
            for (name, value) in response.headers() {
                headers.push((
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                ));
            }
            let body = response.bytes().await.map_err(map_reqwest_error)?;
            Ok(HttpResponse {
                status,
                headers,
                body: body.to_vec(),
            })
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
