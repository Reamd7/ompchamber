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

mod e2ee;
mod host_client;
mod host_lock;
mod identity;
mod routes;
mod service;
mod signing_key;
mod tunnel_codec;
mod tunnel_host;

#[cfg(test)]
mod tests;

use crate::context::RouterContext;

pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router_with(routes::ModuleState {
        service: service::service(&ctx),
    })
}
