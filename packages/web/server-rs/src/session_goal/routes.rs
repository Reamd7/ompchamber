//! Port of `server/lib/session-goal/routes.js`.
//!
//! OMPChamber-owned routes for file-backed goal objectives, keyed by session
//! id (one goal per session; a new goal overwrites the old file). The UI
//! writes the objective file before stamping the goal metadata (which only
//! carries an `objectiveFile: true` flag), reads it back for display, and
//! deletes it when the goal is removed.

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::session_goal::objectives::{delete_objective, read_objective, write_objective};

pub fn routes(data_dir: PathBuf) -> Router {
    Router::new()
        .route(
            "/api/goals/objective/{sessionId}",
            put(put_objective)
                .get(get_objective)
                .delete(delete_objective_route),
        )
        .with_state(data_dir)
}

/// JS relies on express's `express.json()` body parser: only `*json` content
/// types populate `req.body`; anything else (or a malformed body) leaves it
/// undefined, which writeObjective answers with 400 "objective content is
/// required".
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

async fn put_objective(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = parse_json_body(&headers, &body);
    let content = parsed.get("content").cloned().unwrap_or(Value::Null);
    match write_objective(&data_dir, &session_id, &content).await {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(error) => {
            if error.status_code() >= 500 {
                tracing::error!("Failed to write goal objective: {error}");
            }
            (
                StatusCode::from_u16(error.status_code())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

async fn get_objective(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
) -> Response {
    match read_objective(&data_dir, &session_id).await {
        Some(content) => Json(json!({ "content": content })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "objective not found" })),
        )
            .into_response(),
    }
}

async fn delete_objective_route(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
) -> Response {
    delete_objective(&data_dir, &session_id).await;
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-goal-routes-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    async fn body_string(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn put_get_delete_objective_roundtrip_shapes() {
        let dir = temp_dir("roundtrip");
        let app = routes(dir.clone());

        let put = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route1")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"Finish the task"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK);
        assert_eq!(body_string(put).await, r#"{"ok":true}"#);

        let get = app
            .clone()
            .oneshot(
                axum::http::Request::get("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(body_string(get).await, r#"{"content":"Finish the task"}"#);

        let delete = app
            .clone()
            .oneshot(
                axum::http::Request::delete("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::OK);
        assert_eq!(body_string(delete).await, r#"{"ok":true}"#);

        let get_after = app
            .oneshot(
                axum::http::Request::get("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_after.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(get_after).await,
            r#"{"error":"objective not found"}"#
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_rejects_invalid_session_id_and_missing_content() {
        let dir = temp_dir("rejects");
        let app = routes(dir.clone());

        let invalid = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/..%2Fescape")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"text"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(invalid).await,
            r#"{"error":"invalid session id"}"#
        );

        let empty = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route2")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(empty).await,
            r#"{"error":"objective content is required"}"#
        );

        // Non-JSON content types never populate the body (express behavior).
        let text = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route2")
                    .header("content-type", "text/plain")
                    .body(Body::from("{\"content\":\"text\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(text.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(text).await,
            r#"{"error":"objective content is required"}"#
        );

        // DELETE is always ok, even for ids that were never valid.
        let delete = app
            .oneshot(
                axum::http::Request::delete("/api/goals/objective/never-there")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::OK);

        std::fs::remove_dir_all(&dir).ok();
    }
}
