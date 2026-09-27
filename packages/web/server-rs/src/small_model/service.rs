//! Port of `server/lib/small-model/index.js`: orchestration —
//! `generateSmallModelText`, `describeSmallModel`, `listAuthenticatedProviders`
//! — plus the settings override, input clamping and output-budget rules.
//! 中文说明：本模块移植自 `server/lib/small-model/index.js`，承担编排
//! 职责——generateSmallModelText、describeSmallModel、
//! listAuthenticatedProviders 三大入口，外加设置覆盖、输入钳制与输出
//! 预算规则。

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
/// 中文说明：claude-code 无论传输层长什么样都绝不算小模型——插件可以
/// 为 Claude Code 发布 OpenAI 兼容端点，但那只是 Claude Agent SDK 的
/// 门面，每个请求都会拉起 Claude Code CLI 并消耗用户的订阅限额。
const CLAUDE_CODE_PROVIDER: &str = "claude-code";

// Rough safety clamp so a huge input never blows the model's context window.
// Token estimate is ~4 chars/token; when the catalog has no limit for the
// model (Copilot/codex utility models are not listed) a conservative default
// applies.
/// catalog 未提供上下文限制时的保守默认（Copilot/codex 工具模型未收录）。
const DEFAULT_CONTEXT_TOKENS: u64 = 64_000;
/// 输入预算中默认为回答保留的 token 数（与默认输出预算对齐）。
const OUTPUT_RESERVE_TOKENS: u64 = 4_000;
/// 粗略换算率：约 4 字符折 1 token。
const CHARS_PER_TOKEN: u64 = 4;
/// 输入预算下限：即使上下文极小也至少留 1000 token 给输入。
const MIN_INPUT_BUDGET_TOKENS: u64 = 1_000;

/// 输入超过模型上下文预算时的处置策略（对应 JS 的 onOverflow 参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// `truncate` (default): clip the tail and report `inputTruncated: true`.
    /// Correct for callers that degrade gracefully.
    /// 中文说明：截尾并报告 inputTruncated=true，适合能优雅降级的调用方。
    Truncate,
    /// `error`: throw a 413 `context-too-small`. Correct for callers whose
    /// output would be quietly wrong on a clipped input.
    /// 中文说明：抛 413 context-too-small；截断后输出会悄然出错的
    /// 调用方应选它。
    Error,
}

/// OMPChamber's own settings (Settings → Sessions → Small Model): when
/// `smallModelUseDefault` is false, `smallModelOverride` outranks every other
/// resolution step. Parse failures read as "no override".
/// 中文说明：OMPChamber 自身设置（Settings → Sessions → Small Model）：
/// smallModelUseDefault 为 false 时 smallModelOverride 压过一切解析
/// 步骤；解析失败视为"无覆盖"。
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
/// 中文说明：readConfiguredSmallModel——读合并配置层的 small_model 键。
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
/// 中文说明：给定调用方打算留给回答的空间后，输入侧可用的字符预算；
/// reserve 必须与调用方实际请求的输出预算一致，否则两侧预算会漂移。
pub struct InputCharBudget {
    /// 输入可用字符数（token 预算 × 4）；钳制以此为上限。
    pub max_chars: u64,
    /// 模型上下文 token 数（未知时为保守默认值）。
    pub context_tokens: u64,
    /// catalog 是否真的提供了 context 限制（影响上层提示口径）。
    pub context_known: bool,
}

/// 计算模型的输入字符预算：上下文减去输出保留（缺省 4000），下限
/// 1000 token，再按 4 字符/token 折算；catalog 无该模型时用保守默认
/// 上下文并标记 context_known=false。
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
/// 中文说明：实际请求的输出预算——取调用方要的值并用模型自述的
/// limit.output 封顶；多要会被某些 provider 直接拒绝、另一些静默忽略。
/// 传 None 或 0 表示不设预算（返回 None）。
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
/// 中文说明：clampPromptToModelLimit——输入在预算内原样通过；超预算时
/// 按策略截断（尾部加省略号并报告截断）或返回 413 context-too-small
/// （携带 required/available 字符数）。
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

/// generate 的入参集合；Default 全空 + Truncate 策略，对应 JS 的可选
/// 参数对象。
pub struct GenerateParams {
    /// 用户提示词；空白视为缺失，返回 400。
    pub prompt: Option<String>,
    /// 可选 system 指令；空白串视为无。
    pub system: Option<String>,
    /// 期望输出 token；由 resolve_output_tokens 用 catalog 封顶。
    pub max_output_tokens: Option<u64>,
    /// 显式 `provider/model` 引用；存在时跳过整个解析链（source=request）。
    pub model: Option<String>,
    /// 会话工作目录；配置层与相对路径解析以此为基准。
    pub directory: Option<String>,
    /// 会话偏好的 provider；配合 restrict 标志限制漂移。
    pub preferred_provider_id: Option<String>,
    /// 会话偏好的模型 id（解析链的提示，而非硬约束）。
    pub preferred_model_id: Option<String>,
    /// true 时禁止悄悄切到会话 provider 之外（显式选择仍允许）。
    pub restrict_to_preferred_provider: bool,
    /// 可选 JSON Schema；触发结构化输出路径。
    pub response_schema: Option<Value>,
    /// 单次调用超时（毫秒）；0/缺失用默认。
    pub timeout_ms: Option<u64>,
    /// 输入超预算时的处置策略。
    pub on_overflow: OverflowPolicy,
}

/// 全字段缺省的实现。
impl Default for GenerateParams {
    /// 生成默认参数：无提示、无模型引用，溢出策略为截断。
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

/// generate 的结果：生成文本与实际使用的模型信息（供前端展示与诊断）。
#[derive(Debug, Clone)]
pub struct GenerateOutput {
    /// 生成的文本（已 trim）。
    pub text: String,
    /// 实际使用的 provider id。
    pub provider_id: String,
    /// 实际使用的模型 id。
    pub model_id: String,
    /// 模型解析来源（settings/config/request/preferred/auto）。
    pub source: String,
    /// 输入被截断时为 Some(true)，否则 None（序列化时省略该键）。
    pub input_truncated: Option<bool>,
}

/// 结果到 JS 形状 JSON 的序列化。
impl GenerateOutput {
    /// 转成 JS 端期望的对象；inputTruncated 仅在真截断时出现。
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
/// 中文说明：小模型服务——每台 server 一个实例（JS 的模块级状态都
/// 装在这里）。
pub struct SmallModelService {
    /// 调用层依赖（fetch/配置/auth 存储/运行时快照）。
    pub deps: Arc<CallDeps>,
    /// models-dev catalog 缓存（磁盘缓存 + 刷新）。
    pub catalog: Arc<CatalogCache>,
    /// OMPChamber settings.json 路径（读取小模型覆盖）。
    pub settings_path: std::path::PathBuf,
}

/// 服务的构造与三大入口（generate / 列出已认证 provider / describe）。
impl SmallModelService {
    /// Production wiring over the real filesystem and engine.
    /// 中文说明：生产装配——真实文件系统与引擎（reqwest fetch、
    /// FsAuthStore、引擎派生的运行时快照）。
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
    /// 中文说明：测试装配——全部依赖注入。
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
    /// 中文说明：generateSmallModelText——完全在服务端从 OpenCode 配置
    /// 与 auth 存储完成解析和鉴权。空 prompt 400；解析不出模型 404；
    /// 命中 claude-code 422；restrict 模式下漂移到会话 provider 之外
    /// 404。输出预算经 catalog 封顶，输入按策略钳制后调用
    /// call_small_model。
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
    /// 中文说明：listAuthenticatedProviders——本模块实际可调用的
    /// provider：auth.json 登录，或 OpenCode 运行时为插件 provider 解析
    /// 出的凭据+端点（无凭据、zen 哨兵、专用线格式与 claude-code 均排除；
    /// 运行时查询失败只损失它本可新增的条目，不影响已确立的磁盘登录）。
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
    /// 中文说明：describeSmallModel——只报告将使用哪个模型，不真正调用。
    /// 两遍预算计算：第一遍只为拿到上下文，供调用方的 reserve 函数
    /// 决策；has_login 让 readiness 在用户付出 401 之前就能拒绝。
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
/// 中文说明：调用方交给 describe 的输出保留——一个数值，或一个根据
/// 解析出的模型限制回答的函数（想要多大回答空间就留多大）。
pub enum OutputReserve {
    /// 固定 token 数；None 表示用默认保留。
    Tokens(Option<u64>),
    /// 由模型限制（上下文/输出上限）推算保留的回调函数。
    FromLimits(Arc<dyn Fn(ReserveLimits) -> Option<u64> + Send + Sync>),
}

/// OutputReserve 的缺省值。
impl Default for OutputReserve {
    /// 默认不指定保留 token（Tokens(None)）。
    fn default() -> Self {
        Self::Tokens(None)
    }
}

/// 传给 FromLimits 回调的模型限制快照。
#[derive(Debug, Clone, Copy)]
pub struct ReserveLimits {
    /// 模型上下文 token 数（未知时为保守默认）。
    pub context_tokens: u64,
    /// catalog 自述的输出 token 上限；未收录为 None。
    pub output_token_limit: Option<u64>,
}

/// describe 的结果：解析出的模型、登录可用性与预算口径（供 readiness
/// 与 diff 演练等调用方决策）。
pub struct DescribeResult {
    /// 解析出的 provider id。
    pub provider_id: String,
    /// 解析出的模型 id。
    pub model_id: String,
    /// 解析来源（settings/config/request/preferred/auto）。
    pub source: &'static str,
    /// 该 provider 当前是否有可用登录（false 时请求会 401）。
    pub has_login: bool,
    /// 按最终 reserve 计算的输入字符预算。
    pub input_char_budget: u64,
    /// 模型上下文 token 数（未知时为保守默认）。
    pub context_tokens: u64,
    /// catalog 是否提供了 context 限制。
    pub context_known: bool,
    /// 调用方应实际请求的输出 token（与上述 reserve 对齐、不漂移）。
    pub output_tokens: Option<u64>,
    /// catalog 是否声明支持结构化输出；未声明为 None。
    pub structured_output: Option<bool>,
    /// catalog 自述的输出 token 上限；未收录为 None。
    pub output_token_limit: Option<u64>,
}

/// 结果到 JS 形状 JSON 的序列化。
impl DescribeResult {
    /// 转成 JS 端期望的驼峰键对象（providerID/inputCharBudget 等）。
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
