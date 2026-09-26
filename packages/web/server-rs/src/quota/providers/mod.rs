//! Port of `server/lib/quota/providers/index.js` — the provider registry and
//! dispatcher. Registry order matches the JS `Object.entries` insertion
//! order because `GET /api/quota/providers` returns it.

use crate::quota::deps::QuotaDeps;

pub(crate) struct ProviderEntry {
    pub id: &'static str,
    pub is_configured: fn(&QuotaDeps) -> bool,
    pub fetch: fn(
        std::sync::Arc<crate::quota::runtime::QuotaRuntime>,
    ) -> futures::future::BoxFuture<'static, serde_json::Value>,
}

macro_rules! provider_registry_entry {
    ($id:expr, $is_configured:expr, $fetch:expr) => {
        ProviderEntry {
            id: $id,
            is_configured: $is_configured,
            fetch: $fetch,
        }
    };
}

macro_rules! provider {
    ($id:expr, $module:ident) => {
        ProviderEntry {
            id: $id,
            is_configured: $module::is_configured,
            fetch: $module::fetch_quota,
        }
    };
}

pub(crate) fn registry() -> &'static [ProviderEntry] {
    static REGISTRY: &[ProviderEntry] = &[
        provider!("claude", claude),
        provider!("codex", codex),
        provider!("crof", crof),
        provider!("cursor", cursor),
        provider!("deepseek", deepseek),
        provider!("google", google),
        provider!("zai-coding-plan", zai),
        provider!("zhipuai-coding-plan", zhipuai),
        provider!("kimi-for-coding", kimi),
        provider!("openrouter", openrouter),
        provider!("nano-gpt", nanogpt),
        provider!("github-copilot", copilot),
        provider_registry_entry!(
            "github-copilot-addon",
            copilot::is_configured,
            copilot::fetch_quota_addon
        ),
        provider!("minimax-coding-plan", minimax),
        provider_registry_entry!(
            "minimax-cn-coding-plan",
            minimax::is_configured_cn,
            minimax::fetch_quota_cn
        ),
        provider!("ollama-cloud", ollama_cloud),
        provider!("wafer", wafer),
        provider!("opencode-go", opencode_go),
        provider!("neuralwatt", neuralwatt),
        provider!("xai", xai),
    ];
    REGISTRY
}

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod crof;
pub mod cursor;
pub mod deepseek;
pub mod google;
pub mod kimi;
pub mod minimax;
pub mod nanogpt;
pub mod neuralwatt;
pub mod ollama_cloud;
pub mod openai;
pub mod opencode_go;
pub mod openrouter;
pub mod wafer;
pub mod xai;
pub mod zai;
pub mod zhipuai;
