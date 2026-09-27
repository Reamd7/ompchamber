//! Port of `server/lib/linear/status-runtime.js` — consumes OpenCode event
//! hub payloads: the first `session.status` idle posts a completed comment,
//! `session.error` (except user aborts) posts a failure comment.
//! 本模块是 `server/lib/linear/status-runtime.js` 的 Rust 移植：消费
//! OpenCode 事件 hub 的 payload——首个 session.status idle 事件发一条
//! completed 评论，session.error（用户主动中止除外）发一条 failure 评论。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use serde_json::Value;

use super::LinearState;
use super::parse::{is_plain_object, read_trimmed_string};
use super::status::{SessionStatusInput, post_linear_session_status};

/// 读取 payload 的 properties 对象；缺失或非对象时返回进程级共享的空对象。
fn read_properties(payload: &Value) -> &Value {
    // 进程级共享的空对象：properties 缺失或非对象时的统一占位。
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(Default::default()));
    super::parse::as_plain_object(&payload["properties"]).unwrap_or(&EMPTY)
}

/// 读取 properties 内嵌套的子对象；缺失或非对象时返回进程级共享的空对象。
fn read_nested<'a>(properties: &'a Value, key: &str) -> &'a Value {
    // 进程级共享的空对象：嵌套子对象缺失或非对象时的统一占位。
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(Default::default()));
    super::parse::as_plain_object(&properties[key]).unwrap_or(&EMPTY)
}

/// 从 payload 的多个候选字段提取 session id（info.sessionID/sessionId 与
/// 顶层 sessionID/sessionId/session），全部为空时返回空串。
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

/// 仅对 session.status 事件生效：优先取 properties.status.type，
/// 其次 properties.info.type；其余事件返回空串。
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

/// 仅对 session.error 事件生效：读取 properties.error.name；其余事件返回空串。
fn extract_error_name(payload: &Value) -> String {
    if !is_plain_object(payload) || payload["type"] != Value::String("session.error".into()) {
        return String::new();
    }
    read_trimmed_string(&read_nested(read_properties(payload), "error")["name"])
}

/// 会话状态评论运行时：把 hub 的 session 事件转换为 Linear 评论（fire-and-forget）。
pub struct LinearSessionStatusRuntime {
    /// Linear 共享状态（授权与发评论的通道）。
    state: Arc<LinearState>,
    /// 停止标记；置位后不再处理任何 payload。
    stopped: AtomicBool,
}

/// 运行时的生命周期与事件入口。
impl LinearSessionStatusRuntime {
    /// 用共享状态构造运行时（初始处于运行状态）。
    pub fn new(state: Arc<LinearState>) -> Self {
        Self {
            state,
            stopped: AtomicBool::new(false),
        }
    }

    /// 处理一条 hub payload：session.error（MessageAbortedError 除外）派发
    /// failure 评论；session.status idle 派发 completed 评论。返回派发的
    /// 评论任务句柄（JS 调用方忽略它；测试用它确定性等待 fire-and-forget
    /// 的发送完成）。已停止、无 session id 或事件不匹配时返回 None。
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

    /// 停止运行时：之后所有 payload 都被忽略。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
