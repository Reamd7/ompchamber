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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use pairing::ClientIssuer;
use remote_clients::RemoteClientAuth;
use tunnel_auth::TunnelAuth;

use crate::context::RouterContext;

pub mod pairing;
pub mod remote_clients;
pub mod time;
pub mod tunnel_auth;
mod util;

#[cfg(test)]
mod tests;

/// The process-wide client-auth runtimes for one data directory, mirroring
/// the module-level JS runtimes in `server/index.js` (`index.js:1008-1020`).
/// The registry keeps one instance per data dir so the core_routes port, the
/// tunnels port, and ui_auth observe the same tokens and sessions.
pub struct ClientAuthState {
    pub remote_clients: Arc<RemoteClientAuth>,
    pub pairing: Arc<pairing::ClientPairing>,
    pub tunnel_auth: Arc<TunnelAuth>,
}

/// Shared runtimes for the server's data directory.
pub fn state(ctx: &RouterContext) -> Arc<ClientAuthState> {
    state_for_data_dir(&ctx.config.data_dir)
}

/// Shared runtimes for an explicit data directory (the remote-clients and
/// client-pairing-sessions stores live directly inside it).
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
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
