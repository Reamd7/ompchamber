//! Port of `server/lib/quota/providers/copilot.js` — GitHub Copilot (and the
//! Add-on twin) via `GET https://api.github.com/copilot_internal/user`.

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "github-copilot";
pub const PROVIDER_NAME: &str = "GitHub Copilot";
pub const PROVIDER_ID_ADDON: &str = "github-copilot-addon";
pub const PROVIDER_NAME_ADDON: &str = "GitHub Copilot Add-on";
pub const ALIASES: [&str; 2] = ["github-copilot", "copilot"];

const USER_URL: &str = "https://api.github.com/copilot_internal/user";

/// `buildCopilotWindows` — only the `premium_interactions` snapshot is
/// exposed (mirrors microsoft/vscode-copilot-chat quota semantics).
pub fn build_copilot_windows(payload: &Value, now: u64) -> Map<String, Value> {
    let quota = field(payload, "quota_snapshots");
    let reset_at = to_timestamp(field(payload, "quota_reset_date"));
    let reset_at = reset_at.map(|ms| json!(ms));
    let mut windows = Map::new();

    let Some(snapshot) = quota.and_then(|snapshots| field(snapshots, "premium_interactions"))
    else {
        return windows;
    };

    if field(snapshot, "unlimited") == Some(&Value::Bool(true)) {
        windows.insert(
            "premium_interactions".into(),
            to_usage_window(now, None, None, reset_at.as_ref(), Some("Unlimited")),
        );
        return windows;
    }

    let entitlement = to_number(field(snapshot, "entitlement"));
    let remaining = to_number(field(snapshot, "remaining"));
    let mut used_percent = match (entitlement, remaining) {
        (Some(entitlement), Some(remaining)) if entitlement > 0.0 => {
            Some((100.0 - (remaining / entitlement) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    if used_percent.is_none()
        && let Some(percent_remaining) = to_number(field(snapshot, "percent_remaining"))
    {
        used_percent = Some((100.0 - percent_remaining).clamp(0.0, 100.0));
    }
    let value_label = match (entitlement, remaining) {
        (Some(entitlement), Some(remaining)) if entitlement > 0.0 => Some(format!(
            "{} / {} left",
            crate::quota::utils::js_to_fixed(remaining, 0),
            crate::quota::utils::js_to_fixed(entitlement, 0)
        )),
        _ => None,
    };
    windows.insert(
        "premium_interactions".into(),
        to_usage_window(
            now,
            used_percent,
            None,
            reset_at.as_ref(),
            value_label.as_deref(),
        ),
    );
    windows
}

fn load_access_token(deps: &QuotaDeps) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    Ok(entry.and_then(|entry| {
        field(&entry, "access")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_access_token(deps).unwrap_or(None).is_some()
}

fn copilot_request(access_token: &str) -> HttpRequest {
    HttpRequest::get(USER_URL)
        .header("Authorization", format!("token {access_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", "vscode/1.96.2")
        .header("X-Github-Api-Version", "2025-04-01")
}

fn fetch_for(
    rt: Arc<QuotaRuntime>,
    provider_id: &'static str,
    provider_name: &'static str,
) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let access_token = match load_access_token(&deps) {
            Ok(Some(token)) => token,
            Ok(None) => {
                return build_result(
                    provider_id,
                    provider_name,
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
                    provider_id,
                    provider_name,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    now,
                );
            }
        };

        let response = match (deps.http)(copilot_request(&access_token)).await {
            Ok(response) => response,
            Err(error) => {
                return build_result(
                    provider_id,
                    provider_name,
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
                provider_id,
                provider_name,
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
                    provider_id,
                    provider_name,
                    false,
                    true,
                    None,
                    Some("Invalid response from provider"),
                    None,
                    deps.now_ms(),
                );
            }
        };

        let windows = build_copilot_windows(&payload, deps.now_ms());
        build_result(
            provider_id,
            provider_name,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}

pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_for(rt, PROVIDER_ID, PROVIDER_NAME)
}

pub fn fetch_quota_addon(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_for(rt, PROVIDER_ID_ADDON, PROVIDER_NAME_ADDON)
}
