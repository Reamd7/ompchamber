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
//!
//! 中文概要：按会话 id 维护三类状态——活动相位（busy → cooldown → idle）、
//! 会话状态（status 与合并后的 metadata）、关注状态（needsAttention 与已读
//! 客户端集合）。busy/retry 进入 busy 相位，idle 先落入 cooldown（默认 2 秒）
//! 再由 tick 惰性转到 idle；状态更新带 5 秒同状态去重，迁移通过
//! ompchamber:session-status / ompchamber:session-activity 合成事件广播。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::hub::EventHub;

/// busy 结束后的冷却时长（毫秒）：idle 状态先呈现为 cooldown 相位，到期后才转为 idle。
pub const SESSION_COOLDOWN_DURATION_MS: u64 = 2000;
/// 会话状态条目的最大保留时长（24 小时），过期后由 cleanup_old_session_states 清除。
pub const SESSION_STATE_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
/// 关注状态条目的最大保留时长（24 小时）。
pub const SESSION_ATTENTION_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
/// 活动相位条目的最大保留时长（24 小时）。
pub const SESSION_ACTIVITY_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// 当前 Unix 时间戳（毫秒）；系统时钟早于 UNIX_EPOCH 时兜底返回 0。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 会话活动相位：busy（执行中）→ cooldown（刚结束的冷却窗口）→ idle（空闲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityPhaseKind {
    /// 会话正在执行（对应 status busy/retry）。
    Busy,
    /// busy 刚结束后的冷却窗口，只能从 busy 迁入，到期后转为 idle。
    Cooldown,
    /// 空闲：冷却结束或从未活动。
    Idle,
}

/// 活动相位的 wire 字符串编码。
impl ActivityPhaseKind {
    /// 返回与 JS 一致的相位字符串（"busy" / "cooldown" / "idle"）。
    fn as_str(&self) -> &'static str {
        match self {
            ActivityPhaseKind::Busy => "busy",
            ActivityPhaseKind::Cooldown => "cooldown",
            ActivityPhaseKind::Idle => "idle",
        }
    }
}

/// 单个会话的当前活动相位及最后迁移时间。
#[derive(Debug, Clone)]
struct ActivityPhase {
    /// 当前相位。
    phase: ActivityPhaseKind,
    /// 最近一次相位迁移的毫秒时间戳（24 小时过期清理的依据）。
    updated_at: u64,
}

/// 会话状态条目：最近一次 status 及合并后的元数据。
#[derive(Debug, Clone)]
struct SessionState {
    /// 最近一次状态字符串（busy/idle/retry 等）。
    status: String,
    /// 状态最后更新时间戳（5 秒去重窗口与 24 小时过期的依据）。
    last_update_at: u64,
    /// 触发该状态的 SSE 事件 id；缺失时由调用方合成。
    last_event_id: String,
    /// 合并后的元数据（attempt/message/next/reason 等；None 值更新会删除键）。
    metadata: Map<String, Value>,
}

/// 会话“关注”状态：用户是否需要回头看这个会话。
#[derive(Debug, Clone)]
struct AttentionState {
    /// 是否待关注：用户发过消息、会话从 busy/retry 落 idle 且尚无客户端查看时置位。
    needs_attention: bool,
    /// 最近一条用户消息时间戳；None 表示本进程尚未见过该会话的用户消息。
    last_user_message_at: Option<u64>,
    /// 状态最后变化时间戳（关注快照的 24 小时过期依据）。
    last_status_change_at: u64,
    /// 已标记查看的客户端 id 集合；非空即 isViewed = true。
    viewed_by_clients: HashSet<String>,
    /// 最近一次状态字符串（随关注快照一起暴露）。
    status: String,
}

/// 受 Mutex 保护的全部可变会话状态（JS 版为模块级单例）。
#[derive(Default)]
struct SessionInner {
    /// session id → 当前活动相位。
    activity_phases: HashMap<String, ActivityPhase>,
    /// session id → cooldown deadline (JS `setTimeout` handle).
    /// 到期由 tick 统一应用（对应 JS 的 setTimeout 回调语义）。
    cooldowns: HashMap<String, u64>,
    /// session id → 会话状态（status + metadata）。
    states: HashMap<String, SessionState>,
    /// session id → 关注状态。
    attention_states: HashMap<String, AttentionState>,
    /// 当前处于 busy 相位的会话数：随相位进出 busy 增减，钳制下限 0。
    active_session_count: i64,
}

/// 从一条 session.status SSE 载荷中提取出的结构化更新。
struct SessionStatusUpdate {
    /// 目标会话 id（properties.sessionID，trim 后非空）。
    session_id: String,
    /// 状态类型字符串（canonical status.type，回退 legacy info.type）。
    status_type: String,
    /// 触发更新的 SSE 事件 id，可为空字符串。
    event_id: String,
    /// 重试序号；仅当值为 JSON 数字时保留。
    attempt: Option<serde_json::Value>,
    /// 状态附加消息文本。
    message: Option<String>,
    /// 下一步提示；仅当值为 JSON 数字时保留。
    next: Option<serde_json::Value>,
}

/// One synthetic broadcast: (SSE event name, payload JSON).
/// 在锁内累积、锁外统一发布。
type Broadcast = (&'static str, Value);

/// 会话运行时：持有全部会话状态，负责合成事件广播与生命周期清理。
pub struct SessionRuntime {
    /// 受锁保护的全部可变状态。
    inner: Mutex<SessionInner>,
    /// 共享事件总线：所有客户端 SSE 流都订阅它，因此无需 per-client 回写。
    broadcast: Arc<EventHub>,
    /// busy → cooldown 的冷却时长（毫秒）；生产默认 2000，测试可注入。
    cooldown_ms: u64,
}

/// `typeof x === 'number'` check preserving the original JSON number form.
/// 仅 JSON 数字通过，其余类型（含字符串数字）返回 None。
fn number_value(value: &Value) -> Option<Value> {
    value.as_number().cloned().map(Value::Number)
}

/// `extractSessionStatusUpdate`: canonical `properties.status.type` with the
/// legacy `properties.info.type` fallback.
/// 载荷 type 必须为 "session.status"；sessionID 与状态类型（status.type 回退
/// info.type）任一为空即返回 None。
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

/// 会话运行时公开 API：SSE 摄入、各类快照读取、查看标记与生命周期清理。
impl SessionRuntime {
    /// 以默认冷却时长构造运行时；返回 Arc 便于跨任务共享。
    pub fn new(broadcast: Arc<EventHub>) -> Arc<Self> {
        Arc::new(Self::with_cooldown_ms(
            broadcast,
            SESSION_COOLDOWN_DURATION_MS,
        ))
    }

    /// 以自定义冷却时长构造（测试注入用）；初始状态为空。
    pub(crate) fn with_cooldown_ms(broadcast: Arc<EventHub>, cooldown_ms: u64) -> Self {
        Self {
            inner: Mutex::new(SessionInner::default()),
            broadcast,
            cooldown_ms,
        }
    }

    /// 把锁内累积的合成广播逐条发布到 EventHub；必须在释放锁之后调用。
    fn publish(&self, broadcasts: Vec<Broadcast>) {
        for (name, payload) in broadcasts {
            self.broadcast.publish_json(name, &payload);
        }
    }

    /// `processOpenCodeSsePayload`: the SSE ingestion entrypoint.
    /// busy/retry 载荷先置 busy 相位、idle 载荷置 cooldown；事件 id 缺失时合成
    /// "sse-{now}"；锁内累积的广播在锁外一次性发布。
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
    /// 先在锁内收集到期冷却并把 cooldown 会话迁到 idle，再在锁外发布广播；
    /// 任意时刻调用都安全。
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
    /// 形如 { sessionId: { "type": phase } }；读取前先 tick，使过期冷却呈现为 idle。
    pub fn get_session_activity_snapshot(&self) -> Map<String, Value> {
        self.tick();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .activity_phases
            .iter()
            .map(|(id, phase)| (id.clone(), json!({ "type": phase.phase.as_str() })))
            .collect()
    }

    /// 当前处于 busy 相位的会话数量（跨会话幂等计数）。
    pub fn get_active_session_count(&self) -> i64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_session_count
    }

    /// `getSessionStateSnapshot` (24h age filter).
    /// 只返回 24 小时内更新过的条目：{ sessionId: { status, lastUpdateAt, metadata } }。
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

    /// 读取单个会话状态（含 lastEventId 与 metadata）；id 为空或未知返回 None。
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
    /// 只返回 24 小时内的条目；条目结构见 attention_snapshot_value。
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

    /// 读取单个会话的关注快照；id 为空或未知返回 None。
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
    /// 把 client_id 加入已读集合；仅当此前确实 needsAttention 时才清除标记并
    /// 广播一条 needsAttention = false 的 session-status 事件。
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

    /// 把 client_id 移出已读集合（客户端断开时回滚查看）；不改动 needsAttention，也不广播。
    pub fn mark_session_unviewed(&self, session_id: &str, client_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = inner.attention_states.get_mut(session_id) {
            state.viewed_by_clients.remove(client_id);
        }
    }

    /// 记录用户刚在该会话发送消息（更新 lastUserMessageAt，必要时懒创建关注状态），
    /// 为之后 busy → idle 判定“待关注”提供依据。
    pub fn mark_user_message_sent(&self, session_id: &str) {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = get_or_create_attention_state(&mut inner, session_id, now) {
            state.last_user_message_at = Some(now);
        }
    }

    /// `resetAllSessionActivityToIdle`: no per-session broadcasts (JS sets
    /// the map directly).
    /// 清空冷却、活跃计数归零、全部相位直接改 idle；与 JS 一致不产生任何广播。
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
    /// 收集状态为 busy/retry 或相位为 busy 的会话（按 id 去重排序），逐个落 idle
    /// （携带 opencode-restart 标记，绕过 5 秒去重）并广播一条 session.error
    /// （MessageAbortedError），最后内联重置全部活动相位；返回被打断的会话 id。
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
    /// 逐出超过 24 小时未更新的状态、关注与活动条目；清理 busy 活动条目时同步
    /// 扣减活跃计数（下限 0）。JS 由每小时 setInterval 驱动。
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
    /// 清空全部内部状态（冷却、相位、状态、关注、活跃计数）；关停与测试用。
    pub fn dispose(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cooldowns.clear();
        inner.activity_phases.clear();
        inner.states.clear();
        inner.attention_states.clear();
        inner.active_session_count = 0;
    }
}

/// 把关注状态序列化为快照 JSON：needsAttention / lastUserMessageAt /
/// lastStatusChangeAt / status / isViewed。
fn attention_snapshot_value(state: &AttentionState) -> Value {
    json!({
        "needsAttention": state.needs_attention,
        "lastUserMessageAt": state.last_user_message_at,
        "lastStatusChangeAt": state.last_status_change_at,
        "status": state.status,
        "isViewed": !state.viewed_by_clients.is_empty(),
    })
}

/// 取出或懒创建会话的关注状态（新建条目初始为 idle、无关注、无已读）；
/// 会话 id 为空返回 None。
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
/// 同相位直接拒绝；cooldown 仅可从 busy 迁入；busy 进出维护活跃计数；
/// 接受迁移时登记/撤销冷却截止并追加一条广播。返回是否真的发生了迁移。
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
/// 同状态 5 秒内去重（opencode-restart 打断除外）；metadata 按 Some 覆盖、
/// None 删除合并；busy/retry → idle 且有用户消息且无人查看时置
/// needsAttention；“首次 / 状态变化 / 关注翻转 / 重启打断”任一成立才广播；
/// 最后同步活动相位（cooldown 期间不被 idle 覆盖）。
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

/// SessionRuntime 单元测试：相位迁移、状态去重、关注标记与生命周期清理。
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    /// Test harness capturing broadcasts instead of relying on the shared
    /// EventHub wiring.
    /// 订阅 EventHub 广播并在 drain 中解析回 JSON，便于断言。
    struct Recorder {
        /// hub 事件订阅端。
        rx: broadcast::Receiver<crate::hub::HubEvent>,
    }

    /// Recorder 的构造与收取辅助。
    impl Recorder {
        /// 新建 EventHub 并订阅，返回 (Recorder, hub)。
        fn new() -> (Self, Arc<EventHub>) {
            let hub = EventHub::new();
            let rx = hub.subscribe();
            (Self { rx }, hub)
        }

        /// 非阻塞取空积压的广播，返回 (事件名, 载荷 JSON) 列表。
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

    /// 构造 canonical 形态（properties.status.type）的 session.status 载荷。
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

    /// 构造 legacy 形态（properties.info.*，含 attempt/message）的载荷。
    fn legacy_status_payload(session_id: &str, status_type: &str) -> Value {
        json!({
            "type": "session.status",
            "properties": {
                "sessionID": session_id,
                "info": { "type": status_type, "attempt": 2, "message": "retrying" },
            },
        })
    }

    /// 验证非 status 载荷与空 sessionID 被忽略：无广播、无活动条目。
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

    /// 验证 busy 迁移按序广播 session-activity 与 session-status，活跃计数与快照同步。
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

    /// 验证 5 秒去重窗口内重复的同状态不产生任何广播。
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

    /// 验证 busy → idle 先进入 cooldown，冷却到期后由快照读取触发 tick 并广播 idle。
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

    /// 验证从未 busy 的会话直接落 idle，不进入 cooldown。
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

    /// 验证活跃计数跨会话随相位进出 busy 幂等增减。
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

    /// 验证未查看会话落 idle 置 needsAttention 并重播；mark viewed 清除且只广播一次；unview 不重新置位。
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

    /// 验证已被查看的会话落 idle 时不会标记待关注。
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

    /// 验证 legacy info 形态载荷仍能提取状态并合并 attempt/message 元数据。
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

    /// 验证重启打断把 busy 会话落 idle（带 restart 标记）、逐会话广播 session.error 并绕过 5 秒去重。
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

    /// 验证仅有 busy 活动相位（无状态条目）的会话也会被重启打断收尾。
    #[test]
    fn interrupt_picks_up_activity_only_sessions() {
        let (_recorder, hub) = Recorder::new();
        let runtime = SessionRuntime::new(hub);
        // A busy activity phase without a session state entry still counts.
        runtime.process_opencode_sse_payload(&legacy_status_payload("ses_9", "busy"));
        let interrupted = runtime.interrupt_busy_sessions_after_restart();
        assert_eq!(interrupted, vec!["ses_9".to_string()]);
    }

    /// 验证 cleanup 保留新近条目，dispose 清空全部状态与计数。
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
