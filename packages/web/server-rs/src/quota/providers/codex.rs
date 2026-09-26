//! Port of `server/lib/quota/providers/codex.js` — Codex (ChatGPT backend)
//! quota via `GET https://chatgpt.com/backend-api/wham/usage`.

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, format_money, get_auth_entry, js_to_fixed, normalize_auth_entry,
    resolve_window_label, to_number, to_timestamp, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "codex";
pub const PROVIDER_NAME: &str = "Codex";
pub const ALIASES: [&str; 3] = ["openai", "codex", "chatgpt"];

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

struct Credential {
    access_token: String,
    account_id: Option<String>,
}

fn load_credential(deps: &QuotaDeps) -> Result<Option<Credential>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    Ok(entry.and_then(|entry| {
        let access_token = field(&entry, "access")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))?
            .to_string();
        Some(Credential {
            access_token,
            account_id: field(&entry, "accountId")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }))
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_credential(deps).unwrap_or(None).is_some()
}

pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let credential = match load_credential(&deps) {
            Ok(Some(credential)) => credential,
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
                // JS: readAuthFile() throws before the try block, so the
                // registry catch answers with the read error.
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

        let mut request = HttpRequest::get(USAGE_URL)
            .bearer(&credential.access_token)
            .header("Content-Type", "application/json");
        if let Some(account_id) = &credential.account_id {
            request = request.header("ChatGPT-Account-Id", account_id.clone());
        }

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
            let message = if response.status == 401 {
                "Session expired \u{2014} please re-authenticate with OpenAI".to_string()
            } else {
                format!("API error: {}", response.status)
            };
            return build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some(&message),
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

        let rate_limit = field(&payload, "rate_limit");
        let mut windows = Map::new();

        for window_key in ["primary_window", "secondary_window"] {
            let Some(window) = rate_limit.and_then(|limits| field(limits, window_key)) else {
                continue;
            };
            let window_seconds = to_number(field(window, "limit_window_seconds"));
            let label = resolve_window_label(window_seconds);
            let reset_at = to_timestamp(field(window, "reset_at"));
            windows.insert(
                label,
                to_usage_window(
                    now,
                    to_number(field(window, "used_percent")),
                    window_seconds,
                    reset_at.map(|ms| json!(ms)).as_ref(),
                    None,
                ),
            );
        }

        if let Some(credits) = field(&payload, "credits") {
            let balance = to_number(field(credits, "balance"));
            let unlimited = field(credits, "unlimited") == Some(&Value::Bool(true));
            let label = if unlimited {
                Some("Unlimited".to_string())
            } else {
                balance
                    .map(|balance| format!("${}", format_money(Some(balance)).unwrap_or_default()))
            };
            windows.insert(
                "credits_balance".into(),
                to_usage_window(now, None, None, None, label.as_deref()),
            );
        }

        // Business/enterprise accounts expose a dollar spend cap under
        // `spend_control.individual_limit`; surface it as an additive
        // `credits` window.
        if let Some(spend_limit) =
            field(&payload, "spend_control").and_then(|control| field(control, "individual_limit"))
        {
            let used = to_number(field(spend_limit, "used"));
            let limit = to_number(field(spend_limit, "limit"));
            let value_label = match (used, limit) {
                (Some(used), Some(limit)) => Some(format!(
                    "{} / {} used",
                    js_to_fixed(used, 0),
                    js_to_fixed(limit, 0)
                )),
                _ => None,
            };
            windows.insert(
                "credits".into(),
                to_usage_window(
                    now,
                    to_number(field(spend_limit, "used_percent")),
                    None,
                    None,
                    value_label.as_deref(),
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
