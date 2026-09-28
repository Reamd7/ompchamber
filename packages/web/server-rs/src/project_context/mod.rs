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
//!
//! （中文说明）项目上下文模块，移植自 JS server 的 `server/lib/project-context/`
//! （`runtime.js` 与 `routes.js`）。集中管理三类项目知识——笔记（notes）、
//! 待办（todos）与计划文档（plans）——的持久化：runtime 负责
//! `<projects-dir>/<projectId>/` 目录下的 `context.json`（服务端唯一可写的
//! 文件）与 `plans/` 子目录中的 markdown 计划，并执行把旧版客户端配置
//! `<projectId>.json` 里的 `projectNotes`/`projectTodos`/`projectPlanFiles`
//! 键一次性迁移出来的逻辑。
//!
//! [`shared_runtime`]: crate::project_context::shared_runtime

/// HTTP 路由层：以 axum `Router` 挂载 `/api/project-context/*` 全部端点（移植自 `routes.js`）。
pub mod routes;
/// 存储层：notes/todos/plans 的读写、旧键一次性迁移与 plan markdown 解析（移植自 `runtime.js`）。
pub mod runtime;

/// runtime 与路由的单元测试，仅在 `cfg(test)` 下编译。
#[cfg(test)]
mod tests;

/// 统一再导出 runtime 的公开类型与 `parse_plan_markdown`，供路由及 session-knowledge 等兄弟模块直接从本模块导入。
pub use runtime::{
    DeleteOutcome, Note, NoteMutation, NoteOrigin, ParsedPlan, PlanCreateResult, PlanLink,
    PlanPinnedResult, PlanRead, PlanSaveResult, ProjectContext, ProjectContextRuntime, Todo,
    parse_plan_markdown,
};

use std::sync::Arc;

use crate::context::RouterContext;

/// The process-wide runtime for the server's projects directory (mirrors the
/// JS module-level `projectContextRuntime` singleton).
///
/// 按进程返回共享的 `ProjectContextRuntime` 单例（以 projects 目录为键的
/// 进程级注册表）；同一目录的多次调用拿到同一实例，保证 context 写入互不冲突。
pub fn shared_runtime(ctx: &RouterContext) -> Arc<ProjectContextRuntime> {
    runtime::shared(&ctx.config.data_dir.join("projects"))
}

/// 构建 project-context 路由：先经 [`shared_runtime`] 解析共享 runtime，再委托 [`routes::router`] 注册全部端点。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(shared_runtime(&ctx))
}
