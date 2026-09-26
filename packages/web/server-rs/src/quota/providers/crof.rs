//! Port of `server/lib/quota/providers/crof.js` — CrofAI credit balance via
//! `GET https://crof.ai/usage_api/` (15s timeout).

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, format_money, get_auth_entry, normalize_auth_entry, to_number,
    to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "crof";
pub const PROVIDER_NAME: &str = "CrofAI";
pub const ALIASES: [&str; 1] = ["crof"];

const CROF_USAGE_URL: &str = "https://crof.ai/usage_api/";
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

        let request = HttpRequest::get(CROF_USAGE_URL)
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
            Err(crate::quota::http::HttpError::Timeout) => {
                return failure("Request timed out".into(), deps.now_ms());
            }
            Err(error) => return failure(error.message(), deps.now_ms()),
        };
        if !response.ok() {
            let message = if response.status == 401 {
                "Session expired \u{2014} please re-authenticate with CrofAI".to_string()
            } else {
                format!("API error: {}", response.status)
            };
            return failure(message, deps.now_ms());
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Invalid response from provider".into(), deps.now_ms()),
        };

        let credits = to_number(field(&payload, "credits"));
        let value_label =
            credits.map(|credits| format!("${}", format_money(Some(credits)).unwrap_or_default()));

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
