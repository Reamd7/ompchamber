//! Port of `server/lib/openchamber-control/routes.js` — the authenticated
//! CLI HTTP adapter over the control service.
//!
//! `POST /api/ompchamber/control` (JSON body, 1 MB like
//! `express.json({ limit: '1mb' })`): forwards one action, preserves
//! service status and partial-result details on failures, and propagates
//! request cancellation. Cancellation in axum means the handler future is
//! dropped when the client disconnects — the in-flight wait stops and no
//! response is written, which is the observable contract of the JS
//! abort-on-disconnect wiring (`res.writableEnded` guard meant the 499
//! error body never reached a closed socket either).

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::engine_client::BoxFut;
use super::error::ControlError;
use super::service::ControlService;

/// The route forwards one action — tests substitute a double (JS tests
/// inject `{ controlService: { execute } }`).
pub trait ControlExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        action: &'a str,
        input: &'a Value,
        context_directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

impl ControlExecutor for ControlService {
    fn execute<'a>(
        &'a self,
        action: &'a str,
        input: &'a Value,
        context_directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<Value, ControlError>> {
        Box::pin(ControlService::execute(
            self,
            action,
            input,
            context_directory,
        ))
    }
}

#[derive(Clone)]
pub struct ModuleState {
    pub executor: Arc<dyn ControlExecutor>,
}

pub fn router_shared(executor: Arc<dyn ControlExecutor>) -> Router {
    Router::new()
        .route("/api/ompchamber/control", post(control_action))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(ModuleState { executor })
}

async fn control_action(State(state): State<ModuleState>, Json(body): Json<Value>) -> Response {
    let action = body.get("action").and_then(Value::as_str).unwrap_or("");
    let input = match body.get("input") {
        Some(value) if value.is_object() => value.clone(),
        _ => json!({}),
    };
    let context_directory = body.get("contextDirectory").and_then(Value::as_str);
    match state
        .executor
        .execute(action, &input, context_directory)
        .await
    {
        Ok(data) => (StatusCode::OK, Json(data)).into_response(),
        Err(control_error) => {
            let mut payload = json!({ "error": control_error.message });
            if control_error.partial {
                payload["partial"] = json!(true);
                if let Some(partial_action) = &control_error.partial_action {
                    payload["partialAction"] = json!(partial_action);
                }
                if let Some(session_id) = &control_error.session_id {
                    payload["sessionId"] = json!(session_id);
                }
                if let Some(directory) = &control_error.directory {
                    payload["directory"] = json!(directory);
                }
            }
            let status = StatusCode::from_u16(control_error.status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(payload)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::body::Body;
    use tower::ServiceExt;

    /// The JS tests' `execute` double: records the call, answers a canned
    /// result or error.
    struct FakeExecutor {
        result: Option<Result<Value, ControlError>>,
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
    }

    impl ControlExecutor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            action: &'a str,
            input: &'a Value,
            context_directory: Option<&'a str>,
        ) -> BoxFut<'a, Result<Value, ControlError>> {
            let call = (
                action.to_string(),
                input.clone(),
                context_directory.map(String::from),
            );
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(call);
            let result = self.result.clone();
            Box::pin(async move { result.unwrap_or_else(|| Ok(json!({ "projects": [] }))) })
        }
    }

    fn app(result: Option<Result<Value, ControlError>>) -> (Router, Arc<FakeExecutor>) {
        let executor = Arc::new(FakeExecutor {
            result,
            calls: Mutex::new(Vec::new()),
        });
        let router = router_shared(Arc::clone(&executor) as Arc<dyn ControlExecutor>);
        (router, executor)
    }

    async fn read_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    // routes.test.js — "is a thin adapter over the control service".
    #[tokio::test]
    async fn is_a_thin_adapter_over_the_control_service() {
        let (router, executor) = app(None);
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/ompchamber/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"action":"projects.list","input":{},"contextDirectory":"/repo"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_json(response).await, json!({ "projects": [] }));
        let calls = executor
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "projects.list");
        assert_eq!(calls[0].1, json!({}));
        assert_eq!(calls[0].2.as_deref(), Some("/repo"));
    }

    // routes.test.js — "preserves service status and partial-result
    // details".
    #[tokio::test]
    async fn preserves_service_status_and_partial_result_details() {
        let error = ControlError::partial(
            500,
            "dispatch failed",
            Some("fork-created".to_string()),
            Some("ses_fork".to_string()),
            Some("/repo".to_string()),
        );
        let (router, _) = app(Some(Err(error)));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/ompchamber/control")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":"session.fork","input":{}}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            read_json(response).await,
            json!({
                "error": "dispatch failed",
                "partial": true,
                "partialAction": "fork-created",
                "sessionId": "ses_fork",
                "directory": "/repo",
            })
        );
    }

    #[tokio::test]
    async fn non_object_input_becomes_empty_and_missing_action_becomes_empty() {
        let (router, executor) = app(Some(Err(ControlError::bad_request("nope"))));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/api/ompchamber/control")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"action":null,"input":[1,2]}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let calls = executor
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(calls[0].0, "");
        assert_eq!(calls[0].1, json!({}));
        assert_eq!(calls[0].2, None);
    }
}
