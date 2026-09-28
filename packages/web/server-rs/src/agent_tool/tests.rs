//! Tests for the agent-tool port, mirroring `runtime.test.js`.
//!
//! 中文说明：agent tool 移植的测试集，镜像 JS 端 `runtime.test.js`：
//! 授权与回环闸门、allowlist 分发、结果信封形状、插件源生成与合并、
//! prepare 的 token 轮换与文件落盘，以及 HTTP 路由的鉴权与转发行为。

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

/// 录制的调用三元组：(action, input, context_directory)。
type RecordedCall = (String, Value, Option<Value>);

/// 测试用的回环对端地址。
const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321);

/// 测试夹具：独立临时目录模拟 agent-tool 数据根，drop 时整体清理。
struct Fixture {
    /// 临时工作目录。
    dir: std::path::PathBuf,
}

/// 夹具的构造与 runtime/router 派生 helper。
impl Fixture {
    /// 以 tag + 毫秒时间戳 + 进程 id 命名临时目录，避免测试间冲突。
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

    /// 便捷封装：监听 3901 端口、无配置内容的 runtime。
    fn runtime(&self, executor: Option<ExecuteActionFn>) -> AgentToolRuntime {
        self.runtime_with(executor, Some(3901), None)
    }

    /// 构造完全受控的 runtime：executor 为 None 模拟控制服务缺失，
    /// 端口为 None 模拟无可用监听端口，config 内容经闭包动态读取。
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

    /// 把 runtime 包成可直接 oneshot 的 axum Router。
    fn app(&self, runtime: AgentToolRuntime) -> Router {
        router_with_runtime(Arc::new(runtime))
    }
}

/// 夹具析构：递归删除临时目录。
impl Drop for Fixture {
    /// 清理临时目录；失败忽略。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 构造会录制每次调用的假 executor：按序弹出预设响应，队列耗尽后返回
/// 空对象成功；返回 (调用记录, executor) 供断言。
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

/// 构造带固定 context 目录 `/work/project` 的标准工具载荷。
fn payload(input: Value) -> AgentToolPayload {
    AgentToolPayload {
        input: Some(input),
        context_directory: Some(Value::String("/work/project".to_string())),
        tool: None,
    }
}

/// 构造占位 AbortSignal：相关用例的执行路径只把它透传给 executor，
/// 不在此触发取消分支。
fn quiet_signal() -> AbortSignal {
    AbortHandle::new().signal()
}

/// 构造对 /api/ompchamber/agent-tool 的 POST 请求：可选 bearer token、
/// 可选对端地址（ConnectInfo 扩展）与 Content-Type（默认 application/json）。
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

/// 读出响应 body（上限 10 MiB）并按 JSON 解析，供断言使用。
async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Extracts the `enum: [...]` JSON for one generated tool entry.
/// 从生成的插件源中截取指定工具条目的 `enum: [...]` 片段。
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
/// 从生成的插件源中截取指定工具条目的 `properties: {...}` 片段。
fn tool_parameters(source: &str, name: &str) -> String {
    let entry = source
        .find(&format!("    {name}: {{"))
        .unwrap_or_else(|| panic!("tool {name} missing"));
    let rest = &source[entry..];
    let start = rest.find("properties: {").unwrap() + "properties: {".len();
    let end = start + rest[start..].find(", additionalProperties").unwrap();
    rest[start..end].to_string()
}

/// 验证恒时比较：相等为真；长度不同或任一字节不同为假（含空串边界）。
#[test]
fn timing_safe_equal_compares_bytes() {
    assert!(timing_safe_equal(b"token", b"token"));
    assert!(!timing_safe_equal(b"token", b"tokeN"));
    assert!(!timing_safe_equal(b"token", b"toke"));
    assert!(!timing_safe_equal(b"", b"a"));
    assert!(timing_safe_equal(b"", b""));
}

/// 验证回环判定只认 127.0.0.1/::1/IPv4 映射拼写；其它地址、主机名与
/// 缺失地址一律拒绝。
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

/// 验证授权需同时满足：已生成 token、回环对端、严格的 Bearer 头
/// （仅容忍首尾空白），任何一项缺失即拒绝。
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

/// 验证生成的 token 是 43 字符的 URL 安全串，且每次生成都不同。
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

/// 验证 abort 语义：丢弃未完成的 handle 会触发 signal；finish 之后的
/// 丢弃不触发；显式 abort 立即触发。
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

/// 验证 agent 可分发的动作集合是控制面的窄子集：包含只读/创建类动作，
/// 不包含 schedule.status 与 session.delete。
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

/// 验证 allowlist 内的每个动作都带着原始输入与 context 目录转发给
/// 控制服务，并回报成功。
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

/// 验证 allowlist 之外的动作以 usage 错误拒绝，且完全不触发转发。
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

/// 验证缺失 action 时报告 unknown，错误消息列出全部合法动作。
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

/// 验证带 tool 名时可用裸 action（如 read）——工具名已把它限定成
/// memory.read 一类全名。
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

/// 验证 action 解析失败时，错误消息只列调用工具自己的能力，
/// 不泄漏其它工具的动作。
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

/// 验证一个工具不能经裸 action 触达另一个工具的动作（跨工具隔离）。
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

/// 验证成功结果信封的 JSON 序列化逐字段精确（字段顺序与取值）。
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

/// 验证扁平输入与解析出的 action 合并后原样转发给控制服务。
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

/// 验证控制服务失败时仍返回结构化的工具结果信封，而非抛出异常。
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

/// 验证错误 kind 按状态码分档：4xx（至 498）为 usage，
/// 499 及以上/无状态码为 runtime。
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

/// 验证部分失败把 partial 元数据带入 data；缺失字段整体省略
/// 而非置 null（与 JSON.stringify 行为一致）。
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

/// 验证取消信号透传给控制服务，其 499 取消结果按 runtime 错误回报。
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

/// 验证控制服务缺失（executor 为 None）时报 runtime 错误信封。
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

/// 验证生成的插件源：三个工具各带自己的 enum 与参数、动作定义的
/// const/description 对齐全、关键框架文案与传输细节（扁平/嵌套入参、
/// 失败元数据命名空间）俱在。
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

/// 验证 memory 工具的参数顺序与 JS 生成的插件一致。
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

/// 验证被关闭的工具在生成的插件源中整体缺席。
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

/// 验证合并保留既有 plugin 条目（含带配置的数组形式）并把托管插件
/// 追加到末尾。
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

/// 验证合并时同一 URL 的旧引用（裸串或数组形式）被替换而非重复追加。
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

/// 验证缺失/纯空白配置按空对象处理，仅注入托管插件。
#[test]
fn merge_plugin_config_accepts_empty_and_missing_config() {
    for raw in [None, Some(""), Some("   \n")] {
        let merged = plugin::merge_plugin_config(raw, "file:///p.js").unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["plugin"], json!(["file:///p.js"]));
    }
}

/// 验证非法配置（非对象根、plugin 非数组/为 null）报出指明修复方向的
/// 错误消息。
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

/// 验证 prepare：写出 0600 权限且不含 token 的插件文件、向配置注入
/// plugin 条目，并给出 agent tool URL 与 URL 安全的 token。
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

/// 验证重复 prepare 轮换 token，且 plugin 条目不重复累积。
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

/// 验证三个工具全部关闭时 prepare 拒绝注入插件。
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

/// 验证无可用监听端口时 prepare 拒绝注入插件。
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

/// 验证路由鉴权：未 prepare 或 token 错误均 401；正确 token 200；
/// prepare 轮换后旧 token 立即失效。
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

/// 验证路由只接受回环对端（含 IPv4 映射的回环拼写）；非回环地址与
/// 缺失 ConnectInfo 都按 401 拒绝（fail closed）。
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

/// 验证路由把请求体中的 contextDirectory 与输入一起转发给控制服务。
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

/// 验证非 JSON Content-Type 的 body 按空处理：得到“缺少 action”的
/// 结构化 usage 错误，且不触发转发。
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

/// 验证声明 JSON Content-Type 但 body 非法时返回 400。
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

/// 验证超过 1 MiB 的请求体返回 413。
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

/// 验证响应正常完成后，路由不会把服务侧 signal 置为 aborted
/// （完成不等于取消）。
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
