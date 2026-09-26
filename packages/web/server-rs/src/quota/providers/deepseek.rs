//! Port of `server/lib/quota/providers/deepseek.js` — DeepSeek balance via
//! `GET https://api.deepseek.com/user/balance` (15s timeout, USD preferred).

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, format_money, get_auth_entry, normalize_auth_entry, to_number,
    to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "deepseek";
pub const PROVIDER_NAME: &str = "DeepSeek";
pub const ALIASES: [&str; 1] = ["deepseek"];

const DEEPSEEK_QUOTA_URL: &str = "https://api.deepseek.com/user/balance";
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

        let request = HttpRequest::get(DEEPSEEK_QUOTA_URL)
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
            let message = if response.status == 401 || response.status == 403 {
                "Session expired \u{2014} please re-authenticate with DeepSeek".to_string()
            } else {
                format!("API error: {}", response.status)
            };
            return failure(message, deps.now_ms());
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Invalid response from provider".into(), deps.now_ms()),
        };

        let balance_infos = field(&payload, "balance_infos").and_then(Value::as_array);
        let balance_info = balance_infos
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|info| field(info, "currency").and_then(Value::as_str) == Some("USD"))
            })
            .or_else(|| {
                balance_infos.and_then(|infos| {
                    infos
                        .iter()
                        .find(|info| field(info, "currency").and_then(Value::as_str) == Some("CNY"))
                })
            });

        // A missing/empty `total_balance` is unusable; a literal zero is fine.
        let total_balance = balance_info
            .and_then(|info| field(info, "total_balance"))
            .and_then(|raw| match raw {
                Value::Number(_) => to_number(Some(raw)),
                Value::String(text) if !text.trim().is_empty() => to_number(Some(raw)),
                _ => None,
            });

        let Some(total_balance) = total_balance else {
            return failure("No quota data in response".into(), deps.now_ms());
        };

        let is_cny = balance_info
            .and_then(|info| field(info, "currency"))
            .and_then(Value::as_str)
            == Some("CNY");
        let symbol = if is_cny { "\u{a5}" } else { "$" };
        let value_label = format!(
            "{symbol}{}",
            format_money(Some(total_balance)).unwrap_or_default()
        );

        let mut windows = Map::new();
        windows.insert(
            "credits_balance".into(),
            to_usage_window(now, None, None, None, Some(&value_label)),
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
