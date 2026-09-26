//! Port of `server/lib/quota/routes.js` (`registerQuotaRoutes`) on axum.
//!
//! Route map (verbs, status codes, and JSON shapes preserved):
//! - `GET  /api/quota/providers` → `{ providers: [...] }`
//! - `GET  /api/quota/credentials/:providerId` → status or 404
//!   `{ code: 'UNSUPPORTED_PROVIDER', error: 'Unsupported credential provider' }`
//! - `PUT  /api/quota/credentials/:providerId` (16kb body limit) → validated
//!   write; invalid credentials 400 `{ code: 'INVALID_CREDENTIAL', error }`
//! - `POST /api/quota/credentials/:providerId/validate` → `{ valid: true }`
//!   or 404 `{ code: 'NOT_CONFIGURED', error: 'Not configured' }`
//! - `POST /api/quota/credentials/:providerId/import` → cursor only, else 404
//!   `{ code: 'IMPORT_UNAVAILABLE', error: 'Import unavailable' }`
//! - `DELETE /api/quota/credentials/:providerId` → `{ configured: false }`
//! - `GET  /api/quota/:providerId` → provider result envelope (200 even for
//!   unsupported providers).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::quota::credentials::{
    delete_managed_credential, managed_credential_status, normalizer_for, read_managed_credential,
    write_managed_credential,
};
use crate::quota::deps::QuotaDeps;
use crate::quota::providers::cursor;
use crate::quota::providers::ollama_cloud;
use crate::quota::runtime::QuotaRuntime;

/// `express.json({ limit: '16kb' })` on the PUT route.
const CREDENTIAL_BODY_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
pub struct QuotaState {
    pub runtime: Arc<QuotaRuntime>,
}

pub fn router(ctx: RouterContext) -> Router {
    let _ = ctx;
    router_with(QuotaRuntime::shared(QuotaDeps::real()))
}

pub fn router_with(runtime: Arc<QuotaRuntime>) -> Router {
    let credentials = Router::new()
        .route(
            "/api/quota/credentials/{providerId}",
            get(credential_status)
                .put(write_credential)
                .delete(remove_credential),
        )
        .route_layer(DefaultBodyLimit::max(CREDENTIAL_BODY_LIMIT));

    Router::new()
        .route("/api/quota/providers", get(list_providers))
        .merge(credentials)
        .route(
            "/api/quota/credentials/{providerId}/validate",
            post(validate_credential).fallback(method_fallthrough_404),
        )
        .route(
            "/api/quota/credentials/{providerId}/import",
            post(import_credential).fallback(method_fallthrough_404),
        )
        .route("/api/quota/{providerId}", get(provider_quota))
        .with_state(QuotaState { runtime })
}

fn unsupported_provider() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "code": "UNSUPPORTED_PROVIDER", "error": "Unsupported credential provider" })),
    )
        .into_response()
}

fn credential_error(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "INVALID_CREDENTIAL", "error": message })),
    )
        .into_response()
}

/// Express registers the credential POST routes without method fallthrough:
/// a GET probe misses every handler and the app answers 404. axum would
/// answer 405; this fallback restores the JS status.
async fn method_fallthrough_404() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn list_providers(State(state): State<QuotaState>) -> Response {
    Json(json!({ "providers": state.runtime.list_configured() })).into_response()
}

async fn credential_status(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    Json(managed_credential_status(&state.runtime.deps, &provider_id)).into_response()
}

/// `validators[providerId]` — Ollama Cloud usage fetch, Cursor validation.
async fn run_validator(
    deps: &QuotaDeps,
    provider_id: &str,
    credential: &Value,
) -> Result<(), String> {
    match provider_id {
        "ollama-cloud" => ollama_cloud::fetch_ollama_cloud_usage(deps, credential)
            .await
            .map(|_| ()),
        "cursor" => cursor::validate_cursor_credential(deps, credential).await,
        _ => Ok(()),
    }
}

async fn write_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(normalize) = normalizer_for(&provider_id) else {
        return unsupported_provider();
    };
    // express.json parse failures answer as body-parser errors (400).
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };

    let Some(credential) = normalize(&body) else {
        return credential_error("Invalid credential");
    };

    if let Err(error) = run_validator(&state.runtime.deps, &provider_id, &credential).await {
        return credential_error(&error);
    }
    match write_managed_credential(&state.runtime.deps, &provider_id, &credential) {
        Ok(status) => Json(status).into_response(),
        Err(error) => credential_error(&error),
    }
}

async fn validate_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    let Some(credential) = read_managed_credential(&state.runtime.deps, &provider_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NOT_CONFIGURED", "error": "Not configured" })),
        )
            .into_response();
    };
    match run_validator(&state.runtime.deps, &provider_id, &credential).await {
        Ok(()) => Json(json!({ "valid": true })).into_response(),
        Err(error) => credential_error(&error),
    }
}

async fn import_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    if provider_id != "cursor" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "IMPORT_UNAVAILABLE", "error": "Import unavailable" })),
        )
            .into_response();
    }
    match cursor::import_cursor_credential(&state.runtime.deps).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => credential_error(&error),
    }
}

async fn remove_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    match delete_managed_credential(&state.runtime.deps, &provider_id) {
        Ok(()) => Json(json!({ "configured": false })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

async fn provider_quota(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    let result = state.runtime.fetch_quota_for_provider(&provider_id).await;
    Json(result).into_response()
}
