//! Port of `server/lib/quota/providers/openai.js` — internal-only logic twin
//! of the Codex provider (`fetchOpenaiQuota` is exported from the JS module
//! but intentionally not registered for dispatcher routing).
//!
//! 中文概览：openai.js 的内部孪生实现——逻辑与 Codex 提供方一致
//!（相同的 wham/usage 端点与凭据来源），但 fetchOpenaiQuota 在 JS 侧
//! 刻意未注册到 dispatcher 路由，仅作为内部逻辑对照保留。

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

/// 提供方唯一标识。
pub const PROVIDER_ID: &str = "openai";
/// 提供方展示名称（用于 UI 渲染）。
pub const PROVIDER_NAME: &str = "OpenAI";
/// auth.json 中识别本提供方的别名列表（与 Codex 提供方共用 openai/codex/chatgpt）。
pub const ALIASES: [&str; 3] = ["openai", "codex", "chatgpt"];

/// 用量查询端点 URL（与 Codex 提供方相同）。
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// 从 auth.json 的 openai/codex/chatgpt 别名条目读取 access token
///（优先 access 字段，回退 token 字段）；读取失败返回 Err。
fn load_access_token(deps: &QuotaDeps) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, &ALIASES));
    Ok(entry.and_then(|entry| {
        field(&entry, "access")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

/// 是否已配置：能读到 access token 即视为已配置。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_access_token(deps).unwrap_or(None).is_some()
}

/// 拉取流程（内部使用，未注册到路由）：读 token（缺失视为未配置）→
/// 请求 wham/usage → 把 rate_limit.primary_window/secondary_window
/// 分别映射为 "5h"/"weekly" 窗口，返回统一结果。
pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let access_token = match load_access_token(&deps) {
            Ok(Some(token)) => token,
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
            .bearer(&access_token)
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

        let rate_limit = field(&payload, "rate_limit");
        let mut windows = Map::new();
        for (window_key, label) in [("primary_window", "5h"), ("secondary_window", "weekly")] {
            let Some(window) = rate_limit.and_then(|limits| field(limits, window_key)) else {
                continue;
            };
            let reset_at = to_timestamp(field(window, "reset_at"));
            windows.insert(
                label.into(),
                to_usage_window(
                    now,
                    to_number(field(window, "used_percent")),
                    to_number(field(window, "limit_window_seconds")),
                    reset_at.map(|ms| json!(ms)).as_ref(),
                    None,
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
