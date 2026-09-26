//! Port of the error surfaces `openchamber-sessions/routes.js` uses:
//! `OMPChamberControlError` / `asControlError` (from
//! `openchamber-control/error.js`) and the module's `sendServiceError`.
//!
//! JS models every failure as one `Error` instance carrying optional
//! `statusCode`, `goalConfigured`, and the partial-failure bookkeeping the
//! fork/send catch adds; [`SvcError`] keeps those fields in one type. The
//! wire mapping lives in [`send_service_error`], mirroring
//! `asControlError(error, fallback)` + `res.status(...).json({ error, ... })`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

/// `OMPChamberControlError` details block for partial dispatch failures:
/// `{ partial: true, partialAction, sessionId, directory }`.
#[derive(Debug, Clone)]
pub struct PartialDetails {
    pub action: String,
    pub session_id: String,
    /// Serialized as JSON `null` when the failure predates directory
    /// resolution (the JS catch spreads `directory: null` explicitly).
    pub directory: Option<String>,
}

/// One `Error`-shaped failure: message, optional `statusCode`, the
/// `goalConfigured` flag `markGoalPartial` sets, and the partial block only
/// the send/fork catch produces.
#[derive(Debug, Clone)]
pub struct SvcError {
    pub message: String,
    pub status: Option<u16>,
    pub goal_configured: bool,
    pub partial: Option<PartialDetails>,
}

impl SvcError {
    /// `new OMPChamberControlError(message, statusCode)`.
    pub fn control(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            goal_configured: false,
            partial: None,
        }
    }

    /// A plain `Error` without `statusCode` — `asControlError` maps these to
    /// 500.
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            goal_configured: false,
            partial: None,
        }
    }

    /// A plain `Error` carrying `error.statusCode = 400` (the "No model is
    /// configured" throw).
    pub fn with_status(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            goal_configured: false,
            partial: None,
        }
    }

    /// `markGoalPartial`: tag a dispatch failure that happened after goal
    /// metadata was configured.
    pub fn mark_goal_partial(mut self, enabled: bool) -> Self {
        if enabled {
            self.goal_configured = true;
        }
        self
    }
}

/// `sendServiceError(res, error, fallback)`: `asControlError` keeps the
/// message (falling back only when empty), resolves the status (`statusCode`
/// or 500), and emits the partial block when present.
pub fn send_service_error(error: &SvcError, fallback: &str) -> Response {
    let message = if error.message.is_empty() {
        fallback.to_string()
    } else {
        error.message.clone()
    };
    let status = StatusCode::from_u16(error.status.unwrap_or(500))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = Map::new();
    body.insert("error".to_string(), Value::String(message));
    if let Some(partial) = &error.partial {
        body.insert("partial".to_string(), Value::Bool(true));
        body.insert(
            "partialAction".to_string(),
            Value::String(partial.action.clone()),
        );
        body.insert(
            "sessionId".to_string(),
            Value::String(partial.session_id.clone()),
        );
        body.insert(
            "directory".to_string(),
            partial
                .directory
                .as_deref()
                .map(|directory| Value::String(directory.to_string()))
                .unwrap_or(Value::Null),
        );
    }
    (status, Json(Value::Object(body))).into_response()
}
