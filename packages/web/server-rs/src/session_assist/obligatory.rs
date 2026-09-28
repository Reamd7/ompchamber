//! Port of `server/lib/context-obligatory/runtime.js` — after compaction,
//! re-send the messages the user explicitly pinned as required context, plus
//! the project-knowledge block, as ONE synthetic prompt so the agent is not
//! interrupted twice.
//!
//! 中文说明：`context-obligatory` runtime——压缩（compaction）发生后，
//! 把用户显式置顶为必需上下文的消息与项目知识块合并成一条 synthetic
//! prompt 重新发送，避免 agent 被打断两次。事件驱动：仅 `session.compacted`
//! 触发工作，同一会话的并发事件去重。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};

use super::fetch::{OpenCodeFetch, encode_uri_component};
use super::knowledge::{Pins, SessionKnowledgeRuntime};

/// 引擎调用的单请求超时（毫秒），对应 index.js 注入的 `AbortSignal.timeout`。
pub const FETCH_TIMEOUT_MS: u64 = 15_000;
/// 压缩检测时拉取最近消息的条数上限（JS 的 `limit` query 参数）。
pub const MESSAGE_FETCH_LIMIT: usize = 20;
/// `session.metadata.openchamber.context_obligatory_messages`.
///
/// 中文说明：置顶必需消息列表在 session metadata 中的存储 key。
pub const MESSAGES_METADATA_KEY: &str = "context_obligatory_messages";
/// `session.metadata.openchamber` 下的压缩游标：已注入的 summary 消息 id。
/// 等于当前 summary id 即表示已补发完毕，无需再次注入。
pub const LAST_COMPACTION_METADATA_KEY: &str = "context_obligatory_last_compaction_message_id";

// ---------------------------------------------------------------------------
// Payload helpers (pure, exported for tests)
// ---------------------------------------------------------------------------

/// JS `isRecord`.
///
/// 中文说明：值存在且为 JSON 对象才视为 record。
fn is_record(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_object)
}

/// 取 JSON 对象值的克隆；缺省或非对象时返回空 Map（对应 JS 的 `?? {}`
/// 防御式读取）。
fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// JS truthiness for the values read off message info.
///
/// 中文说明：复刻 JS 真值表——null/undefined/false/0/NaN/空串为假，
/// 非空数组与对象恒为真。
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
        Some(Value::Bool(true)) => true,
    }
}

/// 一条被用户置顶为必需上下文的消息的元数据（不含正文，正文按需拉取）。
#[derive(Debug, Clone, PartialEq)]
pub struct PinnedMessage {
    /// 引擎侧的消息 id。
    pub id: String,
    /// 创建时间（epoch 毫秒），用于时间线排序与时间戳渲染。
    pub created_at: f64,
    /// 消息角色（仅接受 user/assistant）。
    pub role: String,
}

/// 从 session JSON 解析出的、驱动注入决策的状态快照。
#[derive(Debug, Clone, Default)]
pub struct ContextState {
    /// 会话完整 metadata 对象（含 ompchamber 之外的字段，PATCH 时原样带回）。
    pub metadata: Map<String, Value>,
    /// metadata.ompchamber 子对象。
    pub ompchamber: Map<String, Value>,
    /// 通过校验的置顶消息列表（保持存储顺序）。
    pub messages: Vec<PinnedMessage>,
}

/// JS: `readContextState`.
///
/// 中文说明：防御式解析 session JSON 中的 metadata、ompchamber 与置顶
/// 消息列表；缺字段、角色非法或非对象的置顶条目直接丢弃，不报错。
pub fn read_context_state(session: &Value) -> ContextState {
    let metadata = if is_record(session.get("metadata")) {
        object_or_empty(session.get("metadata"))
    } else {
        Map::new()
    };
    let ompchamber = if metadata.get("ompchamber").is_some_and(Value::is_object) {
        object_or_empty(metadata.get("ompchamber"))
    } else {
        Map::new()
    };
    let messages = ompchamber
        .get(MESSAGES_METADATA_KEY)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    is_record(Some(item))
                        && item.get("id").and_then(Value::as_str).is_some()
                        && item.get("createdAt").and_then(Value::as_f64).is_some()
                        && matches!(
                            item.get("role").and_then(Value::as_str),
                            Some("user") | Some("assistant")
                        )
                })
                .map(|item| PinnedMessage {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    created_at: item.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0),
                    role: item
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    ContextState {
        metadata,
        ompchamber,
        messages,
    }
}

/// `new Date(ms).toISOString()` — always UTC with millisecond precision.
///
/// 中文说明：把 epoch 毫秒格式化为恒定 UTC、毫秒精度的 ISO-8601 字符串，
/// 负数时间同样正确。
pub fn iso8601_from_epoch_ms(ms: f64) -> String {
    let whole_ms = ms.floor() as i64;
    let days = whole_ms.div_euclid(86_400_000);
    let time_of_day = whole_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3_600_000;
    let minute = (time_of_day % 3_600_000) / 60_000;
    let second = (time_of_day % 60_000) / 1_000;
    let millis = time_of_day % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days algorithm.
///
/// 中文说明：Howard Hinnant 的 civil-from-days 算法——把自 1970-01-01 起
/// 的天数换算为公历 (年, 月, 日)，全程整数运算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// JS: `buildContextPrompt`.
///
/// 中文说明：把置顶消息渲染为时间线（`## role — ISO 时间` 加正文，
/// 条目间以分隔线连接），前置三段固定说明文字，要求 agent 把其当作
/// 背景静默续用而非新任务。
pub fn build_context_prompt(entries: &[(PinnedMessage, String)]) -> String {
    let timeline = entries
        .iter()
        .map(|(pinned, text)| {
            let timestamp = iso8601_from_epoch_ms(pinned.created_at);
            format!("## {} — {timestamp}\n\n{text}", pinned.role)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    [
        "The following messages are from the compacted conversation. The user explicitly marked them as important and required in your context. Pay close attention to them; they may have been sent by either the user or you before compaction.".to_string(),
        "Use them while continuing the pre-compaction work. Do not treat this context restoration as a new standalone task.".to_string(),
        "If any tasks or next steps remain, do not acknowledge, summarize, or mention this restored context in a separate response. Simply continue the work and use it silently as background context. Do not append a recap of it after completing those tasks. Only if no tasks or next steps remain, give the user a very brief summary of the important restored context in no more than one short paragraph, without lists or a detailed recap.".to_string(),
        String::new(),
        timeline,
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// 构造 [`ContextObligatoryRuntime`] 的依赖注入集合。
pub struct ContextObligatoryOptions {
    /// 引擎 fetch 接缝（读写 session、消息与 prompt）。
    pub fetch: OpenCodeFetch,
    /// `sessionKnowledgeRuntime` (optional in the JS factory default).
    ///
    /// 中文说明：知识块 runtime，可缺省——缺省时只重发置顶消息。
    pub session_knowledge: Option<Arc<SessionKnowledgeRuntime>>,
}

/// 压缩后的上下文补发 runtime：监听 `session.compacted`，把置顶消息与
/// 项目知识块合成一条 synthetic prompt 注入会话。
pub struct ContextObligatoryRuntime {
    /// 引擎 fetch 接缝。
    fetch: OpenCodeFetch,
    /// 可选的 session-knowledge runtime，用于重新投递项目知识块。
    session_knowledge: Option<Arc<SessionKnowledgeRuntime>>,
    /// 正在处理 tick 的 session id 集合，实现同会话并发事件去重。
    inflight: Mutex<HashSet<String>>,
    /// 停机标记；置位后不再接受新事件。
    stopped: AtomicBool,
}

/// 获取互斥锁；锁 poisoned 时降级取出内部数据，避免运行时因一次 panic
/// 而永久卡死。
fn lock_set(lock: &Mutex<HashSet<String>>) -> std::sync::MutexGuard<'_, HashSet<String>> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

/// runtime 的构造、注入主流程与事件入口。
impl ContextObligatoryRuntime {
    /// 以给定选项构造 runtime（返回 Arc 便于跨任务共享）。
    pub fn new(options: ContextObligatoryOptions) -> Arc<Self> {
        Arc::new(Self {
            fetch: options.fetch,
            session_knowledge: options.session_knowledge,
            inflight: Mutex::new(HashSet::new()),
            stopped: AtomicBool::new(false),
        })
    }

    /// 在 fetch 接缝之上补一层 query 渲染：key/value 均按 JS
    /// `encodeURIComponent` 编码后以 `&` 连接追加到路径。
    async fn open_code_fetch(
        &self,
        path: &str,
        directory: &str,
        method: &str,
        body: Option<&Value>,
        query: Option<&[(String, String)]>,
    ) -> Result<Value, super::fetch::OpenCodeError> {
        let mut path_with_query = path.to_string();
        if let Some(query) = query {
            let rendered = query
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        encode_uri_component(key),
                        encode_uri_component(value)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            if !rendered.is_empty() {
                path_with_query = format!("{path}?{rendered}");
            }
        }
        (self.fetch)(&path_with_query, Some(directory), method, body).await
    }

    /// JS: `tick`.
    ///
    /// 中文说明：单会话的一次完整注入流程——读会话（子会话直接跳过）、
    /// 检查最近消息中的压缩 summary 是否已完成且游标未跟上，拉取置顶
    /// 消息正文（单条失败只跳过该条）与待重发的知识块，按时间正序合成
    /// synthetic prompt 发送，最后 PATCH 写回压缩游标与知识 signature。
    /// 任一步失败返回 Err，由调用方记 warn。
    pub async fn tick(&self, session_id: &str, directory: &str) -> anyhow::Result<()> {
        let session_path = format!("/session/{}", encode_uri_component(session_id));
        let session = self
            .open_code_fetch(&session_path, directory, "GET", None, None)
            .await?;
        if session
            .get("parentID")
            .and_then(Value::as_str)
            .is_some_and(|parent| !parent.is_empty())
        {
            return Ok(());
        }
        let state = read_context_state(&session);

        // Project knowledge rides along with the pinned messages. Compaction
        // removed the previously delivered block, so its stored signature is
        // no longer evidence that the session still carries it.
        let knowledge = match self.session_knowledge.as_ref() {
            Some(runtime) => runtime
                .resolve_pending(directory, "", &Pins::from_session(&session))
                .await
                .unwrap_or_else(|_| json!({ "text": "", "signature": "" })),
            None => json!({ "text": "", "signature": "" }),
        };
        let knowledge_text = knowledge.get("text").and_then(Value::as_str).unwrap_or("");
        let knowledge_signature = knowledge
            .get("signature")
            .and_then(Value::as_str)
            .unwrap_or("");

        if state.messages.is_empty() && knowledge_text.is_empty() {
            return Ok(());
        }

        let messages_path = format!("/session/{}/message", encode_uri_component(session_id));
        let recent = self
            .open_code_fetch(
                &messages_path,
                directory,
                "GET",
                None,
                Some(&[("limit".to_string(), MESSAGE_FETCH_LIMIT.to_string())]),
            )
            .await?;
        let Some(recent) = recent.as_array() else {
            return Ok(());
        };
        if recent.is_empty() {
            return Ok(());
        }
        let is_summary = |message: &Value| {
            message.get("info").is_some_and(|info| {
                info.get("role").and_then(Value::as_str) == Some("assistant")
                    && info.get("summary") == Some(&Value::Bool(true))
            })
        };
        let Some(summary) = recent
            .iter()
            .rev()
            .find(|message| is_summary(message))
            .and_then(|message| message.get("info"))
        else {
            return Ok(());
        };
        let summary_id = summary.get("id").and_then(Value::as_str).unwrap_or("");
        let completed = summary.get("time").and_then(|time| time.get("completed"));
        if summary_id.is_empty() || !js_truthy(completed) {
            return Ok(());
        }
        if state
            .ompchamber
            .get(LAST_COMPACTION_METADATA_KEY)
            .and_then(Value::as_str)
            == Some(summary_id)
        {
            return Ok(());
        }

        // Pinned-message text fetches settle individually — one failure
        // skips that entry instead of aborting the injection.
        let mut entries: Vec<(PinnedMessage, String)> = Vec::new();
        for pinned in &state.messages {
            let message_path = format!(
                "/session/{}/message/{}",
                encode_uri_component(session_id),
                encode_uri_component(&pinned.id)
            );
            let Ok(message) = self
                .open_code_fetch(&message_path, directory, "GET", None, None)
                .await
            else {
                continue;
            };
            let text = message
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| {
                            part.get("type").and_then(Value::as_str) == Some("text")
                                && part.get("text").and_then(Value::as_str).is_some()
                        })
                        .filter_map(|part| {
                            part.get("text")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|text| !text.is_empty())
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            if !text.is_empty() {
                entries.push((pinned.clone(), text));
            }
        }
        entries.sort_by(|left, right| {
            left.0
                .created_at
                .partial_cmp(&right.0.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if entries.is_empty() && knowledge_text.is_empty() {
            return Ok(());
        }

        let execution_info = recent
            .iter()
            .rev()
            .find(|message| {
                message.get("info").is_some_and(|info| {
                    info.get("role").and_then(Value::as_str) == Some("assistant")
                        && info.get("summary") != Some(&Value::Bool(true))
                })
            })
            .and_then(|message| message.get("info"));
        let provider_id = execution_info
            .and_then(|info| info.get("providerID"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let model_id = execution_info
            .and_then(|info| info.get("modelID"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if provider_id.is_empty() || model_id.is_empty() {
            anyhow::bail!("no pre-compaction assistant provider/model");
        }
        let agent = execution_info
            .and_then(|info| info.get("agent"))
            .filter(|agent| agent.is_string())
            .or_else(|| {
                execution_info
                    .and_then(|info| info.get("mode"))
                    .filter(|mode| mode.is_string())
            })
            .and_then(Value::as_str)
            .filter(|agent| !agent.is_empty());

        let text = [
            if !knowledge_text.is_empty() {
                knowledge_text.to_string()
            } else {
                Default::default()
            },
            if !entries.is_empty() {
                build_context_prompt(&entries)
            } else {
                Default::default()
            },
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

        let mut body = Map::new();
        body.insert(
            "model".into(),
            json!({ "providerID": provider_id, "modelID": model_id }),
        );
        if let Some(agent) = agent {
            body.insert("agent".into(), json!(agent));
        }
        body.insert(
            "parts".into(),
            json!([{ "type": "text", "text": text, "synthetic": true }]),
        );
        let prompt_path = format!("/session/{}/prompt_async", encode_uri_component(session_id));
        self.open_code_fetch(
            &prompt_path,
            directory,
            "POST",
            Some(&Value::Object(body)),
            None,
        )
        .await?;

        let fresh = self
            .open_code_fetch(&session_path, directory, "GET", None, None)
            .await?;
        let fresh_state = read_context_state(&fresh);
        let mut ompchamber = fresh_state.ompchamber;
        ompchamber.insert(LAST_COMPACTION_METADATA_KEY.into(), json!(summary_id));
        // Recorded together with the cursor: the session now carries this
        // knowledge again, so the next send must not repeat it.
        if let Some(runtime) = self.session_knowledge.as_ref()
            && !knowledge_signature.is_empty()
        {
            ompchamber.insert(runtime.metadata_key().into(), json!(knowledge_signature));
        }
        let mut metadata = fresh_state.metadata;
        metadata.insert("ompchamber".into(), Value::Object(ompchamber));
        self.open_code_fetch(
            &session_path,
            directory,
            "PATCH",
            Some(&json!({ "metadata": metadata })),
            None,
        )
        .await?;
        Ok(())
    }

    /// JS: `processPayload(payload, directoryHint)` — only `session.compacted`
    /// events trigger work; concurrent events for one session dedupe.
    ///
    /// 中文说明：hub 事件入口——只处理 `session.compacted`；同一 session
    /// 的并发事件经 inflight 集合去重，实际工作派发到独立 tokio 任务，
    /// 失败仅记 warn。directory 优先取事件属性，缺省回退到调用方提示。
    pub fn process_payload(self: &Arc<Self>, payload: &Value, directory_hint: &str) {
        if self.stopped.load(Ordering::SeqCst)
            || payload.get("type").and_then(Value::as_str) != Some("session.compacted")
        {
            return;
        }
        let Some(session_id) = payload
            .get("properties")
            .and_then(|properties| properties.get("sessionID"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        if lock_set(&self.inflight).contains(&session_id) {
            return;
        }
        let directory = payload
            .get("properties")
            .and_then(|properties| properties.get("directory"))
            .and_then(Value::as_str)
            .filter(|directory| !directory.is_empty())
            .unwrap_or(directory_hint)
            .to_string();
        lock_set(&self.inflight).insert(session_id.clone());
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = this.tick(&session_id, &directory).await {
                tracing::warn!("[context-obligatory] injection failed: {error}");
            }
            lock_set(&this.inflight).remove(&session_id);
        });
    }

    /// JS: `stop`.
    ///
    /// 中文说明：置位停机标记，后续事件一律忽略。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// index.js `onPayload` wiring for this runtime: hub frames carry the
/// event-stream envelope `{payload, directory}` (see
/// `session_goal::spawn_hub_bridge`).
///
/// 中文说明：订阅 hub 帧，剥开 `{payload, directory}` 信封（兼容再包一层
/// 的 payload 形状），把对象载荷连同 directory 交给 `process_payload`；
/// 解析失败或非对象载荷直接跳过，订阅通道关闭即退出。
pub fn spawn_hub_bridge(
    runtime: Arc<ContextObligatoryRuntime>,
    hub: Arc<crate::hub::EventHub>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut receiver = hub.subscribe();
        loop {
            let Ok(event) = receiver.recv().await else {
                return;
            };
            let Ok(envelope) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let raw = envelope.get("payload").cloned().unwrap_or(Value::Null);
            let payload = raw
                .get("payload")
                .filter(|inner| inner.is_object())
                .cloned()
                .unwrap_or(raw);
            if !payload.is_object() {
                continue;
            }
            let directory = envelope
                .get("directory")
                .and_then(Value::as_str)
                .filter(|directory| !directory.is_empty() && *directory != "global")
                .unwrap_or("")
                .to_string();
            runtime.process_payload(&payload, &directory);
        }
    })
}

/// runtime 与纯函数的合同测试：以内存 FakeEngine 驱动真实注入流程。
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;

    /// 测试用会话 id。
    const SESSION_ID: &str = "ses_1";
    /// 测试用项目目录。
    const DIRECTORY: &str = "/work/project";

    /// 单条已记录引擎请求的快照，供测试断言。
    #[derive(Clone, Debug)]
    struct RecordedRequest {
        /// 请求路径（可能带 query string）。
        path: String,
        /// HTTP 方法。
        method: String,
        /// 请求 body（GET 时为 None）。
        body: Option<Value>,
    }

    /// 内存版引擎接缝：记录每次请求，并按路径返回会话/消息/置顶正文。
    struct FakeEngine {
        /// 已发出的请求记录。
        requests: StdMutex<Vec<RecordedRequest>>,
        /// GET /session/:id 返回的会话 JSON。
        session: Value,
        /// GET /session/:id/message 返回的最近消息列表。
        messages: Value,
        /// 消息 id → 置顶消息正文 JSON。
        pinned_texts: HashMap<String, Value>,
    }

    /// FakeEngine 的场景构造与断言辅助。
    impl FakeEngine {
        /// 场景：置顶 user msg_1 与 assistant msg_2，压缩 summary 已完成。
        fn session_with_pins() -> Self {
            Self {
                requests: StdMutex::new(Vec::new()),
                session: json!({
                    "id": SESSION_ID,
                    "metadata": { "ompchamber": { "context_obligatory_messages": [
                        { "id": "msg_2", "createdAt": 20, "role": "assistant" },
                        { "id": "msg_1", "createdAt": 10, "role": "user" },
                    ] } },
                }),
                messages: json!([
                    { "info": { "id": "msg_agent", "role": "assistant", "providerID": "provider", "modelID": "model", "agent": "build" } },
                    { "info": { "id": "msg_summary", "role": "assistant", "summary": true, "time": { "completed": 30 } } },
                ]),
                pinned_texts: HashMap::from([
                    (
                        "msg_1".to_string(),
                        json!({ "parts": [{ "type": "text", "text": "First" }] }),
                    ),
                    (
                        "msg_2".to_string(),
                        json!({ "parts": [{ "type": "text", "text": "Second" }] }),
                    ),
                ]),
            }
        }

        /// 场景：无置顶消息，但知识块压缩前已投递（signature 与 pin 均在）。
        fn knowledge_session() -> Self {
            Self {
                requests: StdMutex::new(Vec::new()),
                session: json!({
                    "id": SESSION_ID,
                    "metadata": { "ompchamber": {
                        "knowledge_context_delivered": "sig-before-compaction",
                        "project_context_pins": { "notes": ["n1"], "plans": [] },
                    } },
                }),
                messages: json!([
                    { "info": { "id": "msg_agent", "role": "assistant", "providerID": "provider", "modelID": "model", "agent": "build" } },
                    { "info": { "id": "msg_summary", "role": "assistant", "summary": true, "time": { "completed": 30 } } },
                ]),
                pinned_texts: HashMap::new(),
            }
        }

        /// 把本 FakeEngine 包装成 OpenCodeFetch：记录请求并按路径分发模拟
        /// 响应（消息列表断言 limit query），未知路径报 404。
        fn fetch(self: &Arc<Self>) -> OpenCodeFetch {
            let engine = Arc::clone(self);
            Arc::new(
                move |path: &str, _directory: Option<&str>, method: &str, body: Option<&Value>| {
                    let engine = Arc::clone(&engine);
                    let path = path.to_string();
                    let method = method.to_string();
                    let body = body.cloned();
                    Box::pin(async move {
                        engine
                            .requests
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(RecordedRequest {
                                path: path.clone(),
                                method: method.clone(),
                                body: body.clone(),
                            });
                        let session_path = format!("/session/{SESSION_ID}");
                        // The fetch seam appends query parameters; match on
                        // the clean path.
                        let clean_path = path.split('?').next().unwrap_or(&path);
                        if clean_path == format!("{session_path}/message") {
                            assert!(path.contains("limit=20"), "limit query missing: {path}");
                            return Ok(engine.messages.clone());
                        }
                        if let Some(pinned_id) = clean_path
                            .strip_prefix(&format!("{session_path}/message/"))
                            .map(str::to_string)
                        {
                            return Ok(engine
                                .pinned_texts
                                .get(&pinned_id)
                                .cloned()
                                .unwrap_or(Value::Null));
                        }
                        if path == format!("{session_path}/prompt_async") {
                            return Ok(Value::Null);
                        }
                        if path == session_path && method == "PATCH" {
                            return Ok(Value::Null);
                        }
                        if path == session_path {
                            return Ok(engine.session.clone());
                        }
                        Err(super::super::fetch::OpenCodeError::Status {
                            method,
                            path,
                            status: 404,
                        })
                    })
                },
            )
        }

        /// 取请求记录快照（锁 poisoned 时降级）。
        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    /// 等待 process_payload 派生的 tick 任务收尾。
    async fn drain() {
        // Let spawned tick tasks finish (process_payload spawns).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// 无 session-knowledge 的 runtime 选项（只测置顶消息路径）。
    fn no_knowledge(fetch: OpenCodeFetch) -> ContextObligatoryOptions {
        ContextObligatoryOptions {
            fetch,
            session_knowledge: None,
        }
    }

    /// 契约：注入按时间正序排列置顶正文，携带压缩前 provider/model 与
    /// agent，之后 PATCH 记录 summary 游标；会话恰好被读取两次。
    #[tokio::test]
    async fn injects_pinned_text_in_chronological_order_and_records_the_cursor() {
        let engine = Arc::new(FakeEngine::session_with_pins());
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));

        runtime.process_payload(
            &json!({ "type": "session.compacted", "properties": { "sessionID": SESSION_ID } }),
            "",
        );
        drain().await;
        runtime.stop();

        let requests = engine.requests();
        let prompt = requests
            .iter()
            .find(|request| request.path.ends_with("/prompt_async"))
            .expect("prompt");
        let body = prompt.body.as_ref().expect("body");
        assert_eq!(
            body["model"],
            json!({ "providerID": "provider", "modelID": "model" })
        );
        assert_eq!(body["agent"], json!("build"));
        assert_eq!(
            body["parts"],
            json!([{ "type": "text", "text": body["parts"][0]["text"], "synthetic": true }])
        );
        let text = body["parts"][0]["text"].as_str().expect("text");
        assert!(text.contains("continuing the pre-compaction work"));
        assert!(text.contains("use it silently as background context"));
        assert!(text.contains("Only if no tasks or next steps remain"));
        assert!(text.contains("no more than one short paragraph"));
        assert!(
            text.find("First").expect("First") < text.find("Second").expect("Second"),
            "chronological order"
        );
        assert!(text.contains("## user — 1970-01-01T00:00:00.010Z\n\nFirst"));
        assert!(text.contains("## assistant — 1970-01-01T00:00:00.020Z\n\nSecond"));
        let patch = requests
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("patch");
        assert_eq!(
            patch.body.as_ref().expect("body")["metadata"]["ompchamber"]
                [LAST_COMPACTION_METADATA_KEY],
            json!("msg_summary")
        );
        // Session read twice: initial + fresh read before the patch.
        let session_gets = requests
            .iter()
            .filter(|request| request.method == "GET" && request.path.ends_with(SESSION_ID))
            .count();
        assert_eq!(session_gets, 2);
    }

    /// 契约：无置顶消息时仍重发被压缩清除的知识块，并把新的 knowledge
    /// signature 与压缩游标一并写回 metadata。
    #[tokio::test]
    async fn restores_project_knowledge_even_with_nothing_pinned() {
        let engine = Arc::new(FakeEngine::knowledge_session());
        let knowledge = SessionKnowledgeRuntime::new(
            super::super::knowledge::SessionKnowledgeOptions {
                fetch: engine.fetch(),
                resolve_project_id: Arc::new(|_d: &str| {
                    Box::pin(async { Ok("path_project".to_string()) })
                }),
                read_context: Arc::new(|_p: &str| {
                    Box::pin(async {
                        Ok(json!({
                            "notes": [{ "id": "n1", "body": "Remember this.", "createdAt": 1, "updatedAt": 1 }],
                            "todos": [],
                            "plans": [],
                        }))
                    })
                }),
                read_plan: Arc::new(|_p: &str, _id: &str| {
                    Box::pin(async { Err(anyhow::anyhow!("unavailable")) })
                }),
                read_all_memory: Arc::new(|_p: Option<&str>| {
                    Box::pin(async { Err(anyhow::anyhow!("unavailable")) })
                }),
                is_memory_enabled: Arc::new(|| Box::pin(async { true })),
            },
        );
        let runtime = ContextObligatoryRuntime::new(ContextObligatoryOptions {
            fetch: engine.fetch(),
            session_knowledge: Some(knowledge),
        });

        runtime.process_payload(
            &json!({
                "type": "session.compacted",
                "properties": { "sessionID": SESSION_ID, "directory": DIRECTORY },
            }),
            "",
        );
        drain().await;
        runtime.stop();

        let requests = engine.requests();
        // resolvePending ran with the compaction-cleared delivered signature.
        let prompt = requests
            .iter()
            .find(|request| request.path.ends_with("/prompt_async"))
            .expect("prompt");
        let text = prompt.body.as_ref().expect("body")["parts"][0]["text"]
            .as_str()
            .expect("text");
        assert!(text.contains("Remember this."), "text: {text}");
        let patch = requests
            .iter()
            .find(|request| request.method == "PATCH")
            .expect("patch");
        let metadata = &patch.body.as_ref().expect("body")["metadata"]["ompchamber"];
        assert_eq!(metadata[LAST_COMPACTION_METADATA_KEY], json!("msg_summary"));
        // Recorded with the cursor, so the next ordinary send does not repeat.
        assert_eq!(
            metadata[super::super::knowledge::KNOWLEDGE_METADATA_KEY],
            json!("n:n1:1")
        );
    }

    /// 契约：prompt 说明文字与时间线格式和文档口径逐字一致。
    #[test]
    fn prompt_builder_renders_the_documented_instructions() {
        let entries = vec![
            (
                PinnedMessage {
                    id: "a".into(),
                    created_at: 10.0,
                    role: "user".into(),
                },
                "First".to_string(),
            ),
            (
                PinnedMessage {
                    id: "b".into(),
                    created_at: 20.0,
                    role: "assistant".into(),
                },
                "Second".to_string(),
            ),
        ];
        let prompt = build_context_prompt(&entries);
        let expected = [
            "The following messages are from the compacted conversation. The user explicitly marked them as important and required in your context. Pay close attention to them; they may have been sent by either the user or you before compaction.",
            "Use them while continuing the pre-compaction work. Do not treat this context restoration as a new standalone task.",
            "If any tasks or next steps remain, do not acknowledge, summarize, or mention this restored context in a separate response. Simply continue the work and use it silently as background context. Do not append a recap of it after completing those tasks. Only if no tasks or next steps remain, give the user a very brief summary of the important restored context in no more than one short paragraph, without lists or a detailed recap.",
            "",
            "## user — 1970-01-01T00:00:00.010Z\n\nFirst\n\n---\n\n## assistant — 1970-01-01T00:00:00.020Z\n\nSecond",
        ]
        .join("\n");
        assert_eq!(prompt, expected);
    }

    /// 契约：ISO-8601 格式覆盖 epoch、现代时间与负时间。
    #[test]
    fn iso8601_covers_epoch_and_beyond() {
        assert_eq!(iso8601_from_epoch_ms(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_from_epoch_ms(1_700_000_000_123.0),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(iso8601_from_epoch_ms(-1.0), "1969-12-31T23:59:59.999Z");
    }

    /// 契约：非 `session.compacted` 事件不做任何引擎请求。
    #[tokio::test]
    async fn ignores_ordinary_idle_events_without_making_requests() {
        let engine = Arc::new(FakeEngine::session_with_pins());
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));

        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": { "sessionID": SESSION_ID, "status": { "type": "idle" } },
            }),
            "",
        );
        drain().await;

        assert!(engine.requests().is_empty());
    }

    /// 契约：游标已等于当前 summary id 时不注入。
    #[tokio::test]
    async fn does_nothing_when_already_caught_up() {
        let mut engine = FakeEngine::session_with_pins();
        // Cursor already at the current summary id and nothing else owed.
        engine.session = json!({ "id": SESSION_ID, "metadata": { "ompchamber": {
            "context_obligatory_last_compaction_message_id": "msg_summary",
        } } });
        let engine = Arc::new(engine);
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));

        runtime.process_payload(
            &json!({ "type": "session.compacted", "properties": { "sessionID": SESSION_ID } }),
            "",
        );
        drain().await;

        assert!(
            !engine
                .requests()
                .iter()
                .any(|request| request.path.ends_with("/prompt_async"))
        );
    }

    /// 契约：同一会话的并发 compacted 事件只注入一次。
    #[tokio::test]
    async fn dedupes_concurrent_events_for_one_session() {
        let engine = Arc::new(FakeEngine::session_with_pins());
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));

        runtime.process_payload(
            &json!({ "type": "session.compacted", "properties": { "sessionID": SESSION_ID } }),
            "",
        );
        runtime.process_payload(
            &json!({ "type": "session.compacted", "properties": { "sessionID": SESSION_ID } }),
            "",
        );
        drain().await;

        let prompts = engine
            .requests()
            .iter()
            .filter(|request| request.path.ends_with("/prompt_async"))
            .count();
        assert_eq!(prompts, 1);
    }

    /// 契约：找不到压缩前 assistant provider/model 时中止注入，不发送 prompt。
    #[tokio::test]
    async fn missing_provider_model_aborts_the_injection() {
        let mut engine = FakeEngine::session_with_pins();
        engine.messages = json!([
            { "info": { "id": "msg_summary", "role": "assistant", "summary": true, "time": { "completed": 30 } } },
        ]);
        let engine = Arc::new(engine);
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));

        runtime.process_payload(
            &json!({ "type": "session.compacted", "properties": { "sessionID": SESSION_ID } }),
            "",
        );
        drain().await;

        assert!(
            !engine
                .requests()
                .iter()
                .any(|request| request.path.ends_with("/prompt_async"))
        );
    }

    /// 契约：置顶条目缺字段、角色非法或非对象一律被过滤。
    #[test]
    fn read_context_state_validates_pinned_entries() {
        let state = read_context_state(&json!({
            "metadata": { "ompchamber": { "context_obligatory_messages": [
                { "id": "ok_user", "createdAt": 1, "role": "user" },
                { "id": "ok_assistant", "createdAt": 2, "role": "assistant" },
                { "createdAt": 3, "role": "user" },
                { "id": "bad_role", "createdAt": 4, "role": "system" },
                { "id": "bad_created", "role": "user" },
                "not-a-record",
            ] } },
        }));
        assert_eq!(
            state
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ok_user", "ok_assistant"]
        );
        assert_eq!(read_context_state(&json!({})).messages, Vec::new());
    }

    /// 契约：hub 桥能解出 `{payload, directory}` 信封中的 compacted 事件并
    /// 完成注入。
    #[tokio::test]
    async fn hub_bridge_routes_compacted_events() {
        let engine = Arc::new(FakeEngine::session_with_pins());
        let runtime = ContextObligatoryRuntime::new(no_knowledge(engine.fetch()));
        let hub = crate::hub::EventHub::new();
        let bridge = spawn_hub_bridge(Arc::clone(&runtime), hub.clone());
        // Let the spawned subscriber connect before publishing.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        hub.publish_json(
            "ompchamber:opencode-event",
            &json!({
                "payload": {
                    "type": "session.compacted",
                    "payload": { "type": "session.compacted", "properties": { "sessionID": SESSION_ID } },
                },
                "directory": DIRECTORY,
            }),
        );
        // Give the bridge task a turn, then let the tick drain.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drain().await;
        bridge.abort();
        runtime.stop();

        assert!(
            engine
                .requests()
                .iter()
                .any(|request| request.path.ends_with("/prompt_async"))
        );
    }
}
