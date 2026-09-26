//! Port of the `opencode/` config-CRUD surfaces from `packages/web/server`:
//!
//! JS source map:
//! - `opencode/routes.js` remainder      → `routes.rs` (behavior/AGENTS.md
//!   endpoints, provider config CRUD, MCP auth pending + OAuth callback)
//! - `opencode/config-entity-routes.js`  → `entity_routes.rs` (agent/command/
//!   MCP entity routes; the snippet routes stay with the snippets port)
//! - `opencode/agents.js`                → `agents.rs`
//! - `opencode/commands.js`              → `commands.rs`
//! - `opencode/mcp.js`                   → `mcp.rs`
//! - `opencode/providers.js`             → `providers.rs`
//! - `opencode/auth.js`                  → `auth.rs`
//! - `opencode/claude-cli-auth.js`       → `claude_cli.rs`
//! - `opencode/config-mutation-response.js` → `mutation.rs`
//! - `opencode/shared.js` subset         → `config_layers.rs` (config layer
//!   machinery) + `md_file.rs` (markdown/frontmatter) + `yaml.rs`
//!   (frontmatter YAML subset — no yaml crate is on the allow-list)
//! - `opencode/project-directory-runtime.js` → `project_directory.rs`
//!   (inline port; `fs_routes::workspace` keeps its own copy)

mod agents;
mod auth;
mod claude_cli;
mod commands;
mod config_layers;
mod entity_routes;
mod mcp;
mod md_file;
mod mutation;
mod project_directory;
mod providers;
mod routes;
mod webutil;
mod yaml;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::context::RouterContext;

/// JS module-scope path constants (`shared.js` / `auth.js` / `routes.js`),
/// resolved per call instead of module load so tests can inject temp roots.
#[derive(Clone, Debug)]
pub(crate) struct OpenCodeEnv {
    /// `~/.config/opencode` (JS `OPENCODE_CONFIG_DIR`).
    pub config_dir: PathBuf,
    /// `~/.local/share/opencode` (auth.js `OPENCODE_DATA_DIR`).
    pub data_dir: PathBuf,
    /// `~` — the static agent-dir fallback is `<home>/.omp/agent`.
    pub home: PathBuf,
    /// Resolved `OPENCODE_CONFIG` (custom layer), read at call time like the JS.
    pub custom_config: Option<PathBuf>,
}

impl OpenCodeEnv {
    /// Production environment: paths from `$HOME`, custom layer from
    /// `$OPENCODE_CONFIG` (resolved per call, JS comment on `getConfigPaths`).
    pub fn current() -> Self {
        let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let custom_config = std::env::var("OPENCODE_CONFIG")
            .ok()
            .map(|value| webutil::resolve_path(&value));
        Self {
            config_dir: home.join(".config").join("opencode"),
            data_dir: home.join(".local").join("share").join("opencode"),
            home,
            custom_config,
        }
    }

    /// JS `CONFIG_FILE` = `~/.config/opencode/config.json`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    /// JS `AGENT_DIR` = `~/.config/opencode/agents`.
    pub fn agent_dir(&self) -> PathBuf {
        self.config_dir.join("agents")
    }

    /// JS `COMMAND_DIR` = `~/.config/opencode/commands`.
    pub fn command_dir(&self) -> PathBuf {
        self.config_dir.join("commands")
    }

    /// JS `SKILL_DIR` = `~/.config/opencode/skills`.
    pub fn skill_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }
}

/// `routes.js` `pendingMcpAuthContextByState` — parked OAuth contexts keyed by
/// the per-flow `state` secret, pruned lazily against a 30-minute TTL.
#[derive(Debug, Clone)]
pub(crate) struct PendingMcpAuthContext {
    pub name: String,
    pub directory: Option<String>,
    pub origin: Option<String>,
    pub expires_at: u64,
}

#[derive(Default)]
pub(crate) struct PendingMcpAuthState {
    entries: Mutex<HashMap<String, PendingMcpAuthContext>>,
}

impl PendingMcpAuthState {
    /// `pruneExpiredPendingMcpAuthContexts`.
    fn prune_expired(&self) {
        let now = webutil::now_millis();
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| entry.expires_at > now);
    }

    fn get(&self, state: &str) -> Option<PendingMcpAuthContext> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(state)
            .cloned()
    }

    fn insert(&self, state: String, entry: PendingMcpAuthContext) {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(state, entry);
    }

    fn remove(&self, state: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(state);
    }
}

/// Module-local state carried by the returned router.
#[derive(Clone)]
pub(crate) struct ModuleState {
    pub ctx: RouterContext,
    pub env: OpenCodeEnv,
    pub pending: Arc<PendingMcpAuthState>,
}

pub fn router(ctx: RouterContext) -> axum::Router {
    router_with_env(ctx, OpenCodeEnv::current())
}

/// Test seam mirroring the JS dependency injection: the same router with an
/// explicit environment (temp config/data roots).
pub(crate) fn router_with_env(ctx: RouterContext, env: OpenCodeEnv) -> axum::Router {
    let state = ModuleState {
        ctx,
        env,
        pending: Arc::new(PendingMcpAuthState::default()),
    };
    routes::provider_routes()
        .merge(entity_routes::entity_routes())
        .with_state(state)
}
