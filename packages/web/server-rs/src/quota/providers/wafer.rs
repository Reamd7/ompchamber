//! Port of `server/lib/quota/providers/wafer.js` — Wafer.ai quota via
//! `GET https://pass.wafer.ai/v1/inference/quota` (15s timeout).
//!
//! 中文概览：Wafer.ai 配额提供方——以 bearer API key 请求
//! pass.wafer.ai/v1/inference/quota（15 秒超时），由剩余/限额请求数、
//! 超额计数与当前周期用量百分比组装单个用量窗口。

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    as_non_empty_string, build_result, field, get_auth_entry, normalize_auth_entry, num_str,
    resolve_window_label, to_number, to_timestamp, to_usage_window, usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "wafer";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "Wafer.ai";
/// auth.json 中识别本提供方的别名列表（四种等价写法）。
pub const ALIASES: [&str; 4] = ["wafer", "wafer-ai", "wafer_ai", "wafer.ai"];

/// 配额查询端点 URL。
const WAFER_QUOTA_URL: &str = "https://pass.wafer.ai/v1/inference/quota";
/// 窗口起止时间缺失时使用的默认窗口时长（5 小时）。
const WAFER_WINDOW_SECONDS: f64 = 5.0 * 3600.0;
/// 请求超时时间（毫秒）。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 从 auth.json 的 wafer 别名条目读取 API key（优先 key 字段，回退 token 字段）；
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

/// 注册表入口：读 key → 请求配额端点 → 由 remaining/limit/overage/
/// used_percent 组装单个窗口（存在超额请求时百分比不封顶），
/// 标签依次拼接套餐档位、"N / M left" 与 "+N overage"；
/// 四类数据全部缺失时按失败处理。
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

        let request = HttpRequest::get(WAFER_QUOTA_URL)
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
            return failure(format!("API error: {}", response.status), deps.now_ms());
        }
        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Invalid response from provider".into(), deps.now_ms()),
        };

        let remaining = to_number(field(&payload, "remaining_included_requests"));
        let limit = to_number(field(&payload, "included_request_limit"));
        let overage = to_number(field(&payload, "overage_request_count"));
        let used_percent_raw = to_number(field(&payload, "current_period_used_percent"));
        let window_start = to_timestamp(field(&payload, "window_start"));
        let window_end = to_timestamp(field(&payload, "window_end"));
        let plan_tier = field(&payload, "plan_tier").and_then(as_non_empty_string);

        if remaining.is_none() && limit.is_none() && overage.is_none() && used_percent_raw.is_none()
        {
            return failure("No quota data in response".into(), deps.now_ms());
        }

        let has_overage = overage.map(|overage| overage > 0.0).unwrap_or(false);
        let used_percent = match used_percent_raw {
            Some(raw) if has_overage => Some(raw.max(0.0)),
            Some(raw) => Some(raw.clamp(0.0, 100.0)),
            None => None,
        };

        let window_seconds = match (window_start, window_end) {
            (Some(start), Some(end)) => (((end - start) as f64) / 1000.0).round(),
            _ => WAFER_WINDOW_SECONDS,
        };
        let window_label = resolve_window_label(Some(window_seconds));

        let value_label = match (remaining, limit) {
            (Some(remaining), Some(limit)) => {
                let mut parts = Vec::new();
                if let Some(tier) = plan_tier.as_deref() {
                    parts.push(tier.to_string());
                }
                parts.push(format!("{} / {} left", num_str(remaining), num_str(limit)));
                if has_overage && let Some(overage) = overage {
                    parts.push(format!("+{} overage", num_str(overage)));
                }
                Some(parts.join(" \u{b7} "))
            }
            _ => None,
        };

        let mut windows = Map::new();
        windows.insert(
            window_label,
            to_usage_window(
                now,
                used_percent,
                Some(window_seconds),
                window_end.map(|ms| json!(ms)).as_ref(),
                value_label.as_deref(),
            ),
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
