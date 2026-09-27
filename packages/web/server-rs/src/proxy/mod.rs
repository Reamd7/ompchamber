//! Port of `server/lib/opencode/proxy.js`: OpenCode wire-API forwarding.
//!
//! Routes (mirroring `registerOpenCodeProxy` registration order):
//! - `GET /api/session`, `GET /api/experimental/session` — sanitized session
//!   list forwarding (`forwardSanitizedSessionListRequest`).
//! - `GET /api/event`, `GET /api/global/event` — verbatim SSE pass-through
//!   with downstream heartbeats, an upstream stall watchdog, and
//!   server-emitted `ompchamber:*` frames from [`crate::hub::EventHub`]
//!   merged into the same stream.
//! - `POST /api/provider/{id}/oauth/callback`, `POST /api/mcp/{name}/auth/
//!   authenticate` — interactive-OAuth budget (15min).
//! - `POST /api/session/{id}/{action}` — turn-bound budget (6h) for
//!   `prompt|prompt_async|command|shell|summarize|init`.
//! - `ANY /api/{*path}` — generic forwarding with hop-by-hop header
//!   filtering (`proxy-headers.js`) and the ordinary 4-minute budget.
//!
//! Every proxied request passes the readiness gate first: while the engine is
//! warming up the request is HELD (bounded by `min(grace, 6s)`) instead of
//! answering 503 immediately, so cold-start callers do not spiral into
//! exponential backoff.
//!
//! 中文说明：OpenCode wire-API 的转发层。会话列表走脱敏转发；事件端点是
//! 逐字节的 SSE 直通（叠加下游心跳、上游静默看门狗与 hub 服务端合帧）；
//! 其余 /api/* 请求在就绪门（冷启动短暂扣住而非立即 503）之后原样转发；
//! OAuth 回调与回合级会话动作使用更长的时间预算。

/// 请求/响应头过滤、x-opencode-directory 归一化与百分号编解码。
mod headers;
/// directory 查询参数的 realpath 缓存与规范化。
mod realpath;
/// 会话列表响应的脱敏逻辑。
mod sanitize;
/// SSE 事件块边界跟踪。
mod sse;

/// Windows 专属的会话列表合并逻辑。
#[cfg(windows)]
mod windows;

use std::collections::VecDeque;
use std::sync::Arc;

use std::time::Duration;

use async_stream::stream;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use tokio::sync::broadcast;

use crate::context::RouterContext;
use crate::hub::HubEvent;
use crate::proxy::headers::{
    collect_forward_request_headers, forward_response_headers, normalize_directory_headers,
};
use crate::proxy::realpath::{RealpathCache, canonicalize_directory_query};
use crate::proxy::sanitize::sanitize_session_list_payload;
use crate::proxy::sse::SseBoundaryTracker;

/// JS `resolveProxyTarget` fallback target when no engine base URL is known.
/// 中文：引擎 base URL 未知时的兜底转发目标。
const DEFAULT_FALLBACK_PROXY_TARGET: &str = "http://127.0.0.1:3902";
/// index.js `OPEN_CODE_READY_GRACE_MS`.
/// 中文：等待引擎就绪的宽限基数，实际扣留时长还会被 6 秒上限截断。
const DEFAULT_READY_GRACE_MS: u64 = 12_000;
/// proxy.js `READINESS_HOLD_MAX_MS`.
/// 中文：就绪扣留的硬上限，引擎真挂了也能快速失败。
const READINESS_HOLD_MAX_MS: u64 = 6_000;
/// index.js `LONG_REQUEST_TIMEOUT_MS` → proxy.js `PROXY_REQUEST_TIMEOUT_MS`.
/// 中文：普通代理请求的时间预算（4 分钟）。
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 4 * 60 * 1000;
/// proxy.js `INTERACTIVE_OAUTH_TIMEOUT_MS`.
/// 中文：交互式 OAuth 路径的预算（15 分钟，等浏览器完成登录）。
const INTERACTIVE_OAUTH_TIMEOUT_MS: u64 = 15 * 60 * 1000;
/// proxy.js `TURN_BOUND_TIMEOUT_MS`.
/// 中文：回合级会话动作的预算（6 小时，等整个回合结束）。
const TURN_BOUND_TIMEOUT_MS: u64 = 6 * 60 * 60 * 1000;
/// proxy.js `DEFAULT_SSE_HEARTBEAT_INTERVAL_MS`.
/// 中文：下游 SSE 心跳间隔。
const DEFAULT_SSE_HEARTBEAT_MS: u64 = 20_000;
/// event-stream `DEFAULT_UPSTREAM_STALL_TIMEOUT_MS`.
/// 中文：上游静默看门狗阈值，超时即结束响应。
const DEFAULT_SSE_STALL_MS: u64 = 20_000;
/// proxy.js `TURN_BOUND_SESSION_ACTIONS`.
/// 中文：按回合计时的会话动作白名单。
const TURN_BOUND_SESSION_ACTIONS: &[&str] = &[
    "prompt",
    "prompt_async",
    "command",
    "shell",
    "summarize",
    "init",
];
/// The JS streams request bodies through http-proxy; this port buffers them
/// (reqwest wants a known length) behind a generous ceiling.
/// 中文：请求体缓冲上限（64 MiB）；JS 走流式转发，reqwest 需要已知
/// 长度故改为先缓冲。
const MAX_FORWARDED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 代理行为参数：各路预算、SSE 心跳/看门狗间隔与兜底目标；测试可
/// 逐项覆盖。
#[derive(Clone, Debug)]
struct ProxySettings {
    /// 引擎未就绪时的最大扣留时长基数。
    ready_grace_ms: u64,
    /// 普通请求的转发预算。
    request_timeout_ms: u64,
    /// 交互式 OAuth 路径的转发预算。
    oauth_timeout_ms: u64,
    /// 回合级会话动作的转发预算。
    turn_bound_timeout_ms: u64,
    /// 下游 SSE 心跳间隔。
    sse_heartbeat_ms: u64,
    /// 上游静默看门狗阈值。
    sse_stall_ms: u64,
    /// 引擎无 base URL 时的兜底转发目标。
    fallback_target: String,
}

/// 默认值全部取自 index.js 与 proxy.js 的同名常量。
impl Default for ProxySettings {
    /// 按 index.js 与 proxy.js 的同名常量填充各字段。
    fn default() -> Self {
        Self {
            ready_grace_ms: DEFAULT_READY_GRACE_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            oauth_timeout_ms: INTERACTIVE_OAUTH_TIMEOUT_MS,
            turn_bound_timeout_ms: TURN_BOUND_TIMEOUT_MS,
            sse_heartbeat_ms: DEFAULT_SSE_HEARTBEAT_MS,
            sse_stall_ms: DEFAULT_SSE_STALL_MS,
            fallback_target: DEFAULT_FALLBACK_PROXY_TARGET.to_string(),
        }
    }
}

/// proxy 路由共享的状态：路由上下文、行为参数与 directory 规范化缓存。
struct ProxyState {
    /// 引擎状态、hub 等共享上下文。
    ctx: RouterContext,
    /// 预算与 SSE 参数。
    settings: ProxySettings,
    /// directory 查询参数的 realpath 缓存。
    realpath: RealpathCache,
}

/// 以默认参数构建 proxy 路由（生产入口）。
pub fn router(ctx: RouterContext) -> Router {
    build_router(ctx, ProxySettings::default())
}

/// 注册全部 /api 转发路由并挂上 UI 鉴权中间件；settings 参数允许测试
/// 覆盖预算与兜底目标。
fn build_router(ctx: RouterContext, settings: ProxySettings) -> Router {
    let gate = crate::ui_auth::middleware(ctx.clone());
    let state = Arc::new(ProxyState {
        ctx,
        settings,
        realpath: RealpathCache::new(),
    });
    Router::new()
        .route("/api/session", any(session_route))
        .route("/api/experimental/session", any(session_route))
        .route("/api/event", get(sse_handler))
        .route("/api/global/event", get(sse_handler))
        .route(
            "/api/provider/{provider_id}/oauth/callback",
            any(interactive_oauth_handler),
        )
        .route(
            "/api/mcp/{name}/auth/authenticate",
            any(interactive_oauth_handler),
        )
        .route(
            "/api/session/{session_id}/{action}",
            any(session_action_handler),
        )
        .route("/api/{*rest}", any(generic_api_handler))
        // JS composition wires `app.use('/api', requireApiAuth)` ahead of the
        // proxy (core-routes.js) — the shared gate layer reproduces that for
        // this router's routes and is a no-op while no UI password is set.
        .route_layer(gate)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Generic `/api/*` catch-all: readiness gate + directory canonicalization +
/// the ordinary request budget, then forwarded verbatim.
/// 中文：/api/* 兜底 handler——先过就绪门，再做 directory 查询规范化，
/// 最后按普通预算原样转发。
async fn generic_api_handler(State(st): State<Arc<ProxyState>>, req: Request) -> Response {
    let uri = req.uri().clone();
    if let Err(denied) = readiness_gate(&st, &api_relative_path(&uri)).await {
        return denied;
    }
    let upstream_path = canonicalized_upstream_path(&st, &uri).await;
    forward_generic(&st, req, &upstream_path, st.settings.request_timeout_ms).await
}

/// Interactive OAuth flows block upstream for the whole browser sign-in, so
/// Interactive OAuth flows block upstream for the whole browser sign-in, so
/// POSTs on these paths run on the 15-minute budget instead of the ordinary
/// deadline; every other method falls through to the generic budget (JS:
/// `app.post(...)`, everything else hits `app.use('/api', apiProxy)`).
/// 中文：交互式 OAuth 路径——POST 走 15 分钟预算，其余方法落回普通
/// 预算（对齐 JS 先 app.post 注册、其余进 use 的组合方式）。
async fn interactive_oauth_handler(State(st): State<Arc<ProxyState>>, req: Request) -> Response {
    let uri = req.uri().clone();
    if let Err(denied) = readiness_gate(&st, &api_relative_path(&uri)).await {
        return denied;
    }
    let upstream_path = canonicalized_upstream_path(&st, &uri).await;
    let budget_ms = if req.method() == Method::POST {
        st.settings.oauth_timeout_ms
    } else {
        st.settings.request_timeout_ms
    };
    forward_generic(&st, req, &upstream_path, budget_ms).await
}

/// Session turns answer when the *turn* settles, not when the engine accepts
/// the request, so turn actions get the six-hour budget; every other session
/// action falls back to the ordinary one (JS: `next()` into the api proxy).
/// 中文：会话动作路由——POST 且动作在白名单内走 6 小时回合预算，
/// 其余动作回落普通预算。
async fn session_action_handler(State(st): State<Arc<ProxyState>>, req: Request) -> Response {
    let uri = req.uri().clone();
    if let Err(denied) = readiness_gate(&st, &api_relative_path(&uri)).await {
        return denied;
    }
    let upstream_path = canonicalized_upstream_path(&st, &uri).await;
    let action = uri
        .path()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let is_turn_bound =
        req.method() == Method::POST && TURN_BOUND_SESSION_ACTIONS.contains(&action.as_str());
    let budget_ms = if is_turn_bound {
        st.settings.turn_bound_timeout_ms
    } else {
        st.settings.request_timeout_ms
    };
    forward_generic(&st, req, &upstream_path, budget_ms).await
}

/// JS composition: `app.get('/api/session')` terminates GETs with the
/// sanitized list; every other method falls through `app.use('/api', apiProxy)`
/// to the engine (`POST /session` creates a session — the desktop chat
/// composer's first request). An axum `get(...)` static route would answer
/// 405 for those instead of falling through, so dispatch by method here.
/// 中文：/api/session 的方法分派——GET/HEAD 返回脱敏列表，其余方法落到
/// 通用转发；axum 静态 get 路由会答 405 而无法穿透，故手动分派。
async fn session_route(state: State<Arc<ProxyState>>, req: Request) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return session_list_handler(state, req).await;
    }
    generic_api_handler(state, req).await
}

/// `GET /api/session` + `GET /api/experimental/session` — sanitized session
/// list forwarding. On Windows the bare `/session` listing additionally
/// merges sessions from every configured project directory (JS parity).
/// 中文：会话列表脱敏转发；Windows 下无 directory 参数的裸列表还会合并
/// 各项目目录的会话（对齐 JS 行为）。
async fn session_list_handler(State(st): State<Arc<ProxyState>>, req: Request) -> Response {
    let uri = req.uri().clone();
    if let Err(denied) = readiness_gate(&st, &api_relative_path(&uri)).await {
        return denied;
    }
    let label = if uri.path() == "/api/experimental/session" {
        "experimental.session"
    } else {
        "session.list"
    };

    #[cfg(windows)]
    {
        let raw = uri.path_and_query().map(|v| v.as_str()).unwrap_or_default();
        if label == "session.list" && !raw.contains("directory=") {
            return windows::merge_session_list(&st, req.headers()).await;
        }
    }

    forward_session_list(&st, &uri, req.headers(), label).await
}

/// `GET /api/event` + `GET /api/global/event` — SSE pass-through.
/// 中文：SSE 直通——上游声明不是事件流时按普通字节流原样回传；事件流
/// 则补齐 nginx 友好响应头，并交给 merged_sse_stream 合并心跳、看门狗
/// 与 hub 帧。
async fn sse_handler(State(st): State<Arc<ProxyState>>, req: Request) -> Response {
    let uri = req.uri().clone();
    if let Err(denied) = readiness_gate(&st, &api_relative_path(&uri)).await {
        return denied;
    }
    // forwardSseRequest does NOT canonicalize the directory query.
    let upstream_path = upstream_path(&uri);
    let base = resolve_proxy_target(&st);
    let mut headers =
        collect_forward_request_headers(req.headers(), st.ctx.engine.auth_header().as_deref());
    normalize_directory_headers(&mut headers);
    headers
        .entry(axum::http::header::ACCEPT)
        .or_insert_with(|| HeaderValue::from_static("text/event-stream"));
    headers
        .entry(axum::http::header::CACHE_CONTROL)
        .or_insert_with(|| HeaderValue::from_static("no-cache"));

    let hub_rx = st.ctx.hub.subscribe();

    let upstream = st
        .ctx
        .engine
        .http()
        .get(format!("{base}{upstream_path}"))
        .headers(headers)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            tracing::error!("[proxy] OpenCode SSE proxy error: {error}");
            return service_unavailable();
        }
    };

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "text/event-stream".to_string());
    let is_event_stream = content_type
        .to_ascii_lowercase()
        .contains("text/event-stream");
    let forwarded = forward_response_headers(upstream.headers());

    if !is_event_stream {
        // JS buffers via text() and ends the response — byte-identical
        // pass-through of status, filtered headers and body.
        let body = Body::from_stream(upstream.bytes_stream().map(|chunk| chunk.map_err(io_error)));
        return build_response(status, forwarded, body);
    }

    // nginx-safe SSE headers (JS also sets Connection: keep-alive, which is
    // the HTTP/1.1 default under hyper and managed by the transport).
    let mut out = forwarded;
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        out.insert(axum::http::header::CONTENT_TYPE, value);
    }
    out.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    out.insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );

    let stream = merged_sse_stream(st.settings.clone(), upstream, hub_rx);
    build_response(status, out, Body::from_stream(stream))
}

// ---------------------------------------------------------------------------
// Generic forwarding
// ---------------------------------------------------------------------------

/// 通用转发核心：重组请求头（过滤 + 引擎鉴权 + directory 归一化 +
/// identity 编码），缓冲请求体后按给定预算发给上游；超时答 504、请求体
/// 超限答 413、其余上游错误答 503，成功时过滤响应头后流式回传。
async fn forward_generic(
    st: &ProxyState,
    req: Request,
    upstream_path: &str,
    timeout_ms: u64,
) -> Response {
    let base = resolve_proxy_target(st);
    let url = format!("{base}{upstream_path}");
    let method = req.method().clone();
    let mut headers =
        collect_forward_request_headers(req.headers(), st.ctx.engine.auth_header().as_deref());
    normalize_directory_headers(&mut headers);
    // Defensive: identity encoding avoids compressed-body/header mismatches
    // in multi-proxy setups (proxy.js proxyReq).
    headers.insert(
        axum::http::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );

    let has_body = method != Method::GET && method != Method::HEAD;
    let body = req.into_body();
    let request = st
        .ctx
        .engine
        .http()
        .request(method, url)
        .headers(headers)
        .timeout(Duration::from_millis(timeout_ms));

    let sent = if has_body {
        match to_bytes(body, MAX_FORWARDED_BODY_BYTES).await {
            Ok(bytes) => request.body(bytes).send().await,
            Err(error) => {
                tracing::error!(
                    "[proxy] OpenCode proxy error: failed reading request body: {error}"
                );
                return body_too_large();
            }
        }
    } else {
        request.send().await
    };

    match sent {
        Ok(upstream) => {
            let status = upstream.status();
            let headers = forward_response_headers(upstream.headers());
            let body =
                Body::from_stream(upstream.bytes_stream().map(|chunk| chunk.map_err(io_error)));
            build_response(status, headers, body)
        }
        Err(error) if error.is_timeout() => {
            tracing::error!("[proxy] OpenCode proxy error: {error}");
            upstream_timed_out()
        }
        Err(error) => {
            tracing::error!("[proxy] OpenCode proxy error: {error}");
            service_unavailable()
        }
    }
}

// ---------------------------------------------------------------------------
// Sanitized session list
// ---------------------------------------------------------------------------

/// 一次会话列表上游响应的快照：状态、头、原文与尽力解析出的 JSON。
struct SessionListFetch {
    /// 上游状态码。
    status: StatusCode,
    /// 上游响应头（未过滤的原始集）。
    headers: HeaderMap,
    /// Content-Type 原文（缺省按 JSON 兜底）。
    content_type: String,
    /// 响应体原文（非 JSON 或解析失败时原样回传）。
    body_text: String,
    /// Content-Type 是否声明为 JSON。
    is_json: bool,
    /// 解析成功的 JSON 值，仅 is_json 时可能有。
    payload: Option<serde_json::Value>,
}

/// 拉取上游会话列表并快照：注入引擎鉴权与 JSON Accept 头，可选超时；
/// 网络错误原样上抛，由调用方决定应答。
async fn fetch_session_list(
    st: &ProxyState,
    upstream_path: &str,
    req_headers: &HeaderMap,
    timeout_ms: Option<u64>,
) -> Result<SessionListFetch, reqwest::Error> {
    let base = resolve_proxy_target(st);
    let mut headers =
        collect_forward_request_headers(req_headers, st.ctx.engine.auth_header().as_deref());
    normalize_directory_headers(&mut headers);
    headers.insert(
        axum::http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        axum::http::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );

    let mut request = st
        .ctx
        .engine
        .http()
        .get(format!("{base}{upstream_path}"))
        .headers(headers);
    if let Some(timeout_ms) = timeout_ms {
        request = request.timeout(Duration::from_millis(timeout_ms));
    }

    let response = request.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "application/json; charset=utf-8".to_string());
    let body_text = response.text().await?;
    let is_json = content_type
        .to_ascii_lowercase()
        .contains("application/json");
    let payload = if is_json {
        serde_json::from_str(&body_text).ok()
    } else {
        None
    };
    Ok(SessionListFetch {
        status,
        headers,
        content_type,
        body_text,
        is_json,
        payload,
    })
}

/// 会话列表对外应答：上游成功且 payload 是 JSON 数组时做脱敏回写；其余
/// 情况按原文透传，Content-Type 统一采用上游值。
async fn forward_session_list(
    st: &ProxyState,
    uri: &Uri,
    req_headers: &HeaderMap,
    label: &str,
) -> Response {
    let upstream_path = canonicalized_upstream_path(st, uri).await;
    let result = match fetch_session_list(st, &upstream_path, req_headers, None).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!("[proxy] OpenCode {label} proxy error: {error}");
            return service_unavailable();
        }
    };

    let status = result.status;
    let mut headers = forward_response_headers(&result.headers);
    // JS: res.setHeader('content-type', result.contentType) — applies the
    // upstream content type (or the JSON default) in every branch.
    if let Ok(value) = HeaderValue::from_str(&result.content_type) {
        headers.insert(axum::http::header::CONTENT_TYPE, value);
    }

    match &result.payload {
        Some(serde_json::Value::Array(items)) if result.is_json => {
            let sanitized = sanitize_session_list_payload(&serde_json::Value::Array(items.clone()));
            build_response(status, headers, Body::from(sanitized.to_string()))
        }
        // Parse failures and non-array payloads pass through verbatim.
        _ => build_response(status, headers, Body::from(result.body_text.clone())),
    }
}

// ---------------------------------------------------------------------------
// Readiness gate
// ---------------------------------------------------------------------------

/// 就绪门：引擎未就绪时扣住请求至多 min(grace, 6s) 等待恢复，超时返回
/// 503 与 restarting 错误体；豁免路径直接放行。
async fn readiness_gate(st: &ProxyState, api_path: &str) -> Result<(), Response> {
    if is_gate_exempt(api_path) || st.ctx.engine.is_ready() {
        return Ok(());
    }
    // HOLD the request while OpenCode starts/restarting instead of answering
    // 503 immediately — a bare 503 pushes clients into exponential backoff
    // and wastes cold-start seconds. Bounded so genuinely-down engines fail
    // fast (proxy.js `READINESS_HOLD_*`).
    let hold = Duration::from_millis(st.settings.ready_grace_ms.min(READINESS_HOLD_MAX_MS));
    if st.ctx.engine.wait_ready(hold).await.is_ok() {
        return Ok(());
    }
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "OpenCode is restarting",
            "restarting": true,
        })),
    )
        .into_response())
}

/// Paths owned by other modules (or intentionally gate-free) — proxy.js's
/// exemption list for the readiness hold.
/// 中文：这些前缀归其它模块所有或有意免门，跳过就绪扣留。
fn is_gate_exempt(api_path: &str) -> bool {
    api_path.starts_with("/themes/custom")
        || api_path.starts_with("/push")
        || api_path.starts_with("/config/agents")
        || api_path.starts_with("/config/opencode-resolution")
        || api_path.starts_with("/config/settings")
        || api_path.starts_with("/config/skills")
        || api_path == "/config/reload"
        || api_path == "/health"
}

// ---------------------------------------------------------------------------
// SSE stream (verbatim pass-through + heartbeat + stall + hub merge)
// ---------------------------------------------------------------------------

/// SSE 泵每轮 select 得出的一步：上游块、hub 事件、心跳、看门狗等。
enum SseStep {
    /// 上游产生的一块非空字节。
    Chunk(Bytes),
    /// hub 广播的一条服务器事件。
    Hub(HubEvent),
    /// hub 通道已关闭，停止订阅。
    HubClosed,
    /// 广播积压丢帧（记日志，不中断流）。
    Lagged(u64),
    /// 到点发送下游心跳。
    Heartbeat,
    /// 上游超过阈值无字节，终止响应。
    Stall,
    /// 上游读流出错，终止响应。
    UpstreamError(reqwest::Error),
    /// 上游流正常结束。
    End,
}

/// One downstream SSE response body: upstream bytes verbatim, downstream
/// `:heartbeat` comments, an upstream-only stall watchdog, and hub-published
/// server frames merged between upstream event blocks.
/// 中文：构造下游 SSE 响应体——上游字节逐字透传；hub 帧只在事件块边界
/// 插入以免拆帧；心跳到点但处于事件中间时跳过；上游静默超阈值或出错
/// 时结束流，促使客户端重连。
fn merged_sse_stream(
    settings: ProxySettings,
    upstream: reqwest::Response,
    mut hub_rx: broadcast::Receiver<HubEvent>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    stream! {
        let mut upstream = upstream.bytes_stream();
        let mut boundary = SseBoundaryTracker::new();
        let mut pending_hub: VecDeque<HubEvent> = VecDeque::new();
        let mut hub_open = true;
        let heartbeat_every = Duration::from_millis(settings.sse_heartbeat_ms);
        let stall_after = Duration::from_millis(settings.sse_stall_ms);
        let mut next_heartbeat = tokio::time::Instant::now() + heartbeat_every;
        let mut stall_deadline = tokio::time::Instant::now() + stall_after;

        loop {
            // Server-emitted frames merge into the same stream, but only at
            // event boundaries so no upstream frame is ever split.
            while boundary.is_at_boundary() {
                let Some(event) = pending_hub.pop_front() else { break };
                yield Ok(Bytes::from(format!(
                    "event: {}\ndata: {}\n\n",
                    event.event, event.data
                )));
            }

            let step = tokio::select! {
                biased;
                chunk = upstream.next() => match chunk {
                    Some(Ok(bytes)) if !bytes.is_empty() => SseStep::Chunk(bytes),
                    // Empty chunks neither yield nor reset the stall timer.
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => SseStep::UpstreamError(error),
                    None => SseStep::End,
                },
                event = hub_rx.recv(), if hub_open => match event {
                    Ok(event) => SseStep::Hub(event),
                    Err(broadcast::error::RecvError::Lagged(missed)) => SseStep::Lagged(missed),
                    Err(broadcast::error::RecvError::Closed) => SseStep::HubClosed,
                },
                _ = tokio::time::sleep_until(next_heartbeat) => SseStep::Heartbeat,
                _ = tokio::time::sleep_until(stall_deadline) => SseStep::Stall,
            };

            match step {
                SseStep::Chunk(bytes) => {
                    boundary.observe(&bytes);
                    stall_deadline = tokio::time::Instant::now() + stall_after;
                    yield Ok(bytes);
                }
                SseStep::Hub(event) => pending_hub.push_back(event),
                SseStep::HubClosed => hub_open = false,
                SseStep::Lagged(missed) => {
                    tracing::warn!("[proxy] SSE hub subscriber lagged, dropped {missed} frame(s)");
                }
                SseStep::Heartbeat => {
                    next_heartbeat = tokio::time::Instant::now() + heartbeat_every;
                    // JS skips the beat when mid-event instead of forcing one.
                    if boundary.is_at_boundary() {
                        yield Ok(Bytes::from_static(b":heartbeat\n\n"));
                    }
                }
                SseStep::Stall => {
                    // Upstream stopped producing bytes despite our own
                    // heartbeats: end the response so clients reconnect
                    // instead of trusting synthetic heartbeats forever.
                    break;
                }
                SseStep::UpstreamError(error) => {
                    tracing::error!("[proxy] OpenCode SSE proxy error: {error}");
                    break;
                }
                SseStep::End => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Generic proxy requests stay on the same upstream base URL health checks
/// use; when the engine is not ready (cold start, restart), the JS falls back
/// to the loopback default rather than failing to build a URL.
/// 中文：取引擎 base URL 并去掉尾部斜杠；未知或为空时使用兜底目标。
fn resolve_proxy_target(st: &ProxyState) -> String {
    let fallback = st
        .settings
        .fallback_target
        .trim_end_matches('/')
        .to_string();
    st.ctx
        .engine
        .base_url()
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
        .unwrap_or(fallback)
}

/// The request path relative to the `/api` mount (JS `req.path` inside
/// `app.use('/api', ...)`), without the query string.
/// 中文：剥掉 /api 前缀后的相对路径（不含查询串），供就绪门做豁免匹配。
fn api_relative_path(uri: &Uri) -> String {
    let path = uri.path();
    let stripped = path.strip_prefix("/api").unwrap_or(path);
    if stripped.is_empty() {
        "/".to_string()
    } else {
        stripped.to_string()
    }
}

/// The upstream path+query: `/api` prefix stripped (JS `pathRewrite` /
/// `requestUrl.slice(4)`), query preserved verbatim.
/// 中文：上游路径 + 查询串——剥掉 /api 前缀，查询部分原样保留。
fn upstream_path(uri: &Uri) -> String {
    let path_and_query = uri.path_and_query().map(|v| v.as_str()).unwrap_or("/");
    let stripped = path_and_query
        .strip_prefix("/api")
        .unwrap_or(path_and_query);
    if stripped.is_empty() {
        "/".to_string()
    } else {
        stripped.to_string()
    }
}

/// 上游路径 + directory 查询参数的 realpath 规范化。
async fn canonicalized_upstream_path(st: &ProxyState, uri: &Uri) -> String {
    let raw = upstream_path(uri);
    canonicalize_directory_query(&st.realpath, &raw).await
}

/// 以给定状态、头、体组装 Response。
fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// 503 与 JS 同款错误体的便捷构造。
fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "OpenCode service unavailable" })),
    )
        .into_response()
}

/// 504 与 JS 同款错误体的便捷构造。
fn upstream_timed_out() -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(serde_json::json!({ "error": "OpenCode upstream timed out" })),
    )
        .into_response()
}

/// 413：请求体超过缓冲上限。
fn body_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(serde_json::json!({ "error": "Request body too large" })),
    )
        .into_response()
}

/// 把 reqwest 错误适配为 io::Error，供流式响应体使用。
fn io_error(error: reqwest::Error) -> std::io::Error {
    std::io::Error::other(error)
}

/// 路由级集成测试：真实 TCP 上游 + 预置响应字节。
#[cfg(test)]
mod tests;
