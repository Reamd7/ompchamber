//! Port of `server/lib/opencode/session-runtime.js`.
//!
//! Session status/attention/activity state machine fed by OpenCode SSE
//! payloads. Synthetic `ompchamber:session-status` / `ompchamber:session-activity`
//! events are broadcast through the shared [`EventHub`] (the JS
//! `broadcastEvent` path); the per-client SSE fallback (`writeSseEvent`) is
//! unnecessary here because every client stream subscribes to that hub.
//!
//! Cooldown expiry uses JS `setTimeout` semantics; in Rust it is a lazily
//! applied deadline (`tick`) driven by a background sweeper and by every
//! snapshot read, so an expired cooldown always reports `idle`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::hub::EventHub;

pub const SESSION_COOLDOWN_DURATION_MS: u64 = 2000;
pub const SESSION_STATE_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
pub const SESSION_ATTENTION_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
pub const SESSION_ACTIVITY_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityPhaseKind {
    Busy,
    Cooldown,
    Idle,
}

impl ActivityPhaseKind {
    fn as_str(&self) -> &'static str {
        match self {
            ActivityPhaseKind::Busy => "busy",
            ActivityPhaseKind::Cooldown => "cooldown",
            ActivityPhaseKind::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone)]
struct ActivityPhase {
    phase: ActivityPhaseKind,
    updated_at: u64,
}

#[derive(Debug, Clone)]
struct SessionState {
    status: String,
    last_update_at: u64,
    last_event_id: String,
    metadata: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct AttentionState {
    needs_attention: bool,
    last_user_message_at: Option<u64>,
    last_status_change_at: u64,
    viewed_by_clients: HashSet<String>,
    status: String,
}

#[derive(Default)]
struct SessionInner {
    activity_phases: HashMap<String, ActivityPhase>,
    /// session id → cooldown deadline (JS `setTimeout` handle).
    cooldowns: HashMap<String, u64>,
    states: HashMap<String, SessionState>,
    attention_states: HashMap<String, AttentionState>,
    active_session_count: i64,
}

struct SessionStatusUpdate {
    session_id: String,
    status_type: String,
    event_id: String,
    attempt: Option<serde_json::Value>,
    message: Option<String>,
    next: Option<serde_json::Value>,
}

/// One synthetic broadcast: (SSE event name, payload JSON).
type Broadcast = (&'static str, Value);

pub struct SessionRuntime {
    inner: Mutex<SessionInner>,
    broadcast: Arc<EventHub>,
    cooldown_ms: u64,
}

/// `typeof x === 'number'` check preserving the original JSON number form.
fn number_value(value: &Value) -> Option<Value> {
    value.as_number().cloned().map(Value::Number)
}

/// `extractSessionStatusUpdate`: canonical `properties.status.type` with the
/// legacy `properties.info.type` fallback.
fn extract_session_status_update(payload: &Value) -> Option<SessionStatusUpdate> {
    if payload.get("type").and_then(Value::as_str) != Some("session.status") {
        return None;
    }
    let empty = Map::new();
    let properties = match payload.get("properties") {
        Some(Value::Object(map)) => map,
        _ => &empty,
    };
    let status = match properties.get("status") {
        Some(Value::Object(map)) => map,
        _ => &empty,
    };
    let info = match properties.get("info") {
        Some(Value::Object(map)) => map,
        _ => &empty,
    };

    let session_id = properties
        .get("sessionID")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let status_type = status
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .or_else(|| {
            info.get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
        })?;

    Some(SessionStatusUpdate {
        session_id: session_id.to_string(),
        status_type: status_type.to_string(),
        event_id: payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        attempt: status
            .get("attempt")
            .and_then(number_value)
            .or_else(|| info.get("attempt").and_then(number_value)),
        message: status
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                info.get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        next: status
            .get("next")
            .and_then(number_value)
            .or_else(|| info.get("next").and_then(number_value)),
    })
}

impl SessionRuntime {
    pub fn new(broadcast: Arc<EventHub>) -> Arc<Self> {
        Arc::new(Self::with_cooldown_ms(
            broadcast,
            SESSION_COOLDOWN_DURATION_MS,
        ))
    }

    pub(crate) fn with_cooldown_ms(broadcast: Arc<EventHub>, cooldown_ms: u64) -> Self {
        Self {
            inner: Mutex::new(SessionInner::default()),
            broadcast,
            cooldown_ms,
        }
    }

    fn publish(&self, broadcasts: Vec<Broadcast>) {
        for (name, payload) in broadcasts {
            self.broadcast.publish_json(name, &payload);
        }
    }

    /// `processOpenCodeSsePayload`: the SSE ingestion entrypoint.
    pub fn process_opencode_sse_payload(&self, payload: &Value) {
        let Some(update) = extract_session_status_update(payload) else {
            return;
        };
        let now = now_ms();
        let mut broadcasts: Vec<Broadcast> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if update.status_type == "busy" || update.status_type == "retry" {
                set_session_activity_phase(
                    &mut inner,
                    &update.session_id,
                    ActivityPhaseKind::Busy,
                    self.cooldown_ms,
                    now,
                    &mut broadcasts,
                );
            } else if update.status_type == "idle" {
                set_session_activity_phase(
                    &mut inner,
                    &update.session_id,
                    ActivityPhaseKind::Cooldown,
                    self.cooldown_ms,
                    now,
                    &mut broadcasts,
                );
            }

            let event_id = if update.event_id.is_empty() {
                format!("sse-{now}")
            } else {
                update.event_id.clone()
            };
            update_session_state(
                &mut inner,
                &update.session_id,
                &update.status_type,
                &event_id,
                vec![
                    ("attempt", update.attempt),
                    ("message", update.message.map(Value::String)),
                    ("next", update.next),
                ],
                now,
                &mut broadcasts,
            );
        }
        self.publish(broadcasts);
    }

    /// Apply expired cooldowns (JS timer callback). Safe to call any time;
    /// driven by the module sweeper and by snapshot reads.
    pub fn tick(&self) {
        let now = now_ms();
        let mut broadcasts: Vec<Broadcast> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let expired: Vec<String> = inner
                .cooldowns
                .iter()
                .filter(|(_, deadline)| now >= **deadline)
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                inner.cooldowns.remove(&id);
                let phase = inner.activity_phases.get(&id).map(|p| p.phase);
                if phase == Some(ActivityPhaseKind::Cooldown) {
                    set_session_activity_phase(
                        &mut inner,
                        &id,
                        ActivityPhaseKind::Idle,
                        self.cooldown_ms,
                        now,
                        &mut broadcasts,
                    );
                }
            }
        }
        self.publish(broadcasts);
    }

    /// `getSessionActivitySnapshot`: `{ sessionId: { type: phase } }`.
    pub fn get_session_activity_snapshot(&self) -> Map<String, Value> {
        self.tick();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .activity_phases
            .iter()
            .map(|(id, phase)| (id.clone(), json!({ "type": phase.phase.as_str() })))
            .collect()
    }

    pub fn get_active_session_count(&self) -> i64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_session_count
    }

    /// `getSessionStateSnapshot` (24h age filter).
    pub fn get_session_state_snapshot(&self) -> Map<String, Value> {
        let now = now_ms();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .states
            .iter()
            .filter(|(_, state)| {
                now.saturating_sub(state.last_update_at) <= SESSION_STATE_MAX_AGE_MS
            })
            .map(|(id, state)| {
                (
                    id.clone(),
                    json!({
                        "status": state.status,
                        "lastUpdateAt": state.last_update_at,
                        "metadata": Value::Object(state.metadata.clone()),
                    }),
                )
            })
            .collect()
    }

    pub fn get_session_state(&self, session_id: &str) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.states.get(session_id).map(|state| {
            json!({
                "status": state.status,
                "lastUpdateAt": state.last_update_at,
                "lastEventId": state.last_event_id,
                "metadata": Value::Object(state.metadata.clone()),
            })
        })
    }

    /// `getSessionAttentionSnapshot` (24h age filter).
    pub fn get_session_attention_snapshot(&self) -> Map<String, Value> {
        let now = now_ms();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .attention_states
            .iter()
            .filter(|(_, state)| {
                now.saturating_sub(state.last_status_change_at) <= SESSION_ATTENTION_MAX_AGE_MS
            })
            .map(|(id, state)| (id.clone(), attention_snapshot_value(state)))
            .collect()
    }

    pub fn get_session_attention_state(&self, session_id: &str) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .attention_states
            .get(session_id)
            .map(attention_snapshot_value)
    }

    /// `markSessionViewed`: clears `needsAttention` and broadcasts the
    /// resolution.
    pub fn mark_session_viewed(&self, session_id: &str, client_id: &str) {
        let now = now_ms();
        let broadcasts: Vec<Broadcast> = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(state) = get_or_create_attention_state(&mut inner, session_id, now) else {
                return;
            };
            let was_needs_attention = state.needs_attention;
            state.viewed_by_clients.insert(client_id.to_string());
            if !was_needs_attention {
                Vec::new()
            } else {
                state.needs_attention = false;
                vec![(
                    "ompchamber:session-status",
                    json!({
                        "type": "ompchamber:session-status",
                        "properties": {
                            "sessionID": session_id,
                            "status": state.status,
                            "timestamp": now,
                            "metadata": {},
                            "needsAttention": false,
                        },
                    }),
                )]
            }
        };
        self.publish(broadcasts);
    }

    pub fn mark_session_unviewed(&self, session_id: &str, client_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = inner.attention_states.get_mut(session_id) {
            state.viewed_by_clients.remove(client_id);
        }
    }

    pub fn mark_user_message_sent(&self, session_id: &str) {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = get_or_create_attention_state(&mut inner, session_id, now) {
            state.last_user_message_at = Some(now);
        }
    }

    /// `resetAllSessionActivityToIdle`: no per-session broadcasts (JS sets
    /// the map directly).
    pub fn reset_all_session_activity_to_idle(&self) {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cooldowns.clear();
        inner.active_session_count = 0;
        for phase in inner.activity_phases.values_mut() {
            phase.phase = ActivityPhaseKind::Idle;
            phase.updated_at = now;
        }
    }

    /// `interruptBusySessionsAfterRestart`: settles busy/retry sessions to
    /// idle with a restart marker and broadcasts one interruption
    /// notification per session, then resets all activity.
    pub fn interrupt_busy_sessions_after_restart(&self) -> Vec<String> {
        let now = now_ms();
        let event_id = format!("opencode-restart-{now}");
        let mut broadcasts: Vec<Broadcast> = Vec::new();
        let interrupted: Vec<String> = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut ids: BTreeSet<String> = BTreeSet::new();
            for (id, state) in inner.states.iter() {
                if state.status == "busy" || state.status == "retry" {
                    ids.insert(id.clone());
                }
            }
            for (id, activity) in inner.activity_phases.iter() {
                if activity.phase == ActivityPhaseKind::Busy {
                    ids.insert(id.clone());
                }
            }
            let ids: Vec<String> = ids.into_iter().collect();
            for id in &ids {
                update_session_state(
                    &mut inner,
                    id,
                    "idle",
                    &event_id,
                    vec![
                        (
                            "message",
                            Some(Value::String("Interrupted by OpenCode restart".to_string())),
                        ),
                        (
                            "reason",
                            Some(Value::String("opencode-restart".to_string())),
                        ),
                    ],
                    now,
                    &mut broadcasts,
                );
                broadcasts.push((
                    "session.error",
                    json!({
                        "type": "session.error",
                        "properties": {
                            "sessionID": id,
                            "error": {
                                "name": "MessageAbortedError",
                                "message": "The running turn was interrupted when OpenCode restarted.",
                            },
                        },
                    }),
                ));
            }
            // resetAllSessionActivityToIdle (inline, no broadcasts).
            inner.cooldowns.clear();
            inner.active_session_count = 0;
            for phase in inner.activity_phases.values_mut() {
                phase.phase = ActivityPhaseKind::Idle;
                phase.updated_at = now;
            }
            ids
        };
        self.publish(broadcasts);
        interrupted
    }

    /// `cleanupOldSessionStates` (JS runs it hourly from a setInterval).
    pub fn cleanup_old_session_states(&self) {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.states.retain(|_, state| {
            now.saturating_sub(state.last_update_at) <= SESSION_STATE_MAX_AGE_MS
        });
        inner.attention_states.retain(|_, state| {
            now.saturating_sub(state.last_status_change_at) <= SESSION_ATTENTION_MAX_AGE_MS
        });
        let expired_activity: Vec<String> = inner
            .activity_phases
            .iter()
            .filter(|(_, phase)| now.saturating_sub(phase.updated_at) > SESSION_ACTIVITY_MAX_AGE_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired_activity {
            let phase = inner.activity_phases.remove(&id);
            inner.cooldowns.remove(&id);
            if phase.is_some_and(|p| p.phase == ActivityPhaseKind::Busy) {
                inner.active_session_count = (inner.active_session_count - 1).max(0);
            }
        }
    }
    pub fn dispose(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cooldowns.clear();
        inner.activity_phases.clear();
        inner.states.clear();
        inner.attention_states.clear();
        inner.active_session_count = 0;
    }
}

fn attention_snapshot_value(state: &AttentionState) -> Value {
    json!({
        "needsAttention": state.needs_attention,
        "lastUserMessageAt": state.last_user_message_at,
        "lastStatusChangeAt": state.last_status_change_at,
        "status": state.status,
        "isViewed": !state.viewed_by_clients.is_empty(),
    })
}

fn get_or_create_attention_state<'a>(
    inner: &'a mut SessionInner,
    session_id: &str,
    now: u64,
) -> Option<&'a mut AttentionState> {
    if session_id.is_empty() {
        return None;
    }
    Some(
        inner
            .attention_states
            .entry(session_id.to_string())
            .or_insert_with(|| AttentionState {
                needs_attention: false,
                last_user_message_at: None,
                last_status_change_at: now,
                viewed_by_clients: HashSet::new(),
                status: "idle".to_string(),
            }),
    )
}

/// `setSessionActivityPhase`: idempotent transitions with an active-count
/// invariant and a `ompchamber:session-activity` broadcast per accepted
/// transition. `cooldown` is only reachable from `busy`.
fn set_session_activity_phase(
    inner: &mut SessionInner,
    session_id: &str,
    phase: ActivityPhaseKind,
    cooldown_ms: u64,
    now: u64,
    broadcasts: &mut Vec<Broadcast>,
) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let current = inner.activity_phases.get(session_id).map(|p| p.phase);
    if current == Some(phase) {
        return false;
    }
    if phase == ActivityPhaseKind::Cooldown && current != Some(ActivityPhaseKind::Busy) {
        return false;
    }

    inner.cooldowns.remove(session_id);
    let was_active = current == Some(ActivityPhaseKind::Busy);
    let is_active = phase == ActivityPhaseKind::Busy;
    if was_active != is_active {
        inner.active_session_count = if is_active {
            inner.active_session_count + 1
        } else {
            (inner.active_session_count - 1).max(0)
        };
    }
    inner.activity_phases.insert(
        session_id.to_string(),
        ActivityPhase {
            phase,
            updated_at: now,
        },
    );

    if phase == ActivityPhaseKind::Cooldown {
        inner
            .cooldowns
            .insert(session_id.to_string(), now.saturating_add(cooldown_ms));
    }

    broadcasts.push((
        "ompchamber:session-activity",
        json!({
            "type": "ompchamber:session-activity",
            "properties": { "sessionId": session_id, "phase": phase.as_str() },
        }),
    ));
    true
}

/// `updateSessionState`: 5s same-status dedupe, metadata merge, attention
/// transition, conditional `ompchamber:session-status` broadcast, then the
/// activity phase.
fn update_session_state(
    inner: &mut SessionInner,
    session_id: &str,
    status: &str,
    event_id: &str,
    metadata_updates: Vec<(&str, Option<Value>)>,
    now: u64,
    broadcasts: &mut Vec<Broadcast>,
) {
    if session_id.is_empty() {
        return;
    }
    let existing = inner.states.get(session_id).cloned();
    let is_restart_interruption = metadata_updates.iter().any(|(key, value)| {
        *key == "reason" && matches!(value, Some(Value::String(s)) if s == "opencode-restart")
    });
    if let Some(existing) = &existing
        && existing.last_update_at > now.saturating_sub(5000)
        && status == existing.status
        && !is_restart_interruption
    {
        return;
    }

    // `{...existing?.metadata, ...metadata}` — an undefined value removes
    // the key from the serialized form, so None drops it.
    let mut metadata = existing
        .as_ref()
        .map(|state| state.metadata.clone())
        .unwrap_or_default();
    for (key, value) in metadata_updates {
        match value {
            Some(value) => {
                metadata.insert(key.to_string(), value);
            }
            None => {
                metadata.remove(key);
            }
        }
    }

    let last_event_id = if event_id.is_empty() {
        format!("server-{now}")
    } else {
        event_id.to_string()
    };
    inner.states.insert(
        session_id.to_string(),
        SessionState {
            status: status.to_string(),
            last_update_at: now,
            last_event_id,
            metadata,
        },
    );

    // Attention state update.
    let attention_changed = match get_or_create_attention_state(inner, session_id, now) {
        None => false,
        Some(state) => {
            let prev_status = state.status.clone();
            let previous_needs_attention = state.needs_attention;
            state.status = status.to_string();
            state.last_status_change_at = now;
            if (prev_status == "busy" || prev_status == "retry")
                && status == "idle"
                && state.last_user_message_at.is_some()
                && state.viewed_by_clients.is_empty()
            {
                state.needs_attention = true;
            }
            state.needs_attention != previous_needs_attention
        }
    };

    let should_broadcast = existing.is_none()
        || existing.as_ref().map(|state| state.status.as_str()) != Some(status)
        || attention_changed
        || is_restart_interruption;
    if should_broadcast {
        let state = inner.states.get(session_id);
        let needs_attention = inner
            .attention_states
            .get(session_id)
            .map(|s| s.needs_attention)
            .unwrap_or(false);
        broadcasts.push((
            "ompchamber:session-status",
            json!({
                "type": "ompchamber:session-status",
                "properties": {
                    "sessionID": session_id,
                    "status": state.map(|s| s.status.clone()).unwrap_or_else(|| status.to_string()),
                    "timestamp": state.map(|s| s.last_update_at).unwrap_or(now),
                    "metadata": state.map(|s| Value::Object(s.metadata.clone())).unwrap_or_else(|| Value::Object(Map::new())),
                    "needsAttention": needs_attention,
                },
            }),
        ));
    }

    let phase = if status == "busy" || status == "retry" {
        ActivityPhaseKind::Busy
    } else {
        ActivityPhaseKind::Idle
    };
    if phase != ActivityPhaseKind::Idle
        || inner.activity_phases.get(session_id).map(|p| p.phase)
            != Some(ActivityPhaseKind::Cooldown)
    {
        set_session_activity_phase(inner, session_id, phase, 0, now, broadcasts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    /// Test harness capturing broadcasts instead of relying on the shared
    /// EventHub wiring.
    struct Recorder {
        rx: broadcast::Receiver<crate::hub::HubEvent>,
    }

    impl Recorder {
        fn new() -> (Self, Arc<EventHub>) {
            let hub = EventHub::new();
            let rx = hub.subscribe();
            (Self { rx }, hub)
        }

        fn drain(&mut self) -> Vec<(String, Value)> {
            let mut out = Vec::new();
            while let Ok(event) = self.rx.try_recv() {
                if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                    out.push((event.event, value));
                }
            }
            out
        }
    }

    fn status_payload(session_id: &str, status_type: &str) -> Value {
        json!({
            "type": "session.status",
            "id": format!("evt-{session_id}-{status_type}"),
            "properties": {
                "sessionID": session_id,
                "status": { "type": status_type },
            },
        })
    }

    fn legacy_status_payload(session_id: &str, status_type: &str) -> Value {
        json!({
            "type": "session.status",
            "properties": {
                "sessionID": session_id,
                "info": { "type": status_type, "attempt": 2, "message": "retrying" },
            },
        })
    }

    #[test]
    fn ignores_non_status_payloads() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime
            .process_opencode_sse_payload(&json!({ "type": "session.updated", "properties": {} }));
        runtime.process_opencode_sse_payload(&json!({ "type": "session.status", "properties": { "sessionID": "", "status": { "type": "busy" } } }));
        assert!(recorder.drain().is_empty());
        assert!(runtime.get_session_activity_snapshot().is_empty());
    }

    #[test]
    fn busy_transitions_broadcast_activity_and_status() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));

        let events = recorder.drain();
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ompchamber:session-activity", "ompchamber:session-status"]
        );
        assert_eq!(
            events[0].1,
            json!({ "type": "ompchamber:session-activity", "properties": { "sessionId": "ses_1", "phase": "busy" } })
        );
        assert_eq!(events[1].1["properties"]["status"], "busy");
        assert_eq!(runtime.get_active_session_count(), 1);
        assert_eq!(
            runtime.get_session_activity_snapshot()["ses_1"]["type"],
            "busy"
        );
    }

    #[test]
    fn same_status_within_dedupe_window_is_suppressed() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        recorder.drain();
        // Second busy within 5s: dedupe suppresses the status broadcast (the
        // activity phase is idempotent too).
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        assert!(recorder.drain().is_empty());
    }

    #[test]
    fn idle_moves_through_cooldown_then_idle() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::with_cooldown_ms(hub, 40);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        recorder.drain();
        assert_eq!(runtime.get_active_session_count(), 1);

        runtime.process_opencode_sse_payload(&status_payload("ses_1", "idle"));
        let events = recorder.drain();
        let phases: Vec<&str> = events
            .iter()
            .filter(|(name, _)| name == "ompchamber:session-activity")
            .map(|(_, value)| value["properties"]["phase"].as_str().unwrap())
            .collect();
        assert_eq!(phases, vec!["cooldown"]);
        assert_eq!(runtime.get_active_session_count(), 0);
        assert_eq!(
            runtime.get_session_activity_snapshot()["ses_1"]["type"],
            "cooldown"
        );

        std::thread::sleep(std::time::Duration::from_millis(60));
        // Snapshot reads tick, so the expired cooldown reports idle.
        assert_eq!(
            runtime.get_session_activity_snapshot()["ses_1"]["type"],
            "idle"
        );
        let events = recorder.drain();
        let phases: Vec<&str> = events
            .iter()
            .filter(|(name, _)| name == "ompchamber:session-activity")
            .map(|(_, value)| value["properties"]["phase"].as_str().unwrap())
            .collect();
        assert_eq!(phases, vec!["idle"]);
    }

    #[test]
    fn idle_without_busy_does_not_enter_cooldown() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "idle"));
        let events = recorder.drain();
        assert!(
            events
                .iter()
                .all(|(_, value)| value["properties"]["phase"] != "cooldown")
        );
        assert_eq!(
            runtime.get_session_activity_snapshot()["ses_1"]["type"],
            "idle"
        );
    }

    #[test]
    fn keeps_idempotent_active_count_across_sessions() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        runtime.process_opencode_sse_payload(&status_payload("ses_2", "busy"));
        assert_eq!(runtime.get_active_session_count(), 2);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "idle"));
        runtime.process_opencode_sse_payload(&status_payload("ses_2", "idle"));
        assert_eq!(runtime.get_active_session_count(), 0);
    }

    #[test]
    fn flags_needs_attention_when_unviewed_session_goes_idle() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        runtime.mark_user_message_sent("ses_1");
        recorder.drain();

        runtime.process_opencode_sse_payload(&status_payload("ses_1", "idle"));
        let attention = runtime
            .get_session_attention_state("ses_1")
            .expect("attention");
        assert_eq!(attention["needsAttention"], true);
        assert_eq!(attention["status"], "idle");
        assert_eq!(attention["isViewed"], false);
        // The attention flip re-broadcasts the session status.
        let events = recorder.drain();
        assert!(
            events
                .iter()
                .any(|(_, value)| value["properties"]["needsAttention"] == true
                    && value["properties"]["sessionID"] == "ses_1")
        );

        runtime.mark_session_viewed("ses_1", "client-1");
        let attention = runtime
            .get_session_attention_state("ses_1")
            .expect("attention");
        assert_eq!(attention["needsAttention"], false);
        assert_eq!(attention["isViewed"], true);
        let events = recorder.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["properties"]["needsAttention"], false);
        assert_eq!(events[0].1["properties"]["metadata"], json!({}));

        // Unviewing again does not re-flag attention on its own.
        runtime.mark_session_unviewed("ses_1", "client-1");
        assert_eq!(
            runtime.get_session_attention_state("ses_1").unwrap()["isViewed"],
            false
        );
        assert!(recorder.drain().is_empty());
    }

    #[test]
    fn viewed_sessions_do_not_flag_attention() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        runtime.mark_user_message_sent("ses_1");
        runtime.mark_session_viewed("ses_1", "client-1");
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "idle"));
        assert_eq!(
            runtime.get_session_attention_state("ses_1").unwrap()["needsAttention"],
            false
        );
    }

    #[test]
    fn legacy_info_type_fallback_is_supported() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&legacy_status_payload("ses_1", "retry"));
        assert_eq!(
            runtime.get_session_state_snapshot()["ses_1"]["status"],
            "retry"
        );
        assert_eq!(
            runtime.get_session_activity_snapshot()["ses_1"]["type"],
            "busy"
        );
        let state = runtime.get_session_state("ses_1").expect("state");
        assert_eq!(state["metadata"]["attempt"], json!(2));
        assert_eq!(state["metadata"]["message"], "retrying");
    }

    #[test]
    fn interrupt_busy_sessions_after_restart_settles_and_broadcasts() {
        let (mut recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        runtime.process_opencode_sse_payload(&status_payload("ses_2", "busy"));
        runtime.process_opencode_sse_payload(&status_payload("ses_3", "idle"));
        recorder.drain();

        let interrupted = runtime.interrupt_busy_sessions_after_restart();
        assert_eq!(interrupted, vec!["ses_1".to_string(), "ses_2".to_string()]);

        let snapshot = runtime.get_session_state_snapshot();
        assert_eq!(snapshot["ses_1"]["status"], "idle");
        assert_eq!(snapshot["ses_1"]["metadata"]["reason"], "opencode-restart");
        assert_eq!(
            snapshot["ses_1"]["metadata"]["message"],
            "Interrupted by OpenCode restart"
        );

        let activity = runtime.get_session_activity_snapshot();
        assert_eq!(activity["ses_1"]["type"], "idle");
        assert_eq!(activity["ses_2"]["type"], "idle");
        assert_eq!(runtime.get_active_session_count(), 0);

        let events = recorder.drain();
        let errors: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "session.error")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(errors.len(), 2, "one session.error per interrupted session");
        assert_eq!(errors[0]["properties"]["sessionID"], "ses_1");
        assert_eq!(
            errors[0]["properties"]["error"]["name"],
            "MessageAbortedError"
        );
        // Restart interruptions bypass the 5s dedupe: status re-broadcast.
        assert!(
            events
                .iter()
                .any(|(name, value)| name == "ompchamber:session-status"
                    && value["properties"]["sessionID"] == "ses_1"
                    && value["properties"]["metadata"]["reason"] == "opencode-restart")
        );
    }

    #[test]
    fn interrupt_picks_up_activity_only_sessions() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        // A busy activity phase without a session state entry still counts.
        runtime.process_opencode_sse_payload(&legacy_status_payload("ses_9", "busy"));
        let interrupted = runtime.interrupt_busy_sessions_after_restart();
        assert_eq!(interrupted, vec!["ses_9".to_string()]);
    }

    #[test]
    fn cleanup_and_dispose_release_state() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        runtime.process_opencode_sse_payload(&status_payload("ses_1", "busy"));
        runtime.cleanup_old_session_states();
        assert!(!runtime.get_session_state_snapshot().is_empty());
        runtime.dispose();
        assert!(runtime.get_session_state_snapshot().is_empty());
        assert!(runtime.get_session_activity_snapshot().is_empty());
        assert_eq!(runtime.get_active_session_count(), 0);
    }
}
