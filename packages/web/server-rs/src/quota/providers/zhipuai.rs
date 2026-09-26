//! Port of `server/lib/quota/providers/zhipuai-coding-plan.js` — Zhipu AI
//! Coding Plan via `GET https://open.bigmodel.cn/api/monitor/usage/quota/limit`.
//!
//! Auth resolution: OpenCode `auth.json` entry first, then the merged OpenCode
//! config layers' `provider[alias].options.apiKey`.

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::{QuotaDeps, read_opencode_user_config};
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, normalize_timestamp,
    resolve_window_seconds, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "zhipuai-coding-plan";
pub const PROVIDER_NAME: &str = "Zhipu AI Coding Plan";
pub const ALIASES: [&str; 3] = ["zhipuai-coding-plan", "zhipuai", "zhipu"];

const LIMIT_URL: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";
const MONTH_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

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

pub fn is_configured(deps: &QuotaDeps) -> bool {
    get_api_key(deps).unwrap_or(None).is_some()
}

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
