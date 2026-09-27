//! Port of `server/lib/tunnels/` (registry, routes, executable-search,
//! install-help, managed-config, providers, types, index) plus
//! `server/lib/cloudflare-tunnel.js`, `server/lib/ngrok-tunnel.js`, and
//! `server/lib/dev-tunnel/` (host runtime + local client).
//!
//! JS file → Rust file:
//! - `tunnels/types.js` → `types.rs`
//! - `tunnels/install-help.js` → `install_help.rs`
//! - `tunnels/executable-search.js` → `executable_search.rs`
//! - `tunnels/managed-config.js` → `managed_config.rs`
//! - `tunnels/registry.js` → `registry.rs`
//! - `cloudflare-tunnel.js` + `tunnels/providers/cloudflare.js` → `cloudflare.rs`
//! - `ngrok-tunnel.js` + `tunnels/providers/ngrok.js` → `ngrok.rs`
//! - `tunnels/index.js` → `service.rs`
//! - `tunnels/routes.js` → `routes.rs`
//! - `opencode/tunnel-auth.js` → consumed from `crate::client_auth::tunnel_auth`
//! - `dev-tunnel/{runtime,client}.js` → `dev_tunnel.rs`
//! - child-process seam for the providers → `runner.rs`

mod cloudflare;
mod dev_tunnel;
mod executable_search;
mod install_help;
mod managed_config;
mod ngrok;
mod registry;
mod routes;
pub use routes::tunnel_public_url;
mod runner;
mod service;
mod types;

pub use dev_tunnel::{DevTunnelClient, DevTunnelState, ListedTunnel};

use crate::context::RouterContext;

/// Tunnel module router (JS: tunnel-wiring-runtime `initialize` +
/// createDevTunnelRuntime's upgrade path).
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router_with(routes::module_state(ctx))
}
