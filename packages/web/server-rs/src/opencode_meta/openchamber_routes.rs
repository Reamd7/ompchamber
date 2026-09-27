//! Port of `server/lib/opencode/openchamber-routes.js` — the OpenChamber
//! update and models-metadata endpoints:
//!
//! - `GET  /api/ompchamber/update-check` — proxies the ported
//!   `package_manager::checkForUpdates` (GitHub releases comparison,
//!   optional hosted update API via `OMPCHAMBER_UPDATE_API_URL`).
//! - `POST /api/ompchamber/update-install` — the JS queues the actual
//!   install with the detected package manager and restarts the server
//!   (container mode stays online, systemd foreground queues a transient
//!   unit, daemon mode spawns a detached shell and exits). Those install
//!   executions replace the *npm-distributed Node server*, which this Rust
//!   build is not: performing them from here could not update the running
//!   binary. Every execution path therefore answers with the JS error shape
//!   and honest not-available semantics, while the pure decision logic
//!   (no-update 400, foreground-without-systemd 409 with the exact JS
//!   message, instance-file launch-mode resolution) is ported in full.
//! - `GET  /api/ompchamber/models-metadata` — the models.dev catalog cache
//!   (`models_metadata.rs`) with the JS's Cache-Control policy.
//! - `GET  /api/zen/models` — the zen model list. The JS dependency is the
//!   notifications template-runtime compatibility stub (`fetchFreeZenModels`
//!   resolves `[]` and never throws; `getCachedZenModels` holds
//!   `{ models: [] }`), so both the success and fallback bodies are
//!   `{ "models": [] }`.
//!
//! 中文说明：本模块是 `server/lib/opencode/openchamber-routes.js` 的 Rust
//! 移植，提供 OpenChamber 自身的更新检查/安装与模型元数据路由。更新安装
//! 的真实执行会替换 npm 发行版 Node server 而非当前运行的二进制，因此各
//! 执行分支统一返回 JS 的错误形状与“本构建不可自更新”的诚实语义；纯决策
//! 逻辑（无更新 400、无 systemd 前台 409、实例文件启动模式解析）完整移植。

use std::sync::Arc;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::{MetaState, parse_query, parse_string};
use crate::package_manager::CheckForUpdatesOptions;

/// 挂载本模块四条路由：update-check、update-install、models-metadata 与 zen models。
pub(crate) fn routes(state: Arc<MetaState>) -> axum::Router {
    axum::Router::new()
        .route("/api/ompchamber/update-check", get(update_check))
        .route("/api/ompchamber/update-install", post(update_install))
        .route("/api/ompchamber/models-metadata", get(models_metadata))
        .route("/api/zen/models", get(zen_models))
        .with_state(state)
}

/// 构造统一 JSON 错误响应：`{ "error": message }` + 给定状态码。
fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/ompchamber/update-check
// ---------------------------------------------------------------------------

/// 从 User-Agent 嗅探设备类型（对应 JS `inferDeviceClass`）：空串 → unknown；
/// ipad/tablet → tablet；mobi/android/iphone → mobile；其余 → desktop。
/// JS `inferDeviceClass`: sniff the device class from the User-Agent.
pub(crate) fn infer_device_class(user_agent: &str) -> &'static str {
    let value = user_agent.to_lowercase();
    if value.is_empty() {
        return "unknown";
    }
    if value.contains("ipad") || value.contains("tablet") {
        return "tablet";
    }
    if value.contains("mobi") || value.contains("android") || value.contains("iphone") {
        return "mobile";
    }
    "desktop"
}

/// 解析 reportUsage 参数（对应 JS `parseReportUsage`）：缺省视为同意上报，仅字面 false/0/no（忽略大小写）视为退出。
/// JS `parseReportUsage`: absent → report; only the literal false/0/no
/// spellings opt out.
pub(crate) fn parse_report_usage(value: Option<&str>) -> Option<bool> {
    let raw = value?;
    let normalized = raw.trim().to_lowercase();
    Some(!(normalized == "false" || normalized == "0" || normalized == "no"))
}

/// `GET /update-check`：把查询参数（appType/deviceClass/platform/arch/
/// instanceMode/currentVersion/installId/reportUsage）与 UA 嗅探结果组装成
/// `CheckForUpdatesOptions`，委托 package_manager 检查更新并原样返回 JSON。
async fn update_check(
    State(state): State<Arc<MetaState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = parse_query(query.as_deref());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    let options = CheckForUpdatesOptions {
        app_type: parse_string(query.single("appType")),
        device_class: parse_string(query.single("deviceClass"))
            .or_else(|| Some(infer_device_class(user_agent).to_string())),
        platform: parse_string(query.single("platform")),
        arch: parse_string(query.single("arch")),
        instance_mode: parse_string(query.single("instanceMode")),
        current_version: parse_string(query.single("currentVersion")),
        install_id: parse_string(query.single("installId")),
        report_usage: parse_report_usage(query.single("reportUsage")),
    };

    let update_info = state.package_manager.check_for_updates(options).await;
    Json(update_info).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/ompchamber/update-install
// ---------------------------------------------------------------------------

/// 校验 systemd unit 名（对应 JS `SYSTEMD_SERVICE_UNIT_PATTERN`）：<安全字符>.service，安全字符为字母数字与 :_.@-。
/// JS `SYSTEMD_SERVICE_UNIT_PATTERN`: `/^[A-Za-z0-9:_.@-]+\.service$/`.
fn is_valid_systemd_unit(unit: &str) -> bool {
    let Some(prefix) = unit.strip_suffix(".service") else {
        return false;
    };
    !prefix.is_empty()
        && prefix.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'-')
        })
}

/// 解析 systemd 服务单元（对应 JS `resolveSystemdServiceUnit`）：仅在 INVOCATION_ID
/// 非空（确认由 systemd 拉起）时生效；未配置时默认 ompchamber.service，配置名
/// 必须通过安全校验。
/// JS `resolveSystemdServiceUnit(environment)`.
pub(crate) fn resolve_systemd_service_unit(
    invocation_id: Option<&str>,
    configured_unit: Option<&str>,
) -> Option<String> {
    if invocation_id.unwrap_or_default().is_empty() {
        return None;
    }
    let configured = configured_unit.unwrap_or_default().trim();
    let unit = if configured.is_empty() {
        "ompchamber.service"
    } else {
        configured
    };
    if is_valid_systemd_unit(unit) {
        Some(unit.to_string())
    } else {
        None
    }
}

/// `update-install` 执行前的决策树结果，对应 JS 的分支结构。
/// The decision tree `update-install` walks before executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallDecision {
    /// 容器环境（/.dockerenv 或 CONTAINER 标记）：JS 在线安装并保持运行。
    /// `/.dockerenv`/env markers: the JS installs and stays online.
    Container,
    /// 前台运行且解析不到 systemd unit：对应 JS 原文的 409 拒绝。
    /// Foreground launch without a resolvable systemd unit → the JS's exact
    /// 409 error.
    ForegroundWithoutSystemd,
    /// 前台 + systemd：JS 排队一个 transient systemd-run 单元（内含 unit 名）。
    /// Foreground under systemd: the JS queues a transient `systemd-run` unit.
    ForegroundSystemd(String),
    /// daemon 运行：JS 拉起 detached 的安装+重启脚本后退出。
    /// Daemon launch: the JS spawns a detached install+restart script and
    /// exits.
    DaemonRestart,
}

/// 走决策树：容器优先；前台时有 unit → ForegroundSystemd、无 unit → 409 拒绝；其余为 daemon 重启路径。
pub(crate) fn install_decision(
    is_container: bool,
    launch_mode_foreground: bool,
    systemd_unit: Option<String>,
) -> InstallDecision {
    if is_container {
        return InstallDecision::Container;
    }
    if launch_mode_foreground {
        return match systemd_unit {
            Some(unit) => InstallDecision::ForegroundSystemd(unit),
            None => InstallDecision::ForegroundWithoutSystemd,
        };
    }
    InstallDecision::DaemonRestart
}

/// 容器探测（对应 JS 输入集合）：存在 /.dockerenv，或 CONTAINER/container 环境变量指示容器，即视为容器。
/// JS container probe inputs (`/.dockerenv` + `CONTAINER` env vars).
fn container_environment() -> bool {
    let dockerenv = std::path::Path::new("/.dockerenv").exists();
    let container_var = std::env::var("CONTAINER").ok().filter(|v| !v.is_empty());
    let container_docker = std::env::var("container").ok().as_deref() == Some("docker");
    dockerenv || container_var.is_some() || container_docker
}

/// JS 原文的前台拒绝文案（409 响应体逐字一致）。
/// The JS's exact foreground refusal message.
const FOREGROUND_WITHOUT_SYSTEMD_MESSAGE: &str = "Foreground servers must be updated by their service manager. Set OMPCHAMBER_SYSTEMD_UNIT when running under systemd, or run ompchamber update and restart the service.";

/// 三条“本构建不可自更新”的诚实文案：真实安装会替换 npm 发行版的 Node
/// server 而非当前运行的二进制，故容器/systemd/daemon 分支分别提示用
/// 对应方式更新。
/// Honest not-available messages for the install executions this Rust build
/// cannot perform (they would replace the npm-distributed Node server, not
/// the running binary).
const NOT_AVAILABLE_CONTAINER: &str = "Self-update is not available in this server build; update the container image and restart the container.";
/// systemd 前台分支的文案：提示改用 `ompchamber update` 并重启服务。
const NOT_AVAILABLE_SYSTEMD: &str = "Self-update is not available in this server build; run 'ompchamber update' and restart the OMPChamber service.";
/// daemon 分支的文案：提示在终端执行 `ompchamber update` 后重启 server。
const NOT_AVAILABLE_DAEMON: &str = "Self-update is not available in this server build; run 'ompchamber update' from a terminal and restart the server.";

/// 读取实例文件 `<dataDir>/run/ompchamber-<port>.json` 判断是否前台启动：
/// 缺失或损坏按 daemon 处理；端口取自配置（0 沿用 JS 的 3000 兜底）。
/// JS instance-file read: `<dataDir>/run/ompchamber-<port>.json` (missing or
/// corrupt files fall back to `{ port, daemon: true }`). The JS reads the
/// bound port from `server.address()`; this port carries the requested port
/// from the server config (`0` keeps the JS's fallback of 3000).
async fn read_launch_mode(state: &MetaState) -> bool {
    let port = if state.port == 0 { 3000 } else { state.port };
    let instance_file = state
        .data_dir
        .join("run")
        .join(format!("ompchamber-{port}.json"));
    let stored = tokio::fs::read_to_string(&instance_file)
        .await
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    matches!(
        stored
            .as_ref()
            .and_then(|options| options.get("launchMode"))
            .and_then(Value::as_str),
        Some("foreground")
    )
}

/// `POST /update-install`：先复查更新可用性（不可用 → 400 "No update
/// available"），再按容器/实例文件/systemd 决策树返回 409；无 systemd 的
/// 前台是 JS 原文拒绝，其余分支是本构建的诚实不可用文案。
async fn update_install(State(state): State<Arc<MetaState>>) -> Response {
    let update_info = state
        .package_manager
        .check_for_updates(CheckForUpdatesOptions::default())
        .await;
    if !update_info.available {
        return json_error(StatusCode::BAD_REQUEST, "No update available");
    }

    let is_container = container_environment();
    let launch_mode_foreground = if is_container {
        // The JS short-circuits before reading the instance file in
        // container mode.
        false
    } else {
        read_launch_mode(&state).await
    };
    let systemd_unit = resolve_systemd_service_unit(
        std::env::var("INVOCATION_ID").ok().as_deref(),
        std::env::var("OMPCHAMBER_SYSTEMD_UNIT").ok().as_deref(),
    );

    match install_decision(is_container, launch_mode_foreground, systemd_unit) {
        InstallDecision::ForegroundWithoutSystemd => {
            json_error(StatusCode::CONFLICT, FOREGROUND_WITHOUT_SYSTEMD_MESSAGE)
        }
        InstallDecision::Container => json_error(StatusCode::CONFLICT, NOT_AVAILABLE_CONTAINER),
        InstallDecision::ForegroundSystemd(_) => {
            json_error(StatusCode::CONFLICT, NOT_AVAILABLE_SYSTEMD)
        }
        InstallDecision::DaemonRestart => json_error(StatusCode::CONFLICT, NOT_AVAILABLE_DAEMON),
    }
}

// ---------------------------------------------------------------------------
// GET /api/ompchamber/models-metadata
// ---------------------------------------------------------------------------

/// `GET /models-metadata`：委托 `ModelsMetadataCache` 读取 models.dev 目录；
/// 非过期缓存命中 Cache-Control 60 秒、其余 300 秒；超时类失败 504、其余
/// 502（与 JS 一致）。
async fn models_metadata(State(state): State<Arc<MetaState>>) -> Response {
    match state
        .models
        .get(
            super::models_metadata::MODELS_DEV_API_URL,
            super::MODELS_METADATA_CACHE_TTL_MS,
            super::models_metadata::DEFAULT_TIMEOUT_MS,
        )
        .await
    {
        Ok(result) => {
            let cache_control = if result.from_cache && !result.stale {
                "public, max-age=60"
            } else {
                "public, max-age=300"
            };
            (
                [(header::CACHE_CONTROL, cache_control)],
                Json(result.metadata),
            )
                .into_response()
        }
        Err(failure) => {
            // JS: `TimeoutError`/`AbortError` → 504, anything else → 502.
            let status = if failure.timeout {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            json_error(status, "Failed to retrieve model metadata")
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/zen/models
// ---------------------------------------------------------------------------

/// `GET /zen/models`：依赖的 `fetchFreeZenModels` 是恒返回空数组且不抛错的
/// 兼容 stub，因此恒定返回 `{ "models": [] }` 与 300 秒 Cache-Control。
async fn zen_models() -> Response {
    // `fetchFreeZenModels` is the notifications compatibility stub resolving
    // `[]`; it never throws, so the cached-fallback branch never runs (and
    // would return `{ models: [] }` too).
    (
        [(header::CACHE_CONTROL, "public, max-age=300")],
        Json(json!({ "models": [] })),
    )
        .into_response()
}
