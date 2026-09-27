//! `/api/walkthrough*` 四条路由（读取、生成、进度、取消）：参数解析对齐
//! express 的宽松语义，错误统一经 WalkthroughError 转成响应。生成不因
//! 客户端断开而中止——任务跑在自己的 task 上且条目已落盘。
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

/// 注册 walkthrough 全部路由，并以服务实例作共享 state。
pub fn routes(service: Arc<WalkthroughService>) -> Router {
    Router::new()
        .route("/api/walkthrough", get(get_walkthrough))
        .route("/api/walkthrough/generate", post(generate))
        .route("/api/walkthrough/progress", get(progress))
        .route("/api/walkthrough/cancel", post(cancel))
        .with_state(service)
}

/// 解析 query 中的 `source` 参数：它是 JSON 编码的描述符字符串，
/// 为空或解析失败都按缺失处理。
/// `readSource(value)`: the query carries the descriptor as a JSON-encoded
/// string; anything else reads as absent.
fn read_source(value: Option<&String>) -> Option<Value> {
    let text = value.filter(|text| !text.is_empty())?;
    serde_json::from_str(text).ok()
}

/// 按 express.json() 的语义解析请求体：仅 `*json` content-type 才尝试，
/// 其余情况（或解析失败）一律得到 `{}`。
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

/// 从 JSON 对象取字符串字段（非字符串按缺失处理）。
fn field_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// GET /api/walkthrough：读取（或触发）某目录 + 来源的 walkthrough；
/// `directory` 缺失直接 400。
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

/// 从 query map 取参数：存在即透传（含空串），回落交由下游解析决定。
/// `typeof req.query.model === 'string' ? req.query.model : undefined` —
/// absent reads as absent; present-but-empty still passes through as a
/// string (and falls back inside resolution).
fn field_str_in(query: &HashMap<String, String>, key: &str) -> Option<String> {
    query.get(key).cloned()
}

/// POST /api/walkthrough/generate：发起生成；`force` 为 true 时忽略缓存
/// 重写。响应即使客户端已断开也照常写出（任务与其条目已落盘）。
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

/// GET /api/walkthrough/progress：查询生成阶段（纯内存读，可在生成
/// 进行中安全轮询；完整读取会重跑 git 管线，不用于此）。
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

/// POST /api/walkthrough/cancel：显式取消进行中的生成任务。
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

/// 路由层测试（见 tests 子模块文件）。
#[cfg(test)]
mod tests;
