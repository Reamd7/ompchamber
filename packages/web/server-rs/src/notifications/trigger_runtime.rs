//! Port of `server/lib/notifications/runtime.js`: the OpenCode
//! event-driven notification trigger state machine — completion/error/
//! question/permission routing with cooldowns and debounces, subtask
//! suppression via the session parent chain, template resolution with
//! fallbacks, the native push badge set (distinct collapse-ids), and the
//! web-push + APNs fanout with presence-aware routing.

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

const PUSH_READY_COOLDOWN_MS: u64 = 5000;
const PUSH_QUESTION_DEBOUNCE_MS: u64 = 500;
const PUSH_PERMISSION_DEBOUNCE_MS: u64 = 500;
const SESSION_PARENT_CACHE_TTL_MS: u64 = 60 * 1000;

/// Fixed scenario titles for native push (mobile design): no model,
/// project, or message content crosses the relay.
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

pub type AutoAcceptFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
/// `setGetIsSessionAutoAccepting`: the authoritative permission
/// auto-accept resolver (the permission module's instance, when wired).
pub type AutoAcceptResolver = Arc<dyn Fn(&str, Option<&str>) -> AutoAcceptFuture + Send + Sync>;
/// `setGetIsWindowFocused`: the desktop shell focus probe.
pub type WindowFocusedFn = Arc<dyn Fn() -> bool + Send + Sync>;

struct QuestionTimer {
    abort: tokio::task::AbortHandle,
}

struct PermissionTimer {
    abort: tokio::task::AbortHandle,
    request_key: Option<String>,
}

struct ParentCacheEntry {
    parent_id: Option<String>,
    at: u64,
}

#[derive(Default)]
struct TriggerInner {
    /// Distinct collapse-ids pushed since the app was last foregrounded
    /// (the absolute APNs badge — see APNS.md).
    pending_push_tags: HashSet<String>,
    question_timers: HashMap<String, QuestionTimer>,
    permission_timers: HashMap<String, PermissionTimer>,
    notified_permission_requests: HashSet<String>,
    last_ready_at: HashMap<String, u64>,
    last_error_at: HashMap<String, u64>,
    parent_cache: HashMap<String, ParentCacheEntry>,
    /// Sessions the client flagged for Permission Auto-Accept via
    /// `/api/notifications/auto-accept` (the JS fallback set).
    auto_accepting_sessions: HashSet<String>,
}

pub struct TriggerRuntime {
    templates: Arc<TemplateRuntime>,
    emitter: Arc<EmitterRuntime>,
    push: Arc<PushRuntime>,
    apns: Arc<ApnsRuntime>,
    store: Arc<SettingsStore>,
    auto_accept_resolver: Mutex<Option<AutoAcceptResolver>>,
    window_focused: Mutex<Option<WindowFocusedFn>>,
    inner: Mutex<TriggerInner>,
    /// Set at construction so spawned debounce tasks can re-acquire the
    /// shared runtime (`Arc<Self>`) from `&self` methods.
    self_weak: Mutex<Option<std::sync::Weak<TriggerRuntime>>>,
}

fn setting_is_false(settings: &Map<String, Value>, key: &str) -> bool {
    settings.get(key) == Some(&Value::Bool(false))
}

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

fn build_session_deep_link_url(session_id: Option<&str>) -> String {
    match session_id.filter(|id| !id.is_empty()) {
        Some(session_id) => format!("/?session={}", encode_uri_component(session_id)),
        None => "/".to_string(),
    }
}

/// `plan\s*mode` / `build\s*agent` case-insensitive probes.
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

pub struct GoalSettlePush {
    pub session_id: String,
    pub directory: Option<String>,
    pub status: String,
    pub title: String,
    pub body: String,
}

impl TriggerRuntime {
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
    pub fn set_get_is_window_focused(&self, probe: Option<WindowFocusedFn>) {
        *self
            .window_focused
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = probe;
    }

    /// `setGetIsSessionAutoAccepting`.
    pub fn set_get_is_session_auto_accepting(&self, resolver: Option<AutoAcceptResolver>) {
        *self
            .auto_accept_resolver
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = resolver;
    }

    /// `setAutoAcceptSession` (the module's own mirror set).
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

    fn is_window_focused(&self) -> bool {
        self.window_focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|probe| probe())
    }

    /// `clearPendingPushBadge`.
    pub fn clear_pending_push_badge(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_push_tags
            .clear();
    }

    fn track_push_and_count_badge(&self, tag: Option<&str>) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tag) = tag.filter(|tag| !tag.is_empty()) {
            inner.pending_push_tags.insert(tag.to_string());
        }
        inner.pending_push_tags.len() as u64
    }

    /// `toApnsGenericPayload`: fixed title + session name as body, badge =
    /// distinct pending tags, deep-link session id forwarded.
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

    fn parent_cache_key(session_id: &str, directory: Option<&str>) -> String {
        format!("{}\0{}", directory.unwrap_or_default(), session_id)
    }

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

    fn notification_mode_is_not_always_and_focused(&self, settings: &Map<String, Value>) -> bool {
        let mode = settings
            .get("notificationMode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        mode != "always" && self.is_window_focused()
    }

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
    #[cfg(test)]
    pub fn pending_push_badge_for_test(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_push_tags
            .len() as u64
    }

    #[cfg(test)]
    pub fn has_question_timer_for_test(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .question_timers
            .contains_key(session_id)
    }

    #[cfg(test)]
    pub fn has_permission_timer_for_test(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_timers
            .contains_key(session_id)
    }

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

struct ResolvedTemplate {
    title: String,
    body: String,
    apply_body: bool,
    session_name: String,
}

impl TriggerRuntime {
    /// Re-acquire the shared runtime for spawned debounce tasks. Returns
    /// `None` once the runtime has been dropped (the task then no-ops).
    fn this_arc(&self) -> Option<Arc<TriggerRuntime>> {
        self.self_weak
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }
}
