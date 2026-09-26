//! Port of `server/lib/small-model/routes.js`.
//!
//! - `GET /api/small-model` — resolution preview.
//! - `POST /api/small-model/generate` — `{ prompt, system?, maxOutputTokens?,
//!   model?, directory? }` → `{ text, providerID, modelID, source }`.

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

pub fn routes(service: Arc<SmallModelService>) -> Router {
    Router::new()
        .route("/api/small-model", get(describe))
        .route("/api/small-model/generate", post(generate))
        .with_state(service)
}

fn query_string(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params.get(key).cloned()
}

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

fn body_str(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_string)
}

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
