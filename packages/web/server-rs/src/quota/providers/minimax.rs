//! Port of `server/lib/quota/providers/minimax-shared.js` plus the two
//! concrete instances (`minimax-coding-plan.js` for minimax.io and
//! `minimax-cn-coding-plan.js` for minimaxi.com).
//!
//! Token Plan endpoint (`/v1/token_plan/remains`, M3) is tried first and
//! falls back to the legacy Coding Plan endpoint; the two disagree on
//! `current_interval_usage_count` semantics (remaining vs consumed).

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

/// Status 3 = window not applicable for the current plan tier.
const WINDOW_STATUS_INACTIVE: f64 = 3.0;
const TEXT_MODELS: [&str; 3] = ["general", "chat", "text"];

pub struct MiniMaxPlan {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    pub aliases: [&'static str; 1],
    pub token_plan_url: &'static str,
    pub coding_plan_url: &'static str,
}

pub const MINIMAX_PLAN: MiniMaxPlan = MiniMaxPlan {
    provider_id: "minimax-coding-plan",
    provider_name: "MiniMax Coding Plan (minimax.io)",
    aliases: ["minimax-coding-plan"],
    token_plan_url: "https://api.minimax.io/v1/token_plan/remains",
    coding_plan_url: "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
};

pub const MINIMAX_CN_PLAN: MiniMaxPlan = MiniMaxPlan {
    provider_id: "minimax-cn-coding-plan",
    provider_name: "MiniMax Coding Plan (minimaxi.com)",
    aliases: ["minimax-cn-coding-plan"],
    token_plan_url: "https://api.minimaxi.com/v1/token_plan/remains",
    coding_plan_url: "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
};

fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// `pickChatModel` — M3 candidate, then text-model names, then any remaining
/// percent entry, then the first entry.
pub fn pick_chat_model(model_remains: Option<&Vec<Value>>) -> Option<&Value> {
    let models = model_remains?;

    if models.is_empty() {
        return None;
    }

    fn model_name(model: &Value) -> Option<&str> {
        field(model, "model_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
    }

    if let Some(candidate) = models.iter().find(|model| {
        model_name(model).is_some_and(|name| starts_with_ignore_case(name, "minimax-m"))
            && to_number(field(model, "current_interval_total_count"))
                .is_some_and(|total| total > 0.0)
    }) {
        return Some(candidate);
    }

    if let Some(candidate) = models.iter().find(|model| {
        model_name(model)
            .is_some_and(|name| TEXT_MODELS.contains(&name.to_ascii_lowercase().as_str()))
    }) {
        return Some(candidate);
    }

    if let Some(candidate) = models.iter().find(|model| {
        field(model, "current_interval_remaining_percent").is_some_and(|v| v.is_number())
    }) {
        return Some(candidate);
    }

    models.first()
}

/// `isUsablePayload` — `base_resp.status_code` must be 0 when present and
/// `model_remains` must be a non-empty array.
fn is_usable_payload(payload: &Value) -> bool {
    if let Some(base_resp) = field(payload, "base_resp")
        && field(base_resp, "status_code") != Some(&serde_json::json!(0))
    {
        return false;
    }
    field(payload, "model_remains")
        .and_then(Value::as_array)
        .is_some_and(|models| !models.is_empty())
}

/// `fetchEndpoint` — any failure reads as "no payload".
async fn fetch_endpoint(deps: &QuotaDeps, url: &str, api_key: &str) -> Option<Value> {
    let request = HttpRequest::get(url)
        .bearer(api_key)
        .header("Content-Type", "application/json");
    let response = (deps.http)(request).await.ok()?;
    if !response.ok() {
        return None;
    }
    let payload = response.json().ok()?;
    is_usable_payload(&payload).then_some(payload)
}

fn coerce_percent(value: Option<&Value>) -> Option<f64> {
    to_number(value).map(|n| n.clamp(0.0, 100.0))
}

/// `isWindowActive` — absent status defaults to active; 3 is inactive.
fn is_window_active(status: Option<&Value>) -> bool {
    match to_number(status) {
        None => true,
        Some(status) => status != WINDOW_STATUS_INACTIVE,
    }
}

/// `calculateWindowSeconds` — from API timestamps (ms) or `remains_time` (ms).
fn calculate_window_seconds(
    start_at: Option<i64>,
    reset_at: Option<i64>,
    remains_time_ms: Option<f64>,
) -> Option<f64> {
    if let (Some(start), Some(reset)) = (start_at, reset_at)
        && reset > start
    {
        return Some(((reset - start) as f64 / 1000.0).floor());
    }
    if let Some(remains) = remains_time_ms.filter(|remains| *remains > 0.0) {
        return Some((remains / 1000.0).floor());
    }
    None
}

pub struct MiniMaxUsage {
    pub interval_used_percent: Option<f64>,
    pub interval_window_seconds: Option<f64>,
    pub interval_reset_at: Option<i64>,
    pub weekly_used_percent: Option<f64>,
    pub weekly_window_seconds: Option<f64>,
    pub weekly_reset_at: Option<i64>,
}

/// `calculateUsage` — token-plan endpoints report *remaining* in the usage
/// count field, so `used = total - reported`.
pub fn calculate_usage(model: &Value, is_token_plan: bool) -> MiniMaxUsage {
    let interval_total = to_number(field(model, "current_interval_total_count"));
    let interval_usage_raw = to_number(field(model, "current_interval_usage_count"));
    let interval_start_at = to_timestamp(field(model, "start_time"));
    let interval_reset_at = to_timestamp(field(model, "end_time"));
    let interval_remains_time = to_number(field(model, "remains_time"));
    let interval_remaining_percent =
        coerce_percent(field(model, "current_interval_remaining_percent"));

    let weekly_total = to_number(field(model, "current_weekly_total_count"));
    let weekly_usage_raw = to_number(field(model, "current_weekly_usage_count"));
    let weekly_start_at = to_timestamp(field(model, "weekly_start_time"));
    let weekly_reset_at = to_timestamp(field(model, "weekly_end_time"));
    let weekly_remains_time = to_number(field(model, "weekly_remains_time"));
    let weekly_remaining_percent = coerce_percent(field(model, "current_weekly_remaining_percent"));

    let compute = |remaining_percent: Option<f64>,
                   total: Option<f64>,
                   usage_raw: Option<f64>|
     -> Option<f64> {
        if let Some(remaining) = remaining_percent {
            return Some(100.0 - remaining);
        }
        match (total, usage_raw) {
            (Some(total), Some(raw)) if total > 0.0 => {
                let used = if is_token_plan {
                    (total - raw).max(0.0)
                } else {
                    raw
                };
                Some(((used / total) * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        }
    };

    MiniMaxUsage {
        interval_used_percent: compute(
            interval_remaining_percent,
            interval_total,
            interval_usage_raw,
        ),
        interval_window_seconds: calculate_window_seconds(
            interval_start_at,
            interval_reset_at,
            interval_remains_time,
        ),
        interval_reset_at,
        weekly_used_percent: compute(weekly_remaining_percent, weekly_total, weekly_usage_raw),
        weekly_window_seconds: calculate_window_seconds(
            weekly_start_at,
            weekly_reset_at,
            weekly_remains_time,
        ),
        weekly_reset_at,
    }
}

fn load_api_key(deps: &QuotaDeps, aliases: &[&str]) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, aliases));
    Ok(entry.and_then(|entry| {
        field(&entry, "key")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

pub fn is_configured_for(deps: &QuotaDeps, aliases: &[&str]) -> bool {
    load_api_key(deps, aliases).unwrap_or(None).is_some()
}

pub fn fetch_quota_for(
    rt: std::sync::Arc<QuotaRuntime>,
    plan: &'static MiniMaxPlan,
) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let api_key = match load_api_key(&deps, &plan.aliases) {
            Ok(Some(key)) => key,
            Ok(None) => {
                return build_result(
                    plan.provider_id,
                    plan.provider_name,
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
                    plan.provider_id,
                    plan.provider_name,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    now,
                );
            }
        };

        let failure = |message: &str, now: u64| {
            build_result(
                plan.provider_id,
                plan.provider_name,
                false,
                true,
                None,
                Some(message),
                None,
                now,
            )
        };

        let mut payload = fetch_endpoint(&deps, plan.token_plan_url, &api_key).await;
        let mut is_token_plan = true;
        if payload.is_none() {
            payload = fetch_endpoint(&deps, plan.coding_plan_url, &api_key).await;
            is_token_plan = false;
        }

        let Some(payload) = payload else {
            return failure("API returned no usable quota data", deps.now_ms());
        };

        let model_remains = field(&payload, "model_remains")
            .and_then(Value::as_array)
            .cloned();
        let Some(model) = pick_chat_model(model_remains.as_ref()) else {
            return failure("No model quota data available", deps.now_ms());
        };
        let model = model.clone();

        let usage = calculate_usage(&model, is_token_plan);

        let mut windows = Map::new();
        windows.insert(
            "5h".into(),
            to_usage_window(
                now,
                usage.interval_used_percent,
                usage.interval_window_seconds,
                usage.interval_reset_at.map(|ms| json!(ms)).as_ref(),
                None,
            ),
        );

        // Weekly window only when the tier supports it (status 3 = legacy).
        let weekly_active = is_window_active(field(&model, "current_weekly_status"));
        let has_weekly_data = weekly_active
            && (coerce_percent(field(&model, "current_weekly_remaining_percent")).is_some()
                || to_number(field(&model, "current_weekly_total_count"))
                    .is_some_and(|total| total > 0.0));
        if has_weekly_data {
            windows.insert(
                "weekly".into(),
                to_usage_window(
                    now,
                    usage.weekly_used_percent,
                    usage.weekly_window_seconds,
                    usage.weekly_reset_at.map(|ms| json!(ms)).as_ref(),
                    None,
                ),
            );
        }

        build_result(
            plan.provider_id,
            plan.provider_name,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    is_configured_for(deps, &MINIMAX_PLAN.aliases)
}

pub fn is_configured_cn(deps: &QuotaDeps) -> bool {
    is_configured_for(deps, &MINIMAX_CN_PLAN.aliases)
}

pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_quota_for(rt, &MINIMAX_PLAN)
}

pub fn fetch_quota_cn(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_quota_for(rt, &MINIMAX_CN_PLAN)
}
