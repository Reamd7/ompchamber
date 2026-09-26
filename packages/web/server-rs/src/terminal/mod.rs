//! Port of the `server/lib/terminal/` module's HTTP + WebSocket surface
//! (`runtime.js` route registrations and the `/api/terminal/ws` transport).
//!
//! Ownership follows `terminal/DOCUMENTATION.md`: this module owns terminal
//! identity, PTY processes, status, ordered output, bounded scrollback,
//! WebSocket attachments, and lifecycle routes. The WS path stays in the
//! ui-auth URL-token set and the relay allow-list; the gate layer below is the
//! same `ui_auth::middleware` the proxy/fs/event-stream modules apply, and the
//! upgrade handler mirrors the JS `upgradeHandler` origin check (only while a
//! UI password is configured — no origin shortcuts added or skipped).

pub mod grid;
pub mod history;
pub mod protocol;
pub mod pty;
pub mod runtime;
pub mod shell_integration;
pub mod shells;
pub mod theme;

#[cfg(test)]
mod testing;

use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, RawQuery, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;
use pty::RealPtyProvider;
use runtime::{CreateSessionRequest, JsField, SessionStatus, TerminalOptions, TerminalState};
use shells::{Platform, ShellDeps};

/// `MAX_INPUT_CHARS` (runtime.js): input cap for one `write` frame.
pub const MAX_INPUT_CHARS: usize = 65_536;
/// `express.json()` default body limit.
const JSON_BODY_LIMIT_BYTES: usize = 100 * 1024;
/// Module state: the terminal runtime plus whether UI auth is configured
/// (JS `uiAuthController?.enabled` — governs the upgrade origin check).
type ModuleState = (Arc<TerminalState>, bool);

pub fn router(ctx: RouterContext) -> axum::Router {
    // The JS auth gate applies to every `/api` route registered after
    // `registerAuthAndAccessRoutes`; layering the shared gate over this
    // module's routes mirrors that registration order.
    let auth_enabled = ctx
        .config
        .ui_password
        .as_deref()
        .map(str::trim)
        .is_some_and(|password| !password.is_empty());
    let state = TerminalState::new(
        TerminalOptions::default(),
        Arc::new(RealPtyProvider::new()),
        real_shell_deps(),
    );
    routes()
        .layer(crate::ui_auth::middleware(ctx))
        .with_state((state, auth_enabled))
}

/// Route table shared by the production router (gated) and the test harness
/// (auth disabled, fake PTY provider).
fn routes() -> Router<ModuleState> {
    Router::new()
        .route("/api/terminal/ws", get(terminal_ws))
        .route("/api/terminal/shells", get(list_shells))
        .route("/api/terminal/sessions", get(list_sessions))
        .route("/api/terminal/touch", post(touch_sessions))
        .route("/api/terminal/create", post(create_terminal))
        .route("/api/terminal/force-kill", post(force_kill))
        .route("/api/terminal/{sessionId}/resize", post(resize_terminal))
        .route(
            "/api/terminal/{sessionId}/appearance",
            post(update_appearance),
        )
        .route("/api/terminal/{sessionId}/restart", post(restart_terminal))
        .route("/api/terminal/{sessionId}", delete(close_terminal))
}

#[cfg(test)]
pub(crate) fn test_router(state: Arc<TerminalState>) -> Router {
    routes().with_state((state, false))
}
/// Real shell-discovery deps (env-runtime.js subset; the login-shell PATH
/// augmentation lands with that port — PATH passes through unchanged).
fn real_shell_deps() -> ShellDeps {
    ShellDeps {
        platform: Platform::current(),
        env: Box::new(|key: &str| std::env::var(key).ok().filter(|value| !value.is_empty())),
        build_augmented_path: Box::new(pty::real_build_augmented_path),
        search_path_for: Box::new(pty::real_search_path_for),
        is_executable: Box::new(pty::real_is_executable),
        read_etc_shells: Box::new(pty::real_read_etc_shells),
    }
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

async fn terminal_ws(
    State(state): State<ModuleState>,
    ws: WebSocketUpgrade,
    parts: Parts,
) -> Response {
    let (terminal, auth_enabled) = state;
    // JS `upgradeHandler`: only while `uiAuthController.enabled` — the 401
    // session-token check already ran in the gate layer for the upgrade
    // request; the origin check is this module's own responsibility.
    // (Non-upgrade GETs answer axum's upgrade-rejection status; the JS had no
    // express route on this path at all.)
    if auth_enabled && !crate::ui_auth::is_request_origin_allowed(&parts) {
        return crate::ui_auth::reject_websocket_upgrade(403, "Invalid origin");
    }
    ws.max_message_size(protocol::TERMINAL_WS_MAX_PAYLOAD_BYTES)
        .on_upgrade(move |socket| async move {
            runtime::run_socket(Arc::clone(&terminal), socket).await
        })
}

// ---------------------------------------------------------------------------
// HTTP command plane
// ---------------------------------------------------------------------------

/// `GET /api/terminal/shells` — available shell ids on the active server.
async fn list_shells(State(state): State<ModuleState>) -> Response {
    let (terminal, _) = state;
    let shells: Vec<Value> = terminal
        .shell_resolver
        .list()
        .into_iter()
        .map(|shell| {
            json!({ "id": shell.id, "name": shell.name, "supportsLogin": shell.supports_login })
        })
        .collect();
    Json(shells).into_response()
}

/// `GET /api/terminal/sessions?cwd=` — live sessions, optionally filtered by
/// resolved cwd so clients can adopt terminals from other devices.
async fn list_sessions(State(state): State<ModuleState>, RawQuery(raw): RawQuery) -> Response {
    let (terminal, _) = state;
    let cwd_filter = single_query_value(raw.as_deref(), "cwd")
        .filter(|cwd| !cwd.trim().is_empty())
        .map(|cwd| runtime::resolve_path(cwd.trim()));
    let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = Vec::new();
    for session in sessions.values() {
        let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(filter) = &cwd_filter
            && runtime::resolve_path(&inner.cwd) != *filter
        {
            continue;
        }
        list.push(json!({
            "sessionId": session.id,
            "cwd": inner.cwd,
            "status": inner.status.as_str(),
            "createdAt": inner.created_at,
        }));
    }
    Json(json!({ "sessions": list })).into_response()
}

/// `POST /api/terminal/touch` — refresh `lastActivity` for listed ids; a
/// `claimant` records a per-window claim that conditional deletes release.
async fn touch_sessions(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let raw_ids = body
        .0
        .get("sessionIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let claimant = body
        .0
        .get("claimant")
        .and_then(Value::as_str)
        .filter(|claimant| !claimant.trim().is_empty() && claimant.len() <= 128)
        .map(|claimant| claimant.trim().to_string());
    let now = runtime::now_ms();
    let mut touched = 0u64;
    let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
    for id in raw_ids {
        let Value::String(id) = id else { continue };
        let Some(session) = sessions.get(&id) else {
            continue;
        };
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last_activity = now;
        if let Some(claimant) = &claimant {
            inner.claims.insert(claimant.clone(), now);
        }
        touched += 1;
    }
    Json(json!({ "touched": touched })).into_response()
}

/// `POST /api/terminal/create`.
async fn create_terminal(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let request = parse_create_request(&body.0);
    match runtime::create_session(&terminal, request).await {
        Ok(session) => {
            let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            Json(json!({
                "sessionId": session.id,
                "cols": inner.cols,
                "rows": inner.rows,
                "status": inner.status.as_str(),
            }))
            .into_response()
        }
        Err(message) => {
            let status = if message == "Maximum terminal sessions reached" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// JS field presence semantics for typed extraction: parameter defaults apply
/// only to `undefined`; `null` and mismatched types read as invalid so the
/// validators reject them with the exact JS messages.
fn js_string_field(body: &Value, key: &str) -> JsField<String> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::String(value)) => JsField::Present(value.clone()),
        Some(_) => JsField::Invalid,
    }
}

fn js_number_field(body: &Value, key: &str) -> JsField<f64> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::Number(number)) => JsField::Present(number.as_f64().unwrap_or(f64::NAN)),
        Some(_) => JsField::Invalid,
    }
}

fn js_bool_field(body: &Value, key: &str) -> JsField<bool> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::Bool(value)) => JsField::Present(*value),
        Some(_) => JsField::Invalid,
    }
}

fn parse_create_request(body: &Value) -> CreateSessionRequest {
    CreateSessionRequest {
        // Non-string sessionIds fall back to a generated UUID, like the JS.
        session_id: body
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        cwd: body.get("cwd").and_then(Value::as_str).map(str::to_string),
        cols: js_number_field(body, "cols"),
        rows: js_number_field(body, "rows"),
        theme_mode: body
            .get("themeMode")
            .and_then(Value::as_str)
            .map(str::to_string),
        terminal_background: body
            .get("terminalBackground")
            .and_then(Value::as_str)
            .map(str::to_string),
        terminal_foreground: body
            .get("terminalForeground")
            .and_then(Value::as_str)
            .map(str::to_string),
        shell: js_string_field(body, "shell"),
        login_shell: js_bool_field(body, "loginShell"),
    }
}

/// `POST /api/terminal/:sessionId/resize`.
async fn resize_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let cols = js_number_field(&body.0, "cols");
    let rows = js_number_field(&body.0, "rows");
    let valid = |field: &JsField<f64>, max: u16| match field {
        JsField::Present(value) => value.fract() == 0.0 && *value >= 1.0 && *value <= max as f64,
        _ => false,
    };
    if !valid(&cols, 1000) || !valid(&rows, 500) {
        return bad_request("Invalid terminal dimensions");
    }
    let (cols, rows) = (
        match cols {
            JsField::Present(value) => value as u16,
            _ => 0,
        },
        match rows {
            JsField::Present(value) => value as u16,
            _ => 0,
        },
    );
    // Direct resize still broadcasts so other devices follow; no floor — the
    // negotiation model owns sizing policy, this route just applies.
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    if inner.status == SessionStatus::Running
        && let Some(process) = inner.process.clone()
    {
        process.resize(cols, rows);
    }
    inner.grid.resize(cols, rows);
    inner.cols = cols;
    inner.rows = rows;
    runtime::publish(
        &terminal,
        &session,
        &mut inner,
        json!({"t": "resized", "cols": cols, "rows": rows}),
    );
    runtime::schedule_grid_drain(&terminal, &session, &mut inner);
    Json(json!({ "success": true, "cols": cols, "rows": rows })).into_response()
}

/// `POST /api/terminal/:sessionId/appearance`.
async fn update_appearance(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    runtime::apply_appearance(&mut inner, &body.0);
    Json(json!({ "success": true })).into_response()
}

/// `POST /api/terminal/:sessionId/restart`.
async fn restart_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    match runtime::restart_session(&terminal, session.clone(), &body.0).await {
        Ok(()) => {
            let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            Json(json!({
                "sessionId": session.id,
                "cols": inner.cols,
                "rows": inner.rows,
                "status": inner.status.as_str(),
            }))
            .into_response()
        }
        Err(message) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// `DELETE /api/terminal/:sessionId?claimant=` — a claimant-scoped delete is a
/// tab close (release exactly that claim, kill only when no live claim
/// remains); without a claimant it is an explicit destructive kill.
async fn close_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let claimant = single_query_value(raw.as_deref(), "claimant")
        .filter(|claimant| !claimant.trim().is_empty() && claimant.len() <= 128)
        .map(|claimant| claimant.trim().to_string());
    if let Some(claimant) = &claimant {
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.claims.remove(claimant);
        let now = runtime::now_ms();
        inner
            .claims
            .retain(|_, touched_at| now.saturating_sub(*touched_at) <= runtime::IDLE_TIMEOUT_MS);
        if !inner.claims.is_empty() {
            return Json(json!({ "success": true, "released": true, "killed": false }))
                .into_response();
        }
    }
    let termination = terminal.remove_session(&session_id, "CLOSED", "Terminal closed", false);
    if let Some(termination) = termination {
        termination.await;
    }
    Json(json!({ "success": true, "released": true, "killed": true })).into_response()
}

/// `POST /api/terminal/force-kill` — terminate matching sessions immediately.
async fn force_kill(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let session_id = body
        .0
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = body
        .0
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let matching: Vec<String> = {
        let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .iter()
            .filter(|(id, session)| {
                if let Some(session_id) = &session_id {
                    return id.as_str() == session_id.as_str();
                }
                if let Some(cwd) = &cwd {
                    let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                    return inner.cwd == *cwd;
                }
                true
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut killed_session_ids = Vec::new();
    for id in matching {
        if let Some(termination) =
            terminal.remove_session(&id, "KILLED", "Terminal was killed", true)
        {
            tokio::spawn(termination);
            killed_session_ids.push(id);
        }
    }
    Json(json!({
        "success": true,
        "killedCount": killed_session_ids.len(),
        "killedSessionIds": killed_session_ids,
    }))
    .into_response()
}

fn lookup_session(
    terminal: &Arc<TerminalState>,
    session_id: &str,
) -> Option<Arc<runtime::Session>> {
    terminal
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
}

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// First value for `key` in a raw query string. Express turns duplicates into
/// arrays (which its consumers treat as non-strings), so repeated keys read as
/// absent here.
fn single_query_value(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    let mut seen = 0;
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if name == key {
            seen += 1;
            if seen > 1 {
                return None;
            }
            return Some(value.into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Body extractor (express.json semantics)
// ---------------------------------------------------------------------------

/// `express.json()` semantics: bodies are parsed only for `application/json`
/// content types; anything else reads as an absent body (handlers treat it as
/// `{}`). Malformed JSON is a 400 JSON error.
struct JsonBody(Value);

impl JsonBody {
    fn absent() -> Self {
        Self(Value::Null)
    }
}

impl<S> axum::extract::FromRequest<S> for JsonBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(
        request: axum::http::Request<axum::body::Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let bytes = match axum::body::to_bytes(request.into_body(), JSON_BODY_LIMIT_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "Payload too large" })),
                )
                    .into_response());
            }
        };
        if content_type != "application/json" || bytes.is_empty() {
            return Ok(Self::absent());
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(Self(value)),
            Err(error) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON body: {error}") })),
            )
                .into_response()),
        }
    }
}
