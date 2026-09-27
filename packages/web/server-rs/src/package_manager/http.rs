//! HTTP transport seam for the package-manager port. The JS module calls
//! global `fetch` with `AbortSignal.timeout(10000)` (and the vitest suite
//! stubs it with a URL-routing mock); the Rust port routes every outbound
//! request through [`HttpTransport`] so tests can inject a fake. Production
//! uses [`ReqwestTransport`] over a rustls client with the same 10s budget.
//!
//! 中文说明：所有出站请求统一走 [`HttpTransport`] trait；测试注入
//! [`testing::FakeTransport`]，生产用 [`ReqwestTransport`]（rustls 客户端，
//! 每个请求带同样的 10 秒超时预算，对齐 JS `AbortSignal.timeout(10000)`）。

use std::pin::Pin;
#[cfg(test)]
use std::sync::{Arc, Mutex};

/// 出站请求使用的 HTTP 方法（当前移植只涉及 GET/POST）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// 只读请求（releases API 查询、changelog 拉取）。
    Get,
    /// 带载荷请求（托管更新检查 API 上报）。
    Post,
}

/// 对应 JS fetch 的 `body` 参数，仅有「无」与「JSON 字符串」两种形态。
#[derive(Debug, Clone)]
pub enum HttpBody {
    /// 无请求体（GET 等）。
    None,
    /// `application/json`
    /// 已序列化的 JSON 字符串，发送时按 `application/json` 处理。
    Json(String),
}

/// 一次出站请求的完整描述（方法/URL/头/体），对具体传输实现透明。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 方法。
    pub method: HttpMethod,
    /// 完整目标 URL（含 query）。
    pub url: String,
    /// 请求头键值对列表（按插入顺序保留）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: HttpBody,
}

/// 请求头的便捷访问。
impl HttpRequest {
    /// First value of a request header, if present.
    /// 头名大小写不敏感，取第一个匹配项的值。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 传输层返回的原始响应：状态码 + 未解码的字节体。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 原始响应体字节。
    pub body: Vec<u8>,
}

/// 响应体的常用解码方式。
impl HttpResponse {
    /// 对应 JS `response.ok`：状态码落在 2xx 区间。
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `await response.json()` — callers treat parse failures as bad payloads.
    /// 解析失败返回 `None`，由调用方按坏载荷分支处理。
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// `await response.text()` (utf8, lossy for invalid bytes).
    /// 非法 UTF-8 字节有损替换，不会失败。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 传输结果：`Err(String)` 对应 fetch reject（网络错误/超时）。
pub type HttpResult = Result<HttpResponse, String>;
/// trait 对象化的异步返回，便于注入同步应答的假实现。
pub type HttpFuture = Pin<Box<dyn Future<Output = HttpResult> + Send>>;

/// 出站 HTTP 传输接缝：生产用 reqwest 实现，测试注入 fake；
/// 要求可跨线程共享（runtime 以 `Arc` 持有）。
pub trait HttpTransport: Send + Sync + 'static {
    /// 发送请求并返回响应；任何失败以 `Err(错误描述)` 表达（等价 fetch reject）。
    fn send(&self, request: HttpRequest) -> HttpFuture;
}

/// Production transport: reqwest (rustls) with the JS 10s abort budget
/// applied per request.
/// 复用调用方构建好的客户端（rustls、连接超时），每个请求再叠加 10 秒总预算。
pub struct ReqwestTransport {
    /// 由 [`ReqwestTransport::new`] 注入的 reqwest 客户端。
    client: reqwest::Client,
}

/// 构造器。
impl ReqwestTransport {
    /// 包装一个已完成配置的 reqwest 客户端。
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// reqwest 版传输实现。
impl HttpTransport for ReqwestTransport {
    /// 逐项设置头与体后发送；reqwest 错误统一转成 `Err(String)`，
    /// 非 2xx 状态码仍算成功响应（由调用方用 `is_ok` 判断）。
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

/// 测试专用的假传输与响应构造工具（仅 `cfg(test)` 下编译）。
#[cfg(test)]
pub mod testing {
    use super::*;

    /// Test fake mirroring the vitest `createFetchMock()` helper: routes by
    /// `url.includes(pattern)` and records every call. Handlers return
    /// `Err` to emulate a rejected fetch (`Promise.reject`).
    /// 未匹配任何 handler 的调用返回 `Err`，测试因此失败——模拟 JS mock 的
    /// 「unexpected fetch call」行为。
    pub struct FakeTransport {
        /// (URL 片段, 预置响应) 列表，先注册先匹配。
        handlers: Mutex<Vec<(String, HttpResult)>>,
        /// 按序记录的每次 `send` 请求。
        calls: Mutex<Vec<HttpRequest>>,
    }

    /// 注册规则与断言辅助。
    impl FakeTransport {
        /// 创建不带任何规则的 fake（返回 `Arc` 便于注入 runtime）。
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                handlers: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// `fetchMock.when(pattern, response)`.
        /// 支持链式调用；pattern 用 `url.contains` 匹配。
        pub fn when(&self, pattern: &str, response: HttpResult) -> &Self {
            self.handlers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((pattern.to_string(), response));
            self
        }

        /// 返回已记录请求的拷贝，用于断言 URL/头/体。
        pub fn calls(&self) -> Vec<HttpRequest> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// 已记录的请求总数。
        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }

        /// 200 OK with a JSON body (`{ ok: true, json: async () => value })`).
        /// 序列化失败直接 panic（测试数据必须合法）。
        pub fn ok_json(value: serde_json::Value) -> HttpResult {
            let body = serde_json::to_vec(&value).expect("serialize json body");
            Ok(HttpResponse { status: 200, body })
        }

        /// 200 OK with a text body.
        /// 纯文本成功响应。
        pub fn ok_text(text: &str) -> HttpResult {
            Ok(HttpResponse {
                status: 200,
                body: text.as_bytes().to_vec(),
            })
        }

        /// Non-ok response (`{ ok: false, status }`).
        /// 指定状态码、空 body，用于走非 ok 分支。
        pub fn status(status: u16) -> HttpResult {
            Ok(HttpResponse {
                status,
                body: Vec::new(),
            })
        }

        /// Rejected fetch (`Promise.reject(new Error(...))`).
        /// 网络层错误（等价 `Promise.reject`）。
        pub fn reject(message: &str) -> HttpResult {
            Err(message.to_string())
        }
    }

    /// fake 的接缝实现。
    impl HttpTransport for FakeTransport {
        /// 先记录请求，再返回首个 URL 匹配的预置响应。
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
