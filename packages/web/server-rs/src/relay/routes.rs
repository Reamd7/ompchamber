//! Port of the relay management routes from `server/lib/relay/service.js`
//! (`registerRoutes`): `GET/POST /api/ompchamber/relay/{status,enable,disable}`.
//!
//! 中文概述：relay 管理路由的 axum 版本（JS `service.js` 的
//! `registerRoutes` 对应物）。三个端点全部委托 `RelayService`：status
//! 查询、enable 带 16 KiB 请求体上限（对齐 express.json limit）、
//! disable 关停；服务层错误统一映射为 500 + JSON `{ "error": ... }`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

use super::service::RelayService;

/// 路由共享状态：包一层的 `RelayService` Arc（axum state 要求可 Clone）。
#[derive(Clone)]
pub struct ModuleState {
    /// relay 服务实例（状态查询/启停的实际执行者）。
    pub service: std::sync::Arc<RelayService>,
}

/// 注册三个 relay 管理端点并绑定共享状态。enable 路由叠加 16 KiB 的
/// 请求体上限（对应 JS 侧 `express.json({ limit: '16kb' })`）。
pub fn router_with(state: ModuleState) -> Router {
    Router::new()
        .route("/api/ompchamber/relay/status", get(status))
        .route(
            "/api/ompchamber/relay/enable",
            // express.json({ limit: '16kb' })
            post(enable).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/api/ompchamber/relay/disable", post(disable))
        .with_state(state)
}

/// GET status：返回 relay 当前状态 JSON；服务层错误转 500。
async fn status(State(service): State<ModuleState>) -> Response {
    match service.service.get_status().await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// POST enable：以请求体 JSON 作为选项启用 relay；服务层错误转 500。
async fn enable(State(service): State<ModuleState>, body: axum::extract::Json<Value>) -> Response {
    match service.service.enable(&body.0).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// POST disable：关闭 relay 并返回结果载荷；服务层错误转 500。
async fn disable(State(service): State<ModuleState>) -> Response {
    match service.service.disable().await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}
