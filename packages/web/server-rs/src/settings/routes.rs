//! `GET`/`PUT /api/config/settings` — port of the settings endpoints from
//! `server/lib/opencode/routes.js` (body parsing mirrors the server-wide
//! `express.json({ limit: '50mb' })` mount from `core-routes.js`).

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
const SETTINGS_BODY_LIMIT: usize = 50 * 1024 * 1024;

pub fn router(store: Arc<SettingsStore>) -> axum::Router {
    axum::Router::new()
        .route("/api/config/settings", get(get_settings).put(put_settings))
        .with_state(store)
}

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

fn failed_to_read() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Failed to read settings" })),
    )
        .into_response()
}

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
