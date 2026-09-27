//! Route + service tests mirroring `openchamber-sessions/routes.test.js`:
//! a trait-level fake engine (the JS `local-engine-client` mock + fetch
//! mock) drives the route shapes, ordering, and error mapping; a spawned
//! HTTP fake engine exercises the production wiring (URLs, headers, goal
//! PATCH) through `HttpEngineClient`.
//! （中文说明）本文件是路由 + 服务测试，对齐
//! `openchamber-sessions/routes.test.js`：用 trait 级假 engine（对应 JS
//! 的 `local-engine-client` mock + fetch mock）驱动路由形状、调用顺序与
//! 错误映射；再用真实起动的 HTTP 假 engine，通过 `HttpEngineClient`
//! 验证生产接线（URL、请求头、goal PATCH）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use crate::config::{EngineConfig, ServerConfig};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;

use super::client::{
    BoxFut, EngineClient, EngineReply, SessionCommandParams, encode_uri_component,
};
use super::routes::{self, SessionState};
use super::service::{GoalCall, SessionDeps, SessionService, WorktreeOps};

/// 共享调用日志：各替身以固定字符串记录动作，供顺序断言。
type Log = Arc<Mutex<Vec<String>>>;

/// 追加一条日志；锁中毒时也照常取值，测试不因 panic 残骸卡死。
fn log_push(log: &Log, entry: &str) {
    log.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(entry.to_string());
}

/// 返回某条日志首次出现的位置，用于调用顺序断言。
fn log_position(log: &Log, entry: &str) -> Option<usize> {
    log.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .position(|item| item == entry)
}

/// 加锁并从 poisoning 中恢复（返回 guard）；测试替身统一用它。
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

/// 临时目录序号，保证并行测试的目录互不冲突。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 创建唯一的临时目录（标签 + 进程号 + 自增序号）。
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-ocs-{label}-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

// ---------------------------------------------------------------------------
// Fake engine (JS: the local-engine-client mock + selection-input fetch mock)
// ---------------------------------------------------------------------------

/// 假 engine 的可变内核：预设消息、命令与各失败开关。
struct FakeEngineState {
    /// `session_messages` 返回的既有消息。
    existing_messages: Vec<Value>,
    /// "落盘 prompt" 的自增序号，用于生成 `msg_dispatched_N`。
    dispatched_seq: usize,
    /// 派发的 prompt 是否出现在消息列表里（模拟"已落地"）。
    land_prompts: bool,
    /// `command_list` 返回的斜杠命令定义。
    commands: Vec<Value>,
    /// 预设的 session_command 失败消息。
    command_error: Option<String>,
    /// 预设的 prompt_async 失败消息。
    prompt_error: Option<String>,
}

/// trait 级假 engine（JS：local-engine-client mock + fetch mock）：记录
/// 每类调用并把动作写入共享日志。
#[derive(Clone)]
struct FakeEngine {
    /// 共享调用日志。
    log: Log,
    /// 可变内核。
    state: Arc<Mutex<FakeEngineState>>,
    /// 记录 (session_id, directory, limit)。
    messages_calls: Arc<Mutex<Vec<(String, String, u32)>>>,
    /// 记录 (session_id, directory, message_id)。
    fork_calls: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    /// 记录斜杠命令派发参数。
    command_calls: Arc<Mutex<Vec<SessionCommandParams>>>,
    /// 记录 fetch_json 的请求路径。
    fetch_calls: Arc<Mutex<Vec<String>>>,
    /// 记录 (directory, title)。
    create_session_calls: Arc<Mutex<Vec<(String, Option<String>)>>>,
    /// 记录 (session_id, directory, payload)。
    prompt_payloads: Arc<Mutex<Vec<(String, String, Value)>>>,
}

/// 构造器与预设注入方法。
impl FakeEngine {
    /// 初始化空内核与各空记录表。
    fn new(log: Log) -> Self {
        Self {
            log,
            state: Arc::new(Mutex::new(FakeEngineState {
                existing_messages: Vec::new(),
                dispatched_seq: 0,
                land_prompts: true,
                commands: Vec::new(),
                command_error: None,
                prompt_error: None,
            })),
            messages_calls: Arc::new(Mutex::new(Vec::new())),
            fork_calls: Arc::new(Mutex::new(Vec::new())),
            command_calls: Arc::new(Mutex::new(Vec::new())),
            fetch_calls: Arc::new(Mutex::new(Vec::new())),
            create_session_calls: Arc::new(Mutex::new(Vec::new())),
            prompt_payloads: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 预设既有消息列表。
    fn set_existing_messages(&self, messages: Vec<Value>) {
        lock(&self.state).existing_messages = messages;
    }

    /// 预设斜杠命令定义。
    fn set_commands(&self, commands: Vec<Value>) {
        lock(&self.state).commands = commands;
    }

    /// 预设 session_command 的失败消息。
    fn set_command_error(&self, error: &str) {
        lock(&self.state).command_error = Some(error.to_string());
    }

    /// 预设 prompt_async 的失败消息。
    fn set_prompt_error(&self, error: &str) {
        lock(&self.state).prompt_error = Some(error.to_string());
    }

    /// 控制派发的 prompt 是否"落地"到消息列表。
    fn set_land_prompts(&self, land: bool) {
        lock(&self.state).land_prompts = land;
    }

    /// `selectionInputResponse`: the providers/agents/config bodies every
    /// prompt-dispatching fetch mock must answer.
    /// 对应 JS 的 `selectionInputResponse`：每个会触发 prompt 派发的 fetch
    /// mock 都要能回答的 providers/agents/config 响应体；未匹配的路径返回
    /// `None`（由调用方回退默认值）。
    fn selection_response(path: &str) -> Option<Value> {
        if path.starts_with("/config/providers") {
            return Some(json!({
                "providers": [
                    { "id": "openai", "models": [{ "id": "gpt-5.5", "variants": { "high": {} } }] },
                    { "id": "anthropic", "models": [{ "id": "claude-sonnet-5", "variants": { "high": {} } }] },
                ],
            }));
        }
        if path.starts_with("/agent") {
            return Some(json!([
                { "name": "build", "mode": "primary" },
                { "name": "plan", "mode": "primary" },
                { "name": "reviewer", "mode": "subagent" },
            ]));
        }
        if path.starts_with("/config") {
            return Some(json!({}));
        }
        None
    }
}

/// 各 engine 接口的假实现：记录调用、写日志、按预设应答或失败。
impl EngineClient for FakeEngine {
    /// 返回既有消息；`land_prompts` 开启时追加一条新派发的用户消息。
    fn session_messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: u32,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        lock(&self.messages_calls).push((session_id.to_string(), directory.to_string(), limit));
        log_push(&self.log, &format!("engine.messages:{session_id}"));
        let messages = {
            let mut state = lock(&self.state);
            let mut messages = state.existing_messages.clone();
            if state.land_prompts {
                state.dispatched_seq += 1;
                let seq = state.dispatched_seq;
                messages.push(json!({
                    "info": {
                        "id": format!("msg_dispatched_{seq}"),
                        "role": "user",
                        "time": { "created": 1000 + seq },
                    },
                }));
            }
            messages
        };
        Box::pin(async move { Ok(EngineReply::ok(Value::Array(messages))) })
    }

    /// 记录 fork 调用并固定返回 `ses_fork` 会话。
    fn session_fork(
        &self,
        session_id: &str,
        directory: &str,
        message_id: Option<&str>,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        lock(&self.fork_calls).push((
            session_id.to_string(),
            directory.to_string(),
            message_id.map(String::from),
        ));
        log_push(&self.log, "engine.session_fork");
        Box::pin(async move {
            Ok(EngineReply::ok(
                json!({ "id": "ses_fork", "title": "Forked session" }),
            ))
        })
    }

    /// 记录斜杠命令派发；预设了错误则返回 `Err`。
    fn session_command(
        &self,
        _session_id: &str,
        params: &SessionCommandParams,
    ) -> BoxFut<'_, Result<EngineReply, String>> {
        lock(&self.command_calls).push(params.clone());
        log_push(&self.log, "engine.session_command");
        let error = lock(&self.state).command_error.clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(error),
                None => Ok(EngineReply::ok(json!({}))),
            }
        })
    }

    /// 返回预设的斜杠命令定义列表。
    fn command_list(&self, _directory: &str) -> BoxFut<'_, Result<EngineReply, String>> {
        log_push(&self.log, "engine.command_list");
        let commands = lock(&self.state).commands.clone();
        Box::pin(async move { Ok(EngineReply::ok(Value::Array(commands))) })
    }

    /// 记录路径并返回 selection 响应；未匹配路径回退调用方默认值。
    fn fetch_json(&self, path: &str, _directory: &str, fallback: Value) -> BoxFut<'_, Value> {
        lock(&self.fetch_calls).push(path.to_string());
        log_push(&self.log, &format!("engine.fetch_json:{path}"));
        let body = Self::selection_response(path).unwrap_or(fallback);
        Box::pin(async move { body })
    }

    /// 记录 (directory, title) 并固定返回 `ses_123`。
    fn create_session(
        &self,
        directory: &str,
        title: Option<&str>,
    ) -> BoxFut<'_, Result<String, String>> {
        lock(&self.create_session_calls).push((directory.to_string(), title.map(String::from)));
        log_push(&self.log, "engine.create_session");
        Box::pin(async move { Ok("ses_123".to_string()) })
    }

    /// 记录 prompt 载荷；预设了错误则返回 `Err`。
    fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        payload: &Value,
    ) -> BoxFut<'_, Result<(), String>> {
        lock(&self.prompt_payloads).push((
            session_id.to_string(),
            directory.to_string(),
            payload.clone(),
        ));
        log_push(&self.log, "engine.prompt_async");
        let error = lock(&self.state).prompt_error.clone();
        Box::pin(async move {
            match error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fake worktrees (JS: the `git/index.js` mock)
// ---------------------------------------------------------------------------

/// 假 worktree 实现（JS：`git/index.js` mock）：记录创建调用并按队列
/// 回放 bootstrap 状态。
struct FakeWorktrees {
    /// 共享调用日志。
    log: Log,
    /// create 固定返回的 worktree 记录。
    record: Value,
    /// 记录 (directory, input)。
    create_calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// 预设的 bootstrap 状态队列（耗尽后回退 ready）。
    statuses: Arc<Mutex<Vec<Value>>>,
}

/// create 与 bootstrap_status 的假实现，均记录日志。
impl WorktreeOps for FakeWorktrees {
    /// 记录调用并返回固定 worktree 记录。
    fn create(&self, directory: &str, input: &Value) -> BoxFut<'_, Result<Value, String>> {
        lock(&self.create_calls).push((directory.to_string(), input.clone()));
        log_push(&self.log, "worktree.create");
        let record = self.record.clone();
        Box::pin(async move { Ok(record) })
    }

    /// 逐条弹出预设状态；队列空了回退 ready（对齐 JS 的
    /// `statuses.shift() || last`）。
    fn bootstrap_status(&self, _directory: &str) -> BoxFut<'_, Result<Value, String>> {
        log_push(&self.log, "worktree.status");
        // JS test harness: `statuses.shift() || last`.
        let status = {
            let mut statuses = lock(&self.statuses);
            if !statuses.is_empty() {
                statuses.remove(0)
            } else {
                json!({ "status": "ready", "phase": "setup-ready", "error": null })
            }
        };
        Box::pin(async move { Ok(status) })
    }
}
// Harness (JS: `createApp`)
// ---------------------------------------------------------------------------

/// 测试装配（JS：`createApp`）：真实路由 + 假依赖，暴露各记录表供断言。
struct Harness {
    /// 被测路由。
    router: Router,
    /// 共享调用日志。
    log: Log,
    /// 假 engine。
    engine: FakeEngine,
    /// 可预置的 bootstrap 状态队列。
    worktree_statuses: Arc<Mutex<Vec<Value>>>,
    /// worktree 创建调用记录。
    create_worktree_calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// goal 元数据创建调用记录。
    goal_calls: Arc<Mutex<Vec<GoalCall>>>,
    /// emit 出的 session-created 事件。
    events: Arc<Mutex<Vec<Value>>>,
}

/// 默认装配：settings 里带一个项目 `/repo/app`。
fn harness() -> Harness {
    harness_with(json!({ "projects": [{ "id": "proj_1", "path": "/repo/app" }] }))
}

/// 用自定义 settings 装配：假 engine/worktree、直通的目录校验与就绪等待、
/// 记录型 goal 创建与事件发射，组合成 `SessionDeps` 并挂上真实路由。
fn harness_with(settings: Value) -> Harness {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let engine = FakeEngine::new(Arc::clone(&log));
    let worktree_statuses: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let create_worktree_calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let worktrees: Arc<dyn WorktreeOps> = Arc::new(FakeWorktrees {
        log: Arc::clone(&log),
        record: json!({
            "head": "abc123",
            "name": "side-task",
            "branch": "ompchamber/side-task",
            "path": "/repo/worktrees/side-task",
        }),
        create_calls: Arc::clone(&create_worktree_calls),
        statuses: Arc::clone(&worktree_statuses),
    });
    let settings = Arc::new(Mutex::new(
        settings.as_object().cloned().unwrap_or_default(),
    ));
    let goal_calls: Arc<Mutex<Vec<GoalCall>>> = Arc::new(Mutex::new(Vec::new()));
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let read_settings = {
        let settings = Arc::clone(&settings);
        Arc::new(move || {
            let settings = Arc::clone(&settings);
            Box::pin(async move { lock(&settings).clone() }) as BoxFut<'static, Map<String, Value>>
        })
    };
    let create_goal = {
        let log = Arc::clone(&log);
        let goal_calls = Arc::clone(&goal_calls);
        Arc::new(move |call: GoalCall| {
            log_push(&log, "goal");
            lock(&goal_calls).push(call);
            Box::pin(async { Ok(()) }) as BoxFut<'static, Result<(), String>>
        })
    };
    let emit = {
        let events = Arc::clone(&events);
        Arc::new(move |event: &Value| {
            lock(&events).push(event.clone());
        })
    };

    let deps = SessionDeps {
        client: Arc::new(engine.clone()) as Arc<dyn EngineClient>,
        read_settings,
        sanitize_projects: Arc::new(|projects: &Value| {
            projects.as_array().cloned().unwrap_or_default()
        }),
        validate_directory: Arc::new(|directory: &str| {
            let directory = directory.to_string();
            Box::pin(async move { Ok(directory) }) as BoxFut<'static, Result<String, String>>
        }),
        wait_ready: Arc::new(|| Box::pin(async { Ok(()) }) as BoxFut<'static, Result<(), String>>),
        worktrees,
        create_goal,
        emit_session_created: emit,
    };
    let router = routes::routes().with_state(SessionState {
        service: Arc::new(SessionService::new(deps)),
    });
    Harness {
        router,
        log,
        engine,
        worktree_statuses,
        create_worktree_calls,
        goal_calls,
        events,
    }
}

/// 向路由发一个 JSON POST，返回 (状态码, 解析后的 body)。
async fn post(router: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// 会话创建路由的 URI。
const CREATE_URI: &str = "/api/ompchamber/sessions";

// ---------------------------------------------------------------------------
// Route tests (JS parity)
// ---------------------------------------------------------------------------

/// 验证：仅带目录与标题创建会话——engine 收到一次 create，不派发
/// prompt，响应不含 model 字段。
#[tokio::test]
async fn creates_a_session_for_a_directory() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "title": "Side task" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessionId"], "ses_123");
    assert_eq!(body["directory"], "/repo/app");
    assert_eq!(body["promptDispatched"], false);
    assert_eq!(body["dispatchedAsCommand"], false);
    assert!(body.get("model").is_none());
    let create_calls = lock(&h.engine.create_session_calls);
    assert_eq!(create_calls.len(), 1);
    assert_eq!(
        create_calls[0],
        ("/repo/app".to_string(), Some("Side task".to_string()))
    );
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：创建成功后发射 session-created 事件，携带 sessionID/目录/标题
/// 与两个派发标志（createdAt 为数值时间戳）。
#[tokio::test]
async fn emits_session_created_event_after_creating_session() {
    let h = harness();
    let (status, _) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "title": "Side task" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = lock(&h.events);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["sessionID"], "ses_123");
    assert_eq!(event["directory"], "/repo/app");
    assert_eq!(event["title"], "Side task");
    assert_eq!(event["promptDispatched"], false);
    assert_eq!(event["dispatchedAsCommand"], false);
    assert!(event["createdAt"].is_u64());
}

/// 验证：prompt 省略 model/agent 时从 settings 默认值解析（并确实拉取了
/// selection 输入），载荷与响应都带上默认选择。
#[tokio::test]
async fn resolves_default_model_and_agent_when_prompt_omits_them() {
    let h = harness_with(json!({
        "defaultModel": "openai/gpt-5.5",
        "defaultAgent": "build",
        "projects": [{ "id": "proj_1", "path": "/repo/app" }],
    }));
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "prompt": "Run this" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["model"],
        json!({ "providerID": "openai", "modelID": "gpt-5.5" })
    );
    assert_eq!(body["agent"], "build");
    assert!(
        lock(&h.engine.fetch_calls)
            .iter()
            .any(|path| path.starts_with("/config/providers")),
        "selection inputs must be fetched"
    );
    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0].2["model"],
        json!({ "providerID": "openai", "modelID": "gpt-5.5" })
    );
    assert_eq!(payloads[0].2["agent"], "build");
}

/// 验证：显式给出 model 时派发初始 prompt；无 agent 配置时默认选第一个
/// primary agent（build）。
#[tokio::test]
async fn dispatches_initial_prompt_when_model_is_provided() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "prompt": "Run this", "model": "openai/gpt-5.5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessionId"], "ses_123");
    assert_eq!(body["promptDispatched"], true);
    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0, "ses_123");
    assert_eq!(payloads[0].1, "/repo/app");
    // No agent requested and none configured in settings: the default
    // selection picks the first primary agent.
    assert_eq!(payloads[0].2["agent"], "build");
}

/// 验证：goal 模式先创建 goal 元数据再派发 prompt；prompt 载荷追加
/// synthetic 引言（含 "Goal mode is active" 与 token 预算），响应回带
/// goalEnabled/goalTokenBudget。
#[tokio::test]
async fn creates_goal_metadata_before_dispatching_the_initial_goal_prompt() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "prompt": "Finish and verify the migration",
            "model": "openai/gpt-5.5",
            "goal": true,
            "goalTokenBudget": 200000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let goal_calls = lock(&h.goal_calls);
    assert_eq!(goal_calls.len(), 1);
    assert_eq!(goal_calls[0].session_id, "ses_123");
    assert_eq!(goal_calls[0].directory, "/repo/app");
    assert_eq!(goal_calls[0].objective, "Finish and verify the migration");
    assert_eq!(goal_calls[0].token_budget, Some(200000));
    assert_eq!(goal_calls[0].provider_id, "openai");
    assert_eq!(goal_calls[0].model_id, "gpt-5.5");
    drop(goal_calls);

    let goal_at = log_position(&h.log, "goal").expect("goal call");
    let prompt_at = log_position(&h.log, "engine.prompt_async").expect("prompt");
    assert!(
        goal_at < prompt_at,
        "goal metadata must exist before dispatch"
    );

    let payloads = lock(&h.engine.prompt_payloads);
    let parts = payloads[0].2["parts"].as_array().expect("parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0],
        json!({ "type": "text", "text": "Finish and verify the migration" })
    );
    assert_eq!(parts[1]["type"], "text");
    assert_eq!(parts[1]["synthetic"], true);
    let intro = parts[1]["text"].as_str().expect("intro text");
    assert!(intro.contains("Goal mode is active"));
    assert!(intro.contains("200000 tokens"));
    drop(payloads);

    assert_eq!(body["goalEnabled"], true);
    assert_eq!(body["goalTokenBudget"], 200000);
    assert_eq!(body["promptDispatched"], true);
}

/// 验证：非法 goal 请求（开 goal 缺 prompt、裸 goalTokenBudget、预算
/// 越界）在触碰 engine 之前就被 400 拒绝，且日志为空。
#[tokio::test]
async fn rejects_invalid_goal_requests_before_creating_a_session() {
    let h = harness();
    let cases = [
        (
            json!({ "directory": "/repo/app", "goal": true }),
            "prompt is required when goal is enabled",
        ),
        (
            json!({ "directory": "/repo/app", "prompt": "Run", "goalTokenBudget": 200000 }),
            "goalTokenBudget requires goal",
        ),
        (
            json!({ "directory": "/repo/app", "prompt": "Run", "goal": true, "goalTokenBudget": 999 }),
            "goalTokenBudget must be an integer from 1000 to 100000000",
        ),
    ];
    for (payload, message) in cases {
        let (status, body) = post(&h.router, CREATE_URI, &payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
        assert_eq!(body, json!({ "error": message }));
    }
    assert!(lock(&h.log).is_empty(), "no engine contact allowed");
}

/// 验证：带 worktree 的创建先建 worktree，会话与 prompt 都落在 worktree
/// 目录（`/repo/worktrees/side-task`）里。
#[tokio::test]
async fn creates_a_worktree_before_creating_a_session() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "worktree": { "name": "side-task", "branchName": "ompchamber/side-task", "startRef": "main" },
            "setUpstream": false,
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let create_calls = lock(&h.create_worktree_calls);
    assert_eq!(create_calls.len(), 1);
    assert_eq!(create_calls[0].0, "/repo/app");
    assert_eq!(
        create_calls[0].1,
        json!({
            "mode": "new",
            "name": "side-task",
            "branchName": "ompchamber/side-task",
            "startRef": "main",
            "setUpstream": false,
        })
    );
    drop(create_calls);
    assert_eq!(body["directory"], "/repo/worktrees/side-task");
    assert_eq!(body["worktree"]["path"], "/repo/worktrees/side-task");
    let engine_creates = lock(&h.engine.create_session_calls);
    assert_eq!(engine_creates.len(), 1);
    assert_eq!(engine_creates[0].0, "/repo/worktrees/side-task");
    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].1, "/repo/worktrees/side-task");
}

/// 验证：会话创建被 bootstrap 轮询挡在后面，顺序为 worktree.status →
/// engine.create_session → engine.prompt_async。
#[tokio::test]
async fn waits_for_the_worktree_bootstrap_before_creating_the_session() {
    let h = harness();
    let mut statuses = lock(&h.worktree_statuses);
    statuses.extend([
        json!({ "status": "pending", "phase": "directory-created", "error": null, "updatedAt": 1 }),
        json!({ "status": "pending", "phase": "git-ready", "error": null, "updatedAt": 2 }),
        json!({ "status": "ready", "phase": "setup-ready", "error": null, "updatedAt": 3 }),
    ]);
    drop(statuses);
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "worktree": { "name": "side-task" },
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["promptDispatched"], true);

    let log = lock(&h.log);
    let first_status = log
        .iter()
        .position(|entry| entry == "worktree.status")
        .expect("bootstrap poll");
    let create_at = log
        .iter()
        .position(|entry| entry == "engine.create_session")
        .expect("session create");
    let prompt_at = log
        .iter()
        .position(|entry| entry == "engine.prompt_async")
        .expect("prompt");
    drop(log);
    assert!(
        first_status < create_at,
        "bootstrap waits gate the session create"
    );
    assert!(create_at < prompt_at);
}

/// 验证：bootstrap 失败时创建以 500 失败并带回 "Worktree bootstrap
/// failed: …"，不派发 prompt。
#[tokio::test]
async fn fails_the_create_when_the_worktree_bootstrap_failed() {
    let h = harness();
    let mut statuses = lock(&h.worktree_statuses);
    statuses.push(json!({
        "status": "failed",
        "phase": "directory-created",
        "error": "branch already exists",
        "updatedAt": 4,
    }));
    drop(statuses);
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "worktree": { "name": "side-task" },
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        json!({ "error": "Worktree bootstrap failed: branch already exists" })
    );
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：向既有会话发送 goal prompt 时先建 goal 元数据再派发，基线消息
/// ID 取最后一条助手消息，variant 原样透传。
#[tokio::test]
async fn sends_a_goal_prompt_to_an_existing_session_after_creating_goal_metadata() {
    let h = harness();
    h.engine.set_existing_messages(vec![json!({
        "info": { "id": "msg_before", "role": "assistant", "time": { "created": 10, "completed": 20 } },
    })]);
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": "/repo/app",
            "prompt": "Apply and verify the review feedback",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "variant": "high",
            "goal": true,
            "goalTokenBudget": 200000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["action"], "send");
    assert_eq!(body["sessionId"], "ses_source");
    assert_eq!(body["directory"], "/repo/app");
    assert_eq!(body["promptDispatched"], true);
    assert_eq!(body["goalEnabled"], true);
    assert_eq!(body["baselineAssistantMessageId"], "msg_before");

    let goal_calls = lock(&h.goal_calls);
    assert_eq!(goal_calls.len(), 1);
    assert_eq!(goal_calls[0].session_id, "ses_source");
    assert_eq!(goal_calls[0].directory, "/repo/app");
    assert_eq!(
        goal_calls[0].objective,
        "Apply and verify the review feedback"
    );
    drop(goal_calls);
    let goal_at = log_position(&h.log, "goal").expect("goal");
    let prompt_at = log_position(&h.log, "engine.prompt_async").expect("prompt");
    assert!(goal_at < prompt_at);

    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0, "ses_source");
    assert_eq!(payloads[0].2["variant"], "high");
}

/// 验证：斜杠命令 prompt 的 goal 目标取展开后的模板（$ARGUMENTS 已
/// 替换）；goal 先建、命令后派发，且不再走 prompt_async。
#[tokio::test]
async fn uses_the_expanded_slash_command_template_as_the_goal_objective() {
    let h = harness();
    h.engine.set_commands(vec![json!({
        "name": "issue--to-pr",
        "template": "Take $ARGUMENTS from issue through a verified pull request. Confirm the PR covers $ARGUMENTS.",
    })]);
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": "/repo/app",
            "prompt": "/issue--to-pr LIN-123",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "goal": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let goal_calls = lock(&h.goal_calls);
    assert_eq!(goal_calls.len(), 1);
    assert_eq!(
        goal_calls[0].objective,
        "Take LIN-123 from issue through a verified pull request. Confirm the PR covers LIN-123."
    );
    drop(goal_calls);

    let command_calls = lock(&h.engine.command_calls);
    assert_eq!(command_calls.len(), 1);
    assert_eq!(command_calls[0].command, "issue--to-pr");
    assert_eq!(command_calls[0].arguments, "LIN-123");
    drop(command_calls);

    let goal_at = log_position(&h.log, "goal").expect("goal");
    let command_at = log_position(&h.log, "engine.session_command").expect("command");
    assert!(goal_at < command_at);
    assert_eq!(body["goalEnabled"], true);
    assert_eq!(body["dispatchedAsCommand"], true);
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：send 省略 selection 时复用会话上一次的用户消息选择
/// （model/agent/variant），完全不访问 selection 端点。
#[tokio::test]
async fn reuses_the_previous_session_selection_when_send_omits_selection() {
    let h = harness();
    h.engine.set_existing_messages(vec![
        json!({
            "info": {
                "id": "msg_user",
                "role": "user",
                "agent": "plan",
                "model": { "providerID": "anthropic", "modelID": "claude-sonnet-5", "variant": "high" },
                "time": { "created": 5 },
            },
        }),
        json!({
            "info": { "id": "msg_before", "role": "assistant", "time": { "created": 10, "completed": 20 } },
        }),
    ]);
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({ "directory": "/repo/app", "prompt": "Continue where you left off" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["action"], "send");
    assert_eq!(body["sessionId"], "ses_source");
    assert_eq!(
        body["model"],
        json!({ "providerID": "anthropic", "modelID": "claude-sonnet-5" })
    );
    assert_eq!(body["agent"], "plan");
    assert_eq!(body["variant"], "high");
    assert_eq!(body["promptDispatched"], true);

    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0].2["model"],
        json!({ "providerID": "anthropic", "modelID": "claude-sonnet-5" })
    );
    assert_eq!(payloads[0].2["agent"], "plan");
    assert_eq!(payloads[0].2["variant"], "high");
    drop(payloads);
    // The default-selection inputs must not be consulted.
    assert!(
        lock(&h.engine.fetch_calls).is_empty(),
        "selection endpoints must stay untouched"
    );
}

/// 验证：fork 透传 messageId，prompt 派发到新会话 `ses_fork`，基线/
/// 落地查询都指向 fork，且发射带 sourceSessionID 的创建事件。
#[tokio::test]
async fn forks_from_a_message_dispatches_the_prompt_and_emits_the_new_session() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/fork",
        &json!({
            "directory": "/repo/app",
            "messageId": "msg_branch_point",
            "prompt": "Try the alternative implementation",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "variant": "high",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let fork_calls = lock(&h.engine.fork_calls);
    assert_eq!(fork_calls.len(), 1);
    assert_eq!(
        fork_calls[0],
        (
            "ses_source".to_string(),
            "/repo/app".to_string(),
            Some("msg_branch_point".to_string())
        )
    );
    drop(fork_calls);

    assert_eq!(body["action"], "fork");
    assert_eq!(body["sourceSessionId"], "ses_source");
    assert_eq!(body["sessionId"], "ses_fork");
    assert_eq!(body["directory"], "/repo/app");
    assert_eq!(body["promptDispatched"], true);
    assert_eq!(body["title"], "Forked session");

    let messages_calls = lock(&h.engine.messages_calls);
    assert!(
        messages_calls
            .iter()
            .any(|(session, _, limit)| session == "ses_fork" && *limit == 100),
        "baseline + landed lookups target the fork: {messages_calls:?}"
    );
    drop(messages_calls);

    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].0, "ses_fork");
    assert_eq!(payloads[0].1, "/repo/app");
    drop(payloads);

    let events = lock(&h.events);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["sessionID"], "ses_fork");
    assert_eq!(events[0]["sourceSessionID"], "ses_source");
    assert_eq!(events[0]["directory"], "/repo/app");
    assert_eq!(events[0]["promptDispatched"], true);
}

/// 验证：send/fork 缺 prompt 时在触碰 engine 之前被 400 拒绝
///（"prompt is required"）。
#[tokio::test]
async fn rejects_send_and_fork_without_a_prompt_before_calling_the_engine() {
    let h = harness();
    for uri in [
        "/api/ompchamber/sessions/ses_source/send",
        "/api/ompchamber/sessions/ses_source/fork",
    ] {
        let (status, body) = post(&h.router, uri, &json!({ "directory": "/repo/app" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "prompt is required" }));
    }
    assert!(lock(&h.log).is_empty());
    assert!(lock(&h.engine.fork_calls).is_empty());
}

/// 验证：fork 成功后 prompt 派发失败时上报部分结果：partial + "fork-created"
/// + 新会话 ID + 目录。
#[tokio::test]
async fn reports_the_forked_session_when_prompt_dispatch_fails() {
    let h = harness();
    h.engine
        .set_prompt_error("prompt_async failed (500): dispatch failed");
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/fork",
        &json!({
            "directory": "/repo/app",
            "prompt": "Try another approach",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "variant": "high",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["partial"], true);
    assert_eq!(body["partialAction"], "fork-created");
    assert_eq!(body["sessionId"], "ses_fork");
    assert_eq!(body["directory"], "/repo/app");
    assert_eq!(body["error"], "prompt_async failed (500): dispatch failed");
}

/// 验证：goal 已配置后派发失败时上报 "goal-configured" 部分结果。
#[tokio::test]
async fn reports_goal_configured_partial_when_dispatch_fails_after_goal_creation() {
    let h = harness();
    h.engine
        .set_prompt_error("prompt_async failed (500): dispatch failed");
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": "/repo/app",
            "prompt": "Apply the review feedback",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "goal": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["partial"], true);
    assert_eq!(body["partialAction"], "goal-configured");
    assert_eq!(body["sessionId"], "ses_source");
    assert_eq!(body["directory"], "/repo/app");
}

/// 验证：显式请求的 model 不继承 settings 里的默认 variant。
#[tokio::test]
async fn does_not_apply_a_default_variant_to_an_explicitly_requested_model() {
    let h = harness_with(json!({
        "defaultModel": "openai/gpt-5.5",
        "defaultVariant": "high",
        "projects": [{ "id": "proj_1", "path": "/repo/app" }],
    }));
    let (status, _) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": "/repo/app",
            "prompt": "Continue",
            "model": "openai/gpt-5.5",
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payloads = lock(&h.engine.prompt_payloads);
    assert_eq!(payloads.len(), 1);
    assert!(
        payloads[0].2.get("variant").is_none(),
        "explicit model must not inherit the settings default variant: {}",
        payloads[0].2
    );
}

/// 验证：未知 agent 在建会话/worktree 之前就被 400 拒绝。
#[tokio::test]
async fn rejects_an_unknown_agent_before_creating_a_session_or_worktree() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "prompt": "Run this",
            "agent": "not-an-agent",
            "worktree": { "name": "side-task" },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Unknown agent 'not-an-agent' for /repo/app" })
    );
    assert!(lock(&h.create_worktree_calls).is_empty());
    assert!(lock(&h.engine.create_session_calls).is_empty());
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：subagent（reviewer）不能直接接收 prompt，被 400 拒绝。
#[tokio::test]
async fn rejects_a_subagent_selection() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
            "agent": "reviewer",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Agent 'reviewer' is a subagent and cannot receive a prompt directly" })
    );
}

/// 验证：未知 model 与未知 variant 都在派发之前被 400 拒绝。
#[tokio::test]
async fn rejects_an_unknown_model_and_an_unknown_variant_before_dispatching() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "prompt": "Run this", "model": "openai/gpt-nope" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Unknown model 'openai/gpt-nope' for /repo/app" })
    );

    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({
            "directory": "/repo/app",
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
            "variant": "ultra",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Unknown variant 'ultra' for model 'openai/gpt-5.5'" })
    );
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：未知 projectId 报 404 "Project not found"；有效 projectId 解析出
/// 项目目录并在响应回带。
#[tokio::test]
async fn rejects_unknown_project_and_resolves_project_directories() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "projectId": "proj_nope", "prompt": "Run this" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "Project not found" }));

    let (status, body) = post(&h.router, CREATE_URI, &json!({ "projectId": "proj_1" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["directory"], "/repo/app");
    assert_eq!(body["projectId"], "proj_1");
}

/// 验证：prompt 未落地时创建仍 200，但 promptDispatched 为 false 并携带
/// promptError 说明。
#[tokio::test]
async fn reports_prompt_dispatched_false_when_the_prompt_never_lands() {
    let h = harness();
    h.engine.set_land_prompts(false);
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "prompt": "Run this", "model": "openai/gpt-5.5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessionId"], "ses_123");
    assert_eq!(body["promptDispatched"], false);
    assert_eq!(
        body["promptError"],
        "OpenCode accepted the prompt but it never appeared in the session"
    );
}

/// 验证：斜杠命令派发失败时直接报 500，不会退回成普通 prompt 重试。
#[tokio::test]
async fn does_not_retry_a_failed_slash_command_as_a_normal_prompt() {
    let h = harness();
    h.engine.set_commands(vec![json!({ "name": "review" })]);
    h.engine.set_command_error("command response failed");
    let (status, body) = post(
        &h.router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": "/repo/app",
            "prompt": "/review fix this",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "variant": "high",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({ "error": "command response failed" }));
    assert_eq!(lock(&h.engine.command_calls).len(), 1);
    assert!(lock(&h.engine.prompt_payloads).is_empty());
}

/// 验证：提供 worktree 对象但缺 name 时被 400 拒绝，不创建 worktree。
#[tokio::test]
async fn worktree_requires_a_name_when_provided() {
    let h = harness();
    let (status, body) = post(
        &h.router,
        CREATE_URI,
        &json!({ "directory": "/repo/app", "worktree": { "branchName": "x" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "worktree.name is required when worktree is provided" })
    );
    assert!(lock(&h.create_worktree_calls).is_empty());
}

// ---------------------------------------------------------------------------
// Production wiring over a spawned fake engine (JS: the global fetch mock)
// ---------------------------------------------------------------------------

/// 假 engine 记录到的一次 HTTP 调用：方法、路径与关键请求头、请求体。
#[derive(Clone, Debug)]
struct RecordedCall {
    /// HTTP 方法。
    method: String,
    /// 路径 + 查询串。
    path: String,
    /// `x-opencode-directory` 请求头（百分号编码后的目录）。
    directory_header: Option<String>,
    /// authorization 请求头。
    authorization: Option<String>,
    /// content-type 请求头。
    content_type: Option<String>,
    /// 解析后的 JSON 请求体（非 JSON 时为 null）。
    body: Value,
}

/// 假 engine 的调用记录器。
#[derive(Default)]
struct EngineRecorder {
    /// 收到的全部调用，按到达顺序。
    calls: Mutex<Vec<RecordedCall>>,
}

/// 查询辅助：按方法与路径前缀检索调用。
impl EngineRecorder {
    /// 找第一条匹配方法与前缀的调用。
    fn find(&self, method: &str, prefix: &str) -> Option<RecordedCall> {
        lock(&self.calls)
            .iter()
            .find(|call| call.method == method && call.path.starts_with(prefix))
            .cloned()
    }

    /// 该会话是否已收到 prompt_async——决定消息查询是否"看到"落地的
    /// prompt。
    fn has_prompt_async(&self, session_id: &str) -> bool {
        lock(&self.calls).iter().any(|call| {
            call.method == "POST"
                && call.path.starts_with(&format!("/session/{session_id}/"))
                && request_path_only(&call.path).ends_with("/prompt_async")
        })
    }
}

/// 生成 `directory=<值>` 的表单编码查询串，与生产客户端的拼法一致。
fn form_encode(value: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("directory", value)
        .finish()
}

/// 去掉查询串，只留路径部分。
fn request_path_only(path: &str) -> String {
    path.split('?').next().unwrap_or(path).to_string()
}

/// 在随机端口起一个假 engine（JS：global fetch mock）：记录所有请求，
/// 并按路径形状应答 session 创建、prompt_async、command、fork、消息、
/// 配置等端点；消息查询依据"该会话是否已派发过 prompt"决定是否返回
/// 落地消息。返回基础 URL 与记录器。
async fn spawn_fake_engine() -> (String, Arc<EngineRecorder>) {
    let recorder = Arc::new(EngineRecorder::default());
    let state_recorder = Arc::clone(&recorder);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&state_recorder);
        async move {
            let method = request.method().to_string();
            let path = request
                .uri()
                .path_and_query()
                .map(|value| value.to_string())
                .unwrap_or_default();
            let headers = request.headers().clone();
            let bytes = to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            lock(&recorder.calls).push(RecordedCall {
                method: method.clone(),
                path: path.clone(),
                directory_header: headers
                    .get("x-opencode-directory")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                authorization: headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                content_type: headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                body: body.clone(),
            });

            let path_only = request_path_only(&path);
            let session_id = path_only
                .strip_prefix("/session/")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default()
                .to_string();
            match (method.as_str(), path_only.as_str()) {
                ("POST", "/session") => Json(json!({ "id": "ses_123" })).into_response(),
                ("POST", p) if p.ends_with("/prompt_async") => {
                    (StatusCode::NO_CONTENT, "").into_response()
                }
                ("POST", p) if p.ends_with("/command") => Json(json!({})).into_response(),
                ("POST", p) if p.ends_with("/fork") => {
                    Json(json!({ "id": "ses_fork", "title": "Forked session" })).into_response()
                }
                ("GET", p) if p.ends_with("/message") => {
                    let messages = if recorder.has_prompt_async(&session_id) {
                        json!([{ "info": { "id": "msg_landed", "role": "user", "time": { "created": 9 } } }])
                    } else {
                        json!([])
                    };
                    Json(messages).into_response()
                }
                ("GET", "/config/providers") => Json(json!({
                    "providers": [
                        { "id": "openai", "models": [{ "id": "gpt-5.5", "variants": { "high": {} } }] },
                    ],
                }))
                .into_response(),
                ("GET", "/agent") => {
                    Json(json!([{ "name": "build", "mode": "primary" }])).into_response()
                }
                ("GET", "/config") => Json(json!({})).into_response(),
                ("GET", "/command") => Json(json!([])).into_response(),
                ("PATCH", p) if p.starts_with("/session/") => Json(json!({})).into_response(),
                _ => (StatusCode::NOT_FOUND, "").into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), recorder)
}

/// 走生产装配路径构建被测路由：外部 engine 配置 + 测试密码 + 临时
/// data_dir，经 `super::router(ctx)` 组装。
fn production_router(data_dir: &Path, base_url: &str) -> Router {
    let config = ServerConfig {
        port: 0,
        host: None,
        lan: false,
        ui_password: None,
        api_only: false,
        data_dir: data_dir.to_path_buf(),
        dist_dir: data_dir.join("dist"),
        tunnel: Default::default(),
        engine: EngineConfig::External {
            base_url: base_url.to_string(),
        },
    };
    let ctx = RouterContext {
        config: Arc::new(config),
        engine: EngineState::external(base_url.to_string(), Some("test-password".to_string())),
        hub: EventHub::new(),
    };
    super::router(ctx)
}

/// 生产客户端应携带的 Basic 鉴权头（opencode:test-password 的 base64）。
fn expected_auth() -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("opencode:test-password")
    )
}

/// 验证：生产客户端 POST /session 带 directory 表单编码查询、Basic
/// 鉴权、百分号编码的 `x-opencode-directory` 头与 JSON 体。
#[tokio::test]
async fn http_client_posts_session_create_with_directory_query_and_headers() {
    let data_dir = temp_dir("http-create");
    let project_dir = data_dir.join("repo").join("app");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let canonical = std::fs::canonicalize(&project_dir).expect("canonical");
    let directory = canonical.to_string_lossy().to_string();
    let (base_url, recorder) = spawn_fake_engine().await;
    let router = production_router(&data_dir, &base_url);

    let (status, body) = post(
        &router,
        CREATE_URI,
        &json!({ "directory": directory, "title": "Side task" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessionId"], "ses_123");
    assert_eq!(body["directory"], directory);

    let call = recorder
        .find("POST", "/session?")
        .expect("session create call");
    assert_eq!(call.path, format!("/session?{}", form_encode(&directory)));
    assert_eq!(
        call.authorization.as_deref(),
        Some(expected_auth().as_str())
    );
    assert_eq!(
        call.directory_header.as_deref(),
        Some(encode_uri_component(&directory).as_str())
    );
    assert_eq!(call.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        call.body,
        json!({ "directory": directory, "title": "Side task" })
    );
}

/// 验证：生产装配下缺目录与目录不存在分别报 "Directory parameter is
/// required" / "Directory not found"。
#[tokio::test]
async fn production_validation_requires_a_directory() {
    let data_dir = temp_dir("http-nodefault");
    let (base_url, _recorder) = spawn_fake_engine().await;
    let router = production_router(&data_dir, &base_url);
    let (status, body) = post(&router, CREATE_URI, &json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "Directory parameter is required" }));

    let (status, body) = post(
        &router,
        CREATE_URI,
        &json!({ "directory": data_dir.join("does-not-exist").to_string_lossy() }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "Directory not found" }));
}

/// 验证：非 ASCII 目录路径在 `x-opencode-directory` 头中被正确百分号
/// 编码。
#[tokio::test]
async fn http_client_percent_encodes_the_directory_header_for_non_ascii_paths() {
    let data_dir = temp_dir("http-unicode");
    let project_dir = data_dir.join("Masaüstü").join("projeler");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let canonical = std::fs::canonicalize(&project_dir).expect("canonical");
    let directory = canonical.to_string_lossy().to_string();
    let (base_url, recorder) = spawn_fake_engine().await;
    let router = production_router(&data_dir, &base_url);

    let (status, _) = post(
        &router,
        CREATE_URI,
        &json!({ "directory": directory, "title": "Side task" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let call = recorder
        .find("POST", "/session?")
        .expect("session create call");
    assert_eq!(
        call.directory_header.as_deref(),
        Some(encode_uri_component(&directory).as_str())
    );
}

/// 验证：prompt_async 走 engine 的 URL 形状（路径 + directory 查询 +
/// 鉴权/目录头），体含 model/agent/parts，且 selection 校验确实访问了
/// /config/providers。
#[tokio::test]
async fn http_client_dispatches_prompt_async_with_the_engine_url_shape() {
    let data_dir = temp_dir("http-prompt");
    let project_dir = data_dir.join("repo").join("app");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let canonical = std::fs::canonicalize(&project_dir).expect("canonical");
    let directory = canonical.to_string_lossy().to_string();
    let (base_url, recorder) = spawn_fake_engine().await;
    let router = production_router(&data_dir, &base_url);

    let (status, body) = post(
        &router,
        "/api/ompchamber/sessions/ses_source/send",
        &json!({
            "directory": directory,
            "prompt": "Run this",
            "model": "openai/gpt-5.5",
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["promptDispatched"], true);

    let call = recorder
        .find("POST", "/session/ses_source/prompt_async")
        .expect("prompt_async call");
    assert_eq!(
        call.path,
        format!(
            "/session/ses_source/prompt_async?{}",
            form_encode(&directory)
        )
    );
    assert_eq!(
        call.authorization.as_deref(),
        Some(expected_auth().as_str())
    );
    assert_eq!(
        call.directory_header.as_deref(),
        Some(encode_uri_component(&directory).as_str())
    );
    assert_eq!(
        call.body["model"],
        json!({ "providerID": "openai", "modelID": "gpt-5.5" })
    );
    assert_eq!(call.body["agent"], "build");
    assert_eq!(
        call.body["parts"],
        json!([{ "type": "text", "text": "Run this" }])
    );

    // Selection validation consulted the engine config endpoints.
    assert!(
        recorder.find("GET", "/config/providers?").is_some(),
        "selection inputs fetched over HTTP"
    );
}

/// 验证：生产 goal 创建器写目标文件并 PATCH 会话元数据
///（status/tokenBudget/objectiveFile），且 PATCH 先于 prompt 派发。
#[tokio::test]
async fn production_goal_creator_writes_the_objective_file_and_patches_metadata() {
    let data_dir = temp_dir("http-goal");
    let project_dir = data_dir.join("repo").join("app");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let canonical = std::fs::canonicalize(&project_dir).expect("canonical");
    let directory = canonical.to_string_lossy().to_string();
    let (base_url, recorder) = spawn_fake_engine().await;
    let router = production_router(&data_dir, &base_url);

    let (status, body) = post(
        &router,
        CREATE_URI,
        &json!({
            "directory": directory,
            "prompt": "Finish and verify the migration",
            "model": "openai/gpt-5.5",
            "agent": "build",
            "goal": true,
            "goalTokenBudget": 200000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["goalEnabled"], true);
    assert_eq!(body["promptDispatched"], true);

    let patch = recorder
        .find("PATCH", "/session/ses_123")
        .expect("goal patch");
    assert_eq!(
        patch.path,
        format!(
            "/session/ses_123?directory={}",
            encode_uri_component(&directory)
        )
    );
    let goal = &patch.body["metadata"]["ompchamber"]["goal"];
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["tokenBudget"], 200000);
    // The objective fits the inline budget, so it went to the file.
    assert_eq!(goal["objectiveFile"], true);
    assert_eq!(goal["objective"], "");

    // The goal PATCH precedes the prompt dispatch.
    let calls = lock(&recorder.calls);
    let patch_at = calls
        .iter()
        .position(|call| call.method == "PATCH")
        .expect("patch index");
    let prompt_at = calls
        .iter()
        .position(|call| call.method == "POST" && call.path.contains("/prompt_async"))
        .expect("prompt index");
    assert!(patch_at < prompt_at);
}

/// 验证：创建会话后 EventHub 收到 ompchamber:session-created 线框，
/// 属性齐全且不含 projectId。
#[tokio::test]
async fn hub_receives_the_session_created_wire_frame() {
    let data_dir = temp_dir("http-emit");
    let project_dir = data_dir.join("repo").join("app");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let canonical = std::fs::canonicalize(&project_dir).expect("canonical");
    let directory = canonical.to_string_lossy().to_string();

    let hub = EventHub::new();
    let (base_url, _recorder) = spawn_fake_engine().await;
    let config = ServerConfig {
        port: 0,
        host: None,
        lan: false,
        ui_password: None,
        api_only: false,
        data_dir: data_dir.clone(),
        dist_dir: data_dir.join("dist"),
        tunnel: Default::default(),
        engine: EngineConfig::External {
            base_url: base_url.clone(),
        },
    };
    let ctx = RouterContext {
        config: Arc::new(config),
        engine: EngineState::external(base_url, Some("test-password".to_string())),
        hub: Arc::clone(&hub),
    };
    let mut frames = hub.subscribe();
    let router = super::router(ctx);

    let (status, _) = post(
        &router,
        CREATE_URI,
        &json!({ "directory": directory, "title": "Side task" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frame = frames.recv().await.expect("hub frame");
    assert_eq!(frame.event, "ompchamber:session-created");
    let payload: Map<String, Value> = serde_json::from_str(&frame.data).expect("frame json");
    assert_eq!(payload["type"], "ompchamber:session-created");
    let properties = &payload["properties"];
    assert_eq!(properties["sessionId"], "ses_123");
    assert_eq!(properties["directory"], directory);
    assert_eq!(properties["title"], "Side task");
    assert_eq!(properties["promptDispatched"], false);
    assert_eq!(properties["dispatchedAsCommand"], false);
    assert!(properties["createdAt"].is_u64());
    assert!(properties.get("projectId").is_none());
}
