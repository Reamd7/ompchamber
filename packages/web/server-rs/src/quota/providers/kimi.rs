//! Port of `server/lib/quota/providers/kimi.js` — Kimi for Coding usage via
//! `GET https://api.kimi.com/coding/v1/usages`.
//!
//! The weekly `usage` block reports `used`; rate-limit `limits[].detail`
//! blocks report `remaining` — `used` wins when both exist.

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, duration_to_label, duration_to_seconds, field, get_auth_entry,
    normalize_auth_entry, to_number, to_timestamp, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "kimi-for-coding";
pub const PROVIDER_NAME: &str = "Kimi for Coding";
pub const ALIASES: [&str; 2] = ["kimi-for-coding", "kimi"];

const USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

/// `computeUsedPercent` — derive usage from whichever field the API returned.
pub fn compute_used_percent(
    total: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
) -> Option<f64> {
    let total = total?;
    if total == 0.0 {
        return None;
    }
    if let Some(used) = used {
        return Some(((used / total) * 100.0).clamp(0.0, 100.0));
    }
    if let Some(remaining) = remaining {
        return Some((100.0 - (remaining / total) * 100.0).clamp(0.0, 100.0));
    }
    None
}

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

        let mut windows = Map::new();

        if let Some(usage) = field(&payload, "usage") {
            let limit = to_number(field(usage, "limit"));
            let used = to_number(field(usage, "used"));
            let remaining = to_number(field(usage, "remaining"));
            let reset_at = to_timestamp(field(usage, "resetTime"));
            windows.insert(
                "weekly".into(),
                to_usage_window(
                    now,
                    compute_used_percent(limit, used, remaining),
                    None,
                    reset_at.map(|ms| json!(ms)).as_ref(),
                    None,
                ),
            );
        }

        if let Some(limits) = field(&payload, "limits").and_then(Value::as_array) {
            for limit in limits {
                let Some(window) = field(limit, "window") else {
                    continue;
                };
                let detail = field(limit, "detail");
                let duration = to_number(field(window, "duration"));
                let time_unit = field(window, "timeUnit").and_then(Value::as_str);
                let raw_label = duration_to_label(duration, time_unit);
                let window_seconds = duration_to_seconds(duration, time_unit);
                let label = if window_seconds == Some(5.0 * 60.0 * 60.0) {
                    format!("Rate Limit ({raw_label})")
                } else {
                    raw_label
                };
                let total = detail.and_then(|detail| to_number(field(detail, "limit")));
                let used = detail.and_then(|detail| to_number(field(detail, "used")));
                let remaining = detail.and_then(|detail| to_number(field(detail, "remaining")));
                let reset_at = detail.and_then(|detail| to_timestamp(field(detail, "resetTime")));
                windows.insert(
                    label,
                    to_usage_window(
                        now,
                        compute_used_percent(total, used, remaining),
                        window_seconds,
                        reset_at.map(|ms| json!(ms)).as_ref(),
                        None,
                    ),
                );
            }
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
