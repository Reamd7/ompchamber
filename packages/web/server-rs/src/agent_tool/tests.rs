//! Tests for the agent-tool port, mirroring `runtime.test.js`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use super::plugin;
use super::*;
use crate::openchamber_control::actions as control_actions;

type RecordedCall = (String, Value, Option<Value>);

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321);

struct Fixture {
    dir: std::path::PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "oc-agent-tool-{tag}-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }

    fn runtime(&self, executor: Option<ExecuteActionFn>) -> AgentToolRuntime {
        self.runtime_with(executor, Some(3901), None)
    }

    fn runtime_with(
        &self,
        executor: Option<ExecuteActionFn>,
        port: Option<u16>,
        config_content: Option<String>,
    ) -> AgentToolRuntime {
        let config = Arc::new(Mutex::new(config_content));
        AgentToolRuntime::new(
            self.dir.clone(),
            Arc::new(move || port),
            executor,
            Arc::new(move || config.lock().unwrap_or_else(|e| e.into_inner()).clone()),
        )
    }

    fn app(&self, runtime: AgentToolRuntime) -> Router {
        router_with_runtime(Arc::new(runtime))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn recording_executor(
    responses: Vec<Result<Value, ActionExecutionError>>,
) -> (Arc<Mutex<Vec<RecordedCall>>>, ExecuteActionFn) {
    let calls: Arc<Mutex<Vec<RecordedCall>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let responses: Arc<Mutex<Vec<Result<Value, ActionExecutionError>>>> =
        Arc::new(Mutex::new(responses));
    let executor: ExecuteActionFn = Arc::new(move |action, input, context_directory, _signal| {
        recorded.lock().unwrap_or_else(|e| e.into_inner()).push((
            action.to_string(),
            input.clone(),
            context_directory.clone(),
        ));
        let response = {
            let mut queue = responses.lock().unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() {
                Ok(json!({}))
            } else {
                queue.remove(0)
            }
        };
        Box::pin(async move { response })
    });
    (calls, executor)
}

fn payload(input: Value) -> AgentToolPayload {
    AgentToolPayload {
        input: Some(input),
        context_directory: Some(Value::String("/work/project".to_string())),
        tool: None,
    }
}

fn quiet_signal() -> AbortSignal {
    AbortHandle::new().signal()
}

fn request(
    token: Option<&str>,
    addr: Option<SocketAddr>,
    content_type: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/ompchamber/agent-tool");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(addr) = addr {
        builder = builder.extension(ConnectInfo(addr));
    }
    builder = builder.header(
        header::CONTENT_TYPE,
        content_type.unwrap_or("application/json"),
    );
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Extracts the `enum: [...]` JSON for one generated tool entry.
fn tool_enum(source: &str, name: &str) -> String {
    let entry = source
        .find(&format!("    {name}: {{"))
        .unwrap_or_else(|| panic!("tool {name} missing"));
    let rest = &source[entry..];
    let start = rest.find("enum: [").unwrap() + "enum: [".len();
    let end = start + rest[start..].find(']').unwrap();
    rest[start..end].to_string()
}

/// Extracts the `properties: {...}` JSON for one generated tool entry.
fn tool_parameters(source: &str, name: &str) -> String {
    let entry = source
        .find(&format!("    {name}: {{"))
        .unwrap_or_else(|| panic!("tool {name} missing"));
    let rest = &source[entry..];
    let start = rest.find("properties: {").unwrap() + "properties: {".len();
    let end = start + rest[start..].find(", additionalProperties").unwrap();
    rest[start..end].to_string()
}

#[test]
fn timing_safe_equal_compares_bytes() {
    assert!(timing_safe_equal(b"token", b"token"));
    assert!(!timing_safe_equal(b"token", b"tokeN"));
    assert!(!timing_safe_equal(b"token", b"toke"));
    assert!(!timing_safe_equal(b"", b"a"));
    assert!(timing_safe_equal(b"", b""));
}

#[test]
fn loopback_gate_accepts_only_loopback_spellings() {
    for accepted in ["127.0.0.1", "::1", "::ffff:127.0.0.1", "::FFFF:127.0.0.1"] {
        assert!(is_loopback_address(Some(accepted)), "{accepted}");
    }
    for rejected in [
        "192.168.1.5",
        "::ffff:192.168.1.5",
        "10.0.0.1",
        "localhost",
        "",
    ] {
        assert!(!is_loopback_address(Some(rejected)), "{rejected}");
    }
    assert!(!is_loopback_address(None));
}

#[test]
fn authorize_requires_token_loopback_and_bearer() {
    let token = "abcdefghijklmnopqrstuvwxyz012345";
    assert!(authorize(
        Some(token),
        Some("127.0.0.1"),
        Some("Bearer abcdefghijklmnopqrstuvwxyz012345")
    ));
    // Leading whitespace is trimmed before the Bearer check (asNonEmptyString).
    assert!(authorize(
        Some(token),
        Some("::1"),
        Some(" Bearer abcdefghijklmnopqrstuvwxyz012345 ")
    ));
    assert!(authorize(
        Some(token),
        Some("::ffff:127.0.0.1"),
        Some("Bearer abcdefghijklmnopqrstuvwxyz012345")
    ));
    // No active token.
    assert!(!authorize(None, Some("127.0.0.1"), Some("Bearer x")));
    // Not loopback.
    assert!(!authorize(
        Some(token),
        Some("192.168.1.5"),
        Some("Bearer abcdefghijklmnopqrstuvwxyz012345")
    ));
    assert!(!authorize(None, None, None));
    // Header shapes.
    assert!(!authorize(Some(token), Some("127.0.0.1"), None));
    assert!(!authorize(
        Some(token),
        Some("127.0.0.1"),
        Some("bearer abcdefghijklmnopqrstuvwxyz012345")
    ));
    assert!(!authorize(
        Some(token),
        Some("127.0.0.1"),
        Some("Basic abcdefghijklmnopqrstuvwxyz012345")
    ));
    assert!(!authorize(Some(token), Some("127.0.0.1"), Some("Bearer ")));
    // Wrong token of the same length is rejected without revealing which byte.
    assert!(!authorize(
        Some(token),
        Some("127.0.0.1"),
        Some("Bearer abcdefghijklmnopqrstuvwxyz012346")
    ));
    assert!(!authorize(
        Some(token),
        Some("127.0.0.1"),
        Some("Bearer short")
    ));
}

#[test]
fn generated_tokens_are_url_safe_and_unique() {
    let first = generate_token();
    let second = generate_token();
    assert_eq!(first.len(), 43);
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_ne!(first, second);
}

#[test]
fn abort_handle_fires_only_for_unfinished_requests() {
    let handle = AbortHandle::new();
    let signal = handle.signal();
    assert!(!signal.is_aborted());
    drop(handle);
    assert!(signal.is_aborted());

    let mut handle = AbortHandle::new();
    let signal = handle.signal();
    handle.finish();
    drop(handle);
    assert!(!signal.is_aborted());

    let handle = AbortHandle::new();
    let signal = handle.signal();
    handle.abort();
    assert!(signal.is_aborted());
}

#[test]
fn dispatchable_actions_stay_narrower_than_the_control_surface() {
    let actions = dispatchable_actions();
    for expected in [
        "projects.list",
        "session.create",
        "schedule.toggle",
        "browser.open",
        "memory.read",
    ] {
        assert!(actions.contains(&expected), "{expected}");
    }
    assert!(!actions.contains(&"schedule.status"));
    assert!(!actions.contains(&"session.delete"));
}

#[tokio::test]
async fn delegates_every_allowlisted_action_to_the_control_service() {
    let actions = [
        "projects.list",
        "models.list",
        "session.list",
        "session.create",
        "session.send",
        "session.fork",
        "session.status",
        "session.messages",
        "schedule.list",
        "schedule.create",
        "schedule.run",
        "schedule.delete",
        "schedule.toggle",
    ];
    let responses = actions
        .iter()
        .map(|_| Ok(json!({ "projects": [] })))
        .collect();
    let (calls, executor) = recording_executor(responses);
    let fixture = Fixture::new("delegate");
    let runtime = fixture.runtime(Some(executor));

    for action in actions {
        let result = runtime
            .execute(
                &payload(json!({ "action": action, "projectId": "project-1" })),
                quiet_signal(),
            )
            .await;
        assert_eq!(result.action, action);
        assert!(result.ok, "{action}: {result:?}");
    }

    let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), actions.len());
    for (index, action) in actions.iter().enumerate() {
        assert_eq!(calls[index].0, *action);
        assert_eq!(
            calls[index].1,
            json!({ "action": action, "projectId": "project-1" })
        );
        assert_eq!(calls[index].2, Some(Value::String("/work/project".into())));
    }
}

#[tokio::test]
async fn rejects_actions_outside_the_agent_allowlist_without_dispatching() {
    for action in ["session.delete", "schedule.status"] {
        let (calls, executor) = recording_executor(vec![]);
        let fixture = Fixture::new("reject");
        let runtime = fixture.runtime(Some(executor));

        let result = runtime
            .execute(
                &AgentToolPayload {
                    input: Some(json!({ "action": action })),
                    context_directory: None,
                    tool: None,
                },
                quiet_signal(),
            )
            .await;

        assert!(!result.ok);
        assert_eq!(result.action, action);
        assert_eq!(result.error.as_ref().unwrap().kind, "usage");
        assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }
}

#[tokio::test]
async fn missing_action_reports_unknown_and_lists_everything() {
    let (calls, executor) = recording_executor(vec![]);
    let fixture = Fixture::new("missing");
    let runtime = fixture.runtime(Some(executor));

    let result = runtime
        .execute(&AgentToolPayload::default(), quiet_signal())
        .await;
    assert_eq!(result.action, "unknown");
    assert!(!result.ok);
    let error = result.error.unwrap();
    assert_eq!(error.kind, "usage");
    assert!(
        error
            .message
            .starts_with("Unsupported OMPChamber action: missing. Use one of: ")
    );
    assert!(error.message.contains("memory.read"));
    assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
}

#[tokio::test]
async fn accepts_the_bare_action_a_tool_name_already_qualifies() {
    let (calls, executor) = recording_executor(vec![Ok(json!({ "memory": {} }))]);
    let fixture = Fixture::new("bare");
    let runtime = fixture.runtime(Some(executor));

    let result = runtime
        .execute(
            &AgentToolPayload {
                input: Some(json!({ "action": "read", "title": "Uses bun" })),
                context_directory: Some(Value::String("/work/project".into())),
                tool: Some(Value::String("ompchamber_memory".into())),
            },
            quiet_signal(),
        )
        .await;

    assert!(result.ok);
    assert_eq!(result.action, "memory.read");
    let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "memory.read");
    assert_eq!(
        calls[0].1,
        json!({ "action": "memory.read", "title": "Uses bun" })
    );
    assert_eq!(calls[0].2, Some(Value::String("/work/project".into())));
}

#[tokio::test]
async fn unresolvable_action_is_told_what_the_calling_tool_can_do() {
    let fixture = Fixture::new("unresolvable");
    let runtime = fixture.runtime(None);

    let result = runtime
        .execute(
            &AgentToolPayload {
                input: Some(json!({ "action": "get" })),
                context_directory: None,
                tool: Some(Value::String("ompchamber_memory".into())),
            },
            quiet_signal(),
        )
        .await;

    assert!(!result.ok);
    let error = result.error.unwrap();
    assert!(error.message.contains("memory.read"), "{error:?}");
    assert!(!error.message.contains("browser.open"));
}

#[tokio::test]
async fn one_tool_cannot_reach_another_tools_actions() {
    let (calls, executor) = recording_executor(vec![]);
    let fixture = Fixture::new("crosstool");
    let runtime = fixture.runtime(Some(executor));

    let result = runtime
        .execute(
            &AgentToolPayload {
                input: Some(json!({ "action": "open", "url": "https://example.test" })),
                context_directory: None,
                tool: Some(Value::String("ompchamber_memory".into())),
            },
            quiet_signal(),
        )
        .await;

    assert!(!result.ok);
    assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
}

#[tokio::test]
async fn success_envelope_shape_is_exact() {
    let (_, executor) = recording_executor(vec![Ok(json!({ "projects": [] }))]);
    let fixture = Fixture::new("envelope");
    let runtime = fixture.runtime(Some(executor));

    let result = runtime
        .execute(
            &payload(json!({ "action": "projects.list" })),
            quiet_signal(),
        )
        .await;

    let serialized = serde_json::to_string(&result).unwrap();
    assert_eq!(
        serialized,
        r#"{"schemaVersion":1,"ok":true,"action":"projects.list","data":{"projects":[]}}"#
    );
}

#[tokio::test]
async fn flattened_inputs_are_forwarded_with_the_resolved_action() {
    let (calls, executor) = recording_executor(vec![Ok(json!({}))]);
    let fixture = Fixture::new("flat");
    let runtime = fixture.runtime(Some(executor));

    let result = runtime
        .execute(
            &AgentToolPayload {
                input: Some(json!({ "action": "browser.open", "url": "https://example.test", "viewport": "mobile" })),
                context_directory: None,
                tool: Some(Value::String("ompchamber_web".into())),
            },
            quiet_signal(),
        )
        .await;

    assert!(result.ok);
    assert_eq!(result.action, "browser.open");
    let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        calls[0].1,
        json!({ "action": "browser.open", "url": "https://example.test", "viewport": "mobile" })
    );
}

#[tokio::test]
async fn service_failures_stay_structured_tool_results() {
    let fixture = Fixture::new("failures");
    let runtime = fixture.runtime(Some(Arc::new(|_action, _input, _dir, _signal| {
        Box::pin(async {
            Err(ActionExecutionError {
                message: "Task not found".to_string(),
                status_code: Some(404),
                ..Default::default()
            })
        })
    })));

    let result = runtime
        .execute(
            &AgentToolPayload {
                input: Some(json!({ "action": "schedule.run", "taskId": "missing" })),
                context_directory: Some(Value::String("/work/project".into())),
                tool: None,
            },
            quiet_signal(),
        )
        .await;

    assert_eq!(
        serde_json::to_string(&result).unwrap(),
        r#"{"schemaVersion":1,"ok":false,"action":"schedule.run","error":{"message":"Task not found","kind":"usage"}}"#
    );
}

#[tokio::test]
async fn error_kind_follows_the_status_code_band() {
    for (status_code, kind) in [
        (Some(400), "usage"),
        (Some(404), "usage"),
        (Some(498), "usage"),
        (Some(499), "runtime"),
        (Some(500), "runtime"),
        (None, "runtime"),
    ] {
        let fixture = Fixture::new("kinds");
        let status = status_code;
        let runtime = fixture.runtime(Some(Arc::new(move |_a, _i, _d, _s| {
            Box::pin(async move {
                Err(ActionExecutionError {
                    message: "boom".to_string(),
                    status_code: status,
                    ..Default::default()
                })
            })
        })));
        let result = runtime
            .execute(
                &payload(json!({ "action": "session.status", "sessionId": "ses_1" })),
                quiet_signal(),
            )
            .await;
        assert_eq!(
            result.error.as_ref().unwrap().kind,
            kind,
            "status {status_code:?}"
        );
    }
}

#[tokio::test]
async fn partial_failures_carry_their_metadata_as_data() {
    let fixture = Fixture::new("partial");
    let runtime = fixture.runtime(Some(Arc::new(|_a, _i, _d, _s| {
        Box::pin(async {
            Err(ActionExecutionError {
                message: "Dispatched session failed".to_string(),
                status_code: Some(502),
                partial: true,
                partial_action: Some(Value::String("session.send".into())),
                session_id: Some(Value::String("ses_1".into())),
                directory: Some(Value::String("/work/project".into())),
            })
        })
    })));

    let result = runtime
        .execute(
            &payload(json!({ "action": "session.send" })),
            quiet_signal(),
        )
        .await;

    assert_eq!(
        result.data,
        Some(json!({
            "partial": true,
            "partialAction": "session.send",
            "sessionId": "ses_1",
            "directory": "/work/project",
        }))
    );
    assert_eq!(result.error.as_ref().unwrap().kind, "runtime");

    // Absent partial fields are omitted, not null (JSON.stringify drops them).
    let runtime = fixture.runtime(Some(Arc::new(|_a, _i, _d, _s| {
        Box::pin(async {
            Err(ActionExecutionError {
                message: "Dispatched session failed".to_string(),
                partial: true,
                ..Default::default()
            })
        })
    })));
    let result = runtime
        .execute(
            &payload(json!({ "action": "session.send" })),
            quiet_signal(),
        )
        .await;
    assert_eq!(result.data, Some(json!({ "partial": true })));
}

#[tokio::test]
async fn forwards_cancellation_to_the_control_service() {
    let fixture = Fixture::new("cancel");
    let runtime = fixture.runtime(Some(Arc::new(|_a, _i, _d, mut signal| {
        Box::pin(async move {
            signal.aborted().await;
            Err(ActionExecutionError {
                message: "OMPChamber action was cancelled".to_string(),
                status_code: Some(499),
                ..Default::default()
            })
        })
    })));

    let handle = AbortHandle::new();
    let signal = handle.signal();
    handle.abort();
    let result = runtime
        .execute(&payload(json!({ "action": "projects.list" })), signal)
        .await;
    assert!(!result.ok);
    assert_eq!(result.action, "projects.list");
    assert_eq!(
        serde_json::to_string(&result).unwrap(),
        r#"{"schemaVersion":1,"ok":false,"action":"projects.list","error":{"message":"OMPChamber action was cancelled","kind":"runtime"}}"#
    );
}

#[tokio::test]
async fn missing_control_service_is_a_runtime_error() {
    let fixture = Fixture::new("noservice");
    let runtime = fixture.runtime(None);

    let result = runtime
        .execute(
            &payload(json!({ "action": "projects.list" })),
            quiet_signal(),
        )
        .await;

    assert_eq!(
        serde_json::to_string(&result).unwrap(),
        r#"{"schemaVersion":1,"ok":false,"action":"projects.list","error":{"message":"OMPChamber control service is unavailable","kind":"runtime"}}"#
    );
}

#[test]
fn plugin_source_carries_both_tool_schemas_and_their_own_inputs() {
    let source = plugin::create_plugin_source(&plugin::tool_specs(true, true, true));

    assert!(source.starts_with("export const OMPChamberPlugin = async () => ({\n  tool: {\n"));
    assert!(source.ends_with("  },\n})\n"));

    let control_at = source.find("    ompchamber: {").unwrap();
    let web_at = source.find("    ompchamber_web: {").unwrap();
    let memory_at = source.find("    ompchamber_memory: {").unwrap();
    assert!(control_at < web_at && web_at < memory_at);

    let control_enum = tool_enum(&source, "ompchamber");
    let web_enum = tool_enum(&source, "ompchamber_web");
    let memory_enum = tool_enum(&source, "ompchamber_memory");
    assert!(control_enum.contains("session.create"));
    assert!(!control_enum.contains("browser.open"));
    assert!(web_enum.contains("browser.open"));
    assert!(!web_enum.contains("session.create"));
    assert!(memory_enum.contains("memory.save"));
    assert!(!source.contains("\"schedule.status\""));

    // Every agent-facing definition keeps its const/description pair.
    for definition in control_actions::agent_tool_action_definitions() {
        let pair = format!(
            "{{\"const\":{},\"description\":{}}}",
            serde_json::to_string(definition.action).unwrap(),
            serde_json::to_string(definition.description).unwrap()
        );
        assert!(source.contains(&pair), "{}", definition.action);
    }

    // Each tool carries only its own inputs.
    let control_parameters = tool_parameters(&source, "ompchamber");
    let web_parameters = tool_parameters(&source, "ompchamber_web");
    let memory_parameters = tool_parameters(&source, "ompchamber_memory");
    assert!(control_parameters.contains("\"sessionId\":{\"type\":\"string\"}"));
    assert!(!control_parameters.contains("\"url\""));
    assert!(!control_parameters.contains("\"memoryId\""));
    assert!(web_parameters.contains("\"url\""));
    assert!(!web_parameters.contains("\"sessionId\""));
    for name in [
        "\"body\"",
        "\"scope\"",
        "\"memoryId\"",
        "\"type\"",
        "\"title\"",
    ] {
        assert!(memory_parameters.contains(name), "{name}");
    }
    // Memory's title override, not the bare shared one.
    assert!(
        memory_parameters.contains("The memory's title, exactly as the session index lists it")
    );
    assert!(control_parameters.contains("\"title\":{\"type\":\"string\"}"));

    // The descriptions carry their framing sentences.
    assert!(source.contains("Session dispatches return immediately by default"));
    assert!(source.contains(
        "Set wait only when the user asks or the next step requires the completed result"
    ));
    assert!(source.contains("check your own work rather than describing what you expect"));
    assert!(source.contains("read the entry with memory.read before acting on it"));

    // Flattened-vs-nested acceptance is part of the generated transport.
    assert!(
        source.contains(
            "const args = { ...flattened, ...(parameters ?? {}), action: requestedAction }"
        )
    );
    // Failure metadata keeps the hardcoded ompchamber namespace.
    assert!(source.contains("metadata: { ompchamber: { schemaVersion: 1, action: args.action, description: title, ok: false } }"));
}

#[test]
fn plugin_source_orders_memory_parameters_like_the_js() {
    let source = plugin::create_plugin_source(&plugin::tool_specs(true, false, true));
    let memory_parameters = tool_parameters(&source, "ompchamber_memory");
    let positions = [
        memory_parameters.find("\"title\":{").unwrap(),
        memory_parameters.find("\"body\":{").unwrap(),
        memory_parameters.find("\"scope\":{").unwrap(),
        memory_parameters.find("\"memoryId\":{").unwrap(),
        memory_parameters.find("\"type\":{").unwrap(),
    ];
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn plugin_source_omits_disabled_tools_entirely() {
    let web_only = plugin::create_plugin_source(&plugin::tool_specs(false, true, false));
    assert!(web_only.contains("    ompchamber_web: {"));
    assert!(!web_only.contains("    ompchamber: {"));
    assert!(!web_only.contains("    ompchamber_memory: {"));

    let control_only = plugin::create_plugin_source(&plugin::tool_specs(true, false, false));
    assert!(control_only.contains("    ompchamber: {"));
    assert!(!control_only.contains("    ompchamber_web: {"));
    assert!(!control_only.contains("    ompchamber_memory: {"));

    let memory_only = plugin::create_plugin_source(&plugin::tool_specs(false, false, true));
    assert!(memory_only.contains("    ompchamber_memory: {"));
    assert!(!memory_only.contains("    ompchamber: {"));

    let control_and_memory = plugin::create_plugin_source(&plugin::tool_specs(true, false, true));
    assert!(control_and_memory.contains("    ompchamber: {"));
    assert!(control_and_memory.contains("    ompchamber_memory: {"));
    assert!(!control_and_memory.contains("    ompchamber_web: {"));
}

#[test]
fn merge_plugin_config_preserves_configured_entries() {
    let raw = r#"{ // existing
 "plugin": ["file:///existing.js", ["example-plugin", {"flag": true}]], "model": "test/model" }"#;
    let merged =
        plugin::merge_plugin_config(Some(raw), "file:///data/agent-tool/ompchamber-plugin.js")
            .unwrap();
    let parsed: Value = serde_json::from_str(&merged).unwrap();
    assert_eq!(parsed["model"], "test/model");
    let plugin_entries = parsed["plugin"].as_array().unwrap();
    assert_eq!(plugin_entries.len(), 3);
    assert_eq!(plugin_entries[0], "file:///existing.js");
    assert_eq!(plugin_entries[1], json!(["example-plugin", {"flag": true}]));
    assert_eq!(
        plugin_entries[2],
        "file:///data/agent-tool/ompchamber-plugin.js"
    );
}

#[test]
fn merge_plugin_config_replaces_prior_references_to_the_same_url() {
    let url = "file:///data/agent-tool/ompchamber-plugin.js";
    let raw = format!(r#"{{"plugin":["file:///existing.js","{url}",["{url}",{{"flag":true}}]]}}"#);
    let merged = plugin::merge_plugin_config(Some(&raw), url).unwrap();
    let parsed: Value = serde_json::from_str(&merged).unwrap();
    let plugin_entries = parsed["plugin"].as_array().unwrap();
    assert_eq!(plugin_entries.len(), 2);
    assert_eq!(plugin_entries[0], "file:///existing.js");
    assert_eq!(plugin_entries[1], url);
}

#[test]
fn merge_plugin_config_accepts_empty_and_missing_config() {
    for raw in [None, Some(""), Some("   \n")] {
        let merged = plugin::merge_plugin_config(raw, "file:///p.js").unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["plugin"], json!(["file:///p.js"]));
    }
}

#[test]
fn merge_plugin_config_rejects_invalid_roots_and_plugin_shapes() {
    let object_error = "OPENCODE_CONFIG_CONTENT must contain a valid JSON object before OMPChamber can inject its managed tool";
    let plugin_error = "OPENCODE_CONFIG_CONTENT plugin must be an array before OMPChamber can inject its managed tool";
    assert_eq!(
        plugin::merge_plugin_config(Some("{ nope"), "file:///p.js").unwrap_err(),
        object_error
    );
    assert_eq!(
        plugin::merge_plugin_config(Some("[1,2]"), "file:///p.js").unwrap_err(),
        object_error
    );
    assert_eq!(
        plugin::merge_plugin_config(Some("\"text\""), "file:///p.js").unwrap_err(),
        object_error
    );
    assert_eq!(
        plugin::merge_plugin_config(Some("null"), "file:///p.js").unwrap_err(),
        object_error
    );
    assert_eq!(
        plugin::merge_plugin_config(Some(r#"{"plugin":"file:///x.js"}"#), "file:///p.js")
            .unwrap_err(),
        plugin_error
    );
    assert_eq!(
        plugin::merge_plugin_config(Some(r#"{"plugin":null}"#), "file:///p.js").unwrap_err(),
        plugin_error
    );
}

#[tokio::test]
async fn prepare_materializes_plugin_and_env_additions() {
    let fixture = Fixture::new("prepare");
    let (_, executor) = recording_executor(vec![Ok(json!({ "projects": [] }))]);
    let runtime = fixture.runtime_with(
        Some(executor),
        Some(3901),
        Some(
            "{ // existing\n \"plugin\": [\"file:///existing.js\"], \"model\": \"test/model\" }"
                .to_string(),
        ),
    );

    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let config: Value = serde_json::from_str(&prepared.opencode_config_content).unwrap();
    assert_eq!(config["model"], "test/model");
    let plugin_entries = config["plugin"].as_array().unwrap();
    assert_eq!(plugin_entries.len(), 2);
    assert_eq!(plugin_entries[0], "file:///existing.js");
    let injected = plugin_entries[1].as_str().unwrap();
    assert!(injected.ends_with("/agent-tool/ompchamber-plugin.js"));
    assert!(injected.starts_with("file://"));

    assert_eq!(
        prepared.agent_tool_url,
        "http://127.0.0.1:3901/api/ompchamber/agent-tool"
    );
    assert!(
        prepared
            .agent_tool_token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );

    let plugin_path = fixture.dir.join("agent-tool").join("ompchamber-plugin.js");
    let source = std::fs::read_to_string(&plugin_path).unwrap();
    assert!(source.contains("    ompchamber: {"));
    assert!(source.contains("    ompchamber_web: {"));
    assert!(source.contains("    ompchamber_memory: {"));
    assert!(!source.contains(&prepared.agent_tool_token));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&plugin_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn prepare_rotates_the_token_and_keeps_one_plugin_entry() {
    let fixture = Fixture::new("rotate");
    let runtime = fixture.runtime(None);

    let first = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();
    let second = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();
    assert_ne!(first.agent_tool_token, second.agent_tool_token);

    let config: Value = serde_json::from_str(&second.opencode_config_content).unwrap();
    let plugin_entries = config["plugin"].as_array().unwrap();
    assert_eq!(plugin_entries.len(), 1);
}

#[tokio::test]
async fn prepare_refuses_a_plugin_with_no_tools() {
    let fixture = Fixture::new("notools");
    let runtime = fixture.runtime(None);
    let error = runtime
        .prepare_managed_opencode_env(ToolIncludes {
            control: false,
            web: false,
            memory: false,
        })
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "At least one OMPChamber managed tool must be enabled to inject the plugin"
    );
}

#[tokio::test]
async fn prepare_requires_an_available_listener_port() {
    let fixture = Fixture::new("noport");
    let runtime = fixture.runtime_with(None, None, None);
    let error = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "OMPChamber listener port is unavailable for managed tool injection"
    );
}

#[tokio::test]
async fn route_requires_the_per_child_token() {
    let fixture = Fixture::new("route-auth");
    let (_, executor) = recording_executor(vec![Ok(json!({ "projects": [] }))]);
    let runtime = fixture.runtime(Some(executor));
    let app = fixture.app(runtime.clone());
    let body = r#"{"input":{"action":"projects.list"}}"#;

    // No token prepared yet.
    let response = app
        .clone()
        .oneshot(request(None, Some(LOOPBACK), None, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    // Wrong or missing credentials.
    for auth in [None, Some("wrong-token")] {
        let response = app
            .clone()
            .oneshot(request(auth, Some(LOOPBACK), None, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Unauthorized" })
        );
    }

    // Valid token + loopback peer.
    let response = app
        .clone()
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({ "schemaVersion": 1, "ok": true, "action": "projects.list", "data": { "projects": [] } })
    );

    // Rotating the token invalidates the previous child's token.
    let rotated = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .clone()
        .oneshot(request(
            Some(&rotated.agent_tool_token),
            Some(LOOPBACK),
            None,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn route_accepts_only_loopback_peers() {
    let fixture = Fixture::new("route-loopback");
    let (_, executor) = recording_executor(vec![Ok(json!({})), Ok(json!({})), Ok(json!({}))]);
    let runtime = fixture.runtime(Some(executor));
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();
    let auth = Some(prepared.agent_tool_token.clone());
    let body = r#"{"input":{"action":"projects.list"},"contextDirectory":"/work/project"}"#;
    let loopbacks = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1111),
        "[::1]:2222".parse().unwrap(),
        "[::ffff:127.0.0.1]:3333".parse().unwrap(),
    ];
    for addr in loopbacks {
        let response = app
            .clone()
            .oneshot(request(auth.as_deref(), Some(addr), None, &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{addr}");
    }

    let others = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 4444),
        "[::ffff:192.168.1.5]:5555".parse().unwrap(),
        "10.0.0.1:66".parse::<SocketAddr>().unwrap(),
    ];
    for addr in others {
        let response = app
            .clone()
            .oneshot(request(auth.as_deref(), Some(addr), None, &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{addr}");
    }

    // Missing ConnectInfo (server not serving with connect info) fails closed.
    let response = app
        .clone()
        .oneshot(request(auth.as_deref(), None, None, &body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn route_forwards_the_context_directory_to_the_service() {
    let fixture = Fixture::new("route-context");
    let (calls, executor) = recording_executor(vec![Ok(json!({ "sessions": [] }))]);
    let runtime = fixture.runtime(Some(executor));
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let response = app
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            r#"{"input":{"action":"session.list","limit":3},"contextDirectory":"/work/project","tool":"ompchamber"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({ "schemaVersion": 1, "ok": true, "action": "session.list", "data": { "sessions": [] } })
    );
    let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "session.list");
    assert_eq!(calls[0].1, json!({ "action": "session.list", "limit": 3 }));
    assert_eq!(calls[0].2, Some(Value::String("/work/project".into())));
}

#[tokio::test]
async fn route_treats_non_json_bodies_as_empty() {
    let fixture = Fixture::new("route-plain");
    let (calls, executor) = recording_executor(vec![]);
    let runtime = fixture.runtime(Some(executor));
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let response = app
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            Some("text/plain"),
            "{\"input\":{\"action\":\"projects.list\"}}",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["action"], "unknown");
    assert_eq!(body["error"]["kind"], "usage");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Unsupported OMPChamber action: missing")
    );
    assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
}

#[tokio::test]
async fn route_rejects_malformed_json_bodies() {
    let fixture = Fixture::new("route-badjson");
    let runtime = fixture.runtime(None);
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let response = app
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            "{invalid",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await.get("error").is_some());
}

#[tokio::test]
async fn route_enforces_the_body_limit() {
    let fixture = Fixture::new("route-limit");
    let runtime = fixture.runtime(None);
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let oversized = format!(
        "{{\"input\":{{\"action\":\"session.messages\",\"prompt\":\"{}\"}}}}",
        "x".repeat(1024 * 1024)
    );
    let response = app
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            &oversized,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn route_does_not_abort_the_service_after_a_completed_response() {
    let fixture = Fixture::new("route-abort");
    let seen_signal = Arc::new(Mutex::new(None::<AbortSignal>));
    let signal_slot = Arc::clone(&seen_signal);
    let runtime = fixture.runtime(Some(Arc::new(move |_a, _i, _d, signal| {
        let slot = Arc::clone(&signal_slot);
        Box::pin(async move {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(signal);
            Ok(json!({}))
        })
    })));
    let app = fixture.app(runtime.clone());
    let prepared = runtime
        .prepare_managed_opencode_env(ToolIncludes::all())
        .await
        .unwrap();

    let response = app
        .oneshot(request(
            Some(&prepared.agent_tool_token),
            Some(LOOPBACK),
            None,
            r#"{"input":{"action":"models.list"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let signal = seen_signal
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("executor ran");
    assert!(!signal.is_aborted());
}
