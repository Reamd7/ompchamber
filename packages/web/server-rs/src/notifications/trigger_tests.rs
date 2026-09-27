//! Trigger runtime tests: settings gates, cooldowns, debounces, subtask
//! suppression, presence-aware fanout, and the native push badge model —
//! driven through fake transports and a canned engine seam.
//!
//! 中文概述：触发链路端到端测试。经 build_state 搭建真实
//! NotificationsState（假 HTTP transport + 罐头 engine 应答），直接调用
//! maybe_send_push_for_trigger / send_goal_settle_push，断言 web-push
//! 请求、relay 请求体、SSE 广播帧、冷却/去抖与角标行为。所有用例持
//! ENV_LOCK 串行运行，避免环境变量竞态。

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::config::{EngineConfig, ServerConfig};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;
use crate::notifications::ENV_LOCK;
use crate::notifications::crypto;

/// Canonical base64url auth secret.
/// 中文：即 26 个大写字母的 base64url 编码，形态与 RFC 8291 测试向量
/// 的 auth secret 一致，仅供测试注册订阅时使用。
const AUTH_SECRET_B64: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo";
use crate::notifications::template_runtime::EngineJsonFetch;
use crate::notifications::transport::{HttpPost, HttpPostResponse};
use crate::notifications::{NotificationsState, trigger_runtime::GoalSettlePush};

/// 录制槽：假 transport 写入的 (url, headers, body) 三元组列表，
/// 断言器按 URL 过滤读取。
type Recorded = Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>;

/// 设置或删除环境变量（None 删除）；unsafe 源于 Rust 2024 起
/// `set_var`/`remove_var` 标记为非线程安全（由 ENV_LOCK 串行化兜底）。
fn set_env(name: &str, value: Option<&str>) {
    match value {
        Some(value) => unsafe { std::env::set_var(name, value) },
        None => unsafe { std::env::remove_var(name) },
    }
}

/// RAII 守卫：构造时把 relay 环境指向测试 URL 并清掉干扰项
/// （relay 禁用开关、APNs 环境），析构时还原，防止泄漏到其它用例。
struct RelayEnvGuard(());
/// 构造即完成测试环境的设定。
impl RelayEnvGuard {
/// 设定三个 OMPCHAMBER_* 环境变量并返回守卫。
    fn new() -> Self {
        set_env(
            "OMPCHAMBER_PUSH_RELAY_URL",
            Some("https://relay.test/v1/push/send"),
        );
        set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", None);
        set_env("OMPCHAMBER_APNS_ENVIRONMENT", None);
        RelayEnvGuard(())
    }
}
/// 析构还原：移除本守卫设置的 relay URL。
impl Drop for RelayEnvGuard {
/// 删除 relay URL 环境变量（其余项本就设为 None）。
    fn drop(&mut self) {
        set_env("OMPCHAMBER_PUSH_RELAY_URL", None);
    }
}

/// 为每个用例创建独立临时数据目录（pid + 毫秒时间 + 随机数命名），
/// settings.json 与订阅文件都写在这里，用例间互不干扰。
fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "notif-trigger-{}-{}",
        std::process::id(),
        crypto::now_ms() * 1000 + rand::random::<u64>() % 100_000
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// 构造假 transport：把每次请求原样录进 Recorded，恒返回 201 +
/// `{ok:true, results:[]}`（推送服务的成功响应形状）。
fn recording_transport() -> (HttpPost, Recorded) {
    let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded_for_transport = Arc::clone(&recorded);
    let transport: HttpPost = Arc::new(move |url, headers, body| {
        let recorded = Arc::clone(&recorded_for_transport);
        let url = url.to_string();
        Box::pin(async move {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((url, headers, body));
            Ok(HttpPostResponse {
                status: 201,
                body: serde_json::to_string(&json!({ "ok": true, "results": [] })).unwrap(),
            })
        })
    });
    (transport, recorded)
}

/// Canned engine: per-path JSON answers.
/// 中文：以「(url 前缀, 应答 JSON)」列表模拟 engine HTTP API；
/// 请求 URL 命中第一个前缀即返回对应 JSON，否则 None。
type EngineAnswers = Arc<Mutex<Vec<(String, Value)>>>;

/// 把罐头应答表包装成 EngineJsonFetch 闭包，注入 TemplateRuntime。
fn engine_fetch(answers: EngineAnswers) -> EngineJsonFetch {
    Arc::new(move |url: String, _timeout: u64, _auth: bool| {
        let answers = Arc::clone(&answers);
        Box::pin(async move {
            let answers = answers.lock().unwrap_or_else(|e| e.into_inner());
            answers
                .iter()
                .find(|(prefix, _)| url.starts_with(prefix.as_str()))
                .map(|(_, value)| value.clone())
        })
    })
}

/// 组装完整测试状态：先把 settings.json 落盘（必须在任何 store 读取
/// 之前），再用外部 engine 占位 URL + 录制 transport + 罐头取数构造
/// NotificationsState，返回 (状态, 录制槽)。
fn build_state(
    dir: std::path::PathBuf,
    settings: Map<String, Value>,
    answers: EngineAnswers,
) -> (Arc<NotificationsState>, Recorded) {
    // Write settings before any store reads.
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&Value::Object(settings)).expect("json"),
    )
    .expect("settings");

    let engine = EngineState::external("http://127.0.0.1:1".to_string(), None);
    let ctx = RouterContext {
        config: Arc::new(ServerConfig {
            port: 3998,
            host: None,
            lan: false,
            ui_password: None,
            api_only: false,
            data_dir: dir,
            dist_dir: std::path::PathBuf::from("/tmp/dist"),
            tunnel: Default::default(),
            engine: EngineConfig::External {
                base_url: "http://127.0.0.1:1".to_string(),
            },
        }),
        engine,
        hub: EventHub::new(),
    };
    let events = crate::event_stream::EventStreamState::from_ctx(ctx.clone());
    let (transport, recorded) = recording_transport();
    let state = NotificationsState::new_with(ctx, events, transport, engine_fetch(answers));
    (state, recorded)
}

/// Register one web-push subscription + one APNs token so fanouts land on
/// the recorded transport.
/// 中文：返回 web-push 订阅的 ua 公钥串，供需要手工解密请求体的
/// 调用方使用。
async fn register_targets(state: &NotificationsState) -> String {
    let ua_secret = crypto::generate_secret_key();
    let ua_public = crypto::public_to_b64url(&ua_secret.public_key());
    state
        .push
        .add_or_update_push_subscription(
            "ui",
            "https://push/endpoint",
            &ua_public,
            AUTH_SECRET_B64,
            None,
            None,
        )
        .await;
    state
        .apns
        .add_or_update_apns_token("ui", "apns-token-1", None, Some("ios"), Some("production"))
        .await;
    ua_public
}

/// 构造一条标准的 ready 触发事件：message.updated + finish=stop 的
/// assistant 消息（session 标题、模型、正文均可定制）。
fn ready_payload(session_id: &str, title: &str) -> Value {
    json!({
        "type": "message.updated",
        "properties": {
            "sessionTitle": title,
            "info": {
                "sessionID": session_id,
                "id": "msg-1",
                "role": "assistant",
                "finish": "stop",
                "mode": "build",
                "modelID": "claude-sonnet-4-5",
                "parts": [{ "type": "text", "text": "All done, ship it." }],
            },
        },
    })
}

/// 从录制槽过滤出 relay URL 的请求体并解析为 JSON 数组（APNs 断言用）。
fn relay_send_bodies(recorded: &Recorded) -> Vec<Value> {
    recorded
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(url, _, _)| url == "https://relay.test/v1/push/send")
        .map(|(_, _, body)| serde_json::from_slice::<Value>(body).expect("json"))
        .collect()
}

/// 从录制槽过滤出 web-push endpoint 的原始 (url, headers, body)
/// 三元组（加密体与 VAPID 头断言用）。
fn web_push_posts(recorded: &Recorded) -> Vec<(String, Vec<(String, String)>, Vec<u8>)> {
    recorded
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(url, _, _)| url == "https://push/endpoint")
        .cloned()
        .collect()
}

/// 空 engine 应答表：所有取数返回 None（「engine 无补充信息」路径）。
fn default_answers() -> EngineAnswers {
    Arc::new(Mutex::new(Vec::new()))
}

/// 验证 ready 触发的完整 fanout：web-push 带加密体与 VAPID 头、APNs
/// relay 带默认标题/角标/deep link/collapseId，SSE 帧含模板化标题正文。
#[tokio::test]
async fn ready_trigger_fans_out_web_push_and_the_generic_apns_payload() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let mut settings = Map::new();
    settings.insert("nativeNotificationsEnabled".to_string(), json!(true));
    let (state, recorded) = build_state(temp_dir(), settings, default_answers());
    register_targets(&state).await;
    let mut notification_rx = state.notification_tx.subscribe();

    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_1", "My Session"))
        .await;

    // Web push: encrypted body with the VAPID authorization.
    let pushes = web_push_posts(&recorded);
    assert_eq!(pushes.len(), 1, "one web-push send");
    assert!(
        pushes[0]
            .1
            .iter()
            .any(|(key, value)| key.eq_ignore_ascii_case("authorization")
                && value.starts_with("vapid t="))
    );

    // APNs relay send: generic title + session name, badge 1, deep link.
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1, "one APNs relay send: {sends:?}");
    assert_eq!(sends[0]["title"], json!("Agent response is ready"));
    assert_eq!(sends[0]["body"], json!("My Session"));
    assert_eq!(sends[0]["badge"], json!(1));
    assert_eq!(sends[0]["collapseId"], json!("ready-ses_1"));
    assert_eq!(sends[0]["data"], json!({ "sessionId": "ses_1" }));
    assert_eq!(sends[0]["env"], json!("production"));

    // The native broadcast reached the UI notification channel.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), notification_rx.recv())
        .await
        .expect("frame")
        .expect("frame");
    let parsed: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(parsed["type"], "ompchamber:notification");
    assert_eq!(parsed["properties"]["kind"], json!("ready"));
    assert_eq!(parsed["properties"]["tag"], json!("ready-ses_1"));
    assert_eq!(parsed["properties"]["sessionId"], json!("ses_1"));
    // The settings migration's default completion template resolves with
    // the payload variables (agent_name), replacing the formatMode default.
    assert_eq!(parsed["properties"]["title"], json!("Build is ready"));
    assert_eq!(
        parsed["properties"]["body"],
        json!("Claude Sonnet 4.5 completed the task")
    );
}

/// 验证同一会话连续两次 ready 触发被冷却窗口合并为一次推送。
#[tokio::test]
async fn ready_cooldown_suppresses_the_second_notification() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;

    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_c", "S"))
        .await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_c", "S"))
        .await;
    assert_eq!(relay_send_bodies(&recorded).len(), 1, "cooldown applies");
}

/// 验证 notifyOnCompletion=false 时 ready 触发对所有推送通道静默。
#[tokio::test]
async fn notify_on_completion_false_suppresses_ready() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let mut settings = Map::new();
    settings.insert("notifyOnCompletion".to_string(), json!(false));
    let (state, recorded) = build_state(temp_dir(), settings, default_answers());
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_g", "S"))
        .await;
    assert!(web_push_posts(&recorded).is_empty());
    assert!(relay_send_bodies(&recorded).is_empty());
}

/// 验证 engine 会话仍挂着活跃 goal 时抑制 ready 通知（goal 进行中不算完成）。
#[tokio::test]
async fn active_session_goal_suppresses_ready_notifications() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let answers: EngineAnswers = Arc::new(Mutex::new(vec![(
        "/session/ses_goal".to_string(),
        json!({ "title": "Goal session", "metadata": { "ompchamber": { "goal": { "status": "active" } } } }),
    )]));
    let (state, recorded) = build_state(temp_dir(), Map::new(), answers);
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_goal", "S"))
        .await;
    assert!(relay_send_bodies(&recorded).is_empty());
}

/// 验证 notifyOnSubtasks=false 抑制子会话通知，根会话照常通知。
#[tokio::test]
async fn subtasks_are_suppressed_when_notify_on_subtasks_is_false() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let mut settings = Map::new();
    settings.insert("notifyOnSubtasks".to_string(), json!(false));
    let answers: EngineAnswers = Arc::new(Mutex::new(vec![(
        "/session/ses_sub".to_string(),
        json!({ "parentID": "ses_parent" }),
    )]));
    let (state, recorded) = build_state(temp_dir(), settings, answers);
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_sub", "S"))
        .await;
    assert!(relay_send_bodies(&recorded).is_empty());

    // A root session still notifies (fetch answers parentID absent).
    let answers: EngineAnswers = Arc::new(Mutex::new(vec![(
        "/session/ses_root".to_string(),
        json!({ "title": "Root" }),
    )]));
    let (state, recorded) = build_state(temp_dir(), Map::new(), answers);
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_root", "S"))
        .await;
    assert_eq!(relay_send_bodies(&recorded).len(), 1);
}

/// 验证任一交互客户端可见（前台 UI 在用）时同时抑制 web-push 与 APNs。
#[tokio::test]
async fn visible_interactive_client_skips_the_apns_fanout() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;
    state.push.update_ui_visibility("desk", true, Some("mac"));
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_v", "S"))
        .await;
    // Web push is ALSO suppressed (desktop subscription gate: any visible).
    assert!(web_push_posts(&recorded).is_empty());
    assert!(relay_send_bodies(&recorded).is_empty(), "no APNs push");
}

/// 验证 session.idle 事件被合成为标准 ready 通知（标题正文取 session 名）。
#[tokio::test]
async fn session_idle_synthesizes_a_ready_notification() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "session.idle",
            "properties": {
                "sessionID": "ses_idle",
                "sessionTitle": "Idle session",
                "info": { "sessionID": "ses_idle", "mode": "plan", "modelID": "x" },
            },
        }))
        .await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["title"], json!("Agent response is ready"));
    assert_eq!(sends[0]["body"], json!("Idle session"));
}

/// 验证 finish=error 走错误模板并使用 error-<session> 的 collapseId。
#[tokio::test]
async fn error_finish_uses_the_error_template_and_tag() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;
    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "message.updated",
            "properties": {
                "sessionTitle": "Broken",
                "info": {
                    "sessionID": "ses_err",
                    "role": "assistant",
                    "finish": "error",
                    "parts": [{ "type": "text", "text": "boom **badly**" }],
                },
            },
        }))
        .await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["title"], json!("Agent hit an error"));
    assert_eq!(sends[0]["body"], json!("Broken"));
    assert_eq!(sends[0]["collapseId"], json!("error-ses_err"));
}

/// 验证连续 question.asked 去抖合并为一次推送，计时器触发后清空。
#[tokio::test]
async fn question_debounce_coalesces_rapid_asks() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;

    let payload = json!({
        "type": "question.asked",
        "properties": {
            "sessionID": "ses_q",
            "sessionTitle": "Q session",
            "questions": [{ "header": "Pick one", "question": "  Which option?  " }],
        },
    });
    state.trigger.maybe_send_push_for_trigger(&payload).await;
    assert!(state.trigger.has_question_timer_for_test("ses_q"));
    // A second ask replaces the pending timer (single notification).
    state.trigger.maybe_send_push_for_trigger(&payload).await;

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1, "debounced to one send");
    assert_eq!(sends[0]["title"], json!("Agent needs your input"));
    assert_eq!(sends[0]["body"], json!("Q session"));
    assert_eq!(sends[0]["collapseId"], json!("question-ses_q"));
    assert!(!state.trigger.has_question_timer_for_test("ses_q"));
}

/// 验证 permission.replied 取消同一会话的待发权限通知。
#[tokio::test]
async fn permission_reply_cancels_the_pending_notification() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;

    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "permission.asked",
            "properties": {
                "sessionID": "ses_p",
                "id": "req-1",
                "sessionTitle": "P session",
                "permission": "bash",
            },
        }))
        .await;
    assert!(state.trigger.has_permission_timer_for_test("ses_p"));
    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "permission.replied",
            "properties": { "sessionID": "ses_p", "requestID": "req-1" },
        }))
        .await;
    assert!(!state.trigger.has_permission_timer_for_test("ses_p"));
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        relay_send_bodies(&recorded).is_empty(),
        "reply cancels the push"
    );
}

/// 验证处于自动接受模式的会话不产生 permission 推送。
#[tokio::test]
async fn permission_auto_accept_suppresses_and_dedupes() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;
    state.trigger.set_auto_accept_session("ses_aa", true);

    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "permission.asked",
            "properties": { "sessionID": "ses_aa", "id": "req-9", "permission": "edit" },
        }))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(
        relay_send_bodies(&recorded).is_empty(),
        "auto-accept suppresses"
    );
}

/// 验证 notifyOnQuestion=true 时 permission.asked 触发推送，且
/// collapseId 按 session 作用域（与原生 tag 对齐）。
#[tokio::test]
async fn permission_notification_uses_the_question_toggle_and_fires() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let mut settings = Map::new();
    settings.insert("notifyOnQuestion".to_string(), json!(true));
    let (state, recorded) = build_state(temp_dir(), settings, default_answers());
    register_targets(&state).await;

    state
        .trigger
        .maybe_send_push_for_trigger(&json!({
            "type": "permission.asked",
            "properties": {
                "sessionID": "ses_pp",
                "id": "req-2",
                "permission": "edit",
                "sessionTitle": "Approve me",
            },
        }))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["title"], json!("Agent needs permission"));
    assert_eq!(sends[0]["body"], json!("Approve me"));
    // Push fanout tag is session-scoped (JS parity with the native tag).
    assert_eq!(sends[0]["collapseId"], json!("permission-ses_pp"));
}

/// 验证角标按未读 tag 数累加，engagement 清零后重新从 1 计数。
#[tokio::test]
async fn badge_counts_distinct_tags_and_clears_on_engagement() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;

    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_b1", "S1"))
        .await;
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_b2", "S2"))
        .await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["badge"], json!(1));
    assert_eq!(sends[1]["badge"], json!(2), "distinct tags accumulate");

    state.trigger.clear_pending_push_badge();
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_b3", "S3"))
        .await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends[2]["badge"], json!(1), "badge resets after clearing");
}

/// 验证 goal 结算推送：APNs 正文只放 session 名（内容无关），完整
/// goal 文案仅走 web-push 通道。
#[tokio::test]
async fn goal_settle_push_uses_the_goal_titles() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let answers: EngineAnswers = Arc::new(Mutex::new(vec![(
        "/session/ses_goal2".to_string(),
        json!({ "title": "The goal session" }),
    )]));
    let (state, recorded) = build_state(temp_dir(), Map::new(), answers);
    register_targets(&state).await;

    state
        .trigger
        .send_goal_settle_push(&GoalSettlePush {
            session_id: "ses_goal2".to_string(),
            directory: None,
            status: "complete".to_string(),
            title: "Goal complete".to_string(),
            body: "did the thing".to_string(),
        })
        .await;
    let sends = relay_send_bodies(&recorded);
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["title"], json!("Goal complete"));
    // Native push is content-free: the body is the session name, and the
    // full text travels only on the web-push channel.
    assert_eq!(sends[0]["body"], json!("The goal session"));
    assert_eq!(sends[0]["collapseId"], json!("goal-ses_goal2"));
    assert_eq!(sends[0]["data"], json!({ "sessionId": "ses_goal2" }));
    let pushes = web_push_posts(&recorded);
    assert_eq!(pushes.len(), 1, "goal text reaches web push");
}

/// 验证窗口聚焦抑制推送，但 notificationMode=always 覆盖该门控。
#[tokio::test]
async fn window_focus_suppresses_unless_mode_is_always() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = RelayEnvGuard::new();
    let (state, recorded) = build_state(temp_dir(), Map::new(), default_answers());
    register_targets(&state).await;
    state
        .trigger
        .set_get_is_window_focused(Some(Arc::new(|| true)));
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_f", "S"))
        .await;
    assert!(
        web_push_posts(&recorded).is_empty(),
        "focused window suppresses"
    );

    // 'always' mode overrides the focus gate.
    let mut settings = Map::new();
    settings.insert("notificationMode".to_string(), json!("always"));
    let (state, recorded) = build_state(temp_dir(), settings, default_answers());
    register_targets(&state).await;
    state
        .trigger
        .set_get_is_window_focused(Some(Arc::new(|| true)));
    state
        .trigger
        .maybe_send_push_for_trigger(&ready_payload("ses_f2", "S"))
        .await;
    assert_eq!(web_push_posts(&recorded).len(), 1, "always mode notifies");
}
