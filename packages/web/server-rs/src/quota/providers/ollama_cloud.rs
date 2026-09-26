//! Port of `server/lib/quota/providers/ollama-cloud.js` — Ollama Cloud usage
//! scraped from `https://ollama.com/settings` with the managed cookie.
//!
//! Redirects are followed manually (rejected): credentials must never be
//! forwarded to a redirect target. The settings page is parsed with the same
//! three patterns the JS regexes match.

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::credentials::read_managed_credential;
use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{build_result, to_number, to_usage_window, usage_payload};

pub const PROVIDER_ID: &str = "ollama-cloud";
pub const PROVIDER_NAME: &str = "Ollama Cloud";

const SETTINGS_URL: &str = "https://ollama.com/settings";
const REQUEST_TIMEOUT_MS: u64 = 15_000;

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Scan for the first `[0-9.]+` run directly followed by `%` at or after
/// `start` (the `[^0-9]*([0-9.]+)%` tail of the JS regexes).
fn percent_after(haystack: &str, start: usize) -> Option<String> {
    let bytes = haystack.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() || bytes[index] == b'.' {
            let run_start = index;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                index += 1;
            }
            if index < bytes.len() && bytes[index] == b'%' {
                return Some(haystack[run_start..index].to_string());
            }
        } else {
            index += 1;
        }
    }
    None
}

/// The `Word\s+usage[^0-9]*([0-9.]+)%` pattern (case-insensitive): the word
/// must be immediately followed by whitespace and then `usage`; the percent
/// may appear anywhere after (the `[^0-9]*` run is unbounded).
fn usage_percent(html: &str, word: &str) -> Option<f64> {
    let bytes = html.as_bytes();
    let mut search_from = 0;
    while let Some(word_at) = find_case_insensitive(&html[search_from..], word) {
        let after_word = search_from + word_at + word.len();
        let mut index = after_word;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let has_whitespace = index > after_word;
        let usage_follows = index + 5 <= html.len()
            && html.is_char_boundary(index + 5)
            && html[index..index + 5].eq_ignore_ascii_case("usage");
        if has_whitespace && usage_follows {
            let after_usage = index + 5;
            return percent_after(html, after_usage)
                .and_then(|raw| to_number(Some(&Value::String(raw))));
        }
        search_from = after_word;
    }
    None
}

/// The `Premium[^0-9]*([0-9]+)\s*/\s*([0-9]+)` pattern (case-insensitive).
fn premium_counts(html: &str) -> Option<(Option<f64>, Option<f64>)> {
    let premium_at = find_case_insensitive(html, "premium")? + "premium".len();
    let bytes = html.as_bytes();
    let mut index = premium_at;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let first_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            let first = html[first_start..index].to_string();
            let mut cursor = index;
            while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'/' {
                cursor += 1;
                while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
                    cursor += 1;
                }
                if cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    let second_start = cursor;
                    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                        cursor += 1;
                    }
                    let second = html[second_start..cursor].to_string();
                    return Some((
                        to_number(Some(&Value::String(first))),
                        to_number(Some(&Value::String(second))),
                    ));
                }
            }
        } else {
            index += 1;
        }
    }
    None
}

/// `parseOllamaSettingsHtml` — session/weekly percent windows plus the
/// premium `used / total` window.
pub fn parse_ollama_settings_html(html: &str, now: u64) -> Map<String, Value> {
    let mut windows = Map::new();
    if let Some(session) = usage_percent(html, "session") {
        windows.insert(
            "session".into(),
            to_usage_window(now, Some(session), None, None, None),
        );
    }
    if let Some(weekly) = usage_percent(html, "weekly") {
        windows.insert(
            "weekly".into(),
            to_usage_window(now, Some(weekly), None, None, None),
        );
    }
    if let Some((used, total)) = premium_counts(html) {
        let used_percent = match (total, used) {
            (Some(total), Some(used)) if total != 0.0 => Some(((used / total) * 100.0).min(100.0)),
            _ => None,
        };
        let label = format!(
            "{} / {}",
            used.map(crate::quota::utils::num_str)
                .unwrap_or_else(|| "0".into()),
            total
                .map(crate::quota::utils::num_str)
                .unwrap_or_else(|| "0".into())
        );
        windows.insert(
            "premium".into(),
            to_usage_window(now, used_percent, None, None, Some(&label)),
        );
    }
    windows
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    read_managed_credential(deps, PROVIDER_ID).is_some()
}

/// `fetchOllamaCloudUsage` — also the credential validator for the routes.
pub async fn fetch_ollama_cloud_usage(
    deps: &QuotaDeps,
    credential: &Value,
) -> Result<Map<String, Value>, String> {
    let cookie = credential
        .get("cookie")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let request = HttpRequest::get(SETTINGS_URL)
        .header("Cookie", cookie)
        .header("User-Agent", "OMPChamber quota provider")
        .manual_redirect()
        .timeout(REQUEST_TIMEOUT_MS);

    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;

    if response.status == 401 || response.status == 403 || (300..400).contains(&response.status) {
        return Err("Ollama Cloud authentication failed".to_string());
    }
    if !response.ok() {
        return Err(format!("Ollama Cloud returned HTTP {}", response.status));
    }

    let windows = parse_ollama_settings_html(&response.text(), deps.now_ms());
    if windows.is_empty() {
        return Err("Ollama Cloud usage data could not be parsed".to_string());
    }
    Ok(windows)
}

pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let Some(credential) = read_managed_credential(&deps, PROVIDER_ID) else {
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
        };

        match fetch_ollama_cloud_usage(&deps, &credential).await {
            Ok(windows) => build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                true,
                true,
                Some(usage_payload(windows, None)),
                None,
                None,
                deps.now_ms(),
            ),
            Err(message) => build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some(&message),
                None,
                deps.now_ms(),
            ),
        }
    })
}
