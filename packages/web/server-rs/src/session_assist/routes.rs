//! Port of `server/lib/session-knowledge/routes.js`.
//!
//! Two calls rather than one, because only the sender knows whether the
//! message carrying the block actually went out. The body parser is
//! per-route in the JS (`express.json({limit: '1mb'})`); here bodies are
//! read raw and parsed leniently so a non-object body gets the JS handler's
//! `Body must be an object` response.

use std::sync::Arc;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;

use super::knowledge::SessionKnowledgeRuntime;

pub fn router(runtime: Arc<SessionKnowledgeRuntime>) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/session-knowledge",
            axum::routing::get(get_session_knowledge),
        )
        .route(
            "/api/session-knowledge/summary",
            axum::routing::get(get_session_knowledge_summary),
        )
        .route(
            "/api/session-knowledge/pin",
            axum::routing::post(post_session_knowledge_pin),
        )
        .route(
            "/api/session-knowledge/delivered",
            axum::routing::post(post_session_knowledge_delivered),
        )
        .with_state(runtime)
}

/// JS `asNonEmptyString`: trimmed non-empty string or ''.
fn as_non_empty_string(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Express simple query semantics for the two keys these routes read
/// (later duplicate wins).
fn query_value(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    let mut found: Option<String> = None;
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if name == key {
            found = Some(value.into_owned());
        }
    }
    found
}

fn error_json(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// Answers with the text to attach and the signature to report back once it
/// has gone. An empty text means the session is already carrying it.
async fn get_session_knowledge(
    State(runtime): State<Arc<SessionKnowledgeRuntime>>,
    RawQuery(query): RawQuery,
) -> axum::response::Response {
    let directory = as_non_empty_string(query_value(query.as_deref(), "directory").as_deref());
    let session_id = as_non_empty_string(query_value(query.as_deref(), "sessionId").as_deref());
    if directory.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "directory is required");
    }

    // Never fails the caller's send: a message without its background is far
    // better than no message at all.
    let pending = if !session_id.is_empty() {
        runtime
            .resolve_pending_for_session(&session_id, &directory)
            .await
    } else {
        // A session that does not exist yet — a draft about to be created —
        // has been told nothing, so everything is still owed.
        runtime
            .resolve_pending(&directory, "", &Default::default())
            .await
    };
    match pending {
        Ok(pending) => Json(pending).into_response(),
        Err(error) => Json(serde_json::json!({
            "text": "",
            "signature": "",
            "unavailable": true,
            "reason": error_message(&error),
        }))
        .into_response(),
    }
}

/// Counts and names for the work status panel; assembles no text.
async fn get_session_knowledge_summary(
    State(runtime): State<Arc<SessionKnowledgeRuntime>>,
    RawQuery(query): RawQuery,
) -> Json<Value> {
    let directory = as_non_empty_string(query_value(query.as_deref(), "directory").as_deref());
    if directory.is_empty() {
        return Json(serde_json::json!({
            "notes": [],
            "plans": [],
            "memory": { "global": 0, "project": 0 },
        }));
    }
    let session_id = as_non_empty_string(query_value(query.as_deref(), "sessionId").as_deref());
    if !session_id.is_empty() {
        Json(
            runtime
                .collect_summary_for_session(&session_id, &directory)
                .await,
        )
    } else {
        Json(
            runtime
                .collect_summary(&directory, &Default::default())
                .await,
        )
    }
}

/// Parses the body like the per-route `express.json` middleware: any
/// unparseable or non-object body is the handler's `Body must be an object`.
fn parse_object_body(bytes: &[u8]) -> Result<Value, axum::response::Response> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(error_json(
            StatusCode::BAD_REQUEST,
            "Body must be an object",
        )),
    }
}

async fn post_session_knowledge_pin(
    State(runtime): State<Arc<SessionKnowledgeRuntime>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let parsed = match parse_object_body(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let field = |key: &str| as_non_empty_string(parsed.get(key).and_then(Value::as_str));
    let session_id = field("sessionId");
    let directory = field("directory");
    let id = field("id");
    let kind = match parsed.get("kind").and_then(Value::as_str) {
        Some("note") | Some("plan") => parsed
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    let pinned = parsed.get("pinned").and_then(Value::as_bool);
    if session_id.is_empty()
        || directory.is_empty()
        || id.is_empty()
        || kind.is_empty()
        || pinned.is_none()
    {
        return error_json(
            StatusCode::BAD_REQUEST,
            "sessionId, directory, kind, id and pinned are required",
        );
    }
    match runtime
        .set_pin(&session_id, &directory, &kind, &id, pinned.unwrap_or(false))
        .await
    {
        Ok(pins) => Json(serde_json::json!({ "pins": pins })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn post_session_knowledge_delivered(
    State(runtime): State<Arc<SessionKnowledgeRuntime>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let parsed = match parse_object_body(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let field = |key: &str| as_non_empty_string(parsed.get(key).and_then(Value::as_str));
    let session_id = field("sessionId");
    let directory = field("directory");
    let signature = field("signature");
    if session_id.is_empty() || directory.is_empty() || signature.is_empty() {
        return error_json(
            StatusCode::BAD_REQUEST,
            "sessionId, directory and signature are required",
        );
    }
    match runtime
        .record_delivered(&session_id, &directory, &signature)
        .await
    {
        Ok(()) => Json(serde_json::json!({ "recorded": true })).into_response(),
        // The message is already sent; failing here only means the block may
        // be sent once more, which is far better than reporting failure.
        Err(error) => Json(serde_json::json!({
            "recorded": false,
            "reason": error.to_string(),
        }))
        .into_response(),
    }
}

fn error_message(error: &anyhow::Error) -> String {
    let message = format!("{error}");
    if message.is_empty() {
        "unknown".to_string()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    const DIRECTORY: &str = "/work/project";

    struct FakeEngine {
        requests: Mutex<Vec<(String, String, Option<Value>)>>,
        sessions: Mutex<HashMap<String, Value>>,
    }

    impl FakeEngine {
        fn with_session(session: Value) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                sessions: Mutex::new(HashMap::from([("ses_a".to_string(), session)])),
            })
        }

        fn runtime(self: &Arc<Self>) -> Arc<SessionKnowledgeRuntime> {
            let engine = Arc::clone(self);
            super::super::knowledge::SessionKnowledgeRuntime::new(
                super::super::knowledge::SessionKnowledgeOptions {
                    fetch: Arc::new(
                        move |path: &str,
                              _directory: Option<&str>,
                              method: &str,
                              body: Option<&Value>| {
                            let engine = Arc::clone(&engine);
                            let path = path.to_string();
                            let method = method.to_string();
                            let body = body.cloned();
                            Box::pin(async move {
                                engine
                                    .requests
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push((path.clone(), method.clone(), body.clone()));
                                let session_id = path
                                    .strip_prefix("/session/")
                                    .unwrap_or_default()
                                    .to_string();
                                if method == "PATCH" {
                                    if let Some(metadata) =
                                        body.as_ref().and_then(|b| b.get("metadata"))
                                    {
                                        let mut sessions = engine
                                            .sessions
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner());
                                        let session =
                                            sessions.entry(session_id).or_insert_with(|| json!({}));
                                        if let Some(object) = session.as_object_mut() {
                                            object.insert("metadata".into(), metadata.clone());
                                        }
                                    }
                                    return Ok(Value::Null);
                                }
                                Ok(engine
                                    .sessions
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .get(&session_id)
                                    .cloned()
                                    .unwrap_or(Value::Null))
                            })
                        },
                    ),
                    resolve_project_id: Arc::new(|_d: &str| Box::pin(async { Ok(String::new()) })),
                    read_context: Arc::new(|_p: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("project context unavailable")) })
                    }),
                    read_plan: Arc::new(|_p: &str, _id: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("project context unavailable")) })
                    }),
                    read_all_memory: Arc::new(|_p: Option<&str>| {
                        Box::pin(async { Err(anyhow::anyhow!("agent memory unavailable")) })
                    }),
                    is_memory_enabled: Arc::new(|| Box::pin(async { true })),
                },
            )
        }

        fn requests(&self) -> Vec<(String, String, Option<Value>)> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    async fn json_body(response: axum::response::Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        json_body(response).await
    }

    async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        json_body(response).await
    }

    #[tokio::test]
    async fn pending_requires_directory() {
        let engine = FakeEngine::with_session(json!({}));
        let app = router(engine.runtime());
        let (status, body) = get(&app, "/api/session-knowledge").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "directory is required" }));
    }

    #[tokio::test]
    async fn pending_answers_empty_text_when_nothing_is_owed() {
        let engine = FakeEngine::with_session(json!({ "metadata": { "ompchamber": {} } }));
        let app = router(engine.runtime());
        let (status, body) = get(
            &app,
            "/api/session-knowledge?directory=%2Fwork%2Fproject&sessionId=ses_a",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "text": "", "signature": "" }));
    }

    #[tokio::test]
    async fn pending_for_a_draft_session_skips_the_engine_read() {
        let engine = FakeEngine::with_session(json!({}));
        let app = router(engine.runtime());
        let (status, body) = get(&app, "/api/session-knowledge?directory=%2Fwork%2Fproject").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "text": "", "signature": "" }));
        assert!(
            engine.requests().is_empty(),
            "no sessionId means no session fetch"
        );
    }

    #[tokio::test]
    async fn pending_reports_unavailable_when_the_engine_read_fails() {
        // JS: a failed session READ is swallowed inside
        // `resolvePendingForSession` (`.catch(() => null)`); the route's
        // `unavailable` shape only fires when `resolvePending` itself
        // rejects — i.e. the project resolver throws.
        let runtime = super::super::knowledge::SessionKnowledgeRuntime::new(
            super::super::knowledge::SessionKnowledgeOptions {
                fetch: Arc::new(|_p, _d, _m, _b| Box::pin(async { Ok(Value::Null) })),
                resolve_project_id: Arc::new(|_d: &str| {
                    Box::pin(async { Err(anyhow::anyhow!("settings unreadable")) })
                }),
                read_context: Arc::new(|_p: &str| Box::pin(async { Err(anyhow::anyhow!("x")) })),
                read_plan: Arc::new(|_p: &str, _id: &str| {
                    Box::pin(async { Err(anyhow::anyhow!("x")) })
                }),
                read_all_memory: Arc::new(|_p: Option<&str>| {
                    Box::pin(async { Err(anyhow::anyhow!("x")) })
                }),
                is_memory_enabled: Arc::new(|| Box::pin(async { true })),
            },
        );
        let app = router(runtime);
        let (status, body) = get(
            &app,
            "/api/session-knowledge?directory=%2Fwork%2Fproject&sessionId=ses_missing",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "text": "",
                "signature": "",
                "unavailable": true,
                "reason": "settings unreadable",
            })
        );
    }

    #[tokio::test]
    async fn summary_defaults_to_empty_shapes() {
        let engine = FakeEngine::with_session(json!({}));
        let app = router(engine.runtime());
        let (status, body) = get(&app, "/api/session-knowledge/summary").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "notes": [], "plans": [], "memory": { "global": 0, "project": 0 } })
        );
    }

    #[tokio::test]
    async fn pin_validates_the_body_contract() {
        let engine = FakeEngine::with_session(json!({}));
        let app = router(engine.runtime());

        let (status, body) = post(&app, "/api/session-knowledge/pin", "[]").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "Body must be an object" }));

        let (status, body) = post(
            &app,
            "/api/session-knowledge/pin",
            r#"{ "sessionId": "ses_a" }"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({ "error": "sessionId, directory, kind, id and pinned are required" })
        );

        let (status, body) = post(
            &app,
            "/api/session-knowledge/pin",
            r#"{ "sessionId": "ses_a", "directory": "/w", "id": "n1", "kind": "snack", "pinned": true }"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({ "error": "sessionId, directory, kind, id and pinned are required" })
        );
    }

    #[tokio::test]
    async fn pin_round_trips_and_resets_the_delivered_signature() {
        let engine = FakeEngine::with_session(json!({
            "metadata": { "ompchamber": {
                "project_context_pins": { "notes": [], "plans": [] },
                "knowledge_context_delivered": "stale",
            } },
        }));
        let app = router(engine.runtime());
        let (status, body) = post(
            &app,
            "/api/session-knowledge/pin",
            &json!({
                "sessionId": "ses_a",
                "directory": DIRECTORY,
                "kind": "note",
                "id": "n1",
                "pinned": true,
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "pins": { "notes": ["n1"], "plans": [] } }));
        let patched = engine
            .requests()
            .iter()
            .find(|(path, method, _)| path == "/session/ses_a" && method == "PATCH")
            .and_then(|(_, _, body)| body.clone())
            .expect("patch");
        assert_eq!(
            patched["metadata"]["ompchamber"]["project_context_pins"],
            json!({ "notes": ["n1"], "plans": [] })
        );
        assert_eq!(
            patched["metadata"]["ompchamber"]["knowledge_context_delivered"],
            json!("")
        );
    }

    #[tokio::test]
    async fn pin_maps_engine_failure_to_a_500_message() {
        let engine = FakeEngine::with_session(json!({}));
        let runtime = {
            let failing = super::super::knowledge::SessionKnowledgeRuntime::new(
                super::super::knowledge::SessionKnowledgeOptions {
                    fetch: Arc::new(|_p, _d, _m, _b| {
                        Box::pin(async {
                            Err(super::super::fetch::OpenCodeError::Status {
                                method: "GET".to_string(),
                                path: "/session/ses_a".to_string(),
                                status: 503,
                            })
                        })
                    }),
                    resolve_project_id: Arc::new(|_d: &str| Box::pin(async { Ok(String::new()) })),
                    read_context: Arc::new(|_p: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    read_plan: Arc::new(|_p: &str, _id: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    read_all_memory: Arc::new(|_p: Option<&str>| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    is_memory_enabled: Arc::new(|| Box::pin(async { true })),
                },
            );
            let _ = engine;
            failing
        };
        let app = router(runtime);
        let (status, body) = post(
            &app,
            "/api/session-knowledge/pin",
            &json!({
                "sessionId": "ses_a",
                "directory": DIRECTORY,
                "kind": "note",
                "id": "n1",
                "pinned": true,
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            json!({ "error": "OpenCode GET /session/ses_a failed with 503" })
        );
    }

    #[tokio::test]
    async fn delivered_two_phase_contract() {
        let engine = FakeEngine::with_session(json!({
            "metadata": { "ompchamber": { "knowledge_context_delivered": "" } },
        }));
        let app = router(engine.runtime());

        let (status, body) = post(
            &app,
            "/api/session-knowledge/delivered",
            &json!({ "sessionId": "ses_a", "directory": DIRECTORY }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({ "error": "sessionId, directory and signature are required" })
        );

        let (status, body) = post(
            &app,
            "/api/session-knowledge/delivered",
            &json!({
                "sessionId": "ses_a",
                "directory": DIRECTORY,
                "signature": "n:n1:1",
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "recorded": true }));
        let patched = engine
            .requests()
            .iter()
            .find(|(path, method, _)| path == "/session/ses_a" && method == "PATCH")
            .and_then(|(_, _, body)| body.clone())
            .expect("patch");
        assert_eq!(
            patched["metadata"]["ompchamber"]["knowledge_context_delivered"],
            json!("n:n1:1")
        );
    }

    #[tokio::test]
    async fn delivered_reports_recorded_false_on_engine_failure() {
        let engine = FakeEngine::with_session(json!({}));
        let runtime = {
            let failing = super::super::knowledge::SessionKnowledgeRuntime::new(
                super::super::knowledge::SessionKnowledgeOptions {
                    fetch: Arc::new(|_p, _d, _m, _b| {
                        Box::pin(async { Err(super::super::fetch::OpenCodeError::Transport) })
                    }),
                    resolve_project_id: Arc::new(|_d: &str| Box::pin(async { Ok(String::new()) })),
                    read_context: Arc::new(|_p: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    read_plan: Arc::new(|_p: &str, _id: &str| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    read_all_memory: Arc::new(|_p: Option<&str>| {
                        Box::pin(async { Err(anyhow::anyhow!("x")) })
                    }),
                    is_memory_enabled: Arc::new(|| Box::pin(async { true })),
                },
            );
            let _ = engine;
            failing
        };
        let app = router(runtime);
        let (status, body) = post(
            &app,
            "/api/session-knowledge/delivered",
            &json!({
                "sessionId": "ses_a",
                "directory": DIRECTORY,
                "signature": "sig",
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "recorded": false, "reason": "fetch failed" }));
    }
}
