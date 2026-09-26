//! Port of `server/lib/agent-memory/routes.js`.
//!
//! The scope is a query parameter rather than part of the path, because
//! global and project memory are the same resource with two homes. Getting
//! the scope wrong must fail loudly, never silently write the user's global
//! memory from a project-scoped call. Memory is created by the agent through
//! the `ompchamber_memory` tool, so there is no create route here; the panel
//! reads, corrects, and deletes.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde::Serialize;
use serde_json::{Value, json};

use super::runtime::{AgentMemoryRuntime, MemoryEnabledGate, MemoryError, Target, UpdatePatch};

/// `express.json({ limit: '1mb' })` on the one route that carries a body.
const JSON_BODY_LIMIT_BYTES: usize = 1024 * 1024;

pub struct RoutesState {
    pub runtime: Arc<AgentMemoryRuntime>,
    /// JS passes `isAgentMemoryEnabled` optionally; omitting it runs the
    /// surface ungated (how the JS route tests mount it).
    pub is_enabled: Option<MemoryEnabledGate>,
}

pub fn routes(runtime: Arc<AgentMemoryRuntime>, is_enabled: Option<MemoryEnabledGate>) -> Router {
    Router::new()
        .route("/api/agent-memory", get(get_memory))
        .route("/api/agent-memory/all", get(get_all_memory))
        .route(
            "/api/agent-memory/{memoryId}",
            patch(patch_memory).delete(delete_memory),
        )
        .with_state(Arc::new(RoutesState {
            runtime,
            is_enabled,
        }))
}

/// One gate for the whole surface. The settings toggle disables the feature,
/// not just its UI: with memory off, these routes must not read or write the
/// store at all, or a stale client would keep editing memory the user
/// believes is turned off.
async fn require_enabled(state: &RoutesState) -> Result<(), Response> {
    let Some(gate) = &state.is_enabled else {
        return Ok(());
    };
    match gate().await {
        // Flagged, not merely 404: a missing entry answers 404 too, and a
        // client that could not tell them apart would report a deleted
        // memory as the whole feature being switched off.
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Agent memory is disabled", "disabled": true })),
        )
            .into_response()),
        // An unreadable settings file must not silently expose a surface the
        // user may have turned off.
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Agent memory availability is unknown" })),
        )
            .into_response()),
        Ok(true) => Ok(()),
    }
}

/// The JS handlers classify caught errors by message substring.
fn is_validation_error(message: &str) -> bool {
    message.contains("is required")
        || message.contains("unsupported characters")
        || message.contains("holds at most")
}

fn respond_with_error(error: &MemoryError, fallback_message: &str) -> Response {
    let message = match error {
        MemoryError::Message(message) => message.clone(),
        MemoryError::Io(err) => err.to_string(),
    };
    if is_validation_error(&message) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    }
    let message = if message.is_empty() {
        fallback_message.to_string()
    } else {
        message
    };
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// Parsed query pairs. [`query_string`] mirrors JS `typeof query.key ===
/// 'string'`: absent or repeated keys (`?k=a&k=b` parses as an array in
/// express) both read as "not a string".
fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    raw.map(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    })
    .unwrap_or_default()
}

fn query_string(pairs: &[(String, String)], key: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for (candidate, value) in pairs {
        if candidate == key {
            if found.is_some() {
                return None;
            }
            found = Some(value.clone());
        }
    }
    found
}

/// Resolves the target scope, or the reason it could not be resolved. A
/// project request without an id is rejected here rather than quietly
/// falling back to global, which would write project facts into every other
/// project.
fn resolve_scope(pairs: &[(String, String)]) -> Result<Target, &'static str> {
    match query_string(pairs, "scope").as_deref() {
        Some("global") => Ok(Target::Global),
        Some("project") => {
            match query_string(pairs, "projectId").filter(|id| !id.trim().is_empty()) {
                Some(project_id) => Ok(Target::Project { project_id }),
                None => Err("projectId is required for project scope"),
            }
        }
        _ => Err("scope must be global or project"),
    }
}

/// JS relies on express's `express.json()`: only `*json` content types
/// populate `req.body`; anything else leaves it undefined, which this route
/// rejects as a malformed body.
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

#[derive(Serialize)]
struct UpdateResponse<'a> {
    entry: &'a super::runtime::MemoryEntry,
    entries: &'a [super::runtime::MemoryEntry],
}

async fn get_memory(State(state): State<Arc<RoutesState>>, RawQuery(query): RawQuery) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };
    match state.runtime.read(&target).await {
        Ok(file) => Json(file).into_response(),
        Err(error) => respond_with_error(&error, "Failed to read agent memory"),
    }
}

/// Both scopes in one response: the panel always shows them together, and
/// two separate requests would let one scope render while the other is still
/// loading, which reads as memory that has gone missing.
async fn get_all_memory(
    State(state): State<Arc<RoutesState>>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let project_id = query_string(&pairs, "projectId").filter(|id| !id.trim().is_empty());
    let all = state.runtime.read_all(project_id.as_deref()).await;
    Json(all).into_response()
}

async fn patch_memory(
    State(state): State<Arc<RoutesState>>,
    Path(memory_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };

    // express.json's limit rejects larger payloads before the handler runs.
    if body.len() > JSON_BODY_LIMIT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request entity too large" })),
        )
            .into_response();
    }

    let parsed = parse_json_body(&headers, &body);
    let Some(record) = parsed.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Body must be an object" })),
        )
            .into_response();
    };
    if record.contains_key("title") && !record["title"].is_string() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "title must be a string" })),
        )
            .into_response();
    }
    if record.contains_key("body") && !record["body"].is_string() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "body must be a string" })),
        )
            .into_response();
    }
    if record.contains_key("type")
        && !record
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|entry_type| super::runtime::MEMORY_TYPES.contains(&entry_type))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "type must be fact, preference, or reference" })),
        )
            .into_response();
    }

    let patch = UpdatePatch {
        title: record
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        body: record
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_string),
        entry_type: record
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    match state.runtime.update(&target, &memory_id, &patch).await {
        Ok(Some(result)) => Json(UpdateResponse {
            entry: &result.entry,
            entries: &result.entries,
        })
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Memory not found" })),
        )
            .into_response(),
        Err(error) => respond_with_error(&error, "Failed to save memory"),
    }
}

async fn delete_memory(
    State(state): State<Arc<RoutesState>>,
    Path(memory_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };

    match state.runtime.remove(&target, &memory_id).await {
        Ok(result) if result.deleted => {
            #[derive(Serialize)]
            struct DeleteResponse<'a> {
                deleted: bool,
                entries: &'a [super::runtime::MemoryEntry],
            }
            Json(DeleteResponse {
                deleted: true,
                entries: &result.entries,
            })
            .into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Memory not found" })),
        )
            .into_response(),
        Err(error) => respond_with_error(&error, "Failed to delete memory"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_memory::runtime::{CreateInput, MemoryError as StoreError};
    use axum::body::Body;
    use tower::ServiceExt;

    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            static SEQUENCE: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "oc-agent-memory-routes-{tag}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root).expect("fixture root");
            Fixture { root }
        }

        fn runtime(&self) -> Arc<AgentMemoryRuntime> {
            Arc::new(AgentMemoryRuntime::new(
                self.root.join("config"),
                self.root.join("config").join("projects"),
                None,
            ))
        }

        fn global_path(&self) -> std::path::PathBuf {
            self.root.join("config").join("memory.json")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn request(method: &str, uri: &str, body: Option<&str>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(Body::from(body.unwrap_or_default().to_string()))
            .expect("request")
    }

    fn gate(returning: Result<bool, &'static str>) -> MemoryEnabledGate {
        Arc::new(move || {
            let outcome = returning;
            Box::pin(async move { outcome.map_err(StoreError::message) })
        })
    }

    #[tokio::test]
    async fn reads_global_scope() {
        let fixture = Fixture::new("get-global");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("Uses bun".into()),
                    body: Some("Tests run with bun test.".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["version"], json!(1));
        assert_eq!(body["entries"][0]["title"], json!("Uses bun"));
        assert_eq!(body["entries"][0]["type"], json!("fact"));
    }

    #[tokio::test]
    async fn reads_project_scope_with_its_id() {
        let fixture = Fixture::new("get-project");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Project {
                    project_id: "path_abc".into(),
                },
                &CreateInput {
                    title: Some("P".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory?scope=project&projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["entries"][0]["title"], json!("P"));
        assert!(
            fixture
                .root
                .join("config")
                .join("projects")
                .join("path_abc")
                .join("memory.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn refuses_a_project_scope_with_no_id_rather_than_falling_back_to_global() {
        let fixture = Fixture::new("no-project-id");
        let runtime = fixture.runtime();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=project", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("projectId is required")
        );
        assert!(!fixture.global_path().exists(), "never touched the store");
    }

    #[tokio::test]
    async fn refuses_a_missing_scope() {
        let fixture = Fixture::new("no-scope");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("scope must be")
        );
    }

    #[tokio::test]
    async fn refuses_a_delete_with_no_scope_before_touching_the_store() {
        let fixture = Fixture::new("delete-no-scope");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), None);

        let response = app
            .oneshot(request("DELETE", "/api/agent-memory/mem-x", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .len(),
            1,
            "store untouched"
        );
    }

    #[tokio::test]
    async fn returns_global_and_project_together() {
        let fixture = Fixture::new("all");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("G".into()),
                    body: Some("x".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("global");
        runtime
            .create(
                &Target::Project {
                    project_id: "path_abc".into(),
                },
                &CreateInput {
                    title: Some("P".into()),
                    body: Some("y".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("project");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory/all?projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["global"].as_array().expect("global").len(), 1);
        assert_eq!(body["project"].as_array().expect("project").len(), 1);
        assert_eq!(body["globalFailed"], json!(false));
        assert_eq!(body["projectFailed"], json!(false));
    }

    #[tokio::test]
    async fn reads_global_alone_when_no_project_is_open() {
        let fixture = Fixture::new("all-no-project");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory/all", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert!(body["project"].as_array().expect("project").is_empty());
    }

    #[tokio::test]
    async fn a_broken_project_scope_is_reported_not_rendered_as_empty() {
        let fixture = Fixture::new("all-broken");
        let runtime = fixture.runtime();
        let project_path = fixture
            .root
            .join("config")
            .join("projects")
            .join("path_abc")
            .join("memory.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(&project_path, "{ broken").unwrap();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory/all?projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["projectFailed"], json!(true));
        assert!(body["project"].as_array().expect("project").is_empty());
    }

    #[tokio::test]
    async fn reports_malformed_storage_as_a_server_error() {
        let fixture = Fixture::new("malformed");
        let runtime = fixture.runtime();
        std::fs::create_dir_all(fixture.global_path().parent().unwrap()).unwrap();
        std::fs::write(fixture.global_path(), "{ not json").unwrap();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("Stored agent memory is malformed"));
    }

    #[tokio::test]
    async fn reports_a_bad_project_id_as_a_client_error() {
        let fixture = Fixture::new("bad-id");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "GET",
                // A bare ".." passes the id pattern (dots are allowed), same
                // as JS; a traversal id with a separator is what fails.
                "/api/agent-memory?scope=project&projectId=..%2Fescape",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(
            body["error"],
            json!("projectId contains unsupported characters")
        );
    }

    #[tokio::test]
    async fn patches_a_memory_from_a_json_body() {
        let fixture = Fixture::new("patch");
        let runtime = fixture.runtime();
        let created = runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("Unclear".into()),
                    body: Some("Original.".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "PATCH",
                &format!("/api/agent-memory/{}?scope=global", created.entry.id),
                Some(r#"{"title": "Clearer", "body": "Reworded."}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["entry"]["title"], json!("Clearer"));
        assert_eq!(body["entry"]["body"], json!("Reworded."));
        assert_eq!(body["entries"].as_array().expect("entries").len(), 1);
    }

    #[tokio::test]
    async fn rejects_a_non_string_title() {
        let fixture = Fixture::new("patch-bad-title");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some(r#"{"title": 42}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("title must be a string"));
    }

    #[tokio::test]
    async fn rejects_a_non_object_body_and_a_missing_content_type() {
        let fixture = Fixture::new("patch-bad-body");
        let app = routes(fixture.runtime(), None);

        let array_body = app
            .clone()
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some(r#"[1, 2]"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(array_body.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(array_body).await["error"],
            json!("Body must be an object")
        );

        // No JSON content type: express leaves req.body undefined.
        let no_content_type = axum::http::Request::patch("/api/agent-memory/mem-1?scope=global")
            .body(Body::from(r#"{"title": "x"}"#))
            .unwrap();
        let response = app.oneshot(no_content_type).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_empty_patch_is_a_client_error() {
        let fixture = Fixture::new("patch-empty");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some("{}"),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["error"],
            json!("title, body or type is required")
        );
    }

    #[tokio::test]
    async fn reports_a_missing_memory_as_404() {
        let fixture = Fixture::new("patch-miss");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/nope?scope=global",
                Some(r#"{"body": "x"}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await["error"],
            json!("Memory not found")
        );
    }

    #[tokio::test]
    async fn deletes_the_named_memory_in_the_named_scope() {
        let fixture = Fixture::new("delete");
        let runtime = fixture.runtime();
        let created = runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), None);

        let response = app
            .oneshot(request(
                "DELETE",
                &format!("/api/agent-memory/{}?scope=global", created.entry.id),
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["deleted"], json!(true));
        assert!(body["entries"].as_array().expect("entries").is_empty());
        assert!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reports_a_missing_memory_as_404_on_delete() {
        let fixture = Fixture::new("delete-miss");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "DELETE",
                "/api/agent-memory/nope?scope=global",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("Memory not found"));
        assert!(body.get("disabled").is_none());
    }

    #[tokio::test]
    async fn the_disabled_answer_is_flagged_so_a_deleted_entry_cannot_be_mistaken_for_it() {
        let fixture = Fixture::new("gate-off");
        let runtime = fixture.runtime();
        let off = routes(runtime.clone(), Some(gate(Ok(false))));

        let disabled = off
            .clone()
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");
        assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
        let body = body_json(disabled).await;
        assert_eq!(body["disabled"], json!(true));
        assert_eq!(body["error"], json!("Agent memory is disabled"));
        assert!(
            !fixture.global_path().exists(),
            "the store is never read while memory is off"
        );
    }

    #[tokio::test]
    async fn refuses_deletes_from_a_stale_client_while_memory_is_off() {
        let fixture = Fixture::new("gate-off-delete");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), Some(gate(Ok(false))));

        let response = app
            .oneshot(request(
                "DELETE",
                "/api/agent-memory/mem-1?scope=global",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .len(),
            1,
            "nothing deleted while memory is off"
        );
    }

    #[tokio::test]
    async fn serves_normally_while_memory_is_on() {
        let fixture = Fixture::new("gate-on");
        let app = routes(fixture.runtime(), Some(gate(Ok(true))));

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn closes_the_surface_when_the_setting_cannot_be_read() {
        let fixture = Fixture::new("gate-error");
        let app = routes(fixture.runtime(), Some(gate(Err("settings unreadable"))));

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await["error"],
            json!("Agent memory availability is unknown")
        );
    }

    #[tokio::test]
    async fn a_repeated_query_key_reads_as_an_array_never_as_its_last_value() {
        let fixture = Fixture::new("repeated-keys");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory?scope=project&projectId=a&projectId=b",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            body_json(response).await["error"]
                .as_str()
                .expect("error")
                .contains("projectId is required")
        );
    }
}
