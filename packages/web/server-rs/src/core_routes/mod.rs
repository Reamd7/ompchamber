//! Port of `server/lib/opencode/core-routes.js` (status/system/settings-utility
//! routes) plus the four engine-facing routes this module owns from
//! `opencode/routes.js` (`/api/opencode/health`, `/api/opencode/version`,
//! `/api/opencode/directory`, `/api/config/opencode-resolution`).
//!
//! `GET/PUT /api/config/settings` are owned by the `settings` module port and
//! are deliberately absent here. The auth/access routes of
//! `registerAuthAndAccessRoutes` (session/passkey/client-auth/pairing, `/connect`,
//! `/api/system/probe-url`, and the `/api` auth middleware) depend on the
//! `ui-auth`/`client-auth` runtimes that are not ported yet and are deferred.
//!
//! Submodules:
//! - [`resolution`]: engine runtime resolution snapshot
//!   (`opencode-resolution-runtime.js` + `env-runtime.js` subset).
//! - [`engine_info`]: `/api/opencode/health` + `/api/opencode/version`.
//! - [`directory`]: `POST /api/opencode/directory`
//!   (`project-directory-runtime.js` subset + minimal settings persistence).
//! - [`settings_utility`]: `GET /api/config/themes` (`theme-runtime.js`) and
//!   `POST /api/config/reload`.
//! server/lib/opencode/core-routes.js（status/system/settings-utility
//! 路由）及本模块承接的四条引擎侧路由（`/api/opencode/health`、
//! `/api/opencode/version`、`/api/opencode/directory`、
//! `/api/config/opencode-resolution`）的移植。
//!
//! settings 归属与 auth/access 路由的暂缓原因见上方英文说明：
//! `GET/PUT /api/config/settings` 属 settings 模块；依赖 ui-auth /
//! client-auth 运行时的路由待接线后落地。

/// `POST /api/opencode/directory` 及其依赖的目录校验与 settings 持久化子集。
pub mod directory;
/// `/api/opencode/health` 与 `/api/opencode/version` 处理器。
mod engine_info;
/// 引擎二进制解析快照（opencode-resolution-runtime.js + env-runtime.js 子集）。
mod resolution;
/// `GET /api/config/themes`（theme-runtime.js）与 `POST /api/config/reload`。
mod settings_utility;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::context::RouterContext;

/// `index.js` `CLIENT_RELOAD_DELAY_MS`.
/// index.js 的 `CLIENT_RELOAD_DELAY_MS`：reload 响应告知客户端的刷新延迟（毫秒）。
const CLIENT_RELOAD_DELAY_MS: u64 = 800;
/// `express.json({ limit: '64kb' })` on the dev-shutdown route.
/// dev-shutdown 路由的请求体上限（`express.json({ limit: '64kb' })`）。
const DEV_SHUTDOWN_BODY_LIMIT: usize = 64 * 1024;
/// `express.json({ limit: '50mb' })` applied to `/api/opencode` bodies by the
/// common request middleware.
/// `/api/opencode` 路由族的请求体上限（`express.json({ limit: '50mb' })`）。
const OPENCODE_BODY_LIMIT: usize = 50 * 1024 * 1024;

/// Process start marker (`index.js` `serverStartedAt`), captured on first use.
/// 进程启动时刻（unix 毫秒），首次访问时捕获一次，等价 index.js 的
/// `serverStartedAt`。
static STARTED_AT_UNIX_MILLIS: LazyLock<i64> = LazyLock::new(now_unix_millis);

/// 当前 unix 毫秒；系统时钟早于 epoch 时返回 0。
pub(crate) fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// ISO-8601 UTC timestamp (`new Date().toISOString()` shape, millisecond
/// precision). Same civil-from-days algorithm as `engine.rs`.
/// unix 毫秒转 ISO-8601 UTC 字符串（毫秒精度）；历法换算与 engine.rs
/// 共用同一 civil-from-days 算法族。
pub(crate) fn iso_utc_from_unix_millis(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let (year, month, day) = epoch_days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// epoch 起始天数转 (年, 月, 日)（Howard Hinnant 算法）。
fn epoch_days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 进程启动时刻的 ISO 字符串；基于 LazyLock，进程内取值恒定。
fn started_at_iso() -> String {
    iso_utc_from_unix_millis(*STARTED_AT_UNIX_MILLIS)
}

/// `index.js` `OMPCHAMBER_VERSION`: the version field of the web package
/// manifest, `'unknown'` when unreadable.
/// index.js 的 `OMPCHAMBER_VERSION`：web 包 manifest 的 version 字段，
/// 读取失败或为空时为 'unknown'；结果进程内缓存。
pub(crate) fn ompchamber_version() -> String {
    static VERSION: LazyLock<String> = LazyLock::new(|| {
        let package_json = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("package.json");
        std::fs::read_to_string(package_json)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|pkg| {
                pkg.get("version")
                    .and_then(|v| v.as_str())
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_string())
    });
    VERSION.clone()
}

/// `runtimeName: process.env.OMPCHAMBER_RUNTIME || 'web'`.
/// `process.env.OMPCHAMBER_RUNTIME`（trim 后非空才采用），缺省 'web'。
pub(crate) fn runtime_name() -> String {
    std::env::var("OMPCHAMBER_RUNTIME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "web".to_string())
}

/// `isEnvFlagEnabled` from `index.js`: `'1'`/`'true'` (case-insensitive,
/// trimmed) enable a flag.
/// index.js 的 `isEnvFlagEnabled`：值 trim、忽略大小写后为 '1' 或
/// 'true' 即视为开启。
pub(crate) fn env_flag_enabled(name: &str) -> bool {
    let Some(raw) = std::env::var(name).ok() else {
        return false;
    };
    let normalized = raw.trim().to_ascii_lowercase();
    normalized == "1" || normalized == "true"
}

/// `core-routes.js` `compatibility` capability block (immutable contract).
/// core-routes.js 的 `compatibility` 能力块（对外不可变的契约字段）。
fn compatibility() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": 1,
        "minClientApiVersion": 1,
        "capabilities": [
            "api.health.v1",
            "api.runtime-url.v1",
            "api.raw-file.v1",
            "realtime.sse.v1",
            "realtime.websocket.global-events.v1",
            "terminal.websocket.v1",
        ],
    })
}

/// `ENV_DESKTOP_NOTIFY` from `index.js`.
/// index.js 的 `ENV_DESKTOP_NOTIFY`：显式 'true'、runtime 为 desktop、
/// 或 argv 前两项含 ompchamber-server 任一命中即开启。
fn desktop_notify_enabled() -> bool {
    if std::env::var("OMPCHAMBER_DESKTOP_NOTIFY")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("true")
    {
        return true;
    }
    if runtime_name() == "desktop" {
        return true;
    }
    // /ompchamber-server/i test against argv[0] and argv[1].
    std::env::args()
        .take(2)
        .any(|arg| arg.to_ascii_lowercase().contains("ompchamber-server"))
}

/// `PLAN_MODE_EXPERIMENT_ENABLED` from `index.js`.
/// index.js 的 `PLAN_MODE_EXPERIMENT_ENABLED`：PLAN_MODE 专用实验开关
/// 或总实验开关任一开启。
fn plan_mode_experimental_enabled() -> bool {
    env_flag_enabled("OPENCODE_EXPERIMENTAL_PLAN_MODE") || env_flag_enabled("OPENCODE_EXPERIMENTAL")
}

/// Port of the engine-facing `getHealthSnapshot()` fields `index.js` spreads
/// into `GET /health`, derived from `ctx.engine.snapshot()` plus the runtime
/// resolution snapshot. `null` fields mark values the Rust engine does not
/// track yet (see PORT-MANIFEST.md).
/// 引擎侧 `getHealthSnapshot()` 字段的移植：由 ctx.engine.snapshot()
/// 与运行时解析快照推导；Rust 引擎尚未跟踪的字段以 null 标记（见
/// PORT-MANIFEST.md）。
fn health_snapshot(ctx: &RouterContext, snapshot: &serde_json::Value) -> serde_json::Value {
    let base_url = snapshot
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ready = snapshot
        .get("ready")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let open_code_port = base_url.as_deref().and_then(|base| {
        url::Url::parse(base)
            .ok()
            .and_then(|u| u.port_or_known_default())
    });
    let resolution = resolution::EngineResolution::resolve();

    serde_json::json!({
        "openCodePort": open_code_port,
        "openCodeRunning": ready && base_url.is_some(),
        // auth-state-runtime: "secure" means Basic auth is active on the
        // engine connection, not TLS.
        "openCodeSecureConnection": ctx.engine.auth_header().is_some(),
        // auth-state-runtime password source is not surfaced by EngineState yet.
        "openCodeAuthSource": ctx.engine.auth_source().map(serde_json::Value::from),
        "openCodeApiPrefix": "",
        "openCodeApiPrefixDetected": true,
        "isOpenCodeReady": ready,
        "lastOpenCodeError": snapshot.get("lastError").cloned().unwrap_or(serde_json::Value::Null),
        "lastOpenCodeLaunchDiagnostics": snapshot.get("lastLaunch").cloned().unwrap_or(serde_json::Value::Null),
        // Health-check failures fold into lastError in the Rust engine; no
        // separate health-failure / managed-process / restart diagnostics yet.
        "lastOpenCodeHealthFailure": serde_json::Value::Null,
        "lastManagedOpenCodeProcess": serde_json::Value::Null,
        "lastOpenCodeRestartDiagnostics": serde_json::Value::Null,
        "opencodeBinaryResolved": resolution.resolved_display(),
        "opencodeBinarySource": resolution.source,
        "opencodeLaunchBinary": resolution.launch_binary,
        "opencodeLaunchArgs": resolution.launch_args,
        "opencodeLaunchWrapperType": resolution.launch_wrapper_type,
        "nodeBinaryResolved": resolution.node_display(),
        "bunBinaryResolved": crate::engine_env::EnvRuntime::shared().resolved_bun_binary(),
        "desktopNotifyEnabled": desktop_notify_enabled(),
        "planModeExperimentalEnabled": plan_mode_experimental_enabled(),
        "apiOnly": ctx.config.api_only,
    })
}

/// `GET /health` — JS shape from `registerServerStatusRoutes` plus the richer
/// engine snapshot (`mode`/`ready`/`baseUrl`/`lastError`/`lastLaunch`).
/// `GET /health`：registerServerStatusRoutes 的 JS 形态，叠加更丰富的
/// 引擎快照（mode/ready/baseUrl/lastError/lastLaunch）；relay serverId
/// 存在时附带。
async fn health(State(ctx): State<RouterContext>) -> Response {
    let snapshot = ctx.engine.snapshot();
    let server_id = crate::relay::server_id(&ctx).await;
    let mut body = serde_json::json!({
        "status": "ok",
        "timestamp": iso_utc_from_unix_millis(now_unix_millis()),
        "ompchamberVersion": ompchamber_version(),
        "runtime": runtime_name(),
        "compatibility": compatibility(),
    });
    if let Some(server_id) = server_id {
        if let Some(map) = body.as_object_mut() {
            map.insert("serverId".to_string(), serde_json::json!(server_id));
        }
    }
    if let Some(map) = body.as_object_mut()
        && let Some(fields) = health_snapshot(&ctx, &snapshot).as_object()
    {
        for (key, value) in fields {
            map.insert(key.clone(), value.clone());
        }
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /api/version`.
/// `GET /api/version`：版本、runtime、启动时间与能力块；有 serverId
/// 时附带。
async fn api_version(State(ctx): State<RouterContext>) -> Response {
    let server_id = crate::relay::server_id(&ctx).await;
    let mut body = serde_json::json!({
        "status": "ok",
        "ompchamberVersion": ompchamber_version(),
        "runtime": runtime_name(),
        "startedAt": started_at_iso(),
        "compatibility": compatibility(),
    });
    if let Some(server_id) = server_id {
        if let Some(map) = body.as_object_mut() {
            map.insert("serverId".to_string(), serde_json::json!(server_id));
        }
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /api/system/info`.
/// `GET /api/system/info`：pid、端口、启动时间等进程信息；tunnelUrl
/// 待 tunnels 移植接线，暂为 null。
async fn system_info(State(ctx): State<RouterContext>) -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ompchamberVersion": ompchamber_version(),
            "runtime": runtime_name(),
            "pid": std::process::id(),
            "startedAt": started_at_iso(),
            // Tunnel active-port wiring lands with the tunnels port; report the
            // configured serve port.
            "port": ctx.config.port,
            "tunnelUrl": serde_json::Value::Null,
        })),
    )
        .into_response()
}

/// `GET /api/system/free-port` — best-effort free TCP port hint on 127.0.0.1.
/// `GET /api/system/free-port`：在 127.0.0.1 绑定 :0 探测一个可用 TCP
/// 端口（尽力而为的提示值）。
async fn free_port() -> Response {
    match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => match listener.local_addr() {
            Ok(addr) if addr.port() > 0 => (
                StatusCode::OK,
                Json(serde_json::json!({ "port": addr.port() })),
            )
                .into_response(),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to allocate port" })),
            )
                .into_response(),
        },
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /api/system/shutdown` — acknowledge, then shut the engine down and
/// exit. JS runs `requireShutdownAuth` (UI session auth, or a tunnel session
/// for tunnel-scoped requests) inline before acknowledging; tunnel scope
/// classification lands with the tunnels port, so every request takes the
/// local `requireAuth` path via [`crate::ui_auth::guard`] (a no-op pass when
/// no UI password is configured, mirroring unconfigured JS wiring).
/// `POST /api/system/shutdown`：先应答，再异步关闭引擎并退出进程
/// （延迟 150ms 让响应先冲刷）。JS 在应答前内联执行 `requireShutdownAuth`
/// （UI 会话或隧道会话）；隧道作用域分类随 tunnels 移植落地，这里所有
/// 请求走本地 `requireAuth` 路径——未配置 UI 密码时直通，与未配置的
/// JS 接线一致。
async fn system_shutdown(State(ctx): State<RouterContext>, request: Request) -> Response {
    let (parts, _body) = request.into_parts();
    if let Err(denied) = crate::ui_auth::guard(&ctx, &parts).await {
        return denied;
    }
    let engine = Arc::clone(&ctx.engine);
    tokio::spawn(async move {
        // Let the response flush before tearing the process down.
        tokio::time::sleep(Duration::from_millis(150)).await;
        engine.shutdown().await;
        std::process::exit(0);
    });
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// `isDevShutdownAllowed`: `OMPCHAMBER_DEV_SHUTDOWN === 'true'`.
/// `isDevShutdownAllowed`：`OMPCHAMBER_DEV_SHUTDOWN` trim 后严格等于
/// 'true' 才放行。
fn dev_shutdown_allowed(env_value: Option<&str>) -> bool {
    matches!(env_value.map(str::trim), Some("true"))
}

/// `isSameOriginRequest`: the Origin header's authority (`host[:port]`, default
/// ports elided per WHATWG URL — `new URL(...).host`) must equal the Host
/// header.
/// `isSameOriginRequest`：Origin 的 authority（host[:port]，默认端口按
/// WHATWG URL 规则省略）必须与 Host 头相等；任一头缺失即拒绝。
fn is_same_origin_request(origin: Option<&str>, host: Option<&str>) -> bool {
    let (Some(raw_origin), Some(raw_host)) = (origin, host) else {
        return false;
    };
    let raw_host = raw_host.trim();
    let Ok(origin) = url::Url::parse(raw_origin) else {
        return false;
    };
    let Some(origin_host) = origin.host_str() else {
        return false;
    };
    let port = origin.port_or_known_default().unwrap_or(0);
    let default_port =
        (origin.scheme() == "http" && port == 80) || (origin.scheme() == "https" && port == 443);
    let authority = if default_port {
        origin_host.to_string()
    } else {
        format!("{origin_host}:{port}")
    };
    authority == raw_host
}

/// `parseLoopbackUrl` + port extraction from `core-routes.js`: http(s) URL on a
/// loopback host, returning its effective port.
/// core-routes.js 的 `parseLoopbackUrl` + 端口提取：http(s) 且主机为
/// loopback（localhost/127.0.0.1/::1/0.0.0.0，接受带括号的 IPv6 写法）
/// 时返回有效端口。
pub(crate) fn parse_loopback_port(raw: &str) -> Option<u16> {
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    // `url` renders IPv6 hosts with brackets; JS compares the bracketed
    // hostname — accept both spellings.
    let host = url.host_str()?.trim_matches(|c| c == '[' || c == ']');
    match host {
        "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" => {}
        _ => return None,
    }
    url.port_or_known_default()
        .filter(|port| (1..=u16::MAX).contains(port))
}

/// `killListenPort` — SIGTERM the PIDs listening on a loopback port (lsof),
/// then SIGKILL after a grace delay. Best-effort, dev-only.
/// `killListenPort`：lsof 找出监听 loopback 端口的进程（排除自身），
/// 先 SIGTERM、宽限 1.2s 后 SIGKILL；尽力而为，仅 dev 使用。
async fn kill_listen_port(port: u16) {
    if cfg!(windows) {
        return;
    }
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output();
    let Ok(output) = output else { return };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let own_pid = std::process::id();
    let pids: Vec<u32> = text
        .split_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0 && *pid != own_pid)
        .collect();
    if pids.is_empty() {
        return;
    }
    for pid in &pids {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    for pid in &pids {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `resolveProcessGroupId` — `ps -o pgid= -p <pid>`.
/// `resolveProcessGroupId`：`ps -o pgid= -p <pid>` 查询进程组 id；
/// 失败或结果为 0 返回 None。
fn resolve_process_group_id(pid: u32) -> Option<u32> {
    if pid == 0 {
        return None;
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pgid| *pgid > 0)
}

/// 向整个进程组（负 pgid）发送指定信号；输出丢弃，尽力而为。
fn kill_process_group_signal(pgid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .args([signal, &format!("-{pgid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `POST /api/system/dev-shutdown` — dev-only escape hatch terminating the
/// whole dev process group.
/// `POST /api/system/dev-shutdown`：dev 专用逃生通道。校验环境开关与
/// 同源 Origin 后，异步清理 previewUrls 中 loopback 端口的监听进程，
/// TERM 再 KILL 本进程与父进程所在的进程组，最终强制退出。
async fn dev_shutdown(State(ctx): State<RouterContext>, request: Request) -> Response {
    if !dev_shutdown_allowed(std::env::var("OMPCHAMBER_DEV_SHUTDOWN").ok().as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "Dev shutdown is disabled" })),
        )
            .into_response();
    }
    let headers = request.headers();
    let origin = headers
        .get(HeaderName::from_static("origin"))
        .and_then(|v| v.to_str().ok());
    let host = headers
        .get(HeaderName::from_static("host"))
        .and_then(|v| v.to_str().ok());
    if !is_same_origin_request(origin, host) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "Invalid origin" })),
        )
            .into_response();
    }

    let body = directory::read_json_body(request).await.unwrap_or_default();
    let preview_ports: Vec<u16> = body
        .get("previewUrls")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .filter_map(parse_loopback_port)
                .collect::<Vec<u16>>()
        })
        .map(|ports| {
            let mut unique = ports;
            unique.sort_unstable();
            unique.dedup();
            unique
        })
        .unwrap_or_default();

    let engine = Arc::clone(&ctx.engine);
    let own_pid = std::process::id();
    tokio::spawn(async move {
        for port in preview_ports {
            kill_listen_port(port).await;
        }

        let pgid = resolve_process_group_id(own_pid);
        let parent_pgid = std::env::var("PPID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .and_then(resolve_process_group_id);

        engine.shutdown().await;

        let mut pgids: Vec<u32> = [pgid, parent_pgid].into_iter().flatten().collect();
        pgids.sort_unstable();
        pgids.dedup();
        for id in &pgids {
            kill_process_group_signal(*id, "-TERM");
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        for id in &pgids {
            kill_process_group_signal(*id, "-KILL");
        }
        // Ensure the server process exits even if the group kill fails.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        std::process::exit(0);
    });

    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// 组装本模块路由，保持 JS 注册顺序：status/system 路由在 /api 鉴权门
/// 之前；settings-utility 与 opencode 路由在其后并套 ui_auth gate 层
/// （未配置 UI 密码时直通）。
pub fn router(ctx: RouterContext) -> Router {
    // JS registration order (bootstrap-runtime.js:77 status routes, :96 the
    // `app.use('/api', requireApiAuth)` gate; feature-routes-runtime.js:136/144
    // the later feature routes): the status/system routes sit BEFORE the /api
    // gate; the settings-utility and opencode routes sit AFTER it and are
    // wrapped with the ui_auth gate layer (a no-op pass while no UI password
    // is configured).
    let gate = crate::ui_auth::middleware(ctx.clone());
    let status_routes = Router::new()
        .route("/health", get(health))
        .route("/api/version", get(api_version))
        .route("/api/system/info", get(system_info))
        .route("/api/system/free-port", get(free_port))
        .route("/api/system/shutdown", post(system_shutdown))
        .route(
            "/api/system/dev-shutdown",
            post(dev_shutdown).layer(DefaultBodyLimit::max(DEV_SHUTDOWN_BODY_LIMIT)),
        )
        .with_state(ctx.clone());
    let gated_routes = Router::new()
        .route("/api/opencode/health", get(engine_info::opencode_health))
        .route("/api/opencode/version", get(engine_info::opencode_version))
        .route(
            "/api/opencode/directory",
            post(directory::set_directory).layer(DefaultBodyLimit::max(OPENCODE_BODY_LIMIT)),
        )
        .route(
            "/api/config/opencode-resolution",
            get(resolution::opencode_resolution),
        )
        .route("/api/config/themes", get(settings_utility::themes))
        .route("/api/config/reload", post(settings_utility::reload))
        .route_layer(gate)
        .with_state(ctx);
    status_routes.merge(gated_routes)
}

/// core_routes 的路由与工具测试，含供子模块复用的测试脚手架
/// （temp_dir/test_ctx/json_response）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EngineConfig, ServerConfig, TunnelOptions};
    use crate::engine::EngineState;
    use crate::hub::EventHub;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request as HttpRequest};
    use std::path::PathBuf;
    use tower::ServiceExt;

/// 以进程 id + 后缀命名新建一个干净临时目录（先清旧残留），供测试
/// 隔离数据目录。
    pub(crate) fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-core-routes-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

/// 构造测试用 RouterContext：固定端口、External 引擎指向不可达地址、
/// 无 UI 密码。
    pub(crate) fn test_ctx(data_dir: PathBuf, engine: Arc<EngineState>) -> RouterContext {
        RouterContext {
            config: Arc::new(ServerConfig {
                port: 39_871,
                host: None,
                lan: false,
                ui_password: None,
                api_only: false,
                data_dir,
                dist_dir: PathBuf::from("/tmp/ompchamber-test-dist"),
                tunnel: TunnelOptions::default(),
                engine: EngineConfig::External {
                    base_url: "http://127.0.0.1:1".to_string(),
                },
            }),
            engine,
            hub: EventHub::new(),
        }
    }

/// 用 tower oneshot 发送请求并解析 JSON 响应为 (状态码, body)；空 body
/// 记为 Null。
    pub(crate) async fn json_response(
        router: Router,
        request: HttpRequest<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = router.oneshot(request).await.expect("oneshot");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        };
        (status, json)
    }

/// 验证 ISO 时间戳以毫秒精度渲染 UTC 形态。
    #[test]
    fn iso_timestamp_renders_utc_with_milliseconds() {
        // 2026-09-26T00:00:00Z == 1790380800000 ms.
        assert_eq!(
            iso_utc_from_unix_millis(1_790_380_800_000),
            "2026-09-26T00:00:00.000Z"
        );
    }

/// 验证 dev-shutdown 开关只接受精确 'true'（大小写与 '1' 均拒绝）。
    #[test]
    fn dev_shutdown_gate_requires_exact_true() {
        assert!(dev_shutdown_allowed(Some("true")));
        assert!(!dev_shutdown_allowed(Some("TRUE")));
        assert!(!dev_shutdown_allowed(Some("1")));
        assert!(!dev_shutdown_allowed(None));
    }

/// 验证同源判定按 authority 匹配：默认端口省略、跨源拒绝、Origin/Host
/// 缺失拒绝。
    #[test]
    fn same_origin_matches_host_authority() {
        assert!(is_same_origin_request(
            Some("http://localhost:5173"),
            Some("localhost:5173")
        ));
        assert!(is_same_origin_request(
            Some("https://example.com"),
            Some("example.com")
        ));
        assert!(!is_same_origin_request(
            Some("http://evil.example"),
            Some("localhost:5173")
        ));
        assert!(!is_same_origin_request(None, Some("localhost:5173")));
        assert!(!is_same_origin_request(Some("http://localhost:5173"), None));
        // Non-hierarchical origins (file://, custom schemes) never match.
        assert!(!is_same_origin_request(
            Some("ompchamber-ui://app"),
            Some("ompchamber-ui://app")
        ));
    }

/// 验证 loopback URL 端口提取与 JS 一致：IPv6、默认端口、非 loopback
/// 与非 http(s) 均正确处理。
    #[test]
    fn loopback_port_parsing_mirrors_js() {
        assert_eq!(parse_loopback_port("http://localhost:4321/"), Some(4321));
        assert_eq!(parse_loopback_port("https://127.0.0.1"), Some(443));
        assert_eq!(parse_loopback_port("http://[::1]:8080"), Some(8080));
        assert_eq!(parse_loopback_port("http://0.0.0.0:3000"), Some(3000));
        assert_eq!(parse_loopback_port("http://example.com:3000"), None);
        assert_eq!(parse_loopback_port("ftp://localhost:3000"), None);
        assert_eq!(parse_loopback_port("not a url"), None);
    }

/// 验证版本号读取自 web 包 manifest（非 'unknown'、非空）。
    #[test]
    fn version_string_comes_from_web_package_manifest() {
        let version = ompchamber_version();
        assert_ne!(version, "");
        assert_ne!(
            version, "unknown",
            "packages/web/package.json must be readable"
        );
    }

/// 验证 /health 返回 JS 形态并平铺引擎快照字段，且不带 engine 对象。
    #[tokio::test]
    async fn health_returns_js_shape_with_engine_snapshot() {
        let engine =
            EngineState::external("http://127.0.0.1:45678".to_string(), Some("pw".to_string()));
        let ctx = test_ctx(temp_dir("health"), engine);
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert!(body["timestamp"].is_string());
        assert_eq!(body["ompchamberVersion"], ompchamber_version());
        assert!(body["runtime"].is_string());
        assert_eq!(body["compatibility"]["apiVersion"], 1);
        assert_eq!(body["compatibility"]["minClientApiVersion"], 1);
        assert_eq!(
            body["compatibility"]["capabilities"]
                .as_array()
                .map(Vec::len),
            Some(6)
        );
        // JS getHealthSnapshot fields.
        assert_eq!(body["openCodePort"], 45678);
        assert_eq!(body["openCodeRunning"], true);
        assert_eq!(body["openCodeSecureConnection"], true);
        assert_eq!(body["openCodeApiPrefix"], "");
        assert_eq!(body["openCodeApiPrefixDetected"], true);
        assert_eq!(body["isOpenCodeReady"], true);
        assert!(body["lastOpenCodeError"].is_null() || body["lastOpenCodeError"].is_string());
        assert_eq!(body["apiOnly"], false);
        assert_eq!(body["desktopNotifyEnabled"], desktop_notify_enabled());
        assert_eq!(
            body["planModeExperimentalEnabled"],
            plan_mode_experimental_enabled()
        );
        // JS /health carries no `engine` object — engine state reaches the
        // snapshot through the flat fields above and launch diagnostics.
        assert!(body.get("engine").is_none());
    }

/// 验证未就绪引擎的字段映射：openCodeRunning/isOpenCodeReady 为假，
/// 错误与启动诊断透传。
    #[tokio::test]
    async fn health_snapshot_maps_a_not_ready_engine() {
        // Not-ready engines cannot be constructed through the public
        // EngineState API (a watch channel with no subscribers reverts to
        // ready), so pin the mapping directly with a synthetic snapshot.
        let ctx = test_ctx(
            temp_dir("health-notready"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let snapshot = serde_json::json!({
            "mode": "managed",
            "ready": false,
            "baseUrl": null,
            "lastError": "engine process exited: signal 9",
            "lastLaunch": { "launchedAt": "2026-09-26T00:00:00.000Z", "port": 41234 },
        });
        let fields = health_snapshot(&ctx, &snapshot);
        assert_eq!(fields["openCodeRunning"], false);
        assert_eq!(fields["isOpenCodeReady"], false);
        assert!(fields["openCodePort"].is_null());
        assert_eq!(
            fields["lastOpenCodeError"],
            "engine process exited: signal 9"
        );
        assert_eq!(fields["lastOpenCodeLaunchDiagnostics"]["port"], 41234);
        assert!(
            fields["openCodeSecureConnection"] == false
                || fields["openCodeSecureConnection"] == true
        );
        // Binary resolution fields are machine-dependent; assert presence only.
        for field in [
            "opencodeBinaryResolved",
            "opencodeBinarySource",
            "opencodeLaunchBinary",
            "opencodeLaunchArgs",
            "opencodeLaunchWrapperType",
            "nodeBinaryResolved",
            "bunBinaryResolved",
        ] {
            assert!(fields.get(field).is_some(), "missing {field}");
        }
    }

/// 验证 /api/version 的 JS 响应形态。
    #[tokio::test]
    async fn api_version_returns_js_shape() {
        let ctx = test_ctx(
            temp_dir("version"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["ompchamberVersion"], ompchamber_version());
        assert!(body["startedAt"].is_string());
        assert_eq!(body["compatibility"]["apiVersion"], 1);
    }

/// 验证 /api/system/info 返回进程 pid、端口与版本数据。
    #[tokio::test]
    async fn system_info_returns_process_and_port_data() {
        let ctx = test_ctx(
            temp_dir("info"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .uri("/api/system/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pid"], std::process::id());
        assert_eq!(body["port"], 39_871);
        assert!(body["tunnelUrl"].is_null());
        assert_eq!(body["ompchamberVersion"], ompchamber_version());
    }

/// 验证 /api/system/free-port 返回 1..=65535 范围内的端口。
    #[tokio::test]
    async fn free_port_returns_a_usable_loopback_port() {
        let (status, body) = json_response(
            router(test_ctx(
                temp_dir("free-port"),
                EngineState::external("http://127.0.0.1:1".into(), None),
            )),
            HttpRequest::builder()
                .uri("/api/system/free-port")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let port = body["port"].as_u64().expect("port number");
        assert!((1..=65_535).contains(&port));
    }

/// 验证缺环境开关或缺 Origin 头的 dev-shutdown 一律 403，且不会触发
/// 进程退出。
    #[tokio::test]
    async fn dev_shutdown_is_forbidden_without_origin_or_gate() {
        // With the gate off: disabled error; with the gate on but no Origin
        // header: invalid origin. Either way 403 { ok: false } — and never an
        // exit, because the success branch is unreachable without both gates.
        let ctx = test_ctx(
            temp_dir("dev-shutdown"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .method(Method::POST)
                .uri("/api/system/dev-shutdown")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["ok"], false);
        assert!(
            body["error"] == "Dev shutdown is disabled" || body["error"] == "Invalid origin",
            "unexpected error: {body}"
        );
    }
}
