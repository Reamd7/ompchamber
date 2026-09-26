//! Port of `server/lib/walkthrough/routes.js` — `/api/walkthrough*`.
//!
//! Generation is deliberately not aborted when the client disconnects: it
//! runs for minutes and a refresh must not throw the work away. In axum the
//! handler is polled to completion by default and the job runs on its own
//! task regardless, so the JS `clientIsGone` socket check (an express quirk)
//! has no equivalent to mirror — the response is simply written.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::service::WalkthroughService;

pub fn routes(service: Arc<WalkthroughService>) -> Router {
    Router::new()
        .route("/api/walkthrough", get(get_walkthrough))
        .route("/api/walkthrough/generate", post(generate))
        .route("/api/walkthrough/progress", get(progress))
        .route("/api/walkthrough/cancel", post(cancel))
        .with_state(service)
}

/// `readSource(value)`: the query carries the descriptor as a JSON-encoded
/// string; anything else reads as absent.
fn read_source(value: Option<&String>) -> Option<Value> {
    let text = value.filter(|text| !text.is_empty())?;
    serde_json::from_str(text).ok()
}

/// JS relies on express's `express.json()` body parser: only `*json` content
/// types populate `req.body`; anything else (or a malformed body) reads as
/// `{}`.
fn parse_json_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let json_content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if !json_content_type {
        return json!({});
    }
    serde_json::from_slice(body).unwrap_or(json!({}))
}

fn field_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

async fn get_walkthrough(
    State(service): State<Arc<WalkthroughService>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let directory = query.get("directory").cloned().unwrap_or_default();
    if directory.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "directory parameter is required" })),
        )
            .into_response();
    }

    match service
        .get_walkthrough(
            &directory,
            read_source(query.get("source")).as_ref(),
            field_str_in(&query, "model").as_deref(),
            field_str_in(&query, "language").as_deref(),
        )
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => error.into_response(),
    }
}

/// `typeof req.query.model === 'string' ? req.query.model : undefined` —
/// absent reads as absent; present-but-empty still passes through as a
/// string (and falls back inside resolution).
fn field_str_in(query: &HashMap<String, String>, key: &str) -> Option<String> {
    query.get(key).cloned()
}

async fn generate(
    State(service): State<Arc<WalkthroughService>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = parse_json_body(&headers, &body);
    let directory = field_str(&parsed, "directory").unwrap_or_default();
    if directory.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "directory is required" })),
        )
            .into_response();
    }

    let source = parsed.get("source").cloned();
    let force = parsed.get("force") == Some(&Value::Bool(true));
    let result = service
        .generate_walkthrough(
            &directory,
            source.as_ref(),
            force,
            field_str(&parsed, "model").as_deref(),
            field_str(&parsed, "language").as_deref(),
        )
        .await;
    // Deliberately delivered even if the requesting client is gone: the job
    // outlives the request and its entry is already written.
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn progress(
    State(service): State<Arc<WalkthroughService>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    // Memory-only, so it is safe to poll while a generation runs. The full
    // read re-runs the whole git pipeline and must not be used for this.
    let directory = query.get("directory").cloned().unwrap_or_default();
    if directory.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "directory parameter is required" })),
        )
            .into_response();
    }

    match service
        .repository_root_for(&directory, read_source(query.get("source")).as_ref())
        .await
    {
        Ok((repo_root, source_key)) => Json(json!({
            "stage": service.generation_stage(&repo_root, &source_key),
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn cancel(
    State(service): State<Arc<WalkthroughService>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = parse_json_body(&headers, &body);
    let directory = field_str(&parsed, "directory").unwrap_or_default();
    if directory.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "directory is required" })),
        )
            .into_response();
    }

    let source = parsed.get("source").cloned();
    match service.cancel_generation(&directory, source.as_ref()).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
mod tests;
