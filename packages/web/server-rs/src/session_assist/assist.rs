//! Port of `server/lib/session-assist/runtime.js` — after a session goes
//! idle and stays quiet for a minute, generate a short recap of the agent's
//! last reply plus one suggested user follow-up with the small model, and
//! store both on the session's metadata (`metadata.ompchamber.assist`).
//! Purely event-driven: no backfill, no session scans.

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
pub const IDLE_QUIET_MS: u64 = 60_000;
pub const TRANSCRIPT_MESSAGE_LIMIT: usize = 12;
pub const RECAP_CHAR_LIMIT: usize = 320;
pub const SUGGESTION_CHAR_LIMIT: usize = 500;
pub const FETCH_TIMEOUT_MS: u64 = 5_000;

// ---------------------------------------------------------------------------
// Settings gate (`sessionRecapEnabled` / `sessionSuggestionEnabled`)
// ---------------------------------------------------------------------------
/// The Chat settings are hard generation switches (default on): when both are
/// off, no small-model calls and no metadata writes happen at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistTargets {
    pub recap: bool,
    pub suggestion: bool,
}

pub type GetTargets = Arc<dyn Fn() -> AssistTargets + Send + Sync>;

/// JS `getSessionAssistTargets`: `settings.sessionRecapEnabled !== false`
/// (default on when settings cannot be read).
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
pub struct AssistRequest {
    pub prompt: String,
    pub system: String,
    pub directory: String,
    pub preferred_provider_id: Option<String>,
    pub preferred_model_id: Option<String>,
}

pub struct AssistOutput {
    pub text: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
}

/// Carries the JS `error.statusCode` (404 = no authenticated small model,
/// silently skipped).
#[derive(Debug)]
pub struct AssistError {
    pub status: Option<u16>,
    pub message: String,
}

pub type AssistFuture = Pin<Box<dyn Future<Output = Result<AssistOutput, AssistError>> + Send>>;
pub type SmallModelText = Arc<dyn Fn(AssistRequest) -> AssistFuture + Send + Sync>;

/// The small-model module is not ported yet: generation fails closed and the
/// assist flow follows the JS error path (404 — silent, nothing written).
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
pub struct UserMessageEvent {
    pub session_id: String,
    pub created_at: f64,
}

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

fn chars_take(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

struct TimerSlot {
    seq: u64,
    handle: tokio::task::JoinHandle<()>,
    /// Set when the callback has fired; a fired slot is treated as absent
    /// (JS deletes the map entry at callback start).
    fired: Arc<AtomicBool>,
    /// JS `armedAt: Date.now()` — only a message created after the timer was
    /// armed means the user actually moved on.
    armed_at_ms: u64,
}

struct RuntimeInner {
    timers: Mutex<HashMap<String, TimerSlot>>,
    inflight: Mutex<HashSet<String>>,
    stopped: AtomicBool,
    seq: AtomicU64,
}

pub struct SessionAssistOptions {
    pub fetch: OpenCodeFetch,
    pub small_model: SmallModelText,
    pub get_targets: GetTargets,
    pub quiet_ms: u64,
}

pub struct SessionAssistRuntime {
    inner: RuntimeInner,
    fetch: OpenCodeFetch,
    small_model: SmallModelText,
    get_targets: GetTargets,
    quiet_ms: u64,
}

fn lock_map<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

impl SessionAssistRuntime {
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

    pub fn quiet_ms(&self) -> u64 {
        self.quiet_ms
    }

    fn has_timer(&self, session_id: &str) -> bool {
        lock_map(&self.inner.timers)
            .get(session_id)
            .is_some_and(|slot| !slot.fired.load(Ordering::SeqCst))
    }

    fn clear_timer(&self, session_id: &str) {
        if let Some(existing) = lock_map(&self.inner.timers).remove(session_id) {
            existing.handle.abort();
        }
    }

    fn inflight_contains(&self, session_id: &str) -> bool {
        lock_map(&self.inner.inflight).contains(session_id)
    }

    /// JS: `generateAssist` — the timer callback's body. Exposed so tests can
    /// drive it deterministically without the quiet window.
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
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let mut timers = lock_map(&self.inner.timers);
        for (_, slot) in timers.drain() {
            slot.handle.abort();
        }
    }
}

/// JS `value.replace(/\s+/g, ' ').trim()`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn collapse_whitespace_matches_js_regex() {
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace("   "), "");
    }

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

    const SESSION_ID: &str = "ses_1";
    const DIRECTORY: &str = "/work/project";

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        method: String,
        body: Option<Value>,
    }

    struct FakeEngine {
        requests: StdMutex<Vec<RecordedRequest>>,
        session: StdMutex<Value>,
        /// Tail contents returned by the SECOND message fetch (stale check).
        messages: StdMutex<Value>,
    }

    impl FakeEngine {
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

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn patches(&self) -> Vec<Value> {
            self.requests()
                .iter()
                .filter(|request| request.method == "PATCH")
                .map(|request| request.body.clone().unwrap_or(Value::Null))
                .collect()
        }
    }

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

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

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
