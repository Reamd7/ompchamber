//! Port of `server/lib/github/rate-limit.js` — process-global GitHub
//! rate-limit gate.
//!
//! Octokit runs without the throttling plugin, so a primary/secondary rate
//! limit surfaces as a thrown 403/429. When one is detected we record a
//! cooldown and skip GitHub work until it passes.

use std::sync::{Arc, LazyLock, Mutex};

use crate::github::client::{GithubError, header_value};

const MAX_COOLDOWN_MS: i64 = 15 * 60 * 1000;
const DEFAULT_COOLDOWN_MS: i64 = 60 * 1000;

pub struct RateLimitGate {
    rate_limited_until_ms: Mutex<i64>,
}

/// `rateLimitedUntil` module state (process-global in JS).
pub static GLOBAL_GATE: LazyLock<Arc<RateLimitGate>> =
    LazyLock::new(|| Arc::new(RateLimitGate::new()));

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

impl RateLimitGate {
    pub fn new() -> Self {
        Self {
            rate_limited_until_ms: Mutex::new(0),
        }
    }

    /// `isGitHubRateLimitError`: 429 always; 403 when the remaining quota is
    /// zero, a retry-after is present, or the message mentions "rate limit".
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
    pub fn note_if_rate_limit_error(&self, error: &GithubError, now_ms: i64) -> bool {
        if !Self::is_rate_limit_error(error, now_ms) {
            return false;
        }
        self.note_rate_limit(error, now_ms);
        true
    }

    /// `isGitHubRateLimited`.
    pub fn is_rate_limited(&self, now_ms: i64) -> bool {
        now_ms
            < *self
                .rate_limited_until_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for RateLimitGate {
    fn default() -> Self {
        Self::new()
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn four_twenty_nine_is_always_rate_limit() {
        assert!(RateLimitGate::is_rate_limit_error(
            &err(Some(429), "nope", vec![]),
            0
        ));
    }

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

    #[test]
    fn forbidden_with_zero_remaining_is_rate_limit() {
        let e = err(Some(403), "Forbidden", vec![("x-ratelimit-remaining", "0")]);
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    #[test]
    fn forbidden_with_retry_after_is_rate_limit() {
        let e = err(Some(403), "Forbidden", vec![("retry-after", "30")]);
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    #[test]
    fn forbidden_message_mentioning_rate_limit_matches() {
        let e = err(
            Some(403),
            "You have exceeded a secondary rate limit",
            vec![],
        );
        assert!(RateLimitGate::is_rate_limit_error(&e, 0));
    }

    #[test]
    fn cooldown_uses_retry_after_header() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![("retry-after", "2")]);
        gate.note_rate_limit(&e, 1_000);
        assert!(gate.is_rate_limited(1_500));
        assert!(gate.is_rate_limited(2_900));
        assert!(!gate.is_rate_limited(3_100));
    }

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

    #[test]
    fn cooldown_falls_back_to_sixty_seconds() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![]);
        gate.note_rate_limit(&e, 0);
        assert!(gate.is_rate_limited(59_000));
        assert!(!gate.is_rate_limited(61_000));
    }

    #[test]
    fn cooldown_is_capped_at_fifteen_minutes() {
        let gate = RateLimitGate::new();
        let e = err(Some(429), "rate limited", vec![("retry-after", "3600")]);
        gate.note_rate_limit(&e, 0);
        assert!(gate.is_rate_limited(14 * 60 * 1000));
        assert!(!gate.is_rate_limited(16 * 60 * 1000));
    }

    #[test]
    fn note_if_returns_whether_it_was_rate_limited() {
        let gate = RateLimitGate::new();
        assert!(!gate.note_if_rate_limit_error(&err(Some(404), "Not Found", vec![]), 0));
        assert!(gate.note_if_rate_limit_error(&err(Some(429), "rl", vec![]), 0));
        assert!(gate.is_rate_limited(1_000));
    }
}
