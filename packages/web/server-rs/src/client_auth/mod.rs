//! Port of `server/lib/client-auth/` (`remote-clients.js`, `pairing.js`) plus
//! the tunnel session auth surface from `server/lib/opencode/tunnel-auth.js`.
//!
//! ## Layout
//! - [`remote_clients`]: the trusted-device registry at
//!   `<data_dir>/remote-clients.json` — token issuance (returned once, stored
//!   hashed), revocation, purge, relay-demand tracking, and bearer
//!   authentication with timing-safe hash comparison.
//! - [`pairing`]: Pairing v2 sessions at
//!   `<data_dir>/client-pairing-sessions.json` — create/cancel/list with the
//!   10-minute default TTL, and one-time secret redemption into a remote
//!   client token.
//! - [`tunnel_auth`]: in-memory tunnel session auth — request-scope
//!   classification, one-time bootstrap token exchange, tunnel session
//!   cookies (`oc_tunnel_session`), and the connect rate limiter.
//!
//! ## Routes
//! This module owns no HTTP routes. In the JS server the `/api/client-auth/*`
//! endpoints live in `opencode/core-routes.js` (the core_routes port) and the
//! `/connect` bootstrap endpoint likewise consumes the tunnel-auth
//! controller; both consume [`state`] for the shared runtimes.
//!
//! ## ui_auth wiring points (NOT wired here — ui_auth is out of scope)
//! The JS gate consumes `remoteClientAuthRuntime.authenticateBearerToken`
//! through `authenticateClientRequest` (`ui-auth.js`):
//! 1. `requireAuth` password mode (`ui-auth.js:731`): after the session
//!    cookie check, a bearer token that authenticates passes the gate. Rust:
//!    `ui_auth::check_require_auth` (`src/ui_auth/mod.rs`) — between the
//!    `authenticate_url_token` check and `unauthorized_response`. Note
//!    `pub fn guard` is synchronous and file-backed auth is async, so the
//!    check needs an async surface (or a pre-resolved decision threaded in
//!    by callers that hold the context).
//! 2. `GET /auth/session` bearer branch (`ui-auth.js:751-766`): an explicit
//!    bearer credential decides on its own — success answers
//!    `{"authenticated":true,"scope":"client"}`, failure 401
//!    `{"authenticated":false,"locked":true}` with no cookie fallback. Rust:
//!    `ui_auth::session_status` (`src/ui_auth/mod.rs`, the
//!    `authorization.to_ascii_lowercase().starts_with("bearer ")` branch)
//!    currently returns the unconditional 401 and should call
//!    [`remote_clients::RemoteClientAuth::authenticate_bearer_token`] with
//!    [`remote_clients::Transport::Relay`] when the request carries a
//!    non-empty `x-ompchamber-relay-connection` header (so transport healing
//!    and last-used tracking fire), else [`Transport::Direct`].
//!    [`remote_clients::RemoteClientAuth::is_valid_client_token`] is the
//!    boolean convenience for these call sites.
//! `server/lib/client-auth/`（remote-clients.js、pairing.js）与
//! `server/lib/opencode/tunnel-auth.js` 隧道会话认证面的移植。
//! 子模块布局、路由归属与 ui_auth 待接线事项见上方英文说明；本模块
//! 不拥有 HTTP 路由，各路由移植通过 state() 取这里的共享运行时。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use pairing::ClientIssuer;
use remote_clients::RemoteClientAuth;
use tunnel_auth::TunnelAuth;

use crate::context::RouterContext;

/// Pairing v2 配对会话（client-pairing-sessions.json）：创建/取消/列举
/// 与一次性密钥兑换成远程客户端令牌。
pub mod pairing;
/// 受信设备注册表（remote-clients.json）：令牌签发、吊销、清理、
/// relay 需求统计与 bearer 认证。
pub mod remote_clients;
/// 可注入墙钟与 ISO-8601 / RFC 7231 时间工具。
pub mod time;
/// 内存态隧道会话认证：请求作用域分类、一次性 bootstrap 令牌交换、
/// `oc_tunnel_session` cookie 与连接限速。
pub mod tunnel_auth;
/// 共享的哈希、常数时间比较与随机编码工具。
mod util;

/// client_auth 模块的测试。
#[cfg(test)]
mod tests;

/// The process-wide client-auth runtimes for one data directory, mirroring
/// the module-level JS runtimes in `server/index.js` (`index.js:1008-1020`).
/// The registry keeps one instance per data dir so the core_routes port, the
/// tunnels port, and ui_auth observe the same tokens and sessions.
/// 单个 data 目录的进程级 client-auth 运行时集合，对应 server/index.js
/// 的模块级 JS 运行时（index.js:1008-1020）；每个 data 目录一个实例，
/// 使 core_routes、tunnels 移植与 ui_auth 观察到同一批令牌与会话。
pub struct ClientAuthState {
    /// 受信设备注册表运行时。
    pub remote_clients: Arc<RemoteClientAuth>,
    /// 配对会话运行时。
    pub pairing: Arc<pairing::ClientPairing>,
    /// 隧道会话认证运行时（纯内存）。
    pub tunnel_auth: Arc<TunnelAuth>,
}

/// Shared runtimes for the server's data directory.
/// 取服务器 data 目录对应的共享运行时（内部委托 state_for_data_dir）。
pub fn state(ctx: &RouterContext) -> Arc<ClientAuthState> {
    state_for_data_dir(&ctx.config.data_dir)
}

/// Shared runtimes for an explicit data directory (the remote-clients and
/// client-pairing-sessions stores live directly inside it).
/// 取/建指定 data 目录的共享运行时：注册表按路径缓存 Weak 引用，实例
/// 仍存活即复用，否则重建；remote-clients 与 pairing 的 store 文件
/// 就存放在该目录内。
pub fn state_for_data_dir(data_dir: &Path) -> Arc<ClientAuthState> {
    static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<ClientAuthState>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(data_dir).and_then(Weak::upgrade) {
        return existing;
    }
    let clock = time::system_clock();
    let remote_clients = Arc::new(RemoteClientAuth::new(
        data_dir.join("remote-clients.json"),
        clock.clone(),
    ));
    // JS passes the same remote-client runtime into the pairing runtime.
    let issuer: Arc<dyn ClientIssuer> = remote_clients.clone();
    let pairing_runtime = Arc::new(pairing::ClientPairing::new(
        data_dir.join("client-pairing-sessions.json"),
        clock.clone(),
        pairing::DEFAULT_TTL_MS,
        issuer,
    ));
    let state = Arc::new(ClientAuthState {
        remote_clients,
        pairing: pairing_runtime,
        tunnel_auth: Arc::new(TunnelAuth::new(clock)),
    });
    registry.insert(data_dir.to_path_buf(), Arc::downgrade(&state));
    state
}

/// Module router: empty by contract — see the Routes section above.
/// 模块路由：按契约为空——HTTP 端点归 core_routes 移植（见上方
/// Routes 一节）。
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
