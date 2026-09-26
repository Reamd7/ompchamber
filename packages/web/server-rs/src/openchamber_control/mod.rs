//! Port of `server/lib/openchamber-control/` — the typed control contract
//! shared by the OpenChamber CLI HTTP adapter and the managed `openchamber`
//! tool: the fixed action allowlist (`actions`), the orchestration service
//! (`service`), the screenshot writer (`screenshots`), the error envelope
//! (`error`), and the authenticated CLI route (`routes`).
//!
//! Boundaries (DOCUMENTATION.md): `service` validates and executes the
//! fixed action allowlist; `actions` marks CLI-only actions with
//! `agent_exposed: false` (currently `schedule.status`) and the
//! agent-tool port consumes the filtered registry; `routes` is a thin
//! authenticated CLI adapter that forwards one action, preserves service
//! status and partial-result details. Session/schedule/memory/browser
//! domain operations are composed in through seams and are owned by their
//! own modules.

pub mod actions;
pub mod engine_client;
pub mod error;
pub mod routes;
pub mod screenshots;
pub mod service;

pub use error::ControlError;
pub use service::{
    AgentMemoryActions, BrowserControl, ControlDeps, ControlService, ScheduleService,
    SessionService,
};

use std::sync::Arc;

use crate::context::RouterContext;

/// Module router: registers `POST /api/ompchamber/control` over a control
/// service built from this context. The session, browser, and memory seams
/// start absent (their ports are pending) and their actions answer 503.
pub fn router(ctx: RouterContext) -> axum::Router {
    let clients = crate::scheduled_tasks::routes::SseClients::new();
    let (_runtime, scheduled) = crate::scheduled_tasks::build_runtime(&ctx, clients);
    let service = service_for_context(&ctx, scheduled);
    routes::router_shared(Arc::new(service))
}

/// Composition for main.rs once the sibling ports land: build the service
/// over the SHARED scheduled-task service the scheduled-task routes serve
/// (index.js wires one stack, not two) and wire the session/browser/memory
/// seams on the returned [`ControlDeps`]-backed service.
pub fn service_for_context(
    ctx: &RouterContext,
    scheduled: Arc<crate::scheduled_tasks::service::ScheduledTaskService>,
) -> ControlService {
    let projects = service::settings_projects(&ctx.config.data_dir);
    let schedule_service = Arc::new(service::PortedScheduleService::new(
        Arc::clone(&scheduled),
        Arc::clone(&projects) as Arc<dyn crate::scheduled_tasks::runtime::ProjectsAccess>,
    ));
    let deps = ControlDeps::for_engine(
        Arc::clone(&ctx.engine),
        crate::settings::store(ctx),
        projects,
        schedule_service,
    );
    ControlService::new(deps)
}
