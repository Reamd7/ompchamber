//! Port of `server/lib/quota/providers/wafer.js` — Wafer.ai quota via
//! `GET https://pass.wafer.ai/v1/inference/quota` (15s timeout).

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    as_non_empty_string, build_result, field, get_auth_entry, normalize_auth_entry, num_str,
    resolve_window_label, to_number, to_timestamp, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "wafer";
pub const PROVIDER_NAME: &str = "Wafer.ai";
pub const ALIASES: [&str; 4] = ["wafer", "wafer-ai", "wafer_ai", "wafer.ai"];

const WAFER_QUOTA_URL: &str = "https://pass.wafer.ai/v1/inference/quota";
const WAFER_WINDOW_SECONDS: f64 = 5.0 * 3600.0;
const REQUEST_TIMEOUT_MS: u64 = 15_000;

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

        let request = HttpRequest::get(WAFER_QUOTA_URL)
            .bearer(&api_key)
            .header("Accept-Encoding", "identity")
            .timeout(REQUEST_TIMEOUT_MS);

        let failure = |message: String, now: u64| {
            build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some(&message),
                None,
                now,
            )
        };

        let response = match (deps.http)(request).await {
            Ok(response) => response,
            Err(HttpError::Timeout) => return failure("Request timed out".into(), deps.now_ms()),
            Err(error) => return failure(error.message(), deps.now_ms()),
        };
        if !response.ok() {
            return failure(format!("API error: {}", response.status), deps.now_ms());
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Invalid response from provider".into(), deps.now_ms()),
        };

        let remaining = to_number(field(&payload, "remaining_included_requests"));
        let limit = to_number(field(&payload, "included_request_limit"));
        let overage = to_number(field(&payload, "overage_request_count"));
        let used_percent_raw = to_number(field(&payload, "current_period_used_percent"));
        let window_start = to_timestamp(field(&payload, "window_start"));
        let window_end = to_timestamp(field(&payload, "window_end"));
        let plan_tier = field(&payload, "plan_tier").and_then(as_non_empty_string);

        if remaining.is_none() && limit.is_none() && overage.is_none() && used_percent_raw.is_none()
        {
            return failure("No quota data in response".into(), deps.now_ms());
        }

        let has_overage = overage.map(|overage| overage > 0.0).unwrap_or(false);
        let used_percent = match used_percent_raw {
            Some(raw) if has_overage => Some(raw.max(0.0)),
            Some(raw) => Some(raw.clamp(0.0, 100.0)),
            None => None,
        };

        let window_seconds = match (window_start, window_end) {
            (Some(start), Some(end)) => (((end - start) as f64) / 1000.0).round(),
            _ => WAFER_WINDOW_SECONDS,
        };
        let window_label = resolve_window_label(Some(window_seconds));

        let value_label = match (remaining, limit) {
            (Some(remaining), Some(limit)) => {
                let mut parts = Vec::new();
                if let Some(tier) = plan_tier.as_deref() {
                    parts.push(tier.to_string());
                }
                parts.push(format!("{} / {} left", num_str(remaining), num_str(limit)));
                if has_overage && let Some(overage) = overage {
                    parts.push(format!("+{} overage", num_str(overage)));
                }
                Some(parts.join(" \u{b7} "))
            }
            _ => None,
        };

        let mut windows = Map::new();
        windows.insert(
            window_label,
            to_usage_window(
                now,
                used_percent,
                Some(window_seconds),
                window_end.map(|ms| json!(ms)).as_ref(),
                value_label.as_deref(),
            ),
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
