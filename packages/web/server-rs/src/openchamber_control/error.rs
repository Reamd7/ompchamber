//! Port of `server/lib/openchamber-control/error.js` — the control-plane
//! error envelope (`OMPChamberControlError`) and the coercion every caught
//! failure passes through (`asControlError`).
//!
//! The scheduled-tasks and openchamber-sessions services throw the same
//! class, so a status plus message (and, for dispatch failures, partial
//! result details) survives to the route response unchanged. Only
//! `goalConfigured` is promoted from a foreign error's fields; everything
//! else on a non-control error is dropped in favor of its message.

use serde_json::Value;

/// `OMPChamberControlError { statusCode, message, ...details }`.
#[derive(Debug, Clone)]
pub struct ControlError {
    pub status: u16,
    pub message: String,
    /// `partial: true` plus the partial-result coordinates, set when a
    /// failed dispatch still created something (a fork, a configured goal).
    pub partial: bool,
    pub partial_action: Option<String>,
    pub session_id: Option<String>,
    pub directory: Option<String>,
    /// A goal was configured before the failure (foreign-error promotion).
    pub goal_configured: bool,
    /// The offending task (`schedule.run` failures carry it on the error).
    pub task: Option<Value>,
}

impl ControlError {
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

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    /// `new OMPChamberControlError(message, status, { partial: true, ... })`.
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
impl From<crate::scheduled_tasks::service::ServiceError> for ControlError {
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
impl From<crate::error::AppError> for ControlError {
    fn from(error: crate::error::AppError) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<anyhow::Error> for ControlError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlError {}
