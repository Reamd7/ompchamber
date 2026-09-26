//! Port of the settings module: `server/lib/opencode/settings-runtime.js`,
//! `settings-helpers.js`, `settings-normalization-runtime.js` (plus the
//! tunnel type normalizers and `createProjectIdFromPath` they depend on) and
//! the `GET`/`PUT /api/config/settings` endpoints from `routes.js`.
//!
//! Persistence keeps settings.json as a raw JSON object so JS spread
//! semantics (unknown keys, undefined-clears) round-trip exactly; [`model::Settings`]
//! is the typed projection for consumers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::context::RouterContext;

pub mod helpers;
pub mod model;
pub mod normalization;
pub mod routes;
pub mod runtime;

#[cfg(test)]
mod tests;
pub use model::Settings;
pub use runtime::SettingsStore;

/// Shared store for the server's data directory. The singleton registry
/// keeps the persist lock and the one-shot migration flags process-wide,
/// mirroring the JS module-level runtime state.
pub fn store(ctx: &RouterContext) -> Arc<SettingsStore> {
    store_for_path(&ctx.config.data_dir.join("settings.json"))
}

/// Shared store for an explicit settings.json path (used by tests and, later,
/// modules that operate on auxiliary data directories).
pub fn store_for_path(settings_path: &Path) -> Arc<SettingsStore> {
    static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<SettingsStore>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(settings_path).and_then(Weak::upgrade) {
        return existing;
    }
    let store = Arc::new(SettingsStore::new(settings_path.to_path_buf()));
    registry.insert(settings_path.to_path_buf(), Arc::downgrade(&store));
    store
}

pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(store(&ctx))
}
