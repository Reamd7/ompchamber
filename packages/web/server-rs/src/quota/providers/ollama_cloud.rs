//! Port of `server/lib/quota/providers/ollama-cloud.js` — Ollama Cloud usage
//! scraped from `https://ollama.com/settings` with the managed cookie.
//!
//! Redirects are followed manually (rejected): credentials must never be
//! forwarded to a redirect target. The settings page is parsed with the same
//! three patterns the JS regexes match.
//!
//! 中文概览：Ollama Cloud 配额提供方——用托管的 cookie 凭据抓取
//! ollama.com/settings 页面并从中解析用量。重定向一律手动跟随（此处直接
//! 视为失败），避免把凭据转发给重定向目标；页面解析复刻 JS 版三个正则
//! 的语义（session/weekly 百分比与 premium used/total）。

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::credentials::read_managed_credential;
use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{build_result, to_number, to_usage_window, usage_payload};

/// 提供方唯一标识（同时作为托管凭据的存储 key）。
pub const PROVIDER_ID: &str = "ollama-cloud";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "Ollama Cloud";

/// 抓取用量的 Ollama 设置页 URL。
const SETTINGS_URL: &str = "https://ollama.com/settings";
/// 请求超时时间（毫秒）。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 在 haystack 中查找 needle 首次出现位置（ASCII 大小写不敏感）；找不到返回 None。
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
/// 从 start 起查找第一段后紧跟 `%` 的 `[0-9.]+` 数字串，
/// 等价于 JS 正则的 `[^0-9]*([0-9.]+)%` 尾部；找不到返回 None。
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
/// 匹配 `<word>\s+usage[^0-9]*([0-9.]+)%` 模式（大小写不敏感）：单词后必须
/// 紧跟空白与 usage，百分比可在其后任意位置出现；返回该百分比的数值。
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
/// 匹配 `Premium[^0-9]*([0-9]+)\s*/\s*([0-9]+)` 模式（大小写不敏感），
/// 返回 (已用数, 总数) 数字对。
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
/// 解析设置页 HTML：session 与 weekly 用量百分比窗口，加上 premium 的
/// used/total 窗口（标签栏展示 "used / total" 文本，百分比封顶 100）；
/// 一个窗口都解析不到时返回空 Map。
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

/// 是否已配置：存在本提供方的托管 cookie 凭据即视为已配置。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    read_managed_credential(deps, PROVIDER_ID).is_some()
}

/// `fetchOllamaCloudUsage` — also the credential validator for the routes.
/// 带 cookie 请求设置页并解析用量窗口；该函数同时被 routes 用作凭据
/// 有效性校验。401/403/3xx 视为认证失败，其它非 2xx 报 HTTP 状态码，
/// 解析不到任何窗口报解析错误。
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

/// 注册表入口：读取托管凭据（缺失视为未配置），抓取并解析设置页，
/// 成功/失败分别组装统一的 quota 结果 JSON。
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
