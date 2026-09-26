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

pub mod auth_store;
pub mod call;
pub mod catalog;
pub mod http;
pub mod opencode_config;
pub mod resolve;
pub mod routes;
pub mod runtime_providers;
pub mod service;
pub mod summarization;

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
pub fn service(ctx: &RouterContext) -> Arc<SmallModelService> {
    SmallModelService::production(Arc::clone(&ctx.engine), ctx.config.data_dir.clone())
}

/// `registerSmallModelRoutes(app, { getSmallModelService })` — the runtime API
/// surface, gated by the ui-auth middleware like every other `/api` route.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::routes(service(&ctx)).layer(crate::ui_auth::middleware(ctx))
}

/// Adapt the service to session-goal's audit seam
/// (`crate::session_goal::runtime::AuditService`): mirrors the JS goal loop's
/// `getSmallModelService()` + `generateSmallModelText({ restrictToPreferredProvider: true })`,
/// mapping the 404 "no small model" answer to `Unavailable` and every other
/// failure to `Failed` (logged by the caller, not here).
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
