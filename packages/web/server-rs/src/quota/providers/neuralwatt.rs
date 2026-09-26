//! Port of `server/lib/quota/providers/neuralwatt.js` — NeuralWatt quota via
//! `GET https://api.neuralwatt.com/v1/quota` (15s timeout).

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    as_non_empty_string, build_result, field, format_money, get_auth_entry, normalize_auth_entry,
    to_number, to_timestamp, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "neuralwatt";
pub const PROVIDER_NAME: &str = "NeuralWatt";
pub const ALIASES: [&str; 1] = ["neuralwatt"];

const NEURALWATT_QUOTA_URL: &str = "https://api.neuralwatt.com/v1/quota";
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 30d month / 365d year are fixed UI approximations.
pub fn period_to_window_seconds(period: &str) -> Option<f64> {
    match period {
        "daily" => Some(86_400.0),
        "weekly" => Some(604_800.0),
        "monthly" | "month" => Some(30.0 * 86_400.0),
        "yearly" | "year" => Some(365.0 * 86_400.0),
        _ => None,
    }
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

        let request = HttpRequest::get(NEURALWATT_QUOTA_URL)
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
            let message = if response.status == 401 {
                "Session expired \u{2014} please re-authenticate with NeuralWatt".to_string()
            } else {
                format!("API error: {}", response.status)
            };
            return failure(message, deps.now_ms());
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Invalid response from provider".into(), deps.now_ms()),
        };

        let subscription = field(&payload, "subscription");
        let in_overage =
            subscription.and_then(|sub| field(sub, "in_overage")) == Some(&Value::Bool(true));
        let allowance = field(&payload, "key").and_then(|key| field(key, "allowance"));
        let key_name = field(&payload, "key")
            .and_then(|key| field(key, "name"))
            .and_then(as_non_empty_string);
        let credits_remaining = field(&payload, "balance")
            .and_then(|balance| to_number(field(balance, "credits_remaining_usd")));

        let mut windows = Map::new();

        if let Some(subscription) = subscription {
            let kwh_included = to_number(field(subscription, "kwh_included"));
            let kwh_used = to_number(field(subscription, "kwh_used"));
            let plan = field(subscription, "plan").and_then(as_non_empty_string);
            // The API exposes no kWh window start, so windowSeconds stays null
            // rather than guessing a duration.
            let sub_key = plan.clone().unwrap_or_else(|| "plan_limit".to_string());
            let used_percent = if in_overage {
                Some(100.0)
            } else {
                match (kwh_included, kwh_used) {
                    (Some(included), Some(used)) if included > 0.0 => {
                        Some(((used / included) * 100.0).clamp(0.0, 100.0))
                    }
                    _ => None,
                }
            };
            let sub_reset_at = to_timestamp(field(subscription, "kwh_reset_date"))
                .or_else(|| to_timestamp(field(subscription, "current_period_end")));
            windows.insert(
                sub_key,
                to_usage_window(
                    now,
                    used_percent,
                    None,
                    sub_reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    None,
                ),
            );
        }

        if allowance.is_some() {
            let allowance = allowance.unwrap_or(&Value::Null);
            let spent = to_number(field(allowance, "spent_usd"));
            let limit = to_number(field(allowance, "limit_usd"));
            // Credits wallet is reduced by each period's spend before the cap
            // bites — the real ceiling is min(limit, credits + spent).
            let effective_spent = spent.unwrap_or(0.0);
            let effective_limit = match (limit, credits_remaining) {
                (Some(limit), Some(credits)) => Some(limit.min(credits + effective_spent)),
                (Some(limit), None) => Some(limit),
                (None, Some(credits)) => Some(credits),
                (None, None) => None,
            };
            let period = field(allowance, "period").and_then(as_non_empty_string);
            let blocked = field(allowance, "blocked") == Some(&Value::Bool(true));
            let used_percent = if blocked {
                Some(100.0)
            } else {
                match (spent, effective_limit) {
                    (Some(spent), Some(limit)) if limit > 0.0 => {
                        Some(((spent / limit) * 100.0).clamp(0.0, 100.0))
                    }
                    _ => None,
                }
            };
            let period_key = match period.as_deref() {
                Some("daily") | Some("weekly") => period.clone(),
                Some("month") | Some("monthly") => Some("monthly".to_string()),
                _ => Some("billing_cycle".to_string()),
            };
            let window_seconds = period.as_deref().and_then(period_to_window_seconds);
            let reset_at = to_timestamp(field(allowance, "reset_at"));
            windows.insert(
                period_key.unwrap_or_else(|| "billing_cycle".to_string()),
                to_usage_window(
                    now,
                    used_percent,
                    window_seconds,
                    reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    key_name.as_deref(),
                ),
            );
        } else if let Some(credits) = credits_remaining {
            let value_label = format!("${}", format_money(Some(credits)).unwrap_or_default());
            windows.insert(
                "credits_balance".into(),
                to_usage_window(now, None, None, None, Some(&value_label)),
            );
        }

        if windows.is_empty() {
            return failure("No quota data in response".into(), deps.now_ms());
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
