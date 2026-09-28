//! Port of `packages/web/server/lib/git/` — the `/api/git/*` route family and
//! its service layer.
//!
//! JS source map:
//! - `git/routes.js`            → `routes.rs`
//! - `git/service.js`           → `service.rs` (context/status/diff/identity
//!   plumbing) + `service_ops.rs` (staging, commit, branches, remotes,
//!   push/pull/fetch, merge/rebase, stash, log) + `worktrees.rs` (worktree
//!   management + bootstrap) + `integrate.rs` (integrate workflow)
//! - `git/credentials.js`       → `identity.rs`
//! - `git/identity-storage.js`  → `identity.rs`
//! - `git/worktree-watcher.js`  → `watcher.rs` (polling watcher; see module
//!   docs for the fs.watch → poll substitution)
//!
//! The JS `simple-git` dependency is not ported as a library: the service
//! invokes `git` directly with the same argv shapes and reproduces the
//! response shapes simple-git's parsers produced (verified against
//! simple-git@3.36.0 in `packages/web/node_modules`).
//!
//! Known gaps (see PORT-MANIFEST.md):
//! - The worktree watcher polls instead of fs.watch (no notify crate); event
//!   debouncing is approximated by the poll interval.
//! - `listProjects` for the watcher reads `<data_dir>/settings.json` directly
//!   instead of going through the settings runtime (not yet wired here).
//! - `git status` runs from the repository root (simple-git's baseDir is the
//!   caller directory); identical outcomes, one fewer process concern.
//!
//! （中文概述）Rust 版 git 路由族的模块入口：组装 `/api/git/*` 的 axum
//! 路由器、构造共享状态 `GitState`，并在进程内启动 linked-worktree
//! 轮询监视器，拓扑变化时经共享事件 hub 广播
//! `ompchamber:worktrees-changed`。服务层不复用 simple-git 库，而是以
//! 相同的 argv 直接调用 `git` 子进程，并逐字段复刻 simple-git 解析出的
//! 响应结构。

/// git 子进程执行层：argv 构造、`GIT_*` 环境注入与结果解析
/// （移植 JS 的 `runGitCommand` / `createGit` 机制；runner 以闭包注入便于测试）。
mod exec;
/// git 身份档案存储（`~/.config/ompchamber/git-identities.json`）
/// 与 `~/.git-credentials` 的 host/username 凭据发现。
mod identity;
/// integrate 工作流的服务层实现（对应 JS `git/service.js` 中的 integrate 部分）。
mod integrate;
/// 路径规整、路径安全校验与 porcelain 输出解析等模块级工具
/// （`normalizeDirectoryPath`、`parseWorktreePorcelain` 等）。
mod paths;
/// `/api/git/*` 的 axum 路由定义与共享状态 `GitState`。
pub mod routes;
/// 服务层入口：仓库上下文、status/diff、identity 管道
/// （对应 JS `git/service.js` 的上下文与查询部分）。
pub mod service;
/// 服务层操作实现：staging、commit、分支、远端、push/pull/fetch、
/// merge/rebase、stash、log。
pub mod service_ops;
/// linked-worktree 轮询监视器：轮询注册项目的 `.git/worktrees` 元数据，
/// 变化时通过回调广播（替代 JS 的 fs.watch，见模块文档的取舍说明）。
mod watcher;
/// 工作树管理：创建/列表/删除 linked worktree 及 bootstrap 流程。
pub mod worktrees;

use std::sync::Arc;

use crate::context::RouterContext;

/// 路由共享状态：`GitService`、身份存储、事件 hub 与 data_dir。
pub use routes::GitState;
/// 工作树轮询监视器，以及 git 公共目录（`.git`/worktrees）解析工具。
pub use watcher::{WorktreeWatcher, resolve_git_common_dir};

/// 组装 `/api/git/*` 路由器：构造 `GitState`（新建 `GitService`、
/// 默认身份存储、克隆事件 hub 与 data_dir），随后启动 linked-worktree
/// 拓扑监视器（detach 运行，进程退出时随之终止），最后返回带状态的
/// axum `Router`。
pub fn router(ctx: RouterContext) -> axum::Router {
    let state = GitState {
        service: Arc::new(service::GitService::new()),
        identity: Arc::new(identity::IdentityStorage::default()),
        hub: Arc::clone(&ctx.hub),
        data_dir: ctx.config.data_dir.clone(),
    };

    // Linked-worktree topology watcher (JS wires this in server/index.js):
    // polls registered projects' `.git/worktrees` metadata and broadcasts
    // `ompchamber:worktrees-changed` frames on the shared event hub.
    {
        let hub = Arc::clone(&ctx.hub);
        let data_dir = ctx.config.data_dir.clone();
        let list_projects: watcher::ListProjects =
            Arc::new(move || watcher::list_projects_from_settings(&data_dir));
        let on_changed: watcher::OnWorktreesChanged = Arc::new(move |directories| {
            watcher::publish_worktrees_changed(&hub, directories);
        });
        let mut runtime = WorktreeWatcher::new(list_projects, on_changed);
        runtime.start();
        // The watcher task stops itself via Drop/abort at process exit; keep
        // it detached like the JS module-scope runtime.
        std::mem::forget(runtime);
    }

    routes::routes().with_state(state)
}

/// git_service 模块的单元测试（实现在 `tests.rs`）。
#[cfg(test)]
mod tests;
