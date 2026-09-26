//! Port of `server/lib/projects/` (`project-config.js`, `project-id.js`).
//!
//! This JS module owns no HTTP routes — verified consumers compose it as a
//! library: `server/index.js` builds the runtime over
//! `OMPCHAMBER_PROJECTS_CONFIG_DIR` (`<data-dir>/projects`), the
//! scheduled-tasks runtime/service drive the task CRUD, agent-memory and the
//! opencode routes derive ids via `createProjectIdFromPath`, and worktree
//! path normalization comes from `opencode/shared.js` + `git/service.js`
//! (ported here as [`worktree`]). So this module exposes the runtime as
//! `pub` APIs and registers no routes; the HTTP surfaces live in the
//! `scheduled_tasks` and `settings` ports.
//!
//! Tolerance note: the JS reads project configs with strict `JSON.parse`;
//! the Rust port reads them with the same tolerance the wider server applies
//! to hand-edited JSONC (comments + trailing commas accepted, loose property
//! names rejected). A genuinely broken file is still an isolated error — it
//! is never silently treated as an empty config that the next write would
//! flush over the file.

pub mod model;
pub mod project_id;
pub mod runtime;
pub mod worktree;

pub use model::{Execution, LoopEntry, Schedule, ScheduledTask, TaskState};
pub use project_id::create_project_id_from_path;
pub use runtime::{DeleteResult, ProjectConfigRuntime, StateUpdateResult, UpsertResult};
pub use worktree::{
    GitCommandResult, GitRunner, PrimaryWorktreeRoot, default_git_runner,
    derive_primary_worktree_root_from_git_dir, find_worktree_root, resolve_primary_worktree_root,
};

#[cfg(test)]
mod tests;

use crate::context::RouterContext;

/// No routes: see the module docs. Kept for the module router contract.
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
