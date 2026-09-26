//! Port of three sibling hub consumers (each JS module keeps its own file
//! here):
//!
//! - [`knowledge`] — `server/lib/session-knowledge/runtime.js`: what a
//!   session must be told about the project, and whether it has been told
//!   yet. Its HTTP surface (`routes.js`) is this module's router.
//! - [`assist`] — `server/lib/session-assist/runtime.js`: idle-quiet recap +
//!   suggestion generation onto session metadata.
//! - [`obligatory`] — `server/lib/context-obligatory/runtime.js`: pinned
//!   messages + knowledge re-sent as one synthetic prompt after compaction.
//!
//! Composition-root wiring notes (JS `server/index.js`):
//! - `projectContextRuntime` / `agentMemoryRuntime` /
//!   `resolveMemoryProjectId` / `isAgentMemoryEnabled` / the small-model
//!   service are injected in the JS factory; the production factories here
//!   ([`knowledge_runtime`], [`assist_runtime`], [`obligatory_runtime`])
//!   wire the real `crate::project_context` / `crate::agent_memory` /
//!   `crate::small_model` seams. The `unavailable_*` / `unresolved_*`
//!   constructors remain for tests and alternate wiring.

pub mod assist;
pub mod fetch;
pub mod knowledge;
pub mod obligatory;
pub mod routes;

pub use assist::{
    AssistError, AssistOutput, AssistRequest, AssistTargets, SessionAssistOptions,
    SessionAssistRuntime, build_assist_system_prompt, extract_user_message, settings_targets,
    spawn_hub_bridge as spawn_assist_hub_bridge, unavailable_small_model,
};
pub use fetch::{OpenCodeError, OpenCodeFetch, engine_fetch};
pub use knowledge::{
    AgentMemorySnapshot, KnowledgeSet, Pins, SessionKnowledgeOptions, SessionKnowledgeRuntime,
    build_knowledge_signature, build_knowledge_text, read_delivered_signature,
    unavailable_agent_memory, unresolved_project_context,
};
pub use obligatory::{
    ContextObligatoryOptions, ContextObligatoryRuntime, PinnedMessage, build_context_prompt,
    read_context_state, spawn_hub_bridge as spawn_obligatory_hub_bridge,
};

use std::sync::Arc;

use crate::context::RouterContext;
use crate::hub::EventHub;

/// `registerSessionKnowledgeRoutes(app, ...)` — the session-knowledge HTTP
/// surface (`/api/session-knowledge*`), wired with the production knowledge
/// runtime.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(knowledge_runtime(&ctx))
}

/// Production session-knowledge runtime — the index.js
/// `createSessionKnowledgeRuntime({ projectContextRuntime, agentMemoryRuntime,
/// resolveProjectId: resolveMemoryProjectId, isAgentMemoryEnabled })` wiring:
/// shared project-context store, shared memory store with its failed-scope
/// flags, the worktree-aware memory project resolver, and the shared memory
/// gate (feature flag first — unreleased means absent).
pub fn knowledge_runtime(ctx: &RouterContext) -> Arc<SessionKnowledgeRuntime> {
    let project_context = crate::project_context::ProjectContextRuntime::for_context(ctx);
    let project_context_plans = project_context.clone();
    let memory = crate::agent_memory::runtime_for_context(ctx);
    let resolver = crate::agent_memory::project_resolver(ctx);
    let gate_ctx = ctx.clone();
    SessionKnowledgeRuntime::new(SessionKnowledgeOptions {
        fetch: engine_fetch(Arc::clone(&ctx.engine), knowledge::FETCH_TIMEOUT_MS),
        resolve_project_id: Arc::new(move |directory: &str| {
            let resolver = resolver.clone();
            let directory = directory.to_string();
            Box::pin(async move { Ok(resolver.resolve(Some(&directory)).await) })
        }),
        read_context: Arc::new(move |project_id: &str| {
            let runtime = project_context.clone();
            let project_id = project_id.to_string();
            Box::pin(async move {
                let context = runtime.read_context(&project_id).await?;
                Ok(serde_json::to_value(context)?)
            })
        }),
        read_plan: Arc::new(move |project_id: &str, plan_id: &str| {
            let runtime = project_context_plans.clone();
            let project_id = project_id.to_string();
            let plan_id = plan_id.to_string();
            Box::pin(async move {
                // `None` reads as a plan with no body: the JS
                // `content?.body?.trim() || ''` marks it unavailable.
                match runtime.read_plan(&project_id, &plan_id).await? {
                    Some(plan) => Ok(serde_json::to_value(plan)?),
                    None => Ok(serde_json::Value::Null),
                }
            })
        }),
        read_all_memory: Arc::new(move |project_id: Option<&str>| {
            let memory = memory.clone();
            let project_id = project_id.map(str::to_string);
            Box::pin(async move {
                let all = memory.read_all(project_id.as_deref()).await;
                Ok(AgentMemorySnapshot {
                    global: entries_to_values(&all.global),
                    project: entries_to_values(&all.project),
                    global_failed: all.global_failed,
                    project_failed: all.project_failed,
                })
            })
        }),
        is_memory_enabled: Arc::new(move || {
            let ctx = gate_ctx.clone();
            Box::pin(async move { crate::agent_memory::is_agent_memory_enabled(&ctx).await })
        }),
    })
}

fn entries_to_values(entries: &[crate::agent_memory::MemoryEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap_or(serde_json::Value::Null))
        .collect()
}

/// Adapt the shared small-model service to the assist seam — the JS
/// `getSmallModelService()` + `generateSmallModelText({
/// restrictToPreferredProvider: true })` with the session's own
/// provider/model, so conversation content never leaves the provider the
/// user picked (unless the small model was chosen explicitly).
pub fn assist_small_model(
    service: Arc<crate::small_model::SmallModelService>,
) -> assist::SmallModelText {
    use crate::small_model::GenerateParams;
    Arc::new(move |request: assist::AssistRequest| {
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
                Ok(output) => Ok(assist::AssistOutput {
                    text: output.text,
                    provider_id: Some(output.provider_id),
                    model_id: Some(output.model_id),
                }),
                Err(error) => Err(assist::AssistError {
                    status: Some(error.status_code),
                    message: error.message,
                }),
            }
        })
    })
}

/// Production session-assist runtime — the index.js
/// `createSessionAssistRuntime({ buildOpenCodeUrl, getOpenCodeAuthHeaders,
/// getSmallModelService })` wiring: engine fetch, the shared small-model
/// service, and the settings-file recap/suggestion switches.
pub fn assist_runtime(ctx: &RouterContext) -> Arc<SessionAssistRuntime> {
    SessionAssistRuntime::new(SessionAssistOptions {
        fetch: engine_fetch(Arc::clone(&ctx.engine), assist::FETCH_TIMEOUT_MS),
        small_model: assist_small_model(crate::small_model::service(ctx)),
        get_targets: settings_targets(ctx.config.data_dir.clone()),
        quiet_ms: assist::IDLE_QUIET_MS,
    })
}
/// Production context-obligatory runtime over the production knowledge
pub fn obligatory_runtime(ctx: &RouterContext) -> Arc<ContextObligatoryRuntime> {
    ContextObligatoryRuntime::new(ContextObligatoryOptions {
        fetch: engine_fetch(Arc::clone(&ctx.engine), obligatory::FETCH_TIMEOUT_MS),
        session_knowledge: Some(knowledge_runtime(ctx)),
    })
}

/// Subscribe both event-driven runtimes to hub frames — the shared
/// equivalent of index.js `onPayload` handing each payload + directory to
/// `sessionAssistRuntime.processPayload` and
/// `contextObligatoryRuntime.processPayload`.
pub fn spawn_bridge(
    assist: Arc<SessionAssistRuntime>,
    obligatory: Arc<ContextObligatoryRuntime>,
    hub: Arc<EventHub>,
) -> Vec<tokio::task::JoinHandle<()>> {
    vec![
        spawn_assist_hub_bridge(assist, hub.clone()),
        spawn_obligatory_hub_bridge(obligatory, hub),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_factories_wire_the_real_sibling_sources() {
        let ctx = test_context();
        let knowledge = knowledge_runtime(&ctx);
        let assist = assist_runtime(&ctx);
        assert_eq!(assist.quiet_ms(), assist::IDLE_QUIET_MS);
        let obligatory = obligatory_runtime(&ctx);
        obligatory.stop();
        assist.stop();
        let _ = knowledge;
    }

    fn test_context() -> RouterContext {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-assist-mod-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        RouterContext {
            config: Arc::new(crate::config::ServerConfig {
                port: 0,
                host: None,
                lan: false,
                ui_password: None,
                api_only: false,
                data_dir: dir.clone(),
                dist_dir: dir.join("dist"),
                tunnel: Default::default(),
                engine: crate::config::EngineConfig::External {
                    base_url: String::new(),
                },
            }),
            engine: crate::engine::EngineState::external(String::new(), None),
            hub: EventHub::new(),
        }
    }
}
