//! Port of `server/lib/quota/providers/copilot.js` — GitHub Copilot (and the
//! Add-on twin) via `GET https://api.github.com/copilot_internal/user`.
//!
//! 中文概览：GitHub Copilot 配额提供方（含 Add-on 孪生实例）——以
//! GitHub token 请求 copilot_internal/user 端点，从 quota_snapshots 的
//! premium_interactions 快照组装用量窗口，语义与
//! microsoft/vscode-copilot-chat 的配额展示保持一致。

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

/// 主实例的提供方唯一标识。
pub const PROVIDER_ID: &str = "github-copilot";
/// 主实例的提供方展示名称。
pub const PROVIDER_NAME: &str = "GitHub Copilot";
/// Add-on 孪生实例的提供方唯一标识（复用同一套认证与解析逻辑）。
pub const PROVIDER_ID_ADDON: &str = "github-copilot-addon";
/// Add-on 孪生实例的提供方展示名称。
pub const PROVIDER_NAME_ADDON: &str = "GitHub Copilot Add-on";
/// auth.json 中识别本提供方的别名列表（github-copilot 与简写 copilot）。
pub const ALIASES: [&str; 2] = ["github-copilot", "copilot"];

/// Copilot 内部用户/配额查询端点 URL。
const USER_URL: &str = "https://api.github.com/copilot_internal/user";

/// `buildCopilotWindows` — only the `premium_interactions` snapshot is
/// exposed (mirrors microsoft/vscode-copilot-chat quota semantics).
/// 从 copilot_internal/user 响应组装窗口：仅暴露 premium_interactions
/// 一个快照——unlimited 时标签显示 "Unlimited"；否则用 entitlement 与
/// remaining 计算/回退 percent_remaining 得到已用百分比，并拼出
/// "remaining / entitlement left" 标签；无快照时返回空 Map。
pub fn build_copilot_windows(payload: &Value, now: u64) -> Map<String, Value> {
    let quota = field(payload, "quota_snapshots");
    let reset_at = to_timestamp(field(payload, "quota_reset_date"));
    let reset_at = reset_at.map(|ms| json!(ms));
    let mut windows = Map::new();

    let Some(snapshot) = quota.and_then(|snapshots| field(snapshots, "premium_interactions"))
    else {
        return windows;
    };

    if field(snapshot, "unlimited") == Some(&Value::Bool(true)) {
        windows.insert(
            "premium_interactions".into(),
            to_usage_window(now, None, None, reset_at.as_ref(), Some("Unlimited")),
        );
        return windows;
    }

    let entitlement = to_number(field(snapshot, "entitlement"));
    let remaining = to_number(field(snapshot, "remaining"));
    let mut used_percent = match (entitlement, remaining) {
        (Some(entitlement), Some(remaining)) if entitlement > 0.0 => {
            Some((100.0 - (remaining / entitlement) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    if used_percent.is_none()
        && let Some(percent_remaining) = to_number(field(snapshot, "percent_remaining"))
    {
        used_percent = Some((100.0 - percent_remaining).clamp(0.0, 100.0));
    }
    let value_label = match (entitlement, remaining) {
        (Some(entitlement), Some(remaining)) if entitlement > 0.0 => Some(format!(
            "{} / {} left",
            crate::quota::utils::js_to_fixed(remaining, 0),
            crate::quota::utils::js_to_fixed(entitlement, 0)
        )),
        _ => None,
    };
    windows.insert(
        "premium_interactions".into(),
        to_usage_window(
            now,
            used_percent,
            None,
            reset_at.as_ref(),
            value_label.as_deref(),
        ),
    );
    windows
}

/// 从 auth.json 的 copilot 别名条目读取 access token（优先 access 字段，
/// 回退 token 字段）；auth 文件读取失败返回 Err（携带错误消息）。
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

/// 构造对 USER_URL 的 GET 请求：token 鉴权 + 伪装 VS Code Copilot 的
/// Editor-Version 与 GitHub API 版本请求头。
fn copilot_request(access_token: &str) -> HttpRequest {
    HttpRequest::get(USER_URL)
        .header("Authorization", format!("token {access_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", "vscode/1.96.2")
        .header("X-Github-Api-Version", "2025-04-01")
}

/// 两个实例共用的拉取流程：读 token（缺失视为未配置）→ 请求用户端点 →
/// build_copilot_windows 组装窗口并返回统一结果；
/// provider_id/provider_name 参数区分主实例与 Add-on 实例。
fn fetch_for(
    rt: Arc<QuotaRuntime>,
    provider_id: &'static str,
    provider_name: &'static str,
) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let access_token = match load_access_token(&deps) {
            Ok(Some(token)) => token,
            Ok(None) => {
                return build_result(
                    provider_id,
                    provider_name,
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
                    provider_id,
                    provider_name,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    now,
                );
            }
        };

        let response = match (deps.http)(copilot_request(&access_token)).await {
            Ok(response) => response,
            Err(error) => {
                return build_result(
                    provider_id,
                    provider_name,
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
                provider_id,
                provider_name,
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
                    provider_id,
                    provider_name,
                    false,
                    true,
                    None,
                    Some("Invalid response from provider"),
                    None,
                    deps.now_ms(),
                );
            }
        };

        let windows = build_copilot_windows(&payload, deps.now_ms());
        build_result(
            provider_id,
            provider_name,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}

/// 主实例的注册表 fetch 入口。
pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_for(rt, PROVIDER_ID, PROVIDER_NAME)
}

/// Add-on 孪生实例的注册表 fetch 入口。
pub fn fetch_quota_addon(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_for(rt, PROVIDER_ID_ADDON, PROVIDER_NAME_ADDON)
}
