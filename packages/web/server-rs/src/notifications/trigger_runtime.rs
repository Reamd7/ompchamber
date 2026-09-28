//! Port of `server/lib/notifications/runtime.js`: the OpenCode
//! event-driven notification trigger state machine — completion/error/
//! question/permission routing with cooldowns and debounces, subtask
//! suppression via the session parent chain, template resolution with
//! fallbacks, the native push badge set (distinct collapse-ids), and the
//! web-push + APNs fanout with presence-aware routing.
//!
//! 中文说明：本模块是 JS 版 runtime.js 的 Rust 移植，为 OpenCode 事件驱动的
//! 通知触发状态机：完成/出错/提问/权限四类事件的路由（带冷却与防抖）、经会话
//! 父链抑制子任务通知、模板解析与回退、原生推送角标（去重 collapse-id 集合），
//! 以及 web-push + APNs 的双通道扇出（按交互客户端可见性路由）。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::notifications::apns_runtime::ApnsRuntime;
use crate::notifications::crypto::now_ms;
use crate::notifications::emitter_runtime::EmitterRuntime;
use crate::notifications::message::prepare_notification_last_message;
use crate::notifications::push_runtime::PushRuntime;
use crate::notifications::template_runtime::{
    TemplateRuntime, encode_uri_component, format_mode, format_model_id,
};
use crate::settings::SettingsStore;

/// 同一会话两次 ready 推送之间的最小冷却间隔。
const PUSH_READY_COOLDOWN_MS: u64 = 5000;
/// question.asked 的防抖等待时长；期间重复提问会重置计时。
const PUSH_QUESTION_DEBOUNCE_MS: u64 = 500;
/// permission.asked 的防抖等待时长；已被回复的请求会取消计时。
const PUSH_PERMISSION_DEBOUNCE_MS: u64 = 500;
/// 会话 parentID 查询结果的内存缓存有效期。
const SESSION_PARENT_CACHE_TTL_MS: u64 = 60 * 1000;

/// Fixed scenario titles for native push (mobile design): no model,
/// project, or message content crosses the relay.
/// 中文：按事件类型返回固定标题、正文只带会话名——模型、项目与消息内容一律
/// 不经 relay 外传。
fn apns_title_for_type(event_type: &str) -> &'static str {
    match event_type {
        "ready" => "Agent response is ready",
        "error" => "Agent hit an error",
        "question" => "Agent needs your input",
        "permission" => "Agent needs permission",
        "goal_complete" => "Goal complete",
        "goal_blocked" => "Goal blocked",
        "goal_budget" => "Goal reached its token budget",
        _ => "Agent update",
    }
}

/// auto-accept 解析器的异步返回类型（装箱 future）。
pub type AutoAcceptFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
/// `setGetIsSessionAutoAccepting`: the authoritative permission
/// auto-accept resolver (the permission module's instance, when wired).
/// 中文：入参为 (session_id, directory)；未接线时回退到内部父链遍历实现。
pub type AutoAcceptResolver = Arc<dyn Fn(&str, Option<&str>) -> AutoAcceptFuture + Send + Sync>;
/// `setGetIsWindowFocused`: the desktop shell focus probe.
/// 中文：返回桌面窗口是否聚焦，用于 notificationMode 非 always 时抑制通知。
pub type WindowFocusedFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// 一个待触发的 question 防抖计时器（可中止）。
struct QuestionTimer {
    /// tokio 任务的中止句柄；同会话新提问到来时先 abort 旧任务。
    abort: tokio::task::AbortHandle,
}

/// 一个待触发的 permission 防抖计时器（可中止，并记录触发源请求）。
struct PermissionTimer {
    /// tokio 任务的中止句柄。
    abort: tokio::task::AbortHandle,
    /// 触发源请求的 session:requestId 键，用于回复时精确取消。
    request_key: Option<String>,
}

/// 会话 parentID 的缓存条目。
struct ParentCacheEntry {
    /// 查到的父会话 ID；Some/None 语义与 fetch 结果一致。
    parent_id: Option<String>,
    /// 写入时刻（毫秒），超过 TTL 视为失效。
    at: u64,
}

/// 触发器的全部可变状态（由一把互斥锁保护）。
#[derive(Default)]
struct TriggerInner {
    /// Distinct collapse-ids pushed since the app was last foregrounded
    /// (the absolute APNs badge — see APNS.md).
    /// 中文：应用回到前台前累计的去重 collapse-id，其数量即 APNs 绝对角标。
    pending_push_tags: HashSet<String>,
    /// 按 session 排队的 question 防抖计时器。
    question_timers: HashMap<String, QuestionTimer>,
    /// 按 session 排队的 permission 防抖计时器。
    permission_timers: HashMap<String, PermissionTimer>,
    /// 已通知过（或因 auto-accept 跳过）的权限请求键，防止重复打扰。
    notified_permission_requests: HashSet<String>,
    /// 各会话最近一次 ready 推送时刻（冷却判据）。
    last_ready_at: HashMap<String, u64>,
    /// 各会话最近一次 error 推送时刻（冷却判据）。
    last_error_at: HashMap<String, u64>,
    /// session 到 parentID 的缓存（带 TTL）。
    parent_cache: HashMap<String, ParentCacheEntry>,
    /// Sessions the client flagged for Permission Auto-Accept via
    /// `/api/notifications/auto-accept` (the JS fallback set).
    /// 中文：与 permission 模块的权威集合互为镜像，仅在解析器未接线时生效。
    auto_accepting_sessions: HashSet<String>,
}

/// 通知触发运行时：消费 OpenCode 事件流，决定何时、以何种文案发出桌面/web/APNs 通知。
pub struct TriggerRuntime {
    /// 模板与 OpenCode API 访问运行时。
    templates: Arc<TemplateRuntime>,
    /// 桌面通知与 UI 广播发射器。
    emitter: Arc<EmitterRuntime>,
    /// web-push（含订阅管理）运行时。
    push: Arc<PushRuntime>,
    /// APNs 原生推送运行时。
    apns: Arc<ApnsRuntime>,
    /// 设置存储（通知开关、模板等）。
    store: Arc<SettingsStore>,
    /// 外部注入的权威 auto-accept 解析器（permission 模块实例）。
    auto_accept_resolver: Mutex<Option<AutoAcceptResolver>>,
    /// 外部注入的桌面窗口聚焦探测。
    window_focused: Mutex<Option<WindowFocusedFn>>,
    /// 可变触发状态。
    inner: Mutex<TriggerInner>,
    /// Set at construction so spawned debounce tasks can re-acquire the
    /// shared runtime (`Arc<Self>`) from `&self` methods.
    /// 中文：防抖任务在 &self 方法里拿不到 Arc，靠 Weak 升级解决生命周期问题。
    self_weak: Mutex<Option<std::sync::Weak<TriggerRuntime>>>,
}

/// JS 语义的"设置显式关闭"：仅当值恰好是布尔 false 才算关闭（缺省视为开启）。
fn setting_is_false(settings: &Map<String, Value>, key: &str) -> bool {
    settings.get(key) == Some(&Value::Bool(false))
}

/// 复刻 JS 真值表：null/缺省为假；非零数字、非空字符串、数组、对象为真。
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// JS `payload.properties?.info?.sessionID ?? … ?? props.session`.
/// 中文：依次尝试 info.sessionID/sessionId、properties 上的同名键与 session，
/// 返回第一个非空字符串。
fn extract_session_id(payload: &Value) -> Option<String> {
    let properties = payload.get("properties")?;
    let info = properties.get("info");
    let candidate = info
        .and_then(|info| info.get("sessionID").or_else(|| info.get("sessionId")))
        .or_else(|| properties.get("sessionID"))
        .or_else(|| properties.get("sessionId"))
        .or_else(|| properties.get("session"));
    candidate
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
}

/// 提取事件的工作目录：优先 properties.directory，其次 properties.info.directory；
/// 空白视为未提供。
fn extract_directory(payload: &Value) -> Option<String> {
    let properties = payload.get("properties")?;
    let directory = properties
        .get("directory")
        .or_else(|| {
            properties
                .get("info")
                .and_then(|info| info.get("directory"))
        })
        .and_then(Value::as_str)?;
    let trimmed = directory.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `getParentIdFromPayload`: `None` means "not a parent-bearing event"
/// (nothing cached); `Some(None)` is a known root session.
/// 中文：仅 session.created/session.updated 事件携带 parentID；用 Option 的两层
/// 嵌套区分"该事件不带此信息"与"已知是根会话"。
fn get_parent_id_from_payload(payload: &Value) -> Option<Option<String>> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    if !matches!(event_type, "session.created" | "session.updated") {
        return None;
    }
    let parent = payload
        .get("properties")
        .and_then(|props| props.get("info"))
        .and_then(|info| info.get("parentID"))
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty());
    Some(parent.map(String::from))
}

/// 构造点击通知后的会话深链（/?session=...，无 id 时回到根路径）。
fn build_session_deep_link_url(session_id: Option<&str>) -> String {
    match session_id.filter(|id| !id.is_empty()) {
        Some(session_id) => format!("/?session={}", encode_uri_component(session_id)),
        None => "/".to_string(),
    }
}

/// `plan\s*mode` / `build\s*agent` case-insensitive probes.
/// 中文：在 header 中查找 first 后跟空白再接 second 的组合（不区分大小写），
/// 用于识别 plan mode / build agent 类提问。
fn header_matches(header: &str, first: &str, second: &str) -> bool {
    let lower = header.to_ascii_lowercase();
    let mut search = 0;
    while let Some(start) = lower[search..].find(first) {
        let after = &lower[search + start + first.len()..];
        let skipped: String = after.chars().take_while(|c| c.is_whitespace()).collect();
        if after[skipped.len()..].starts_with(second) {
            return true;
        }
        search += start + first.len();
    }
    false
}

/// goal 结算推送的输入（goal-runtime 在目标结算后调用）。
pub struct GoalSettlePush {
    /// 目标所在会话。
    pub session_id: String,
    /// 会话所属工作目录（多目录路由用）。
    pub directory: Option<String>,
    /// 结算状态：complete / budgetLimited / 其它均按 blocked 处理。
    pub status: String,
    /// 通知标题。
    pub title: String,
    /// 通知正文。
    pub body: String,
}

/// TriggerRuntime 主实现：事件入口路由、模板解析、防抖计时与推送扇出。
impl TriggerRuntime {
    /// 构造运行时；立即记下自身 Weak 引用，供防抖任务重新拿到 Arc&lt;Self&gt;。
    pub fn new(
        templates: Arc<TemplateRuntime>,
        emitter: Arc<EmitterRuntime>,
        push: Arc<PushRuntime>,
        apns: Arc<ApnsRuntime>,
        store: Arc<SettingsStore>,
    ) -> Arc<Self> {
        let runtime = Arc::new(Self {
            templates,
            emitter,
            push,
            apns,
            store,
            auto_accept_resolver: Mutex::new(None),
            window_focused: Mutex::new(None),
            inner: Mutex::new(TriggerInner::default()),
            self_weak: Mutex::new(None),
        });
        *runtime.self_weak.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::downgrade(&runtime));
        runtime
    }

    /// `setGetIsWindowFocused`.
    /// 中文：注入/移除桌面窗口聚焦探测。
    pub fn set_get_is_window_focused(&self, probe: Option<WindowFocusedFn>) {
        *self
            .window_focused
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = probe;
    }

    /// `setGetIsSessionAutoAccepting`.
    /// 中文：注入/移除权威 auto-accept 解析器；传 None 时回退内部实现。
    pub fn set_get_is_session_auto_accepting(&self, resolver: Option<AutoAcceptResolver>) {
        *self
            .auto_accept_resolver
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = resolver;
    }

    /// `setAutoAcceptSession` (the module's own mirror set).
    /// 中文：维护模块自己的 auto-accept 会话镜像集合（JS 回退路径使用）。
    pub fn set_auto_accept_session(&self, session_id: &str, enabled: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if session_id.is_empty() {
            return;
        }
        if enabled {
            inner.auto_accepting_sessions.insert(session_id.to_string());
        } else {
            inner.auto_accepting_sessions.remove(session_id);
        }
    }

    /// 当前窗口是否聚焦；未注入探测时按不聚焦处理。
    fn is_window_focused(&self) -> bool {
        self.window_focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|probe| probe())
    }

    /// `clearPendingPushBadge`.
    /// 中文：应用回到前台时调用，清空待处理 collapse-id 集合（角标归零）。
    pub fn clear_pending_push_badge(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_push_tags
            .clear();
    }

    /// 记录新的 collapse-id 并返回集合大小，作为 APNs 绝对角标值。
    fn track_push_and_count_badge(&self, tag: Option<&str>) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tag) = tag.filter(|tag| !tag.is_empty()) {
            inner.pending_push_tags.insert(tag.to_string());
        }
        inner.pending_push_tags.len() as u64
    }

    /// `toApnsGenericPayload`: fixed title + session name as body, badge =
    /// distinct pending tags, deep-link session id forwarded.
    /// 中文：native 推送不透传完整文案——标题固定、正文用会话名、角标为待处理
    /// 标签数，data 只带 sessionId 深链。
    fn to_apns_generic_payload(&self, payload: &Value) -> Value {
        let data = payload
            .get("data")
            .filter(|data| data.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let session_name = data
            .get("sessionName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Session")
            .to_string();
        let event_type = data.get("type").and_then(Value::as_str).unwrap_or_default();
        let tag = payload.get("tag").and_then(Value::as_str);
        let badge = self.track_push_and_count_badge(tag);
        let mut result = json!({
            "title": apns_title_for_type(event_type),
            "body": session_name,
            "badge": badge,
            "tag": tag,
        });
        if let Some(session_id) = data.get("sessionId").and_then(Value::as_str) {
            result["data"] = json!({ "sessionId": session_id });
        }
        result
    }

    /// `fanoutPush`: web-push with the full templated payload, native APNs
    /// with generic text — unless an interactive client is already
    /// visible (it shows the in-app notification; skipping also avoids
    /// counting an undelivered push toward the badge).
    /// 中文：web-push 拿完整模板文案；已有可见交互客户端时跳过 APNs——避免与
    /// 站内通知重复，也避免虚增无人接收的角标。
    async fn fanout_push(&self, payload: &Value, require_no_sse: bool) {
        let interactive_visible = self.push.is_any_interactive_client_visible();
        self.push
            .send_push_to_all_ui_sessions(payload, require_no_sse)
            .await;
        if !interactive_visible {
            let apns_payload = self.to_apns_generic_payload(payload);
            self.apns.send_apns_to_all_ui_sessions(&apns_payload).await;
        }
    }

    // -----------------------------------------------------------------------
    // Session parent chain
    // -----------------------------------------------------------------------

    /// 组装父链缓存键：目录 + 会话 ID（同 ID 不同目录不互相污染）。
    fn parent_cache_key(session_id: &str, directory: Option<&str>) -> String {
        format!("{}\0{}", directory.unwrap_or_default(), session_id)
    }

    /// 查询父链缓存；未命中与过期同样返回 None，不区分两者。
    fn get_cached_parent_id(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Option<Option<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = inner
            .parent_cache
            .get(&Self::parent_cache_key(session_id, directory))?;
        if now_ms().saturating_sub(entry.at) > SESSION_PARENT_CACHE_TTL_MS {
            return None;
        }
        Some(entry.parent_id.clone())
    }

    /// 写入父链缓存（带时间戳）。
    fn set_cached_parent_id(
        &self,
        session_id: &str,
        directory: Option<&str>,
        parent_id: Option<String>,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .parent_cache
            .insert(
                Self::parent_cache_key(session_id, directory),
                ParentCacheEntry {
                    parent_id,
                    at: now_ms(),
                },
            );
    }

    /// 顺手从事件中学习 session 到 parent 的映射（session.created/updated 事件）。
    fn maybe_cache_session_parent_from_payload(&self, payload: &Value) {
        let Some(session_id) = extract_session_id(payload) else {
            return;
        };
        let directory = extract_directory(payload);
        if let Some(parent_id) = get_parent_id_from_payload(payload) {
            self.set_cached_parent_id(&session_id, directory.as_deref(), parent_id);
        }
    }

    /// `fetchSessionParentId`: `None` = unknown (fetch failed), `Some(_)`
    /// = known parent chain root answer.
    /// 中文：缓存优先；未命中时带 2s 超时查 /session/{id}。查询失败返回 None
    /// （父链未知），已知根会话返回 Some(None)。
    async fn fetch_session_parent_id(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Option<Option<String>> {
        if session_id.is_empty() {
            return None;
        }
        if let Some(cached) = self.get_cached_parent_id(session_id, directory) {
            return Some(cached);
        }
        let mut url = format!("/session/{}", encode_uri_component(session_id));
        if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
            url.push_str(&format!("?directory={}", encode_uri_component(directory)));
        }
        let session = self.templates.fetch_json(url, 2000, true).await?;
        let parent_id = session
            .get("parentID")
            .and_then(Value::as_str)
            .filter(|parent| !parent.is_empty())
            .map(String::from);
        self.set_cached_parent_id(session_id, directory, parent_id.clone());
        Some(parent_id)
    }

    /// `hasActiveSessionGoal`: a session with an active goal suppresses
    /// per-turn ready notifications (the goal's settle notification is the
    /// final word).
    /// 中文：读取 session.metadata.ompchamber.goal.status，active 即抑制逐轮
    /// ready 通知（结算通知才是最终结论）。
    async fn has_active_session_goal(&self, session_id: &str, directory: Option<&str>) -> bool {
        if session_id.is_empty() {
            return false;
        }
        let mut url = format!("/session/{}", encode_uri_component(session_id));
        if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
            url.push_str(&format!("?directory={}", encode_uri_component(directory)));
        }
        let Some(session) = self.templates.fetch_json(url, 2000, true).await else {
            return false;
        };
        session
            .get("metadata")
            .and_then(|metadata| metadata.get("ompchamber"))
            .and_then(|ompchamber| ompchamber.get("goal"))
            .and_then(|goal| goal.get("status"))
            .and_then(Value::as_str)
            == Some("active")
    }

    /// `isSessionAutoAccepting` (internal fallback): walks the parent
    /// chain; a session auto-accepts if it or any ancestor is flagged.
    /// 中文：沿父链逐级上溯（带环检测），自身或任一祖先被标记即视为 auto-accept。
    async fn is_session_auto_accepting_internal(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> bool {
        if session_id.is_empty() {
            return false;
        }
        if self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_accepting_sessions
            .is_empty()
        {
            return false;
        }
        let mut current = session_id.to_string();
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            if seen.contains(&current) {
                return false;
            }
            if self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .auto_accepting_sessions
                .contains(&current)
            {
                return true;
            }
            seen.insert(current.clone());
            match self.fetch_session_parent_id(&current, directory).await {
                Some(Some(parent)) => current = parent,
                _ => return false,
            }
        }
    }

    /// auto-accept 判定入口：优先外部权威解析器，否则内部父链遍历。
    async fn is_session_auto_accepting(&self, session_id: &str, directory: Option<&str>) -> bool {
        let resolver = self
            .auto_accept_resolver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match resolver {
            Some(resolver) => resolver(session_id, directory).await,
            None => {
                self.is_session_auto_accepting_internal(session_id, directory)
                    .await
            }
        }
    }

    // -----------------------------------------------------------------------
    // Trigger entrypoint
    // -----------------------------------------------------------------------

    /// `maybeSendPushForTrigger`.
    /// 中文：先缓存事件中的父链信息；session.idle/session.error 会合成一条
    /// message.updated 再递归进入；其余事件按类型分流到对应处理器。
    pub async fn maybe_send_push_for_trigger(&self, payload: &Value) {
        if !payload.is_object() {
            return;
        }
        self.maybe_cache_session_parent_from_payload(payload);

        let Some(session_id) = extract_session_id(payload) else {
            return;
        };
        let directory = extract_directory(payload);
        let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

        if matches!(event_type, "session.idle" | "session.error") {
            // Synthesize an assistant message finish event and recurse.
            let error = payload
                .get("properties")
                .and_then(|props| props.get("error"));
            let error_text = match error {
                Some(Value::String(text)) => text.clone(),
                Some(error) => error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_default(),
                None => String::new(),
            };
            let mut info = Map::new();
            info.insert("sessionID".into(), json!(session_id));
            info.insert("role".into(), json!("assistant"));
            info.insert(
                "finish".into(),
                json!(if event_type == "session.error" {
                    "error"
                } else {
                    "stop"
                }),
            );
            if !error_text.is_empty() {
                info.insert(
                    "parts".into(),
                    json!([{ "type": "text", "text": error_text }]),
                );
            }
            let mut properties = payload
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            properties.insert("info".into(), Value::Object(info));
            let synthesized = json!({
                "type": "message.updated",
                "properties": Value::Object(properties),
            });
            Box::pin(self.maybe_send_push_for_trigger(&synthesized)).await;
            return;
        }

        match event_type {
            "message.updated" => {
                self.handle_message_updated(payload, &session_id, directory.as_deref())
                    .await;
            }
            "question.asked" => {
                self.schedule_question_notification(payload, session_id, directory);
            }
            "permission.replied" => {
                self.handle_permission_replied(&session_id, payload);
            }
            "permission.asked" => {
                self.schedule_permission_notification(payload, session_id, directory)
                    .await;
            }
            _ => {}
        }
    }

    /// assistant 消息收尾处理：finish=stop 走完成通知（子任务/开关/活动目标/
    /// 聚焦/冷却逐级过滤），finish=error 走错误通知（开关 + 冷却 + 聚焦过滤）。
    async fn handle_message_updated(
        &self,
        payload: &Value,
        session_id: &str,
        directory: Option<&str>,
    ) {
        let Some(info) = payload
            .get("properties")
            .and_then(|props| props.get("info"))
            .and_then(Value::as_object)
        else {
            return;
        };
        let role = info.get("role").and_then(Value::as_str);
        let finish = info.get("finish").and_then(Value::as_str);

        if role == Some("assistant") && finish == Some("stop") {
            let settings = self.store.read_raw().await;

            if setting_is_false(&settings, "notifyOnSubtasks") {
                let parent_id = match get_parent_id_from_payload(payload) {
                    Some(Some(parent)) => Some(Some(parent)),
                    _ => self.fetch_session_parent_id(session_id, directory).await,
                };
                // Continue only for a KNOWN root session (JS `!== null`;
                // an unknown parent also suppresses).
                if parent_id != Some(None) {
                    return;
                }
            }

            if setting_is_false(&settings, "notifyOnCompletion") {
                return;
            }

            if self.has_active_session_goal(session_id, directory).await {
                return;
            }

            if self.notification_mode_is_not_always_and_focused(&settings) {
                return;
            }

            let now = now_ms();
            let last_at = self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_ready_at
                .get(session_id)
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(last_at) < PUSH_READY_COOLDOWN_MS {
                return;
            }
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_ready_at
                .insert(session_id.to_string(), now);

            let default_title = format!(
                "{} agent is ready",
                format_mode(info.get("mode").and_then(Value::as_str))
            );
            let default_body = format!(
                "{} completed the task",
                format_model_id(info.get("modelID").and_then(Value::as_str))
            );
            let mut title = default_title;
            let mut body = default_body;
            let mut session_name = String::new();

            match self
                .resolve_completion_template(payload, &settings, session_id, directory, info)
                .await
            {
                Ok(resolved) => {
                    session_name = resolved.session_name;
                    if !resolved.title.is_empty() {
                        title = resolved.title;
                    }
                    if resolved.apply_body {
                        body = resolved.body;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "[Notification] Template resolution failed, using defaults: {error}"
                    );
                }
            }

            self.emit_native_notification(
                &json!({
                    "title": title,
                    "body": body,
                    "tag": format!("ready-{session_id}"),
                    "kind": "ready",
                    "sessionId": session_id,
                    "directory": directory,
                    "requireHidden": self.require_hidden(&settings),
                }),
                &settings,
            )
            .await;

            self.fanout_push(
                &json!({
                    "title": title,
                    "body": body,
                    "tag": format!("ready-{session_id}"),
                    "data": {
                        "url": build_session_deep_link_url(Some(session_id)),
                        "sessionId": session_id,
                        "sessionName": session_name,
                        "type": "ready",
                    },
                }),
                true,
            )
            .await;
            return;
        }

        if role == Some("assistant") && finish == Some("error") {
            let settings = self.store.read_raw().await;
            if setting_is_false(&settings, "notifyOnError") {
                return;
            }

            let now = now_ms();
            let last_at = self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_error_at
                .get(session_id)
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(last_at) < PUSH_READY_COOLDOWN_MS {
                return;
            }
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_error_at
                .insert(session_id.to_string(), now);

            if self.notification_mode_is_not_always_and_focused(&settings) {
                return;
            }

            let mut title = "Tool error".to_string();
            let mut body = "An error occurred".to_string();
            let mut session_name = String::new();
            match self
                .resolve_error_template(payload, &settings, session_id, info)
                .await
            {
                Ok(resolved) => {
                    session_name = resolved.session_name;
                    if !resolved.title.is_empty() {
                        title = resolved.title;
                    }
                    if resolved.apply_body {
                        body = resolved.body;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "[Notification] Error template resolution failed, using defaults: {error}"
                    );
                }
            }

            self.emit_native_notification(
                &json!({
                    "title": title,
                    "body": body,
                    "tag": format!("error-{session_id}"),
                    "kind": "error",
                    "sessionId": session_id,
                    "directory": directory,
                    "requireHidden": self.require_hidden(&settings),
                }),
                &settings,
            )
            .await;

            self.fanout_push(
                &json!({
                    "title": title,
                    "body": body,
                    "tag": format!("error-{session_id}"),
                    "data": {
                        "url": build_session_deep_link_url(Some(session_id)),
                        "sessionId": session_id,
                        "sessionName": session_name,
                        "type": "error",
                    },
                }),
                true,
            )
            .await;
        }
    }

    /// notificationMode 非 always 且窗口聚焦时应抑制通知（always 模式不受聚焦影响）。
    fn notification_mode_is_not_always_and_focused(&self, settings: &Map<String, Value>) -> bool {
        let mode = settings
            .get("notificationMode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        mode != "always" && self.is_window_focused()
    }

    /// 桌面通知的 requireHidden 标志：非 always 模式下要求窗口隐藏才弹出。
    fn require_hidden(&self, settings: &Map<String, Value>) -> bool {
        let mode = settings
            .get("notificationMode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        mode != "always"
    }

    /// Native emission uses the branch's settings snapshot (JS reads
    /// `settings.nativeNotificationsEnabled` from the same read that gated
    /// the branch).
    /// 中文：用进入分支时抓取的设置快照判断 nativeNotificationsEnabled（与 JS
    /// 行为一致），再走桌面通知 + UI 广播。
    async fn emit_native_notification(
        &self,
        notification_payload: &Value,
        settings: &Map<String, Value>,
    ) {
        if !js_truthy(settings.get("nativeNotificationsEnabled")) {
            return;
        }
        let delivered = self.emitter.emit_desktop_notification(notification_payload);
        self.emitter
            .broadcast_ui_notification(notification_payload, delivered);
    }

    /// 解析完成通知模板：子任务用 subtask 模板（回退 completion），否则用
    /// completion；均未配置时用内置默认文案。
    async fn resolve_completion_template(
        &self,
        payload: &Value,
        settings: &Map<String, Value>,
        session_id: &str,
        directory: Option<&str>,
        info: &Map<String, Value>,
    ) -> Result<ResolvedTemplate, String> {
        let templates = settings
            .get("notificationTemplates")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let is_subtask = self
            .fetch_session_parent_id(session_id, directory)
            .await
            .is_some_and(|parent| parent.is_some());
        let default = json!({ "title": "{agent_name} is ready", "message": "{model_name} completed the task" });
        let completion_template = if is_subtask && !setting_is_false(settings, "notifyOnSubtasks") {
            templates
                .get("subtask")
                .or_else(|| templates.get("completion"))
                .unwrap_or(&default)
        } else {
            templates.get("completion").unwrap_or(&default)
        };
        self.resolve_question_like_template(
            payload,
            settings,
            completion_template,
            session_id,
            info,
        )
        .await
    }

    /// 解析错误通知模板：未配置 error 模板时回退到 last_message 占位默认。
    async fn resolve_error_template(
        &self,
        payload: &Value,
        settings: &Map<String, Value>,
        session_id: &str,
        info: &Map<String, Value>,
    ) -> Result<ResolvedTemplate, String> {
        let templates = settings
            .get("notificationTemplates")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let default = json!({ "title": "Tool error", "message": "{last_message}" });
        let error_template = templates.get("error").unwrap_or(&default);
        self.resolve_question_like_template(payload, settings, error_template, session_id, info)
            .await
    }

    /// Shared resolution: build variables, extract the last message, then
    /// resolve title/message with the JS fallback rules.
    /// 中文：构造模板变量、提取（必要时拉取）最后一条消息并按上限截断，再按
    /// JS 的回退规则解析 title/message。
    async fn resolve_question_like_template(
        &self,
        payload: &Value,
        settings: &Map<String, Value>,
        template: &Value,
        session_id: &str,
        info: &Map<String, Value>,
    ) -> Result<ResolvedTemplate, String> {
        let mut variables = self
            .templates
            .build_template_variables(payload, session_id)
            .await;
        let session_name = variables
            .get("session_name")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default();

        let message_id = info.get("id").and_then(Value::as_str);
        let mut last_message = self.templates.extract_last_message_text(payload);
        if last_message.is_empty() {
            last_message = self
                .templates
                .fetch_last_assistant_message_text(session_id, message_id)
                .await;
        }
        let max_last_message_length = settings.get("maxLastMessageLength").and_then(Value::as_f64);
        let prepared = prepare_notification_last_message(&last_message, max_last_message_length);
        variables.insert("last_message".into(), Value::String(prepared));

        let template_title = template.get("title").and_then(Value::as_str).unwrap_or("");
        let template_message = template
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let resolved_title = self
            .templates
            .resolve_notification_template(template_title, &variables);
        let resolved_body = self
            .templates
            .resolve_notification_template(template_message, &variables);
        let apply_body = self.templates.should_apply_resolved_template_message(
            template_message,
            &resolved_body,
            &variables,
        );
        Ok(ResolvedTemplate {
            title: resolved_title,
            body: resolved_body,
            apply_body,
            session_name,
        })
    }

    // -----------------------------------------------------------------------
    // Question / permission debounces
    // -----------------------------------------------------------------------

    /// question 防抖：中止同会话旧计时器后重新起 500ms 任务，到点才真正通知。
    fn schedule_question_notification(
        &self,
        payload: &Value,
        session_id: String,
        directory: Option<String>,
    ) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = inner.question_timers.remove(&session_id) {
                existing.abort.abort();
            }
        }
        let payload = payload.clone();
        let this = self.this_arc();
        let timer_session_id = session_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(PUSH_QUESTION_DEBOUNCE_MS)).await;
            if let Some(this) = this {
                this.question_timer_fired(payload, timer_session_id, directory)
                    .await;
            }
        });
        let abort = handle.abort_handle();
        let _ = handle;
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .question_timers
            .insert(session_id, QuestionTimer { abort });
    }

    /// 防抖到期后的提问通知：过设置开关与聚焦过滤，按 header 识别 plan/build
    /// 模式；模板优先、默认文案兜底；桌面通知同步发，推送扇出异步发（对齐 JS
    /// 的 void fanoutPush）。
    async fn question_timer_fired(
        &self,
        payload: Value,
        session_id: String,
        directory: Option<String>,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .question_timers
            .remove(&session_id);
        let directory = directory.as_deref();

        let settings = self.store.read_raw().await;
        if setting_is_false(&settings, "notifyOnQuestion") {
            return;
        }
        if self.notification_mode_is_not_always_and_focused(&settings) {
            return;
        }

        let first_question = payload
            .get("properties")
            .and_then(|props| props.get("questions"))
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
            .cloned()
            .unwrap_or(Value::Null);
        let header = first_question
            .get("header")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        let question_text = first_question
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();

        let mut title = if header_matches(&header, "plan", "mode") {
            "Switch to plan mode".to_string()
        } else if header_matches(&header, "build", "agent") {
            "Switch to build mode".to_string()
        } else if !header.is_empty() {
            header.clone()
        } else {
            "Input needed".to_string()
        };
        let mut body = if !question_text.is_empty() {
            question_text.clone()
        } else {
            "Agent is waiting for your response".to_string()
        };
        let mut session_name = String::new();

        let empty_info = Map::new();
        let info = payload
            .get("properties")
            .and_then(|props| props.get("info"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or(empty_info);
        match self
            .resolve_question_notification_template(
                &payload,
                &settings,
                &session_id,
                &info,
                &question_text,
                &header,
            )
            .await
        {
            Ok(resolved) => {
                session_name = resolved.session_name;
                if !resolved.title.is_empty() {
                    title = resolved.title;
                }
                if resolved.apply_body {
                    body = resolved.body;
                }
            }
            Err(error) => {
                tracing::warn!(
                    "[Notification] Question template resolution failed, using defaults: {error}"
                );
            }
        }

        self.emit_native_notification(
            &json!({
                "kind": "question",
                "title": title,
                "body": body,
                "tag": format!("question-{session_id}"),
                "sessionId": session_id,
                "directory": directory,
                "requireHidden": self.require_hidden(&settings),
            }),
            &settings,
        )
        .await;

        // JS fires this fanout without awaiting (`void fanoutPush`).
        let fanout_payload = json!({
            "title": title,
            "body": body,
            "tag": format!("question-{session_id}"),
            "data": {
                "url": build_session_deep_link_url(Some(&session_id)),
                "sessionId": session_id,
                "sessionName": session_name,
                "type": "question",
            },
        });
        if let Some(this) = self.this_arc() {
            tokio::spawn(async move {
                this.fanout_push(&fanout_payload, true).await;
            });
        }
    }

    /// 提问/权限共用的模板解析：last_message 取问题文本或 header，其余与
    /// completion 路径一致（info 参数保留占位但不参与）。
    async fn resolve_question_notification_template(
        &self,
        payload: &Value,
        settings: &Map<String, Value>,
        session_id: &str,
        _info: &Map<String, Value>,
        question_text: &str,
        header: &str,
    ) -> Result<ResolvedTemplate, String> {
        // `info` is intentionally unused here: question/permission paths
        // derive last_message from the question text or header.
        let templates = settings
            .get("notificationTemplates")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let default = json!({ "title": "Input needed", "message": "{last_message}" });
        let question_template = templates.get("question").unwrap_or(&default);
        let mut variables = self
            .templates
            .build_template_variables(payload, session_id)
            .await;
        let session_name = variables
            .get("session_name")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default();
        let last_message = if !question_text.is_empty() {
            question_text
        } else {
            header
        };
        variables.insert(
            "last_message".into(),
            Value::String(last_message.to_string()),
        );
        let template_title = question_template
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("");
        let template_message = question_template
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let resolved_title = self
            .templates
            .resolve_notification_template(template_title, &variables);
        let resolved_body = self
            .templates
            .resolve_notification_template(template_message, &variables);
        let apply_body = self.templates.should_apply_resolved_template_message(
            template_message,
            &resolved_body,
            &variables,
        );
        Ok(ResolvedTemplate {
            title: resolved_title,
            body: resolved_body,
            apply_body,
            session_name,
        })
    }

    /// permission.replied 处理：待触发计时器的请求键与回复匹配（或任一侧无法
    /// 判定）时中止计时，避免已答复的请求仍弹通知。
    fn handle_permission_replied(&self, session_id: &str, payload: &Value) {
        let properties = payload.get("properties");
        let request_id = properties
            .and_then(|props| props.get("requestID"))
            .or_else(|| properties.and_then(|props| props.get("requestId")))
            .or_else(|| properties.and_then(|props| props.get("id")))
            .and_then(Value::as_str);
        let request_key = request_id.map(|id| format!("{session_id}:{id}"));
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = inner.permission_timers.get(session_id) else {
            return;
        };
        let cancel = match (&request_key, &pending.request_key) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(current), Some(pending)) => current == pending,
        };
        if cancel {
            if let Some(pending) = inner.permission_timers.remove(session_id) {
                pending.abort.abort();
            }
        }
    }

    /// permission 防抖：已通知过的请求直接跳过；auto-accept 会话记键后跳过；
    /// 否则重置同会话 500ms 计时器。
    async fn schedule_permission_notification(
        &self,
        payload: &Value,
        session_id: String,
        directory: Option<String>,
    ) {
        let properties = payload.get("properties");
        let request_id = properties
            .and_then(|props| props.get("id"))
            .or_else(|| properties.and_then(|props| props.get("requestID")))
            .or_else(|| properties.and_then(|props| props.get("requestId")))
            .and_then(Value::as_str);
        let request_key = request_id.map(|id| format!("{session_id}:{id}"));

        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(request_key) = &request_key {
                if inner.notified_permission_requests.contains(request_key) {
                    return;
                }
            }
        }

        // Client may be in Permission Auto-Accept for this session (or an
        // ancestor) — skip the whole notification path.
        if self
            .is_session_auto_accepting(&session_id, directory.as_deref())
            .await
        {
            if let Some(request_key) = &request_key {
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .notified_permission_requests
                    .insert(request_key.clone());
            }
            return;
        }

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = inner.permission_timers.remove(&session_id) {
                existing.abort.abort();
            }
        }

        let payload = payload.clone();
        let task_request_key = request_key.clone();
        let this = self.this_arc();
        let timer_session_id = session_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(PUSH_PERMISSION_DEBOUNCE_MS)).await;
            if let Some(this) = this {
                this.permission_timer_fired(payload, timer_session_id, directory, task_request_key)
                    .await;
            }
        });
        let abort = handle.abort_handle();
        let _ = handle;
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_timers
            .insert(session_id, PermissionTimer { abort, request_key });
    }

    /// 防抖到期后的权限通知：再次校验 auto-accept；文案用 sessionTitle /
    /// permission 兜底；通知后记录请求键防止重复推送。
    async fn permission_timer_fired(
        &self,
        payload: Value,
        session_id: String,
        directory: Option<String>,
        request_key: Option<String>,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_timers
            .remove(&session_id);
        let directory = directory.as_deref();

        if self.is_session_auto_accepting(&session_id, directory).await {
            if let Some(request_key) = &request_key {
                self.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .notified_permission_requests
                    .insert(request_key.clone());
            }
            return;
        }

        let settings = self.store.read_raw().await;
        if setting_is_false(&settings, "notifyOnQuestion") {
            return;
        }
        if self.notification_mode_is_not_always_and_focused(&settings) {
            return;
        }

        let properties = payload.get("properties");
        let session_title = properties
            .and_then(|props| props.get("sessionTitle"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(String::from);
        let permission_text = properties
            .and_then(|props| props.get("permission"))
            .and_then(Value::as_str)
            .filter(|permission| !permission.is_empty())
            .map(String::from);
        let fallback_message = session_title
            .or(permission_text)
            .unwrap_or_else(|| "Agent is waiting for your approval".to_string());

        let mut title = "Permission required".to_string();
        let mut body = fallback_message.clone();
        let mut session_name = String::new();
        let empty_info = Map::new();
        let info = properties
            .and_then(|props| props.get("info"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or(empty_info);
        match self
            .resolve_question_notification_template(
                &payload,
                &settings,
                &session_id,
                &info,
                &fallback_message,
                "",
            )
            .await
        {
            Ok(resolved) => {
                session_name = resolved.session_name;
                if !resolved.title.is_empty() {
                    title = resolved.title;
                }
                if resolved.apply_body {
                    body = resolved.body;
                }
            }
            Err(error) => {
                tracing::warn!(
                    "[Notification] Permission template resolution failed, using defaults: {error}"
                );
            }
        }

        let native_tag = match &request_key {
            Some(request_key) => format!("permission-{request_key}"),
            None => format!("permission-{session_id}"),
        };
        self.emit_native_notification(
            &json!({
                "kind": "permission",
                "title": title,
                "body": body,
                "tag": native_tag,
                "sessionId": session_id,
                "directory": directory,
                "requireHidden": self.require_hidden(&settings),
            }),
            &settings,
        )
        .await;

        if let Some(request_key) = &request_key {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .notified_permission_requests
                .insert(request_key.clone());
        }

        // Push fanout uses the session-scoped tag (JS parity).
        let fanout_payload = json!({
            "title": title,
            "body": body,
            "tag": format!("permission-{session_id}"),
            "data": {
                "url": build_session_deep_link_url(Some(&session_id)),
                "sessionId": session_id,
                "sessionName": session_name,
                "type": "permission",
            },
        });
        if let Some(this) = self.this_arc() {
            tokio::spawn(async move {
                this.fanout_push(&fanout_payload, true).await;
            });
        }
    }

    /// `sendGoalSettlePush`: goal settle fanout (full text to web-push,
    /// generic per-type title + session name to APNs).
    /// 中文：status 映射 goal_complete/goal_budget/goal_blocked 事件类型；先查
    /// 会话名，再走统一的双通道扇出。
    pub async fn send_goal_settle_push(&self, settle: &GoalSettlePush) {
        let mut session_name = String::new();
        let mut url = format!("/session/{}", encode_uri_component(&settle.session_id));
        if let Some(directory) = settle.directory.as_deref().filter(|d| !d.is_empty()) {
            url.push_str(&format!("?directory={}", encode_uri_component(directory)));
        }
        if let Some(session) = self.templates.fetch_json(url, 2000, true).await {
            if let Some(title) = session.get("title").and_then(Value::as_str) {
                session_name = title.trim().to_string();
            }
        }
        let event_type = match settle.status.as_str() {
            "complete" => "goal_complete",
            "budgetLimited" => "goal_budget",
            _ => "goal_blocked",
        };
        self.fanout_push(
            &json!({
                "title": settle.title,
                "body": settle.body,
                "tag": format!("goal-{}", settle.session_id),
                "data": {
                    "url": build_session_deep_link_url(Some(&settle.session_id)),
                    "sessionId": settle.session_id,
                    "sessionName": session_name,
                    "type": event_type,
                },
            }),
            true,
        )
        .await;
    }

    /// Test hooks.
    /// 中文：返回当前待处理角标数（测试断言用）。
    #[cfg(test)]
    pub fn pending_push_badge_for_test(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_push_tags
            .len() as u64
    }

    /// 测试钩子：该会话是否存在待触发的 question 计时器。
    #[cfg(test)]
    pub fn has_question_timer_for_test(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .question_timers
            .contains_key(session_id)
    }

    /// 测试钩子：该会话是否存在待触发的 permission 计时器。
    #[cfg(test)]
    pub fn has_permission_timer_for_test(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_timers
            .contains_key(session_id)
    }

    /// 测试钩子：该会话最近一次 ready 冷却时间戳。
    #[cfg(test)]
    pub fn cooldown_ready_at_for_test(&self, session_id: &str) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_ready_at
            .get(session_id)
            .copied()
    }
}

/// 模板解析结果：解析出的标题/正文、是否采纳正文，以及推送 data 用的会话名。
struct ResolvedTemplate {
    /// 解析后的标题；空串表示沿用默认标题。
    title: String,
    /// 解析后的正文。
    body: String,
    /// JS 回退规则判定是否用解析正文覆盖默认正文。
    apply_body: bool,
    /// 会话名，放入推送 data.sessionName。
    session_name: String,
}

/// 辅助 impl：从 Weak 引用重获共享运行时。
impl TriggerRuntime {
    /// Re-acquire the shared runtime for spawned debounce tasks. Returns
    /// `None` once the runtime has been dropped (the task then no-ops).
    /// 中文：运行时已释放时返回 None，防抖任务随即空转退出。
    fn this_arc(&self) -> Option<Arc<TriggerRuntime>> {
        self.self_weak
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }
}
