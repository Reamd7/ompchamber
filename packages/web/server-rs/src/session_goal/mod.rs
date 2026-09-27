//! Port of `server/lib/session-goal/` — file-backed goal objectives
//! ([`objectives`]), server-side goal creation ([`create`]), the backend-driven
//! goal continuation loop ([`runtime`]), and the objective-file HTTP surface
//! ([`routes`]). See `server/lib/session-goal/DOCUMENTATION.md` for the
//! behavioral contract.
//!
//! Composition-root wiring notes (JS `server/index.js`):
//! - The runtime subscribes to the global SSE hub. `spawn_hub_bridge` wires
//!   the equivalent subscription against [`crate::hub::EventHub`] frames; the
//!   event-stream module owns the envelope, so the bridge parses
//!   `{payload, directory}` defensively. Direct feeders (tests, alternate
//!   wiring) call [`SessionGoalRuntime::process_payload`].
//! - `emitGoalNotification` is injected in JS with desktop + UI broadcast +
//!   web-push fanout; until the notifications module is ported the default
//!   notifier keeps the `notifyOnCompletion` gate and hub broadcast only.
//! - The small-model audit and objective distillation call the (not yet
//!   ported) small-model service; those seams report unavailability and the
//!   goal loop follows the documented degraded path.
//! （中文概览）本模块为 JS `server/lib/session-goal/` 的移植：objectives
//! 负责文件化目标存储，create 负责服务端目标创建，runtime 负责后端驱动的
//! goal 续跑循环，routes 暴露目标文件的 HTTP 接口。

/// 服务端目标创建（JS `create.js`）：objective 裁剪/蒸馏、目标文件先写、
/// 会话元数据 PATCH。
pub mod create;
/// 文件化目标存取（JS `objectives.js`）：按 session id 键控 goals 目录读写，
/// 带 5000 字符上限与 id 格式校验。
pub mod objectives;
/// 目标文件的 axum HTTP 路由层（JS `routes.js`）。
pub mod routes;
/// 后端驱动的 goal 续跑循环（JS `runtime.js`）：完成度审计、续跑提示生成、
/// 通知与 goal 状态机。
pub mod runtime;

/// 再导出 create 模块的公开接口，供 composition root 与测试使用。
pub use create::{
    CreateDeps, CreateGoalError, CreateGoalParams, DistillRequest, Distiller, PatchFetch,
    WarningSink, build_goal_intro_text, create_session_goal, engine_patch_fetch,
};
/// 再导出 objectives 模块的公开接口。
pub use objectives::{
    GOAL_OBJECTIVE_CHAR_LIMIT, ObjectiveError, delete_objective, goals_dir, is_valid_objective_key,
    read_objective, write_objective,
};
/// 再导出 runtime 模块的公开接口，供 composition root 与外部调用方使用。
pub use runtime::{
    AUDIT_FAIL_LIMIT, AuditError, AuditOutput, AuditRequest, AuditService, BLOCKED_STREAK_LIMIT,
    EngineRequestError, Fetch, GoalMetadata, GoalNotifier, IDLE_QUIET_MS, IsEnabled,
    KICKOFF_QUIET_MS, MAX_AUTO_TURNS, SessionGoalRuntime, SessionGoalRuntimeOptions,
    build_continuation_prompt, engine_fetch, hub_notifier, parse_goal_metadata,
    settings_is_enabled, unavailable_audit,
};

use std::sync::Arc;

use serde_json::Value;

use crate::context::RouterContext;
use crate::hub::EventHub;

/// `registerSessionGoalRoutes(app)` — the objective-file route family.
/// 组装目标文件路由族（PUT/GET/DELETE `/api/goals/objective/{sessionId}`）。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::routes(ctx.config.data_dir.clone())
}

/// Production runtime (engine fetch, settings gate, hub notifier, data-dir
/// objectives). Feed it through [`spawn_hub_bridge`] or direct
/// `process_payload` calls — mirrors `createSessionGoalRuntime(...)` +
/// `globalMessageStreamHub.subscribeEvent(...)` in index.js.
/// 构造生产环境的 goal runtime：注入 engine HTTP fetch、settings 开关、
/// hub 通知器与 data_dir 目标存储，节流与轮次上限取 runtime 模块常量。
/// 等价于 JS 侧 `createSessionGoalRuntime(...)` 的组装。
pub fn runtime(ctx: &RouterContext) -> Arc<SessionGoalRuntime> {
    SessionGoalRuntime::new(SessionGoalRuntimeOptions {
        fetch: engine_fetch(Arc::clone(&ctx.engine)),
        audit: unavailable_audit(),
        notifier: hub_notifier(Arc::clone(&ctx.hub), ctx.config.data_dir.clone()),
        is_enabled: settings_is_enabled(ctx.config.data_dir.clone()),
        data_dir: ctx.config.data_dir.clone(),
        idle_quiet_ms: runtime::IDLE_QUIET_MS,
        kickoff_quiet_ms: runtime::KICKOFF_QUIET_MS,
        max_auto_turns: runtime::MAX_AUTO_TURNS,
    })
}

/// Subscribe the runtime to hub frames. Frame data is parsed as the
/// event-stream envelope `{payload, directory}`; a `payload.payload` object
/// wins over the wrapper (index.js `raw?.payload` handling), and `global`
/// directories become an empty hint.
/// 将 goal runtime 桥接到全局 EventHub：逐帧解析事件 envelope 并转发给
/// [`SessionGoalRuntime::process_payload`]。hub 发送端全部关闭（recv 失败）
/// 即退出；无法解析或 payload 非对象时静默跳过该帧。
pub fn spawn_hub_bridge(
    runtime: Arc<SessionGoalRuntime>,
    hub: Arc<EventHub>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut receiver = hub.subscribe();
        loop {
            let Ok(event) = receiver.recv().await else {
                return;
            };
            let Ok(envelope) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let raw = envelope.get("payload").cloned().unwrap_or(Value::Null);
            let payload = raw
                .get("payload")
                .filter(|inner| inner.is_object())
                .cloned()
                .unwrap_or(raw);
            if !payload.is_object() {
                continue;
            }
            let directory = envelope
                .get("directory")
                .and_then(Value::as_str)
                .filter(|directory| !directory.is_empty() && *directory != "global")
                .unwrap_or("")
                .to_string();
            runtime.process_payload(&payload, &directory);
        }
    })
}
