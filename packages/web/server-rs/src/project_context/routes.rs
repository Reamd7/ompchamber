//! Port of `server/lib/project-context/routes.js`.
//!
//! OMPChamber project context routes: notes, todos, and plan files. Plan
//! markdown is addressed by id rather than by an absolute path supplied by the
//! caller.
//!
//! Body parsing mirrors the JS's per-route `express.json({ limit: '1mb' })`:
//! the server has no global JSON parser (the OpenCode proxy needs an unread
//! request stream), so a request without a JSON content type leaves `req.body`
//! undefined and every write handler rejects it with `Body must be an object`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::runtime::ProjectContextRuntime;

/// `express.json({ limit: '1mb' })`.
const BODY_LIMIT: usize = 1024 * 1024;

pub fn router(runtime: Arc<ProjectContextRuntime>) -> Router {
    Router::new()
        .route("/api/project-context/{projectId}", get(get_context))
        .route("/api/project-context/{projectId}/todos", put(put_todos))
        .route("/api/project-context/{projectId}/notes", post(post_notes))
        .route(
            "/api/project-context/{projectId}/notes/{noteId}",
            patch(patch_note).delete(delete_note),
        )
        .route("/api/project-context/{projectId}/plans", post(post_plans))
        .route(
            "/api/project-context/{projectId}/plans/{planId}",
            patch(patch_plan_pinned)
                .get(get_plan)
                .put(put_plan)
                .delete(delete_plan),
        )
        .with_state(runtime)
}

// ---------------------------------------------------------------------------
// Body parsing (express.json semantics)
// ---------------------------------------------------------------------------

/// Returns `Value::Null` where express would leave `req.body` undefined (no
/// JSON content type); an empty JSON body parses as `{}`; malformed JSON is a
/// 400 and an over-limit body is a 413, both before the handler runs.
async fn parse_json_body(headers: &HeaderMap, body: axum::body::Body) -> Result<Value, Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let is_json =
        mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"));
    if !is_json {
        return Ok(Value::Null);
    }

    let bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let status = if error.to_string().contains("length limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return Err((status, error.to_string()).into_response());
        }
    };
    if bytes.is_empty() {
        // body-parser: an empty JSON body yields `{}`.
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => Ok(value),
        Err(error) => Err((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
    }
}

// ---------------------------------------------------------------------------
// Response helpers (port of respondWithError)
// ---------------------------------------------------------------------------

fn is_validation_error(message: &str) -> bool {
    message.contains("is required") || message.contains("unsupported characters")
}

fn respond_with_error(error: crate::error::AppError, fallback_message: &str) -> Response {
    let raw_message = error.to_string();
    if is_validation_error(&raw_message) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": raw_message })),
        )
            .into_response();
    }
    let message = if raw_message.is_empty() {
        fallback_message.to_string()
    } else {
        raw_message
    };
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

fn is_object_record(value: &Value) -> bool {
    value.as_object().is_some()
}

fn is_valid_note_source(value: &Value) -> bool {
    matches!(value.as_str(), Some("manual" | "selection" | "agent"))
}

/// `hasValidTodosShape`.
fn has_valid_todos_shape(value: &Value) -> bool {
    let Some(entries) = value.as_array() else {
        return false;
    };
    entries.iter().all(|todo| {
        let Some(object) = todo.as_object() else {
            return false;
        };
        object.get("id").is_some_and(Value::is_string)
            && object.get("text").is_some_and(Value::is_string)
            && object.get("completed").is_none_or(Value::is_boolean)
            && object.get("createdAt").is_none_or(Value::is_number)
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn get_context(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path(project_id): Path<String>,
) -> Response {
    match runtime.read_context(&project_id).await {
        Ok(context) => Json(context).into_response(),
        Err(error) => respond_with_error(error, "Failed to read project context"),
    }
}

async fn put_todos(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path(project_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) {
        return bad_request("Body must be an object");
    }
    let todos = parsed.get("todos").unwrap_or(&Value::Null);
    if !has_valid_todos_shape(todos) {
        return bad_request("todos must be an array of todo items");
    }

    match runtime.save_todos(&project_id, todos).await {
        Ok(context) => Json(context).into_response(),
        Err(error) => respond_with_error(error, "Failed to save project todos"),
    }
}

async fn post_notes(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path(project_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) {
        return bad_request("Body must be an object");
    }
    if !parsed.get("body").is_some_and(Value::is_string) {
        return bad_request("body must be a string");
    }
    if parsed.get("source").is_some()
        && !is_valid_note_source(parsed.get("source").unwrap_or(&Value::Null))
    {
        return bad_request("source must be manual, selection, or agent");
    }
    if parsed.get("origin").is_some()
        && !is_object_record(parsed.get("origin").unwrap_or(&Value::Null))
    {
        return bad_request("origin must be an object");
    }

    match runtime.create_note(&project_id, &parsed).await {
        Ok(result) => (
            StatusCode::CREATED,
            Json(json!({ "note": result.note, "context": result.context })),
        )
            .into_response(),
        Err(error) => respond_with_error(error, "Failed to create note"),
    }
}

async fn patch_note(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, note_id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) {
        return bad_request("Body must be an object");
    }
    if parsed.get("body").is_some() && !parsed.get("body").is_some_and(Value::is_string) {
        return bad_request("body must be a string");
    }
    if parsed.get("pinned").is_some() && !parsed.get("pinned").is_some_and(Value::is_boolean) {
        return bad_request("pinned must be a boolean");
    }

    let mut patch = Map::new();
    if let Some(body_value) = parsed.get("body") {
        patch.insert("body".to_string(), body_value.clone());
    }
    if let Some(pinned) = parsed.get("pinned") {
        patch.insert("pinned".to_string(), pinned.clone());
    }

    match runtime
        .update_note(&project_id, &note_id, &Value::Object(patch))
        .await
    {
        Ok(Some(result)) => {
            Json(json!({ "note": result.note, "context": result.context })).into_response()
        }
        Ok(None) => not_found("Note not found"),
        Err(error) => respond_with_error(error, "Failed to save note"),
    }
}

async fn delete_note(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, note_id)): Path<(String, String)>,
) -> Response {
    match runtime.delete_note(&project_id, &note_id).await {
        Ok(outcome) if outcome.deleted => Json(outcome.context).into_response(),
        Ok(_) => not_found("Note not found"),
        Err(error) => respond_with_error(error, "Failed to delete note"),
    }
}

async fn patch_plan_pinned(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, plan_id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) || !parsed.get("pinned").is_some_and(Value::is_boolean) {
        return bad_request("pinned must be a boolean");
    }

    let pinned = parsed
        .get("pinned")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match runtime.set_plan_pinned(&project_id, &plan_id, pinned).await {
        Ok(Some(result)) => {
            Json(json!({ "plan": result.plan, "context": result.context })).into_response()
        }
        Ok(None) => not_found("Plan not found"),
        Err(error) => respond_with_error(error, "Failed to update plan"),
    }
}

async fn get_plan(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, plan_id)): Path<(String, String)>,
) -> Response {
    match runtime.read_plan(&project_id, &plan_id).await {
        Ok(Some(plan)) => Json(plan).into_response(),
        Ok(None) => not_found("Plan not found"),
        Err(error) => respond_with_error(error, "Failed to read plan"),
    }
}

async fn put_plan(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, plan_id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) {
        return bad_request("Body must be an object");
    }
    if !parsed.get("raw").is_some_and(Value::is_string) {
        return bad_request("raw must be a string");
    }

    let payload = json!({ "raw": parsed.get("raw").unwrap_or(&Value::Null) });
    match runtime.update_plan(&project_id, &plan_id, &payload).await {
        Ok(Some(result)) => Json(json!({
            "plan": result.plan,
            "context": result.context,
            "title": result.title,
            "body": result.body,
            "raw": result.raw,
        }))
        .into_response(),
        Ok(None) => not_found("Plan not found"),
        Err(error) => respond_with_error(error, "Failed to save plan"),
    }
}

async fn post_plans(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path(project_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_object_record(&parsed) {
        return bad_request("Body must be an object");
    }
    if !parsed.get("body").is_some_and(Value::is_string) {
        return bad_request("body must be a string");
    }
    if parsed.get("title").is_some() && !parsed.get("title").is_some_and(Value::is_string) {
        return bad_request("title must be a string");
    }

    let payload = json!({
        "title": parsed.get("title").and_then(Value::as_str).unwrap_or(""),
        "body": parsed.get("body").unwrap_or(&Value::Null),
    });
    match runtime.create_plan(&project_id, &payload).await {
        Ok(result) => (
            StatusCode::CREATED,
            Json(json!({ "plan": result.plan, "context": result.context })),
        )
            .into_response(),
        Err(error) => respond_with_error(error, "Failed to create plan"),
    }
}

async fn delete_plan(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path((project_id, plan_id)): Path<(String, String)>,
) -> Response {
    match runtime.delete_plan(&project_id, &plan_id).await {
        Ok(outcome) if outcome.deleted => Json(outcome.context).into_response(),
        Ok(_) => not_found("Plan not found"),
        Err(error) => respond_with_error(error, "Failed to delete plan"),
    }
}
