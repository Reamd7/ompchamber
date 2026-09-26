//! Port of `server/lib/session-folders/routes.js`.
//!
//! Persists the UI's session folder tree at `<data-dir>/sessions-directories.json`
//! with shape validation, last-writer-wins ordering (`updatedAt`), atomic
//! temp+rename writes, and serialized saves. A valid incoming snapshot
//! repairs malformed prior state; a stale snapshot is ignored, not rejected.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

fn is_object_record(value: &Value) -> bool {
    value.as_object().is_some()
}

fn finite_positive_number(value: &Value) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}

fn has_valid_folder_shape(folder: &Value) -> bool {
    let Some(map) = folder.as_object() else {
        return false;
    };
    map.get("id").is_some_and(Value::is_string)
        && map.get("name").is_some_and(Value::is_string)
        && map
            .get("sessionIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().all(Value::is_string))
        && map
            .get("createdAt")
            .map(finite_positive_number_at_least_zero)
            .unwrap_or(false)
        && match map.get("parentId") {
            None | Some(Value::Null) => true,
            Some(value) => value.is_string(),
        }
}

fn finite_positive_number_at_least_zero(value: &Value) -> bool {
    value.as_f64().is_some_and(|n| n.is_finite())
}

fn has_valid_folders_map_shape(folders_map: &Value) -> bool {
    let Some(map) = folders_map.as_object() else {
        return false;
    };
    map.values().all(|folders| {
        folders
            .as_array()
            .is_some_and(|list| list.iter().all(has_valid_folder_shape))
    })
}

fn has_valid_folder_snapshot_shape(snapshot: &Value) -> bool {
    is_object_record(snapshot)
        && snapshot.get("version").and_then(Value::as_i64) == Some(1)
        && snapshot
            .get("foldersMap")
            .map(has_valid_folders_map_shape)
            .unwrap_or(false)
        && snapshot
            .get("collapsedFolderIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().all(Value::is_string))
}

#[derive(Clone)]
struct ModuleState {
    file_path: PathBuf,
    save_queue: Arc<tokio::sync::Mutex<()>>,
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn read_raw(path: &PathBuf) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

async fn get_folders(State(state): State<ModuleState>) -> Response {
    let raw = match read_raw(&state.file_path).await {
        Ok(raw) => raw,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let Some(raw) = raw else {
        return Json(json!({ "version": 1, "exists": false })).into_response();
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Stored session folders are malformed",
            );
        }
    };
    let updated_at_valid = parsed
        .get("updatedAt")
        .map(finite_positive_number)
        .unwrap_or(false)
        && parsed
            .get("updatedAt")
            .and_then(Value::as_f64)
            .is_some_and(|n| n > 0.0);
    if !has_valid_folder_snapshot_shape(&parsed) || !updated_at_valid {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored session folders have an invalid shape",
        );
    }
    let mut body = parsed;
    if let Some(map) = body.as_object_mut() {
        map.insert("exists".to_string(), Value::Bool(true));
    }
    Json(body).into_response()
}

async fn post_folders(State(state): State<ModuleState>, Json(body): Json<Value>) -> Response {
    if !is_object_record(&body) {
        return error_response(StatusCode::BAD_REQUEST, "Body must be an object");
    }
    if !has_valid_folder_snapshot_shape(&body) {
        return error_response(StatusCode::BAD_REQUEST, "Invalid session folders payload");
    }
    let serialized = match serde_json::to_string_pretty(&body) {
        Ok(serialized) => serialized,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if serialized.len() > MAX_BODY_BYTES {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large");
    }
    let updated_at = body.get("updatedAt").and_then(Value::as_f64);
    match updated_at {
        Some(updated_at) if updated_at.is_finite() && updated_at > 0.0 => {}
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "updatedAt must be a positive finite number",
            );
        }
    }
    let updated_at = updated_at.expect("checked above");

    // Serialized saves (JS saveQueue): one writer at a time.
    let _guard = state.save_queue.lock().await;
    let tmp =
        state
            .file_path
            .with_extension(format!("json.tmp-{}-{}", std::process::id(), utc_millis(),));
    let mut saved = false;
    let result: Result<Response, std::io::Error> = async {
        if let Some(current_raw) = read_raw(&state.file_path).await? {
            // A valid current snapshot newer-or-equal wins; malformed prior
            // state falls through and is repaired by this save.
            if let Ok(current) = serde_json::from_str::<Value>(&current_raw) {
                let current_updated_at = if has_valid_folder_snapshot_shape(&current) {
                    current
                        .get("updatedAt")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                if current_updated_at >= updated_at {
                    return Ok(Json(json!({ "success": true, "ignored": true })).into_response());
                }
            }
        }
        if let Some(parent) = state.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&tmp, &serialized).await?;
        match tokio::fs::rename(&tmp, &state.file_path).await {
            Ok(()) => {
                saved = true;
                Ok(Json(json!({ "success": true })).into_response())
            }
            Err(e) => Err(e),
        }
    }
    .await;

    match result {
        Ok(response) => response,
        Err(e) => {
            if !saved {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

fn utc_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

pub fn router(ctx: RouterContext) -> Router {
    let file_path = ctx.config.data_dir.join("sessions-directories.json");
    Router::new()
        .route("/api/session-folders", get(get_folders).post(post_folders))
        .with_state(ModuleState {
            file_path,
            save_queue: Arc::new(tokio::sync::Mutex::new(())),
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
                "oc-session-folders-{tag}-{}-{}",
                utc_millis(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        fn app(&self) -> Router {
            Router::new()
                .route("/api/session-folders", get(get_folders).post(post_folders))
                .with_state(ModuleState {
                    file_path: self.dir.join("sessions-directories.json"),
                    save_queue: Arc::new(tokio::sync::Mutex::new(())),
                })
        }

        async fn post(&self, value: Value) -> axum::response::Response {
            self.app()
                .oneshot(
                    axum::http::Request::post("/api/session-folders")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_string(&value).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }

        async fn get(&self) -> axum::response::Response {
            self.app()
                .oneshot(
                    axum::http::Request::get("/api/session-folders")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        }

        async fn body_json(response: axum::response::Response) -> Value {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    fn sample_snapshot(updated_at: f64) -> Value {
        json!({
            "version": 1,
            "foldersMap": {
                "/repo": [
                    { "id": "f1", "name": "WIP", "sessionIds": ["s1"], "createdAt": 1.0, "parentId": null }
                ]
            },
            "collapsedFolderIds": [],
            "updatedAt": updated_at
        })
    }

    #[tokio::test]
    async fn get_missing_answers_default_snapshot() {
        let fixture = Fixture::new("missing");
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["version"], 1);
        assert_eq!(value["exists"], false);
    }

    #[tokio::test]
    async fn get_malformed_and_invalid_shape_fail_distinctly() {
        let fixture = Fixture::new("malformed");
        std::fs::write(fixture.dir.join("sessions-directories.json"), "{nope").unwrap();
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["error"], "Stored session folders are malformed");

        std::fs::write(
            fixture.dir.join("sessions-directories.json"),
            serde_json::to_string(&json!({
                "version": 1, "foldersMap": {}, "collapsedFolderIds": []
            }))
            .unwrap(),
        )
        .unwrap();
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let value = Fixture::body_json(response).await;
        assert_eq!(
            value["error"],
            "Stored session folders have an invalid shape"
        );
    }

    #[tokio::test]
    async fn post_roundtrip_and_stale_snapshot_ignored() {
        let fixture = Fixture::new("roundtrip");

        let response = fixture.post(sample_snapshot(10.0)).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Stale write is ignored, not an error.
        let response = fixture.post(sample_snapshot(5.0)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["ignored"], true);

        let response = fixture.get().await;
        let value = Fixture::body_json(response).await;
        assert_eq!(value["exists"], true);
        assert_eq!(value["updatedAt"], 10.0);
    }

    #[tokio::test]
    async fn post_rejects_invalid_payloads_with_js_messages() {
        let fixture = Fixture::new("reject");

        let response = fixture.post(json!([1])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["error"], "Body must be an object");

        let mut bad_shape = sample_snapshot(1.0);
        bad_shape["version"] = json!(2);
        let response = fixture.post(bad_shape).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let mut no_updated = sample_snapshot(0.0);
        no_updated["updatedAt"] = json!(0);
        let response = fixture.post(no_updated).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn valid_snapshot_repairs_malformed_prior_state() {
        let fixture = Fixture::new("repair");
        std::fs::write(fixture.dir.join("sessions-directories.json"), "{broken").unwrap();
        let response = fixture.post(sample_snapshot(3.0)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["success"], true);
    }
}
