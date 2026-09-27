//! `GET`/`PUT /api/config/settings` — port of the settings endpoints from
//! `server/lib/opencode/routes.js` (body parsing mirrors the server-wide
//! `express.json({ limit: '50mb' })` mount from `core-routes.js`).
//!
//! 中文说明：本模块挂载 settings 的读/写两个 HTTP 端点；GET 返回迁移
//! 后的完整 settings，PUT 走 store.persist 清洗落盘并返回更新结果。

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use super::runtime::SettingsStore;

/// JS: `express.json({ limit: '50mb' })` for `/api/config/*` paths.
/// 与 core-routes.js 中 `express.json({ limit: '50mb' })` 的挂载上限一致（字节）。
const SETTINGS_BODY_LIMIT: usize = 50 * 1024 * 1024;

/// 构建 settings 路由：`/api/config/settings` 的 GET 与 PUT 共用一个 store 状态。
pub fn router(store: Arc<SettingsStore>) -> axum::Router {
    axum::Router::new()
        .route("/api/config/settings", get(get_settings).put(put_settings))
        .with_state(store)
}

/// `GET /api/config/settings`：读取（必要时迁移）settings 并以 JSON 返回；
/// 读取或格式化失败统一记日志并返回 500。
async fn get_settings(State(store): State<Arc<SettingsStore>>) -> Response {
    match store.read_migrated().await {
        Ok(settings) => match super::helpers::format_settings_response(&settings) {
            Ok(response) => Json(response).into_response(),
            Err(err) => {
                tracing::error!("Failed to read settings: {err}");
                failed_to_read()
            }
        },
        Err(err) => {
            tracing::error!("Failed to read settings: {err}");
            failed_to_read()
        }
    }
}

/// 读取失败的统一 500 响应体（与 JS 端点的错误文案保持一致）。
fn failed_to_read() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Failed to read settings" })),
    )
        .into_response()
}

/// `PUT /api/config/settings`：先按 express.json 语义解析请求体，再持久化；
/// 解析错误直接返回对应的 400/413 响应，持久化失败返回 500。
/// 成功时返回清洗后的最新 settings JSON。
async fn put_settings(State(store): State<Arc<SettingsStore>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match parse_json_body(parts.headers, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match store.persist(&body).await {
        Ok(updated) => Json(updated).into_response(),
        Err(err) => {
            tracing::error!("[API:PUT /api/config/settings] Failed to save settings: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to save settings" })),
            )
                .into_response()
        }
    }
}

/// Mirror `express.json`: only `application/json` / `application/*+json`
/// bodies are parsed (an empty JSON body parses as `{}`), anything else is
/// left unparsed (`req.body ?? {}`), malformed JSON is a 400, and an
/// over-limit body is a 413.
/// 中文说明：Content-Type 仅取分号前的 MIME 并转小写比较；非 JSON 类型
/// 返回 Null（调用方按 `req.body ?? {}` 处理）；读取 body 超限时
/// 依据错误信息区分 413 与 400。
async fn parse_json_body(headers: HeaderMap, body: Body) -> Result<Value, Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
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
        // express.json skips the request entirely → `req.body ?? {}`.
        return Ok(Value::Null);
    }

    let bytes = match axum::body::to_bytes(body, SETTINGS_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let status = if err.to_string().contains("length limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return Err((status, format!("{err}")).into_response());
        }
    };
    if bytes.is_empty() {
        // body-parser: an empty JSON body yields `{}`.
        return Ok(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => Ok(value),
        Err(err) => {
            // body-parser rejects with a 400 before the route handler runs.
            Err((StatusCode::BAD_REQUEST, err.to_string()).into_response())
        }
    }
}
