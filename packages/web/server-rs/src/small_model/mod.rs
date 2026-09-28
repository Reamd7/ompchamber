//! Port of `server/lib/small-model/` — server-side direct LLM calls that
//! reuse the user's existing OpenCode provider logins
//! (`~/.local/share/opencode/auth.json`), plus `server/lib/text/summarization.js`
//! (moved here as [`summarization`] with an injectable model seam).
//!
//! Composition-root notes (JS `feature-routes-runtime.js` + `server/index.js`):
//! - Routes (`routes::routes`) are mounted lazily in JS; here they are part of
//!   this module's [`router`] behind the ui-auth gate like every other
//!   runtime API.
//! - The runtime-provider snapshot is wired to the managed engine (JS
//!   `configureOpenCodeRuntimeProviders`) with a 30s TTL refresh; the JS
//!   restart-reset is covered by the TTL.
//! - [`audit_service`] adapts the service to session-goal's `AuditService`
//!   seam (404 → `Unavailable`, anything else → `Failed`), and
//!   [`distiller`] adapts it to the goal-creation `Distiller` seam, so goal
//!   mode can wire real small-model evaluation once the composition root
//!   passes them in.
//!
//! 中文说明：`server/lib/small-model/` 的移植——服务端直连 LLM，复用用户既有的
//! OpenCode provider 登录（~/.local/share/opencode/auth.json），并收编
//! `server/lib/text/summarization.js`（成为 summarization 子模块，模型缝可注入）。
//! 组装要点：路由与其它 /api 路由一样挂在 ui-auth 门之后；runtime provider 快照
//! 接引擎并按 30s TTL 刷新（JS 里的重启重置由 TTL 覆盖）；audit_service 与
//! distiller 两个适配器把 service 接到 session-goal 的对应缝上。

/// auth.json 的读取、写回与备份。
pub mod auth_store;
/// 按 provider 线格式直连小模型的调用层。
pub mod call;
/// models.dev 目录缓存（内存 + 磁盘 + 条件网络刷新）。
pub mod catalog;
/// 可注入的出站 HTTP 通道与 JS 语义工具函数。
pub mod http;
/// OpenCode 配置层的读取与深合并。
pub mod opencode_config;
/// 小模型选择的 fallback 链。
pub mod resolve;
/// /api/small-model 的 axum 路由。
pub mod routes;
/// 引擎 runtime provider 的快照与解析。
pub mod runtime_providers;
/// 组装解析与调用的门面 service。
pub mod service;
/// 文本摘要（summarization.js 的移植）。
pub mod summarization;

/// 本模块的集成测试。
#[cfg(test)]
mod tests;

pub use auth_store::{AuthStore, FsAuthStore, auth_file_path};
pub use call::{
    CallDeps, CallParams, DEDICATED_WIRE_FORMAT_PROVIDERS, SmallModelError, SmallModelResult,
    call_small_model, is_dedicated_wire_format_provider, resolve_provider_login,
};
pub use catalog::{CatalogCache, MODELS_DEV_API_URL, get_catalog_provider};
pub use http::{Fetch, FetchRequest, FetchResponse};
pub use opencode_config::{ConfigLayers, ConfigReader, read_config, read_config_layers};
pub use resolve::{ResolvedModel, is_usable_auth_entry, parse_model_ref, resolve_small_model};
pub use runtime_providers::{
    ProviderSnapshot, RuntimeProvider, RuntimeProviders, ZEN_ANONYMOUS_API_KEY,
    engine_provider_fetch, parse_provider_listing,
};
pub use service::{
    DescribeResult, GenerateOutput, GenerateParams, OutputReserve, OverflowPolicy, ReserveLimits,
    SmallModelService,
};
pub use summarization::{
    ModelSummarySource, SummarizeParams, SummaryMode, sanitize_for_note, sanitize_for_tts,
    summarize_text,
};

use std::sync::Arc;

use crate::context::RouterContext;

/// The shared service for this server process (JS module state).
///
/// 中文补充：进程内共享的 service 工厂（等价 JS 的模块级状态），由引擎与数据目录
/// 组装出生产实现。
pub fn service(ctx: &RouterContext) -> Arc<SmallModelService> {
    SmallModelService::production(Arc::clone(&ctx.engine), ctx.config.data_dir.clone())
}

/// `registerSmallModelRoutes(app, { getSmallModelService })` — the runtime API
/// surface, gated by the ui-auth middleware like every other `/api` route.
///
/// 中文补充：注册 /api/small-model 路由并套上与其它 /api 路由相同的 ui-auth 中间件。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::routes(service(&ctx)).layer(crate::ui_auth::middleware(ctx))
}

/// Adapt the service to session-goal's audit seam
/// (`crate::session_goal::runtime::AuditService`): mirrors the JS goal loop's
/// `getSmallModelService()` + `generateSmallModelText({ restrictToPreferredProvider: true })`,
/// mapping the 404 "no small model" answer to `Unavailable` and every other
/// failure to `Failed` (logged by the caller, not here).
///
/// 中文补充：404（无小模型）映射为 Unavailable、其余失败映射为 Failed；
/// 日志由调用方记录。
pub fn audit_service(service: Arc<SmallModelService>) -> crate::session_goal::AuditService {
    Arc::new(move |request: crate::session_goal::AuditRequest| {
        let service = Arc::clone(&service);
        Box::pin(async move {
            match service
                .generate(GenerateParams {
                    prompt: Some(request.prompt),
                    system: Some(request.system),
                    directory: Some(request.directory),
                    preferred_provider_id: request.preferred_provider_id,
                    preferred_model_id: request.preferred_model_id,
                    restrict_to_preferred_provider: true,
                    ..Default::default()
                })
                .await
            {
                Ok(output) => Ok(crate::session_goal::AuditOutput {
                    text: output.text,
                    provider_id: Some(output.provider_id),
                    model_id: Some(output.model_id),
                }),
                Err(error) if error.status_code == 404 => {
                    Err(crate::session_goal::AuditError::Unavailable)
                }
                Err(error) => Err(crate::session_goal::AuditError::Failed(error.message)),
            }
        })
    })
}

/// Adapt the service to session-goal's objective-distillation seam
/// (`crate::session_goal::create::Distiller`): the JS `fitObjective` prompt,
/// verbatim. Errors surface as `Err(message)`; the caller warns and falls
/// back to the head/tail trim.
///
/// 中文补充：fitObjective 提示词原样移植；出错以 Err(message) 返回，
/// 调用方警告并回退到头尾截断。
pub fn distiller(service: Arc<SmallModelService>) -> crate::session_goal::Distiller {
    Arc::new(move |request: crate::session_goal::DistillRequest<'_>| {
        let service = Arc::clone(&service);
        let objective = request.objective.to_string();
        let directory = request.directory.to_string();
        let preferred_provider_id = request.provider_id.map(str::to_string);
        let preferred_model_id = request.model_id.map(str::to_string);
        Box::pin(async move {
            let generated = service
                .generate(GenerateParams {
                    prompt: Some(objective),
                    system: Some(
                        [
                            "You distill a large task description into the COMPLETION CRITERIA a progress auditor will judge against.",
                            "Return ONLY the criteria text — no preamble, no headers, no markdown fences.",
                            "Capture: the end goals, what must exist and work when the task is fully done, and how each major part is verified. Omit implementation steps.",
                            "Preserve verbatim any file paths, commands, and identifiers that define the task.",
                            "Stay under 4000 characters.",
                            "Write in the same language as the task text.",
                        ]
                        .join("\n"),
                    ),
                    directory: Some(directory),
                    preferred_provider_id,
                    preferred_model_id,
                    restrict_to_preferred_provider: true,
                    ..Default::default()
                })
                .await
                .map_err(|error| error.message)?;
            Ok(generated.text)
        })
    })
}
