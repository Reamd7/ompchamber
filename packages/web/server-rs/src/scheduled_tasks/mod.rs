//! Port of `packages/web/server/lib/scheduled-tasks/` — runtime.js, loops.js,
//! snippets/service/routes surfaces — OpenChamber-owned scheduled task
//! scheduling, markdown loop discovery, CRUD/run routes, and the
//! `ompchamber` SSE event stream.
//!
//! `pub mod cron` is crate-visible: `projects::project_config` reuses it for
//! `schedule.cron` validation. Persistence goes through the ported
//! `projects` project-config runtime; engine calls go through `ctx.engine`.

pub mod compute;
pub mod cron;
pub mod dispatch;
pub mod loops;
pub mod md;
pub mod runtime;
pub mod service;
pub mod snippets;
pub mod timeutil;

#[cfg(test)]
pub mod testing;

pub mod routes;

use std::sync::Arc;

use crate::context::RouterContext;

pub use service::ScheduledTaskService;

use crate::permission_auto_accept::{FileSettingsAccess, PermissionAutoAccept};
use crate::projects::ProjectConfigRuntime;

use dispatch::HttpEngineDispatch;
use routes::SseClients;
use runtime::{EmitTaskRunEvent, RuntimeDeps, ScheduledTasksRuntime, SharedRuntime, TaskRunEvent};
use service::SettingsProjects;

/// `emitTaskRunEvent` wiring: every connected `/api/ompchamber/events` client
/// receives `ompchamber:scheduled-task-ran` frames (server/index.js).
fn task_run_event_publisher(clients: Arc<SseClients>) -> EmitTaskRunEvent {
    Arc::new(move |event: &TaskRunEvent| {
        clients.publish_task_run_event(event);
    })
}

/// Build the shared runtime stack (used by the router and tests).
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
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(ctx)
}
