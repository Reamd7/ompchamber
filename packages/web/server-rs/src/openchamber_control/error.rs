//! Port of `server/lib/openchamber-control/error.js` — the control-plane
//! error envelope (`OMPChamberControlError`) and the coercion every caught
//! failure passes through (`asControlError`).
//!
//! The scheduled-tasks and openchamber-sessions services throw the same
//! class, so a status plus message (and, for dispatch failures, partial
//! result details) survives to the route response unchanged. Only
//! `goalConfigured` is promoted from a foreign error's fields; everything
//! else on a non-control error is dropped in favor of its message.
//! （中文说明）本文件是 `server/lib/openchamber-control/error.js` 的移植：
//! 定义控制平面统一错误信封 [`ControlError`]，以及所有被捕获失败都要经过
//! 的收敛逻辑（对应 JS 的 `asControlError`）。scheduled-tasks 与
//! openchamber-sessions 服务抛出的是同一个类，因此 status + message
//! （派发失败时还有部分结果详情）能原样保留到路由响应；只有
//! `goal_configured` 会从外来错误字段中提升，其余字段一律丢弃、只保留
//! message。

use serde_json::Value;

/// `OMPChamberControlError { statusCode, message, ...details }`.
#[derive(Debug, Clone)]
pub struct ControlError {
    /// JS 的 `statusCode`：随错误原样保留到路由响应的 HTTP 状态码。
    pub status: u16,
    /// 返回给调用方的错误消息文本。
    pub message: String,
    /// `partial: true` plus the partial-result coordinates, set when a
    /// failed dispatch still created something (a fork, a configured goal).
    /// 为 true 时路由响应会附带下列 partial* 详情字段。
    pub partial: bool,
    /// 触发部分结果的 action（如 `fork-created`、`goal-configured`）。
    pub partial_action: Option<String>,
    /// 部分结果涉及的会话 ID。
    pub session_id: Option<String>,
    /// 部分结果涉及的目录。
    pub directory: Option<String>,
    /// A goal was configured before the failure (foreign-error promotion).
    /// 失败前已配置 goal（从外来错误字段提升而来）。
    pub goal_configured: bool,
    /// The offending task (`schedule.run` failures carry it on the error).
    /// 出错的计划任务（`schedule.run` 失败时错误上携带）。
    pub task: Option<Value>,
}

/// 各便捷构造器，对应 JS `OMPChamberControlError` 的工厂用法。
impl ControlError {
    /// 基础构造：指定 status 与 message，partial 详情字段全部置空。
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            partial: false,
            partial_action: None,
            session_id: None,
            directory: None,
            goal_configured: false,
            task: None,
        }
    }

    /// 便捷构造：400 Bad Request。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    /// 便捷构造：404 Not Found。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    /// 便捷构造：500 Internal Server Error。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    /// `new OMPChamberControlError(message, status, { partial: true, ... })`.
    /// 标记本次失败前已产生部分结果（fork 已建、goal 已配置等），
    /// 并携带 `partial_action`/`session_id`/`directory` 详情坐标。
    pub fn partial(
        status: u16,
        message: impl Into<String>,
        partial_action: Option<String>,
        session_id: Option<String>,
        directory: Option<String>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            partial: true,
            partial_action,
            session_id,
            directory,
            goal_configured: false,
            task: None,
        }
    }
}

/// `asControlError(error, fallback)` for the scheduled-task service's
/// `ServiceError` (already a control error: status, message, task detail).
/// 该 `ServiceError` 本身就是控制错误：status、message、task 详情原样保留。
impl From<crate::scheduled_tasks::service::ServiceError> for ControlError {
    /// 逐字段搬运 status/message/task，partial 相关字段置空。
    fn from(error: crate::scheduled_tasks::service::ServiceError) -> Self {
        Self {
            status: error.status,
            message: error.message,
            partial: false,
            partial_action: None,
            session_id: None,
            directory: None,
            goal_configured: false,
            task: error.task,
        }
    }
}

/// `asControlError` for non-control errors: keep the message, default the
/// status to 500 (plain JS errors carry no `statusCode`).
/// 外来错误只保留 message，status 默认 500（普通 JS Error 不携带
/// `statusCode`）。
impl From<crate::error::AppError> for ControlError {
    /// 收敛为 500 控制错误，只保留 message。
    fn from(error: crate::error::AppError) -> Self {
        Self::internal(error.to_string())
    }
}

/// 任意 `anyhow::Error` 同样收敛为 500 控制错误，只保留其 message。
impl From<anyhow::Error> for ControlError {
    /// 收敛为 500 控制错误，只保留 message。
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

/// 直接输出 `message`，与 JS `Error` 的字符串化行为一致。
impl std::fmt::Display for ControlError {
    /// 输出 `message` 本身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 空实现：`Display` 已提供全部信息，仅用于接入标准错误生态。
impl std::error::Error for ControlError {}
