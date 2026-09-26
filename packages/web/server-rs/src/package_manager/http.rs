//! HTTP transport seam for the package-manager port. The JS module calls
//! global `fetch` with `AbortSignal.timeout(10000)` (and the vitest suite
//! stubs it with a URL-routing mock); the Rust port routes every outbound
//! request through [`HttpTransport`] so tests can inject a fake. Production
//! uses [`ReqwestTransport`] over a rustls client with the same 10s budget.

use std::pin::Pin;
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

#[derive(Debug, Clone)]
pub enum HttpBody {
    None,
    /// `application/json`
    Json(String),
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: HttpBody,
}

impl HttpRequest {
    /// First value of a request header, if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `await response.json()` — callers treat parse failures as bad payloads.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// `await response.text()` (utf8, lossy for invalid bytes).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub type HttpResult = Result<HttpResponse, String>;
pub type HttpFuture = Pin<Box<dyn Future<Output = HttpResult> + Send>>;

pub trait HttpTransport: Send + Sync + 'static {
    fn send(&self, request: HttpRequest) -> HttpFuture;
}

/// Production transport: reqwest (rustls) with the JS 10s abort budget
/// applied per request.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, request: HttpRequest) -> HttpFuture {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
        };
        let mut outbound = self
            .client
            .request(method, request.url.clone())
            .timeout(std::time::Duration::from_secs(10));
        for (name, value) in &request.headers {
            outbound = outbound.header(name.as_str(), value.as_str());
        }
        match request.body {
            HttpBody::None => {}
            HttpBody::Json(body) => {
                outbound = outbound.body(body);
            }
        }
        Box::pin(async move {
            let response = outbound.send().await.map_err(|e| e.to_string())?;
            let status = response.status().as_u16();
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            Ok(HttpResponse {
                status,
                body: bytes.to_vec(),
            })
        })
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Test fake mirroring the vitest `createFetchMock()` helper: routes by
    /// `url.includes(pattern)` and records every call. Handlers return
    /// `Err` to emulate a rejected fetch (`Promise.reject`).
    pub struct FakeTransport {
        handlers: Mutex<Vec<(String, HttpResult)>>,
        calls: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                handlers: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// `fetchMock.when(pattern, response)`.
        pub fn when(&self, pattern: &str, response: HttpResult) -> &Self {
            self.handlers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((pattern.to_string(), response));
            self
        }

        pub fn calls(&self) -> Vec<HttpRequest> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        /// 200 OK with a JSON body (`{ ok: true, json: async () => value })`).
        pub fn ok_json(value: serde_json::Value) -> HttpResult {
            let body = serde_json::to_vec(&value).expect("serialize json body");
            Ok(HttpResponse { status: 200, body })
        }

        /// 200 OK with a text body.
        pub fn ok_text(text: &str) -> HttpResult {
            Ok(HttpResponse {
                status: 200,
                body: text.as_bytes().to_vec(),
            })
        }

        /// Non-ok response (`{ ok: false, status }`).
        pub fn status(status: u16) -> HttpResult {
            Ok(HttpResponse {
                status,
                body: Vec::new(),
            })
        }

        /// Rejected fetch (`Promise.reject(new Error(...))`).
        pub fn reject(message: &str) -> HttpResult {
            Err(message.to_string())
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(&self, request: HttpRequest) -> HttpFuture {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            let url = request.url.clone();
            let matched = {
                let handlers = self.handlers.lock().unwrap_or_else(|e| e.into_inner());
                handlers
                    .iter()
                    .find(|(pattern, _)| url.contains(pattern.as_str()))
                    .map(|(_, response)| response.clone())
            };
            Box::pin(async move {
                matched.unwrap_or_else(|| Err(format!("Unexpected fetch call: {url}")))
            })
        }
    }
}
