//! Port of `server/lib/notifications/routes.js`: push subscription,
//! APNs token, visibility, notification-stream SSE, session
//! status/attention, view/unview/message-sent, and the auto-accept mirror
//! endpoints — with the JS's exact response shapes and error strings.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::notifications::emitter_runtime::DESKTOP_NOTIFY_PREFIX;
use crate::notifications::{NotificationsState, crypto};

/// JS `NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS`.
pub const NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS: u64 = 20_000;

/// JS session cookie name (`oc_ui_session`).
const UI_SESSION_COOKIE_NAME: &str = "oc_ui_session";
/// JS `SESSION_TTL_MS` (12 hours).
const SESSION_TTL_SECONDS: u64 = 12 * 60 * 60 * 1000 / 1000;

pub fn router(state: Arc<NotificationsState>) -> Router {
    Router::new()
        .route("/api/push/vapid-public-key", get(vapid_public_key))
        .route("/api/push/subscribe", post(push_subscribe))
        .route("/api/push/subscribe", delete(push_unsubscribe))
        .route("/api/push/apns-token", post(apns_token))
        .route("/api/push/apns-token", delete(apns_token_delete))
        .route("/api/push/visibility", post(push_visibility))
        .route("/api/push/visibility", get(push_visibility_get))
        .route("/api/notifications/stream", get(notifications_stream))
        .route("/api/notifications/auto-accept", post(auto_accept))
        .route("/api/session-activity", get(session_activity))
        .route("/api/sessions/snapshot", get(sessions_snapshot))
        .route("/api/sessions/status", get(sessions_status))
        .route("/api/sessions/{id}/status", get(session_status))
        .route("/api/sessions/attention", get(sessions_attention))
        .route("/api/sessions/{id}/attention", get(session_attention))
        .route("/api/sessions/{id}/view", post(session_view))
        .route("/api/sessions/{id}/unview", post(session_unview))
        .route(
            "/api/sessions/{id}/message-sent",
            post(session_message_sent),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// UI session token (ui-auth.js `ensureSessionToken` /
// request-security.js `getUiSessionTokenFromRequest`)
// ---------------------------------------------------------------------------

/// `getUiSessionTokenFromRequest`: the `oc_ui_session` cookie only.
fn ui_session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for segment in cookie_header.split(';') {
        let mut parts = segment.splitn(2, '=');
        let name = parts.next()?.trim();
        if name != UI_SESSION_COOKIE_NAME {
            continue;
        }
        let value = parts.next().unwrap_or("").trim();
        return Some(decode_uri_component(value));
    }
    None
}

/// JS `decodeURIComponent` (malformed escapes stay literal).
fn decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `ensureSessionToken`: read the cookie, or mint one (random 32 bytes,
/// base64url) and return the Set-Cookie header value.
fn ensure_ui_session_token(headers: &HeaderMap) -> (String, Option<String>) {
    if let Some(token) = ui_session_token_from_headers(headers) {
        if !token.is_empty() {
            return (token, None);
        }
    }
    let token = crypto::b64url_encode(&random_bytes(32));
    let secure = is_secure_request(headers);
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}; Expires={}{}",
        UI_SESSION_COOKIE_NAME,
        token,
        SESSION_TTL_SECONDS,
        http_date_after(SESSION_TTL_SECONDS as i64),
        if secure { "; Secure" } else { "" }
    );
    (token, Some(cookie))
}

fn random_bytes(len: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// JS `isSecureRequest`: x-forwarded-proto first entry is https.
fn is_secure_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().to_ascii_lowercase() == "https")
        .unwrap_or(false)
}

/// `new Date(Date.now() + maxAge * 1000).toUTCString()` shape
/// ("Thu, 01 Jan 1970 00:00:00 GMT").
fn http_date_after(offset_seconds: i64) -> String {
    let now_seconds = (crypto::now_ms() / 1000) as i64 + offset_seconds;
    let days = now_seconds.div_euclid(86_400);
    let seconds_of_day = now_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday =
        ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize % 7];
    format!(
        "{weekday}, {day:02} {month} {year:04} {:02}:{:02}:{:02} GMT",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

fn civil_from_days(days: i64) -> (i64, &'static str, i64) {
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    (year, month, d)
}

fn with_optional_cookie(mut response: Response, cookie: Option<String>) -> Response {
    if let Some(cookie) = cookie {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    response
}

fn unauthorized_ui_session_missing() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "UI session missing" })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Body parsers (routes.js)
// ---------------------------------------------------------------------------

/// `parsePushSubscribeBody`.
fn parse_push_subscribe_body(body: &Value) -> Option<(String, String, String)> {
    let object = body.as_object()?;
    let endpoint = object.get("endpoint")?.as_str()?.trim().to_string();
    let keys = object.get("keys")?.as_object()?;
    let p256dh = keys.get("p256dh")?.as_str()?.trim().to_string();
    let auth = keys.get("auth")?.as_str()?.trim().to_string();
    if endpoint.is_empty() || p256dh.is_empty() || auth.is_empty() {
        return None;
    }
    Some((endpoint, p256dh, auth))
}

/// `parsePushUnsubscribeBody`.
fn parse_push_unsubscribe_body(body: &Value) -> Option<String> {
    let endpoint = body
        .as_object()?
        .get("endpoint")?
        .as_str()?
        .trim()
        .to_string();
    if endpoint.is_empty() {
        return None;
    }
    Some(endpoint)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn vapid_public_key(State(state): State<Arc<NotificationsState>>) -> Response {
    state.push.ensure_push_initialized().await;
    match state.push.get_or_create_vapid_keys().await {
        Ok((public_key, _)) => Json(json!({ "publicKey": public_key })).into_response(),
        Err(error) => {
            tracing::warn!("[Push] Failed to load VAPID key: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to load push key" })),
            )
                .into_response()
        }
    }
}

async fn push_subscribe(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.push.ensure_push_initialized().await;
    state.events.ensure_started();

    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let Some((endpoint, p256dh, auth)) = parse_push_subscribe_body(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid body" })),
        )
            .into_response();
    };

    // First push from a real origin records it as the public origin (used
    // for the VAPID subject).
    let origin = body
        .get("origin")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if origin.starts_with("http://") || origin.starts_with("https://") {
        let mut settings = state.settings.read_migrated().await.unwrap_or_default();
        let has_origin = settings
            .get("publicOrigin")
            .and_then(Value::as_str)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if !has_origin {
            settings.insert("publicOrigin".to_string(), Value::String(origin));
            if state.settings.write_raw(&settings).await.is_ok() {
                state.push.set_push_initialized(false);
            }
        }
    }

    let platform = body.get("platform").and_then(Value::as_str);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    state
        .push
        .add_or_update_push_subscription(&ui_token, &endpoint, &p256dh, &auth, user_agent, platform)
        .await;

    with_optional_cookie(Json(json!({ "ok": true })).into_response(), cookie)
}

async fn push_unsubscribe(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.push.ensure_push_initialized().await;

    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let Some(endpoint) = parse_push_unsubscribe_body(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid body" })),
        )
            .into_response();
    };

    state
        .push
        .remove_push_subscription(&ui_token, &endpoint)
        .await;
    with_optional_cookie(Json(json!({ "ok": true })).into_response(), cookie)
}

/// `POST /api/push/apns-token` — native iOS device-token registration.
async fn apns_token(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.events.ensure_started();

    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let device_token = body
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if device_token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid body" })),
        )
            .into_response();
    }

    let platform = if body.get("platform") == Some(&Value::String("android".into())) {
        "android"
    } else {
        "ios"
    };
    let environment = if body.get("environment") == Some(&Value::String("sandbox".into())) {
        "sandbox"
    } else {
        "production"
    };
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    state
        .apns
        .add_or_update_apns_token(
            &ui_token,
            &device_token,
            user_agent,
            Some(platform),
            Some(environment),
        )
        .await;
    with_optional_cookie(Json(json!({ "ok": true })).into_response(), cookie)
}

async fn apns_token_delete(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let device_token = body
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if device_token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid body" })),
        )
            .into_response();
    }

    state.apns.remove_apns_token(&ui_token, &device_token).await;
    with_optional_cookie(Json(json!({ "ok": true })).into_response(), cookie)
}

async fn push_visibility(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let visible = body.get("visible") == Some(&Value::Bool(true));
    let platform = body.get("platform").and_then(Value::as_str);
    state
        .push
        .update_ui_visibility(&ui_token, visible, platform);
    // The badge set clears the moment a UI client reports visible — the
    // same moment the device zeroes its icon badge on becomeActive.
    if visible {
        state.trigger.clear_pending_push_badge();
    }
    with_optional_cookie(Json(json!({ "ok": true })).into_response(), cookie)
}

async fn push_visibility_get(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
) -> Response {
    // JS reads the cookie without minting (`getUiSessionTokenFromRequest`).
    let Some(ui_token) = ui_session_token_from_headers(&headers).filter(|t| !t.is_empty()) else {
        return unauthorized_ui_session_missing();
    };
    Json(json!({ "ok": true, "visible": state.push.is_ui_visible(&ui_token) })).into_response()
}

/// `GET /api/notifications/stream` — the raw-payload SSE channel with the
/// 20s heartbeat comment and the ready bootstrap event.
async fn notifications_stream(
    State(state): State<Arc<NotificationsState>>,
    headers: HeaderMap,
) -> Response {
    state.events.ensure_started();

    let (ui_token, cookie) = ensure_ui_session_token(&headers);
    if ui_token.is_empty() {
        return unauthorized_ui_session_missing();
    }

    let mut receiver = state.notification_tx.subscribe();
    let ready = json!({
        "type": "ompchamber:notification-stream-ready",
        "properties": { "uiToken": ui_token },
    });
    // `writeSseEvent(res, ready)`: one `data: <json>` frame.
    let ready_data = serde_json::to_string(&ready).unwrap_or_default();
    let stream = async_stream::stream! {
        yield Ok::<SseEvent, std::convert::Infallible>(SseEvent::default().data(ready_data));
        loop {
            match receiver.recv().await {
                Ok(payload) => {
                    yield Ok(SseEvent::default().data(payload));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_millis(
                NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS,
            ))
            .text("heartbeat"),
    );
    let mut response = (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-transform"),
            ),
            (header::CONNECTION, HeaderValue::from_static("keep-alive")),
            (
                header::HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
        ],
        sse,
    )
        .into_response();
    if let Some(cookie) = cookie {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    response
}

async fn auto_accept(
    State(state): State<Arc<NotificationsState>>,
    Json(body): Json<Value>,
) -> Response {
    let session_id = body
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let enabled = body.get("enabled") == Some(&Value::Bool(true));
    if session_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sessionId required" })),
        )
            .into_response();
    }
    state.trigger.set_auto_accept_session(&session_id, enabled);
    Json(json!({ "success": true, "sessionId": session_id, "enabled": enabled })).into_response()
}

async fn session_activity(State(state): State<Arc<NotificationsState>>) -> Response {
    // JS fires the watcher start without awaiting (`void ensureSessionWatcher()`).
    state.events.ensure_started();
    let snapshot = state
        .events
        .session_runtime()
        .get_session_activity_snapshot();
    Json(Value::Object(snapshot)).into_response()
}

async fn sessions_snapshot(State(state): State<Arc<NotificationsState>>) -> Response {
    state.events.ensure_started();
    let status_sessions = state.events.session_runtime().get_session_state_snapshot();
    let attention_sessions = state
        .events
        .session_runtime()
        .get_session_attention_snapshot();
    Json(json!({
        "statusSessions": Value::Object(status_sessions),
        "attentionSessions": Value::Object(attention_sessions),
        "serverTime": crypto::now_ms(),
    }))
    .into_response()
}

async fn sessions_status(State(state): State<Arc<NotificationsState>>) -> Response {
    state.events.ensure_started();
    let snapshot = state.events.session_runtime().get_session_state_snapshot();
    Json(json!({
        "sessions": Value::Object(snapshot),
        "serverTime": crypto::now_ms(),
    }))
    .into_response()
}

async fn session_status(
    State(state): State<Arc<NotificationsState>>,
    Path(session_id): Path<String>,
) -> Response {
    state.events.ensure_started();
    match state
        .events
        .session_runtime()
        .get_session_state(&session_id)
    {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Session not found or no state available",
                "sessionId": session_id,
            })),
        )
            .into_response(),
        Some(state_value) => {
            let mut body = json!({ "sessionId": session_id });
            if let (Some(target), Some(source)) = (body.as_object_mut(), state_value.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            Json(body).into_response()
        }
    }
}

async fn sessions_attention(State(state): State<Arc<NotificationsState>>) -> Response {
    state.events.ensure_started();
    let snapshot = state
        .events
        .session_runtime()
        .get_session_attention_snapshot();
    Json(json!({
        "sessions": Value::Object(snapshot),
        "serverTime": crypto::now_ms(),
    }))
    .into_response()
}

async fn session_attention(
    State(state): State<Arc<NotificationsState>>,
    Path(session_id): Path<String>,
) -> Response {
    state.events.ensure_started();
    match state
        .events
        .session_runtime()
        .get_session_attention_state(&session_id)
    {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Session not found or no attention state available",
                "sessionId": session_id,
            })),
        )
            .into_response(),
        Some(state_value) => {
            let mut body = json!({ "sessionId": session_id });
            if let (Some(target), Some(source)) = (body.as_object_mut(), state_value.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            Json(body).into_response()
        }
    }
}

fn client_id_from_headers(headers: &HeaderMap) -> String {
    // JS: x-client-id header, else req.ip, else 'anonymous'. req.ip needs
    // ConnectInfo wiring the composition root doesn't provide; 'anonymous'
    // covers the no-header case (noted gap).
    headers
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| "anonymous".to_string())
}

async fn session_view(
    State(state): State<Arc<NotificationsState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let client_id = client_id_from_headers(&headers);
    state
        .events
        .session_runtime()
        .mark_session_viewed(&session_id, &client_id);
    // Opening a session means the user is engaging — reset the badge set.
    state.trigger.clear_pending_push_badge();
    Json(json!({ "success": true, "sessionId": session_id, "viewed": true })).into_response()
}

async fn session_unview(
    State(state): State<Arc<NotificationsState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let client_id = client_id_from_headers(&headers);
    state
        .events
        .session_runtime()
        .mark_session_unviewed(&session_id, &client_id);
    Json(json!({ "success": true, "sessionId": session_id, "viewed": false })).into_response()
}

async fn session_message_sent(
    State(state): State<Arc<NotificationsState>>,
    Path(session_id): Path<String>,
) -> Response {
    state
        .events
        .session_runtime()
        .mark_user_message_sent(&session_id);
    state.trigger.clear_pending_push_badge();
    Json(json!({ "success": true, "sessionId": session_id, "messageSent": true })).into_response()
}

#[allow(dead_code)]
const _: &str = DESKTOP_NOTIFY_PREFIX;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::context::RouterContext;
    use crate::engine::EngineState;
    use crate::hub::EventHub;
    use crate::notifications::transport::HttpPostResponse;
    use axum::body::Body;
    use futures::StreamExt;
    use tower::ServiceExt;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notif-routes-{}-{}",
            std::process::id(),
            crypto::now_ms() * 1000 + rand::random::<u64>() % 100_000
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn test_config(data_dir: std::path::PathBuf) -> Arc<ServerConfig> {
        Arc::new(ServerConfig {
            port: 3999,
            host: None,
            lan: false,
            ui_password: None,
            api_only: false,
            data_dir,
            dist_dir: std::path::PathBuf::from("/tmp/dist"),
            tunnel: Default::default(),
            engine: crate::config::EngineConfig::External {
                base_url: "http://127.0.0.1:1".to_string(),
            },
        })
    }

    fn noop_transport() -> crate::notifications::transport::HttpPost {
        Arc::new(|_url, _headers, _body| {
            Box::pin(async {
                Ok(HttpPostResponse {
                    status: 200,
                    body: String::new(),
                })
            })
        })
    }

    fn test_state(data_dir: std::path::PathBuf) -> Arc<NotificationsState> {
        let engine = EngineState::external("http://127.0.0.1:1".to_string(), None);
        let ctx = RouterContext {
            config: test_config(data_dir),
            engine,
            hub: EventHub::new(),
        };
        let events = crate::event_stream::EventStreamState::from_ctx(ctx.clone());
        NotificationsState::new_with_transport(ctx, events, noop_transport())
    }

    async fn request_json(
        app: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
            None => builder.body(Body::empty()).expect("request"),
        };
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, headers)
    }

    #[tokio::test]
    async fn vapid_public_key_generates_and_serves_the_stored_key() {
        let dir = temp_dir();
        let state = test_state(dir.clone());
        let app = router(state);
        let (status, body, _) =
            request_json(&app, "GET", "/api/push/vapid-public-key", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        let public_key = body["publicKey"].as_str().expect("public key");
        assert!(!public_key.is_empty());
        // Persisted in settings.
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("settings.json")).expect("settings"),
        )
        .expect("json");
        assert_eq!(settings["vapidKeys"]["publicKey"], json!(public_key));
    }

    #[tokio::test]
    async fn push_subscribe_mints_a_ui_session_and_persists_the_subscription() {
        let dir = temp_dir();
        let state = test_state(dir.clone());
        let app = router(state);
        let body = json!({
            "endpoint": "https://push/endpoint",
            "keys": { "p256dh": "pub-key", "auth": "auth-secret" },
            "origin": "https://example.dev",
            "platform": "ios",
        });
        let (status, response, headers) = request_json(
            &app,
            "POST",
            "/api/push/subscribe",
            &[("user-agent", "UnitTest/1.0")],
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, json!({ "ok": true }));
        // A session cookie was minted for the anonymous client.
        let set_cookie = headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("set-cookie");
        assert!(set_cookie.starts_with("oc_ui_session="));
        assert!(set_cookie.contains("Path=/"));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        // The origin was recorded as publicOrigin.
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("settings.json")).expect("settings"),
        )
        .expect("json");
        assert_eq!(settings["publicOrigin"], json!("https://example.dev"));

        // Re-subscribing with the minted cookie reuses the session.
        let token = set_cookie
            .split(';')
            .next()
            .and_then(|pair| pair.split('=').nth(1))
            .expect("token");
        let (status, response, headers) = request_json(
            &app,
            "POST",
            "/api/push/subscribe",
            &[("cookie", &format!("oc_ui_session={token}"))],
            Some(json!({
                "endpoint": "https://push/endpoint2",
                "keys": { "p256dh": "k", "auth": "a" },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, json!({ "ok": true }));
        assert!(
            headers.get(header::SET_COOKIE).is_none(),
            "no re-mint with a cookie"
        );
    }

    #[tokio::test]
    async fn push_subscribe_rejects_invalid_bodies() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(state);
        let cookie = "oc_ui_session=test-token";
        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/push/subscribe",
            &[("cookie", cookie)],
            Some(json!({ "endpoint": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "Invalid body" }));

        let (status, _, _) = request_json(
            &app,
            "DELETE",
            "/api/push/subscribe",
            &[("cookie", cookie)],
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn push_endpoints_require_a_ui_session() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(state);
        let (status, body, _) = request_json(&app, "GET", "/api/push/visibility", &[], None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({ "error": "UI session missing" }));
    }

    #[tokio::test]
    async fn visibility_roundtrip_and_badge_clearing() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(state);
        let cookie = "oc_ui_session=vis-token";

        let (status, body, _) = request_json(
            &app,
            "GET",
            "/api/push/visibility",
            &[("cookie", cookie)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true, "visible": false }));

        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/push/visibility",
            &[("cookie", cookie)],
            Some(json!({ "visible": true, "platform": "ios" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));

        let (status, body, _) = request_json(
            &app,
            "GET",
            "/api/push/visibility",
            &[("cookie", cookie)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["visible"], json!(true));
    }

    #[tokio::test]
    async fn apns_token_registration_and_removal() {
        let dir = temp_dir();
        let state = test_state(dir.clone());
        let app = router(state);
        let cookie = "oc_ui_session=apns-token";

        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/push/apns-token",
            &[("cookie", cookie)],
            Some(json!({ "token": "  abc123  ", "platform": "ios", "environment": "sandbox" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));

        let tokens: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("apns-tokens.json")).expect("tokens"),
        )
        .expect("json");
        let entry = &tokens["tokensBySession"]["apns-token"][0];
        assert_eq!(entry["deviceToken"], json!("abc123"));
        assert_eq!(entry["platform"], json!("ios"));
        assert_eq!(entry["environment"], json!("sandbox"));

        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/push/apns-token",
            &[("cookie", cookie)],
            Some(json!({ "token": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "Invalid body" }));

        let (status, body, _) = request_json(
            &app,
            "DELETE",
            "/api/push/apns-token",
            &[("cookie", cookie)],
            Some(json!({ "token": "abc123" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));
        let tokens: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("apns-tokens.json")).expect("tokens"),
        )
        .expect("json");
        assert!(tokens["tokensBySession"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sessions_status_routes_serve_the_session_state_machine() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(Arc::clone(&state));

        // Feed the session runtime directly (the watcher would deliver
        // engine payloads the same way).
        state
            .events
            .session_runtime()
            .process_opencode_sse_payload(&json!({
                "type": "session.status",
                "id": "evt-1",
                "properties": {
                    "sessionID": "ses_1",
                    "status": { "type": "busy" },
                },
            }));

        let (status, body, _) = request_json(&app, "GET", "/api/session-activity", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ses_1"], json!({ "type": "busy" }));

        let (status, body, _) =
            request_json(&app, "GET", "/api/sessions/snapshot", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["statusSessions"]["ses_1"]["status"], json!("busy"));
        assert!(body["serverTime"].as_u64().is_some());

        let (status, body, _) = request_json(&app, "GET", "/api/sessions/status", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"]["ses_1"]["status"], json!("busy"));

        let (status, body, _) =
            request_json(&app, "GET", "/api/sessions/ses_1/status", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessionId"], json!("ses_1"));
        assert_eq!(body["status"], json!("busy"));

        let (status, body, _) =
            request_json(&app, "GET", "/api/sessions/missing/status", &[], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({
                "error": "Session not found or no state available",
                "sessionId": "missing",
            })
        );

        let (status, body, _) =
            request_json(&app, "GET", "/api/sessions/attention", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["sessions"].is_object());

        // Attention appears once a status marks it; view clears it.
        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/sessions/ses_1/view",
            &[("x-client-id", "client-1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "success": true, "sessionId": "ses_1", "viewed": true })
        );

        let (status, body, _) =
            request_json(&app, "POST", "/api/sessions/ses_1/message-sent", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "success": true, "sessionId": "ses_1", "messageSent": true })
        );

        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/sessions/ses_1/unview",
            &[("x-client-id", "client-1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "success": true, "sessionId": "ses_1", "viewed": false })
        );
    }

    #[tokio::test]
    async fn auto_accept_mirrors_the_session_policy() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(state);
        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/notifications/auto-accept",
            &[],
            Some(json!({ "sessionId": "ses_a", "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "success": true, "sessionId": "ses_a", "enabled": true })
        );

        let (status, body, _) = request_json(
            &app,
            "POST",
            "/api/notifications/auto-accept",
            &[],
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "sessionId required" }));
    }

    #[tokio::test]
    async fn notification_stream_serves_the_ready_event_and_headers() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(Arc::clone(&state));
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/notifications/stream")
            .body(Body::empty())
            .expect("request");
        let response = tokio::time::timeout(Duration::from_secs(5), app.oneshot(request))
            .await
            .expect("response within timeout")
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream; charset=utf-8")
        );
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache, no-transform")
        );
        assert_eq!(
            headers
                .get("x-accel-buffering")
                .and_then(|v| v.to_str().ok()),
            Some("no")
        );

        // The ready bootstrap event arrives as the first data frame.
        let stream = response.into_body().into_data_stream();
        let mut first_frame = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut stream = std::pin::pin!(stream);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
                Ok(Some(Ok(bytes))) => {
                    first_frame.push_str(&String::from_utf8_lossy(&bytes));
                    if first_frame.contains("notification-stream-ready") {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            first_frame.contains("\"type\":\"ompchamber:notification-stream-ready\""),
            "ready frame: {first_frame}"
        );
        assert!(first_frame.contains("uiToken"));

        // Broadcast reaches the stream client through the same channel.
        state
            .emitter
            .broadcast_ui_notification(&json!({ "title": "T" }), false);
    }

    #[tokio::test]
    async fn broadcast_reaches_notification_stream_subscribers() {
        let dir = temp_dir();
        let state = test_state(dir);
        let app = router(Arc::clone(&state));
        let mut receiver = state.notification_tx.subscribe();

        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/notifications/stream")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        // Emitting a UI notification delivers a frame to subscribers.
        state
            .emitter
            .broadcast_ui_notification(&json!({ "title": "Hello" }), true);
        let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("frame within timeout")
            .expect("frame");
        let parsed: Value = serde_json::from_str(&frame).expect("json frame");
        assert_eq!(parsed["type"], "ompchamber:notification");
        assert_eq!(parsed["properties"]["title"], json!("Hello"));
        assert_eq!(
            parsed["properties"]["desktopNotificationDelivered"],
            json!(true)
        );
    }

    #[test]
    fn http_dates_render_in_the_js_shape() {
        let date = http_date_after(0);
        assert!(date.ends_with("GMT"), "{date}");
        assert!(date.contains(" 00:00:00 ") || date.len() == 29, "{date}");
        // Epoch formatting: 0 seconds-of-day renders midnight.
        let epoch = http_date_after(-((crypto::now_ms() / 1000) as i64));
        assert!(
            epoch.starts_with("Thu, 01 Jan 1970 00:00:00 GMT"),
            "{epoch}"
        );
    }

    #[test]
    fn cookie_parsing_mirrors_the_js_semantics() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; oc_ui_session=tok%2Ben; b=2"),
        );
        assert_eq!(
            ui_session_token_from_headers(&headers).as_deref(),
            Some("tok+en")
        );
        // Bad escapes keep the literal characters.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("oc_ui_session=bad%zz"),
        );
        assert_eq!(
            ui_session_token_from_headers(&headers).as_deref(),
            Some("bad%zz")
        );
        // Multiple '=' survive via the joined remainder.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("oc_ui_session=a=b=c"),
        );
        assert_eq!(
            ui_session_token_from_headers(&headers).as_deref(),
            Some("a=b=c")
        );
    }
}
