//! Port of `server/lib/quota/providers/openrouter.js` — OpenRouter credit
//! balance via `GET https://openrouter.ai/api/v1/credits`.

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, format_money, get_auth_entry, normalize_auth_entry, to_number,
    to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "openrouter";
pub const PROVIDER_NAME: &str = "OpenRouter";
pub const ALIASES: [&str; 1] = ["openrouter"];

const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

fn load_api_key(deps: &QuotaDeps) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    Ok(entry.and_then(|entry| {
        field(&entry, "key")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_api_key(deps).unwrap_or(None).is_some()
}

pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let api_key = match load_api_key(&deps) {
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

        let request = HttpRequest::get(CREDITS_URL)
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

        let credits = field(&payload, "data").unwrap_or(&Value::Null);
        let total_credits = to_number(field(credits, "total_credits"));
        let total_usage = to_number(field(credits, "total_usage"));
        let remaining = match (total_credits, total_usage) {
            (Some(total), Some(used)) => Some((total - used).max(0.0)),
            _ => None,
        };
        let value_label = match (remaining, total_usage) {
            (Some(remaining), Some(used)) => Some(format!(
                "${} left \u{b7} ${} spent",
                format_money(Some(remaining)).unwrap_or_default(),
                format_money(Some(used)).unwrap_or_default()
            )),
            _ => None,
        };

        let mut windows = Map::new();
        windows.insert(
            "credits".into(),
            to_usage_window(now, None, None, None, value_label.as_deref()),
        );

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
