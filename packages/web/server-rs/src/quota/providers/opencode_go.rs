//! Port of `server/lib/quota/providers/opencode-go.js` — OpenCode Go usage
//! via `GET https://opencode.ai/zen/go/v1/usage` (API key from OpenCode
//! `auth.json`). On the first refresh after the upgrade the obsolete
//! `quota/opencode-go.json` cookie file is deleted without reading it.
//!
//! 中文概览：OpenCode Go 用量提供方——API key 取自 OpenCode auth.json，
//! 请求 opencode.ai/zen/go/v1/usage。升级后的首次刷新会把废弃的
//! quota/opencode-go.json cookie 凭据文件直接删除（不读取其内容）。

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::credentials::delete_legacy_opencode_go_credential;
use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, parse_iso_ms, to_usage_window,
    usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "opencode-go";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "OpenCode Go";
/// auth.json 中识别本提供方的别名列表。
pub const ALIASES: [&str; 1] = ["opencode-go"];

/// 用量查询端点 URL。
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
/// 请求超时时间（毫秒）。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 窗口名到 API usage 字段名的映射：rolling→5h、weekly→weekly、monthly→monthly。
const WINDOWS_BY_API_KEY: [(&str, &str); 3] = [
    ("5h", "rolling"),
    ("weekly", "weekly"),
    ("monthly", "monthly"),
];

/// `parseOpenCodeGoUsage` — windows keyed 5h/weekly/monthly from the API's
/// rolling/weekly/monthly entries; both `percent` and a parseable `resetsAt`
/// are required.
/// 解析用量响应：按 WINDOWS_BY_API_KEY 依次取 usage.rolling/weekly/monthly
/// 条目，percent 与可解析的 resetsAt 两者缺一不可（NaN percent 也跳过），
/// 组装 5h/weekly/monthly 窗口；usage 块缺失返回空 Map。
pub fn parse_opencode_go_usage(payload: Option<&Value>, now: u64) -> Map<String, Value> {
    let mut windows = Map::new();
    let Some(usage) = payload.and_then(|payload| field(payload, "usage")) else {
        return windows;
    };
    for (window_key, api_key) in WINDOWS_BY_API_KEY {
        let Some(entry) = field(usage, api_key) else {
            continue;
        };
        let Some(used_percent) = field(entry, "percent").and_then(Value::as_f64) else {
            continue;
        };
        if !used_percent.is_finite() {
            continue;
        }
        let Some(reset_at) = field(entry, "resetsAt").and_then(Value::as_str) else {
            continue;
        };
        if parse_iso_ms(reset_at).is_none() {
            continue;
        }
        windows.insert(
            window_key.into(),
            to_usage_window(
                now,
                Some(used_percent.clamp(0.0, 100.0)),
                None,
                Some(&Value::String(reset_at.to_string())),
                None,
            ),
        );
    }
    windows
}

/// 从 auth.json 的 opencode-go 条目读取 API key（优先 key 字段，回退 token 字段）；
/// auth 文件读取失败返回 Err（携带错误消息）。
fn get_api_key(deps: &QuotaDeps) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    Ok(entry.and_then(|entry| {
        field(&entry, "key")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

/// 是否已配置：能读到 API key 即视为已配置。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    get_api_key(deps).unwrap_or(None).is_some()
}

/// `fetchOpenCodeGoUsage` — also the credential validator seam for tests.
/// 以 bearer key 请求用量端点并解析窗口；该函数同时是测试注入的
/// 凭据校验接缝。401/403 报认证失败，其它非 2xx 报 HTTP 状态码，
/// 解析不出任何窗口报解析错误。
pub async fn fetch_opencode_go_usage(
    deps: &QuotaDeps,
    api_key: &str,
) -> Result<Map<String, Value>, String> {
    let request = HttpRequest::get(USAGE_URL)
        .header("Accept", "application/json")
        .bearer(api_key)
        .header("User-Agent", "OMPChamber quota provider")
        .timeout(REQUEST_TIMEOUT_MS);

    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;

    if response.status == 401 || response.status == 403 {
        return Err("OpenCode Go authentication failed".to_string());
    }
    if !response.ok() {
        return Err(format!(
            "OpenCode Go usage API returned HTTP {}",
            response.status
        ));
    }
    let payload = response.json().ok();
    let windows = parse_opencode_go_usage(payload.as_ref(), deps.now_ms());
    if windows.is_empty() {
        return Err("OpenCode Go usage data could not be parsed".to_string());
    }
    Ok(windows)
}

/// 注册表入口：先删除废弃的 legacy cookie 凭据文件，再读 key
///（缺失视为未配置）、拉取用量并组装统一结果。
pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        // Remove the obsolete browser-cookie credential without reading it.
        delete_legacy_opencode_go_credential(&deps);

        let api_key = match get_api_key(&deps) {
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

        match fetch_opencode_go_usage(&deps, &api_key).await {
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
