//! Port of `server/lib/skills-catalog/`: skills catalog scanning,
//! installation, caching, curated sources, and GitHub metadata enrichment.
//!
//! The JS module registers no routes of its own — its functions are consumed
//! by the skill routes (`server/lib/opencode/skill-routes.js`, ported in the
//! `opencode_plugins` module), so `router` returns an empty router and this
//! module exposes its library API instead.
//!
//! File map: `curated-sources.js` → `curated_sources.rs`, `cache.js` →
//! `cache.rs`, `disk-cache.js` → `disk_cache.rs`, `git.js` → `git.rs`,
//! `github-meta.js` → `github_meta.rs`, `install.js` → `install.rs`,
//! `scan.js` → `scan.rs`, `source.js` → `source.rs`.

use crate::context::RouterContext;

pub(crate) mod cache;
pub(crate) mod curated_sources;
pub(crate) mod disk_cache;
pub(crate) mod error;
pub(crate) mod git;
pub(crate) mod github_meta;
pub(crate) mod install;
pub(crate) mod scan;
pub(crate) mod source;
#[cfg(test)]
mod test_support;

pub use cache::{clear_cache, get_cache_key, get_cached_scan, scan_with_cache, set_cached_scan};
pub use curated_sources::{CuratedSource, get_curated_skills_sources};
pub use error::{CatalogError, ConflictEntry};
pub use git::{
    GitIdentity, GitResult, GitRunOptions, GitRunner, RealGitRunner, looks_like_auth_error, run_git,
};
pub use github_meta::{
    MetaTransport, RepoMeta, ReqwestMetaTransport, clear_github_meta_cache,
    fetch_github_repo_metas, fetch_github_repo_metas_with,
};
pub use install::{
    InstallParams, InstallResult, InstallSelection, InstalledSkill, SkippedSkill,
    install_skills_from_repository,
};
pub use scan::{ScanParams, ScanResult, SkillCatalogItem, scan_skills_repository};
pub use source::{ParsedSource, SourceParseResult, parse_skill_repo_source};

/// The JS module wires no routes; see module docs.
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
