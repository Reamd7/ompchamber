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
//!
//! 中文概述：OpenCode 插件/技能/代码片段相关表面的聚合模块——按原 JS
//! 模块边界拆分为数据层、spec 解析、registry 客户端与 axum 路由层，
//! 共享的查询解析/目录解析/延迟重启辅助集中在 http_util 与 md_yaml。

/// JSONC 配置分层（custom/project/user）的读取、合并与安全写回。
pub(crate) mod config_layers;
/// Express 风格查询解析、可选目录解析、延迟重启响应等共享 HTTP 辅助。
pub(crate) mod http_util;
/// markdown frontmatter（YAML 子集）的读写辅助。
pub(crate) mod md_yaml;
/// npm registry 元数据客户端（TTL 缓存 + 在途去重）。
pub(crate) mod npm_registry;
/// `/api/config/plugins*` 路由与 npm registry 状态端点。
pub(crate) mod plugin_routes;
/// npm/path 插件描述符解析（纯函数）。
pub(crate) mod plugin_spec;
/// 插件配置数据层：JSONC 分层、base64url id 与插件目录文件。
pub(crate) mod plugins;
/// `/api/config/skills*` 路由与 skill 目录扫描/安装委派。
pub(crate) mod skill_routes;
/// skill 的发现、CRUD、重命名与配套文件管理。
pub(crate) mod skills;
/// 代码片段（snippet）库的完整实现。
pub(crate) mod snippets;

use crate::context::RouterContext;

/// `registerPluginRoutes` + `registerSkillRoutes` composed.
///
/// 中文：把插件路由与技能路由合并为一个 axum Router，供 server 顶层统一挂载。
pub fn router(ctx: RouterContext) -> axum::Router {
    plugin_routes::routes(ctx.clone()).merge(skill_routes::routes(ctx))
}
