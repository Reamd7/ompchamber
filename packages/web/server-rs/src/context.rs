//! Shared construction context handed to every ported module router.
//!
//! Contract for module ports (mirrors how `server/index.js` threads shared
//! runtimes into each `create*Runtime`): a module owns `src/<name>/mod.rs`
//! and exposes `pub fn router(ctx: RouterContext) -> axum::Router` returning a
//! fully-stateless router (apply any module-local state with
//! `Router::with_state` before returning). Modules never edit `lib.rs`,
//! `main.rs`, or `Cargo.toml`; new shared crates must be proposed in the
//! module's PORT-MANIFEST entry instead.

use std::sync::Arc;

use crate::config::ServerConfig;
use crate::engine::EngineState;
use crate::hub::EventHub;

#[derive(Clone)]
pub struct RouterContext {
    pub config: Arc<ServerConfig>,
    pub engine: Arc<EngineState>,
    pub hub: Arc<EventHub>,
}
