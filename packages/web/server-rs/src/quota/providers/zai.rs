//! Port of `server/lib/quota/providers/zai.js` — z.ai Coding Plan quota via
//! `GET https://api.z.ai/api/monitor/usage/quota/limit`.
//!
//! TOKENS_LIMIT and the renamed CREDIT_LIMIT entries map to the same windows;
//! CREDIT_LIMIT entries additionally carry usage/currentValue/remaining which
//! surface as a credit `valueLabel`, and `data.level` becomes `planLabel`.
//!
//! 中文概览：z.ai Coding Plan 配额提供方——以 bearer API key 请求
//! quota/limit 端点，把 limits 中的 TOKENS_LIMIT 与改名后的 CREDIT_LIMIT
//! 条目映射为同名窗口（CREDIT_LIMIT 额外带 usage/currentValue，展示为
//! credits valueLabel），TIME_LIMIT 映射为固定 30 天的 "MCP Tools" 窗口，
//! data.level 作为 planLabel 透传。

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, normalize_timestamp, num_str,
    resolve_window_label, resolve_window_seconds, to_number, to_usage_window, usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "zai-coding-plan";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "z.ai";
/// auth.json 中识别本提供方的别名列表（含简写 zai 与域名写法 z.ai）。
pub const ALIASES: [&str; 3] = ["zai-coding-plan", "zai", "z.ai"];

/// 配额限额查询端点 URL。
const LIMIT_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
/// "MCP Tools" 窗口使用的固定时长（30 天，API 未返回窗口秒数）。
const MCP_TOOLS_WINDOW_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

/// `formatCreditAmount` — plain under 1k, `12k`-style above.
/// 格式化 credit 数量：低于 1000 原样输出，否则缩写成 "12k" 风格。
pub fn format_credit_amount(value: f64) -> String {
    if value < 1000.0 {
        num_str(value)
    } else {
        format!("{}k", num_str((value / 100.0).round() / 10.0))
    }
}

/// `formatCreditValueLabel` — `used / total credits`.
/// 由 limit 条目的 currentValue(已用) 与 usage(总量) 拼 "used / total credits"
/// 展示标签；任一字段缺失返回 None。
pub fn format_credit_value_label(limit: &Value) -> Option<String> {
    let used = to_number(field(limit, "currentValue"))?;
    let total = to_number(field(limit, "usage"))?;
    Some(format!(
        "{} / {} credits",
        format_credit_amount(used),
        format_credit_amount(total)
    ))
}

/// 从 auth.json 的 z.ai 别名条目读取 API key（优先 key 字段，回退 token 字段）；
/// auth 文件读取失败时返回 Err（携带错误消息）。
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

/// 注册表入口：读 key → 请求限额端点 → 遍历 limits 组装窗口
/// （TOKENS_LIMIT/CREDIT_LIMIT 按窗口秒数归类，TIME_LIMIT 固定 30 天），
/// 成功时携带 data.level 作为套餐标签。
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

        let request = HttpRequest::get(LIMIT_URL)
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

        let limits = field(&payload, "data")
            .and_then(|data| field(data, "limits"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut windows = Map::new();
        for limit in &limits {
            let limit_type = field(limit, "type").and_then(Value::as_str).unwrap_or("");
            if limit_type != "TOKENS_LIMIT" && limit_type != "CREDIT_LIMIT" {
                continue;
            }
            let window_seconds = resolve_window_seconds(limit);
            let window_label = resolve_window_label(window_seconds);
            let reset_at = field(limit, "nextResetTime")
                .filter(|value| value.is_number() && value.as_f64() != Some(0.0))
                .and_then(normalize_timestamp)
                .map(|ms| json!(ms));
            let used_percent = field(limit, "percentage").and_then(Value::as_f64);
            let credit_value_label = format_credit_value_label(limit);
            windows.insert(
                window_label,
                to_usage_window(
                    now,
                    used_percent,
                    window_seconds,
                    reset_at.as_ref(),
                    credit_value_label.as_deref(),
                ),
            );
        }

        if let Some(mcp_limit) = limits
            .iter()
            .find(|limit| field(limit, "type").and_then(Value::as_str) == Some("TIME_LIMIT"))
        {
            let reset_at = field(mcp_limit, "nextResetTime")
                .filter(|value| value.is_number() && value.as_f64() != Some(0.0))
                .and_then(normalize_timestamp)
                .map(|ms| json!(ms));
            let used_percent = field(mcp_limit, "percentage").and_then(Value::as_f64);
            windows.insert(
                "MCP Tools".into(),
                to_usage_window(
                    now,
                    used_percent,
                    Some(MCP_TOOLS_WINDOW_SECONDS),
                    reset_at.as_ref(),
                    None,
                ),
            );
        }

        let plan_label = field(&payload, "data")
            .and_then(|data| field(data, "level"))
            .and_then(Value::as_str)
            .filter(|level| !level.is_empty());

        build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            plan_label,
            deps.now_ms(),
        )
    })
}
