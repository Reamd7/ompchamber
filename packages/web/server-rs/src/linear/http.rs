//! HTTP transport seam for the Linear port. The JS module calls global
//! `fetch` (and the tests stub it with `vi.stubGlobal`); the Rust port routes
//! every outbound Linear/broker request through [`HttpTransport`] so tests can
//! inject a fake. Production uses [`ReqwestTransport`] over the shared engine
//! reqwest client (rustls).

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
    /// `application/x-www-form-urlencoded`
    Form(String),
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

    /// `await response.json().catch(() => null)`
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub type HttpResult = Result<HttpResponse, String>;
pub type HttpFuture = Pin<Box<dyn Future<Output = HttpResult> + Send>>;

pub trait HttpTransport: Send + Sync + 'static {
    fn send(&self, request: HttpRequest) -> HttpFuture;
}

/// Production transport: shared reqwest client (rustls) from the engine.
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
        let mut outbound = self.client.request(method, request.url.clone());
        for (name, value) in &request.headers {
            outbound = outbound.header(name.as_str(), value.as_str());
        }
        match request.body {
            HttpBody::None => {}
            HttpBody::Form(body) | HttpBody::Json(body) => {
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

/// Test fake mirroring `vi.stubGlobal('fetch', handler)`: records every call
/// and answers synchronously from the handler.
#[cfg(test)]
pub struct FakeTransport {
    handler: Arc<dyn Fn(&HttpRequest) -> HttpResult + Send + Sync>,
    calls: Mutex<Vec<HttpRequest>>,
}

#[cfg(test)]
impl FakeTransport {
    pub fn new<F>(handler: F) -> Arc<Self>
    where
        F: Fn(&HttpRequest) -> HttpResult + Send + Sync + 'static,
    {
        Arc::new(Self {
            handler: Arc::new(handler),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn calls_to(&self, url_suffix: &str) -> Vec<HttpRequest> {
        self.calls()
            .into_iter()
            .filter(|call| call.url.ends_with(url_suffix))
            .collect()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
impl HttpTransport for FakeTransport {
    fn send(&self, request: HttpRequest) -> HttpFuture {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        let result = (self.handler)(&request);
        Box::pin(async move { result })
    }
}
