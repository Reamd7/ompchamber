//! The walkthrough module's error shape.
//!
//! Mirrors the JS `fail(message, statusCode, extra)` errors and
//! `respondWithError` in `routes.js`: every failure answers
//! `{ error, code?, model?, requiredChars?, availableChars? }` with the
//! carried status (500 when nothing better is known), and 5xx failures are
//! logged server-side.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct WalkthroughError {
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
    pub model: Option<Value>,
    pub required_chars: Option<i64>,
    pub available_chars: Option<i64>,
}

impl WalkthroughError {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            model: None,
            required_chars: None,
            available_chars: None,
        }
    }

    /// `fail(message, statusCode, { code })`.
    pub fn with_code(message: impl Into<String>, status: u16, code: &str) -> Self {
        Self {
            code: Some(code.to_string()),
            ..Self::new(message, status)
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(message, 500)
    }

    pub fn code(mut self, code: &str) -> Self {
        self.code = Some(code.to_string());
        self
    }

    pub fn model(mut self, model: Value) -> Self {
        self.model = Some(model);
        self
    }

    pub fn required_chars(mut self, required_chars: i64) -> Self {
        self.required_chars = Some(required_chars);
        self
    }

    pub fn available_chars(mut self, available_chars: i64) -> Self {
        self.available_chars = Some(available_chars);
        self
    }
}

impl std::fmt::Display for WalkthroughError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WalkthroughError {}

impl IntoResponse for WalkthroughError {
    fn into_response(self) -> Response {
        if self.status >= 500 {
            tracing::error!("walkthrough error: {}", self.message);
        }
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = json!({ "error": self.message });
        if let Some(code) = &self.code {
            body["code"] = json!(code);
        }
        if let Some(model) = &self.model {
            body["model"] = model.clone();
        }
        if let Some(required) = self.required_chars {
            body["requiredChars"] = json!(required);
        }
        if let Some(available) = self.available_chars {
            body["availableChars"] = json!(available);
        }
        (status, Json(body)).into_response()
    }
}
