//! Port of `server/lib/quota/` — quota usage tracking for AI providers.
//!
//! Layout mirrors the JS module:
//! - [`utils`]: `quota/utils/` shared coercion, formatters, transformers.
//! - [`credentials`]: `quota/credentials/` managed-credential store.
//! - [`providers`]: `quota/providers/*` — every provider fetch, auth
//!   resolution, and transform, plus the dispatcher registry.
//! - [`runtime`]: the coalescing `pendingFetches` map and Claude/xAI caches.
//! - [`routes`]: `registerQuotaRoutes` on axum.
//!
//! Known port gaps (documented for PORT-MANIFEST):
//! - `Date.parse` is an ISO-8601 subset; offset-less timestamps read as UTC
//!   (JS would apply the server's local zone).
//! - `resetAtFormatted`/`resetAfterFormatted` use deterministic en-US/UTC
//!   labels; JS renders them in the server locale.
//! - Body-parse failures on the credential PUT answer with axum's text
//!   rejection (JS Express answers its HTML error page); both are 400s.
//! - Network error strings carry the reqwest message where JS carried the
//!   Node fetch message (`fetch failed`); mapped messages ("Request timed
//!   out", "Invalid response from provider", provider status strings) match.
//!
//! 中文说明：本模块是 `server/lib/quota/`（AI provider 配额/用量追踪）的
//! Rust 移植，子模块划分与 JS 版一一对应（见上）；已知的移植差异也逐条
//! 记录在上方 PORT-MANIFEST 段落中。

/// 受管凭据存取：ollama-cloud/cursor 凭据的安全落盘、读取与状态掩码。
pub mod credentials;
/// provider 共用的依赖注入接口（`QuotaDeps`）及其真实环境实现。
pub mod deps;
/// quota 出站 HTTP 的传输 seam（`HttpFetch`）与 reqwest 生产实现。
pub mod http;
/// 各 provider 的配额抓取、凭据解析与结果转换，以及分发注册表。
pub mod providers;
/// `/api/quota/*` 的 axum 路由（`registerQuotaRoutes` 的移植）。
pub mod routes;
/// 请求合并运行时：`pendingFetches` 槽位与 Claude/xAI 跨请求状态。
pub mod runtime;
/// 共享的类型强制转换、格式化器与结果转换工具。
pub mod utils;

use crate::context::RouterContext;

#[cfg(test)]
/// quota 模块的测试（tests 子模块）。
mod tests;
#[cfg(test)]
/// 测试共享的 fake 设施（脚本化 HTTP、隔离环境等）。
pub(crate) mod tests_support;

/// `registerQuotaRoutes(app, { getQuotaProviders })`.
///
/// 中文说明：注册 quota 路由的入口（对应 JS 的
/// `registerQuotaRoutes(app, { getQuotaProviders })`），直接委托
/// [`routes::router`]。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(ctx)
}
