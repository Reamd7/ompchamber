//! Port of `server/lib/quota/providers/zhipuai-coding-plan.js` — Zhipu AI
//! Coding Plan via `GET https://open.bigmodel.cn/api/monitor/usage/quota/limit`.
//!
//! Auth resolution: OpenCode `auth.json` entry first, then the merged OpenCode
//! config layers' `provider[alias].options.apiKey`.
//!
//! 中文概览：智谱 AI Coding Plan 配额提供方——以 API key 请求
//! open.bigmodel.cn 的 quota/limit 端点，把 limits 中的 TOKENS_LIMIT
//! （5 小时 token 窗口）与 TIME_LIMIT（固定 30 天的 "MCP Tools" 窗口）
//! 转换成统一的用量窗口。

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::{QuotaDeps, read_opencode_user_config};
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, normalize_timestamp,
    resolve_window_seconds, to_usage_window, usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "zhipuai-coding-plan";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "Zhipu AI Coding Plan";
/// auth.json 中识别本提供方的别名列表（含简写 zhipuai/zhipu）。
pub const ALIASES: [&str; 3] = ["zhipuai-coding-plan", "zhipuai", "zhipu"];

/// 配额限额查询端点 URL。
const LIMIT_URL: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";
/// "MCP Tools" 窗口使用的固定时长（30 天，API 未返回窗口秒数）。
const MONTH_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

/// 解析 API key：优先取 auth.json 别名条目的 key/token 字段，
/// 否则回退到合并后的 OpenCode 配置层 provider[alias].options.apiKey
/// （回退层吞掉错误）；auth 文件本身读取失败则向上返回 Err。
fn get_api_key(deps: &QuotaDeps) -> Result<Option<String>, String> {
    // JS: readAuthFile() runs outside the try — a read error propagates to
    // the registry catch; only the config-layer fallback swallows errors.
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    if let Some(key) = entry.and_then(|entry| {
        field(&entry, "key")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }) {
        return Ok(Some(key));
    }

    // `readConfigLayers().mergedConfig.provider[alias].options.apiKey`
    if let Some(merged) = read_opencode_user_config(deps) {
        for alias in ALIASES {
            let api_key = merged
                .get("provider")
                .and_then(Value::as_object)
                .and_then(|providers| providers.get(alias))
                .and_then(|provider| field(provider, "options"))
                .and_then(|options| field(options, "apiKey"))
                .and_then(Value::as_str);
            if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
                return Ok(Some(api_key.to_string()));
            }
        }
    }
    Ok(None)
}

/// 是否已配置：能解析出 API key 即视为已配置。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    get_api_key(deps).unwrap_or(None).is_some()
}

/// 注册表入口：读 key（缺失视为未配置）→ 请求限额端点 → 从 limits 中
/// 挑出 TOKENS_LIMIT 与 TIME_LIMIT 分别组装 "Tokens"（窗口秒数由条目
/// 决定）与 "MCP Tools"（固定 30 天）窗口，返回统一结果。
pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let api_key = match get_api_key(&deps) {
            Ok(Some(key)) => key,
            Ok(None) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    false,
                    None,
                    Some("Not configured"),
                    None,
                    now,
                );
            }
            Err(message) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    now,
                );
            }
        };
        let request = HttpRequest::get(LIMIT_URL)
            .bearer(&api_key)
            .header("Content-Type", "application/json");

        let response = match (deps.http)(request).await {
            Ok(response) => response,
            Err(error) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    true,
                    None,
                    Some(&error.message()),
                    None,
                    deps.now_ms(),
                );
            }
        };
        if !response.ok() {
            return build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some(&format!("API error: {}", response.status)),
                None,
                deps.now_ms(),
            );
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    true,
                    None,
                    Some("Invalid response from provider"),
                    None,
                    deps.now_ms(),
                );
            }
        };

        let limits = field(&payload, "data")
            .and_then(|data| field(data, "limits"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let tokens_limit = limits
            .iter()
            .find(|limit| field(limit, "type").and_then(Value::as_str) == Some("TOKENS_LIMIT"));
        let mcp_tools_limit = limits
            .iter()
            .find(|limit| field(limit, "type").and_then(Value::as_str) == Some("TIME_LIMIT"));

        let mut windows = Map::new();

        // TOKENS_LIMIT — 5-hour token window.
        if let Some(tokens_limit) = tokens_limit {
            let reset_at = field(tokens_limit, "nextResetTime")
                .filter(|value| value.is_number() && value.as_f64() != Some(0.0))
                .and_then(normalize_timestamp)
                .map(|ms| json!(ms));
            windows.insert(
                "Tokens".into(),
                to_usage_window(
                    now,
                    field(tokens_limit, "percentage").and_then(Value::as_f64),
                    resolve_window_seconds(tokens_limit),
                    reset_at.as_ref(),
                    None,
                ),
            );
        }

        // TIME_LIMIT — MCP tools monthly window.
        if let Some(mcp_limit) = mcp_tools_limit {
            let reset_at = field(mcp_limit, "nextResetTime")
                .filter(|value| value.is_number() && value.as_f64() != Some(0.0))
                .and_then(normalize_timestamp)
                .map(|ms| json!(ms));
            windows.insert(
                "MCP Tools".into(),
                to_usage_window(
                    now,
                    field(mcp_limit, "percentage").and_then(Value::as_f64),
                    Some(MONTH_SECONDS),
                    reset_at.as_ref(),
                    None,
                ),
            );
        }

        build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}
