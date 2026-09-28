//! walkthrough（diff 引导读法）子系统的入口与模块地图。
//!
//! 职责：把一次 diff 组织成有顺序的导读——small model 将相关 hunk 分组
//! 为 stop/chapter 并逐组解释，UI 再把解说与对应代码交替渲染。生成只能
//! 由用户显式发起（消耗 token），服务端绝不自动触发。
//! Port of `server/lib/walkthrough/` — guided walkthroughs of a diff.
//!
//! Generates a guided, ordered reading path through a diff: the small model
//! groups related hunks into stops and chapters and explains each group, and
//! the UI renders those stops interleaved with the code they describe.
//! Generation is **always user-initiated** — it spends tokens, so a person has
//! to ask for it.
//!
//! JS source map:
//! - `walkthrough/hunks.js`          → `hunks.rs` (hunk parsing + ids)
//! - `walkthrough/generated.js`      → `generated.rs`
//! - `walkthrough/sources.js`        → `sources.rs` (+ `GitDeps` seam)
//! - `walkthrough/digest.js`         → `digest.rs`
//! - `walkthrough/prompt.js`         → `prompt.rs`
//! - `walkthrough/schema.js`         → `schema.rs`
//! - `walkthrough/store.js`          → `store.rs`
//! - `walkthrough/pull-request.js`   → `pull_request.rs`
//! - `walkthrough/model-settings.js` → `model_settings.rs`
//! - `walkthrough/languages.js`      → `languages.rs`
//! - `walkthrough/index.js`          → `service.rs` (jobs + orchestration)
//! - `walkthrough/routes.js`         → `routes.rs`
//! - SHA-1 (hunk ids must stay byte-identical to Node's `crypto`) → `sha1.rs`
//! - `walkthrough/small-model` dependency → `small_model.rs` seam, defaulting
//!   to Unavailable until that module's port lands (see `small_model.rs` for
//!   the wiring point)
//!
//! Hunk identity lives in `hunks.rs` alone: the client never recomputes ids.

/// 构建 model-facing digest（文件、hunk、别名映射与序列化）。
pub mod digest;
/// 统一错误类型及其到 HTTP 响应的映射。
pub mod error;
/// 工具产物文件（lockfile、压缩产物、codegen 等）的识别。
pub mod generated;
/// unified diff 解析与 hunk id 的唯一权威实现。
pub mod hunks;
/// 导览输出语言表与 tag 规整。
pub mod languages;
/// walkthrough 专用模型覆盖设置的读取。
pub mod model_settings;
/// system/user prompt 的构建。
pub mod prompt;
/// GitHub PR diff 的获取。
pub mod pull_request;
/// /api/walkthrough* HTTP 路由。
pub mod routes;
/// 输出 schema、规模上限与版本常量。
pub mod schema;
/// 生成任务编排（jobs + orchestration）。
pub mod service;
/// 与 Node crypto 逐字节一致的 SHA-1 实现。
pub mod sha1;
/// small-model 服务 seam 及取消信号、错误映射等本地机制。
pub mod small_model;
/// diff 来源（working-tree/branch/pr）的采集。
pub mod sources;
/// walkthrough 缓存条目与指针的磁盘存储。
pub mod store;

use std::sync::Arc;

use crate::context::RouterContext;

/// 装配 walkthrough 子系统：构造 git 服务与 PR differ，创建服务实例
///（small-model seam 暂为 unavailable），启动一次后台指针清理，
/// 最后返回挂载好的 axum 路由。
/// `registerWalkthroughRoutes(app, { getWalkthroughService })` — the
/// `/api/walkthrough*` family on axum.
pub fn router(ctx: RouterContext) -> axum::Router {
    let git = Arc::new(crate::git_service::service::GitService::new());
    let pr_differ =
        pull_request::pull_request_differ(ctx.config.data_dir.clone(), Arc::clone(&git));
    let service = service::WalkthroughService::new(
        ctx.config.data_dir.clone(),
        sources::GitDeps::real(git),
        small_model::unavailable_small_model(),
        Some(pr_differ.into_pr_diff_fn()),
    );

    // JS deferred the pointer prune off the module's first import; the router
    // is that moment here. Never awaited on the request path.
    service::spawn_housekeeping(Arc::new(store::Store::new(&ctx.config.data_dir)));

    routes::routes(service)
}

/// 服务层测试模块。
#[cfg(test)]
mod service_tests;
