//! Port of `server/lib/small-model/routes.js`.
//!
//! - `GET /api/small-model` — resolution preview.
//! - `POST /api/small-model/generate` — `{ prompt, system?, maxOutputTokens?,
//!   model?, directory? }` → `{ text, providerID, modelID, source }`.
//!
//! 中文说明：`server/lib/small-model/routes.js` 的移植——GET /api/small-model
//! 提供解析预览；POST /api/small-model/generate 接收
//! { prompt, system?, maxOutputTokens?, model?, directory? } 并返回
//! { text, providerID, modelID, source }。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value};

use crate::small_model::http::js_positive_number;
use crate::small_model::service::{GenerateParams, SmallModelService};

/// 构建本模块路由：GET /api/small-model 与 POST /api/small-model/generate，
/// 并注入共享的 SmallModelService 作为状态。
pub fn routes(service: Arc<SmallModelService>) -> Router {
    Router::new()
        .route("/api/small-model", get(describe))
        .route("/api/small-model/generate", post(generate))
        .with_state(service)
}

/// 取查询参数的克隆值；键不存在返回 None。
fn query_string(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params.get(key).cloned()
}

/// GET /api/small-model：解析当前生效的小模型，返回 available、model 与
/// authenticatedProviders；解析失败记 error 日志并返回 500 + error 消息。
async fn describe(
    State(service): State<Arc<SmallModelService>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    match service
        .describe_small_model(
            query_string(&params, "directory").as_deref(),
            query_string(&params, "providerID").as_deref(),
            query_string(&params, "modelID").as_deref(),
            Default::default(),
            None,
        )
        .await
    {
        Ok(resolved) => {
            let authenticated_providers = service.list_authenticated_providers().await;
            let mut object = Map::new();
            object.insert("available".to_string(), Value::Bool(resolved.is_some()));
            object.insert(
                "model".to_string(),
                resolved
                    .as_ref()
                    .map(|described| described.to_json())
                    .unwrap_or(Value::Null),
            );
            object.insert(
                "authenticatedProviders".to_string(),
                Value::Array(
                    authenticated_providers
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
            Json(Value::Object(object)).into_response()
        }
        Err(error) => {
            tracing::error!("Failed to resolve small model: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": if error.message.is_empty() {
                        "Failed to resolve small model".to_string()
                    } else {
                        error.message
                    },
                })),
            )
                .into_response()
        }
    }
}

/// express's `express.json()` body parser: only `*json` content types
/// populate `req.body`; anything else (or malformed) reads as absent.
///
/// 中文补充：仅当 Content-Type 含 "json" 才尝试解析；失败按无 body（Null）处理。
fn parse_json_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let json_content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if !json_content_type {
        return Value::Null;
    }
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// 取 JSON body 顶层的字符串字段。
fn body_str(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_string)
}

/// POST /api/small-model/generate：按 express.json 语义解析 body 后调用 service。
/// maxOutputTokens 沿用 JS Number() 的宽松折算；404 映射"无可用小模型"文案，
/// 其余错误保留原始状态码并返回提示用户更换小模型的固定文案（5xx 记 error 日志）。
async fn generate(
    State(service): State<Arc<SmallModelService>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body = parse_json_body(&headers, &body);
    let restrict = body.get("restrictToPreferredProvider") == Some(&Value::Bool(true));
    // JS `Number(maxOutputTokens)`: numeric strings coerce like the route
    // destructure hands them to generateSmallModelText.
    let max_output_tokens = body
        .get("maxOutputTokens")
        .and_then(|value| js_positive_number(value).map(|number| number as u64));
    let result = service
        .generate(GenerateParams {
            prompt: body_str(&body, "prompt"),
            system: body_str(&body, "system"),
            max_output_tokens,
            model: body_str(&body, "model"),
            directory: body_str(&body, "directory"),
            preferred_provider_id: body_str(&body, "preferredProviderID"),
            preferred_model_id: body_str(&body, "preferredModelID"),
            restrict_to_preferred_provider: restrict,
            ..Default::default()
        })
        .await;
    match result {
        Ok(output) => Json(output.to_json()).into_response(),
        Err(error) => {
            let status_code = if error.status_code == 0 {
                500
            } else {
                error.status_code
            };
            if status_code >= 500 {
                tracing::error!("Small model generation failed: {error}");
            }
            let message = if status_code == 404 {
                if error.message.is_empty() {
                    "No small model is available".to_string()
                } else {
                    error.message
                }
            } else {
                "The selected Small Model could not complete this action. Choose another model in Settings → Sessions → Small Model and try again.".to_string()
            };
            let mut object = Map::new();
            object.insert("error".to_string(), Value::String(message));
            if let Some(code) = error.code {
                object.insert("code".to_string(), Value::String(code));
            }
            (
                StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(Value::Object(object)),
            )
                .into_response()
        }
    }
}
