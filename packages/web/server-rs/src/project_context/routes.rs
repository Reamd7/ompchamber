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
//!
//! （中文说明）project-context HTTP 路由（axum 移植）：提供项目笔记、待办与
//! 计划文档的 REST 端点；计划 markdown 以 id 寻址，而非由调用方传入绝对路径。
//! 请求体解析复刻 JS 端逐路由的 `express.json({ limit: '1mb' })` 语义：server
//! 不设全局 JSON 解析器（OpenCode proxy 需要未读取的请求流），因此无 JSON
//! content type 的请求 body 视为未定义（`Value::Null`），所有写 handler 都会
//! 以 `Body must be an object` 拒绝。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::runtime::ProjectContextRuntime;

/// `express.json({ limit: '1mb' })`.
/// 请求体大小上限 1 MiB；超限在进入 handler 前即返回 413。
const BODY_LIMIT: usize = 1024 * 1024;

/// 组装 project-context 路由表并绑定共享 runtime 作为 state：`GET /{projectId}`
/// 读整体上下文，`PUT .../todos` 整体替换待办，`.../notes` 与 `.../plans`
/// 提供增删改查及计划置顶切换，全部挂在 `/api/project-context` 前缀下。
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
///
/// 按 express.json 语义解析请求体：Content-Type 非 JSON 时返回 `Value::Null`
/// （对应 express 中 `req.body` 保持 undefined）；空 JSON body 解析为 `{}`；
/// JSON 语法错误返回 400、超过 [`BODY_LIMIT`] 返回 413——两者都以
/// `Err(Response)` 在 handler 之前短路。
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

/// 判断错误消息是否属于输入校验类（"is required" / "unsupported characters"）；
/// `respond_with_error` 借此把这类 runtime 错误降级为 400 而非 500。
fn is_validation_error(message: &str) -> bool {
    message.contains("is required") || message.contains("unsupported characters")
}

/// `respondWithError` 的移植：校验类错误返回 400，其余返回 500；错误消息为空
/// 时用 `fallback_message` 兜底，响应体统一为 `{ "error": message }` JSON。
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

/// 构造 400 响应，body 为 `{ "error": message }`。
fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// 构造 404 响应，body 为 `{ "error": message }`。
fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

/// 判断 JSON 值是否为 object（JS `typeof x === 'object'` 语义），用于请求体形状校验。
fn is_object_record(value: &Value) -> bool {
    value.as_object().is_some()
}

/// 笔记 `source` 字段白名单校验：仅接受 `manual`、`selection` 或 `agent`。
fn is_valid_note_source(value: &Value) -> bool {
    matches!(value.as_str(), Some("manual" | "selection" | "agent"))
}

/// `hasValidTodosShape`.
/// `hasValidTodosShape` 的移植：todos 必须是数组，且每项含字符串 `id` 与
/// `text`；可选的 `completed` 须为 boolean、`createdAt` 须为数字。
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

/// GET `/api/project-context/{projectId}`：读取并返回整个项目上下文
/// （notes + todos + plans 索引）；读取失败按 `respond_with_error` 语义兜底。
async fn get_context(
    State(runtime): State<Arc<ProjectContextRuntime>>,
    Path(project_id): Path<String>,
) -> Response {
    match runtime.read_context(&project_id).await {
        Ok(context) => Json(context).into_response(),
        Err(error) => respond_with_error(error, "Failed to read project context"),
    }
}

/// PUT `.../todos`：整体替换项目待办列表。请求体必须是 object 且其 `todos`
/// 字段通过 `has_valid_todos_shape` 校验；成功返回更新后的完整上下文。
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

/// POST `.../notes`：创建笔记。校验 `body` 为必填字符串、可选 `source` 属于
/// 白名单、可选 `origin` 为 object；成功返回 201 与 `{ note, context }`。
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

/// PATCH `.../notes/{noteId}`：部分更新笔记。仅接受可选的 `body`（字符串）与
/// `pinned`（boolean），其余字段忽略；笔记不存在返回 404，成功返回
/// `{ note, context }`。
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

/// DELETE `.../notes/{noteId}`：删除笔记。`outcome.deleted` 为真时返回更新
/// 后的上下文，笔记本就不存在则返回 404。
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

/// PATCH `.../plans/{planId}`：切换计划置顶。请求体必须是 object 且含
/// boolean `pinned`；计划不存在返回 404，成功返回 `{ plan, context }`。
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

/// GET `.../plans/{planId}`：按 id 读取单个计划的解析结果；不存在返回 404。
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

/// PUT `.../plans/{planId}`：整体替换计划 markdown（`raw` 必须为字符串）；
/// 返回 `{ plan, context, title, body, raw }`，计划不存在返回 404。
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

/// POST `.../plans`：创建新计划。`body`（markdown 文本）为必填字符串，
/// `title` 为可选字符串（缺省空串）；成功返回 201 与 `{ plan, context }`。
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

/// DELETE `.../plans/{planId}`：删除计划。`outcome.deleted` 为真时返回更新
/// 后的上下文，计划本就不存在则返回 404。
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
