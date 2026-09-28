//! Port of `server/lib/scheduled-tasks/routes.js` — CRUD endpoints, manual
//! run, status, and the `/api/ompchamber/events` SSE stream.
//!
//! Route map (paths, verbs, JSON shapes mirror routes.js exactly):
//! - `GET    /api/projects/:projectId/scheduled-tasks` → `{ tasks }`
//! - `PUT    /api/projects/:projectId/scheduled-tasks` → `{ tasks, task, created }`
//! - `DELETE /api/projects/:projectId/scheduled-tasks/:taskId` → `{ tasks }`
//! - `PATCH  /api/projects/:projectId/scheduled-tasks/:taskId/loop-file` → `{ task }`
//! - `DELETE /api/projects/:projectId/scheduled-tasks/:taskId/loop-file` → `{ tasks }`
//! - `POST   /api/projects/:projectId/scheduled-tasks/:taskId/run` → `{ ok, task, sessionId, persistError? }`
//! - `GET    /api/ompchamber/scheduled-tasks/status`
//! - `GET    /api/ompchamber/events` (SSE: event-stream-ready + 25 s heartbeats)
//! 中文说明：scheduled-tasks 的 axum 路由与 `/api/ompchamber/events`
//! SSE 流。处理器只做参数校验与 JSON 形状组装，业务语义全部在
//! `ScheduledTaskService`；SSE 客户端表由路由与运行时共享，任务运行
//! 事件经 `SseClients::publish_task_run_event` 广播给所有连接。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::context::RouterContext;
use crate::scheduled_tasks::runtime::TaskRunEvent;
use crate::scheduled_tasks::service::{ScheduledTaskService, ServiceError};

/// Connected `/api/ompchamber/events` clients (`uiOMPChamberEventClients`).
/// Frames are `data: {...}\n\n` (`writeSseEvent`); dead senders are pruned on
/// the next publish (the JS equivalent removes clients on `res.on('close')`).
/// `/api/ompchamber/events` 的在线客户端表；失效发送端在下一次 publish
/// 时惰性清理。
pub struct SseClients {
/// 客户端 id → 帧发送通道；Mutex 保护，持锁时间仅为投递。
    inner: std::sync::Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>,
/// 自增的客户端 id 分配器。
    next_id: AtomicU64,
}

/// 注册、广播与清理。
impl SseClients {
/// 创建共享的客户端表。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

/// 注册一个客户端：分配 id 并返回配对的接收端（供 SSE 流消费）。
    pub(crate) fn register(&self) -> (u64, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        (id, rx)
    }

/// 测试用：主动移除客户端。
    #[cfg(test)]
    pub(crate) fn remove(&self, id: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// `writeSseEvent(res, {type, properties})` → `data: {...}\n\n` frames.
/// 把 payload 序列化为 `data: {...}\n\n` 帧并广播给全部客户端；发送
/// 失败（接收端已关闭）的条目随即移除。
    pub fn write_event(&self, payload: &Value) {
        let frame = format!(
            "data: {}\n\n",
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
        );
        let mut dead = Vec::new();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (id, tx) in guard.iter() {
            if tx.send(frame.clone()).is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            guard.remove(&id);
        }
    }

    /// `ompchamber:scheduled-task-ran` broadcast (emitTaskRunEvent wiring).
/// 组装 `ompchamber:scheduled-task-ran` 事件（含可选 sessionId）并广播。
    pub fn publish_task_run_event(&self, event: &TaskRunEvent) {
        let mut properties = json!({
            "projectId": event.project_id,
            "taskId": event.task_id,
            "ranAt": event.ran_at,
            "status": event.status,
        });
        if let Some(session_id) = &event.session_id {
            properties["sessionId"] = json!(session_id);
        }
        self.write_event(&json!({
            "type": "ompchamber:scheduled-task-ran",
            "properties": properties,
        }));
    }

/// 测试用：当前客户端数量。
    #[cfg(test)]
    pub(crate) fn client_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 各处理器共享的状态：任务服务与 SSE 客户端表。
#[derive(Clone)]
pub struct ModuleState {
/// 业务服务。
    pub service: Arc<ScheduledTaskService>,
/// SSE 客户端表。
    pub clients: Arc<SseClients>,
}

/// 独立组装入口：自建运行时（随引擎就绪即开始调度）并挂载全部路由。
pub fn router(ctx: RouterContext) -> Router {
    let clients = SseClients::new();
    let (_runtime, service) = crate::scheduled_tasks::build_runtime(&ctx, Arc::clone(&clients));
    router_shared(clients, service)
}

/// Composition entry for main.rs: the caller owns the runtime (and starts
/// the scheduler after the engine is up) and passes the SAME clients/service
/// the routes serve — index.js wires one stack, not two.
/// 挂载全部路由（清单见文件头）；服务错误经 `error_response` 统一转
/// JSON 响应。
pub fn router_shared(clients: Arc<SseClients>, service: Arc<ScheduledTaskService>) -> Router {
    Router::new()
        .route(
            "/api/projects/{projectId}/scheduled-tasks",
            get(list_tasks).put(upsert_task),
        )
        .route(
            "/api/projects/{projectId}/scheduled-tasks/{taskId}",
            axum::routing::delete(delete_task),
        )
        .route(
            "/api/projects/{projectId}/scheduled-tasks/{taskId}/loop-file",
            patch(set_loop_enabled).delete(delete_loop_file),
        )
        .route(
            "/api/projects/{projectId}/scheduled-tasks/{taskId}/run",
            post(run_task),
        )
        .route("/api/ompchamber/scheduled-tasks/status", get(status))
        .route("/api/ompchamber/events", get(events))
        .with_state(ModuleState { service, clients })
}

/// 把服务错误转为 (状态码, `{"error": message}`) 响应，可选附带 task。
fn error_response(error: ServiceError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = json!({ "error": error.message });
    if let Some(task) = error.task {
        body["task"] = task;
    }
    (status, Json(body)).into_response()
}

/// trim 后非空则返回 Some(修剪值)。
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 400 + `{"error": ...}` 的便捷响应。
fn bad_request(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

/// `GET /api/projects/:projectId/scheduled-tasks`.
/// 列出项目任务并返回 `{tasks}`；空白 projectId 为 400。
async fn list_tasks(State(state): State<ModuleState>, Path(project_id): Path<String>) -> Response {
    let Some(project_id) = non_empty(&project_id) else {
        return bad_request("projectId is required");
    };
    match state.service.list(&project_id).await {
        Ok(tasks) => (StatusCode::OK, Json(json!({ "tasks": tasks }))).into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /api/projects/:projectId/scheduled-tasks`.
/// 保存/更新任务：body.task 必须是对象，成功返回服务层的完整结果。
async fn upsert_task(
    State(state): State<ModuleState>,
    Path(project_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(project_id) = non_empty(&project_id) else {
        return bad_request("projectId is required");
    };
    let Some(task_input) = body.get("task").filter(|task| task.is_object()).cloned() else {
        return bad_request("task payload is required");
    };
    match state.service.upsert(&project_id, &task_input).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => error_response(error),
    }
}

/// `DELETE /api/projects/:projectId/scheduled-tasks/:taskId`.
/// 删除任务并返回删除后的 `{tasks}`。
async fn delete_task(
    State(state): State<ModuleState>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Response {
    let (Some(project_id), Some(task_id)) = (non_empty(&project_id), non_empty(&task_id)) else {
        return missing_params_response(&project_id, &task_id);
    };
    match state.service.remove(&project_id, &task_id).await {
        Ok(tasks) => (StatusCode::OK, Json(json!({ "tasks": tasks }))).into_response(),
        Err(error) => error_response(error),
    }
}

/// `PATCH /api/projects/:projectId/scheduled-tasks/:taskId/loop-file`.
/// 切换 loop 文件启停：body 中 `enabled` 必须是布尔，返回 `{task}`。
async fn set_loop_enabled(
    State(state): State<ModuleState>,
    Path((project_id, task_id)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Response {
    let (Some(project_id), Some(task_id)) = (non_empty(&project_id), non_empty(&task_id)) else {
        return missing_params_response(&project_id, &task_id);
    };
    let enabled = body
        .and_then(|Json(body)| body.get("enabled").cloned())
        .and_then(|value| value.as_bool());
    match state
        .service
        .set_loop_enabled(&project_id, &task_id, enabled)
        .await
    {
        Ok(task) => (StatusCode::OK, Json(json!({ "task": task }))).into_response(),
        Err(error) => error_response(error),
    }
}

/// `DELETE /api/projects/:projectId/scheduled-tasks/:taskId/loop-file`.
/// 删除 loop 文件并返回 `{tasks}`。
async fn delete_loop_file(
    State(state): State<ModuleState>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Response {
    let (Some(project_id), Some(task_id)) = (non_empty(&project_id), non_empty(&task_id)) else {
        return missing_params_response(&project_id, &task_id);
    };
    match state.service.remove_loop_file(&project_id, &task_id).await {
        Ok(tasks) => (StatusCode::OK, Json(json!({ "tasks": tasks }))).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /api/projects/:projectId/scheduled-tasks/:taskId/run`.
/// 手动运行任务，成功返回 `{ok, task, sessionId?, persistError?}`。
async fn run_task(
    State(state): State<ModuleState>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Response {
    let (Some(project_id), Some(task_id)) = (non_empty(&project_id), non_empty(&task_id)) else {
        return missing_params_response(&project_id, &task_id);
    };
    match state.service.run(&project_id, &task_id).await {
        Ok(result) => {
            let mut body = json!({ "ok": true, "task": result.task });
            if let Some(session_id) = result.session_id {
                body["sessionId"] = json!(session_id);
            }
            if let Some(persist_error) = result.persist_error {
                body["persistError"] = json!(persist_error);
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(error) => error_response(error),
    }
}

/// 组合校验失败时的 400：优先报 projectId，其次 taskId。
fn missing_params_response(project_id: &str, task_id: &str) -> Response {
    if non_empty(project_id).is_none() {
        return bad_request("projectId is required");
    }
    if non_empty(task_id).is_none() {
        return bad_request("taskId is required");
    }
    bad_request("projectId is required")
}

/// `GET /api/ompchamber/scheduled-tasks/status`.
/// 调度状态快照。
async fn status(State(state): State<ModuleState>) -> Response {
    (StatusCode::OK, Json(state.service.status())).into_response()
}

/// SSE 响应头：text/event-stream、禁缓存/代理缓冲、keep-alive。
fn sse_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers
}

/// 把 payload 编码为一帧 SSE 字节（ready 与 heartbeat 复用）。
fn frame_bytes(payload: &Value) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
    .into_bytes()
}

/// `GET /api/ompchamber/events` — SSE stream with an immediate
/// `ompchamber:event-stream-ready` frame and 25 s heartbeats.
/// 建立 SSE 流：立即发送 event-stream-ready 帧，随后合并客户端帧与
/// 25 秒心跳；断连时接收端被 drop，下一次 publish 清理发送端。
async fn events(
    State(state): State<ModuleState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // `?browser=1` marks a browser-capable client (res.ompchamberBrowserCapable).
    // The consumers of the flag land with the browser-control port, so it is
    // only recorded here (see PORT-MANIFEST.md).
    let _browser_capable = params.get("browser").map(String::as_str) == Some("1");

    let (client_id, rx) = state.clients.register();
    let ready = frame_bytes(&json!({
        "type": "ompchamber:event-stream-ready",
        "properties": { "connectedAt": crate::scheduled_tasks::runtime::system_clock()() },
    }));

    let client_frames = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        .map(|frame| Ok::<_, std::io::Error>(frame.into_bytes()));
    let initial = futures::stream::once(async { Ok::<_, std::io::Error>(ready) });
    let heartbeat = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
        Duration::from_secs(25),
    ))
    .map(|_| {
        Ok::<_, std::io::Error>(frame_bytes(&json!({
            "type": "ompchamber:heartbeat",
            "properties": { "timestamp": crate::scheduled_tasks::runtime::system_clock()() },
        })))
    });

    // The stream lives until the client disconnects; when it does, the
    // receiver drops and the next publish prunes this sender.
    let _ = client_id;
    let stream =
        futures::StreamExt::chain(initial, futures::stream::select(client_frames, heartbeat));

    let mut response = Response::new(Body::from_stream(stream));
    *response.headers_mut() = sse_headers();
    response
}

/// 路由层测试：JSON 形状、参数校验与 SSE 帧格式。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_tasks::dispatch::BoxFut;
    use crate::scheduled_tasks::runtime::{ProjectRef, ProjectsAccess};
    use crate::scheduled_tasks::runtime::{RuntimeDeps, ScheduledTasksRuntime};
    use crate::scheduled_tasks::testing::{MemoryStore, RecordingDispatch, daily_task};
    use axum::http::Request;
    use tower::ServiceExt;

/// 固定返回 p1 的项目列表测试替身。
    struct FixedProjects;

/// 恒定项目列表。
    impl ProjectsAccess for FixedProjects {
/// 返回固定的 p1 条目。
        fn list_projects(&self) -> BoxFut<'static, crate::error::AppResult<Vec<ProjectRef>>> {
            Box::pin(async {
                Ok(vec![ProjectRef {
                    id: "p1".into(),
                    path: "/repo".into(),
                }])
            })
        }
    }

/// 组装被测路由（内存存储 + 固定项目 + 录制派发器）。
    fn module_router() -> Router {
        let clients = SseClients::new();
        let store = Arc::new(MemoryStore::new(daily_task()));
        let deps = RuntimeDeps {
            store: Arc::clone(&store)
                as Arc<dyn crate::scheduled_tasks::runtime::ScheduledTaskStore>,
            projects: Arc::new(FixedProjects),
            dispatch: RecordingDispatch::new(),
            emit_task_run_event: Arc::new(|_| {}),
            clock: Arc::new(|| 1_750_000_000_000),
            max_global_concurrency: 4,
            max_project_concurrency: 2,
            max_run_duration_ms: 30 * 60 * 1000,
        };
        let runtime = ScheduledTasksRuntime::new(deps);
        let service =
            ScheduledTaskService::new(Arc::new(FixedProjects), store, Arc::clone(&runtime));
        let _ = &runtime;
        Router::new()
            .route(
                "/api/projects/{projectId}/scheduled-tasks",
                get(list_tasks).put(upsert_task),
            )
            .route(
                "/api/projects/{projectId}/scheduled-tasks/{taskId}",
                axum::routing::delete(delete_task),
            )
            .route(
                "/api/projects/{projectId}/scheduled-tasks/{taskId}/loop-file",
                patch(set_loop_enabled).delete(delete_loop_file),
            )
            .route(
                "/api/projects/{projectId}/scheduled-tasks/{taskId}/run",
                post(run_task),
            )
            .route("/api/ompchamber/scheduled-tasks/status", get(status))
            .with_state(ModuleState { service, clients })
    }

/// 读出并解析响应 body 为 JSON。
    async fn read_json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

/// 构造测试请求，有 body 时附带 JSON content-type。
    fn request(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(Body::from(body.unwrap_or_default().to_string()))
            .unwrap()
    }

/// GET 列表返回 `{tasks}` 形状。
    #[tokio::test]
    async fn list_route_returns_tasks_shape() {
        let app = module_router();
        let response = app
            .oneshot(request("GET", "/api/projects/p1/scheduled-tasks", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert!(json["tasks"].is_array());
        assert_eq!(json["tasks"][0]["id"], "task-1");
        assert_eq!(json["tasks"][0]["name"], "Daily Sync");
    }

/// PUT 缺少 task 对象时返回 400。
    #[tokio::test]
    async fn put_requires_task_payload() {
        let app = module_router();
        let response = app
            .oneshot(request(
                "PUT",
                "/api/projects/p1/scheduled-tasks",
                Some("{}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            read_json(response).await["error"],
            "task payload is required"
        );
    }

/// 空白 projectId 路径参数被拒绝为 400。
    #[tokio::test]
    async fn blank_project_param_is_rejected() {
        let app = module_router();
        // Whitespace-only id trims to empty → 400 (JS asNonEmptyString).
        let response = app
            .oneshot(request("GET", "/api/projects/%20/scheduled-tasks", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(read_json(response).await["error"], "projectId is required");
    }

/// 未知项目返回 404 Project not found。
    #[tokio::test]
    async fn unknown_project_is_404() {
        let app = module_router();
        let response = app
            .oneshot(request("GET", "/api/projects/ghost/scheduled-tasks", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(read_json(response).await["error"], "Project not found");
    }

/// 删除不存在的任务返回 404。
    #[tokio::test]
    async fn delete_missing_task_is_404() {
        let app = module_router();
        let response = app
            .oneshot(request(
                "DELETE",
                "/api/projects/p1/scheduled-tasks/ghost",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(read_json(response).await["error"], "Task not found");
    }

/// PATCH loop-file 的 enabled 非布尔时返回 400。
    #[tokio::test]
    async fn loop_file_patch_requires_boolean_enabled() {
        let app = module_router();
        let response = app
            .oneshot(request(
                "PATCH",
                "/api/projects/p1/scheduled-tasks/task-1/loop-file",
                Some("{\"enabled\": \"yes\"}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            read_json(response).await["error"],
            "enabled must be a boolean"
        );
    }

/// status 返回启用/运行计数字段。
    #[tokio::test]
    async fn status_route_reports_counts() {
        let app = module_router();
        // Sync the project first so the runtime's in-memory map is primed
        // (service.status reads the runtime snapshot, like JS getStatus).
        let _ = app
            .clone()
            .oneshot(request("GET", "/api/projects/p1/scheduled-tasks", None))
            .await
            .unwrap();
        let response = app
            .oneshot(request(
                "GET",
                "/api/ompchamber/scheduled-tasks/status",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = read_json(response).await;
        assert_eq!(json["hasEnabledScheduledTasks"], true);
        assert_eq!(json["enabledScheduledTasksCount"], 1);
        assert_eq!(json["hasRunningScheduledTasks"], false);
    }

/// run 不存在的任务返回 404 Task not found or disabled。
    #[tokio::test]
    async fn run_route_reports_task_not_found_for_missing_task() {
        let app = module_router();
        let response = app
            .oneshot(request(
                "POST",
                "/api/projects/p1/scheduled-tasks/ghost/run",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            read_json(response).await["error"],
            "Task not found or disabled"
        );
    }

/// 广播帧严格符合 `data: {...}\n\n` 格式。
    #[tokio::test]
    async fn sse_frame_format_matches_write_sse_event() {
        let clients = SseClients::new();
        let (_id, mut rx) = clients.register();
        assert_eq!(clients.client_count(), 1);
        clients.write_event(&json!({"type": "x", "properties": {"a": 1}}));
        let frame = rx.recv().await.expect("frame delivered");
        assert!(frame.starts_with("data: {"));
        assert!(frame.ends_with("\n\n"));
        let payload: Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(payload["type"], "x");
    }

/// 任务运行事件帧的字段形状与 index.js 一致。
    #[tokio::test]
    async fn task_run_event_shape_matches_index_js() {
        let clients = SseClients::new();
        let event = TaskRunEvent {
            project_id: "p1".into(),
            task_id: "task-1".into(),
            ran_at: 123,
            status: "success".into(),
            session_id: Some("sess-9".into()),
        };
        clients.publish_task_run_event(&event);
        let frame = {
            let (_id, mut rx) = clients.register();
            clients.publish_task_run_event(&event);
            rx.recv().await.expect("frame")
        };
        let payload: Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(payload["type"], "ompchamber:scheduled-task-ran");
        assert_eq!(payload["properties"]["projectId"], "p1");
        assert_eq!(payload["properties"]["status"], "success");
        assert_eq!(payload["properties"]["sessionId"], "sess-9");
    }

/// 掉线客户端在主动移除与惰式 publish 两条路径上都被清理。
    #[tokio::test]
    async fn dead_clients_are_pruned_on_publish() {
        let clients = SseClients::new();
        let (id, _rx) = clients.register();
        assert_eq!(clients.client_count(), 1);
        clients.remove(id);
        assert_eq!(clients.client_count(), 0);
        // A dropped receiver also prunes lazily on the next publish.
        let (_id2, rx2) = clients.register();
        drop(rx2);
        clients.write_event(&json!({"type": "x"}));
        assert_eq!(clients.client_count(), 0);
    }
}
