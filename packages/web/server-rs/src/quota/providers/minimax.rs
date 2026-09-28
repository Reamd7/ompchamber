//! Port of `server/lib/quota/providers/minimax-shared.js` plus the two
//! concrete instances (`minimax-coding-plan.js` for minimax.io and
//! `minimax-cn-coding-plan.js` for minimaxi.com).
//!
//! Token Plan endpoint (`/v1/token_plan/remains`, M3) is tried first and
//! falls back to the legacy Coding Plan endpoint; the two disagree on
//! `current_interval_usage_count` semantics (remaining vs consumed).
//!
//! 中文概览：移植 minimax-shared.js 及两个具体实例——minimax.io 与
//! minimaxi.com（国内站）的 Coding Plan 配额查询。优先请求新版 Token Plan
//! 端点（/v1/token_plan/remains，M3），失败后回退旧 Coding Plan 端点；
//! 两个端点对 current_interval_usage_count 的语义相反（剩余 vs 已用），
//! 由 is_token_plan 标志区分处理。

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, get_auth_entry, normalize_auth_entry, to_number, to_timestamp,
    to_usage_window, usage_payload,
};

/// Status 3 = window not applicable for the current plan tier.
/// 周窗口 status=3 表示当前套餐档位不适用周窗口（legacy 套餐）。
const WINDOW_STATUS_INACTIVE: f64 = 3.0;
/// 通用文本模型的候选名（小写比较），用于挑选默认展示模型。
const TEXT_MODELS: [&str; 3] = ["general", "chat", "text"];

/// 一个 MiniMax 站点（国际站/国内站）的静态配置：提供方标识、别名与两个端点 URL。
pub struct MiniMaxPlan {
    /// 注册到 quota 注册表的提供方 id。
    pub provider_id: &'static str,
    /// 提供方展示名称（含站点域名）。
    pub provider_name: &'static str,
    /// auth.json 中识别该提供方的别名（单元素数组）。
    pub aliases: [&'static str; 1],
    /// 新版 Token Plan（M3）端点，优先尝试。
    pub token_plan_url: &'static str,
    /// 旧版 Coding Plan 端点，Token Plan 不可用时回退。
    pub coding_plan_url: &'static str,
}

/// minimax.io（国际站）实例配置。
pub const MINIMAX_PLAN: MiniMaxPlan = MiniMaxPlan {
    provider_id: "minimax-coding-plan",
    provider_name: "MiniMax Coding Plan (minimax.io)",
    aliases: ["minimax-coding-plan"],
    token_plan_url: "https://api.minimax.io/v1/token_plan/remains",
    coding_plan_url: "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
};

/// minimaxi.com（国内站）实例配置。
pub const MINIMAX_CN_PLAN: MiniMaxPlan = MiniMaxPlan {
    provider_id: "minimax-cn-coding-plan",
    provider_name: "MiniMax Coding Plan (minimaxi.com)",
    aliases: ["minimax-cn-coding-plan"],
    token_plan_url: "https://api.minimaxi.com/v1/token_plan/remains",
    coding_plan_url: "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
};

/// ASCII 大小写不敏感的前缀匹配。
fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// `pickChatModel` — M3 candidate, then text-model names, then any remaining
/// percent entry, then the first entry.
/// 挑选用于展示用量的模型条目，优先级：minimax-m* 且总量大于 0 的 M3 候选，
/// 其次通用文本模型名，其次带剩余百分比的任意条目，最后回退首个条目。
pub fn pick_chat_model(model_remains: Option<&Vec<Value>>) -> Option<&Value> {
    let models = model_remains?;

    if models.is_empty() {
        return None;
    }

    // 提取模型条目的非空 model_name 字段。
    fn model_name(model: &Value) -> Option<&str> {
        field(model, "model_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
    }

    if let Some(candidate) = models.iter().find(|model| {
        model_name(model).is_some_and(|name| starts_with_ignore_case(name, "minimax-m"))
            && to_number(field(model, "current_interval_total_count"))
                .is_some_and(|total| total > 0.0)
    }) {
        return Some(candidate);
    }

    if let Some(candidate) = models.iter().find(|model| {
        model_name(model)
            .is_some_and(|name| TEXT_MODELS.contains(&name.to_ascii_lowercase().as_str()))
    }) {
        return Some(candidate);
    }

    if let Some(candidate) = models.iter().find(|model| {
        field(model, "current_interval_remaining_percent").is_some_and(|v| v.is_number())
    }) {
        return Some(candidate);
    }

    models.first()
}

/// `isUsablePayload` — `base_resp.status_code` must be 0 when present and
/// `model_remains` must be a non-empty array.
/// 校验响应可用性：base_resp.status_code 存在时必须为 0，
/// 且 model_remains 必须是非空数组。
fn is_usable_payload(payload: &Value) -> bool {
    if let Some(base_resp) = field(payload, "base_resp")
        && field(base_resp, "status_code") != Some(&serde_json::json!(0))
    {
        return false;
    }
    field(payload, "model_remains")
        .and_then(Value::as_array)
        .is_some_and(|models| !models.is_empty())
}

/// `fetchEndpoint` — any failure reads as "no payload".
/// 以 bearer token 鉴权 GET 请求端点并校验 payload；
/// 网络失败、非 2xx 或校验不过都按"无数据"处理返回 None。
async fn fetch_endpoint(deps: &QuotaDeps, url: &str, api_key: &str) -> Option<Value> {
    let request = HttpRequest::get(url)
        .bearer(api_key)
        .header("Content-Type", "application/json");
    let response = (deps.http)(request).await.ok()?;
    if !response.ok() {
        return None;
    }
    let payload = response.json().ok()?;
    is_usable_payload(&payload).then_some(payload)
}

/// 将 JSON 值转为数字并夹取到 [0,100] 区间。
fn coerce_percent(value: Option<&Value>) -> Option<f64> {
    to_number(value).map(|n| n.clamp(0.0, 100.0))
}

/// `isWindowActive` — absent status defaults to active; 3 is inactive.
/// 窗口是否生效：status 缺省视为生效，status=3 视为不生效。
fn is_window_active(status: Option<&Value>) -> bool {
    match to_number(status) {
        None => true,
        Some(status) => status != WINDOW_STATUS_INACTIVE,
    }
}

/// `calculateWindowSeconds` — from API timestamps (ms) or `remains_time` (ms).
/// 计算窗口总时长（秒）：优先用 end_time - start_time 的毫秒差，
/// 否则用 remains_time（毫秒）；两者皆无效返回 None。
fn calculate_window_seconds(
    start_at: Option<i64>,
    reset_at: Option<i64>,
    remains_time_ms: Option<f64>,
) -> Option<f64> {
    if let (Some(start), Some(reset)) = (start_at, reset_at)
        && reset > start
    {
        return Some(((reset - start) as f64 / 1000.0).floor());
    }
    if let Some(remains) = remains_time_ms.filter(|remains| *remains > 0.0) {
        return Some((remains / 1000.0).floor());
    }
    None
}

/// 从单模型条目计算出的用量聚合：间隔窗口与周窗口各自的
/// 已用百分比、窗口秒数与重置时间戳。
pub struct MiniMaxUsage {
    /// 间隔窗口已用百分比（0-100），优先由剩余百分比换算。
    pub interval_used_percent: Option<f64>,
    /// 间隔窗口总时长（秒）。
    pub interval_window_seconds: Option<f64>,
    /// 间隔窗口重置时间戳（毫秒）。
    pub interval_reset_at: Option<i64>,
    /// 周窗口已用百分比（0-100）。
    pub weekly_used_percent: Option<f64>,
    /// 周窗口总时长（秒）。
    pub weekly_window_seconds: Option<f64>,
    /// 周窗口重置时间戳（毫秒）。
    pub weekly_reset_at: Option<i64>,
}

/// `calculateUsage` — token-plan endpoints report *remaining* in the usage
/// count field, so `used = total - reported`.
/// 解析单模型条目的间隔/周两窗口用量。注意 Token Plan 端点（is_token_plan）
/// 的 usage 字段上报的是"剩余"而非"已用"，故 used = total - reported；
/// legacy Coding Plan 端点则直接用上报值按 total 归一为百分比。
pub fn calculate_usage(model: &Value, is_token_plan: bool) -> MiniMaxUsage {
    let interval_total = to_number(field(model, "current_interval_total_count"));
    let interval_usage_raw = to_number(field(model, "current_interval_usage_count"));
    let interval_start_at = to_timestamp(field(model, "start_time"));
    let interval_reset_at = to_timestamp(field(model, "end_time"));
    let interval_remains_time = to_number(field(model, "remains_time"));
    let interval_remaining_percent =
        coerce_percent(field(model, "current_interval_remaining_percent"));

    let weekly_total = to_number(field(model, "current_weekly_total_count"));
    let weekly_usage_raw = to_number(field(model, "current_weekly_usage_count"));
    let weekly_start_at = to_timestamp(field(model, "weekly_start_time"));
    let weekly_reset_at = to_timestamp(field(model, "weekly_end_time"));
    let weekly_remains_time = to_number(field(model, "weekly_remains_time"));
    let weekly_remaining_percent = coerce_percent(field(model, "current_weekly_remaining_percent"));

    let compute = |remaining_percent: Option<f64>,
                   total: Option<f64>,
                   usage_raw: Option<f64>|
     -> Option<f64> {
        if let Some(remaining) = remaining_percent {
            return Some(100.0 - remaining);
        }
        match (total, usage_raw) {
            (Some(total), Some(raw)) if total > 0.0 => {
                let used = if is_token_plan {
                    (total - raw).max(0.0)
                } else {
                    raw
                };
                Some(((used / total) * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        }
    };

    MiniMaxUsage {
        interval_used_percent: compute(
            interval_remaining_percent,
            interval_total,
            interval_usage_raw,
        ),
        interval_window_seconds: calculate_window_seconds(
            interval_start_at,
            interval_reset_at,
            interval_remains_time,
        ),
        interval_reset_at,
        weekly_used_percent: compute(weekly_remaining_percent, weekly_total, weekly_usage_raw),
        weekly_window_seconds: calculate_window_seconds(
            weekly_start_at,
            weekly_reset_at,
            weekly_remains_time,
        ),
        weekly_reset_at,
    }
}

/// 从 auth.json 按别名读取 API key（优先 key 字段，回退 token 字段）；
/// auth 文件读取失败时返回 Err（携带错误消息）。
fn load_api_key(deps: &QuotaDeps, aliases: &[&str]) -> Result<Option<String>, String> {
    let auth = deps.read_auth_value()?;
    let entry = normalize_auth_entry(get_auth_entry(&auth, aliases));
    Ok(entry.and_then(|entry| {
        field(&entry, "key")
            .and_then(Value::as_str)
            .or_else(|| field(&entry, "token").and_then(Value::as_str))
            .map(str::to_string)
    }))
}

/// 指定别名下是否已配置：能读到 API key 即视为已配置。
pub fn is_configured_for(deps: &QuotaDeps, aliases: &[&str]) -> bool {
    load_api_key(deps, aliases).unwrap_or(None).is_some()
}

/// 通用拉取流程：读 key → 先 Token Plan 后 Coding Plan 端点 → 挑展示模型 →
/// 计算用量 → 组装 5h（及套餐支持时的 weekly）窗口结果；
/// 未配置、无可用数据、无模型条目分别返回对应错误消息。
pub fn fetch_quota_for(
    rt: std::sync::Arc<QuotaRuntime>,
    plan: &'static MiniMaxPlan,
) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let api_key = match load_api_key(&deps, &plan.aliases) {
            Ok(Some(key)) => key,
            Ok(None) => {
                return build_result(
                    plan.provider_id,
                    plan.provider_name,
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
                    plan.provider_id,
                    plan.provider_name,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    now,
                );
            }
        };

        let failure = |message: &str, now: u64| {
            build_result(
                plan.provider_id,
                plan.provider_name,
                false,
                true,
                None,
                Some(message),
                None,
                now,
            )
        };

        let mut payload = fetch_endpoint(&deps, plan.token_plan_url, &api_key).await;
        let mut is_token_plan = true;
        if payload.is_none() {
            payload = fetch_endpoint(&deps, plan.coding_plan_url, &api_key).await;
            is_token_plan = false;
        }

        let Some(payload) = payload else {
            return failure("API returned no usable quota data", deps.now_ms());
        };

        let model_remains = field(&payload, "model_remains")
            .and_then(Value::as_array)
            .cloned();
        let Some(model) = pick_chat_model(model_remains.as_ref()) else {
            return failure("No model quota data available", deps.now_ms());
        };
        let model = model.clone();

        let usage = calculate_usage(&model, is_token_plan);

        let mut windows = Map::new();
        windows.insert(
            "5h".into(),
            to_usage_window(
                now,
                usage.interval_used_percent,
                usage.interval_window_seconds,
                usage.interval_reset_at.map(|ms| json!(ms)).as_ref(),
                None,
            ),
        );

        // Weekly window only when the tier supports it (status 3 = legacy).
        let weekly_active = is_window_active(field(&model, "current_weekly_status"));
        let has_weekly_data = weekly_active
            && (coerce_percent(field(&model, "current_weekly_remaining_percent")).is_some()
                || to_number(field(&model, "current_weekly_total_count"))
                    .is_some_and(|total| total > 0.0));
        if has_weekly_data {
            windows.insert(
                "weekly".into(),
                to_usage_window(
                    now,
                    usage.weekly_used_percent,
                    usage.weekly_window_seconds,
                    usage.weekly_reset_at.map(|ms| json!(ms)).as_ref(),
                    None,
                ),
            );
        }

        build_result(
            plan.provider_id,
            plan.provider_name,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}

/// minimax.io 实例的 is_configured 入口。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    is_configured_for(deps, &MINIMAX_PLAN.aliases)
}

/// minimaxi.com（国内站）实例的 is_configured 入口。
pub fn is_configured_cn(deps: &QuotaDeps) -> bool {
    is_configured_for(deps, &MINIMAX_CN_PLAN.aliases)
}

/// minimax.io 实例的注册表 fetch 入口。
pub fn fetch_quota(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_quota_for(rt, &MINIMAX_PLAN)
}

/// minimaxi.com（国内站）实例的注册表 fetch 入口。
pub fn fetch_quota_cn(rt: std::sync::Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    fetch_quota_for(rt, &MINIMAX_CN_PLAN)
}
