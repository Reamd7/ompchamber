//! Port of `server/lib/quota/providers/nanogpt.js` — NanoGPT subscription
//! usage via `GET https://nano-gpt.com/api/subscription/v1/usage`.

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "nano-gpt";
pub const PROVIDER_NAME: &str = "NanoGPT";
pub const ALIASES: [&str; 3] = ["nano-gpt", "nanogpt", "nano_gpt"];

const USAGE_URL: &str = "https://nano-gpt.com/api/subscription/v1/usage";
const DAILY_WINDOW_SECONDS: f64 = 86_400.0;

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

/// `percentUsed` is a 0-1 fraction; the used/limit fallback computes one
/// (`block.limit ?? block.limits.<limits_key>`).
fn window_used_percent(block: &Value, limits_key: &str) -> Option<f64> {
    if let Some(percent_used) = field(block, "percentUsed").and_then(Value::as_f64) {
        return Some((percent_used * 100.0).clamp(0.0, 100.0));
    }
    let used = to_number(field(block, "used"));
    let limit = to_number(field(block, "limit"))
        .or_else(|| field(block, "limits").and_then(|limits| to_number(field(limits, limits_key))));
    match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => {
            Some(((used / limit) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    }
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

        let request = HttpRequest::get(USAGE_URL)
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

        let period = field(&payload, "period");
        let state = field(&payload, "state")
            .and_then(Value::as_str)
            .filter(|state| !state.is_empty())
            .unwrap_or("active")
            .to_string();
        let state_label = (state != "active").then(|| format!("({state})"));

        let mut windows = Map::new();

        if let Some(daily) = field(&payload, "daily") {
            let reset_at = to_timestamp(field(daily, "resetAt"));
            windows.insert(
                "daily".into(),
                to_usage_window(
                    now,
                    window_used_percent(daily, "daily"),
                    Some(DAILY_WINDOW_SECONDS),
                    reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    state_label.as_deref(),
                ),
            );
        }

        if let Some(monthly) = field(&payload, "monthly") {
            let reset_at = to_timestamp(field(monthly, "resetAt")).or_else(|| {
                period.and_then(|period| to_timestamp(field(period, "currentPeriodEnd")))
            });
            windows.insert(
                "monthly".into(),
                to_usage_window(
                    now,
                    window_used_percent(monthly, "monthly"),
                    None,
                    reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    state_label.as_deref(),
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
