//! Test-only canned SSE server (shared by the upstream-reader and router
//! suites). Serves a scripted sequence of SSE attempts over a real local
//! HTTP listener so reader behavior (connect, headers, stall→reconnect) is
//! exercised against actual HTTP, not mocks.
//!
//! 中文概要：测试专用的脚本化 SSE 服务器。按脚本顺序在真实本地 HTTP 监听器上
//! 回应每次连接尝试（写块后结束 / 写块后挂起制造停滞 / 直接返回状态码），并
//! 记录请求头供断言，让读取器行为在真实 HTTP 栈上得到验证。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::pending;

/// 记录到的一次上游请求（断言游标/身份/鉴权请求头用）。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    /// 请求路径。
    pub path: String,
    /// Last-Event-ID 请求头（断点续传游标）。
    pub last_event_id: Option<String>,
    /// x-omp-epoch 请求头（启动身份回显）。
    pub epoch: Option<String>,
    /// authorization 请求头。
    pub authorization: Option<String>,
    /// accept 请求头。
    pub accept: Option<String>,
}

/// 单次连接尝试的脚本化回应。
#[derive(Debug, Clone)]
pub enum Attempt {
    /// 200 `text/event-stream` streaming the given blocks, then ending.
    /// 模拟一次正常服务完即关闭的上游。
    Respond {
        /// 依次写出的 SSE 块（原样字节）。
        blocks: Vec<String>,
        /// 响应中回显的 x-omp-epoch 值。
        epoch: Option<String>,
    },
    /// 200 `text/event-stream` streaming the blocks, then holding open
    /// forever (stall) until the client aborts.
    /// 用于触发读取器的停滞超时路径。
    RespondHolding {
        /// 挂起前写出的 SSE 块。
        blocks: Vec<String>,
        /// 响应中回显的 x-omp-epoch 值。
        epoch: Option<String>,
    },
    /// Immediate non-2xx response.
    /// 用于触发 UpstreamUnavailable 错误路径。
    Status(u16),
}

/// 脚本条目的便捷构造器。
impl Attempt {
    /// 写块后正常结束（无 epoch 回显）。
    pub fn respond(blocks: Vec<&str>) -> Self {
        Self::Respond {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: None,
        }
    }
    /// 写块后挂起（制造停滞）。
    pub fn respond_holding(blocks: Vec<&str>) -> Self {
        Self::RespondHolding {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: None,
        }
    }
    /// 写块后正常结束并回显指定 epoch。
    pub fn respond_with_epoch(blocks: Vec<&str>, epoch: &str) -> Self {
        Self::Respond {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: Some(epoch.to_string()),
        }
    }
    /// 立即返回给定状态码。
    pub fn status(code: u16) -> Self {
        Self::Status(code)
    }
}

/// axum 共享状态。
#[derive(Clone)]
struct ServerState {
    /// 待消费的脚本队列；每次请求弹出一个，耗尽后默认挂起。
    script: Arc<Mutex<VecDeque<Attempt>>>,
    /// 已记录的请求列表。
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// 脚本化服务器句柄（地址 + 状态 + 服务任务）。
pub struct CannedSseServer {
    /// 实际监听地址（随机端口）。
    addr: SocketAddr,
    /// 共享状态（读取请求记录）。
    state: ServerState,
    /// 服务任务句柄；仅保活，不 join。
    _serve: tokio::task::JoinHandle<()>,
}

/// 启动与查询接口。
impl CannedSseServer {
    /// 绑定 127.0.0.1:0 随机端口并启动 axum；任意路径都由同一 handler 处理。
    pub async fn start(script: Vec<Attempt>) -> Self {
        let state = ServerState {
            script: Arc::new(Mutex::new(script.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/{*path}", get(handle))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind canned SSE server");
        let addr = listener.local_addr().expect("local addr");
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            state,
            _serve: serve,
        }
    }

    /// 拼出指向本服务器的完整 URL。
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// 返回已记录请求的副本（断言请求头用）。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 取请求头并 trim；缺失或空串返回 None。
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

/// 先记录请求头，再弹出脚本条目回应：Status 直接返回状态码；Respond/RespondHolding
/// 以流式 body 写出块，后者用永不 resolve 的 future 挂住连接制造停滞。
async fn handle(State(state): State<ServerState>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    state
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(RecordedRequest {
            path: parts.uri.path().to_string(),
            last_event_id: header(&parts.headers, "last-event-id"),
            epoch: header(&parts.headers, "x-omp-epoch"),
            authorization: header(&parts.headers, "authorization"),
            accept: header(&parts.headers, "accept"),
        });

    let attempt = state
        .script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop_front()
        .unwrap_or(Attempt::RespondHolding {
            blocks: vec![],
            epoch: None,
        });
    let hold = matches!(&attempt, Attempt::RespondHolding { .. });

    match attempt {
        Attempt::Status(status) => StatusCode::from_u16(status)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
        Attempt::Respond { blocks, epoch } | Attempt::RespondHolding { blocks, epoch } => {
            let body = async_stream::stream! {
                for block in blocks {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(block));
                }
                if hold {
                    // Never resolves: the connection stalls until aborted.
                    pending::<()>().await;
                }
            };
            let mut response = Response::new(axum::body::Body::from_stream(body));
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            headers.insert(
                "content-type",
                "text/event-stream".parse().expect("valid header value"),
            );
            headers.insert(
                "cache-control",
                "no-cache".parse().expect("valid header value"),
            );
            if let Some(epoch) = epoch {
                headers.insert("x-omp-epoch", epoch.parse().expect("valid header value"));
            }
            response
        }
    }
}
