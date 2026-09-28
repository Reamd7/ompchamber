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
//! （中文说明）本文件是 `server/lib/openchamber-control/routes.js` 的
//! 移植：架在控制服务之上的带鉴权 CLI HTTP 适配层。`POST
//! /api/ompchamber/control`（JSON 体，上限 1 MB，对应
//! `express.json({ limit: '1mb' })`）：转发单个 action，失败时保留服务
//! 给出的 status 与部分结果详情，并传播请求取消。在 axum 中取消意味着
//! 客户端断开时 handler future 被 drop——进行中的等待停止、不写响应，
//! 这正是 JS 版断线中止接线的可观察契约（`res.writableEnded` 守卫本来
//! 也保证 499 错误体不会写进已关闭的 socket）。

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
/// 路由只转发单个 action；测试用替身实现该 trait（JS 测试注入
/// `{ controlService: { execute } }`）。
pub trait ControlExecutor: Send + Sync {
    /// 执行一个 action：`input` 为该 action 的参数对象，
    /// `context_directory` 提供调用时的目录上下文；失败以 `ControlError`
    /// 携带 status 与详情。
    fn execute<'a>(
        &'a self,
        action: &'a str,
        input: &'a Value,
        context_directory: Option<&'a str>,
    ) -> BoxFut<'a, Result<Value, ControlError>>;
}

/// 生产实现：直接委托给 `ControlService::execute`，仅做 trait 对象装箱。
impl ControlExecutor for ControlService {
    /// 转发到服务的 `execute`，装箱为共享 future。
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

/// 路由共享状态：持有任意的 action 执行器。
#[derive(Clone)]
pub struct ModuleState {
    /// 生产服务或测试替身。
    pub executor: Arc<dyn ControlExecutor>,
}

/// 注册 `POST /api/ompchamber/control` 并挂 1 MB 请求体上限，以给定
/// 执行器构建带状态的 Router。
pub fn router_shared(executor: Arc<dyn ControlExecutor>) -> Router {
    Router::new()
        .route("/api/ompchamber/control", post(control_action))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(ModuleState { executor })
}

/// 单入口 handler：从 JSON 体取 `action`（缺失按空串）、`input`（非对象
/// 折叠为空对象）与 `contextDirectory`，转发给执行器；成功回 200 + 数据，
/// 失败按 `ControlError` 的 status 回错误体，`partial` 时附带
/// `partialAction`/`sessionId`/`directory` 详情。
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

/// 薄适配层行为测试（对齐 routes.test.js）。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::body::Body;
    use tower::ServiceExt;

    /// The JS tests' `execute` double: records the call, answers a canned
    /// result or error.
    /// 对应 JS 测试的 `execute` 替身：记录每次调用，返回预设结果或错误。
    struct FakeExecutor {
    /// 预设返回值；`None` 时返回空项目列表。
        result: Option<Result<Value, ControlError>>,
    /// 记录 (action, input, contextDirectory) 三元组。
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
    }

    /// 记录调用后异步返回预设结果。
    impl ControlExecutor for FakeExecutor {
    /// 记录 (action, input, context_directory) 后异步返回预设结果。
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

    /// 用给定预设结果装配 (router, executor)，返回替身句柄供断言。
    fn app(result: Option<Result<Value, ControlError>>) -> (Router, Arc<FakeExecutor>) {
        let executor = Arc::new(FakeExecutor {
            result,
            calls: Mutex::new(Vec::new()),
        });
        let router = router_shared(Arc::clone(&executor) as Arc<dyn ControlExecutor>);
        (router, executor)
    }

    /// 读出响应体并解析为 JSON（测试断言用）。
    async fn read_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    // routes.test.js — "is a thin adapter over the control service".
    /// 验证：路由是服务的薄适配——action/input/contextDirectory 原样
    /// 转发，结果原样返回。
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
    /// 验证：失败时保留服务 status 与部分结果详情
    /// （partial/partialAction/sessionId/directory）。
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

    /// 验证：非对象 input 折叠为空对象、缺失 action 折叠为空串后再转发。
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
