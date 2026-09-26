//! Port of `server/lib/walkthrough/` — guided walkthroughs of a diff.
//!
//! Generates a guided, ordered reading path through a diff: the small model
//! groups related hunks into stops and chapters and explains each group, and
//! the UI renders those stops interleaved with the code they describe.
//! Generation is **always user-initiated** — it spends tokens, so a person has
//! to ask for it.
//!
//! JS source map:
//! - `walkthrough/hunks.js`          → `hunks.rs` (hunk parsing + ids)
//! - `walkthrough/generated.js`      → `generated.rs`
//! - `walkthrough/sources.js`        → `sources.rs` (+ `GitDeps` seam)
//! - `walkthrough/digest.js`         → `digest.rs`
//! - `walkthrough/prompt.js`         → `prompt.rs`
//! - `walkthrough/schema.js`         → `schema.rs`
//! - `walkthrough/store.js`          → `store.rs`
//! - `walkthrough/pull-request.js`   → `pull_request.rs`
//! - `walkthrough/model-settings.js` → `model_settings.rs`
//! - `walkthrough/languages.js`      → `languages.rs`
//! - `walkthrough/index.js`          → `service.rs` (jobs + orchestration)
//! - `walkthrough/routes.js`         → `routes.rs`
//! - SHA-1 (hunk ids must stay byte-identical to Node's `crypto`) → `sha1.rs`
//! - `walkthrough/small-model` dependency → `small_model.rs` seam, defaulting
//!   to Unavailable until that module's port lands (see `small_model.rs` for
//!   the wiring point)
//!
//! Hunk identity lives in `hunks.rs` alone: the client never recomputes ids.

pub mod digest;
pub mod error;
pub mod generated;
pub mod hunks;
pub mod languages;
pub mod model_settings;
pub mod prompt;
pub mod pull_request;
pub mod routes;
pub mod schema;
pub mod service;
pub mod sha1;
pub mod small_model;
pub mod sources;
pub mod store;

use std::sync::Arc;

use crate::context::RouterContext;

/// `registerWalkthroughRoutes(app, { getWalkthroughService })` — the
/// `/api/walkthrough*` family on axum.
pub fn router(ctx: RouterContext) -> axum::Router {
    let git = Arc::new(crate::git_service::service::GitService::new());
    let pr_differ =
        pull_request::pull_request_differ(ctx.config.data_dir.clone(), Arc::clone(&git));
    let service = service::WalkthroughService::new(
        ctx.config.data_dir.clone(),
        sources::GitDeps::real(git),
        small_model::unavailable_small_model(),
        Some(pr_differ.into_pr_diff_fn()),
    );

    // JS deferred the pointer prune off the module's first import; the router
    // is that moment here. Never awaited on the request path.
    service::spawn_housekeeping(Arc::new(store::Store::new(&ctx.config.data_dir)));

    routes::routes(service)
}

#[cfg(test)]
mod service_tests;
