//! HTTP transport seam for the `opencode_meta` module (models.dev catalog +
//! npm registry lookups).
//!
//! JS precedent: both libraries call the global `fetch` with
//! `AbortSignal.timeout(ms)` and their vitest suites stub it with canned
//! responses. The Rust port routes every outbound call through [`HttpFetch`]
//! so tests inject the same fakes; the production transport is reqwest +
//! rustls.
//!
//! 中文说明：`opencode_meta` 模块的 HTTP 传输接缝。models.dev 目录与 npm
//! registry 的全部出站请求都经由 [`HttpFetch`] 函数对象发出，生产实现为
//! 共享 reqwest 客户端直连，测试注入 canned 响应，与 JS vitest 桩
//! global fetch 的做法一一对应。

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;

/// One outbound request, mirroring the `fetch` options the JS uses.
#[derive(Debug, Clone)]
pub(crate) struct HttpRequest {
/// HTTP 方法名（"GET"/"POST" 等字符串）。
    pub method: String,
/// 完整目标 URL。
    pub url: String,
/// 请求头列表（保持插入顺序，允许同名重复）。
    pub headers: Vec<(String, String)>,
/// 请求体字节；GET 请求为 `None`。
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout(ms)`.
    pub timeout_ms: Option<u64>,
}

/// 请求构造器：链式便捷方法对应 JS fetch 的 options 拼装。
impl HttpRequest {
/// 构造无头、无体、无超时的 GET 请求。
    pub(crate) fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        }
    }

/// 追加一个请求头（builder 风格：消耗自身并返回新值）。
    pub(crate) fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

/// 设置整体请求超时，等价 JS 的 `AbortSignal.timeout(ms)`。
    pub(crate) fn timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }
}

/// 一次入站响应：状态码、响应头与原始字节体。
#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
/// HTTP 状态码（如 200、404）。
    pub status: u16,
/// 响应头列表（查找按不区分大小写处理，同 fetch 的 headers.get）。
    pub headers: Vec<(String, String)>,
/// 原始响应体字节（文本场景由 [`HttpResponse::text`] 转换）。
    pub body: Vec<u8>,
}

/// JS `Response` 语义的只读辅助方法。
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
/// 请求超时：JS 的 `TimeoutError`/`AbortError` 对应物，路由层映射为 504。
    Timeout,
/// 其它传输层失败，携带底层错误文本。
    Other(String),
}

/// 错误文本与超时类别探测辅助。
impl HttpError {
/// 返回错误 message；`Timeout` 使用 JS 同款文案 "The operation timed out"。
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

/// 可注入的传输函数类型：吃进 [`HttpRequest`]，返回异步 [`HttpResponse`]
/// 或 [`HttpError`]；测试用它注入 canned 响应。
pub(crate) type HttpFetch =
    Arc<dyn Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> + Send + Sync>;

/// 进程级共享的 reqwest 客户端（复用连接池）。
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

/// 将 reqwest 错误归类：超时映射为 [`HttpError::Timeout`]，其余归 [`HttpError::Other`]。
fn map_reqwest_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Other(error.to_string())
    }
}
