//! Port of the relay management routes from `server/lib/relay/service.js`
//! (`registerRoutes`): `GET/POST /api/ompchamber/relay/{status,enable,disable}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

use super::service::RelayService;

#[derive(Clone)]
pub struct ModuleState {
    pub service: std::sync::Arc<RelayService>,
}

pub fn router_with(state: ModuleState) -> Router {
    Router::new()
        .route("/api/ompchamber/relay/status", get(status))
        .route(
            "/api/ompchamber/relay/enable",
            // express.json({ limit: '16kb' })
            post(enable).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/api/ompchamber/relay/disable", post(disable))
        .with_state(state)
}

async fn status(State(service): State<ModuleState>) -> Response {
    match service.service.get_status().await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn enable(State(service): State<ModuleState>, body: axum::extract::Json<Value>) -> Response {
    match service.service.enable(&body.0).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn disable(State(service): State<ModuleState>) -> Response {
    match service.service.disable().await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}
