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
pub struct SseClients {
    inner: std::sync::Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>,
    next_id: AtomicU64,
}

impl SseClients {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub(crate) fn register(&self) -> (u64, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        (id, rx)
    }

    #[cfg(test)]
    pub(crate) fn remove(&self, id: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// `writeSseEvent(res, {type, properties})` → `data: {...}\n\n` frames.
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

    #[cfg(test)]
    pub(crate) fn client_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[derive(Clone)]
pub struct ModuleState {
    pub service: Arc<ScheduledTaskService>,
    pub clients: Arc<SseClients>,
}

pub fn router(ctx: RouterContext) -> Router {
    let clients = SseClients::new();
    let (_runtime, service) = crate::scheduled_tasks::build_runtime(&ctx, Arc::clone(&clients));
    router_shared(clients, service)
}

/// Composition entry for main.rs: the caller owns the runtime (and starts
/// the scheduler after the engine is up) and passes the SAME clients/service
/// the routes serve — index.js wires one stack, not two.
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

fn error_response(error: ServiceError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = json!({ "error": error.message });
    if let Some(task) = error.task {
        body["task"] = task;
    }
    (status, Json(body)).into_response()
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn bad_request(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

/// `GET /api/projects/:projectId/scheduled-tasks`.
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
async fn status(State(state): State<ModuleState>) -> Response {
    (StatusCode::OK, Json(state.service.status())).into_response()
}

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

fn frame_bytes(payload: &Value) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
    .into_bytes()
}

/// `GET /api/ompchamber/events` — SSE stream with an immediate
/// `ompchamber:event-stream-ready` frame and 25 s heartbeats.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_tasks::dispatch::BoxFut;
    use crate::scheduled_tasks::runtime::{ProjectRef, ProjectsAccess};
    use crate::scheduled_tasks::runtime::{RuntimeDeps, ScheduledTasksRuntime};
    use crate::scheduled_tasks::testing::{MemoryStore, RecordingDispatch, daily_task};
    use axum::http::Request;
    use tower::ServiceExt;

    struct FixedProjects;

    impl ProjectsAccess for FixedProjects {
        fn list_projects(&self) -> BoxFut<'static, crate::error::AppResult<Vec<ProjectRef>>> {
            Box::pin(async {
                Ok(vec![ProjectRef {
                    id: "p1".into(),
                    path: "/repo".into(),
                }])
            })
        }
    }

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

    async fn read_json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn request(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(Body::from(body.unwrap_or_default().to_string()))
            .unwrap()
    }

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
