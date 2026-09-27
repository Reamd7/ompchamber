//! HTTP transport seam for the Linear port. The JS module calls global
//! `fetch` (and the tests stub it with `vi.stubGlobal`); the Rust port routes
//! every outbound Linear/broker request through [`HttpTransport`] so tests can
//! inject a fake. Production uses [`ReqwestTransport`] over the shared engine
//! reqwest client (rustls).
//! Linear 移植的 HTTP 传输接缝。JS 模块直接调用全局 fetch（测试用
//! vi.stubGlobal 打桩）；Rust 移植把所有 Linear/broker 出站请求都经由
//! [`HttpTransport`] 发出，测试因此可以注入 fake。生产实现为基于引擎
//! 共享 reqwest 客户端（rustls）的 [`ReqwestTransport`]。

use std::pin::Pin;
#[cfg(test)]
use std::sync::{Arc, Mutex};

/// 出站请求的 HTTP 方法（Linear 集成只用到 GET/POST）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// HTTP GET。
    Get,
    /// HTTP POST。
    Post,
}

/// 出站请求体形态。
#[derive(Debug, Clone)]
pub enum HttpBody {
    /// 无请求体。
    None,
    /// 表单体（application/x-www-form-urlencoded）。
    /// `application/x-www-form-urlencoded`
    Form(String),
    /// JSON 体（application/json）。
    /// `application/json`
    Json(String),
}

/// 与具体传输实现无关的出站请求描述。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 方法。
    pub method: HttpMethod,
    /// 目标 URL。
    pub url: String,
    /// 请求头列表（保留插入顺序）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: HttpBody,
}

/// 请求的辅助读取。
impl HttpRequest {
    /// 按名字（大小写不敏感）取第一个匹配请求头的值。
    /// First value of a request header, if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 与具体传输实现无关的响应：状态码 + 原始字节。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应体原始字节。
    pub body: Vec<u8>,
}

/// 响应的常用判定与解析。
impl HttpResponse {
    /// 是否为 2xx 成功状态。
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 对应 JS 的 await response.json().catch(() => null)：解析失败返回 None。
    /// `await response.json().catch(() => null)`
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// 传输层结果：响应，或错误描述字符串（对应 JS fetch 抛错的 message）。
pub type HttpResult = Result<HttpResponse, String>;
/// 传输调用的装箱异步返回类型（Send，可在任意 executor 上轮询）。
pub type HttpFuture = Pin<Box<dyn Future<Output = HttpResult> + Send>>;

/// 所有出站 Linear/broker 请求的传输抽象；生产与测试各有实现。
pub trait HttpTransport: Send + Sync + 'static {
    /// 发送请求并返回响应；网络失败以 Err(错误描述字符串) 表达。
    fn send(&self, request: HttpRequest) -> HttpFuture;
}

/// 生产传输：使用引擎共享的 reqwest 客户端（rustls）。
/// Production transport: shared reqwest client (rustls) from the engine.
pub struct ReqwestTransport {
    /// 引擎共享的 reqwest 客户端。
    client: reqwest::Client,
}

/// 生产传输的构造。
impl ReqwestTransport {
    /// 用给定 reqwest 客户端构建传输。
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// reqwest 传输实现：组装方法/请求头/请求体并读取状态码与完整响应体。
impl HttpTransport for ReqwestTransport {
    /// 执行请求；任何 reqwest 错误都转为 Err(错误描述字符串)。
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

/// 对应 vi.stubGlobal('fetch', handler) 的测试桩：记录每次调用，
/// 并由 handler 同步给出响应。
/// Test fake mirroring `vi.stubGlobal('fetch', handler)`: records every call
/// and answers synchronously from the handler.
#[cfg(test)]
pub struct FakeTransport {
    /// 根据请求同步生成响应（或错误）的测试处理器。
    handler: Arc<dyn Fn(&HttpRequest) -> HttpResult + Send + Sync>,
    /// 已记录的请求列表（按发送顺序）。
    calls: Mutex<Vec<HttpRequest>>,
}

/// 测试桩的构造与调用记录查询。
#[cfg(test)]
impl FakeTransport {
    /// 用处理器构建测试桩（返回 Arc 便于直接作为传输注入）。
    pub fn new<F>(handler: F) -> Arc<Self>
    where
        F: Fn(&HttpRequest) -> HttpResult + Send + Sync + 'static,
    {
        Arc::new(Self {
            handler: Arc::new(handler),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 返回已记录的全部请求副本。
    pub fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 返回 URL 以指定后缀结尾的请求。
    pub fn calls_to(&self, url_suffix: &str) -> Vec<HttpRequest> {
        self.calls()
            .into_iter()
            .filter(|call| call.url.ends_with(url_suffix))
            .collect()
    }

    /// 返回已记录的请求数。
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 测试桩的传输实现：先记录请求，再交给 handler 应答。
#[cfg(test)]
impl HttpTransport for FakeTransport {
    /// 记录请求并由 handler 同步给出结果（包装成立即完成的 future）。
    fn send(&self, request: HttpRequest) -> HttpFuture {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        let result = (self.handler)(&request);
        Box::pin(async move { result })
    }
}
