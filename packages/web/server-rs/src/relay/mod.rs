//! Port of `server/lib/relay/` — the private relay host side.
//!
//! The private relay lets an OpenChamber client (mobile app, browser, or
//! another desktop) reach a user's OpenChamber instance through
//! OpenChamber-hosted infrastructure when the instance is not directly
//! reachable. The instance dials **outbound** to the relay; nothing needs to
//! be exposed inbound. Traffic is end-to-end encrypted between the two
//! endpoints — the relay infrastructure forwards opaque ciphertext and cannot
//! read application traffic.
//!
//! JS file → Rust file:
//! - `relay/e2ee.js` → `e2ee.rs` (crypto + responder handshake, byte-compatible
//!   with `packages/ui/src/lib/relay/*`; pinned vectors in-module)
//! - `relay/tunnel-codec.js` → `tunnel_codec.rs`
//! - `relay/tunnel-host.js` → `tunnel_host.rs`
//! - `relay/host-client.js` → `host_client.rs`
//! - `relay/service.js` → `service.rs` (+ `routes.rs`)
//! - `relay/signing-key.js` → `signing_key.rs`
//! - `relay/identity.js` → `identity.rs`
//! - `relay/host-lock.js` → `host_lock.rs`
//!
//! Routes: `GET/POST /api/ompchamber/relay/{status,enable,disable}`.

pub(crate) mod e2ee;
pub(crate) mod host_client;
pub(crate) mod host_lock;
pub(crate) mod identity;
pub(crate) mod routes;
pub(crate) mod service;
pub(crate) mod signing_key;
pub(crate) mod tunnel_codec;
pub(crate) mod tunnel_host;

#[cfg(test)]
mod tests;

use crate::context::RouterContext;

/// The relay identity's stable server id (get_or_create, same store the JS
/// relay service uses). Exposed for /health and /api/version parity.
/// Derived once per process — the JS caches this on the relay service
/// instance; deriving per request would read settings + run P-256 math on
/// every /health.
pub async fn server_id(ctx: &RouterContext) -> Option<String> {
    static CACHED: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    CACHED
        .get_or_init(|| async {
            let store = crate::settings::store(ctx);
            let runtime = identity::RelayIdentityRuntime::new(store, identity::system_clock());
            runtime
                .get_relay_identity()
                .await
                .ok()
                .map(|identity| identity.server_id.clone())
        })
        .await
        .clone()
}

pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router_with(routes::ModuleState {
        service: service::service(&ctx),
    })
}
