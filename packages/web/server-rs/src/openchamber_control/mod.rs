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
//! （中文说明）本模块是 JS 版 `server/lib/openchamber-control/` 的 Rust
//! 移植，为 OpenChamber CLI HTTP 适配器与受管的 `openchamber` 工具提供
//! 类型化的控制契约：固定动作白名单（`actions`）、编排服务（`service`）、
//! 截图写入器（`screenshots`）、错误信封（`error`）与带鉴权的 CLI 路由
//! （`routes`）。

/// 固定动作白名单与按调用工具解析 action 名称的 resolver。
pub mod actions;
/// 控制服务消费的本地 OpenCode engine HTTP 客户端封装。
pub mod engine_client;
/// 控制平面统一错误信封与外来错误的收敛转换。
pub mod error;
/// `POST /api/ompchamber/control` 的薄适配路由。
pub mod routes;
/// 把浏览器截图写入项目目录并回报写入位置。
pub mod screenshots;
/// 校验并执行固定动作白名单的编排服务。
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
/// 模块路由入口：基于当前上下文构建控制服务并注册
/// `POST /api/ompchamber/control`。会话、浏览器、记忆三个 seam 初始缺位
/// （对应端口尚未移植），相关动作回答 503。
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
/// 供 main.rs 使用的组合入口：让控制服务复用 scheduled-task 路由所服务的
/// 同一个共享计划任务服务（JS 版 index.js 只装配一套栈，而非两套），并在
/// 返回的基于 `ControlDeps` 的服务上接好会话/浏览器/记忆 seam。
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
