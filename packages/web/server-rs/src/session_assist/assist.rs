//! Port of `server/lib/session-assist/runtime.js` — after a session goes
//! idle and stays quiet for a minute, generate a short recap of the agent's
//! last reply plus one suggested user follow-up with the small model, and
//! store both on the session's metadata (`metadata.ompchamber.assist`).
//! Purely event-driven: no backfill, no session scans.
//!
//! 会话辅助（session-assist）运行期：会话空闲并静默满一分钟后，用小
//! 模型为助手最近一条回复生成简短回顾（recap）与一条可直接发送的后续
//! 建议（suggestion），写入 metadata.ompchamber.assist。纯事件驱动：
//! 无回填、无会话扫描。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::hub::EventHub;
use crate::session_goal::runtime::{
    SessionStatusEvent, extract_json_object, extract_session_status, has_script_mismatch,
    message_parts_to_text,
};

use super::fetch::{OpenCodeFetch, encode_uri_component};

/// The "1 minute of quiet" rule between idle and generation.
/// idle 到触发生成之间的静默窗口（1 分钟）。
pub const IDLE_QUIET_MS: u64 = 60_000;
/// 拉取最近消息的条数上限。
pub const TRANSCRIPT_MESSAGE_LIMIT: usize = 12;
/// 回顾文本的字符上限。
pub const RECAP_CHAR_LIMIT: usize = 320;
/// 建议文本的字符上限。
pub const SUGGESTION_CHAR_LIMIT: usize = 500;
/// 单次引擎请求的超时（毫秒）。
pub const FETCH_TIMEOUT_MS: u64 = 5_000;

// ---------------------------------------------------------------------------
// Settings gate (`sessionRecapEnabled` / `sessionSuggestionEnabled`)
// ---------------------------------------------------------------------------
/// The Chat settings are hard generation switches (default on): when both are
/// off, no small-model calls and no metadata writes happen at all.
/// 两个生成开关的当前取值（默认全开）；全关时小模型调用与
/// metadata 写入完全不发生。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistTargets {
    /// 是否生成回顾（recap）。
    pub recap: bool,
    /// 是否生成后续建议（suggestion）。
    pub suggestion: bool,
}

/// 读取当前 AssistTargets 的闭包类型；每次生成前实时读取。
pub type GetTargets = Arc<dyn Fn() -> AssistTargets + Send + Sync>;

/// JS `getSessionAssistTargets`: `settings.sessionRecapEnabled !== false`
/// (default on when settings cannot be read).
/// 基于 settings.json 构建开关读取器：sessionRecapEnabled/
/// sessionSuggestionEnabled 显式为 false 才关闭；读不到或非法一律默认开启。
pub fn settings_targets(data_dir: PathBuf) -> GetTargets {
    Arc::new(move || {
        let Ok(raw) = std::fs::read_to_string(data_dir.join("settings.json")) else {
            return AssistTargets {
                recap: true,
                suggestion: true,
            };
        };
        let Ok(settings) = serde_json::from_str::<Value>(&raw) else {
            return AssistTargets {
                recap: true,
                suggestion: true,
            };
        };
        AssistTargets {
            recap: settings.get("sessionRecapEnabled") != Some(&Value::Bool(false)),
            suggestion: settings.get("sessionSuggestionEnabled") != Some(&Value::Bool(false)),
        }
    })
}

// ---------------------------------------------------------------------------
// Small-model seam (`getSmallModelService` → `generateSmallModelText`)
// ---------------------------------------------------------------------------

/// Mirrors `generateSmallModelText({restrictToPreferredProvider: true, ...})`
/// — conversation content must never leave the session's own provider unless
/// the user explicitly picked a small model.
/// 一次小模型文本生成的请求；restrictToPreferredProvider 语义要求
/// 会话内容不离开其自身 provider，除非用户显式选定小模型。
pub struct AssistRequest {
    /// 用户 prompt（含最新一轮对话摘录）。
    pub prompt: String,
    /// 系统提示词（build_assist_system_prompt 的产物）。
    pub system: String,
    /// 会话所在项目目录（引擎请求按目录路由）。
    pub directory: String,
    /// 会话最近回复所用 provider（优先约束）。
    pub preferred_provider_id: Option<String>,
    /// 会话最近回复所用 model（优先约束）。
    pub preferred_model_id: Option<String>,
}

/// 小模型生成结果。
pub struct AssistOutput {
    /// 生成的原始文本（应为单个 JSON 对象）。
    pub text: String,
    /// 实际使用的 provider（记入日志）。
    pub provider_id: Option<String>,
    /// 实际使用的 model。
    pub model_id: Option<String>,
}

/// Carries the JS `error.statusCode` (404 = no authenticated small model,
/// silently skipped).
/// 生成失败：status 对应 JS 的 error.statusCode
///（404 = 无已认证小模型，静默跳过）。
#[derive(Debug)]
pub struct AssistError {
    /// 可选的 HTTP 风格状态码。
    pub status: Option<u16>,
    /// 人类可读的错误消息。
    pub message: String,
}

/// 生成调用的装箱 future 类型。
pub type AssistFuture = Pin<Box<dyn Future<Output = Result<AssistOutput, AssistError>> + Send>>;
/// 小模型文本生成的 seam 类型：请求 → future，测试可注入假实现。
pub type SmallModelText = Arc<dyn Fn(AssistRequest) -> AssistFuture + Send + Sync>;

/// The small-model module is not ported yet: generation fails closed and the
/// assist flow follows the JS error path (404 — silent, nothing written).
/// 小模型模块未移植前的占位实现：恒返回 404，让 assist 流程走
/// JS 的错误路径（静默、什么都不写）。
pub fn unavailable_small_model() -> SmallModelText {
    Arc::new(|_request: AssistRequest| {
        Box::pin(async {
            Err(AssistError {
                status: Some(404),
                message: "small model unavailable".to_string(),
            })
        })
    })
}

// ---------------------------------------------------------------------------
// Prompt + payload helpers (pure, exported for tests)
// ---------------------------------------------------------------------------

/// JS: `buildAssistSystemPrompt({recap, suggestion})`.
/// 按开关拼接系统提示词：要求只输出一个 JSON 对象，并给出 recap 与
/// suggestion 的措辞规则与示例；未开启的目标整段省略。
pub fn build_assist_system_prompt(targets: AssistTargets) -> String {
    let recap = targets.recap;
    let suggestion = targets.suggestion;
    let mut shape_parts: Vec<&str> = Vec::new();
    if recap {
        shape_parts.push("\"recap\": string");
    }
    if suggestion {
        shape_parts.push("\"suggestion\": string");
    }
    [
        "You assist a user who chats with a coding agent. Based on the conversation transcript, return exactly one JSON object and nothing else — no prose, no markdown, no code fences.".to_string(),
        format!("Shape: {{{}}}", shape_parts.join(", ")),
        if recap { "recap: at most 20 words. State the substance directly — the facts, result, or conclusion, plus the next move if there is one. NEVER narrate (\"The assistant explained…\", \"The agent did…\") — write the content itself, like a note the user jotted down." } else { Default::default() }.to_string(),
        if suggestion { "suggestion: write ONE immediately sendable next user message addressed TO the coding agent." } else { Default::default() }.to_string(),
        if suggestion { "The suggestion should be the most useful next step after the assistant's latest reply. It should help the user continue productively, not inspect already-known details." } else { Default::default() }.to_string(),
        if suggestion { "Prefer suggestions that ask the agent to make a concrete improvement, implement something specific, validate the latest change, explain tradeoffs, improve the current approach, or continue from the current result." } else { Default::default() }.to_string(),
        if suggestion { "Rules for suggestion:" } else { Default::default() }.to_string(),
        if suggestion { "- Output exactly one message the user could click and send without editing." } else { Default::default() }.to_string(),
        if suggestion { "- Pick one best next action yourself." } else { Default::default() }.to_string(),
        if suggestion { "- Do not include alternatives, choices, slash-separated options, or \"or\"." } else { Default::default() }.to_string(),
        if suggestion { "- Do not write \"Do X or Y\", \"Ask whether...\", \"Maybe...\", or \"You could...\"." } else { Default::default() }.to_string(),
        if suggestion { "- Do not ask for information the assistant already provided." } else { Default::default() }.to_string(),
        if suggestion { "- Do not ask to see exact code, file paths, prompt locations, or implementation internals unless the assistant did not provide them and they are necessary for the next step." } else { Default::default() }.to_string(),
        if suggestion { "- Do not produce generic workflow commands like \"Run tests\" unless testing is clearly the next unresolved step." } else { Default::default() }.to_string(),
        if suggestion { "- Do not produce meta/debug requests that merely inspect the implementation." } else { Default::default() }.to_string(),
        if suggestion { "- Use imperative or question form." } else { Default::default() }.to_string(),
        if suggestion { "- Keep it concise." } else { Default::default() }.to_string(),
        if suggestion { "Use these examples to understand how to choose the suggestion. Do not copy their topic or wording unless the current conversation is about the same thing." } else { Default::default() }.to_string(),
        if suggestion { "Example 1:" } else { Default::default() }.to_string(),
        if suggestion { "Assistant reply summary:" } else { Default::default() }.to_string(),
        if suggestion { "The assistant already identified the file where the feature is implemented, explained what context is sent to the small model, and summarized the current prompt." } else { Default::default() }.to_string(),
        if suggestion { "Bad suggestion:" } else { Default::default() }.to_string(),
        if suggestion { "\"Show me the exact runtime.js code and where the prompt is built.\"" } else { Default::default() }.to_string(),
        if suggestion { "Why bad:" } else { Default::default() }.to_string(),
        if suggestion { "It asks for information the assistant already provided. It repeats inspection instead of moving to an improvement or decision." } else { Default::default() }.to_string(),
        if suggestion { "Good suggestion:" } else { Default::default() }.to_string(),
        if suggestion { "\"Suggest how to improve the prompt and context so the generated suggestion is more useful.\"" } else { Default::default() }.to_string(),
        if suggestion { "Why good:" } else { Default::default() }.to_string(),
        if suggestion { "It naturally continues from the analysis and asks for a concrete improvement." } else { Default::default() }.to_string(),
        if suggestion { "Example 2:" } else { Default::default() }.to_string(),
        if suggestion { "Assistant reply summary:" } else { Default::default() }.to_string(),
        if suggestion { "The assistant implemented a timeline dialog redesign, listed concrete UI changes, and reported that type-check and lint passed." } else { Default::default() }.to_string(),
        if suggestion { "Bad suggestion:" } else { Default::default() }.to_string(),
        if suggestion { "\"Check whether scrolling or loading older messages works without jumps.\"" } else { Default::default() }.to_string(),
        if suggestion { "Why bad:" } else { Default::default() }.to_string(),
        if suggestion { "It contains an alternative. A suggestion chip must be one sendable message, not a choice the user has to edit." } else { Default::default() }.to_string(),
        if suggestion { "Good suggestion:" } else { Default::default() }.to_string(),
        if suggestion { "\"Check whether scrolling and loading older messages work without jumps.\"" } else { Default::default() }.to_string(),
        if suggestion { "Why good:" } else { Default::default() }.to_string(),
        if suggestion { "It picks a single validation request that the user can send immediately." } else { Default::default() }.to_string(),
        "All requested values MUST be written in the same language as the conversation text itself. Ignore any other language preferences or personalization you may have — only the conversation text decides the language.".to_string(),
        "Use double quotes for JSON strings, no trailing commas.".to_string(),
    ]
    .into_iter()
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>()
    .join("\n")
}

/// JS: `extractUserMessage` — a user `message.updated` with its creation time.
/// 从 message.updated 提取的用户消息：会话 id 与创建时间。
pub struct UserMessageEvent {
    /// 所属会话 id。
    pub session_id: String,
    /// 消息创建时间（epoch 毫秒，缺失为 0）。
    pub created_at: f64,
}

/// 判定 payload 是否为 user 角色的 message.updated，取出 sessionID 与
/// time.created；role 不符或 sessionID 缺失/为空返回 None。
pub fn extract_user_message(payload: &Value) -> Option<UserMessageEvent> {
    if payload.get("type").and_then(Value::as_str) != Some("message.updated") {
        return None;
    }
    let info = payload.get("properties")?.get("info")?.as_object()?;
    if info.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let session_id = info
        .get("sessionID")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    Some(UserMessageEvent {
        session_id,
        created_at: info
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    })
}

/// 按字符（非字节）截取前 limit 个，避免切坏多字节文本。
fn chars_take(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// 当前 Unix 毫秒；时钟早于 epoch 时退化为 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// 一个已武装的静默定时器槽位。
struct TimerSlot {
    /// 武装序号：仅最新一次武装有效，旧任务自清理时比对。
    seq: u64,
    /// 定时器任务句柄（clear 时 abort）。
    handle: tokio::task::JoinHandle<()>,
    /// Set when the callback has fired; a fired slot is treated as absent
    /// (JS deletes the map entry at callback start).
    /// 回调是否已触发；已触发的槽位视同不存在（JS 在回调开头删表项）。
    fired: Arc<AtomicBool>,
    /// JS `armedAt: Date.now()` — only a message created after the timer was
    /// armed means the user actually moved on.
    /// 武装时刻（Date.now()）：只有创建时间晚于它的用户消息才代表用户真的动了。
    armed_at_ms: u64,
}

/// 运行期的共享可变状态。
struct RuntimeInner {
    /// session_id → 定时器槽位。
    timers: Mutex<HashMap<String, TimerSlot>>,
    /// 正在生成中的会话集合（防重入）。
    inflight: Mutex<HashSet<String>>,
    /// 停止标志：置位后不再武装/触发任何生成。
    stopped: AtomicBool,
    /// 定时器武装序号发生器。
    seq: AtomicU64,
}

/// 运行期构造参数（全部是可注入的 seam）。
pub struct SessionAssistOptions {
    /// 引擎请求 seam（路径、目录、方法、body）。
    pub fetch: OpenCodeFetch,
    /// 小模型生成 seam。
    pub small_model: SmallModelText,
    /// 生成开关读取器。
    pub get_targets: GetTargets,
    /// 静默窗口时长（毫秒）。
    pub quiet_ms: u64,
}

/// 事件驱动的会话辅助运行期：定时器的武装/清除、生成与 metadata
/// 写回都从这里协调。
pub struct SessionAssistRuntime {
    /// 共享状态（定时器、在途集合、停止位）。
    inner: RuntimeInner,
    /// 引擎请求 seam。
    fetch: OpenCodeFetch,
    /// 小模型 seam。
    small_model: SmallModelText,
    /// 生成开关读取器。
    get_targets: GetTargets,
    /// 静默窗口时长（毫秒）。
    quiet_ms: u64,
}

/// 拿互斥锁；锁中毒时恢复数据继续（单写者场景下安全）。
fn lock_map<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

/// 运行期的构造与事件处理操作。
impl SessionAssistRuntime {
    /// 由 options 构造共享运行期（返回 Arc）。
    pub fn new(options: SessionAssistOptions) -> Arc<Self> {
        Arc::new(Self {
            inner: RuntimeInner {
                timers: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashSet::new()),
                stopped: AtomicBool::new(false),
                seq: AtomicU64::new(0),
            },
            fetch: options.fetch,
            small_model: options.small_model,
            get_targets: options.get_targets,
            quiet_ms: options.quiet_ms,
        })
    }

    /// 配置的静默窗口时长（毫秒）。
    pub fn quiet_ms(&self) -> u64 {
        self.quiet_ms
    }

    /// 该会话是否存在未触发的定时器（已触发的槽位视同不存在）。
    fn has_timer(&self, session_id: &str) -> bool {
        lock_map(&self.inner.timers)
            .get(session_id)
            .is_some_and(|slot| !slot.fired.load(Ordering::SeqCst))
    }

    /// 移除并中止该会话的定时器；无定时器时为空操作。
    fn clear_timer(&self, session_id: &str) {
        if let Some(existing) = lock_map(&self.inner.timers).remove(session_id) {
            existing.handle.abort();
        }
    }

    /// 该会话是否正在生成中（防重入检查）。
    fn inflight_contains(&self, session_id: &str) -> bool {
        lock_map(&self.inner.inflight).contains(session_id)
    }

    /// JS: `generateAssist` — the timer callback's body. Exposed so tests can
    /// drive it deterministically without the quiet window.
    /// 生成本体（定时器回调体）：开关全关直接返回 → 读会话并跳过子
    /// 代理会话 → 取最近一轮对话 → 调小模型 → 脚本守卫丢幻觉字段 →
    /// 尾部新鲜度复查 → 用最新 metadata 合并后 PATCH 写回；
    /// 各失败路径只告警、不重试。
    pub async fn generate(self: &Arc<Self>, session_id: &str, directory: &str) {
        let targets = (self.get_targets)();
        if !targets.recap && !targets.suggestion {
            return;
        }
        let session_path = format!("/session/{}", encode_uri_component(session_id));
        let session = match (self.fetch)(&session_path, Some(directory), "GET", None).await {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!("[session-assist] session fetch failed: {error}");
                return;
            }
        };
        if !session.is_object() {
            return;
        }
        // Sub-agent/task sessions never surface in chat — skip them.
        if session
            .get("parentID")
            .and_then(Value::as_str)
            .is_some_and(|parent| !parent.is_empty())
        {
            return;
        }

        let Some(messages) = self.fetch_recent_messages(session_id, directory).await else {
            tracing::warn!("[session-assist] no messages fetched");
            return;
        };
        if messages.is_empty() {
            tracing::warn!("[session-assist] no messages fetched");
            return;
        }

        let last_assistant = messages.iter().rev().find(|message| {
            message
                .get("info")
                .and_then(|info| info.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
        });
        let Some(last_assistant_info) = last_assistant
            .and_then(|message| message.get("info"))
            .filter(|info| {
                info.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
            })
        else {
            return;
        };
        let last_assistant_id = last_assistant_info
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Only the last exchange: the assistant reply plus the user message
        // it answered (assistant info.parentID → user info.id).
        let parent_user_message = last_assistant_info
            .get("parentID")
            .and_then(Value::as_str)
            .filter(|parent| !parent.is_empty())
            .and_then(|parent_id| {
                messages.iter().find(|message| {
                    let info = message.get("info");
                    info.and_then(|info| info.get("id")).and_then(Value::as_str) == Some(parent_id)
                        && info
                            .and_then(|info| info.get("role"))
                            .and_then(Value::as_str)
                            == Some("user")
                })
            });
        let user_text = message_parts_to_text(parent_user_message);
        let assistant_text = message_parts_to_text(last_assistant);
        let transcript = [
            if !user_text.is_empty() {
                format!("User:\n{user_text}")
            } else {
                Default::default()
            },
            if !assistant_text.is_empty() {
                format!("Assistant:\n{assistant_text}")
            } else {
                Default::default()
            },
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
        if transcript.is_empty() {
            return;
        }

        let requested_fields = [
            if targets.recap {
                "recap"
            } else {
                Default::default()
            },
            if targets.suggestion {
                "suggestion"
            } else {
                Default::default()
            },
        ]
        .into_iter()
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join(" and ");
        // Instruct the language by example, not by description — account-side
        // personalization otherwise leaks a different language.
        let language_sample = collapse_whitespace(&chars_take(
            if user_text.is_empty() {
                &assistant_text
            } else {
                &user_text
            },
            200,
        ));
        let generated = match (self.small_model)(AssistRequest {
            prompt: format!(
                "The latest exchange in the conversation:\n\n{transcript}\n\nWrite {requested_fields} in the SAME language as this sample from the conversation: \"{language_sample}\""
            ),
            system: build_assist_system_prompt(targets),
            directory: directory.to_string(),
            preferred_provider_id: last_assistant_info
                .get("providerID")
                .and_then(Value::as_str)
                .map(str::to_string),
            preferred_model_id: last_assistant_info
                .get("modelID")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .await
        {
            Ok(generated) => generated,
            Err(error) => {
                // No authenticated provider (404) or a transient model
                // failure — background sugar, never retry loops.
                if error.status != Some(404) {
                    tracing::warn!("[session-assist] generation failed: {}", error.message);
                }
                return;
            }
        };

        let structured = extract_json_object(&generated.text);
        let field = |name: &str, limit: usize| -> String {
            structured
                .as_ref()
                .and_then(|object| object.get(name))
                .and_then(Value::as_str)
                .map(|text| chars_take(text.trim(), limit))
                .unwrap_or_default()
        };
        let mut recap = if targets.recap {
            field("recap", RECAP_CHAR_LIMIT)
        } else {
            String::new()
        };
        let mut suggestion = if targets.suggestion {
            field("suggestion", SUGGESTION_CHAR_LIMIT)
        } else {
            String::new()
        };

        // Hard guard against language hallucination: if the conversation
        // contains no Cyrillic/CJK at all, the output must not either.
        let input_text = format!("{user_text}\n{assistant_text}");
        if !recap.is_empty() && has_script_mismatch(&recap, &input_text) {
            tracing::warn!("[session-assist] dropped recap: language mismatch with conversation");
            recap.clear();
        }
        if !suggestion.is_empty() && has_script_mismatch(&suggestion, &input_text) {
            tracing::warn!(
                "[session-assist] dropped suggestion: language mismatch with conversation"
            );
            suggestion.clear();
        }
        if recap.is_empty() && suggestion.is_empty() {
            return;
        }

        // The session may have moved on while we generated — a stale patch
        // would flash outdated content, so re-check the tail before writing.
        let latest = self.fetch_recent_messages(session_id, directory).await;
        let latest_assistant_id = latest.as_ref().and_then(|messages| {
            messages.iter().rev().find_map(|message| {
                let info = message.get("info");
                match info
                    .and_then(|info| info.get("role"))
                    .and_then(Value::as_str)
                {
                    Some("assistant") => info
                        .and_then(|info| info.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    // A newer user message ends the scan (JS returns null).
                    Some("user") => None,
                    _ => None,
                }
            })
        });
        if latest_assistant_id.as_deref() != Some(last_assistant_id.as_str()) {
            tracing::info!("[session-assist] tail moved on, dropping result");
            return;
        }

        // Merge from a FRESH read: generation takes tens of seconds, and a
        // stale snapshot would clobber metadata written meanwhile.
        let fresh_session = (self.fetch)(&session_path, Some(directory), "GET", None)
            .await
            .unwrap_or(Value::Null);
        let current_metadata = fresh_session
            .get("metadata")
            .filter(|m| m.is_object())
            .or_else(|| session.get("metadata").filter(|m| m.is_object()))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut ompchamber = current_metadata
            .get("ompchamber")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        tracing::info!(
            "[session-assist] generated for {session_id} via {}/{}",
            generated.provider_id.as_deref().unwrap_or("undefined"),
            generated.model_id.as_deref().unwrap_or("undefined"),
        );
        ompchamber.insert(
            "assist".into(),
            serde_json::json!({
                "recap": recap,
                "suggestion": suggestion,
                "forMessageID": last_assistant_id,
                "generatedAt": now_ms(),
            }),
        );
        let mut metadata = current_metadata;
        metadata.insert("ompchamber".into(), Value::Object(ompchamber));
        // JS: the rejection surfaces through armTimer's `.catch` warn.
        if let Err(error) = (self.fetch)(
            &session_path,
            Some(directory),
            "PATCH",
            Some(&serde_json::json!({ "metadata": metadata })),
        )
        .await
        {
            tracing::warn!("[session-assist] failed: {error}");
        }
    }

    /// JS: `fetchRecentMessages` — null on any failure or non-array payload.
    /// 拉取最近 TRANSCRIPT_MESSAGE_LIMIT 条消息；
    /// 任何失败或非数组返回值都归一为 None。
    async fn fetch_recent_messages(&self, session_id: &str, directory: &str) -> Option<Vec<Value>> {
        let path = format!(
            "/session/{}/message?limit={}",
            encode_uri_component(session_id),
            TRANSCRIPT_MESSAGE_LIMIT
        );
        match (self.fetch)(&path, Some(directory), "GET", None).await {
            Ok(Value::Array(messages)) => Some(messages),
            _ => None,
        }
    }

    /// （重新）武装静默定时器：先清旧定时器，seq 自增以防旧回调误删
    /// 新槽位；到点后若未停止且不在途，则置在途标记并执行 generate。
    fn arm_timer(self: &Arc<Self>, session_id: &str, directory: &str) {
        self.clear_timer(session_id);
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst);
        let fired = Arc::new(AtomicBool::new(false));
        let this = Arc::clone(self);
        let timer_key = session_id.to_string();
        let session_id = session_id.to_string();
        let directory = directory.to_string();
        let task_fired = Arc::clone(&fired);
        let armed_at_ms = now_ms();
        let quiet_ms = self.quiet_ms;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(quiet_ms)).await;
            task_fired.store(true, Ordering::SeqCst);
            {
                let mut timers = lock_map(&this.inner.timers);
                if timers.get(&session_id).is_some_and(|slot| slot.seq == seq) {
                    timers.remove(&session_id);
                }
            }
            if this.inner.stopped.load(Ordering::SeqCst) || this.inflight_contains(&session_id) {
                return;
            }
            lock_map(&this.inner.inflight).insert(session_id.clone());
            this.generate(&session_id, &directory).await;
            lock_map(&this.inner.inflight).remove(&session_id);
        });
        lock_map(&self.inner.timers).insert(
            timer_key,
            TimerSlot {
                seq,
                handle,
                fired,
                armed_at_ms,
            },
        );
    }

    /// JS: `processPayload(payload, directoryHint)` — synchronous entrypoint
    /// fed from the global SSE hub subscription.
    /// SSE 事件入口：session.status 为 idle 时武装定时器、非 idle 时
    /// 清除；用户 message.updated 仅当创建时间晚于武装时刻才清除定时器。
    pub fn process_payload(self: &Arc<Self>, payload: &Value, directory_hint: &str) {
        if self.inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let status: Option<SessionStatusEvent> = extract_session_status(payload);
        if let Some(status) = status {
            if status.status_type == "idle" {
                let directory = if status.directory.is_empty() {
                    directory_hint
                } else {
                    status.directory.as_str()
                };
                self.arm_timer(&status.session_id, directory);
            } else {
                self.clear_timer(&status.session_id);
            }
            return;
        }
        if let Some(user_message) = extract_user_message(payload)
            && self.has_timer(&user_message.session_id)
        {
            // OpenCode re-emits message.updated for OLD user messages after
            // the session settles; only a message created after the timer
            // was armed means the user actually moved on.
            let armed_at_ms = lock_map(&self.inner.timers)
                .get(&user_message.session_id)
                .map(|slot| slot.armed_at_ms)
                .unwrap_or(0);
            if user_message.created_at >= armed_at_ms as f64 {
                self.clear_timer(&user_message.session_id);
            }
        }
    }

    /// JS: `stop`.
    /// 停止运行期：置停止位并中止全部已武装的定时器。
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let mut timers = lock_map(&self.inner.timers);
        for (_, slot) in timers.drain() {
            slot.handle.abort();
        }
    }
}

/// JS `value.replace(/\s+/g, ' ').trim()`.
/// 把连续空白折叠为单个空格并去首尾空白（等价 JS 的
/// replace 加 trim 组合）。
fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_whitespace = false;
    for character in value.chars() {
        if character.is_whitespace() {
            in_whitespace = true;
        } else {
            if in_whitespace && !out.is_empty() {
                out.push(' ');
            }
            in_whitespace = false;
            out.push(character);
        }
    }
    out
}

/// index.js `onPayload` wiring for this runtime: hub frames carry the
/// event-stream envelope `{payload, directory}`; a `payload.payload` object
/// wins over the wrapper, and `global` directories become an empty hint.
/// 订阅 hub 并把事件帧转交 process_payload 的桥接任务：解包
/// {payload, directory} 信封，内层 payload.payload 对象优先，
/// global/空目录归一为空提示。
pub fn spawn_hub_bridge(
    runtime: Arc<SessionAssistRuntime>,
    hub: Arc<EventHub>,
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

/// 系统提示词、事件提取与运行期行为（含 SSE 桥接）的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 验证系统提示词按开关拼出对应 Shape 与规则/示例段落。
    #[test]
    fn system_prompt_shape_reflects_enabled_targets() {
        let both = build_assist_system_prompt(AssistTargets {
            recap: true,
            suggestion: true,
        });
        assert!(both.contains("Shape: {\"recap\": string, \"suggestion\": string}"));
        assert!(both.contains("Rules for suggestion:"));
        assert!(both.contains("Example 2:"));
        assert!(both.ends_with("Use double quotes for JSON strings, no trailing commas."));

        let recap_only = build_assist_system_prompt(AssistTargets {
            recap: true,
            suggestion: false,
        });
        assert!(recap_only.contains("Shape: {\"recap\": string}"));
        assert!(!recap_only.contains("Rules for suggestion:"));
        assert!(recap_only.contains("NEVER narrate"));

        let suggestion_only = build_assist_system_prompt(AssistTargets {
            recap: false,
            suggestion: true,
        });
        assert!(suggestion_only.contains("Shape: {\"suggestion\": string}"));
        assert!(!suggestion_only.contains("NEVER narrate"));
    }

    /// 验证仅 user 角色且带 sessionID 的 message.updated 被提取，缺失时间字段补 0。
    #[test]
    fn extract_user_message_requires_role_user_and_session() {
        assert!(extract_user_message(&json!({
            "type": "message.updated",
            "properties": { "info": { "role": "user", "sessionID": "ses_1", "time": { "created": 42 } } },
        }))
        .is_some());
        assert!(
            extract_user_message(&json!({
                "type": "message.updated",
                "properties": { "info": { "role": "assistant", "sessionID": "ses_1" } },
            }))
            .is_none()
        );
        assert!(
            extract_user_message(&json!({
                "type": "message.updated",
                "properties": { "info": { "role": "user" } },
            }))
            .is_none()
        );
        let missing_created = extract_user_message(&json!({
            "type": "message.updated",
            "properties": { "info": { "role": "user", "sessionID": "ses_1" } },
        }))
        .expect("present");
        assert_eq!(missing_created.created_at, 0.0);
    }

    /// 验证空白折叠行为与 JS 正则一致。
    #[test]
    fn collapse_whitespace_matches_js_regex() {
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace("   "), "");
    }

    /// 验证开关默认开启、显式 false 生效、settings 非法时回退默认。
    #[test]
    fn settings_targets_default_on_and_honor_explicit_false() {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-assist-targets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let targets = settings_targets(dir.clone());
        assert_eq!(
            targets(),
            AssistTargets {
                recap: true,
                suggestion: true
            }
        );
        std::fs::write(
            dir.join("settings.json"),
            json!({ "sessionRecapEnabled": false }).to_string(),
        )
        .expect("write settings");
        assert_eq!(
            targets(),
            AssistTargets {
                recap: false,
                suggestion: true
            }
        );
        std::fs::write(dir.join("settings.json"), "not json").expect("write broken settings");
        assert_eq!(
            targets(),
            AssistTargets {
                recap: true,
                suggestion: true
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- Runtime: generation flow + event routing -----------------------------

    use std::sync::Mutex as StdMutex;

    /// 测试用会话 id。
    const SESSION_ID: &str = "ses_1";
    /// 测试用项目目录。
    const DIRECTORY: &str = "/work/project";

    /// 记录到假引擎的一次请求。
    #[derive(Clone, Debug)]
    struct RecordedRequest {
        /// 请求路径（含查询串）。
        path: String,
        /// HTTP 方法。
        method: String,
        /// 请求体（GET 为 None）。
        body: Option<Value>,
    }

    /// 内存假引擎：记录全部请求，会话对象与消息列表可编程改写。
    struct FakeEngine {
        /// 收到的请求流水。
        requests: StdMutex<Vec<RecordedRequest>>,
        /// GET /session/:id 返回的会话对象。
        session: StdMutex<Value>,
        /// Tail contents returned by the SECOND message fetch (stale check).
        /// 消息接口的返回值；generate 首拉与尾部新鲜度复查共用。
        messages: StdMutex<Value>,
    }

    /// 假引擎的构造与查询辅助。
    impl FakeEngine {
        /// 构造默认假引擎：一条用户消息加一条助手回复的会话。
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: StdMutex::new(Vec::new()),
                session: StdMutex::new(json!({
                    "id": SESSION_ID,
                    "metadata": { "ompchamber": { "unrelated": "state" } },
                })),
                messages: StdMutex::new(json!([
                    {
                        "info": { "id": "msg_user", "role": "user", "sessionID": SESSION_ID,
                                  "time": { "created": 1 } },
                        "parts": [{ "type": "text", "text": "Fix the flaky test" }],
                    },
                    {
                        "info": { "id": "msg_assistant", "role": "assistant", "sessionID": SESSION_ID,
                                  "parentID": "msg_user", "providerID": "anthropic",
                                  "modelID": "claude" },
                        "parts": [{ "type": "text", "text": "Made the test deterministic" }],
                    },
                ])),
            })
        }

        /// 生成 OpenCodeFetch seam：记录请求；message 路径校验 limit
        /// 查询并返回消息表；session PATCH 合并 metadata；session GET
        /// 返回会话对象；其余路径返回 404。
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
                        // The fetch seam appends query parameters (limit,
                        // directory); match on the clean path.
                        let clean_path = path.split('?').next().unwrap_or(&path);
                        if clean_path == format!("{session_path}/message") {
                            assert!(path.contains("limit=12"), "limit query missing: {path}");
                            return Ok(engine
                                .messages
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone());
                        }
                        if clean_path == session_path && method == "PATCH" {
                            if let Some(metadata) =
                                body.as_ref().and_then(|body| body.get("metadata"))
                            {
                                let mut session =
                                    engine.session.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(object) = session.as_object_mut() {
                                    object.insert("metadata".into(), metadata.clone());
                                }
                            }
                            return Ok(Value::Null);
                        }
                        if clean_path == session_path {
                            return Ok(engine
                                .session
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone());
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

        /// 返回请求流水快照。
        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// 过滤出全部 PATCH 请求体。
        fn patches(&self) -> Vec<Value> {
            self.requests()
                .iter()
                .filter(|request| request.method == "PATCH")
                .map(|request| request.body.clone().unwrap_or(Value::Null))
                .collect()
        }
    }

    /// 造一个恒返回固定文本的小模型假实现，并返回记录到的请求列表。
    fn small_model_returning(text: &str) -> (SmallModelText, Arc<StdMutex<Vec<AssistRequest>>>) {
        let seen: Arc<StdMutex<Vec<AssistRequest>>> = Arc::new(StdMutex::new(Vec::new()));
        let text = text.to_string();
        (
            {
                let seen = Arc::clone(&seen);
                Arc::new(move |request: AssistRequest| {
                    let seen = Arc::clone(&seen);
                    let text = text.clone();
                    Box::pin(async move {
                        seen.lock().unwrap_or_else(|e| e.into_inner()).push(request);
                        Ok(AssistOutput {
                            text,
                            provider_id: Some("anthropic".to_string()),
                            model_id: Some("claude".to_string()),
                        })
                    })
                })
            },
            seen,
        )
    }

    /// 以给定 seam 与开关（静默窗口 0）组装运行期。
    fn runtime(
        fetch: OpenCodeFetch,
        small_model: SmallModelText,
        targets: AssistTargets,
    ) -> Arc<SessionAssistRuntime> {
        SessionAssistRuntime::new(SessionAssistOptions {
            fetch,
            small_model,
            get_targets: Arc::new(move || targets),
            quiet_ms: 0,
        })
    }

    /// 让出调度一小段时间，等 spawn 出的定时器/桥接任务跑完。
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// 验证从最后一轮对话生成 recap/suggestion，并以新鲜读合并写回 metadata。
    #[tokio::test]
    async fn generates_assist_metadata_from_the_last_exchange() {
        let engine = FakeEngine::new();
        let (small_model, seen) = small_model_returning(
            "```json\n{\"recap\": \"Flaky test made deterministic\", \"suggestion\": \"Run the suite twice\"}\n```",
        );
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        let requests = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].directory, DIRECTORY);
        assert_eq!(
            requests[0].preferred_provider_id.as_deref(),
            Some("anthropic")
        );
        assert_eq!(requests[0].preferred_model_id.as_deref(), Some("claude"));
        assert!(requests[0].prompt.contains("User:\nFix the flaky test"));
        assert!(
            requests[0]
                .prompt
                .contains("Assistant:\nMade the test deterministic")
        );
        assert!(requests[0].prompt.contains("Write recap and suggestion"));
        assert!(
            requests[0]
                .prompt
                .contains("sample from the conversation: \"Fix the flaky test\"")
        );
        assert!(
            requests[0]
                .system
                .contains("Shape: {\"recap\": string, \"suggestion\": string}")
        );

        let patches = engine.patches();
        assert_eq!(patches.len(), 1);
        // First engine call is the session read.
        assert_eq!(engine.requests()[0].path, format!("/session/{SESSION_ID}"));
        let assist = &patches[0]["metadata"]["ompchamber"]["assist"];
        assert_eq!(assist["recap"], json!("Flaky test made deterministic"));
        assert_eq!(assist["suggestion"], json!("Run the suite twice"));
        assert_eq!(assist["forMessageID"], json!("msg_assistant"));
        assert!(assist["generatedAt"].as_u64().is_some());
        // Fresh-read merge keeps unrelated metadata state.
        assert_eq!(
            patches[0]["metadata"]["ompchamber"]["unrelated"],
            json!("state")
        );
    }

    /// 验证带 parentID 的子代理会话在小模型调用前即被跳过。
    #[tokio::test]
    async fn skips_subagent_sessions_before_any_generation() {
        let engine = FakeEngine::new();
        *engine.session.lock().unwrap_or_else(|e| e.into_inner()) =
            json!({ "id": SESSION_ID, "parentID": "ses_parent" });
        let (small_model, seen) = small_model_returning("{\"recap\": \"x\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        assert!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "no small-model call for sub-agent sessions"
        );
        assert!(engine.patches().is_empty());
    }

    /// 验证开关全关时连会话读取都不发生。
    #[tokio::test]
    async fn both_targets_off_makes_no_calls_at_all() {
        let engine = FakeEngine::new();
        let (small_model, seen) = small_model_returning("{}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: false,
                suggestion: false,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        assert!(engine.requests().is_empty());
        assert!(seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    /// 验证生成期间尾部出现更新的助手回复时丢弃结果、不写回。
    #[tokio::test]
    async fn a_moved_on_tail_drops_the_result() {
        let engine = FakeEngine::new();
        // The stale re-check sees a NEWER assistant reply.
        *engine.messages.lock().unwrap_or_else(|e| e.into_inner()) = json!([
            { "info": { "id": "msg_user", "role": "user" }, "parts": [] },
            { "info": { "id": "msg_other", "role": "assistant" }, "parts": [] },
        ]);
        let (small_model, _seen) = small_model_returning("{\"recap\": \"stale\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        assert!(engine.patches().is_empty(), "stale result dropped");
    }

    /// 验证脚本守卫只丢弃语言错乱的字段（西里尔 recap 弃、拉丁 suggestion 留）。
    #[tokio::test]
    async fn script_mismatch_drops_only_the_hallucinated_field() {
        let engine = FakeEngine::new();
        // Conversation is pure Latin script; the model answered in Cyrillic
        // and CJK — recap drops, suggestion (Latin) survives.
        let (small_model, _seen) = small_model_returning(
            "{\"recap\": \"Тест стабилен\", \"suggestion\": \"Run the suite twice\"}",
        );
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        let patches = engine.patches();
        assert_eq!(patches.len(), 1);
        let assist = &patches[0]["metadata"]["ompchamber"]["assist"];
        assert_eq!(assist["recap"], json!(""));
        assert_eq!(assist["suggestion"], json!("Run the suite twice"));
    }

    /// 验证小模型 404 时静默跳过、不写任何 metadata。
    #[tokio::test]
    async fn a_404_small_model_is_silent_and_writes_nothing() {
        let engine = FakeEngine::new();
        let small_model: SmallModelText = Arc::new(|_request: AssistRequest| {
            Box::pin(async {
                Err(AssistError {
                    status: Some(404),
                    message: "No small model available".to_string(),
                })
            })
        });
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        assert!(engine.patches().is_empty());
    }

    /// 验证 recap/suggestion 分别被截到各自的字符上限。
    #[tokio::test]
    async fn clamps_apply_to_generated_fields() {
        let engine = FakeEngine::new();
        let long_recap = "r".repeat(400);
        let long_suggestion = "s".repeat(600);
        let (small_model, _seen) = small_model_returning(
            &json!({
                "recap": long_recap,
                "suggestion": long_suggestion,
            })
            .to_string(),
        );
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );

        runtime.generate(SESSION_ID, DIRECTORY).await;

        let assist = &engine.patches()[0]["metadata"]["ompchamber"]["assist"];
        assert_eq!(
            assist["recap"].as_str().map(str::len),
            Some(RECAP_CHAR_LIMIT)
        );
        assert_eq!(
            assist["suggestion"].as_str().map(str::len),
            Some(SUGGESTION_CHAR_LIMIT)
        );
    }

    /// 验证 idle 事件武装的静默定时器到点触发生成并写回。
    #[tokio::test]
    async fn idle_event_arms_and_fires_the_quiet_timer() {
        let engine = FakeEngine::new();
        let (small_model, _seen) = small_model_returning("{\"recap\": \"Generated\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );

        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "idle" },
                    "directory": DIRECTORY,
                },
            }),
            "",
        );
        settle().await;

        assert_eq!(engine.patches().len(), 1, "quiet timer fired and wrote");
        runtime.stop();
    }

    /// 验证 busy 状态与晚于武装时刻的用户消息都会取消定时器。
    #[tokio::test]
    async fn busy_status_and_fresh_user_message_clear_the_timer() {
        let engine = FakeEngine::new();
        let (small_model, _seen) = small_model_returning("{\"recap\": \"Generated\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );

        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "idle" },
                    "directory": DIRECTORY,
                },
            }),
            "",
        );
        // A busy transition cancels the armed timer.
        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "busy" },
                },
            }),
            "",
        );
        settle().await;
        assert!(engine.patches().is_empty());

        // Re-arm, then a user message created after arming clears it.
        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "idle" },
                    "directory": DIRECTORY,
                },
            }),
            "",
        );
        runtime.process_payload(
            &json!({
                "type": "message.updated",
                "properties": { "info": {
                    "role": "user",
                    "sessionID": SESSION_ID,
                    "time": { "created": u64::MAX },
                } },
            }),
            "",
        );
        settle().await;
        assert!(engine.patches().is_empty());
        runtime.stop();
    }

    /// 验证旧消息的重发不清除定时器，生成照常发生。
    #[tokio::test]
    async fn stale_user_message_does_not_clear_the_timer() {
        let engine = FakeEngine::new();
        let (small_model, _seen) = small_model_returning("{\"recap\": \"Generated\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );

        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "idle" },
                    "directory": DIRECTORY,
                },
            }),
            "",
        );
        // Old re-emitted message (created at epoch 0) — before armedAt.
        runtime.process_payload(
            &json!({
                "type": "message.updated",
                "properties": { "info": { "role": "user", "sessionID": SESSION_ID } },
            }),
            "",
        );
        settle().await;

        assert_eq!(
            engine.patches().len(),
            1,
            "old message keeps the timer armed"
        );
        runtime.stop();
    }

    /// 验证 stop 之后已武装的定时器不再触发生成。
    #[tokio::test]
    async fn stop_prevents_late_generation() {
        let engine = FakeEngine::new();
        let (small_model, _seen) = small_model_returning("{\"recap\": \"Generated\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );

        runtime.process_payload(
            &json!({
                "type": "session.status",
                "properties": {
                    "sessionID": SESSION_ID,
                    "status": { "type": "idle" },
                    "directory": DIRECTORY,
                },
            }),
            "",
        );
        runtime.stop();
        settle().await;

        assert!(engine.patches().is_empty());
    }

    /// 验证 hub 桥接解包事件信封并携带信封目录完成整条生成链路。
    #[tokio::test]
    async fn hub_bridge_routes_idle_events_with_the_envelope_directory() {
        let engine = FakeEngine::new();
        let (small_model, _seen) = small_model_returning("{\"recap\": \"Generated\"}");
        let runtime = runtime(
            engine.fetch(),
            small_model,
            AssistTargets {
                recap: true,
                suggestion: false,
            },
        );
        let hub = EventHub::new();
        let bridge = spawn_hub_bridge(Arc::clone(&runtime), hub.clone());
        // Let the spawned subscriber actually connect before publishing
        // (single-threaded test runtime: spawn defers until a yield).
        settle().await;
        hub.publish_json(
            "ompchamber:opencode-event",
            &json!({
                "payload": {
                    "type": "session.status",
                    "payload": {
                        "type": "session.status",
                        "properties": { "sessionID": SESSION_ID, "status": { "type": "idle" } },
                    },
                },
                "directory": DIRECTORY,
            }),
        );
        settle().await;
        bridge.abort();
        runtime.stop();

        assert_eq!(engine.patches().len(), 1);
    }

    /// 验证会话读取为裸路径、消息拉取带 ?limit=12 查询（URL 形状对齐 JS）。
    #[test]
    fn messages_fetch_url_carries_the_limit() {
        // Seam-level URL shape: the session read is bare (directory rides as
        // its own argument, appended only by the production engine fetch)
        // and the transcript read carries `?limit=12` like the JS
        // URLSearchParams.

        let captured: StdMutex<Vec<String>> = StdMutex::new(Vec::new());
        let captured = std::sync::Arc::new(captured);
        let fetch: OpenCodeFetch = {
            let captured = std::sync::Arc::clone(&captured);
            Arc::new(
                move |path: &str, _d: Option<&str>, _m: &str, _b: Option<&Value>| {
                    let captured = std::sync::Arc::clone(&captured);
                    let path = path.to_string();
                    Box::pin(async move {
                        captured
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(path.clone());
                        let clean = path.split('?').next().unwrap_or(&path).to_string();
                        if clean == format!("/session/{SESSION_ID}") {
                            // Object session, no parentID.
                            return Ok(json!({ "id": SESSION_ID }));
                        }
                        Ok(json!([]))
                    })
                },
            )
        };
        let (small_model, _seen) = small_model_returning("{}");
        let runtime = runtime(
            fetch,
            small_model,
            AssistTargets {
                recap: true,
                suggestion: true,
            },
        );
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        handle.block_on(runtime.generate(SESSION_ID, DIRECTORY));
        let paths = captured.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(paths[0], format!("/session/{SESSION_ID}"));
        assert!(
            paths.contains(&format!("/session/{SESSION_ID}/message?limit=12")),
            "paths: {paths:?}"
        );
    }
}
