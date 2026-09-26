//! Port of `server/lib/fs/routes.js` + `search.js` — the `/api/fs/*` route
//! family (directory listing, file read/write/upload/delete/rename, reveal,
//! exec jobs, git-dir discovery) plus the fuzzy filesystem search runtime
//! used by other modules (project icon discovery).
//!
//! JS source map:
//! - `server/lib/fs/routes.js`  → `handlers.rs` (+ `workspace.rs`, `exec.rs`,
//!   `git_dirs.rs` for the helpers the file defines at module scope)
//! - `server/lib/fs/search.js`  → `search.rs`
//!
//! Composition-root dependencies the JS receives from `index.js` are ported
//! inline and documented at each use: `normalizeDirectoryPath`
//! (settings-normalization-runtime.js), `resolveProjectDirectory`
//! (opencode/project-directory-runtime.js), `ompchamberUserConfigRoot`
//! (index.js `~/.config/ompchamber`), and `resolveGitBinaryForSpawn`
//! (plain `git` off win32). The login-shell PATH merge
//! (`buildAugmentedPath`) is not ported — exec/clone inherit the parent PATH.

mod exec;
mod git_dirs;
pub mod handlers;
mod paths;
pub mod search;
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

pub use search::{SearchHit, SearchOptions, search_filesystem_files};
pub use workspace::OutsideGrantStore;

use crate::context::RouterContext;
use axum::Router;

/// Realpath memoization — port of path-realpath-cache.js as used by the
/// `/api/fs/list` route (success TTL 10 min, failure TTL 60 s, 256 entries).
#[derive(Default)]
pub struct RealpathCache {
    entries: Mutex<HashMap<String, (u64, Result<PathBuf, std::io::ErrorKind>)>>,
}

const REALPATH_SUCCESS_TTL_MS: u64 = 600_000;
const REALPATH_FAILURE_TTL_MS: u64 = 60_000;
const REALPATH_MAX_ENTRIES: usize = 256;

impl RealpathCache {
    pub fn resolve(&self, value: &Path) -> std::io::Result<PathBuf> {
        let key = value.to_string_lossy().into_owned();
        let now = exec::now_ms();
        {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((expires_at, cached)) = entries.get(&key)
                && now < *expires_at
            {
                return cached.clone().map_err(std::io::Error::from);
            }
        }
        let result = paths::realpath(value);
        let ttl = if result.is_ok() {
            REALPATH_SUCCESS_TTL_MS
        } else {
            REALPATH_FAILURE_TTL_MS
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Cap the cache; the JS evicts oldest-inserted first, the map here is
        // unordered — the bound is what matters.
        if entries.len() >= REALPATH_MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            key,
            (
                now + ttl,
                result.as_ref().map(|p| p.clone()).map_err(|e| e.kind()),
            ),
        );
        result
    }
}

pub struct FsInner {
    pub grants: OutsideGrantStore,
    pub exec_jobs: exec::ExecJobStore,
    pub git_read_cache: Arc<exec::GitReadCache>,
    pub realpath_cache: RealpathCache,
    pub command_timeout_ms: u64,
    pub git_check_ignore_timeout_ms: u64,
}

/// Module-local state owned by the returned router (`Router::with_state`).
#[derive(Clone)]
pub struct FsState {
    inner: Arc<FsInner>,
}

impl std::ops::Deref for FsState {
    type Target = FsInner;
    fn deref(&self) -> &FsInner {
        &self.inner
    }
}

impl Default for FsState {
    fn default() -> Self {
        Self::new()
    }
}

impl FsState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FsInner {
                grants: OutsideGrantStore::default(),
                exec_jobs: exec::ExecJobStore::default(),
                git_read_cache: Arc::new(exec::GitReadCache::new(exec::git_read_cache_ttl_ms())),
                realpath_cache: RealpathCache::default(),
                command_timeout_ms: exec::command_timeout_ms(),
                git_check_ignore_timeout_ms: exec::git_check_ignore_timeout_ms(),
            }),
        }
    }

    /// Access to the outside-file grant store for cross-module callers that
    /// mint grants after an explicit user file pick (JS:
    /// `mintOutsideFileGrant`).
    pub fn grants(&self) -> &OutsideGrantStore {
        &self.inner.grants
    }
}

pub fn router(ctx: RouterContext) -> Router {
    router_with(ctx, FsState::new())
}

/// Composition entry for main.rs: the caller owns the state so sibling
/// modules (markdown-image-grants) mint grants against the SAME store that
/// `/api/fs/raw` serves.
pub fn router_with(ctx: RouterContext, state: FsState) -> Router {
    let core = Router::new()
        .route("/api/fs/home", get(handlers::home))
        .route("/api/fs/stat", get(handlers::stat))
        .route("/api/fs/read", get(handlers::read))
        .route("/api/fs/raw", get(handlers::raw))
        .route("/api/fs/serve/{*path}", get(handlers::serve))
        .route("/api/fs/list", get(handlers::list))
        .route("/api/fs/git-dirs", get(handlers::git_dirs))
        .route("/api/fs/exec/{jobId}", get(handlers::exec_job))
        // JSON-body POST family rides the `express.json({ limit: '50mb' })`
        // body budget that registerCommonRequestMiddleware applies to
        // `/api/fs` paths.
        .route("/api/fs/mkdir", post(handlers::mkdir))
        .route("/api/fs/write", post(handlers::write))
        .route("/api/fs/delete", post(handlers::delete))
        .route("/api/fs/rename", post(handlers::rename))
        .route("/api/fs/reveal", post(handlers::reveal))
        .route("/api/fs/clone", post(handlers::clone))
        .route("/api/fs/exec", post(handlers::exec))
        .route_layer(DefaultBodyLimit::max(handlers::JSON_BODY_LIMIT));

    // The upload route streams an octet-stream body and enforces its own
    // byte budget (OMPCHAMBER_FS_UPLOAD_MAX_BYTES), so the shared extractor
    // limit must not truncate it first.
    let upload = Router::new()
        .route("/api/fs/upload", post(handlers::upload))
        .route_layer(DefaultBodyLimit::disable());

    core.merge(upload)
        // `/api` requests pass through `requireApiAuth` before any route
        // (core-routes.js `app.use('/api', requireApiAuth)`); the ported
        // gate is owned by the ui_auth module.
        .route_layer(crate::ui_auth::middleware(ctx))
        .with_state(state)
}

#[cfg(test)]
mod tests;
