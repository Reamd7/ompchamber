//! Port of `server/lib/quota/providers/nanogpt.js` — NanoGPT subscription
//! usage via `GET https://nano-gpt.com/api/subscription/v1/usage`.
//!
//! 中文概览：NanoGPT 订阅配额提供方——以 bearer API key 请求
//! nano-gpt.com 的 subscription/v1/usage 端点，把响应中的 daily 与
//! monthly 两块组装成统一的用量窗口；订阅 state 非 active 时在窗口
//! 标签上附加 "(state)" 后缀。

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "nano-gpt";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "NanoGPT";
/// auth.json 中识别本提供方的别名列表（nano-gpt/nanogpt/nano_gpt 三种写法）。
pub const ALIASES: [&str; 3] = ["nano-gpt", "nanogpt", "nano_gpt"];

/// 用量查询端点 URL。
const USAGE_URL: &str = "https://nano-gpt.com/api/subscription/v1/usage";
/// daily 窗口的固定时长（一天，API 未返回窗口秒数）。
const DAILY_WINDOW_SECONDS: f64 = 86_400.0;

/// 从 auth.json 的 nano-gpt 别名条目读取 API key（优先 key 字段，回退 token 字段）；
/// auth 文件读取失败返回 Err（携带错误消息）。
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

/// 是否已配置：能读到 API key 即视为已配置。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_api_key(deps).unwrap_or(None).is_some()
}

/// `percentUsed` is a 0-1 fraction; the used/limit fallback computes one
/// (`block.limit ?? block.limits.<limits_key>`).
/// 计算单个用量块的已用百分比：优先用 percentUsed（0-1 小数，乘 100 并
/// 夹取到 [0,100]）；否则回退 used / limit（limit 缺失时取
/// limits.<limits_key>）；无法计算返回 None。
fn window_used_percent(block: &Value, limits_key: &str) -> Option<f64> {
    if let Some(percent_used) = field(block, "percentUsed").and_then(Value::as_f64) {
        return Some((percent_used * 100.0).clamp(0.0, 100.0));
    }
    let used = to_number(field(block, "used"));
    let limit = to_number(field(block, "limit"))
        .or_else(|| field(block, "limits").and_then(|limits| to_number(field(limits, limits_key))));
    match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => {
            Some(((used / limit) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    }
}

/// 注册表入口：读 key（缺失视为未配置）→ 请求用量端点 → 组装 daily
/// （固定 24 小时窗口）与 monthly（reset 缺失时回退 period.currentPeriodEnd）
/// 窗口，非 active 状态附加状态标签，返回统一结果。
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

        let request = HttpRequest::get(USAGE_URL)
            .bearer(&api_key)
            .header("Content-Type", "application/json");

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
            return build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
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

        let period = field(&payload, "period");
        let state = field(&payload, "state")
            .and_then(Value::as_str)
            .filter(|state| !state.is_empty())
            .unwrap_or("active")
            .to_string();
        let state_label = (state != "active").then(|| format!("({state})"));

        let mut windows = Map::new();

        if let Some(daily) = field(&payload, "daily") {
            let reset_at = to_timestamp(field(daily, "resetAt"));
            windows.insert(
                "daily".into(),
                to_usage_window(
                    now,
                    window_used_percent(daily, "daily"),
                    Some(DAILY_WINDOW_SECONDS),
                    reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    state_label.as_deref(),
                ),
            );
        }

        if let Some(monthly) = field(&payload, "monthly") {
            let reset_at = to_timestamp(field(monthly, "resetAt")).or_else(|| {
                period.and_then(|period| to_timestamp(field(period, "currentPeriodEnd")))
            });
            windows.insert(
                "monthly".into(),
                to_usage_window(
                    now,
                    window_used_percent(monthly, "monthly"),
                    None,
                    reset_at.map(|ms| serde_json::json!(ms)).as_ref(),
                    state_label.as_deref(),
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
