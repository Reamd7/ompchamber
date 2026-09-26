//! Port of `server/lib/project-context/` (`runtime.js`, `routes.js`).
//!
//! Project knowledge storage — notes, todos, and plan files — under
//! `<projects-dir>/<projectId>/`. The runtime owns `context.json` (the sole
//! server-written file), plan markdown under `plans/`, and a one-time
//! migration of the legacy `projectNotes`/`projectTodos`/`projectPlanFiles`
//! keys out of the client-owned `<projectId>.json`.
//!
//! In the JS server, `index.js` builds one `projectContextRuntime` shared by
//! these routes and the session-knowledge runtime; [`shared_runtime`] mirrors
//! that with a process-wide registry keyed by the projects directory.

pub mod routes;
pub mod runtime;

#[cfg(test)]
mod tests;

pub use runtime::{
    DeleteOutcome, Note, NoteMutation, NoteOrigin, ParsedPlan, PlanCreateResult, PlanLink,
    PlanPinnedResult, PlanRead, PlanSaveResult, ProjectContext, ProjectContextRuntime, Todo,
    parse_plan_markdown,
};

use std::sync::Arc;

use crate::context::RouterContext;

/// The process-wide runtime for the server's projects directory (mirrors the
/// JS module-level `projectContextRuntime` singleton).
pub fn shared_runtime(ctx: &RouterContext) -> Arc<ProjectContextRuntime> {
    runtime::shared(&ctx.config.data_dir.join("projects"))
}

pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(shared_runtime(&ctx))
}
