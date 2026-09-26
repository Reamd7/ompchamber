//! Port of `server/lib/small-model/index.js`: orchestration —
//! `generateSmallModelText`, `describeSmallModel`, `listAuthenticatedProviders`
//! — plus the settings override, input clamping and output-budget rules.

use std::sync::Arc;

use serde_json::Value;

use crate::small_model::auth_store::FsAuthStore;
use crate::small_model::call::{
    CallDeps, SmallModelError, is_dedicated_wire_format_provider, resolve_provider_login,
};
use crate::small_model::catalog::CatalogCache;
use crate::small_model::http::{utf16_len, utf16_prefix};
use crate::small_model::opencode_config::{ConfigReader, fs_config_reader};
use crate::small_model::resolve::{
    ResolveParams, ResolvedModel, get_auth_entry_for_provider, is_usable_auth_entry,
    parse_model_ref, resolve_small_model,
};
use crate::small_model::runtime_providers::RuntimeProviders;

/// Never a small model, whatever the transport looks like. A plugin can
/// publish an OpenAI-compatible endpoint for Claude Code, but it is a façade
/// over the Claude Agent SDK, which spawns the Claude Code CLI per request
/// and spends the user's Claude subscription rate limit.
const CLAUDE_CODE_PROVIDER: &str = "claude-code";

// Rough safety clamp so a huge input never blows the model's context window.
// Token estimate is ~4 chars/token; when the catalog has no limit for the
// model (Copilot/codex utility models are not listed) a conservative default
// applies.
const DEFAULT_CONTEXT_TOKENS: u64 = 64_000;
const OUTPUT_RESERVE_TOKENS: u64 = 4_000;
const CHARS_PER_TOKEN: u64 = 4;
const MIN_INPUT_BUDGET_TOKENS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// `truncate` (default): clip the tail and report `inputTruncated: true`.
    /// Correct for callers that degrade gracefully.
    Truncate,
    /// `error`: throw a 413 `context-too-small`. Correct for callers whose
    /// output would be quietly wrong on a clipped input.
    Error,
}

/// OMPChamber's own settings (Settings → Sessions → Small Model): when
/// `smallModelUseDefault` is false, `smallModelOverride` outranks every other
/// resolution step. Parse failures read as "no override".
fn read_small_model_settings_override(settings_path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(settings_path).ok()?;
    let settings: Value = serde_json::from_str(&raw).ok()?;
    if !settings.as_object().is_some() {
        return None;
    }
    if settings.get("smallModelUseDefault") != Some(&Value::Bool(false)) {
        return None;
    }
    settings
        .get("smallModelOverride")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `readConfiguredSmallModel`: `small_model` from the merged config layers.
fn read_configured_small_model(
    config: &ConfigReader,
    working_directory: Option<&str>,
) -> Option<String> {
    let merged = config(working_directory).merged;
    merged
        .get("small_model")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Input budget in characters, given how much of the context the caller
/// intends to leave for the answer. The reserve must match the output budget
/// the caller will actually request.
pub struct InputCharBudget {
    pub max_chars: u64,
    pub context_tokens: u64,
    pub context_known: bool,
}

pub fn get_model_input_char_budget(
    catalog: &Value,
    provider_id: &str,
    model_id: &str,
    output_reserve_tokens: Option<u64>,
) -> InputCharBudget {
    let limit = catalog
        .get(provider_id)
        .and_then(|provider| provider.get("models"))
        .and_then(|models| models.get(model_id))
        .and_then(|model| model.get("limit"));
    let raw_context = limit
        .and_then(|limit| limit.get("context"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let known = raw_context > 0.0;
    let context_tokens = if known {
        raw_context as u64
    } else {
        DEFAULT_CONTEXT_TOKENS
    };
    let reserve = output_reserve_tokens
        .filter(|value| *value > 0)
        .unwrap_or(OUTPUT_RESERVE_TOKENS);
    let input_budget_tokens = context_tokens
        .saturating_sub(reserve)
        .max(MIN_INPUT_BUDGET_TOKENS);
    InputCharBudget {
        max_chars: input_budget_tokens * CHARS_PER_TOKEN,
        context_tokens,
        context_known: known,
    }
}

/// The output budget to actually request: what the caller asked for, capped
/// by what the model admits it can emit (`limit.output`). Asking for more is
/// rejected outright by some providers and silently ignored by others.
fn resolve_output_tokens(
    catalog: &Value,
    provider_id: &str,
    model_id: &str,
    max_output_tokens: Option<u64>,
) -> Option<u64> {
    let requested = max_output_tokens?;
    if requested == 0 {
        return None;
    }
    let limit = catalog
        .get(provider_id)
        .and_then(|provider| provider.get("models"))
        .and_then(|models| models.get(model_id))
        .and_then(|model| model.get("limit"))
        .and_then(|limit| limit.get("output"))
        .and_then(Value::as_u64)
        .filter(|limit| *limit > 0);
    Some(limit.map_or(requested, |limit| requested.min(limit)))
}

/// `clampPromptToModelLimit`.
fn clamp_prompt_to_model_limit(
    prompt: &str,
    catalog: &Value,
    provider_id: &str,
    model_id: &str,
    on_overflow: OverflowPolicy,
    output_reserve_tokens: Option<u64>,
) -> Result<(String, bool), SmallModelError> {
    let budget = get_model_input_char_budget(catalog, provider_id, model_id, output_reserve_tokens);
    if utf16_len(prompt) as u64 <= budget.max_chars {
        return Ok((prompt.to_string(), false));
    }
    if on_overflow == OverflowPolicy::Error {
        return Err(SmallModelError {
            status_code: 413,
            message: format!(
                "Input is too large for {provider_id}/{model_id}: {} characters exceeds the {} the model's context allows",
                utf16_len(prompt),
                budget.max_chars
            ),
            code: Some("context-too-small".to_string()),
            provider_id: Some(provider_id.to_string()),
            required_chars: Some(utf16_len(prompt)),
            available_chars: Some(budget.max_chars as usize),
        });
    }
    Ok((
        format!(
            "{}\u{2026}",
            utf16_prefix(prompt, budget.max_chars as usize)
        ),
        true,
    ))
}

pub struct GenerateParams {
    pub prompt: Option<String>,
    pub system: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub model: Option<String>,
    pub directory: Option<String>,
    pub preferred_provider_id: Option<String>,
    pub preferred_model_id: Option<String>,
    pub restrict_to_preferred_provider: bool,
    pub response_schema: Option<Value>,
    pub timeout_ms: Option<u64>,
    pub on_overflow: OverflowPolicy,
}

impl Default for GenerateParams {
    fn default() -> Self {
        Self {
            prompt: None,
            system: None,
            max_output_tokens: None,
            model: None,
            directory: None,
            preferred_provider_id: None,
            preferred_model_id: None,
            restrict_to_preferred_provider: false,
            response_schema: None,
            timeout_ms: None,
            on_overflow: OverflowPolicy::Truncate,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerateOutput {
    pub text: String,
    pub provider_id: String,
    pub model_id: String,
    pub source: String,
    pub input_truncated: Option<bool>,
}

impl GenerateOutput {
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("text".to_string(), Value::String(self.text.clone()));
        object.insert(
            "providerID".to_string(),
            Value::String(self.provider_id.clone()),
        );
        object.insert("modelID".to_string(), Value::String(self.model_id.clone()));
        object.insert("source".to_string(), Value::String(self.source.clone()));
        if self.input_truncated == Some(true) {
            object.insert("inputTruncated".to_string(), Value::Bool(true));
        }
        Value::Object(object)
    }
}

/// The service: one per server (the JS module state lives here).
pub struct SmallModelService {
    pub deps: Arc<CallDeps>,
    pub catalog: Arc<CatalogCache>,
    pub settings_path: std::path::PathBuf,
}

impl SmallModelService {
    /// Production wiring over the real filesystem and engine.
    pub fn production(
        engine: Arc<crate::engine::EngineState>,
        data_dir: std::path::PathBuf,
    ) -> Arc<Self> {
        let fetch = crate::small_model::http::reqwest_fetch();
        Arc::new(Self {
            catalog: CatalogCache::new(
                Arc::clone(&fetch),
                data_dir.join("models-dev.catalog.json"),
            ),
            deps: CallDeps::new(
                fetch,
                fs_config_reader(),
                Arc::new(FsAuthStore::default()),
                RuntimeProviders::from_engine(engine),
            ),
            settings_path: data_dir.join("settings.json"),
        })
    }

    /// Test wiring over injected seams.
    pub fn new(
        deps: Arc<CallDeps>,
        catalog: Arc<CatalogCache>,
        settings_path: std::path::PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            deps,
            catalog,
            settings_path,
        })
    }

    /// `generateSmallModelText`: resolves and authenticates entirely
    /// server-side from the OpenCode config and auth store.
    pub async fn generate(
        &self,
        params: GenerateParams,
    ) -> Result<GenerateOutput, SmallModelError> {
        let prompt = params.prompt.as_deref().unwrap_or("");
        if prompt.trim().is_empty() {
            return Err(SmallModelError::with_status(400, "prompt is required"));
        }

        let auth = self
            .deps
            .auth_store
            .read()
            .map_err(SmallModelError::internal)?;
        let catalog = self
            .catalog
            .get_model_catalog()
            .await
            .unwrap_or_else(|_| std::sync::Arc::new(Value::Object(Default::default())));

        let resolved = match params.model.as_deref().and_then(parse_model_ref) {
            Some((provider_id, model_id)) => Some(ResolvedModel {
                provider_id,
                model_id,
                source: "request",
            }),
            None => resolve_small_model(ResolveParams {
                auth: &auth,
                catalog: &catalog,
                settings_small_model: read_small_model_settings_override(&self.settings_path)
                    .as_deref(),
                config_small_model: read_configured_small_model(
                    &self.deps.config,
                    params.directory.as_deref(),
                )
                .as_deref(),
                preferred_provider_id: params
                    .preferred_provider_id
                    .as_deref()
                    .filter(|value| !value.is_empty()),
                preferred_model_id: params
                    .preferred_model_id
                    .as_deref()
                    .filter(|value| !value.is_empty()),
            }),
        };
        let Some(resolved) = resolved else {
            return Err(SmallModelError::with_status(
                404,
                "No small model available — no authenticated provider has a suitable model",
            ));
        };

        if resolved.provider_id == CLAUDE_CODE_PROVIDER {
            return Err(SmallModelError::with_code(
                422,
                "small-model-provider-unsupported",
                "Claude Code cannot be used for background small-model actions. Choose another Small Model in Settings → Sessions.",
            ));
        }

        // Callers with a session context can forbid silently switching
        // providers: an explicit user choice (settings override, opencode
        // config, request model) is always allowed, anything else must stay
        // on the session's provider.
        if params.restrict_to_preferred_provider
            && !matches!(resolved.source, "settings" | "config" | "request")
            && Some(resolved.provider_id.as_str())
                != params
                    .preferred_provider_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
        {
            return Err(SmallModelError::with_status(
                404,
                "No small model available within the session provider",
            ));
        }

        let output_tokens = resolve_output_tokens(
            &catalog,
            &resolved.provider_id,
            &resolved.model_id,
            params.max_output_tokens,
        );

        let (clamped_prompt, truncated) = clamp_prompt_to_model_limit(
            prompt.trim(),
            &catalog,
            &resolved.provider_id,
            &resolved.model_id,
            params.on_overflow,
            output_tokens,
        )?;

        let system = params
            .system
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let text = crate::small_model::call::call_small_model(
            &self.deps,
            crate::small_model::call::CallParams {
                auth: &auth,
                catalog: &catalog,
                working_directory: params.directory.as_deref(),
                provider_id: &resolved.provider_id,
                model_id: &resolved.model_id,
                prompt: &clamped_prompt,
                system,
                max_output_tokens: output_tokens,
                response_schema: params.response_schema.as_ref(),
                timeout_ms: params.timeout_ms,
            },
        )
        .await?;

        Ok(GenerateOutput {
            text: text.trim().to_string(),
            provider_id: resolved.provider_id,
            model_id: resolved.model_id,
            source: resolved.source.to_string(),
            input_truncated: truncated.then_some(true),
        })
    }

    /// `listAuthenticatedProviders`: provider ids this module can actually
    /// call — an auth.json login, or the credential + endpoint OpenCode
    /// resolved at runtime for a plugin provider.
    pub async fn list_authenticated_providers(&self) -> Vec<String> {
        let auth = match self.deps.auth_store.read() {
            Ok(auth) => auth,
            Err(_) => return Vec::new(),
        };
        let mut ids: Vec<String> = Vec::new();
        let Some(object) = auth.as_object() else {
            return ids;
        };
        for (provider_id, entry) in object {
            if is_usable_auth_entry(entry) && !ids.contains(provider_id) {
                ids.push(provider_id.clone());
            }
        }
        // The catalog id is github-copilot while legacy auth entries may sit
        // under the copilot alias.
        if get_auth_entry_for_provider(&auth, "github-copilot").is_some_and(is_usable_auth_entry)
            && !ids.iter().any(|id| id == "github-copilot")
        {
            ids.push("github-copilot".to_string());
        }
        // Kept separate so a runtime lookup that goes wrong costs the
        // providers it would have added, never the logins already established
        // from disk.
        if let Some(snapshot) = self.deps.runtime.snapshot().await {
            for id in &snapshot.connected {
                let Some(provider) = snapshot.providers.get(id) else {
                    continue;
                };
                // No credential we may use — including the zen sentinel,
                // whose free models belong to OpenCode's own server.
                if provider.api_key.as_deref().unwrap_or("").is_empty()
                    || provider.base_url.as_deref().unwrap_or("").is_empty()
                {
                    continue;
                }
                // Reached through a dedicated wire format and already covered
                // by the auth.json scan above.
                if is_dedicated_wire_format_provider(id) {
                    continue;
                }
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }
        ids.retain(|id| id != CLAUDE_CODE_PROVIDER);
        ids
    }

    /// `describeSmallModel`: reports which model would be used, without
    /// calling it.
    pub async fn describe_small_model(
        &self,
        directory: Option<&str>,
        preferred_provider_id: Option<&str>,
        preferred_model_id: Option<&str>,
        output_reserve_tokens: OutputReserve,
        override_model: Option<&str>,
    ) -> Result<Option<DescribeResult>, SmallModelError> {
        let auth = self
            .deps
            .auth_store
            .read()
            .map_err(SmallModelError::internal)?;
        let catalog = self
            .catalog
            .get_model_catalog()
            .await
            .unwrap_or_else(|_| std::sync::Arc::new(Value::Object(Default::default())));
        // A caller with its own model setting (the diff walkthrough) outranks
        // the small-model chain entirely — it asked for this model on purpose.
        let resolved = match override_model.and_then(parse_model_ref) {
            Some((provider_id, model_id)) => Some(ResolvedModel {
                provider_id,
                model_id,
                source: "request",
            }),
            None => resolve_small_model(ResolveParams {
                auth: &auth,
                catalog: &catalog,
                settings_small_model: read_small_model_settings_override(&self.settings_path)
                    .as_deref(),
                config_small_model: read_configured_small_model(&self.deps.config, directory)
                    .as_deref(),
                preferred_provider_id: preferred_provider_id.filter(|value| !value.is_empty()),
                preferred_model_id: preferred_model_id.filter(|value| !value.is_empty()),
            }),
        };
        let Some(resolved) = resolved else {
            return Ok(None);
        };

        let entry = catalog
            .get(&resolved.provider_id)
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(&resolved.model_id))
            .cloned()
            .unwrap_or(Value::Null);
        let output_token_limit = entry
            .get("limit")
            .and_then(|limit| limit.get("output"))
            .and_then(Value::as_u64)
            .filter(|limit| *limit > 0);
        // Two passes: the first only to learn the context, which a
        // caller-supplied reserve function needs before it can answer.
        let context =
            get_model_input_char_budget(&catalog, &resolved.provider_id, &resolved.model_id, None);
        let reserve_tokens = match output_reserve_tokens {
            OutputReserve::Tokens(value) => value,
            OutputReserve::FromLimits(function) => function(ReserveLimits {
                context_tokens: context.context_tokens,
                output_token_limit,
            }),
        };
        let budget = get_model_input_char_budget(
            &catalog,
            &resolved.provider_id,
            &resolved.model_id,
            reserve_tokens,
        );

        // Settings/config/request overrides can name a provider with no
        // usable login. Report that here so readiness can refuse before the
        // user pays for a 401.
        let has_login = resolve_provider_login(&self.deps, &auth, directory, &resolved.provider_id)
            .await
            .is_some();

        Ok(Some(DescribeResult {
            provider_id: resolved.provider_id,
            model_id: resolved.model_id,
            source: resolved.source,
            has_login,
            input_char_budget: budget.max_chars,
            context_tokens: context.context_tokens,
            context_known: context.context_known,
            // What the caller should ask for, so the request and the reserve
            // above cannot drift apart.
            output_tokens: reserve_tokens.filter(|value| *value > 0),
            structured_output: match entry.get("structured_output") {
                Some(Value::Bool(value)) => Some(*value),
                _ => None,
            },
            output_token_limit,
        }))
    }
}

/// The reserve a caller hands to [`SmallModelService::describe_small_model`]:
/// a number, or a function of the resolved model's limits for callers that
/// want as much answer room as the model allows.
pub enum OutputReserve {
    Tokens(Option<u64>),
    FromLimits(Arc<dyn Fn(ReserveLimits) -> Option<u64> + Send + Sync>),
}

impl Default for OutputReserve {
    fn default() -> Self {
        Self::Tokens(None)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReserveLimits {
    pub context_tokens: u64,
    pub output_token_limit: Option<u64>,
}

pub struct DescribeResult {
    pub provider_id: String,
    pub model_id: String,
    pub source: &'static str,
    pub has_login: bool,
    pub input_char_budget: u64,
    pub context_tokens: u64,
    pub context_known: bool,
    pub output_tokens: Option<u64>,
    pub structured_output: Option<bool>,
    pub output_token_limit: Option<u64>,
}

impl DescribeResult {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "providerID": self.provider_id,
            "modelID": self.model_id,
            "source": self.source,
            "hasLogin": self.has_login,
            "inputCharBudget": self.input_char_budget,
            "contextTokens": self.context_tokens,
            "contextKnown": self.context_known,
            "outputTokens": self.output_tokens,
            "structuredOutput": self.structured_output,
            "outputTokenLimit": self.output_token_limit,
        })
    }
}
