//! Port of the `opencode/` config-CRUD surfaces from `packages/web/server`:
//!
//! JS source map:
//! - `opencode/routes.js` remainder      → `routes.rs` (behavior/AGENTS.md
//!   endpoints, provider config CRUD, MCP auth pending + OAuth callback)
//! - `opencode/config-entity-routes.js`  → `entity_routes.rs` (agent/command/
//!   MCP entity routes; the snippet routes stay with the snippets port)
//! - `opencode/agents.js`                → `agents.rs`
//! - `opencode/commands.js`              → `commands.rs`
//! - `opencode/mcp.js`                   → `mcp.rs`
//! - `opencode/providers.js`             → `providers.rs`
//! - `opencode/auth.js`                  → `auth.rs`
//! - `opencode/claude-cli-auth.js`       → `claude_cli.rs`
//! - `opencode/config-mutation-response.js` → `mutation.rs`
//! - `opencode/shared.js` subset         → `config_layers.rs` (config layer
//!   machinery) + `md_file.rs` (markdown/frontmatter) + `yaml.rs`
//!   (frontmatter YAML subset — no yaml crate is on the allow-list)
//! - `opencode/project-directory-runtime.js` → `project_directory.rs`
//!   (inline port; `fs_routes::workspace` keeps its own copy)

//!
//! 中文说明：本目录把旧 `packages/web/server` 中 `opencode/` 的配置
//! CRUD 接口按文件对应移植为 Rust（axum）实现，映射关系见上方英文
//! 注释。公共载体集中在：[`OpenCodeEnv`]（配置/数据目录环境）、
//! [`PendingMcpAuthState`]（按 state 挂起的 MCP OAuth 上下文）、
//! [`ModuleState`]（路由共享状态）；由 [`router_with_env`] 组装
//! provider 路由与实体路由并注入状态。
/// agent 配置的 markdown 读写端点（`opencode/agents.js` 移植）。
mod agents;
/// auth.json 凭据存取（`opencode/auth.js` 移植）。
mod auth;
/// Claude CLI 登录状态探测（`opencode/claude-cli-auth.js` 移植）。
mod claude_cli;
/// command 配置的 markdown 读写端点（`opencode/commands.js` 移植）。
mod commands;
/// JSONC 配置层读取/合并/定位机制（`opencode/shared.js` 子集移植）。
mod config_layers;
/// agent/command/MCP 实体路由（`opencode/config-entity-routes.js` 移植）。
mod entity_routes;
/// MCP 服务器配置 CRUD 与条目归一化（`opencode/mcp.js` 移植）。
mod mcp;
/// markdown/frontmatter 解析与写回（`opencode/shared.js` 子集移植）。
mod md_file;
/// 配置变更统一响应体构造（`opencode/config-mutation-response.js` 移植）。
mod mutation;
/// 请求级项目目录解析（`opencode/project-directory-runtime.js` 移植）。
mod project_directory;
/// provider 配置校验与增删（`opencode/providers.js` 移植）。
mod providers;
/// 行为/AGENTS.md 端点、provider 配置 CRUD、MCP OAuth 回调
/// （`opencode/routes.js` 移植）。
mod routes;
/// JS 语义工具函数（路径解析、query 读取、真值判定等）。
mod webutil;
/// frontmatter YAML 子集的解析与序列化（不引入 yaml crate）。
mod yaml;

/// 本模块的单元测试。
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::context::RouterContext;

/// JS module-scope path constants (`shared.js` / `auth.js` / `routes.js`),
/// resolved per call instead of module load so tests can inject temp roots.
/// 中文：OpenCode 运行环境路径。与 JS 的模块级常量不同，这里在每次
/// 调用时解析，测试可注入临时目录。
#[derive(Clone, Debug)]
pub(crate) struct OpenCodeEnv {
    /// `~/.config/opencode` (JS `OPENCODE_CONFIG_DIR`).
    /// 中文：用户级配置目录 `~/.config/opencode`。
    pub config_dir: PathBuf,
    /// `~/.local/share/opencode` (auth.js `OPENCODE_DATA_DIR`).
    /// 中文：数据目录 `~/.local/share/opencode`，auth.json 所在地。
    pub data_dir: PathBuf,
    /// `~` — the static agent-dir fallback is `<home>/.omp/agent`.
    /// 中文：用户 home 目录；静态 agent 目录回退为 `<home>/.omp/agent`。
    pub home: PathBuf,
    /// Resolved `OPENCODE_CONFIG` (custom layer), read at call time like the JS.
    /// 中文：`$OPENCODE_CONFIG` 指向的自定义配置层路径（调用时读取）。
    pub custom_config: Option<PathBuf>,
}

/// 中文：`OpenCodeEnv` 的构造与各标准子路径（config.json、agents/
/// commands/skills 目录）的便捷访问器。
impl OpenCodeEnv {
    /// Production environment: paths from `$HOME`, custom layer from
    /// `$OPENCODE_CONFIG` (resolved per call, JS comment on `getConfigPaths`).
    /// 中文：生产环境：home 来自 `crate::config::home_dir()`（失败回退
    /// `.`），自定义层来自 `$OPENCODE_CONFIG` 并做词法解析。
    pub fn current() -> Self {
        let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let custom_config = std::env::var("OPENCODE_CONFIG")
            .ok()
            .map(|value| webutil::resolve_path(&value));
        Self {
            config_dir: home.join(".config").join("opencode"),
            data_dir: home.join(".local").join("share").join("opencode"),
            home,
            custom_config,
        }
    }

    /// JS `CONFIG_FILE` = `~/.config/opencode/config.json`.
    /// 中文：用户级配置文件 `~/.config/opencode/config.json` 的路径。
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    /// JS `AGENT_DIR` = `~/.config/opencode/agents`.
    /// 中文：agent markdown 目录 `~/.config/opencode/agents`。
    pub fn agent_dir(&self) -> PathBuf {
        self.config_dir.join("agents")
    }

    /// JS `COMMAND_DIR` = `~/.config/opencode/commands`.
    /// 中文：command markdown 目录 `~/.config/opencode/commands`。
    pub fn command_dir(&self) -> PathBuf {
        self.config_dir.join("commands")
    }

    /// JS `SKILL_DIR` = `~/.config/opencode/skills`.
    /// 中文：skill 目录 `~/.config/opencode/skills`。
    pub fn skill_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }
}

/// `routes.js` `pendingMcpAuthContextByState` — parked OAuth contexts keyed by
/// the per-flow `state` secret, pruned lazily against a 30-minute TTL.
/// 中文：一条挂起的 MCP OAuth 授权流：以回调携带的 `state` 秘钥为键，
/// 记录发起时的服务器名/目录/来源，30 分钟 TTL 内有效。
#[derive(Debug, Clone)]
pub(crate) struct PendingMcpAuthContext {
    /// 发起授权的 MCP 服务器名。
    pub name: String,
    /// 发起时的工作目录（项目级服务器才有）。
    pub directory: Option<String>,
    /// 发起请求的来源（用于回调后重定向回前端）。
    pub origin: Option<String>,
    /// 过期时间（墙钟毫秒，now_millis 口径）。
    pub expires_at: u64,
}

/// 中文：以 `state` 为键的挂起 OAuth 上下文表（对应 JS 模块级
/// `pendingMcpAuthContextByState`），内部用 Mutex 保护、惰性清理过期项。
#[derive(Default)]
pub(crate) struct PendingMcpAuthState {
    /// state 秘钥 → 挂起上下文的映射表。
    entries: Mutex<HashMap<String, PendingMcpAuthContext>>,
}

/// 中文：挂起上下文表的存取原语：读/写前先惰性清理过期项（TTL 30 分钟）。
impl PendingMcpAuthState {
    /// `pruneExpiredPendingMcpAuthContexts`.
    /// 中文：删除所有 `expires_at` 不大于当前时间的条目。
    fn prune_expired(&self) {
        let now = webutil::now_millis();
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| entry.expires_at > now);
    }

    /// 中文：按 state 查询上下文（先清理过期项；返回克隆）。
    fn get(&self, state: &str) -> Option<PendingMcpAuthContext> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(state)
            .cloned()
    }

    /// 中文：登记一条挂起上下文（先清理过期项再插入）。
    fn insert(&self, state: String, entry: PendingMcpAuthContext) {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(state, entry);
    }

    /// 中文：按 state 移除挂起上下文（OAuth 回调消费后调用）。
    fn remove(&self, state: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(state);
    }
}

/// Module-local state carried by the returned router.
/// 中文：随路由分发携带的模块共享状态：路由上下文、环境路径与
/// 挂起的 MCP OAuth 表。
#[derive(Clone)]
pub(crate) struct ModuleState {
    /// 全局路由上下文（会话/设置/引擎等）。
    pub ctx: RouterContext,
    /// 注入的 OpenCode 环境路径。
    pub env: OpenCodeEnv,
    /// 挂起的 MCP OAuth 上下文表（路由间共享）。
    pub pending: Arc<PendingMcpAuthState>,
}

/// 中文：生产入口：以当前真实环境构造 opencode 配置路由。
pub fn router(ctx: RouterContext) -> axum::Router {
    router_with_env(ctx, OpenCodeEnv::current())
}

/// Test seam mirroring the JS dependency injection: the same router with an
/// explicit environment (temp config/data roots).
/// 中文：测试接缝：与 [`router`] 相同的路由，但显式注入环境
/// （临时 config/data 根目录），镜像 JS 的依赖注入写法。
pub(crate) fn router_with_env(ctx: RouterContext, env: OpenCodeEnv) -> axum::Router {
    let state = ModuleState {
        ctx,
        env,
        pending: Arc::new(PendingMcpAuthState::default()),
    };
    routes::provider_routes()
        .merge(entity_routes::entity_routes())
        .with_state(state)
}
