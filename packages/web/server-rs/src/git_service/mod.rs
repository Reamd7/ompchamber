//! Port of `packages/web/server/lib/git/` — the `/api/git/*` route family and
//! its service layer.
//!
//! JS source map:
//! - `git/routes.js`            → `routes.rs`
//! - `git/service.js`           → `service.rs` (context/status/diff/identity
//!   plumbing) + `service_ops.rs` (staging, commit, branches, remotes,
//!   push/pull/fetch, merge/rebase, stash, log) + `worktrees.rs` (worktree
//!   management + bootstrap) + `integrate.rs` (integrate workflow)
//! - `git/credentials.js`       → `identity.rs`
//! - `git/identity-storage.js`  → `identity.rs`
//! - `git/worktree-watcher.js`  → `watcher.rs` (polling watcher; see module
//!   docs for the fs.watch → poll substitution)
//!
//! The JS `simple-git` dependency is not ported as a library: the service
//! invokes `git` directly with the same argv shapes and reproduces the
//! response shapes simple-git's parsers produced (verified against
//! simple-git@3.36.0 in `packages/web/node_modules`).
//!
//! Known gaps (see PORT-MANIFEST.md):
//! - The worktree watcher polls instead of fs.watch (no notify crate); event
//!   debouncing is approximated by the poll interval.
//! - `listProjects` for the watcher reads `<data_dir>/settings.json` directly
//!   instead of going through the settings runtime (not yet wired here).
//! - `git status` runs from the repository root (simple-git's baseDir is the
//!   caller directory); identical outcomes, one fewer process concern.

mod exec;
mod identity;
mod integrate;
mod paths;
pub mod routes;
pub mod service;
pub mod service_ops;
mod watcher;
pub mod worktrees;

use std::sync::Arc;

use crate::context::RouterContext;

pub use routes::GitState;
pub use watcher::{WorktreeWatcher, resolve_git_common_dir};

pub fn router(ctx: RouterContext) -> axum::Router {
    let state = GitState {
        service: Arc::new(service::GitService::new()),
        identity: Arc::new(identity::IdentityStorage::default()),
        hub: Arc::clone(&ctx.hub),
        data_dir: ctx.config.data_dir.clone(),
    };

    // Linked-worktree topology watcher (JS wires this in server/index.js):
    // polls registered projects' `.git/worktrees` metadata and broadcasts
    // `ompchamber:worktrees-changed` frames on the shared event hub.
    {
        let hub = Arc::clone(&ctx.hub);
        let data_dir = ctx.config.data_dir.clone();
        let list_projects: watcher::ListProjects =
            Arc::new(move || watcher::list_projects_from_settings(&data_dir));
        let on_changed: watcher::OnWorktreesChanged = Arc::new(move |directories| {
            watcher::publish_worktrees_changed(&hub, directories);
        });
        let mut runtime = WorktreeWatcher::new(list_projects, on_changed);
        runtime.start();
        // The watcher task stops itself via Drop/abort at process exit; keep
        // it detached like the JS module-scope runtime.
        std::mem::forget(runtime);
    }

    routes::routes().with_state(state)
}

#[cfg(test)]
mod tests;
