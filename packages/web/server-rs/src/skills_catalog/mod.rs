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
//!
//! 中文说明：`server/lib/skills-catalog/` 的 Rust 移植总入口：技能目录
//! 扫描、安装、缓存、预置源与 GitHub 元数据补全。JS 模块自身不注册
//! 路由——其函数由技能路由（`server/lib/opencode/skill-routes.js`，
//! 移植于 `opencode_plugins` 模块）消费，故 [`router`] 返回空路由器，
//! 本模块以纯库 API 形式对外暴露；子模块与 JS 文件一一对应（见上方
//! 文件映射表）。

use crate::context::RouterContext;

/// 扫描结果缓存（cache.js）：内存 TTL 缓存、per-key 在途去重、防抖
/// 磁盘持久化与全局并发上限。
pub(crate) mod cache;
/// 预置技能源清单（curated-sources.js）及其读取入口。
pub(crate) mod curated_sources;
/// JSON 缓存持久化（disk-cache.js）：数据目录下临时文件原子改名写入，
/// 权限位 0600。
pub(crate) mod disk_cache;
/// 与 JS 错误载荷逐字段对齐的错误类型（Rust 侧独有）：kind 字段供路由
/// 层映射 authRequired/conflicts/invalidSource 为 401/409/400。
pub(crate) mod error;
/// 非交互 git 执行封装（git.js）：`GitRunner` trait 便于测试注入假实现，
/// 真实 runner 对齐 Node `execFile` 语义。
pub(crate) mod git;
/// GitHub 仓库元数据补全（github-meta.js）：尽力获取 star 数与最近推送，
/// 内存 + 磁盘双级缓存，失败静默解析为 null。
pub(crate) mod github_meta;
/// 技能安装（install.js）：浅克隆 + sparse checkout、逐技能冲突决策与
/// 无符号链接、带遍历防护的目录复制。
pub(crate) mod install;
/// 技能仓库扫描（scan.js）：列举 SKILL.md、解析 YAML frontmatter 并
/// 产出目录条目（根级 SKILL.md 按约定忽略）。
pub(crate) mod scan;
/// 仓库源字符串解析（source.js）：HTTPS/SSH URL 与 `owner/repo` 简写
/// 解析为克隆 URL 与归一化标识。
pub(crate) mod source;
/// 测试共享辅助：唯一临时目录、环境变量守卫、全局串行锁、假 git
/// runner 与仓库夹具。
#[cfg(test)]
mod test_support;

/// 再导出缓存 API：缓存键计算、读写/清除与带缓存的扫描入口。
pub use cache::{clear_cache, get_cache_key, get_cached_scan, scan_with_cache, set_cached_scan};
/// 再导出预置源条目类型与清单读取入口。
pub use curated_sources::{CuratedSource, get_curated_skills_sources};
/// 再导出错误类型：路由层据 kind 完成错误码映射。
pub use error::{CatalogError, ConflictEntry};
/// 再导出 git 执行封装：runner trait、选项/结果/身份类型、真实实现与
/// 鉴权错误识别。
pub use git::{
    GitIdentity, GitResult, GitRunOptions, GitRunner, RealGitRunner, looks_like_auth_error, run_git,
};
/// 再导出 GitHub 元数据补全 API：可注入 transport、批量拉取与缓存清理。
pub use github_meta::{
    MetaTransport, RepoMeta, ReqwestMetaTransport, clear_github_meta_cache,
    fetch_github_repo_metas, fetch_github_repo_metas_with,
};
/// 再导出安装 API：安装参数/选择/结果类型与安装入口。
pub use install::{
    InstallParams, InstallResult, InstallSelection, InstalledSkill, SkippedSkill,
    install_skills_from_repository,
};
/// 再导出扫描 API：扫描参数/结果、目录条目类型与扫描入口。
pub use scan::{ScanParams, ScanResult, SkillCatalogItem, scan_skills_repository};
/// 再导出源解析 API：解析结果类型与解析入口。
pub use source::{ParsedSource, SourceParseResult, parse_skill_repo_source};

/// The JS module wires no routes; see module docs.
/// 中文：本模块不接任何路由（见模块文档），返回空路由器仅为保持与
/// 其它模块一致的组合接口。
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
