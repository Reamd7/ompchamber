//! Port of `server/lib/session-goal/` — file-backed goal objectives
//! ([`objectives`]), server-side goal creation ([`create`]), the backend-driven
//! goal continuation loop ([`runtime`]), and the objective-file HTTP surface
//! ([`routes`]). See `server/lib/session-goal/DOCUMENTATION.md` for the
//! behavioral contract.
//!
//! Composition-root wiring notes (JS `server/index.js`):
//! - The runtime subscribes to the global SSE hub. `spawn_hub_bridge` wires
//!   the equivalent subscription against [`crate::hub::EventHub`] frames; the
//!   event-stream module owns the envelope, so the bridge parses
//!   `{payload, directory}` defensively. Direct feeders (tests, alternate
//!   wiring) call [`SessionGoalRuntime::process_payload`].
//! - `emitGoalNotification` is injected in JS with desktop + UI broadcast +
//!   web-push fanout; until the notifications module is ported the default
//!   notifier keeps the `notifyOnCompletion` gate and hub broadcast only.
//! - The small-model audit and objective distillation call the (not yet
//!   ported) small-model service; those seams report unavailability and the
//!   goal loop follows the documented degraded path.

pub mod create;
pub mod objectives;
pub mod routes;
pub mod runtime;

pub use create::{
    CreateDeps, CreateGoalError, CreateGoalParams, DistillRequest, Distiller, PatchFetch,
    WarningSink, build_goal_intro_text, create_session_goal, engine_patch_fetch,
};
pub use objectives::{
    GOAL_OBJECTIVE_CHAR_LIMIT, ObjectiveError, delete_objective, goals_dir, is_valid_objective_key,
    read_objective, write_objective,
};
pub use runtime::{
    AUDIT_FAIL_LIMIT, AuditError, AuditOutput, AuditRequest, AuditService, BLOCKED_STREAK_LIMIT,
    EngineRequestError, Fetch, GoalMetadata, GoalNotifier, IDLE_QUIET_MS, IsEnabled,
    KICKOFF_QUIET_MS, MAX_AUTO_TURNS, SessionGoalRuntime, SessionGoalRuntimeOptions,
    build_continuation_prompt, engine_fetch, hub_notifier, parse_goal_metadata,
    settings_is_enabled, unavailable_audit,
};

use std::sync::Arc;

use serde_json::Value;

use crate::context::RouterContext;
use crate::hub::EventHub;

/// `registerSessionGoalRoutes(app)` — the objective-file route family.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::routes(ctx.config.data_dir.clone())
}

/// Production runtime (engine fetch, settings gate, hub notifier, data-dir
/// objectives). Feed it through [`spawn_hub_bridge`] or direct
/// `process_payload` calls — mirrors `createSessionGoalRuntime(...)` +
/// `globalMessageStreamHub.subscribeEvent(...)` in index.js.
pub fn runtime(ctx: &RouterContext) -> Arc<SessionGoalRuntime> {
    SessionGoalRuntime::new(SessionGoalRuntimeOptions {
        fetch: engine_fetch(Arc::clone(&ctx.engine)),
        audit: unavailable_audit(),
        notifier: hub_notifier(Arc::clone(&ctx.hub), ctx.config.data_dir.clone()),
        is_enabled: settings_is_enabled(ctx.config.data_dir.clone()),
        data_dir: ctx.config.data_dir.clone(),
        idle_quiet_ms: runtime::IDLE_QUIET_MS,
        kickoff_quiet_ms: runtime::KICKOFF_QUIET_MS,
        max_auto_turns: runtime::MAX_AUTO_TURNS,
    })
}

/// Subscribe the runtime to hub frames. Frame data is parsed as the
/// event-stream envelope `{payload, directory}`; a `payload.payload` object
/// wins over the wrapper (index.js `raw?.payload` handling), and `global`
/// directories become an empty hint.
pub fn spawn_hub_bridge(
    runtime: Arc<SessionGoalRuntime>,
    hub: Arc<EventHub>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut receiver = hub.subscribe();
        loop {
            let Ok(event) = receiver.recv().await else {
                return;
            };
            let Ok(envelope) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let raw = envelope.get("payload").cloned().unwrap_or(Value::Null);
            let payload = raw
                .get("payload")
                .filter(|inner| inner.is_object())
                .cloned()
                .unwrap_or(raw);
            if !payload.is_object() {
                continue;
            }
            let directory = envelope
                .get("directory")
                .and_then(Value::as_str)
                .filter(|directory| !directory.is_empty() && *directory != "global")
                .unwrap_or("")
                .to_string();
            runtime.process_payload(&payload, &directory);
        }
    })
}
