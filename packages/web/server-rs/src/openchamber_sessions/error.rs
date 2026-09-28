//! Port of the error surfaces `openchamber-sessions/routes.js` uses:
//! `OMPChamberControlError` / `asControlError` (from
//! `openchamber-control/error.js`) and the module's `sendServiceError`.
//!
//! JS models every failure as one `Error` instance carrying optional
//! `statusCode`, `goalConfigured`, and the partial-failure bookkeeping the
//! fork/send catch adds; [`SvcError`] keeps those fields in one type. The
//! wire mapping lives in [`send_service_error`], mirroring
//! `asControlError(error, fallback)` + `res.status(...).json({ error, ... })`.
//!
//! 中文说明：移植 routes.js 使用的错误面——`OMPChamberControlError` /
//! `asControlError` 与 `sendServiceError`。JS 用单个 Error 携带可选
//! `statusCode`、`goalConfigured` 与 send/fork catch 附加的部分失败记账；
//! `SvcError` 把这些字段收拢为一个类型，wire 映射集中在 `send_service_error`。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

/// `OMPChamberControlError` details block for partial dispatch failures:
/// `{ partial: true, partialAction, sessionId, directory }`.
///
/// 中文说明：派发中途失败时已产生副作用的记录块——fork 已创建或 goal 已
/// 配置时由 send/fork 的 catch 附加。
#[derive(Debug, Clone)]
pub struct PartialDetails {
    /// 部分失败类别：`"fork-created"` 或 `"goal-configured"`。
    pub action: String,
    /// 已创建/受影响的会话 ID。
    pub session_id: String,
    /// Serialized as JSON `null` when the failure predates directory
    /// resolution (the JS catch spreads `directory: null` explicitly).
    /// 目录解析前的失败序列化为 JSON `null`（JS catch 显式展开
    /// `directory: null`）。
    pub directory: Option<String>,
}

/// One `Error`-shaped failure: message, optional `statusCode`, the
/// `goalConfigured` flag `markGoalPartial` sets, and the partial block only
/// the send/fork catch produces.
///
/// 中文说明：JS 单个 Error 形状的失败——消息、可选状态码、goal 标志与
/// partial 块；无状态码的 `plain` 映射为 500，`control`/`with_status` 带码。
#[derive(Debug, Clone)]
pub struct SvcError {
    /// 错误消息；空串在 wire 层回退到 fallback 文本。
    pub message: String,
    /// 可选 HTTP 状态码；`None` 按 500 处理。
    pub status: Option<u16>,
    /// goal 元数据已配置标志（影响 partial 记账）。
    pub goal_configured: bool,
    /// 部分失败细节；仅 send/fork 失败路径附加。
    pub partial: Option<PartialDetails>,
}

/// 三种 JS 构造路径与 goal 部分失败标记。
impl SvcError {
    /// `new OMPChamberControlError(message, statusCode)`.
    ///
    /// 中文说明：对应 `new OMPChamberControlError(message, statusCode)`。
    pub fn control(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            goal_configured: false,
            partial: None,
        }
    }

    /// A plain `Error` without `statusCode` — `asControlError` maps these to
    /// 500.
    ///
    /// 中文说明：无状态码的普通 Error；`asControlError` 将其映射为 500。
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            goal_configured: false,
            partial: None,
        }
    }

    /// A plain `Error` carrying `error.statusCode = 400` (the "No model is
    /// configured" throw).
    ///
    /// 中文说明：带 `error.statusCode` 的普通 Error（如 "No model is
    /// configured" 的 400）。
    pub fn with_status(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            goal_configured: false,
            partial: None,
        }
    }

    /// `markGoalPartial`: tag a dispatch failure that happened after goal
    /// metadata was configured.
    ///
    /// 中文说明：goal 元数据配置后派发失败时打标，供 catch 记账。
    pub fn mark_goal_partial(mut self, enabled: bool) -> Self {
        if enabled {
            self.goal_configured = true;
        }
        self
    }
}

/// `sendServiceError(res, error, fallback)`: `asControlError` keeps the
/// message (falling back only when empty), resolves the status (`statusCode`
/// or 500), and emits the partial block when present.
///
/// 中文说明：消息为空才回退 fallback；状态取 `statusCode` 否则 500；存在
/// partial 块时输出 `partial`/`partialAction`/`sessionId`/`directory`。
pub fn send_service_error(error: &SvcError, fallback: &str) -> Response {
    let message = if error.message.is_empty() {
        fallback.to_string()
    } else {
        error.message.clone()
    };
    let status = StatusCode::from_u16(error.status.unwrap_or(500))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = Map::new();
    body.insert("error".to_string(), Value::String(message));
    if let Some(partial) = &error.partial {
        body.insert("partial".to_string(), Value::Bool(true));
        body.insert(
            "partialAction".to_string(),
            Value::String(partial.action.clone()),
        );
        body.insert(
            "sessionId".to_string(),
            Value::String(partial.session_id.clone()),
        );
        body.insert(
            "directory".to_string(),
            partial
                .directory
                .as_deref()
                .map(|directory| Value::String(directory.to_string()))
                .unwrap_or(Value::Null),
        );
    }
    (status, Json(Value::Object(body))).into_response()
}
