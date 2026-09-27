//! Port of `server/lib/small-model/resolve.js`: model selection mirroring
//! OpenCode's `getSmallModel` fallback chain.
//!
//! 中文说明：`server/lib/small-model/resolve.js` 的移植——按 OpenCode
//! getSmallModel 的 fallback 链选择小模型。

use serde_json::Value;

/// Mirrors OpenCode's getSmallModel fallback chain:
/// 1. `small_model` from the merged config layers ("provider/model").
/// 2. GitHub Copilot's hidden utility models when Copilot is logged in.
/// 3. Family-priority scan of the authenticated providers' catalog models.
///
/// 中文补充：家族扫描顺序从高到低为 gemini-flash、gpt-nano、claude-haiku。
const FAMILY_PRIORITY: [&str; 3] = ["gemini-flash", "gpt-nano", "claude-haiku"];
/// GitHub Copilot 的隐藏工具模型清单（不出现在目录中）；取首个作为小模型。
const COPILOT_UTILITY_MODELS: [&str; 4] = ["gpt-5.4-nano", "gpt-4.1", "gpt-4o", "gpt-4o-mini"];
/// The ChatGPT-plan codex backend only accepts a small allowlist of models
/// (nano/API-key models are rejected with 400) — this is its cheapest one.
///
/// 中文补充：ChatGPT 套餐的 codex 后端只接受一个小模型白名单（nano 与 API-key
/// 模型会被 400 拒绝），这是其中最便宜的一个。
const OPENAI_OAUTH_SMALL_MODEL: &str = "gpt-5.4-mini";

/// 返回 provider 在 auth.json 中可能出现的键名序列（含历史别名，如裸的 copilot）。
fn auth_provider_aliases(provider_id: &str) -> Vec<&str> {
    match provider_id {
        "github-copilot" => vec!["github-copilot", "copilot"],
        _ => vec![provider_id],
    }
}

/// `getAuthEntryForProvider`: alias-aware auth.json lookup (legacy entries
/// may sit under the bare `copilot` key).
///
/// 中文补充：按别名顺序在 auth 顶层查找第一个对象型条目。
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

/// Some 且非空字符串时才返回 true。
fn non_empty_string(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.is_empty())
}

/// `isUsableAuthEntry`.
///
/// 中文补充：判断 auth 条目是否携带可用凭据（type/key/access token 等）。
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
///
/// 中文补充：按首个斜杠拆分为 (provider, model)；没有斜杠则返回 None。
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
///
/// 中文补充：同族模型按 release_date 降序取最新（JS 用字符串 localeCompare，
/// 此处为字典序比较）。
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
///
/// 中文补充：目录顶层的对象型条目才有效，否则视为不存在。
pub fn get_catalog_provider<'a>(catalog: &'a Value, provider_id: &str) -> Option<&'a Value> {
    let entry = catalog.get(provider_id)?;
    entry.as_object().map(|_| entry)
}

/// 解析出的小模型：provider/model 标识加上说明其来源的静态标签。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// provider 标识（auth.json / 目录中的键名）。
    pub provider_id: String,
    /// 模型 ID。
    pub model_id: String,
    /// 来源标签（settings/config/session-model/codex-small/copilot-utility/family-scan）。
    pub source: &'static str,
}

/// Small-model candidates within ONE provider, by family priority. Copilot and
/// ChatGPT-plan OpenAI have fixed small models that never appear in the
/// catalog; everyone else is scanned through the catalog families.
///
/// 中文补充：Copilot 与 ChatGPT 套餐 OpenAI 的小模型是固定的（不查目录），
/// 其余 provider 走目录的家族扫描。
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

/// 解析入参：auth.json 与目录快照，加上各覆盖层与 session 上下文。
pub struct ResolveParams<'a> {
    /// ~/.local/share/opencode/auth.json 的内容。
    pub auth: &'a Value,
    /// models.dev 目录快照。
    pub catalog: &'a Value,
    /// OMPChamber 设置中的小模型覆盖，优先级最高。
    pub settings_small_model: Option<&'a str>,
    /// OpenCode 合并配置里的 small_model。
    pub config_small_model: Option<&'a str>,
    /// session 所在 provider；有 session 上下文时优先在它内部选。
    pub preferred_provider_id: Option<&'a str>,
    /// session 当前模型 ID；provider 内家族扫描全部落空时的兜底。
    pub preferred_model_id: Option<&'a str>,
}

/// `resolveSmallModel`.
///
/// 中文补充：优先级依次为——设置覆盖 → OpenCode 配置 → session provider 的家族
/// 扫描/当前模型 → 全部已认证 provider 的家族扫描 → Copilot 工具模型兜底；
/// 全部落空返回 None。
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
