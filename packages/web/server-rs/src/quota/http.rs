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
//!
//! 中文说明：本模块是 quota provider 的 HTTP 传输 seam。JS 版 provider 直接
//! 调用全局 `fetch`（可选 `AbortSignal.timeout` / `redirect: 'manual'`），
//! 测试用预置响应 stub 它；Rust 移植把所有出站请求收口到 [`HttpFetch`]
//! trait 对象上，生产实现基于 reqwest + rustls，测试注入 fake。
//! 错误映射：超时映射为 [`HttpError::Timeout`]（provider 转译为
//! "Request timed out"），其余错误保留传输层消息（对应 JS 的
//! `error.message`）。

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

/// One outbound request, mirroring the `fetch` options the JS providers use.
///
/// 中文说明：一次出站请求的完整描述，字段与 JS provider 用到的 `fetch`
/// 选项一一对应；通过链式 builder 方法构造。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 方法（GET/POST 等）。
    pub method: String,
    /// 请求目标 URL。
    pub url: String,
    /// 请求头列表（保持插入顺序，可含重复键）。
    pub headers: Vec<(String, String)>,
    /// 请求体字节；`None` 表示无 body。
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout(ms)`.
    ///
    /// 中文说明：请求超时毫秒数（对应 `AbortSignal.timeout(ms)`）。
    pub timeout_ms: Option<u64>,
    /// `redirect: 'manual'` (Ollama Cloud rejects redirects without
    /// forwarding credentials).
    ///
    /// 中文说明：是否禁用自动重定向（对应 `redirect: 'manual'`；Ollama Cloud
    /// 拒绝重定向且不透传凭据）。
    pub redirect_manual: bool,
}

/// [`HttpRequest`] 的链式构造器：以 `new`/`get`/`post` 起步，按需追加
/// header、body 与超时/重定向选项。
impl HttpRequest {
    /// 以指定方法与 URL 创建空请求（无 header/body/超时，自动跟随重定向）。
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

    /// 创建 GET 请求的快捷方式。
    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    /// 创建 POST 请求的快捷方式。
    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    /// 追加一个请求头（消费并返回 self，支持链式调用）。
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// 追加 `Authorization: Bearer <token>` bearer token 头。
    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// 设置 JSON 请求体：自动加 `Content-Type: application/json` 头并把
    /// `body` 序列化为字节（序列化失败退化为空 body）。
    pub fn json_body(mut self, body: &Value) -> Self {
        self.headers
            .push(("Content-Type".to_string(), "application/json".to_string()));
        self.body = Some(serde_json::to_vec(body).unwrap_or_default());
        self
    }

    /// 设置原始字节请求体，`Content-Type` 由调用方指定。
    pub fn raw_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = Some(body);
        self
    }

    /// 设置 `application/x-www-form-urlencoded` 表单请求体（字符串原样
    /// 编码为字节）。
    pub fn form_body(mut self, body: String) -> Self {
        self.headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ));
        self.body = Some(body.into_bytes());
        self
    }

    /// 设置请求超时（毫秒）。
    pub fn timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// 切换为手动重定向模式（不自动跟随 3xx）。
    pub fn manual_redirect(mut self) -> Self {
        self.redirect_manual = true;
        self
    }
}

/// 一次出站请求的响应：状态码、响应头（保持原序）与原始 body 字节。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码（如 200、401）。
    pub status: u16,
    /// 响应头列表（保持服务端返回顺序）。
    pub headers: Vec<(String, String)>,
    /// 响应体原始字节。
    pub body: Vec<u8>,
}

/// [`HttpResponse`] 的读取辅助：header 查找、`ok` 判定与 JSON/text 解码，
/// 语义对齐 JS `fetch` Response 的同名能力。
impl HttpResponse {
    /// Case-insensitive header lookup.
    ///
    /// 中文说明：按头名查找（大小写不敏感），返回第一个匹配的值。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// JS `response.ok` (2xx).
    ///
    /// 中文说明：对应 JS `response.ok`——状态码为 2xx 时为 true。
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// JS `response.json()` — body parse failure is the caller's SyntaxError.
    ///
    /// 中文说明：对应 JS `response.json()`；body 解析失败把
    /// `serde_json::Error` 交给调用方（即 JS 的 SyntaxError 路径）。
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// JS `response.text()`.
    ///
    /// 中文说明：对应 JS `response.text()`；非 UTF-8 字节以替换字符兜底。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 出站请求的传输层错误：超时与其他（网络/DNS/构造失败等）两类，
/// 对齐 JS `fetch` 抛错时 provider 关心的区分维度。
#[derive(Debug, Clone)]
pub enum HttpError {
    /// 请求超时（含 `AbortSignal.timeout` 触发的取消）。
    Timeout,
    /// 其余错误，携带传输层错误消息。
    Other(String),
}

/// [`HttpError`] 的消息转换。
impl HttpError {
    /// 转为可展示的错误消息：超时固定为 "The operation timed out"，
    /// 其余原样返回内部消息。
    pub fn message(&self) -> String {
        match self {
            HttpError::Timeout => "The operation timed out".to_string(),
            HttpError::Other(message) => message.clone(),
        }
    }
}

/// HTTP 执行器类型：以 [`HttpRequest`] 换取一个异步 future；由
/// [`QuotaDeps::http`] 注入，生产用 [`default_fetch`]，测试注入 canned fake。
pub type HttpFetch =
    Arc<dyn Fn(HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> + Send + Sync>;

/// reqwest client with the default fetch redirect policy (follow, up to 10).
///
/// 中文说明：默认跟随重定向（最多 10 跳，与 fetch 默认策略一致）的
/// 全局共享 reqwest 客户端。
static FOLLOW_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
/// `redirect: 'manual'`.
///
/// 中文说明：禁用自动重定向（对应 `redirect: 'manual'`）的全局共享
/// reqwest 客户端；构建失败退化为默认客户端。
static MANUAL_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
});

/// 把 reqwest 响应头复制为保持原序的 `(名字, 值)` 向量；非法 UTF-8 的
/// 头值以空字符串兜底。
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
///
/// 中文说明：生产传输实现：按 `redirect_manual` 选择共享客户端，转换
/// method/URL（非法即 `HttpError::Other`），套用 header、body 与每请求
/// 超时后发送；响应收集状态码、响应头与完整 body。reqwest 错误经
/// [`map_reqwest_error`] 归一化。
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

/// 把 reqwest 错误归一化：超时类映射为 [`HttpError::Timeout`]，其余保留
/// 原始错误消息为 `HttpError::Other`。
fn map_reqwest_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Other(error.to_string())
    }
}
