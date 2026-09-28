//! Port of `server/lib/github/rate-limit.js` — process-global GitHub
//! rate-limit gate.
//!
//! Octokit runs without the throttling plugin, so a primary/secondary rate
//! limit surfaces as a thrown 403/429. When one is detected we record a
//! cooldown and skip GitHub work until it passes.
//!
//! 中文说明：进程级全局的 GitHub 限流闸门。Octokit 客户端未启用
//! throttling 插件，主/次级限流会以 403/429 错误的形式抛出；本模块
//! 负责识别这类错误、记录冷却截止时间，并在冷却期内让调用方跳过
//! GitHub 请求，避免在限流窗口内继续打请求。

use std::sync::{Arc, LazyLock, Mutex};

use crate::github::client::{GithubError, header_value};

/// 单次冷却的上限（15 分钟）：即使响应头暗示更久的等待也只冷却这么长。
const MAX_COOLDOWN_MS: i64 = 15 * 60 * 1000;
/// 响应头没有 retry-after / x-ratelimit-reset 时的默认冷却时长（60 秒）。
const DEFAULT_COOLDOWN_MS: i64 = 60 * 1000;

/// GitHub 限流闸门：记录限流冷却的截止时间戳，供调用方查询是否应暂停请求。
pub struct RateLimitGate {
    /// 冷却截止时间（Unix 毫秒）；0 表示当前未被限流。
    rate_limited_until_ms: Mutex<i64>,
}

/// `rateLimitedUntil` module state (process-global in JS).
///
/// 对应 JS 版的模块级 rateLimitedUntil 状态，进程内全局共享一份。
pub static GLOBAL_GATE: LazyLock<Arc<RateLimitGate>> =
    LazyLock::new(|| Arc::new(RateLimitGate::new()));

/// 从限流响应头解析建议的冷却毫秒数：优先 retry-after（秒，可为小数），
/// 其次 x-ratelimit-reset（Unix 秒，换算成相对 now_ms 的差值）；头缺失、
/// 解析失败或算出的等待非正数时返回 None。
fn parse_retry_after_ms(headers: &[(String, String)], now_ms: i64) -> Option<i64> {
    if let Some(retry_after) = header_value(headers, "retry-after")
        && let Ok(secs) = retry_after.parse::<f64>()
        && secs.is_finite()
        && secs > 0.0
    {
        return Some((secs * 1000.0) as i64);
    }
    if let Some(reset) = header_value(headers, "x-ratelimit-reset")
        && let Ok(reset_secs) = reset.parse::<f64>()
    {
        let delta = reset_secs * 1000.0 - now_ms as f64;
        if delta.is_finite() && delta > 0.0 {
            return Some(delta as i64);
        }
    }
    None
}

/// 限流闸门的核心 API：识别限流错误、记录冷却期、查询冷却是否生效。
impl RateLimitGate {
    /// 构造一个未处于冷却状态的闸门（截止时间为 0）。
    pub fn new() -> Self {
        Self {
            rate_limited_until_ms: Mutex::new(0),
        }
    }

    /// `isGitHubRateLimitError`: 429 always; 403 when the remaining quota is
    /// zero, a retry-after is present, or the message mentions "rate limit".
    ///
    /// 判定规则（对应 JS 版 isGitHubRateLimitError，为不读实例状态的
    /// 静态方法）：429 一律算限流；403 仅当 x-ratelimit-remaining 为 0
    /// （"0"/"0.0" 或数值 0）、带 retry-after 头、或错误消息小写化后
    /// 包含 "rate limit" 时才算；其余状态码一律不是。now_ms 参数仅为
    /// 对齐 JS 签名而保留。
    pub fn is_rate_limit_error(error: &GithubError, now_ms: i64) -> bool {
        let status = error.status;
        if status == Some(429) {
            return true;
        }
        if status != Some(403) {
            return false;
        }
        let headers = &error.headers;
        let remaining = header_value(headers, "x-ratelimit-remaining");
        if remaining == Some("0") || remaining == Some("0.0") {
            return true;
        }
        if header_value(headers, "retry-after").is_some() {
            return true;
        }
        // JS `Number(remaining) === 0` also matches numeric zero shapes like "0".
        if let Some(remaining) = remaining
            && let Ok(parsed) = remaining.parse::<f64>()
            && parsed == 0.0
        {
            return true;
        }
        let _ = now_ms;
        error.message.to_lowercase().contains("rate limit")
    }

    /// `noteGitHubRateLimit`: record a cooldown after a rate-limit response.
    ///
    /// 记录一次限流冷却：时长取 retry-after / x-ratelimit-reset 的建议值，
    /// 缺省 60 秒、封顶 15 分钟；仅当新截止时间晚于已记录值时才更新，并
    /// 输出 warn 日志提示将暂停 GitHub 调用约多少秒。
    pub fn note_rate_limit(&self, error: &GithubError, now_ms: i64) {
        let retry_ms = parse_retry_after_ms(&error.headers, now_ms)
            .unwrap_or(DEFAULT_COOLDOWN_MS)
            .min(MAX_COOLDOWN_MS);
        let until = now_ms + retry_ms;
        let mut guarded = self
            .rate_limited_until_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if until > *guarded {
            *guarded = until;
            tracing::warn!(
                "[github] rate limited — pausing GitHub PR status calls for ~{}s",
                (retry_ms as f64 / 1000.0).round() as i64
            );
        }
    }

    /// `noteIfGitHubRateLimit`: note the error if it is a rate-limit error;
    /// returns whether it was.
    ///
    /// 组合入口：错误确属限流时记录冷却并返回 true，否则不记录并返回
    /// false，供调用方在错误处理路径上一行完成判定与登记。
    pub fn note_if_rate_limit_error(&self, error: &GithubError, now_ms: i64) -> bool {
        if !Self::is_rate_limit_error(error, now_ms) {
            return false;
        }
        self.note_rate_limit(error, now_ms);
        true
    }

    /// `isGitHubRateLimited`.
    ///
    /// 查询当前时刻是否仍处于限流冷却期（早于已记录的截止时间）。
    pub fn is_rate_limited(&self, now_ms: i64) -> bool {
        now_ms
            < *self
                .rate_limited_until_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner())
    }
}

/// Default 实现，等价于 new()（无冷却状态）。
impl Default for RateLimitGate {
    /// 默认实例与 RateLimitGate::new() 完全一致。
    fn default() -> Self {
        Self::new()
    }
}

/// 当前 Unix 时间戳（毫秒），作为所有冷却计算的时钟基准；时钟早于
/// epoch 等异常情况下返回 0。独立成函数便于测试注入固定时刻。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 验证限流错误的判定规则与冷却时长的取值、封顶和回退行为。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：构造带状态码、消息与响应头的 GithubError。
    fn err(status: Option<u16>, message: &str, headers: Vec<(&str, &str)>) -> GithubError {
        GithubError {
            status,
            message: message.to_string(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            data: None,
        }
    }

    /// 验证 429 无条件判定为限流。
    #[test]
    fn four_twenty_nine_is_always_rate_limit() {
        assert!(RateLimitGate::is_rate_limit_error(
            &err(Some(429), "nope", vec![]),
            0
        ));
    }

    /// 验证普通 500/403/网络错误不属于限流。
    #[test]
    fn plain_errors_are_not_rate_limits() {
        assert!(!RateLimitGate::is_rate_limit_error(
            &err(Some(500), "boom", vec![]),
            0
        ));
        assert!(!RateLimitGate::is_rate_limit_error(
            &err(Some(403), "Forbidden", vec![]),
            0
        ));
        assert!(!RateLimitGate::is_rate_limit_error(
            &err(None, "network", vec![]),
            0
        ));
    }

    /// 验证 403 且剩余配额为 0 时判定为限流。
    #[test]
    fn forbidden_with_zero_remaining_is_rate_limit() {
        let e = err(Some(403), "Forbidden", vec![("x-ratelimit-remaining", "0")]);
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    /// 验证 403 且带 retry-after 头时判定为限流。
    #[test]
    fn forbidden_with_retry_after_is_rate_limit() {
        let e = err(Some(403), "Forbidden", vec![("retry-after", "30")]);
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    /// 验证 403 且错误消息提及 "rate limit"（次级限流）时判定为限流。
    #[test]
    fn forbidden_message_mentioning_rate_limit_matches() {
        let e = err(
            Some(403),
            "You have exceeded a secondary rate limit",
            vec![],
        );
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    /// 验证冷却时长优先采用 retry-after 头（秒）。
    #[test]
    fn cooldown_uses_retry_after_header() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![("retry-after", "2")]);
        gate.note_rate_limit(&e, 1_000);
        assert!(gate.is_rate_limited(1_500));
        assert!(gate.is_rate_limited(2_900));
        assert!(!gate.is_rate_limited(3_100));
    }

    /// 验证没有 retry-after 时回退用 x-ratelimit-reset 的差值。
    #[test]
    fn cooldown_uses_ratelimit_reset_when_no_retry_after() {
        let gate = RateLimitGate::new();
        let e = err(
            Some(429),
            "rate limited",
            vec![("x-ratelimit-reset", "100")],
        );
        gate.note_rate_limit(&e, 0);
        assert!(gate.is_rate_limited(50_000));
        assert!(!gate.is_rate_limited(150_000));
    }

    /// 验证无任何提示头时默认冷却 60 秒。
    #[test]
    fn cooldown_falls_back_to_sixty_seconds() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![]);
        gate.note_rate_limit(&e, 0);
        assert!(gate.is_rate_limited(59_000));
        assert!(!gate.is_rate_limited(61_000));
    }

    /// 验证冷却时长封顶 15 分钟。
    #[test]
    fn cooldown_is_capped_at_fifteen_minutes() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![("retry-after", "3600")]);
        gate.note_rate_limit(&e, 0);
        assert!(gate.is_rate_limited(14 * 60 * 1000));
        assert!(!gate.is_rate_limited(16 * 60 * 1000));
    }

    /// 验证 note_if_rate_limit_error 的返回值与实际的冷却登记效果。
    #[test]
    fn note_if_returns_whether_it_was_rate_limited() {
        let gate = RateLimitGate::new();
        assert!(!gate.note_if_rate_limit_error(&err(Some(404), "Not Found", vec![]), 0));
        assert!(gate.note_if_rate_limit_error(&err(Some(429), "rl", vec![]), 0));
        assert!(gate.is_rate_limited(1_000));
    }
}
