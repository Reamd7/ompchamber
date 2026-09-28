//! Port of `packages/web/server/lib/openchamber-sessions/` — the
//! OMPChamber session-orchestration routes (`routes.js`): session create
//! (optionally in a fresh worktree, with an initial prompt and goal),
//! `send` to an existing session, and `fork`-then-send, all dispatched
//! through the local engine client.
//!
//! JS source map:
//! - `routes.js` (service + route registration) → `service.rs` + `routes.rs`
//! - `opencode/local-engine-client.js` (consumed subset) → `client.rs`
//! - `opencode/project-directory-runtime.js` `validateDirectoryPath`
//!   (consumed seam) → the production `validate_directory` closure below
//! - `openchamber-control/error.js` (consumed subset) → `error.rs`
//! - `git/index.js` `createWorktree`/`getWorktreeBootstrapStatus` → the
//!   already-ported `crate::git_service::worktrees` behind `WorktreeOps`
//! - `session-goal/create.js` → already-ported `crate::session_goal::create`
//! - `scheduled-tasks/runtime.js` command helpers → already-ported
//!   `crate::scheduled_tasks::compute` / `snippets`
//!
//! Composition-root wiring (JS `server/index.js`): settings come from the
//! shared settings store, the engine client is HTTP-backed over the managed
//! engine, and `emitSessionCreatedEvent` broadcasts the
//! `ompchamber:session-created` frame on the shared hub (the JS emitter
//! forwards only the `{type, properties}` wire subset to connected SSE
//! clients).
//!
//! Known gaps (module-scoped):
//! - `sessionKnowledgeRuntime` is not ported yet; dispatched prompts carry
//!   no standing-project context (the JS behavior when the runtime is
//!   null), and `recordDelivered` is a no-op.
//!
//! 中文说明：本目录是 `packages/web/server/lib/openchamber-sessions/` 的
//! Rust 移植——OMPChamber 会话编排路由：创建会话（可选新 worktree、初始
//! prompt 与 goal）、向既有会话 `send`、`fork` 后 `send`，统一经本地
//! engine 客户端派发。本文件（mod.rs）承担组合根职责：装配生产依赖
//! （HTTP engine 客户端、settings 存储、目录校验、git worktree、goal
//! 创建器、SSE hub 事件广播）并挂载路由。

/// engine 客户端（SDK 约定 + raw fetch 帮助函数）及其 HTTP 实现。
mod client;
/// 控制错误类型与 wire 映射（`OMPChamberControlError` / `sendServiceError`）。
mod error;
/// 请求 payload 形状工具与 JS 真值规则。
mod payload;
/// axum 路由注册与请求体解析（对应 Express 路由层）。
mod routes;
/// selection 默认值推导与请求前校验。
mod selection;
/// 会话编排服务主体（`create` / `runExisting` 流程）。
mod service;

/// 模块级测试（JS routes 测试的移植）。
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::git_service::service::GitService;
use crate::git_service::worktrees as git_worktrees;
use crate::hub::EventHub;
use crate::session_goal::create::{
    CreateDeps, CreateGoalParams, WarningSink, create_session_goal, engine_patch_fetch,
};
use crate::settings::normalization::{
    normalize_directory_path, path_resolve, sanitize_projects as sanitize_projects_value,
};

use client::{BoxFut, EngineClient, HttpEngineClient};
use routes::SessionState;
use service::{
    EmitSessionCreated, GoalCall, GoalCreator, SanitizeProjects, SessionDeps, SessionService,
    ValidateDirectory, WaitReady, WorktreeOps,
};

/// JS `waitForOpenCodeReady(10_000, 250)`: no engine port yet → the JS
/// `'OpenCode port is not available'` throw; otherwise wait for readiness
/// and surface the JS timeout message.
///
/// 中文说明：engine 尚无端口时报 "OpenCode port is not available"；否则
/// 等待就绪，10 秒超时报 JS 同款超时消息。
async fn wait_for_opencode_ready(engine: &EngineState) -> Result<(), String> {
    if engine.base_url().is_none() {
        return Err("OpenCode port is not available".to_string());
    }
    engine
        .wait_ready(Duration::from_secs(10))
        .await
        .map_err(|_| "Timed out waiting for OpenCode to become ready".to_string())
}

/// `project-directory-runtime.js` `validateDirectoryPath`: trim → normalize
/// (quotes/~) → resolve → stat → canonicalize, with the JS error strings.
///
/// 中文说明：trim → 规范化（引号/`~` 展开）→ resolve → stat → canonicalize
/// 的目录校验链，错误字符串与 JS 保持一致（目录必填/不存在/无权限/非目录）。
async fn validate_directory_path(candidate: &str) -> Result<String, String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = path_resolve(&normalize_directory_path(trimmed));
    let metadata = match tokio::fs::metadata(&resolved).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => "Directory not found".to_string(),
                std::io::ErrorKind::PermissionDenied => "Access to directory denied".to_string(),
                _ => "Failed to validate directory".to_string(),
            });
        }
    };
    if !metadata.is_dir() {
        return Err("Specified path is not a directory".to_string());
    }
    match std::fs::canonicalize(&resolved) {
        Ok(canonical) => Ok(
            crate::settings::normalization::strip_verbatim_prefix(canonical)
                .to_string_lossy()
                .into_owned(),
        ),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

/// `git/index.js` `createWorktree` + `getWorktreeBootstrapStatus` over the
/// ported git service.
///
/// 中文说明：用已移植的 git service 实现 `WorktreeOps` 接缝——
/// `createWorktree` 与 `getWorktreeBootstrapStatus`。
struct GitWorktreeOps {
    /// 共享的 git 服务实例。
    service: Arc<GitService>,
}

/// 把接缝方法适配为 BoxFut 并转发给 git service。
impl WorktreeOps for GitWorktreeOps {
    /// 创建 worktree；克隆参数以便 move 进异步块。
    fn create(&self, directory: &str, input: &Value) -> BoxFut<'_, Result<Value, String>> {
        let service = Arc::clone(&self.service);
        let directory = directory.to_string();
        let input = input.clone();
        Box::pin(async move { git_worktrees::create_worktree(&service, &directory, &input).await })
    }

    /// 查询 worktree bootstrap 状态。
    fn bootstrap_status(&self, directory: &str) -> BoxFut<'_, Result<Value, String>> {
        let service = Arc::clone(&self.service);
        let directory = directory.to_string();
        Box::pin(
            async move { git_worktrees::get_worktree_bootstrap_status(&service, &directory).await },
        )
    }
}

/// `createSessionGoal` — objective file in `<data_dir>/goals` plus the
/// metadata PATCH through the managed engine (`create.rs` production path).
///
/// 中文说明：生产环境的 `createSessionGoal` 闭包——在 `<data_dir>/goals`
/// 写 objective 文件并经受管 engine PATCH 元数据（复用 `create.rs` 的生产
/// 路径）；警告走 tracing，engine 不可用时报 "engine unavailable"。
fn production_goal_creator(engine: Arc<EngineState>, data_dir: &Path) -> GoalCreator {
    let data_dir = data_dir.to_path_buf();
    Arc::new(move |call: GoalCall| {
        let engine = Arc::clone(&engine);
        let data_dir = data_dir.clone();
        Box::pin(async move {
            let raw_base = engine.base_url().unwrap_or_default();
            let base = raw_base.strip_suffix('/').unwrap_or(&raw_base).to_string();
            if base.is_empty() {
                return Err("engine unavailable".to_string());
            }
            let warn: WarningSink = Arc::new(|message: &str, error: &str| {
                tracing::warn!("[OMPChamberSessions] {message}: {error}")
            });
            let params = CreateGoalParams {
                session_id: call.session_id,
                directory: call.directory,
                objective: call.objective,
                token_budget: call.token_budget,
                provider_id: Some(call.provider_id),
                model_id: Some(call.model_id),
            };
            let deps = CreateDeps {
                patch: engine_patch_fetch(Arc::clone(&engine)),
                distiller: None,
                warn: Some(warn),
            };
            create_session_goal(&base, &data_dir, params, deps)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    })
}

/// `emitSessionCreatedEvent` (server/index.js): forward the wire subset to
/// every connected OMPChamber SSE client as `ompchamber:session-created`.
///
/// 中文说明：对应 server/index.js 的 `emitSessionCreatedEvent`——只转发
/// wire 子集（sessionId/directory/createdAt/promptDispatched/
/// dispatchedAsCommand，projectID 与 title 按 JS 真值过滤），以
/// `ompchamber:session-created` 帧广播给所有连接中的 OMPChamber SSE 客户端。
fn emit_session_created_via_hub(hub: Arc<EventHub>) -> EmitSessionCreated {
    Arc::new(move |event: &Value| {
        let mut properties = Map::new();
        properties.insert(
            "sessionId".to_string(),
            event.get("sessionID").cloned().unwrap_or(Value::Null),
        );
        properties.insert(
            "directory".to_string(),
            event.get("directory").cloned().unwrap_or(Value::Null),
        );
        properties.insert(
            "createdAt".to_string(),
            event.get("createdAt").cloned().unwrap_or(Value::Null),
        );
        properties.insert(
            "promptDispatched".to_string(),
            Value::Bool(event.get("promptDispatched") == Some(&Value::Bool(true))),
        );
        properties.insert(
            "dispatchedAsCommand".to_string(),
            Value::Bool(event.get("dispatchedAsCommand") == Some(&Value::Bool(true))),
        );
        if let Some(project_id) = event
            .get("projectID")
            .filter(|value| payload::is_truthy(Some(value)))
        {
            properties.insert("projectId".to_string(), project_id.clone());
        }
        if let Some(title) = event
            .get("title")
            .filter(|value| payload::is_truthy(Some(value)))
        {
            properties.insert("title".to_string(), title.clone());
        }
        hub.publish_json(
            "ompchamber:session-created",
            &json!({
                "type": "ompchamber:session-created",
                "properties": properties,
            }),
        );
    })
}

/// 组合根：装配全部生产依赖并构造 SessionService，挂载三条路由；对所有
/// 路由施加 1MB 请求体上限（对应 express.json 的 limit: '1mb'）。
pub fn router(ctx: RouterContext) -> axum::Router {
    let engine = Arc::clone(&ctx.engine);
    let settings_store = crate::settings::store(&ctx);
    let goal_data_dir = ctx.config.data_dir.clone();

    let deps = SessionDeps {
        client: Arc::new(HttpEngineClient::new(Arc::clone(&engine))) as Arc<dyn EngineClient>,
        read_settings: Arc::new(move || {
            let store = Arc::clone(&settings_store);
            Box::pin(async move { store.read_raw().await }) as BoxFut<'static, Map<String, Value>>
        }),
        sanitize_projects: Arc::new(|projects: &Value| {
            sanitize_projects_value(Some(projects)).unwrap_or_default()
        }) as SanitizeProjects,
        validate_directory: Arc::new(|directory: &str| {
            let directory = directory.to_string();
            Box::pin(async move { validate_directory_path(&directory).await })
                as BoxFut<'static, Result<String, String>>
        }) as ValidateDirectory,
        wait_ready: Arc::new(move || {
            let engine = Arc::clone(&engine);
            Box::pin(async move { wait_for_opencode_ready(&engine).await })
                as BoxFut<'static, Result<(), String>>
        }) as WaitReady,
        worktrees: Arc::new(GitWorktreeOps {
            service: Arc::new(GitService::new()),
        }),
        create_goal: production_goal_creator(Arc::clone(&ctx.engine), &goal_data_dir),
        emit_session_created: emit_session_created_via_hub(Arc::clone(&ctx.hub)),
    };
    let service = Arc::new(SessionService::new(deps));
    routes::routes()
        .with_state(SessionState { service })
        // express.json({ limit: '1mb' }) on every route of this family.
        .layer(axum::extract::DefaultBodyLimit::max(1_048_576))
}
