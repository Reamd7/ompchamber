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
//!
//! 中文说明：本模块移植 server/lib/projects/（project-config.js、
//! project-id.js 等），不注册任何 HTTP 路由，而是作为库被组合：
//! scheduled-tasks 与 settings 端口驱动任务 CRUD，agent-memory 和
//! opencode 路由用 createProjectIdFromPath 派生 id，worktree 归一化来自
//! opencode/shared.js 与 git/service.js。读取项目配置时沿用全服务的
//! JSONC 宽容解析；真正损坏的文件作为独立错误上报，绝不静默当成空
//! 配置而被下次写入冲掉。

/// 类型化任务模型与各规整器（project-config.js 帮助层）。
pub mod model;
/// 目录路径 → 稳定项目 id 的派生（project-id.js）。
pub mod project_id;
/// 项目配置运行时：带文件锁的任务 CRUD 与 loop 对账。
pub mod runtime;
/// worktree 根目录归一化（shared.js 与 git/service.js 的相关函数）。
pub mod worktree;

pub use model::{Execution, LoopEntry, Schedule, ScheduledTask, TaskState};
pub use project_id::create_project_id_from_path;
pub use runtime::{DeleteResult, ProjectConfigRuntime, StateUpdateResult, UpsertResult};
pub use worktree::{
    GitCommandResult, GitRunner, PrimaryWorktreeRoot, default_git_runner,
    derive_primary_worktree_root_from_git_dir, find_worktree_root, resolve_primary_worktree_root,
};

/// projects 模块的单元测试。
#[cfg(test)]
mod tests;

use crate::context::RouterContext;

/// No routes: see the module docs. Kept for the module router contract.
/// 中文：本模块不注册任何路由（见模块文档），保留函数以符合模块
/// router 的统一约定。
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
