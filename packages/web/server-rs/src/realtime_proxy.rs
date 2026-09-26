//! Port of `server/lib/realtime-proxy.js`.
//!
//! Desktop realtime proxy: the Electron desktop runs the web server in-process
//! and forwards a fixed allowlist of realtime endpoints (SSE + WS) to its own
//! loopback UI runtime. Target resolution is fail-closed: a `?url=` param is
//! honored only when the desktop runtime config exists, the path is
//! allowlisted for the transport type, the scheme matches, and the target
//! origin equals the runtime's API base origin. The `authorization` header is
//! never forwarded.
//!
//! Desktop runtime config stand-in: the JS receives an injected
//! `getDesktopRuntimeConfig()` from Electron. The standalone Rust server reads
//! `OMPCHAMBER_DESKTOP_API_BASE_URL` + `OMPCHAMBER_DESKTOP_REQUEST_HEADERS`
//! (JSON object) — set by the desktop shell that embeds this binary.

use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use futures::{SinkExt, StreamExt};
use serde_json::json;

const PROXY_SSE_PATH: &str = "/api/ompchamber/realtime-proxy/sse";
const PROXY_WS_PATH: &str = "/api/ompchamber/realtime-proxy/ws";

pub fn is_allowed_sse_path(pathname: &str) -> bool {
    matches!(
        pathname,
        "/api/event" | "/api/global/event" | "/api/ompchamber/events" | "/api/notifications/stream"
    )
}

pub fn is_allowed_web_socket_path(pathname: &str) -> bool {
    matches!(
        pathname,
        "/api/event/ws" | "/api/global/event/ws" | "/api/terminal/ws"
    )
}

fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

/// sanitizeHeaders: drop empty values, header-injection characters, and
/// `authorization`.
pub fn sanitize_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
    let mut next = HashMap::new();
    for (name, value) in headers {
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            continue;
        }
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        next.insert(name.to_string(), value.to_string());
    }
    next
}

#[derive(Debug, Clone, Default)]
pub struct DesktopRuntimeConfig {
    pub api_base_url: String,
    pub request_headers: HashMap<String, String>,
}

impl DesktopRuntimeConfig {
    /// Stand-in for Electron's injected config provider (see module docs).
    pub fn from_env() -> Option<Self> {
        let api_base_url = std::env::var("OMPCHAMBER_DESKTOP_API_BASE_URL").ok()?;
        let api_base_url = normalize_base_url(&api_base_url);
        if api_base_url.is_empty() {
            return None;
        }
        let request_headers = std::env::var("OMPCHAMBER_DESKTOP_REQUEST_HEADERS")
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .map(|headers| sanitize_headers(&headers))
            .filter(|headers| !headers.is_empty())?;
        Some(Self {
            api_base_url,
            request_headers,
        })
    }
}

fn urls_match_runtime(target: &url::Url, api_base_url: &str) -> bool {
    let base = normalize_base_url(api_base_url);
    if base.is_empty() {
        return false;
    }
    let Ok(mut target_for_compare) = url::Url::parse(target.as_ref()) else {
        return false;
    };
    if target_for_compare.scheme() == "ws" {
        let _ = target_for_compare.set_scheme("http");
    } else if target_for_compare.scheme() == "wss" {
        let _ = target_for_compare.set_scheme("https");
    }
    let Ok(base_url) = url::Url::parse(&base) else {
        return false;
    };
    target_for_compare.origin() == base_url.origin()
}

fn protocol_matches_proxy_type(target: &url::Url, ws: bool) -> bool {
    if ws {
        matches!(target.scheme(), "ws" | "wss")
    } else {
        matches!(target.scheme(), "http" | "https")
    }
}

fn path_matches_proxy_type(target: &url::Url, ws: bool) -> bool {
    if ws {
        is_allowed_web_socket_path(target.path())
    } else {
        is_allowed_sse_path(target.path())
    }
}

/// Fail-closed target resolution for a request's `?url=` param.
pub fn resolve_proxy_target(
    raw_url: Option<&str>,
    config: Option<&DesktopRuntimeConfig>,
    ws: bool,
) -> Option<(url::Url, HashMap<String, String>)> {
    let config = config?;
    if config.request_headers.is_empty() {
        return None;
    }
    let raw = raw_url?.trim();
    if raw.is_empty() {
        return None;
    }
    let target = url::Url::parse(raw).ok()?;
    if !protocol_matches_proxy_type(&target, ws) {
        return None;
    }
    if !path_matches_proxy_type(&target, ws) {
        return None;
    }
    if !urls_match_runtime(&target, &config.api_base_url) {
        return None;
    }
    Some((target, config.request_headers.clone()))
}

pub fn build_realtime_proxy_sse_url(local_origin: &str, target_url: &str) -> Option<String> {
    let mut url = url::Url::parse(local_origin).ok()?;
    url.set_path(PROXY_SSE_PATH);
    url.query_pairs_mut().clear().append_pair("url", target_url);
    Some(url.to_string())
}

pub fn build_realtime_proxy_ws_url(local_origin: &str, target_url: &str) -> Option<String> {
    let mut url = url::Url::parse(local_origin).ok()?;
    url.set_path(PROXY_WS_PATH);
    url.query_pairs_mut().clear().append_pair("url", target_url);
    if url.scheme() == "https" {
        let _ = url.set_scheme("wss");
    } else {
        let _ = url.set_scheme("ws");
    }
    Some(url.to_string())
}

#[derive(Clone)]
struct ModuleState {
    config: Option<DesktopRuntimeConfig>,
}

fn sse_error(status: axum::http::StatusCode, message: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (status, axum::Json(json!({ "error": message }))).into_response()
}

async fn proxy_sse(State(state): State<ModuleState>, request: Request) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    // The gate (ui_auth middleware) already ran for /api paths; the JS ALSO
    // origin-checks here — replicate the 403 shape.
    let (parts, _body) = request.into_parts();
    let origin_allowed = crate::ui_auth::is_request_origin_allowed(&parts);
    let raw_url = parts
        .uri
        .query()
        .and_then(|query| form_urlencoded_lookup(query, "url"))
        .unwrap_or_default();
    let accept = parts
        .headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let last_event_id = parts
        .headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    if !origin_allowed {
        return sse_error(
            StatusCode::FORBIDDEN,
            "Realtime proxy origin is not allowed",
        );
    }

    let Some((target, mut headers)) =
        resolve_proxy_target(Some(&raw_url), state.config.as_ref(), false)
    else {
        return sse_error(StatusCode::NOT_FOUND, "Realtime proxy is unavailable");
    };

    // Accept + Last-Event-ID pass through; runtime headers win on conflict.
    if let Some(accept) = accept {
        headers.entry("Accept".to_string()).or_insert(accept);
    }
    if let Some(last_event_id) = last_event_id {
        headers
            .entry("Last-Event-ID".to_string())
            .or_insert(last_event_id);
    }

    let mut upstream = state_upstream_client().get(target.to_string());
    for (name, value) in &headers {
        upstream = upstream.header(name, value);
    }
    let response = match upstream.send().await {
        Ok(response) => response,
        Err(error) => return sse_error(StatusCode::BAD_GATEWAY, &error.to_string()),
    };
    if !response.status().is_success() {
        return (
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            "",
        )
            .into_response();
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();
    let stream = response.bytes_stream();
    let body = Body::from_stream(stream);
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(&content_type)
                    .unwrap_or(axum::http::HeaderValue::from_static("text/event-stream")),
            ),
            (
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            ),
            (
                header::CONNECTION,
                axum::http::HeaderValue::from_static("keep-alive"),
            ),
        ],
        body,
    )
        .into_response()
}

fn state_upstream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    });
    &CLIENT
}
fn form_urlencoded_lookup(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if percent_decode_loose(name) == key {
            return Some(percent_decode_loose(value));
        }
    }
    None
}

fn percent_decode_loose(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'+' {
            out.push(b' ');
            index += 1;
        } else if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
            out.push(bytes[index]);
            index += 1;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn proxy_ws(
    State(state): State<ModuleState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    upgrade: WebSocketUpgrade,
) -> axum::response::Response {
    let raw_url = query
        .as_deref()
        .and_then(|query| form_urlencoded_lookup(query, "url"))
        .unwrap_or_default();
    let Some((target, headers)) = resolve_proxy_target(Some(&raw_url), state.config.as_ref(), true)
    else {
        return upgrade.on_upgrade(|socket| async move {
            let _ = close_with(socket, 1008, "Realtime proxy is unavailable").await;
        });
    };
    let target = target.to_string();
    upgrade.on_upgrade(move |client| async move {
        if let Err(error) = proxy_ws_bridge(client, &target, &headers).await {
            tracing::warn!("[realtime-proxy] ws bridge ended: {error}");
        }
    })
}

async fn close_with(mut socket: WebSocket, code: u16, reason: &str) -> Result<(), axum::Error> {
    socket
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await
}

/// Bidirectional bridge: client ↔ upstream, close-code and error propagation
/// mirrored from the JS (upstream close ⇒ same code; upstream error ⇒ 1011).
async fn proxy_ws_bridge(
    mut client: WebSocket,
    target: &str,
    headers: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(target)?;
    for (name, value) in headers {
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())?,
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value)?,
        );
    }
    let (mut upstream, _response) = tokio_tungstenite::connect_async(request).await?;

    loop {
        tokio::select! {
            from_client = client.next() => {
                match from_client {
                    Some(Ok(message)) => {
                        if let Some(converted) = convert_axum_to_tungstenite(message)
                            && upstream.send(converted).await.is_err() {
                                return Ok(());
                            }
                    }
                    Some(Err(_)) | None => {
                        let _ = upstream.close(None).await;
                        return Ok(());
                    }
                }
            }
            from_upstream = upstream.next() => {
                match from_upstream {
                    Some(Ok(message)) => {
                        if let Some(converted) = convert_tungstenite_to_axum(message)
                            && client.send(converted).await.is_err() {
                                return Ok(());
                            }
                    }
                    Some(Err(_)) => {
                        let _ = close_with(client, 1011, "Realtime proxy upstream error").await;
                        return Ok(());
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

fn convert_axum_to_tungstenite(
    message: Message,
) -> Option<tokio_tungstenite::tungstenite::Message> {
    use tokio_tungstenite::tungstenite::Message as Tm;
    Some(match message {
        Message::Text(text) => Tm::Text(text.as_str().to_string()),
        Message::Binary(bytes) => Tm::Binary(bytes.to_vec()),
        Message::Ping(bytes) => Tm::Ping(bytes.to_vec()),
        Message::Pong(bytes) => Tm::Pong(bytes.to_vec()),
        Message::Close(_) => return None,
    })
}

fn convert_tungstenite_to_axum(
    message: tokio_tungstenite::tungstenite::Message,
) -> Option<Message> {
    use tokio_tungstenite::tungstenite::Message as Tm;
    Some(match message {
        Tm::Text(text) => Message::Text(text.as_str().to_string().into()),
        Tm::Binary(bytes) => Message::Binary(bytes.to_vec().into()),
        Tm::Ping(bytes) => Message::Ping(bytes.to_vec().into()),
        Tm::Pong(bytes) => Message::Pong(bytes.to_vec().into()),
        Tm::Close(_) => return None,
        Tm::Frame(_) => return None,
    })
}

pub fn router(_ctx: crate::context::RouterContext) -> axum::Router {
    let state = ModuleState {
        config: DesktopRuntimeConfig::from_env(),
    };
    axum::Router::new()
        .route(PROXY_SSE_PATH, axum::routing::get(proxy_sse))
        .route(PROXY_WS_PATH, axum::routing::get(proxy_ws))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base: &str) -> DesktopRuntimeConfig {
        DesktopRuntimeConfig {
            api_base_url: base.to_string(),
            request_headers: HashMap::from([("X-Auth".to_string(), "t".to_string())]),
        }
    }

    #[test]
    fn allowlists_match_js() {
        assert!(is_allowed_sse_path("/api/event"));
        assert!(is_allowed_sse_path("/api/global/event"));
        assert!(is_allowed_sse_path("/api/ompchamber/events"));
        assert!(is_allowed_sse_path("/api/notifications/stream"));
        assert!(!is_allowed_sse_path("/api/terminal/ws"));
        assert!(is_allowed_web_socket_path("/api/event/ws"));
        assert!(is_allowed_web_socket_path("/api/global/event/ws"));
        assert!(is_allowed_web_socket_path("/api/terminal/ws"));
        assert!(!is_allowed_web_socket_path("/api/event"));
    }

    #[test]
    fn sanitize_drops_auth_and_injection() {
        let headers = HashMap::from([
            ("Authorization".to_string(), "Bearer x".to_string()),
            ("X-Good".to_string(), " v ".to_string()),
            ("Bad\nName".to_string(), "v".to_string()),
            ("Bad:Name".to_string(), "v".to_string()),
            ("Empty".to_string(), "  ".to_string()),
        ]);
        let out = sanitize_headers(&headers);
        assert_eq!(out.len(), 1);
        assert_eq!(out["X-Good"], "v");
    }

    #[test]
    fn target_resolution_is_fail_closed() {
        let cfg = config("http://127.0.0.1:3000");

        assert!(
            resolve_proxy_target(Some("http://127.0.0.1:3000/api/event"), Some(&cfg), false)
                .is_some()
        );
        assert!(
            resolve_proxy_target(
                Some("ws://127.0.0.1:3000/api/terminal/ws"),
                Some(&cfg),
                true
            )
            .is_some()
        );
        // Wrong transport, wrong path, wrong origin, missing config.
        assert!(
            resolve_proxy_target(Some("ws://127.0.0.1:3000/api/event"), Some(&cfg), false)
                .is_none()
        );
        assert!(
            resolve_proxy_target(Some("http://127.0.0.1:3000/api/other"), Some(&cfg), false)
                .is_none()
        );
        assert!(
            resolve_proxy_target(Some("http://evil.com/api/event"), Some(&cfg), false).is_none()
        );
        assert!(
            resolve_proxy_target(Some("http://127.0.0.1:3000/api/event"), None, false).is_none()
        );
        assert!(resolve_proxy_target(None, Some(&cfg), false).is_none());
        // ws/wss origins match http/https runtime base (JS protocol swap).
        assert!(
            resolve_proxy_target(Some("ws://127.0.0.1:3000/api/event/ws"), Some(&cfg), true)
                .is_some()
        );
    }

    #[test]
    fn url_builders_shape() {
        let sse = build_realtime_proxy_sse_url(
            "http://localhost:3999",
            "http://127.0.0.1:3000/api/event",
        )
        .unwrap();
        assert!(sse.starts_with("http://localhost:3999/api/ompchamber/realtime-proxy/sse?url="));
        let ws = build_realtime_proxy_ws_url(
            "http://localhost:3999",
            "ws://127.0.0.1:3000/api/event/ws",
        )
        .unwrap();
        assert!(ws.starts_with("ws://localhost:3999/api/ompchamber/realtime-proxy/ws?url="));
        let wss = build_realtime_proxy_ws_url(
            "https://localhost:3999",
            "wss://127.0.0.1:3000/api/event/ws",
        )
        .unwrap();
        assert!(wss.starts_with("wss://localhost:3999/"));
    }

    #[test]
    fn query_lookup_decodes_plus_and_percent() {
        assert_eq!(
            form_urlencoded_lookup("url=a%2Fb+x&other=1", "url").as_deref(),
            Some("a/b x")
        );
        assert_eq!(form_urlencoded_lookup("other=1", "url"), None);
    }
}
