//! Shared error/response conventions for all ported modules.
//!
//! JS precedent: route handlers answer JSON errors (`{ error: ... }` shapes
//! vary per module) and never leak panics to the wire. Modules map their
//! failures onto [`AppError`] and use [`AppResult`].

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{message}")]
    Http { status: StatusCode, message: String },
    /// The managed/external engine is not ready or a proxied engine call failed.
    #[error("engine unavailable: {0}")]
    EngineUnavailable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self::Http {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Http { status, .. } => *status,
            AppError::EngineUnavailable(_) => StatusCode::BAD_GATEWAY,
            AppError::Io(_) | AppError::Unexpected(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(serde_json::json!({ "error": self.to_string() }));
        (status, body).into_response()
    }
}
