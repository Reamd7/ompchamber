//! Port of the OpenCode plugin/skill/snippet surfaces from
//! `server/lib/opencode/`:
//!
//! - `plugins.js` → [`plugins`] — plugin config data layer (JSONC layers,
//!   base64url ids, plugin dir files).
//! - `plugin-spec.js` → [`plugin_spec`] — npm/path spec parsing.
//! - `plugin-routes.js` → [`plugin_routes`] — `/api/config/plugins*` routes
//!   + the npm registry status endpoint (`npm-registry.js` →
//!   [`npm_registry`]).
//! - `skills.js` → [`skills`] — skill discovery/CRUD/rename/supporting files
//!   (with the `shared.js` subset they use).
//! - `skill-routes.js` → [`skill_routes`] — `/api/config/skills*` routes,
//!   engine `GET /skill` passthrough merge, catalog scan/install delegation
//!   to `crate::skills_catalog`.
//! - `snippets.js` → [`snippets`] — the full snippet library.
//!
//! Shared plumbing (Express-style query parsing, optional-directory
//! resolution, deferred-restart responses) lives in [`http_util`]; markdown
//! frontmatter IO in [`md_yaml`].

pub(crate) mod config_layers;
pub(crate) mod http_util;
pub(crate) mod md_yaml;
pub(crate) mod npm_registry;
pub(crate) mod plugin_routes;
pub(crate) mod plugin_spec;
pub(crate) mod plugins;
pub(crate) mod skill_routes;
pub(crate) mod skills;
pub(crate) mod snippets;

use crate::context::RouterContext;

/// `registerPluginRoutes` + `registerSkillRoutes` composed.
pub fn router(ctx: RouterContext) -> axum::Router {
    plugin_routes::routes(ctx.clone()).merge(skill_routes::routes(ctx))
}
