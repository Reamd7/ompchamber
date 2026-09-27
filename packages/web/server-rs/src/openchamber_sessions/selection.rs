//! Port of the selection plumbing in `openchamber-sessions/routes.js`:
//! provider/model/variant lookup helpers, `fetchSelectionInputs`
//! (`/config/providers` + `/agent` + `/config`), `resolveDefaultSelection`,
//! and `validateRequestedSelection`.
//!
//! 中文说明：移植 `openchamber-sessions/routes.js` 中的 selection 管道——
//! provider/model/variant 查找工具、`fetchSelectionInputs`（并发拉取
//! `/config/providers`、`/agent`、`/config`，各自降级回退）、
//! `resolveDefaultSelection` 默认值推导与 `validateRequestedSelection`
//! 请求前校验。

use serde_json::{Map, Value};

use super::client::EngineClient;
use super::error::SvcError;
use super::payload::{ModelRef, as_non_empty_string};

/// 默认模型兜底 provider ID（JS 同名常量 `'opencode'`）。
pub const FALLBACK_PROVIDER_ID: &str = "opencode";
/// 默认模型兜底 model ID（JS 同名常量 `'big-pickle'`）。
pub const FALLBACK_MODEL_ID: &str = "big-pickle";

/// JS `isPrimaryAgentMode`: missing/`'primary'`/`'all'`.
///
/// 中文说明：mode 缺失/null/空串或取值 `'primary'`/`'all'` 视为主 agent；
/// 其余（如 `'subagent'`）不是。
fn is_primary_agent_mode(mode: Option<&Value>) -> bool {
    match mode {
        None | Some(Value::Null) => true,
        Some(Value::String(mode)) => mode.is_empty() || mode == "primary" || mode == "all",
        _ => true,
    }
}

/// JS `providerModels`: `models` is either an array or an id→model map.
///
/// 中文说明：provider 的 `models` 既可能是数组也可能是 id→model 对象，
/// 统一收敛为模型列表（注意：对象形式按 key 排序遍历，与 JS 保持插入
/// 顺序的行为不同）。
fn provider_models(provider: Option<&Value>) -> Vec<&Value> {
    let Some(models) = provider.and_then(|p| p.get("models")) else {
        return Vec::new();
    };
    match models {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => Vec::new(),
    }
}

/// JS `hasProviderModel`.
///
/// 中文说明：provider 与 model 必须同时真实存在才返回 true。
pub fn has_provider_model(providers: &[Value], provider_id: &str, model_id: &str) -> bool {
    providers.iter().any(|provider| {
        provider.get("id").and_then(Value::as_str) == Some(provider_id)
            && provider_models(Some(provider))
                .iter()
                .any(|model| model.get("id").and_then(Value::as_str) == Some(model_id))
    })
}

/// JS `resolveVariant`: the requested variant only survives when the model
/// declares it (`hasOwnProperty` on the variants object).
///
/// 中文说明：请求的 variant 仅在该 model 的 `variants` 对象中声明时才保留
/// （对齐 JS `hasOwnProperty` 语义），否则丢弃返回 `None`。
pub fn resolve_variant(
    providers: &[Value],
    provider_id: &str,
    model_id: &str,
    variant: Option<&str>,
) -> Option<String> {
    let normalized = as_non_empty_string(
        variant
            .map(|value| Value::String(value.to_string()))
            .as_ref(),
    )?;
    let provider = providers
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(provider_id));
    let model = provider_models(provider)
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model_id));
    let variants = model?.get("variants")?;
    variants.get(&normalized)?;
    Some(normalized)
}

/// JS `parseConfigModel` = `splitModel` over a raw string.
///
/// 中文说明：直接对原始字符串调用 `split_model` 拆出 provider/model。
fn parse_config_model(value: Option<&Value>) -> Option<ModelRef> {
    super::payload::split_model(value)
}

/// `fetchSelectionInputs` output.
///
/// 中文说明：selection 推导的全部输入——settings 持久化默认值、providers
/// 与 agents 列表、opencode 配置级默认 agent/model（各端点失败时回退为空）。
pub struct SelectionInputs {
    /// Persisted defaults actually consumed by selection: `defaultAgent`,
    /// `defaultModel`, `defaultVariant` (raw strings, may be absent).
    /// 被 selection 消费的持久化默认值键：`defaultAgent`/`defaultModel`/
    /// `defaultVariant`（原始字符串，可缺失）。
    pub settings: Map<String, Value>,
    /// `/config/providers` 返回的 provider 列表（含各自 models）。
    pub providers: Vec<Value>,
    /// `/agent` 返回的 agent 定义列表。
    pub agents: Vec<Value>,
    /// `/config` 的 `default_agent`（兼容驼峰 `defaultAgent`）。
    pub opencode_default_agent: Option<String>,
    /// `/config` 的 `model`（opencode 配置级默认模型）。
    pub opencode_default_model: Option<String>,
}

/// JS `fetchSelectionInputs`: settings read plus the three config endpoints
/// fetched concurrently, each degrading to its fallback.
///
/// 中文说明：读取 settings 之外，用 `tokio::join!` 并发拉取三个配置端点；
/// 任一端点失败各自降级（空 providers / 空 agents / 空 config），不阻塞
/// selection 流程。
pub async fn fetch_selection_inputs(
    client: &dyn EngineClient,
    directory: &str,
    settings: Map<String, Value>,
) -> SelectionInputs {
    let providers_fallback = serde_json::json!({ "providers": [] });
    let (providers_body, agents_body, config_body) = tokio::join!(
        client.fetch_json("/config/providers", directory, providers_fallback),
        client.fetch_json("/agent", directory, Value::Array(Vec::new())),
        client.fetch_json("/config", directory, Value::Object(Map::new())),
    );
    let providers = providers_body
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agents = agents_body.as_array().cloned().unwrap_or_default();
    let opencode_default_agent = as_non_empty_string(config_body.get("default_agent"))
        .or_else(|| as_non_empty_string(config_body.get("defaultAgent")));
    let opencode_default_model = as_non_empty_string(config_body.get("model"));
    SelectionInputs {
        settings,
        providers,
        agents,
        opencode_default_agent,
        opencode_default_model,
    }
}

/// A resolved agent/model/variant triple — `variant` is `undefined` when
/// absent (JS leaves the key out of dispatch payloads).
///
/// 中文说明：默认选择三元组；variant 缺失为 `None`（JS 侧是 `undefined`，
/// 派发 payload 会省略该键）。
#[derive(Debug, Clone, Default)]
pub struct DefaultSelection {
    /// 选中的 agent 名（`name` 字段）。
    pub agent: Option<String>,
    /// 选中的模型引用。
    pub model: Option<ModelRef>,
    /// 选中的模型 variant。
    pub variant: Option<String>,
}

/// JS `resolveDefaultSelection`: settings default agent → opencode default
/// agent (when primary+visible) → `'build'` → first primary → first agent;
/// model from settings default (validated, variant resolved) → agent model →
/// opencode config model → the `opencode/big-pickle` fallback → first
/// provider's first model.
///
/// 中文说明：多级回退推导——agent 依次尝试 settings 默认、opencode 默认
/// （需 primary 且未隐藏）、`'build'`、首个 primary、首个 agent；model 依次
/// 尝试 settings 默认（需真实存在且 variant 有声明）、agent 自带 model、
/// opencode 配置 model、`opencode/big-pickle` 兜底、首个 provider 的首个
/// model。
pub fn resolve_default_selection(inputs: &SelectionInputs) -> DefaultSelection {
    let primary_agents: Vec<&Value> = inputs
        .agents
        .iter()
        .filter(|agent| {
            is_primary_agent_mode(agent.get("mode"))
                && agent.get("hidden") != Some(&Value::Bool(true))
        })
        .collect();

    let find_agent = |name: &str| -> Option<&Value> {
        inputs
            .agents
            .iter()
            .find(|agent| agent.get("name").and_then(Value::as_str) == Some(name))
    };

    let mut resolved_agent: Option<&Value> = None;
    if let Some(settings_default) = as_non_empty_string(inputs.settings.get("defaultAgent")) {
        resolved_agent = find_agent(&settings_default);
    }
    if resolved_agent.is_none()
        && let Some(opencode_default) = &inputs.opencode_default_agent
    {
        let candidate = find_agent(opencode_default);
        if candidate.is_some_and(|candidate| {
            is_primary_agent_mode(candidate.get("mode"))
                && candidate.get("hidden") != Some(&Value::Bool(true))
        }) {
            resolved_agent = candidate;
        }
    }
    if resolved_agent.is_none() {
        resolved_agent = primary_agents
            .iter()
            .find(|agent| agent.get("name").and_then(Value::as_str) == Some("build"))
            .copied()
            .or_else(|| primary_agents.first().copied())
            .or_else(|| inputs.agents.first());
    }

    let mut model: Option<ModelRef> = None;
    let mut variant: Option<String> = None;
    let settings_default_model = parse_config_model(inputs.settings.get("defaultModel"));
    if let Some(candidate) = settings_default_model.filter(|candidate| {
        has_provider_model(
            &inputs.providers,
            &candidate.provider_id,
            &candidate.model_id,
        )
    }) {
        variant = resolve_variant(
            &inputs.providers,
            &candidate.provider_id,
            &candidate.model_id,
            inputs
                .settings
                .get("defaultVariant")
                .and_then(Value::as_str),
        );
        model = Some(candidate);
    }

    if model.is_none() {
        let agent_model = resolved_agent.and_then(|agent| {
            let provider_id = agent
                .get("model")
                .and_then(|m| m.get("providerID"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())?;
            let model_id = agent
                .get("model")
                .and_then(|m| m.get("modelID"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())?;
            Some(ModelRef {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
            })
        });
        if let Some(candidate) = agent_model.filter(|candidate| {
            has_provider_model(
                &inputs.providers,
                &candidate.provider_id,
                &candidate.model_id,
            )
        }) {
            variant = resolve_variant(
                &inputs.providers,
                &candidate.provider_id,
                &candidate.model_id,
                resolved_agent
                    .and_then(|agent| agent.get("variant"))
                    .and_then(Value::as_str),
            );
            model = Some(candidate);
        }
    }

    if model.is_none() {
        let opencode_model = inputs
            .opencode_default_model
            .as_deref()
            .map(|value| Value::String(value.to_string()))
            .as_ref()
            .and_then(|value| parse_config_model(Some(value)));
        if let Some(candidate) = opencode_model.filter(|candidate| {
            has_provider_model(
                &inputs.providers,
                &candidate.provider_id,
                &candidate.model_id,
            )
        }) {
            model = Some(candidate);
        }
    }

    if model.is_none()
        && has_provider_model(&inputs.providers, FALLBACK_PROVIDER_ID, FALLBACK_MODEL_ID)
    {
        model = Some(ModelRef {
            provider_id: FALLBACK_PROVIDER_ID.to_string(),
            model_id: FALLBACK_MODEL_ID.to_string(),
        });
    }

    if model.is_none() {
        let provider = inputs.providers.first();
        let first_model = provider_models(provider).into_iter().next();
        if let (Some(provider_id), Some(model_id)) = (
            provider
                .and_then(|provider| provider.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty()),
            first_model
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty()),
        ) {
            model = Some(ModelRef {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
            });
        }
    }

    DefaultSelection {
        agent: resolved_agent
            .and_then(|agent| agent.get("name"))
            .and_then(Value::as_str)
            .map(String::from),
        model,
        variant,
    }
}

/// JS `validateRequestedSelection` — reject unknown agents/models/variants
/// before any session, worktree, or goal side effect. Empty lookups never
/// reject (a failed config fetch must not block a valid selection).
///
/// 中文说明：在产生任何会话/worktree/goal 副作用之前校验请求的
/// agent/model/variant（未知项 400）；agents 或 providers 为空（配置拉取
/// 失败的回退态）时跳过对应校验，避免误拒合法选择。
pub async fn validate_requested_selection(
    client: &dyn EngineClient,
    settings: Map<String, Value>,
    directory: &str,
    requested_model: Option<&ModelRef>,
    requested_agent: Option<&str>,
    requested_variant: Option<&str>,
) -> Result<(), SvcError> {
    if requested_model.is_none() && requested_agent.is_none() && requested_variant.is_none() {
        return Ok(());
    }
    let inputs = fetch_selection_inputs(client, directory, settings).await;

    if let Some(agent_name) = requested_agent
        && !inputs.agents.is_empty()
    {
        let agent = inputs
            .agents
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(agent_name));
        let Some(agent) = agent else {
            return Err(SvcError::control(
                format!("Unknown agent '{agent_name}' for {directory}"),
                400,
            ));
        };
        if !is_primary_agent_mode(agent.get("mode")) {
            return Err(SvcError::control(
                format!("Agent '{agent_name}' is a subagent and cannot receive a prompt directly"),
                400,
            ));
        }
    }

    if let Some(model) = requested_model
        && !inputs.providers.is_empty()
    {
        if !has_provider_model(&inputs.providers, &model.provider_id, &model.model_id) {
            return Err(SvcError::control(
                format!(
                    "Unknown model '{}/{}' for {directory}",
                    model.provider_id, model.model_id
                ),
                400,
            ));
        }
        if let Some(variant) = requested_variant
            && resolve_variant(
                &inputs.providers,
                &model.provider_id,
                &model.model_id,
                Some(variant),
            )
            .is_none()
        {
            return Err(SvcError::control(
                format!(
                    "Unknown variant '{variant}' for model '{}/{}'",
                    model.provider_id, model.model_id
                ),
                400,
            ));
        }
    }

    Ok(())
}

/// 默认选择推导优先级与 variant 丢弃规则的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试辅助：由 agents/providers/settings 三个 JSON 构造 SelectionInputs
    /// （opencode 级默认留空）。
    fn inputs(agents: Value, providers: Value, settings: Value) -> SelectionInputs {
        SelectionInputs {
            settings: settings.as_object().cloned().unwrap_or_default(),
            providers: providers.as_array().cloned().unwrap_or_default(),
            agents: agents.as_array().cloned().unwrap_or_default(),
            opencode_default_agent: None,
            opencode_default_model: None,
        }
    }

    /// 验证默认选择优先级：settings 默认（含 variant）> agent 自带 model >
    /// 首个 provider 的首个 model。
    #[test]
    fn default_selection_prefers_settings_then_agent_model_then_first() {
        let providers = json!([
            { "id": "openai", "models": { "gpt-5.5": { "id": "gpt-5.5" }, "default": { "id": "default", "variants": { "high": {} } } } },
            { "id": "anthropic", "models": [ { "id": "claude-sonnet-5" } ] },
        ]);
        let agents = json!([
            { "name": "build", "mode": "primary", "model": { "providerID": "anthropic", "modelID": "claude-sonnet-5" } },
            { "name": "sub", "mode": "subagent" },
            { "name": "hidden", "mode": "primary", "hidden": true },
        ]);

        // Settings default wins and carries the settings variant.
        let selection = resolve_default_selection(&inputs(
            agents.clone(),
            providers.clone(),
            json!({ "defaultModel": "openai/default", "defaultVariant": "high", "defaultAgent": "sub" }),
        ));
        // `defaultAgent: "sub"` matches by name even though it is a subagent
        // (JS only applies the primary filter to the opencode-config agent).
        assert_eq!(selection.agent.as_deref(), Some("sub"));
        assert_eq!(
            selection.model.map(|m| m.to_slash_form()),
            Some("openai/default".to_string())
        );
        assert_eq!(selection.variant.as_deref(), Some("high"));

        // No settings: agent's own model + the agent's variant.
        let selection =
            resolve_default_selection(&inputs(agents.clone(), providers.clone(), json!({})));
        assert_eq!(selection.agent.as_deref(), Some("build"));
        assert_eq!(
            selection.model.map(|m| m.to_slash_form()),
            Some("anthropic/claude-sonnet-5".to_string())
        );
        assert_eq!(selection.variant, None);

        // No agent model available: first provider/model (array-form models,
        // since serde_json's object keys sort while JS keeps insertion order).
        let providers = json!([
            { "id": "openai", "models": [ { "id": "gpt-5.5" } ] },
            { "id": "anthropic", "models": [ { "id": "claude-sonnet-5" } ] },
        ]);
        let selection = resolve_default_selection(&inputs(
            json!([{ "name": "build", "mode": "primary" }]),
            providers,
            json!({}),
        ));
        assert_eq!(
            selection.model.map(|m| m.to_slash_form()),
            Some("openai/gpt-5.5".to_string())
        );
    }

    /// 验证 `opencode/big-pickle` 兜底仅在 provider 实际列出该模型时生效；
    /// 空配置时 model/agent 均为 `None`。
    #[test]
    fn fallback_model_used_when_provider_lists_it() {
        let mut case = inputs(
            json!([{ "name": "build", "mode": "primary" }]),
            json!([{ "id": "opencode", "models": [ { "id": "big-pickle" } ] }]),
            json!({}),
        );
        let selection = resolve_default_selection(&case);
        assert_eq!(
            selection.model.map(|m| m.to_slash_form()),
            Some("opencode/big-pickle".to_string())
        );

        case = inputs(json!([]), json!([]), json!({}));
        let selection = resolve_default_selection(&case);
        assert_eq!(selection.model, None);
        assert_eq!(selection.agent, None);
    }

    /// 验证默认值路径中未声明的 variant 被静默丢弃而不是报错。
    #[test]
    fn unknown_variant_is_dropped_not_fatal_in_defaults() {
        let providers = json!([
            { "id": "openai", "models": [ { "id": "gpt-5.5", "variants": { "high": {} } } ] },
        ]);
        let selection = resolve_default_selection(&inputs(
            json!([{ "name": "build", "mode": "primary" }]),
            providers,
            json!({ "defaultModel": "openai/gpt-5.5", "defaultVariant": "ultra" }),
        ));
        assert_eq!(
            selection.model.map(|m| m.to_slash_form()),
            Some("openai/gpt-5.5".to_string())
        );
        assert_eq!(selection.variant, None);
    }
}
