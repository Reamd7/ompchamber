//! Shared injectable HTTP POST seam for the push and APNs runtimes (the JS
//! modules call global `fetch` / `http2`; tests inject fakes the same way
//! the JS tests stub globals).
//!
//! 中文概述：push 与 APNs 运行时共用的可注入 HTTP POST 接缝。生产
//! 实现复用共享 reqwest 客户端（rustls + HTTP/2，APNs 直连需 h2）；
//! 测试注入录制型假实现，从而在不触网的情况下断言请求内容 —— 与
//! JS 测试 stub 全局 fetch 的手法一一对应。

use std::pin::Pin;
use std::sync::Arc;

/// Response facts the runtimes branch on: status code + body text.
/// 中文：只保留运行时分流所需的两个字段 —— 状态码驱动重试/清理
/// （如 410 触发订阅删除），body 供错误诊断与响应解析。
pub struct HttpPostResponse {
/// HTTP 状态码（如 201/200/410）。
    pub status: u16,
/// 响应体文本（已按 UTF-8 解码的字符串）。
    pub body: String,
}

/// POST 的 boxed future：必须 Send 以便在 tokio 任务间传递；
/// `Err(String)` 携带传输层错误原因。
pub type HttpPostFuture = Pin<Box<dyn Future<Output = Result<HttpPostResponse, String>> + Send>>;

/// `(url, headers, body) -> response`. Transport-level failures surface as
/// `Err(reason)` (the JS `fetch` throw / request error paths).
/// 中文：签名固定为 `(url, headers, body)`；`Arc<dyn Fn>` 让同一份
/// 假实现可同时注入 push 与 APNs 两个运行时并被分别断言。
pub type HttpPost =
    Arc<dyn Fn(&str, Vec<(String, String)>, Vec<u8>) -> HttpPostFuture + Send + Sync>;

/// Production transport over the shared reqwest client (rustls + HTTP/2,
/// so APNs direct mode negotiates h2 with Apple).
/// 中文：闭包按值捕获共享 client 以复用连接池；`send`/读 body 任一
/// 失败都映射为 `Err(e.to_string())`，与 JS fetch 抛错路径对齐。
pub fn reqwest_post(client: reqwest::Client) -> HttpPost {
    Arc::new(
        move |url: &str, headers: Vec<(String, String)>, body: Vec<u8>| {
            let mut request = client.post(url).body(body);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            Box::pin(async move {
                let response = request.send().await.map_err(|e| e.to_string())?;
                let status = response.status().as_u16();
                let body = response.text().await.map_err(|e| e.to_string())?;
                Ok(HttpPostResponse { status, body })
            })
        },
    )
}
