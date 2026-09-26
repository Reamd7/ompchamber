//! Port of `server/lib/quota/providers/google/` — Gemini CLI + Antigravity
//! quota via the CloudCode internal endpoints.
//!
//! - `auth.js`: Gemini CLI entry from OpenCode `auth.json`
//!   (`google`/`google.oauth`), plus Antigravity accounts files; OAuth
//!   refresh uses the well-known public client IDs each CLI ships.
//! - `api.js`: token refresh, `v1internal:retrieveUserQuota` buckets (gemini
//!   only), and `v1internal:fetchAvailableModels` across the three endpoints.
//! - `transforms.js`: `refreshToken|projectId|managedProjectId` parsing and
//!   per-model window transforms.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    antigravity_accounts_paths, as_non_empty_string, build_result, date_to_ms, field,
    get_auth_entry, normalize_auth_entry, read_json_file, to_number, to_usage_window,
};

pub const PROVIDER_ID: &str = "google";
pub const PROVIDER_NAME: &str = "Google";
pub const ALIASES: [&str; 2] = ["google", "google.oauth"];

const ANTIGRAVITY_GOOGLE_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const ANTIGRAVITY_GOOGLE_CLIENT_SECRET: &str = "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";
const GEMINI_GOOGLE_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GEMINI_GOOGLE_CLIENT_SECRET: &str = "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";
pub const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

const GOOGLE_PRIMARY_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_ENDPOINTS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://autopush-cloudcode-pa.sandbox.googleapis.com",
    GOOGLE_PRIMARY_ENDPOINT,
];

const REQUEST_TIMEOUT_MS: u64 = 15_000;
const FIVE_HOUR_WINDOW_SECONDS: f64 = 5.0 * 3600.0;
const DAILY_WINDOW_SECONDS: f64 = 24.0 * 3600.0;

/// `resolveGoogleOAuthClient`.
pub fn resolve_google_oauth_client(source_id: &str) -> (&'static str, &'static str) {
    if source_id == "gemini" {
        (GEMINI_GOOGLE_CLIENT_ID, GEMINI_GOOGLE_CLIENT_SECRET)
    } else {
        (
            ANTIGRAVITY_GOOGLE_CLIENT_ID,
            ANTIGRAVITY_GOOGLE_CLIENT_SECRET,
        )
    }
}

/// `parseGoogleRefreshToken` — `token|projectId|managedProjectId`.
pub fn parse_google_refresh_token(
    raw: Option<&Value>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(text) = raw.and_then(Value::as_str) else {
        return (None, None, None);
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (None, None, None);
    }
    let mut parts = trimmed.splitn(3, '|');
    let token = parts.next().map(str::to_string).filter(|t| !t.is_empty());
    let project = parts.next().map(str::to_string).filter(|p| !p.is_empty());
    let managed = parts.next().map(str::to_string).filter(|p| !p.is_empty());
    (token, project, managed)
}

#[derive(Debug, Clone)]
pub struct GoogleSource {
    pub source_id: &'static str,
    pub source_label: &'static str,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub project_id: Option<String>,
    pub expires: Option<i64>,
}

/// `resolveGeminiCliAuth` — the OpenCode auth entry with an optional nested
/// `oauth` object.
fn resolve_gemini_cli_auth(auth: &Value) -> Option<GoogleSource> {
    let entry = normalize_auth_entry(get_auth_entry(auth, &ALIASES))?;
    let entry_object = entry.as_object()?;
    let oauth_object = entry_object
        .get("oauth")
        .filter(|oauth| oauth.is_object())
        .unwrap_or(&entry);

    let access_token = field(oauth_object, "access")
        .and_then(as_non_empty_string)
        .or_else(|| field(oauth_object, "token").and_then(as_non_empty_string));
    let (refresh_token, project_id, managed_project_id) =
        parse_google_refresh_token(field(oauth_object, "refresh"));

    if access_token.is_none() && refresh_token.is_none() {
        return None;
    }

    Some(GoogleSource {
        source_id: "gemini",
        source_label: "Gemini",
        access_token,
        refresh_token,
        project_id: project_id.or(managed_project_id),
        expires: crate::quota::utils::to_timestamp(field(oauth_object, "expires")),
    })
}

/// `resolveAntigravityAuth` — first accounts file with a refresh token.
fn resolve_antigravity_auth(deps: &QuotaDeps) -> Option<GoogleSource> {
    for path in antigravity_accounts_paths(deps) {
        let Some(data) = read_json_file(&path) else {
            continue;
        };
        let Some(accounts) = field(&data, "accounts").and_then(Value::as_array) else {
            continue;
        };
        if accounts.is_empty() {
            continue;
        }
        let index = field(&data, "activeIndex")
            .and_then(Value::as_f64)
            .map(|index| index as usize)
            .unwrap_or(0);
        let account = accounts.get(index).or_else(|| accounts.first())?;
        let Some(raw_refresh) = field(account, "refreshToken") else {
            continue;
        };
        let (refresh_token, project_id, managed_project_id) =
            parse_google_refresh_token(Some(raw_refresh));
        let Some(refresh_token) = refresh_token else {
            continue;
        };
        return Some(GoogleSource {
            source_id: "antigravity",
            source_label: "Antigravity",
            access_token: None,
            refresh_token: Some(refresh_token),
            project_id: field(account, "projectId")
                .and_then(as_non_empty_string)
                .or_else(|| field(account, "managedProjectId").and_then(as_non_empty_string))
                .or(project_id)
                .or(managed_project_id),
            expires: None,
        });
    }
    None
}

/// `resolveGoogleAuthSources` — Gemini CLI first, Antigravity second.
pub fn resolve_google_auth_sources(deps: &QuotaDeps) -> Vec<GoogleSource> {
    let mut sources = Vec::new();
    if let Ok(auth) = deps.read_auth_value()
        && let Some(source) = resolve_gemini_cli_auth(&auth)
    {
        sources.push(source);
    }
    if let Some(source) = resolve_antigravity_auth(deps) {
        sources.push(source);
    }
    sources
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    !resolve_google_auth_sources(deps).is_empty()
}

// ============== api.js ==============

/// `refreshGoogleAccessToken` — None on any failure (per-source error).
async fn refresh_google_access_token(
    deps: &QuotaDeps,
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<Option<String>, String> {
    let form = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        urlencode(client_id),
        urlencode(client_secret),
        urlencode(refresh_token)
    );
    let request = HttpRequest::post("https://oauth2.googleapis.com/token").form_body(form);

    // JS: a network throw escapes the loop (registry catch); !ok → null.
    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;
    if !response.ok() {
        return Ok(None);
    }
    let data = response
        .json()
        .map_err(|_| "Invalid response from provider".to_string())?;
    Ok(field(&data, "access_token")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `fetchGoogleQuotaBuckets` — gemini-only quota buckets; None on any failure.
async fn fetch_google_quota_buckets(
    deps: &QuotaDeps,
    access_token: &str,
    project_id: &str,
) -> Option<Value> {
    let mut body = Map::new();
    if !project_id.is_empty() {
        body.insert("project".into(), json!(project_id));
    }
    let request = HttpRequest::post(format!(
        "{GOOGLE_PRIMARY_ENDPOINT}/v1internal:retrieveUserQuota"
    ))
    .bearer(access_token)
    .header("Content-Type", "application/json")
    .json_body(&Value::Object(body))
    .timeout(REQUEST_TIMEOUT_MS);
    let response = (deps.http)(request).await.ok()?;
    if !response.ok() {
        return None;
    }
    response.json().ok()
}

/// `fetchGoogleModels` — endpoints tried in order; None when all fail.
async fn fetch_google_models(
    deps: &QuotaDeps,
    access_token: &str,
    project_id: &str,
) -> Option<Value> {
    let mut body = Map::new();
    if !project_id.is_empty() {
        body.insert("project".into(), json!(project_id));
    }
    for endpoint in GOOGLE_ENDPOINTS {
        let request = HttpRequest::post(format!("{endpoint}/v1internal:fetchAvailableModels"))
            .bearer(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", "antigravity/1.11.5 windows/amd64")
            .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
            .header(
                "Client-Metadata",
                r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
            )
            .json_body(&Value::Object(body.clone()))
            .timeout(REQUEST_TIMEOUT_MS);
        if let Ok(response) = (deps.http)(request).await
            && response.ok()
        {
            return response.json().ok();
        }
    }
    None
}

// ============== transforms.js ==============

/// `resolveGoogleWindow` — gemini daily; antigravity 5h/daily by remaining.
fn resolve_google_window(source_id: &str, reset_at: Option<i64>, now: u64) -> (&'static str, f64) {
    if source_id == "antigravity" {
        if let Some(reset_at) = reset_at {
            let remaining_seconds = (((reset_at - now as i64) / 1000).max(0)) as f64;
            if remaining_seconds > 10.0 * 60.0 * 60.0 {
                return ("daily", DAILY_WINDOW_SECONDS);
            }
            return ("5h", FIVE_HOUR_WINDOW_SECONDS);
        }
        return ("5h", FIVE_HOUR_WINDOW_SECONDS);
    }
    ("daily", DAILY_WINDOW_SECONDS)
}

fn scoped_model_name(model_id: &str, source_id: &str) -> String {
    if model_id.starts_with(&format!("{source_id}/")) {
        model_id.to_string()
    } else {
        format!("{source_id}/{model_id}")
    }
}

fn model_windows_entry(
    label: &str,
    used_percent: Option<f64>,
    seconds: f64,
    reset_at: Option<i64>,
    now: u64,
) -> Value {
    let mut windows = Map::new();
    let reset_at_value = reset_at.map(|ms| json!(ms));
    windows.insert(
        label.to_string(),
        to_usage_window(
            now,
            used_percent,
            Some(seconds),
            reset_at_value.as_ref(),
            None,
        ),
    );
    json!({ "windows": windows })
}

/// `transformQuotaBucket`.
pub fn transform_quota_bucket(
    bucket: &Value,
    source_id: &str,
    now: u64,
) -> Option<(String, Value)> {
    let model_id = field(bucket, "modelId").and_then(as_non_empty_string)?;
    let remaining_fraction = to_number(field(bucket, "remainingFraction"));
    let remaining_percent = remaining_fraction.map(|fraction| (fraction * 100.0).round());
    let used_percent = remaining_percent.map(|remaining| (100.0 - remaining).max(0.0));
    let reset_at = crate::quota::utils::to_timestamp(field(bucket, "resetTime"));
    let (label, seconds) = resolve_google_window(source_id, reset_at, now);
    Some((
        scoped_model_name(&model_id, source_id),
        model_windows_entry(label, used_percent, seconds, reset_at, now),
    ))
}

/// `transformModelData`.
pub fn transform_model_data(
    model_name: &str,
    model_data: &Value,
    source_id: &str,
    now: u64,
) -> (String, Value) {
    let quota_info = field(model_data, "quotaInfo");
    let remaining_fraction = quota_info
        .and_then(|info| field(info, "remainingFraction"))
        .and_then(Value::as_f64);
    let remaining_percent = remaining_fraction.map(|fraction| (fraction * 100.0).round());
    let used_percent = remaining_percent.map(|remaining| (100.0 - remaining).max(0.0));
    let reset_at = quota_info
        .and_then(|info| field(info, "resetTime"))
        .and_then(date_to_ms);
    let (label, seconds) = resolve_google_window(source_id, reset_at, now);
    (
        scoped_model_name(model_name, source_id),
        model_windows_entry(label, used_percent, seconds, reset_at, now),
    )
}

pub fn fetch_google_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let auth_sources = resolve_google_auth_sources(&deps);
        if auth_sources.is_empty() {
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

        let mut models = Map::new();
        let mut source_errors: Vec<String> = Vec::new();

        for source in &auth_sources {
            let now = deps.now_ms();
            let mut access_token = source.access_token.clone();

            let expired = source
                .expires
                .map(|expires| expires <= now as i64)
                .unwrap_or(false);
            if access_token.is_none() || expired {
                let Some(refresh_token) = source.refresh_token.as_deref() else {
                    source_errors.push(format!("{}: Missing refresh token", source.source_label));
                    continue;
                };
                let (client_id, client_secret) = resolve_google_oauth_client(source.source_id);
                match refresh_google_access_token(&deps, refresh_token, client_id, client_secret)
                    .await
                {
                    Ok(Some(token)) => access_token = Some(token),
                    Ok(None) => {
                        source_errors.push(format!(
                            "{}: Failed to refresh OAuth token",
                            source.source_label
                        ));
                        continue;
                    }
                    Err(message) => {
                        // JS: the fetch throw escapes the loop into the
                        // registry catch.
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
                }
            }

            let Some(access_token) = access_token.filter(|token| !token.is_empty()) else {
                source_errors.push(format!(
                    "{}: Failed to refresh OAuth token",
                    source.source_label
                ));
                continue;
            };

            let project_id = source
                .project_id
                .clone()
                .unwrap_or_else(|| DEFAULT_PROJECT_ID.to_string());
            let mut merged_any_model = false;

            if source.source_id == "gemini"
                && let Some(quota_payload) =
                    fetch_google_quota_buckets(&deps, &access_token, &project_id).await
            {
                let buckets = field(&quota_payload, "buckets")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for bucket in &buckets {
                    if let Some((name, entry)) =
                        transform_quota_bucket(bucket, source.source_id, deps.now_ms())
                    {
                        models.insert(name, entry);
                        merged_any_model = true;
                    }
                }
            }

            if let Some(payload) = fetch_google_models(&deps, &access_token, &project_id).await
                && let Some(payload_models) = field(&payload, "models").and_then(Value::as_object)
            {
                for (model_name, model_data) in payload_models {
                    let (name, entry) = transform_model_data(
                        model_name,
                        model_data,
                        source.source_id,
                        deps.now_ms(),
                    );
                    models.insert(name, entry);
                    merged_any_model = true;
                }
            }

            if !merged_any_model {
                source_errors.push(format!("{}: Failed to fetch models", source.source_label));
            }
        }

        if models.is_empty() {
            let message = source_errors
                .first()
                .cloned()
                .unwrap_or_else(|| "Failed to fetch models".to_string());
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

        build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(json!({ "windows": {}, "models": models })),
            None,
            None,
            deps.now_ms(),
        )
    })
}

pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_google_quota(rt)
}
