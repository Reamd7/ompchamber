//! Port of `server/lib/small-model/resolve.js`: model selection mirroring
//! OpenCode's `getSmallModel` fallback chain.

use serde_json::Value;

/// Mirrors OpenCode's getSmallModel fallback chain:
/// 1. `small_model` from the merged config layers ("provider/model").
/// 2. GitHub Copilot's hidden utility models when Copilot is logged in.
/// 3. Family-priority scan of the authenticated providers' catalog models.
const FAMILY_PRIORITY: [&str; 3] = ["gemini-flash", "gpt-nano", "claude-haiku"];
const COPILOT_UTILITY_MODELS: [&str; 4] = ["gpt-5.4-nano", "gpt-4.1", "gpt-4o", "gpt-4o-mini"];
/// The ChatGPT-plan codex backend only accepts a small allowlist of models
/// (nano/API-key models are rejected with 400) — this is its cheapest one.
const OPENAI_OAUTH_SMALL_MODEL: &str = "gpt-5.4-mini";

fn auth_provider_aliases(provider_id: &str) -> Vec<&str> {
    match provider_id {
        "github-copilot" => vec!["github-copilot", "copilot"],
        _ => vec![provider_id],
    }
}

/// `getAuthEntryForProvider`: alias-aware auth.json lookup (legacy entries
/// may sit under the bare `copilot` key).
pub fn get_auth_entry_for_provider<'a>(auth: &'a Value, provider_id: &str) -> Option<&'a Value> {
    auth.as_object()?;
    for alias in auth_provider_aliases(provider_id) {
        if let Some(entry) = auth.get(alias)
            && entry.as_object().is_some()
        {
            return Some(entry);
        }
    }
    None
}

fn non_empty_string(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.is_empty())
}

/// `isUsableAuthEntry`.
pub fn is_usable_auth_entry(entry: &Value) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "api" => non_empty_string(object.get("key").and_then(Value::as_str)),
        "oauth" => {
            non_empty_string(object.get("access").and_then(Value::as_str))
                || non_empty_string(object.get("refresh").and_then(Value::as_str))
        }
        "wellknown" => non_empty_string(object.get("token").and_then(Value::as_str)),
        _ => false,
    }
}

/// `parseModelRef`: `provider/model` split on the first slash.
pub fn parse_model_ref(value: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    let slash = trimmed.find('/')?;
    if slash == 0 || slash == trimmed.len() - 1 {
        return None;
    }
    Some((
        trimmed[..slash].to_string(),
        trimmed[slash + 1..].to_string(),
    ))
}

/// Sorted matches by `release_date` descending (JS `String.localeCompare`
/// on the raw strings; lexicographic here), newest first.
fn pick_by_family<'a>(
    models: &'a serde_json::Map<String, Value>,
    family: &str,
) -> Option<&'a Value> {
    let mut matches: Vec<&Value> = models
        .values()
        .filter(|model| {
            model
                .as_object()
                .is_some_and(|object| object.get("family").and_then(Value::as_str) == Some(family))
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    matches.sort_by(|a, b| {
        let a_date = a.get("release_date").and_then(Value::as_str).unwrap_or("");
        let b_date = b.get("release_date").and_then(Value::as_str).unwrap_or("");
        b_date.cmp(a_date)
    });
    matches.into_iter().next()
}

/// `getCatalogProvider`: object entry or null.
pub fn get_catalog_provider<'a>(catalog: &'a Value, provider_id: &str) -> Option<&'a Value> {
    let entry = catalog.get(provider_id)?;
    entry.as_object().map(|_| entry)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model_id: String,
    pub source: &'static str,
}

/// Small-model candidates within ONE provider, by family priority. Copilot and
/// ChatGPT-plan OpenAI have fixed small models that never appear in the
/// catalog; everyone else is scanned through the catalog families.
fn pick_within_provider(
    provider_id: &str,
    auth: &Value,
    catalog: &Value,
    family: &str,
) -> Option<ResolvedModel> {
    if provider_id == "openai"
        && auth
            .get("openai")
            .and_then(|entry| entry.get("type"))
            .and_then(Value::as_str)
            == Some("oauth")
    {
        return (family == "gpt-nano").then(|| ResolvedModel {
            provider_id: provider_id.to_string(),
            model_id: OPENAI_OAUTH_SMALL_MODEL.to_string(),
            source: "codex-small",
        });
    }
    if provider_id == "github-copilot" {
        return (family == "gpt-nano").then(|| ResolvedModel {
            provider_id: provider_id.to_string(),
            model_id: COPILOT_UTILITY_MODELS[0].to_string(),
            source: "copilot-utility",
        });
    }
    let provider = get_catalog_provider(catalog, provider_id)?;
    let models = provider.get("models")?.as_object()?;
    let model = pick_by_family(models, family)?;
    let model_id = model.get("id").and_then(Value::as_str)?;
    if model_id.is_empty() {
        return None;
    }
    Some(ResolvedModel {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        source: "family-scan",
    })
}

pub struct ResolveParams<'a> {
    pub auth: &'a Value,
    pub catalog: &'a Value,
    pub settings_small_model: Option<&'a str>,
    pub config_small_model: Option<&'a str>,
    pub preferred_provider_id: Option<&'a str>,
    pub preferred_model_id: Option<&'a str>,
}

/// `resolveSmallModel`.
pub fn resolve_small_model(params: ResolveParams<'_>) -> Option<ResolvedModel> {
    let ResolveParams {
        auth,
        catalog,
        settings_small_model,
        config_small_model,
        preferred_provider_id,
        preferred_model_id,
    } = params;

    // OMPChamber's own setting (Settings → Sessions → Small Model override)
    // outranks everything, including the OpenCode config.
    if let Some(value) = settings_small_model
        && let Some((provider_id, model_id)) = parse_model_ref(value)
    {
        return Some(ResolvedModel {
            provider_id,
            model_id,
            source: "settings",
        });
    }

    if let Some(value) = config_small_model
        && let Some((provider_id, model_id)) = parse_model_ref(value)
    {
        return Some(ResolvedModel {
            provider_id,
            model_id,
            source: "config",
        });
    }

    // Like OpenCode: when the caller has a session context, the utility call
    // stays on the session's provider. Scan its families for a small model,
    // otherwise run on the session's own model — never silently switch to a
    // different provider's subscription.
    let preferred = preferred_provider_id.filter(|value| !value.is_empty());
    if let Some(preferred) = preferred
        && is_usable_auth_entry(
            get_auth_entry_for_provider(auth, preferred).unwrap_or(&Value::Null),
        )
    {
        for family in FAMILY_PRIORITY {
            if let Some(found) = pick_within_provider(preferred, auth, catalog, family) {
                return Some(found);
            }
        }
        if let Some(model_id) = preferred_model_id.filter(|value| !value.is_empty()) {
            return Some(ResolvedModel {
                provider_id: preferred.to_string(),
                model_id: model_id.to_string(),
                source: "session-model",
            });
        }
    }

    // No session context (or its provider has no usable login): scan all
    // authenticated providers by family priority.
    let authed_providers: Vec<&String> = auth
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|provider_id| {
                    **provider_id != preferred.unwrap_or("")
                        && is_usable_auth_entry(&object[*provider_id])
                })
                .collect()
        })
        .unwrap_or_default();

    for family in FAMILY_PRIORITY {
        for provider_id in &authed_providers {
            if let Some(found) = pick_within_provider(provider_id, auth, catalog, family) {
                return Some(found);
            }
        }
    }

    // Copilot's utility fallback for legacy auth aliases the loop above missed.
    let copilot_entry = get_auth_entry_for_provider(auth, "github-copilot");
    if copilot_entry.is_some_and(is_usable_auth_entry) {
        return Some(ResolvedModel {
            provider_id: "github-copilot".to_string(),
            model_id: COPILOT_UTILITY_MODELS[0].to_string(),
            source: "copilot-utility",
        });
    }

    None
}
