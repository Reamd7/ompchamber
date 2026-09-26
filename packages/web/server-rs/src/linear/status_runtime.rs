//! Port of `server/lib/linear/status-runtime.js` — consumes OpenCode event
//! hub payloads: the first `session.status` idle posts a completed comment,
//! `session.error` (except user aborts) posts a failure comment.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use serde_json::Value;

use super::LinearState;
use super::parse::{is_plain_object, read_trimmed_string};
use super::status::{SessionStatusInput, post_linear_session_status};

fn read_properties(payload: &Value) -> &Value {
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(Default::default()));
    super::parse::as_plain_object(&payload["properties"]).unwrap_or(&EMPTY)
}

fn read_nested<'a>(properties: &'a Value, key: &str) -> &'a Value {
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(Default::default()));
    super::parse::as_plain_object(&properties[key]).unwrap_or(&EMPTY)
}

fn extract_session_id(payload: &Value) -> String {
    let properties = read_properties(payload);
    let info = read_nested(properties, "info");
    [
        read_trimmed_string(&info["sessionID"]),
        read_trimmed_string(&info["sessionId"]),
        read_trimmed_string(&properties["sessionID"]),
        read_trimmed_string(&properties["sessionId"]),
        read_trimmed_string(&properties["session"]),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_default()
}

fn extract_status_type(payload: &Value) -> String {
    if !is_plain_object(payload) || payload["type"] != Value::String("session.status".into()) {
        return String::new();
    }
    let properties = read_properties(payload);
    let status = read_nested(properties, "status");
    let info = read_nested(properties, "info");
    let from_status = read_trimmed_string(&status["type"]);
    if !from_status.is_empty() {
        return from_status;
    }
    read_trimmed_string(&info["type"])
}

fn extract_error_name(payload: &Value) -> String {
    if !is_plain_object(payload) || payload["type"] != Value::String("session.error".into()) {
        return String::new();
    }
    read_trimmed_string(&read_nested(read_properties(payload), "error")["name"])
}

pub struct LinearSessionStatusRuntime {
    state: Arc<LinearState>,
    stopped: AtomicBool,
}

impl LinearSessionStatusRuntime {
    pub fn new(state: Arc<LinearState>) -> Self {
        Self {
            state,
            stopped: AtomicBool::new(false),
        }
    }

    /// Feed one hub payload. Returns the spawned comment task (ignored by the
    /// JS caller) so tests can await the fire-and-forget post deterministically.
    pub fn process_payload(&self, payload: &Value) -> Option<tokio::task::JoinHandle<()>> {
        if self.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let session_id = extract_session_id(payload);
        if session_id.is_empty() {
            return None;
        }

        if is_plain_object(payload) && payload["type"] == Value::String("session.error".into()) {
            if extract_error_name(payload) == "MessageAbortedError" {
                return None;
            }
            let state = self.state.clone();
            let input = SessionStatusInput {
                kind: "failure".into(),
                session_id,
                ..SessionStatusInput::default()
            };
            return Some(tokio::spawn(async move {
                if let Err(error) = post_linear_session_status(&state, &input).await {
                    tracing::warn!(
                        "[linear] failed to post session failure comment: {}",
                        error.message
                    );
                }
            }));
        }

        if extract_status_type(payload) != "idle" {
            return None;
        }
        let state = self.state.clone();
        let input = SessionStatusInput {
            kind: "completed".into(),
            session_id,
            ..SessionStatusInput::default()
        };
        Some(tokio::spawn(async move {
            if let Err(error) = post_linear_session_status(&state, &input).await {
                tracing::warn!(
                    "[linear] failed to post session completed comment: {}",
                    error.message
                );
            }
        }))
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
