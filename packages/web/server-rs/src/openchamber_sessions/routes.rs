//! Port of `registerOMPChamberSessionRoutes` — the three Express routes with
//! their per-route `express.json({ limit: '1mb' })` body handling:
//!
//! - `POST /api/ompchamber/sessions`
//! - `POST /api/ompchamber/sessions/:sessionId/send`
//! - `POST /api/ompchamber/sessions/:sessionId/fork`
//!
//! Errors flow through `sendServiceError` (`asControlError` semantics); the
//! `console.error` calls become `tracing::error!` with the message only.

use std::sync::Arc;

use super::error::send_service_error;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

use super::service::SessionService;

/// Module-local state (`Router::with_state` before returning).
#[derive(Clone)]
pub struct SessionState {
    pub service: Arc<SessionService>,
}

pub fn routes() -> Router<SessionState> {
    Router::new()
        .route("/api/ompchamber/sessions", post(create_route))
        .route(
            "/api/ompchamber/sessions/{sessionId}/send",
            post(send_route),
        )
        .route(
            "/api/ompchamber/sessions/{sessionId}/fork",
            post(fork_route),
        )
}

/// Express `req.body && typeof req.body === 'object' ? req.body : {}`:
/// JSON content-type parses any JSON document (objects pass through; `null`
/// and non-documents become `{}`), anything else leaves the body unset, and
/// malformed JSON under a JSON content-type is a 400 like express.json.
fn parse_json_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, Response> {
    let is_json = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    if !is_json {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Null) => Ok(Value::Object(serde_json::Map::new())),
        Ok(value) => Ok(value),
        Err(error) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("Failed to parse JSON body: {error}") })),
        )
            .into_response()),
    }
}

async fn create_route(
    State(state): State<SessionState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let payload = match parse_json_body(&headers, &body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    match state.service.create(&payload).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => {
            tracing::error!(
                "[OMPChamberSessions] failed to create session: {} ({})",
                error.message,
                error.status.unwrap_or(500)
            );
            send_service_error(&error, "Failed to create session")
        }
    }
}

async fn send_route(
    State(state): State<SessionState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let payload = match parse_json_body(&headers, &body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    match state.service.send(&session_id, &payload).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => {
            tracing::error!(
                "[OMPChamberSessions] failed to send session: {} ({})",
                error.message,
                error.status.unwrap_or(500)
            );
            send_service_error(&error, "Failed to send session")
        }
    }
}

async fn fork_route(
    State(state): State<SessionState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let payload = match parse_json_body(&headers, &body) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    match state.service.fork(&session_id, &payload).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => {
            tracing::error!(
                "[OMPChamberSessions] failed to fork session: {} ({})",
                error.message,
                error.status.unwrap_or(500)
            );
            send_service_error(&error, "Failed to fork session")
        }
    }
}
