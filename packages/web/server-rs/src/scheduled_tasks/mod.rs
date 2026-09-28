//! Port of `packages/web/server/lib/scheduled-tasks/` — runtime.js, loops.js,
//! snippets/service/routes surfaces — OpenChamber-owned scheduled task
//! scheduling, markdown loop discovery, CRUD/run routes, and the
//! `ompchamber` SSE event stream.
//!
//! `pub mod cron` is crate-visible: `projects::project_config` reuses it for
//! `schedule.cron` validation. Persistence goes through the ported
//! `projects` project-config runtime; engine calls go through `ctx.engine`.
//!
//! 本模块是 JS `packages/web/server/lib/scheduled-tasks/` 的 Rust 移植：
//! 调度运行期（runtime）、markdown loop 发现、任务 CRUD/run 路由，
//! 以及 `ompchamber` SSE 事件流。
//! 持久化经移植版 `projects` 项目配置运行期完成；引擎调用经 `ctx.engine`。

/// schedule 到下一次触发时刻的计算。
pub mod compute;
/// cron 表达式的解析与匹配；`projects::project_config` 也复用它校验 `schedule.cron`。
pub mod cron;
/// 计划任务运行期与引擎之间的调度边界（seam）及其 HTTP 实现。
pub mod dispatch;
/// markdown loop 文件的发现、解析与任务调和。
pub mod loops;
/// markdown+frontmatter 的极简读写器与用户级目录解析。
pub mod md;
/// 计划任务运行期：定时唤醒、并发限额与单次运行看门狗。
pub mod runtime;
/// 面向路由的任务服务层（CRUD 与手动 run）。
pub mod service;
/// prompt 中 `#hashtag` 片段（snippet）的展开。
pub mod snippets;
/// civil 日期运算与 IANA 时区偏移解析。
pub mod timeutil;

/// 各模块共享的测试替身（仅测试构建编译）。
#[cfg(test)]
pub mod testing;

/// HTTP 路由注册与 SSE 客户端集合。
pub mod routes;

use std::sync::Arc;

use crate::context::RouterContext;

/// 服务类型重导出，路由与上层直接从这里引用。
pub use service::ScheduledTaskService;

use crate::permission_auto_accept::{FileSettingsAccess, PermissionAutoAccept};
use crate::projects::ProjectConfigRuntime;

use dispatch::HttpEngineDispatch;
use routes::SseClients;
use runtime::{EmitTaskRunEvent, RuntimeDeps, ScheduledTasksRuntime, SharedRuntime, TaskRunEvent};
use service::SettingsProjects;

/// `emitTaskRunEvent` wiring: every connected `/api/ompchamber/events` client
/// receives `ompchamber:scheduled-task-ran` frames (server/index.js).
/// 把任务运行事件广播给每个已连接的 `/api/ompchamber/events` 客户端
///（对应 JS `emitTaskRunEvent` 的接线方式）。
fn task_run_event_publisher(clients: Arc<SseClients>) -> EmitTaskRunEvent {
    Arc::new(move |event: &TaskRunEvent| {
        clients.publish_task_run_event(event);
    })
}

/// Build the shared runtime stack (used by the router and tests).
/// 组装完整的共享运行期栈（store、projects、auto-accept、dispatch、
/// 时钟与并发限额），供 router 与测试复用。
pub fn build_runtime(
    ctx: &RouterContext,
    clients: Arc<SseClients>,
) -> (SharedRuntime, Arc<ScheduledTaskService>) {
    let store = Arc::new(ProjectConfigRuntime::for_context(ctx));
    let projects = Arc::new(SettingsProjects::new(&ctx.config.data_dir));
    let auto_accept = PermissionAutoAccept::new(
        Arc::clone(&ctx.engine),
        Arc::new(FileSettingsAccess::new(&ctx.config.data_dir)),
        Some(Arc::clone(&ctx.hub)),
    );
    let dispatch = Arc::new(HttpEngineDispatch::new(
        Arc::clone(&ctx.engine),
        ctx.config.data_dir.join("goals"),
        Some(auto_accept),
    ));
    let deps = RuntimeDeps {
        store: Arc::clone(&store) as Arc<dyn runtime::ScheduledTaskStore>,
        projects: Arc::clone(&projects) as Arc<dyn runtime::ProjectsAccess>,
        dispatch,
        emit_task_run_event: task_run_event_publisher(Arc::clone(&clients)),
        clock: runtime::system_clock(),
        max_global_concurrency: runtime::DEFAULT_GLOBAL_CONCURRENCY,
        max_project_concurrency: runtime::DEFAULT_PROJECT_CONCURRENCY,
        max_run_duration_ms: runtime::DEFAULT_MAX_RUN_MS,
    };
    let scheduled_runtime = ScheduledTasksRuntime::new(deps);
    let service = ScheduledTaskService::new(projects, store, Arc::clone(&scheduled_runtime));
    (scheduled_runtime, service)
}

/// Module router: registers the scheduled-task routes. The scheduler is NOT
/// auto-started here — `main.rs` owns the boot sequence (`start()` after the
/// engine is up), mirroring server/index.js.
/// 注册计划任务路由。调度器不在此自动启动——启动时序归 `main.rs` 所有
///（引擎就绪后再 start()，对齐 server/index.js）。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(ctx)
}
