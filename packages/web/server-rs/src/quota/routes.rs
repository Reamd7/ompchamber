//! Port of `server/lib/quota/routes.js` (`registerQuotaRoutes`) on axum.
//!
//! Route map (verbs, status codes, and JSON shapes preserved):
//! - `GET  /api/quota/providers` → `{ providers: [...] }`
//! - `GET  /api/quota/credentials/:providerId` → status or 404
//!   `{ code: 'UNSUPPORTED_PROVIDER', error: 'Unsupported credential provider' }`
//! - `PUT  /api/quota/credentials/:providerId` (16kb body limit) → validated
//!   write; invalid credentials 400 `{ code: 'INVALID_CREDENTIAL', error }`
//! - `POST /api/quota/credentials/:providerId/validate` → `{ valid: true }`
//!   or 404 `{ code: 'NOT_CONFIGURED', error: 'Not configured' }`
//! - `POST /api/quota/credentials/:providerId/import` → cursor only, else 404
//!   `{ code: 'IMPORT_UNAVAILABLE', error: 'Import unavailable' }`
//! - `DELETE /api/quota/credentials/:providerId` → `{ configured: false }`
//! - `GET  /api/quota/:providerId` → provider result envelope (200 even for
//!   unsupported providers).
//!
//! 中文说明：本模块是 `server/lib/quota/routes.js`（`registerQuotaRoutes`）
//! 的 axum 移植，注册全部 `/api/quota/*` 路由；动词、状态码与 JSON 载荷
//! 形状与 JS 版逐条对齐（详见上方路由表）。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::quota::credentials::{
    delete_managed_credential, managed_credential_status, normalizer_for, read_managed_credential,
    write_managed_credential,
};
use crate::quota::deps::QuotaDeps;
use crate::quota::providers::cursor;
use crate::quota::providers::ollama_cloud;
use crate::quota::runtime::QuotaRuntime;

/// `express.json({ limit: '16kb' })` on the PUT route.
///
/// 中文说明：凭据写入请求体的 16KB 上限（对应 JS 版 PUT 路由上的
/// `express.json({ limit: '16kb' })`）。
const CREDENTIAL_BODY_LIMIT: usize = 16 * 1024;

#[derive(Clone)]
/// quota 路由的共享状态：携带进程级单例 [`QuotaRuntime`]（内含各 provider
/// 共用的 [`QuotaDeps`]），由 axum 的 `State` 提取器注入各 handler。
pub struct QuotaState {
    /// 进程级共享的 quota runtime：聚合依赖注入与 provider 结果缓存。
    pub runtime: Arc<QuotaRuntime>,
}

/// 生产入口：忽略传入的 [`RouterContext`]，用全局共享的 [`QuotaRuntime`]
/// （真实依赖装配）构建 quota 路由。
pub fn router(ctx: RouterContext) -> Router {
    let _ = ctx;
    router_with(QuotaRuntime::shared(QuotaDeps::real()))
}

/// 用指定的 runtime 构建 quota 路由（测试用 fake runtime 注入走这里）：
/// credentials 子路由挂 16KB body limit；validate/import 两条 POST 路由
/// 额外挂 method fallthrough 404 兜底以还原 JS 的状态码语义。
pub fn router_with(runtime: Arc<QuotaRuntime>) -> Router {
    let credentials = Router::new()
        .route(
            "/api/quota/credentials/{providerId}",
            get(credential_status)
                .put(write_credential)
                .delete(remove_credential),
        )
        .route_layer(DefaultBodyLimit::max(CREDENTIAL_BODY_LIMIT));

    Router::new()
        .route("/api/quota/providers", get(list_providers))
        .merge(credentials)
        .route(
            "/api/quota/credentials/{providerId}/validate",
            post(validate_credential).fallback(method_fallthrough_404),
        )
        .route(
            "/api/quota/credentials/{providerId}/import",
            post(import_credential).fallback(method_fallthrough_404),
        )
        .route("/api/quota/{providerId}", get(provider_quota))
        .with_state(QuotaState { runtime })
}

/// 构造 404 `UNSUPPORTED_PROVIDER` 响应：provider 不在受管凭据集合内时统一返回。
fn unsupported_provider() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "code": "UNSUPPORTED_PROVIDER", "error": "Unsupported credential provider" })),
    )
        .into_response()
}

/// 构造 400 `INVALID_CREDENTIAL` 响应：凭据归一化失败或厂商校验失败时返回，
/// `message` 为具体错误描述。
fn credential_error(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "INVALID_CREDENTIAL", "error": message })),
    )
        .into_response()
}

/// Express registers the credential POST routes without method fallthrough:
/// a GET probe misses every handler and the app answers 404. axum would
/// answer 405; this fallback restores the JS status.
///
/// 中文说明：Express 中凭据 POST 路由不做 method fallthrough：GET 探测
/// 命不中任何 handler，整个 app 回 404；axum 默认会回 405，此 fallback
/// 把 JS 的 404 语义还原回来。
async fn method_fallthrough_404() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// `GET /api/quota/providers`：返回 `{ providers: [...] }`，列出当前已配置
/// 的 quota provider 及其状态摘要。
async fn list_providers(State(state): State<QuotaState>) -> Response {
    Json(json!({ "providers": state.runtime.list_configured() })).into_response()
}

/// `GET /api/quota/credentials/:providerId`：返回受管凭据的 status 掩码
/// 载荷；provider 不受管时返回 404 `UNSUPPORTED_PROVIDER`。
async fn credential_status(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    Json(managed_credential_status(&state.runtime.deps, &provider_id)).into_response()
}

/// `validators[providerId]` — Ollama Cloud usage fetch, Cursor validation.
///
/// 中文说明：按 provider 执行写前/validate 校验（对应 JS 的
/// `validators[providerId]`）：ollama-cloud 真实拉取一次用量接口，
/// cursor 调用凭据校验；未知 provider 视为无校验（`Ok`）。任何厂商侧
/// 错误都以 `Err(消息)` 上抛给调用方转成 400。
async fn run_validator(
    deps: &QuotaDeps,
    provider_id: &str,
    credential: &Value,
) -> Result<(), String> {
    match provider_id {
        "ollama-cloud" => ollama_cloud::fetch_ollama_cloud_usage(deps, credential)
            .await
            .map(|_| ()),
        "cursor" => cursor::validate_cursor_credential(deps, credential).await,
        _ => Ok(()),
    }
}

/// `PUT /api/quota/credentials/:providerId`：校验并写入受管凭据。顺序为
/// provider 白名单检查（404）→ JSON body 解析（解析失败按 body-parser
/// 语义回 400）→ 归一化（失败 400 `INVALID_CREDENTIAL`）→ 厂商校验
/// （失败 400）→ 落盘；成功返回写入后的 status 掩码载荷。
async fn write_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(normalize) = normalizer_for(&provider_id) else {
        return unsupported_provider();
    };
    // express.json parse failures answer as body-parser errors (400).
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };

    let Some(credential) = normalize(&body) else {
        return credential_error("Invalid credential");
    };

    if let Err(error) = run_validator(&state.runtime.deps, &provider_id, &credential).await {
        return credential_error(&error);
    }
    match write_managed_credential(&state.runtime.deps, &provider_id, &credential) {
        Ok(status) => Json(status).into_response(),
        Err(error) => credential_error(&error),
    }
}

/// `POST /api/quota/credentials/:providerId/validate`：校验已存储的凭据。
/// provider 不受管回 404 `UNSUPPORTED_PROVIDER`；凭据未配置回 404
/// `NOT_CONFIGURED`；校验通过返回 `{ "valid": true }`，失败转 400。
async fn validate_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    let Some(credential) = read_managed_credential(&state.runtime.deps, &provider_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NOT_CONFIGURED", "error": "Not configured" })),
        )
            .into_response();
    };
    match run_validator(&state.runtime.deps, &provider_id, &credential).await {
        Ok(()) => Json(json!({ "valid": true })).into_response(),
        Err(error) => credential_error(&error),
    }
}

/// `POST /api/quota/credentials/:providerId/import`：仅 cursor 支持导入
/// （从本机 Cursor 安装中提取凭据）；provider 不受管回 404
/// `UNSUPPORTED_PROVIDER`，非 cursor 回 404 `IMPORT_UNAVAILABLE`。
/// 成功返回导入后的 status 掩码载荷，失败转 400。
async fn import_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    if provider_id != "cursor" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "IMPORT_UNAVAILABLE", "error": "Import unavailable" })),
        )
            .into_response();
    }
    match cursor::import_cursor_credential(&state.runtime.deps).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => credential_error(&error),
    }
}

/// `DELETE /api/quota/credentials/:providerId`：删除受管凭据，成功返回
/// `{ "configured": false }`；provider 不受管回 404，删除失败回 500。
async fn remove_credential(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    if normalizer_for(&provider_id).is_none() {
        return unsupported_provider();
    }
    match delete_managed_credential(&state.runtime.deps, &provider_id) {
        Ok(()) => Json(json!({ "configured": false })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `GET /api/quota/:providerId`：查询指定 provider 的配额/用量，返回
/// provider 结果信封（即使 provider 不支持也返回 200 的错误信封，与
/// JS 行为一致）。
async fn provider_quota(
    State(state): State<QuotaState>,
    Path(provider_id): Path<String>,
) -> Response {
    let result = state.runtime.fetch_quota_for_provider(&provider_id).await;
    Json(result).into_response()
}
