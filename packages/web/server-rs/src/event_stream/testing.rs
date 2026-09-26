//! Test-only canned SSE server (shared by the upstream-reader and router
//! suites). Serves a scripted sequence of SSE attempts over a real local
//! HTTP listener so reader behavior (connect, headers, stall→reconnect) is
//! exercised against actual HTTP, not mocks.

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

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    pub path: String,
    pub last_event_id: Option<String>,
    pub epoch: Option<String>,
    pub authorization: Option<String>,
    pub accept: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Attempt {
    /// 200 `text/event-stream` streaming the given blocks, then ending.
    Respond {
        blocks: Vec<String>,
        epoch: Option<String>,
    },
    /// 200 `text/event-stream` streaming the blocks, then holding open
    /// forever (stall) until the client aborts.
    RespondHolding {
        blocks: Vec<String>,
        epoch: Option<String>,
    },
    /// Immediate non-2xx response.
    Status(u16),
}

impl Attempt {
    pub fn respond(blocks: Vec<&str>) -> Self {
        Self::Respond {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: None,
        }
    }
    pub fn respond_holding(blocks: Vec<&str>) -> Self {
        Self::RespondHolding {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: None,
        }
    }
    pub fn respond_with_epoch(blocks: Vec<&str>, epoch: &str) -> Self {
        Self::Respond {
            blocks: blocks.into_iter().map(String::from).collect(),
            epoch: Some(epoch.to_string()),
        }
    }
    pub fn status(code: u16) -> Self {
        Self::Status(code)
    }
}

#[derive(Clone)]
struct ServerState {
    script: Arc<Mutex<VecDeque<Attempt>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

pub struct CannedSseServer {
    addr: SocketAddr,
    state: ServerState,
    _serve: tokio::task::JoinHandle<()>,
}

impl CannedSseServer {
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

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

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
