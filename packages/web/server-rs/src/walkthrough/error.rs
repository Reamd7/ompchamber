//! walkthrough 模块的统一错误类型：携带 HTTP 状态码与结构化附加字段，
//! 经 `IntoResponse` 序列化为与 JS `fail()`/`respondWithError` 相同的
//! 响应体 `{ error, code?, model?, requiredChars?, availableChars? }`。
//! The walkthrough module's error shape.
//!
//! Mirrors the JS `fail(message, statusCode, extra)` errors and
//! `respondWithError` in `routes.js`: every failure answers
//! `{ error, code?, model?, requiredChars?, availableChars? }` with the
//! carried status (500 when nothing better is known), and 5xx failures are
//! logged server-side.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// 一次 walkthrough 请求失败的全部信息：状态码 + 展示消息 + 可选的
/// 结构化字段（错误码、模型描述、上下文字符预算），原样透传给客户端。
#[derive(Debug, Clone, PartialEq)]
pub struct WalkthroughError {
/// 返回给客户端的 HTTP 状态码。
    pub status: u16,
/// 人类可读的错误消息，即响应体的 `error` 字段。
    pub message: String,
/// 机器可读错误码（如 `context-too-small`），供客户端分支处理。
    pub code: Option<String>,
/// 触发失败的模型描述（describe 结果），原样嵌入响应体。
    pub model: Option<Value>,
/// `context-too-small` 时 digest 所需的字符数。
    pub required_chars: Option<i64>,
/// `context-too-small` 时模型实际可用的字符数。
    pub available_chars: Option<i64>,
}

/// 构造器与链式附加字段（builder 风格）。
impl WalkthroughError {
/// 以消息与状态码构造错误，其余可选字段为空。
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            model: None,
            required_chars: None,
            available_chars: None,
        }
    }

/// 以消息、状态码与错误码一步构造。
    /// `fail(message, statusCode, { code })`.
    pub fn with_code(message: impl Into<String>, status: u16, code: &str) -> Self {
        Self {
            code: Some(code.to_string()),
            ..Self::new(message, status)
        }
    }

/// 500 内部错误的快捷构造。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(message, 500)
    }

/// 链式设置错误码。
    pub fn code(mut self, code: &str) -> Self {
        self.code = Some(code.to_string());
        self
    }

/// 链式附加模型描述。
    pub fn model(mut self, model: Value) -> Self {
        self.model = Some(model);
        self
    }

/// 链式附加 requiredChars（digest 所需字符数）。
    pub fn required_chars(mut self, required_chars: i64) -> Self {
        self.required_chars = Some(required_chars);
        self
    }

/// 链式附加 availableChars（模型可用字符数）。
    pub fn available_chars(mut self, available_chars: i64) -> Self {
        self.available_chars = Some(available_chars);
        self
    }
}

/// Display 只呈现消息本身。
impl std::fmt::Display for WalkthroughError {
/// 写出 `self.message`。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// 满足 `std::error::Error`（消息即 Display 输出）。
impl std::error::Error for WalkthroughError {}

/// 把错误转成 JSON 响应；5xx 先记录服务端日志。
impl IntoResponse for WalkthroughError {
/// 序列化为 `{ error, ...可选字段 }`；状态码非法时回落 500。
    fn into_response(self) -> Response {
        if self.status >= 500 {
            tracing::error!("walkthrough error: {}", self.message);
        }
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = json!({ "error": self.message });
        if let Some(code) = &self.code {
            body["code"] = json!(code);
        }
        if let Some(model) = &self.model {
            body["model"] = model.clone();
        }
        if let Some(required) = self.required_chars {
            body["requiredChars"] = json!(required);
        }
        if let Some(available) = self.available_chars {
            body["availableChars"] = json!(available);
        }
        (status, Json(body)).into_response()
    }
}
