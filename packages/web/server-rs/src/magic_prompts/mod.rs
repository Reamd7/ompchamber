//! Port of `server/lib/magic-prompts/{runtime,routes}.js`.
//!
//! Persisted prompt overrides at `<data-dir>/magic-prompts.json`. Read
//! failures (missing or malformed) degrade to the empty default with a
//! warning — the JS module treats this cache as non-authoritative. Writes
//! are serialized (JS `writeLock` chain) and pretty-printed 2-space JSON.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;

const FILE_VERSION: u64 = 1;
const MAX_PROMPT_TEXT_LENGTH: usize = 200_000;
const PROMPT_ID_MAX: usize = 160;

#[derive(Debug, Clone, Default)]
pub struct PromptState {
    pub overrides: BTreeMap<String, String>,
}

impl PromptState {
    fn to_json(&self) -> Value {
        json!({ "version": FILE_VERSION, "overrides": self.overrides })
    }
}

/// `PROMPT_ID_PATTERN`: `/^[a-z0-9._-]{1,160}$/`.
fn is_valid_prompt_id(id: &str) -> bool {
    let len = id.len();
    (1..=PROMPT_ID_MAX).contains(&len)
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn is_visible_prompt_id(id: &str) -> bool {
    id.ends_with(".visible")
}

/// sanitizeOverrides: keep only id-pattern keys with string values.
fn sanitize_overrides(value: &Value) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    if let Some(map) = value.get("overrides").and_then(Value::as_object) {
        for (id, entry) in map {
            if is_valid_prompt_id(id)
                && let Some(text) = entry.as_str()
            {
                overrides.insert(id.clone(), text.to_string());
            }
        }
    }
    overrides
}

#[derive(Clone)]
struct ModuleState {
    file_path: PathBuf,
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ModuleState {
    async fn read_prompt_state(&self) -> PromptState {
        match tokio::fs::read_to_string(&self.file_path).await {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(parsed) => PromptState {
                    overrides: sanitize_overrides(&parsed),
                },
                Err(error) => {
                    tracing::warn!("Failed to read magic prompts file: {error}");
                    PromptState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PromptState::default(),
            Err(e) => {
                tracing::warn!("Failed to read magic prompts file: {e}");
                PromptState::default()
            }
        }
    }

    async fn write_prompt_state(&self, state: &PromptState) -> std::io::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &self.file_path,
            serde_json::to_string_pretty(&state.to_json())?,
        )
        .await
    }

    async fn persist<F>(&self, mutator: F) -> Result<PromptState, String>
    where
        F: FnOnce(PromptState) -> PromptState,
    {
        let _guard = self.write_lock.lock().await;
        let current = self.read_prompt_state().await;
        let next = mutator(current);
        self.write_prompt_state(&next)
            .await
            .map_err(|e| e.to_string())?;
        Ok(next)
    }

    async fn set_override(&self, id: &str, text: Option<&str>) -> Result<PromptState, String> {
        let normalized = id.trim();
        if !is_valid_prompt_id(normalized) {
            return Err("Invalid prompt id".to_string());
        }
        let Some(text) = text else {
            return Err("Prompt text must be a string".to_string());
        };
        if is_visible_prompt_id(normalized) && text.trim().is_empty() {
            return Err("Visible prompt text cannot be empty".to_string());
        }
        if text.len() > MAX_PROMPT_TEXT_LENGTH {
            return Err("Prompt text is too long".to_string());
        }
        let key = normalized.to_string();
        let value = text.to_string();
        self.persist(|state| PromptState {
            overrides: {
                let mut overrides = state.overrides;
                overrides.insert(key, value);
                overrides
            },
        })
        .await
    }

    async fn reset_override(&self, id: &str) -> Result<PromptState, String> {
        let normalized = id.trim();
        if !is_valid_prompt_id(normalized) {
            return Err("Invalid prompt id".to_string());
        }
        let key = normalized.to_string();
        self.persist(|state| PromptState {
            overrides: {
                let mut overrides = state.overrides;
                overrides.remove(&key);
                overrides
            },
        })
        .await
    }

    async fn reset_all_overrides(&self) -> Result<PromptState, String> {
        self.persist(|_| PromptState::default()).await
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// JS routes map these message fragments to 400; anything else is 500.
fn status_for(message: &str) -> StatusCode {
    if message.contains("Invalid prompt id")
        || message.contains("too long")
        || message.contains("cannot be empty")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

async fn get_prompts(State(state): State<ModuleState>) -> Response {
    Json(state.read_prompt_state().await.to_json()).into_response()
}

async fn put_prompt(
    State(state): State<ModuleState>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    let text = body.get("text").and_then(Value::as_str);
    if body.get("text").is_some_and(|v| !v.is_string()) {
        return error_response(StatusCode::BAD_REQUEST, "text is required");
    }
    match state.set_override(&id, text).await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => error_response(status_for(&message), &message),
    }
}

async fn delete_prompt(State(state): State<ModuleState>, Path(id): Path<String>) -> Response {
    match state.reset_override(&id).await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => {
            let status = if message.contains("Invalid prompt id") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error_response(status, &message)
        }
    }
}

async fn delete_all_prompts(State(state): State<ModuleState>) -> Response {
    match state.reset_all_overrides().await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

pub fn router(ctx: RouterContext) -> Router {
    Router::new()
        .route(
            "/api/magic-prompts",
            get(get_prompts).delete(delete_all_prompts),
        )
        .route(
            "/api/magic-prompts/{id}",
            axum::routing::put(put_prompt).delete(delete_prompt),
        )
        .with_state(ModuleState {
            file_path: ctx.config.data_dir.join("magic-prompts.json"),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oc-magic-prompts-{tag}-{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        fn app(&self) -> Router {
            Router::new()
                .route(
                    "/api/magic-prompts",
                    get(get_prompts).delete(delete_all_prompts),
                )
                .route(
                    "/api/magic-prompts/{id}",
                    axum::routing::put(put_prompt).delete(delete_prompt),
                )
                .with_state(ModuleState {
                    file_path: self.dir.join("magic-prompts.json"),
                    write_lock: Arc::new(tokio::sync::Mutex::new(())),
                })
        }

        async fn request(
            &self,
            method: &str,
            uri: &str,
            body: Option<String>,
        ) -> axum::response::Response {
            let mut builder = axum::http::Request::builder().method(method).uri(uri);
            if body.is_some() {
                builder = builder.header("content-type", "application/json");
            }
            self.app()
                .oneshot(builder.body(Body::from(body.unwrap_or_default())).unwrap())
                .await
                .unwrap()
        }

        async fn json(response: axum::response::Response) -> Value {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    #[tokio::test]
    async fn roundtrip_set_get_reset() {
        let fixture = Fixture::new("roundtrip");
        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/my.prompt-1",
                Some(r#"{"text":"hello"}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert_eq!(value["overrides"]["my.prompt-1"], "hello");

        let response = fixture.request("GET", "/api/magic-prompts", None).await;
        let value = Fixture::json(response).await;
        assert_eq!(value["version"], 1);
        assert_eq!(value["overrides"]["my.prompt-1"], "hello");

        let response = fixture
            .request("DELETE", "/api/magic-prompts/my.prompt-1", None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert!(value["overrides"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn id_and_text_validation_mirror_js() {
        let fixture = Fixture::new("validation");

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/BAD%20ID",
                Some(r#"{"text":"x"}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Fixture::json(response).await["error"], "Invalid prompt id");

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/a.visible",
                Some(r#"{"text":"  "}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Fixture::json(response).await["error"],
            "Visible prompt text cannot be empty"
        );

        let long = "x".repeat(MAX_PROMPT_TEXT_LENGTH + 1);
        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/big",
                Some(format!(r#"{{"text":"{long}"}}"#)),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Fixture::json(response).await["error"],
            "Prompt text is too long"
        );

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/ok",
                Some(r#"{"text":42}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Fixture::json(response).await["error"], "text is required");
    }

    #[tokio::test]
    async fn malformed_file_degrades_to_empty_not_error() {
        let fixture = Fixture::new("malformed");
        std::fs::write(fixture.dir.join("magic-prompts.json"), "{nope").unwrap();
        let response = fixture.request("GET", "/api/magic-prompts", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert!(value["overrides"].as_object().unwrap().is_empty());
    }

    #[test]
    fn prompt_id_pattern_matches_js() {
        assert!(is_valid_prompt_id("a"));
        assert!(is_valid_prompt_id("my.prompt_1-x"));
        assert!(!is_valid_prompt_id(""));
        assert!(!is_valid_prompt_id("UPPER"));
        assert!(!is_valid_prompt_id("a b"));
        assert!(!is_valid_prompt_id(&"a".repeat(PROMPT_ID_MAX + 1)));
    }
}
