//! Shared error/response conventions for all ported modules.
//!
//! JS precedent: route handlers answer JSON errors (`{ error: ... }` shapes
//! vary per module) and never leak panics to the wire. Modules map their
//! failures onto [`AppError`] and use [`AppResult`].
//!
//! 所有移植模块共享的错误与响应约定：各模块把失败映射为
//! [`AppError`]，统一通过 [`AppResult`] 返回，panic 绝不泄漏到网络上，
//! 错误一律以 `{ "error": ... }` JSON 形状应答（各模块形状有别）。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// 全部路由处理器的统一返回类型：成功载荷或 [`AppError`]。
pub type AppResult<T> = Result<T, AppError>;

/// 路由层统一的应用错误类型，经 `IntoResponse` 渲染为 JSON 错误应答；
/// 状态码由变体决定（见 [`AppError::into_response`]）。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 显式 HTTP 应答：携带状态码与面向调用方的错误消息。
    #[error("{message}")]
    Http { status: StatusCode, message: String },
    /// The managed/external engine is not ready or a proxied engine call failed.
    /// 受管/外部引擎未就绪，或代理的引擎调用失败（应答 502）。
    #[error("engine unavailable: {0}")]
    EngineUnavailable(String),
    /// 底层 I/O 失败，自动从 `std::io::Error` 转换（应答 500）。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 未归类的意外失败，自动从 `anyhow::Error` 转换（应答 500）。
    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
}

/// 常用状态码的快捷构造。
impl AppError {
    /// 以任意 [`axum::http::StatusCode`] 与消息构造 `Http` 变体。
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self::Http {
            status,
            message: message.into(),
        }
    }

    /// 构造 400 BAD_REQUEST 错误。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// 构造 404 NOT_FOUND 错误。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// 构造 500 INTERNAL_SERVER_ERROR 错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

/// 把 [`AppError`] 变为 axum 应答：任何变体都序列化为带状态码的
/// `{ "error": ... }` JSON，绝不泄漏 panic。
impl IntoResponse for AppError {
    /// 按变体选择状态码（`Http` 用自带状态、`EngineUnavailable` 映射 502、
    /// `Io`/`Unexpected` 映射 500），并以消息构造 JSON 错误体。
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Http { status, .. } => *status,
            AppError::EngineUnavailable(_) => StatusCode::BAD_GATEWAY,
            AppError::Io(_) | AppError::Unexpected(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(serde_json::json!({ "error": self.to_string() }));
        (status, body).into_response()
    }
}
