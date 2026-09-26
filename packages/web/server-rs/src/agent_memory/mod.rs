//! Port of `server/lib/agent-memory/` — the agent memory store.
//!
//! JS file → Rust file:
//! - `runtime.js` → [`runtime`] (store: two scopes, restatement-replacement,
//!   flagged-entry retention, per-key write locks, atomic writes)
//! - `actions.js` → [`actions`] (the `memory.*` dispatch the
//!   `ompchamber_memory` tool calls; consumed by the openchamber-control port)
//! - `routes.js` → [`routes`] (the panel's read/correct/delete surface)
//! - `feature-flag.js` → [`feature_flag`] (`OMPCHAMBER_MEMORY_ENABLE`)
//! - `project-resolution.js` → [`project_resolution`] (session directory →
//!   project via worktree→project mapping)
//! - `threat-patterns.js` → [`threat_patterns`] (hand-rolled matchers; the
//!   allowed crate set has no `regex`)
//!
//! Wiring parity with `server/index.js`: the routes and the actions share one
//! `is_agent_memory_enabled` gate (feature flag first — unreleased means
//! absent, so no stored setting can bring it back — then the
//! `agentMemoryToolEnabled` setting), the resolver maps worktree sessions
//! onto the project's store, and writes are announced over the SSE hub as
//! `ompchamber:agent-memory-changed`. Like the rest of the Rust port, the
//! JS `~/.config/ompchamber` user-config root is the resolved
//! `ServerConfig::data_dir` (`OMPCHAMBER_DATA_DIR` or the same default).

pub mod actions;
pub mod feature_flag;
pub mod project_resolution;
pub mod routes;
pub mod runtime;
pub mod threat_patterns;

pub use actions::{AgentMemoryActions, MemoryActionError, OnMemoryChanged, ResolveProjectId};
pub use feature_flag::is_agent_memory_feature_available;
pub use project_resolution::{ListProjectPaths, MemoryProjectResolver, ResolvePrimaryWorktreeRoot};
pub use runtime::{
    AgentMemoryRuntime, AllMemory, CreateInput, CreateResult, IdFactory, MemoryEnabledGate,
    MemoryEntry, MemoryError, MemoryFile, RemoveResult, Scope, Target, UpdatePatch, UpdateResult,
};
pub use threat_patterns::{find_threat_pattern, looks_like_injection};

use std::sync::Arc;

use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::hub::EventHub;

pub fn router(ctx: RouterContext) -> axum::Router {
    let runtime = runtime_for_context(&ctx);
    let gate_ctx = ctx.clone();
    let gate: MemoryEnabledGate = Arc::new(move || {
        let gate_ctx = gate_ctx.clone();
        Box::pin(async move { Ok(is_agent_memory_enabled(&gate_ctx).await) })
    });
    routes::routes(runtime, Some(gate))
}

/// `createAgentMemoryRuntime({ projectsDirPath, userConfigRoot })`.
pub fn runtime_for_context(ctx: &RouterContext) -> Arc<AgentMemoryRuntime> {
    Arc::new(AgentMemoryRuntime::new(
        ctx.config.data_dir.clone(),
        ctx.config.data_dir.join("projects"),
        None,
    ))
}

/// One switch for everything memory-related (`isAgentMemoryEnabled` in
/// index.js): it gates the tool, the routes, and the session index alike, so
/// turning memory off leaves nothing behind that still reads or writes the
/// store. Never fails: an unreadable settings file reads as off.
pub async fn is_agent_memory_enabled(ctx: &RouterContext) -> bool {
    // The feature gate comes first: unreleased means absent, not merely
    // switched off, so no stored setting can bring it back.
    if !feature_flag::is_agent_memory_feature_available() {
        return false;
    }
    let settings = crate::settings::store(ctx)
        .read_migrated()
        .await
        .unwrap_or_default();
    settings
        .get("agentMemoryToolEnabled")
        .and_then(Value::as_bool)
        == Some(true)
}

/// `createMemoryProjectResolver({ listProjectPaths, resolvePrimaryWorktreeRoot,
/// managedProjectRoots: [<config root>/chats] })`.
pub fn project_resolver(ctx: &RouterContext) -> MemoryProjectResolver {
    let store = crate::settings::store(ctx);
    let list_project_paths: ListProjectPaths = Arc::new(move || {
        let store = store.clone();
        Box::pin(async move {
            // JS `readSettingsFromDiskMigrated().catch(() => null)` — an
            // unreadable list reads as no configured projects, and the
            // git-derived root below still converges worktrees.
            let settings = store.read_migrated().await.unwrap_or_default();
            let paths = crate::settings::normalization::sanitize_projects(settings.get("projects"))
                .unwrap_or_default()
                .iter()
                .filter_map(|project| {
                    project
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>();
            Ok(paths)
        })
    });

    let git = crate::projects::default_git_runner();
    let resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot =
        Arc::new(move |directory: String| {
            let git = git.clone();
            Box::pin(async move {
                Some(
                    crate::projects::resolve_primary_worktree_root(&directory, &git)
                        .await
                        .root,
                )
            })
        });

    MemoryProjectResolver::new(
        list_project_paths,
        resolve_primary_worktree_root,
        vec![
            ctx.config
                .data_dir
                .join("chats")
                .to_string_lossy()
                .into_owned(),
        ],
    )
}

/// `emitAgentMemoryChangedEvent`: tells open panels that the agent changed
/// what it remembers, so what it just stored is visible without reopening
/// anything. Mirrors the SSE envelope the other hub broadcasters use.
pub fn hub_announcer(hub: Arc<EventHub>) -> OnMemoryChanged {
    Arc::new(move |event: &Value| {
        hub.publish_json(
            "ompchamber:agent-memory-changed",
            &json!({
                "type": "ompchamber:agent-memory-changed",
                "properties": event,
            }),
        );
    })
}

/// `createAgentMemoryActions({...})` with the production wiring: the shared
/// runtime, the project resolver, the shared settings gate, and the hub
/// announcer.
pub fn actions_for_context(ctx: &RouterContext) -> AgentMemoryActions {
    let gate_ctx = ctx.clone();
    let gate: MemoryEnabledGate = Arc::new(move || {
        let gate_ctx = gate_ctx.clone();
        Box::pin(async move { Ok(is_agent_memory_enabled(&gate_ctx).await) })
    });
    AgentMemoryActions::with_resolver(
        runtime_for_context(ctx),
        project_resolver(ctx),
        Some(gate),
        Some(hub_announcer(ctx.hub.clone())),
    )
}
