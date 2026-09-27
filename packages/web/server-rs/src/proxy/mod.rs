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

mod headers;
mod realpath;
mod sanitize;
mod sse;

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
const DEFAULT_FALLBACK_PROXY_TARGET: &str = "http://127.0.0.1:3902";
/// index.js `OPEN_CODE_READY_GRACE_MS`.
const DEFAULT_READY_GRACE_MS: u64 = 12_000;
/// proxy.js `READINESS_HOLD_MAX_MS`.
const READINESS_HOLD_MAX_MS: u64 = 6_000;
/// index.js `LONG_REQUEST_TIMEOUT_MS` → proxy.js `PROXY_REQUEST_TIMEOUT_MS`.
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 4 * 60 * 1000;
/// proxy.js `INTERACTIVE_OAUTH_TIMEOUT_MS`.
const INTERACTIVE_OAUTH_TIMEOUT_MS: u64 = 15 * 60 * 1000;
/// proxy.js `TURN_BOUND_TIMEOUT_MS`.
const TURN_BOUND_TIMEOUT_MS: u64 = 6 * 60 * 60 * 1000;
/// proxy.js `DEFAULT_SSE_HEARTBEAT_INTERVAL_MS`.
const DEFAULT_SSE_HEARTBEAT_MS: u64 = 20_000;
/// event-stream `DEFAULT_UPSTREAM_STALL_TIMEOUT_MS`.
const DEFAULT_SSE_STALL_MS: u64 = 20_000;
/// proxy.js `TURN_BOUND_SESSION_ACTIONS`.
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
const MAX_FORWARDED_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
struct ProxySettings {
    ready_grace_ms: u64,
    request_timeout_ms: u64,
    oauth_timeout_ms: u64,
    turn_bound_timeout_ms: u64,
    sse_heartbeat_ms: u64,
    sse_stall_ms: u64,
    fallback_target: String,
}

impl Default for ProxySettings {
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

struct ProxyState {
    ctx: RouterContext,
    settings: ProxySettings,
    realpath: RealpathCache,
}

pub fn router(ctx: RouterContext) -> Router {
    build_router(ctx, ProxySettings::default())
}

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
async fn session_route(state: State<Arc<ProxyState>>, req: Request) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return session_list_handler(state, req).await;
    }
    generic_api_handler(state, req).await
}

/// `GET /api/session` + `GET /api/experimental/session` — sanitized session
/// list forwarding. On Windows the bare `/session` listing additionally
/// merges sessions from every configured project directory (JS parity).
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

struct SessionListFetch {
    status: StatusCode,
    headers: HeaderMap,
    content_type: String,
    body_text: String,
    is_json: bool,
    payload: Option<serde_json::Value>,
}

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

enum SseStep {
    Chunk(Bytes),
    Hub(HubEvent),
    HubClosed,
    Lagged(u64),
    Heartbeat,
    Stall,
    UpstreamError(reqwest::Error),
    End,
}

/// One downstream SSE response body: upstream bytes verbatim, downstream
/// `:heartbeat` comments, an upstream-only stall watchdog, and hub-published
/// server frames merged between upstream event blocks.
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

async fn canonicalized_upstream_path(st: &ProxyState, uri: &Uri) -> String {
    let raw = upstream_path(uri);
    canonicalize_directory_query(&st.realpath, &raw).await
}

fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "OpenCode service unavailable" })),
    )
        .into_response()
}

fn upstream_timed_out() -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(serde_json::json!({ "error": "OpenCode upstream timed out" })),
    )
        .into_response()
}

fn body_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(serde_json::json!({ "error": "Request body too large" })),
    )
        .into_response()
}

fn io_error(error: reqwest::Error) -> std::io::Error {
    std::io::Error::other(error)
}

#[cfg(test)]
mod tests;
