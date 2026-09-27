//! Port of `server/lib/agent-memory/` — the agent memory store.
//!
//! JS file → Rust file:
//! - `runtime.js` → [`runtime`] (store: two scopes, restatement-replacement,
//!   flagged-entry retention, per-key write locks, atomic writes)
//! - `actions.js` → [`actions`] (the `memory.*` dispatch the
//!   `ompchamber_memory` tool calls; consumed by the openchamber-control port)
//! - `routes.js` → [`routes`] (the panel's read/correct/delete surface)
//! - `feature-flag.js` → [`feature_flag`] (`OMPCHAMBER_MEMORY_ENABLE`)
//! - `project-resolution.js` → [`project_resolution`] (session directory →
//!   project via worktree→project mapping)
//! - `threat-patterns.js` → [`threat_patterns`] (hand-rolled matchers; the
//!   allowed crate set has no `regex`)
//!
//! Wiring parity with `server/index.js`: the routes and the actions share one
//! `is_agent_memory_enabled` gate (feature flag first — unreleased means
//! absent, so no stored setting can bring it back — then the
//! `agentMemoryToolEnabled` setting), the resolver maps worktree sessions
//! onto the project's store, and writes are announced over the SSE hub as
//! `ompchamber:agent-memory-changed`. Like the rest of the Rust port, the
//! JS `~/.config/ompchamber` user-config root is the resolved
//! `ServerConfig::data_dir` (`OMPCHAMBER_DATA_DIR` or the same default).
//!
//! 中文说明：agent memory（agent 记忆库）模块入口，聚合存储运行时、
//! 动作分发、面板路由、feature flag、项目归属解析与威胁模式筛查，
//! 并提供与 `server/index.js` 对齐的生产装配：`router` 与
//! `actions_for_context` 等工厂共享同一 runtime、同一启用闸门与同一
//! SSE 广播通道。

/// 动作分发层：`ompchamber_memory` 工具调用的 `memory.*` 命令入口，
/// 由 openchamber-control 移植层消费。
pub mod actions;
/// feature flag：`OMPCHAMBER_MEMORY_ENABLE` 决定本功能是否随构建发布。
pub mod feature_flag;
/// 项目归属解析：把会话目录（含 worktree）映射到项目记忆键。
pub mod project_resolution;
/// 面板路由：记忆的读取、更正与删除 HTTP 接口（创建走工具，无创建路由）。
pub mod routes;
/// 存储运行时：双 scope 存储、复述替换（restatement-replacement）、
/// 可疑条目保留、按 key 写锁与原子写入。
pub mod runtime;
/// 威胁模式筛查：手写的注入模式匹配器（可用 crate 集合无 `regex`）。
pub mod threat_patterns;

pub use actions::{AgentMemoryActions, MemoryActionError, OnMemoryChanged, ResolveProjectId};
pub use feature_flag::is_agent_memory_feature_available;
pub use project_resolution::{ListProjectPaths, MemoryProjectResolver, ResolvePrimaryWorktreeRoot};
pub use runtime::{
    AgentMemoryRuntime, AllMemory, CreateInput, CreateResult, IdFactory, MemoryEnabledGate,
    MemoryEntry, MemoryError, MemoryFile, RemoveResult, Scope, Target, UpdatePatch, UpdateResult,
};
pub use threat_patterns::{find_threat_pattern, looks_like_injection};

use std::sync::Arc;

use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::hub::EventHub;

/// 装配 agent memory 的 HTTP 路由：构造共享 runtime，并把
/// [`is_agent_memory_enabled`] 包装成闸门传入 [`routes::routes`]，
/// 使路由层与工具层共享同一启用判定。
pub fn router(ctx: RouterContext) -> axum::Router {
    let runtime = runtime_for_context(&ctx);
    let gate_ctx = ctx.clone();
    let gate: MemoryEnabledGate = Arc::new(move || {
        let gate_ctx = gate_ctx.clone();
        Box::pin(async move { Ok(is_agent_memory_enabled(&gate_ctx).await) })
    });
    routes::routes(runtime, Some(gate))
}

/// `createAgentMemoryRuntime({ projectsDirPath, userConfigRoot })`.
/// 以生产配置构造共享的 [`AgentMemoryRuntime`]：用户配置根取
/// `ServerConfig::data_dir`，项目记忆存放于其下 `projects` 子目录；
/// 第三参 `None` 表示使用默认 id 工厂。
pub fn runtime_for_context(ctx: &RouterContext) -> Arc<AgentMemoryRuntime> {
    Arc::new(AgentMemoryRuntime::new(
        ctx.config.data_dir.clone(),
        ctx.config.data_dir.join("projects"),
        None,
    ))
}

/// One switch for everything memory-related (`isAgentMemoryEnabled` in
/// index.js): it gates the tool, the routes, and the session index alike, so
/// turning memory off leaves nothing behind that still reads or writes the
/// store. Never fails: an unreadable settings file reads as off.
/// agent memory 的统一总开关（index.js 的 `isAgentMemoryEnabled`）：
/// feature flag 优先（未发布即不存在，任何已存储设置都无法唤回），
/// 再看设置中的 `agentMemoryToolEnabled`（必须显式为 true）。
/// 工具、路由与会话索引共用它；设置不可读按关闭处理，因此永不失败。
pub async fn is_agent_memory_enabled(ctx: &RouterContext) -> bool {
    // The feature gate comes first: unreleased means absent, not merely
    // switched off, so no stored setting can bring it back.
    if !feature_flag::is_agent_memory_feature_available() {
        return false;
    }
    let settings = crate::settings::store(ctx)
        .read_migrated()
        .await
        .unwrap_or_default();
    settings
        .get("agentMemoryToolEnabled")
        .and_then(Value::as_bool)
        == Some(true)
}

/// `createMemoryProjectResolver({ listProjectPaths, resolvePrimaryWorktreeRoot,
/// managedProjectRoots: [<config root>/chats] })`.
/// 构造生产用的 [`MemoryProjectResolver`]：项目清单来自迁移后的设置
/// （读取失败按空清单处理），git 解析用 `crate::projects` 的默认
/// runner，托管根为 `<data_dir>/chats`（托管会话共享该根的项目存储）。
pub fn project_resolver(ctx: &RouterContext) -> MemoryProjectResolver {
    let store = crate::settings::store(ctx);
    let list_project_paths: ListProjectPaths = Arc::new(move || {
        let store = store.clone();
        Box::pin(async move {
            // JS `readSettingsFromDiskMigrated().catch(() => null)` — an
            // unreadable list reads as no configured projects, and the
            // git-derived root below still converges worktrees.
            let settings = store.read_migrated().await.unwrap_or_default();
            let paths = crate::settings::normalization::sanitize_projects(settings.get("projects"))
                .unwrap_or_default()
                .iter()
                .filter_map(|project| {
                    project
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>();
            Ok(paths)
        })
    });

    let git = crate::projects::default_git_runner();
    let resolve_primary_worktree_root: ResolvePrimaryWorktreeRoot =
        Arc::new(move |directory: String| {
            let git = git.clone();
            Box::pin(async move {
                Some(
                    crate::projects::resolve_primary_worktree_root(&directory, &git)
                        .await
                        .root,
                )
            })
        });

    MemoryProjectResolver::new(
        list_project_paths,
        resolve_primary_worktree_root,
        vec![
            ctx.config
                .data_dir
                .join("chats")
                .to_string_lossy()
                .into_owned(),
        ],
    )
}

/// `emitAgentMemoryChangedEvent`: tells open panels that the agent changed
/// what it remembers, so what it just stored is visible without reopening
/// anything. Mirrors the SSE envelope the other hub broadcasters use.
/// 构造记忆变更回调（JS `emitAgentMemoryChangedEvent`）：把事件包成
/// `ompchamber:agent-memory-changed` 的 SSE 信封发布到 hub，让已打开的
/// 面板无需重开即可看到 agent 刚写入的记忆。
pub fn hub_announcer(hub: Arc<EventHub>) -> OnMemoryChanged {
    Arc::new(move |event: &Value| {
        hub.publish_json(
            "ompchamber:agent-memory-changed",
            &json!({
                "type": "ompchamber:agent-memory-changed",
                "properties": event,
            }),
        );
    })
}

/// `createAgentMemoryActions({...})` with the production wiring: the shared
/// runtime, the project resolver, the shared settings gate, and the hub
/// announcer.
/// 以生产装配构造 [`AgentMemoryActions`]（JS `createAgentMemoryActions`）：
/// 共享 runtime、项目解析器、共享设置闸门与 hub 广播回调。
pub fn actions_for_context(ctx: &RouterContext) -> AgentMemoryActions {
    let gate_ctx = ctx.clone();
    let gate: MemoryEnabledGate = Arc::new(move || {
        let gate_ctx = gate_ctx.clone();
        Box::pin(async move { Ok(is_agent_memory_enabled(&gate_ctx).await) })
    });
    AgentMemoryActions::with_resolver(
        runtime_for_context(ctx),
        project_resolver(ctx),
        Some(gate),
        Some(hub_announcer(ctx.hub.clone())),
    )
}
