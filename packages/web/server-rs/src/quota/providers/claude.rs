//! Port of `server/lib/quota/providers/claude/` — Claude subscription quota
//! from `GET https://api.anthropic.com/api/oauth/usage`.
//!
//! - `auth.js`: credential discovery — macOS Keychain, then
//!   `${CLAUDE_CONFIG_DIR:-~/.claude}/.credentials.json`, then the OpenCode
//!   `auth.json` entry, then `CLAUDE_CODE_OAUTH_TOKEN`. All sources are
//!   read-only (refreshing here would sign Claude Code out).
//! - `transforms.js`: the `limits` array keyed by `kind` (session → `5h`,
//!   weekly_all → `7d`, weekly_scoped → per-model `7d`), legacy named-field
//!   fallback, and the `spend`-gated extra-usage money window.
//! - `index.js`: in-memory last-good cache + 429 cooldown (Retry-After, else
//!   5 minutes, capped at 1 hour), keyed by a SHA-256 fingerprint of the
//!   access+refresh tokens so switching accounts drops it.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::HttpRequest;
use crate::quota::runtime::{QuotaRuntime, SharedSlot};
use crate::quota::utils::{
    as_non_empty_string, as_object_value, build_result, field, format_money, normalize_timestamp,
    parse_iso_ms, to_number, to_timestamp, to_usage_window,
};

pub const PROVIDER_ID: &str = "claude";
pub const PROVIDER_NAME: &str = "Claude";
pub const ALIASES: [&str; 2] = ["anthropic", "claude"];

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const DEFAULT_COOLDOWN_MS: u64 = 5 * 60 * 1000;
const MAX_COOLDOWN_MS: u64 = 60 * 60 * 1000;

const SESSION_WINDOW: &str = "5h";
const WEEKLY_WINDOW: &str = "7d";
const EXTRA_USAGE_WINDOW: &str = "extra_usage";
const SESSION_WINDOW_SECONDS: f64 = 5.0 * 60.0 * 60.0;
const WEEKLY_WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Keychain,
    CredentialsFile,
    OpencodeAuth,
    Env,
}

#[derive(Debug, Clone)]
pub struct ClaudeCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub plan_label: Option<String>,
    pub source: CredentialSource,
}

/// `claudeConfigDirectory`.
fn claude_config_directory(deps: &QuotaDeps) -> std::path::PathBuf {
    match (deps.env)("CLAUDE_CONFIG_DIR") {
        Some(override_dir) => {
            let path = std::path::PathBuf::from(override_dir);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        }
        None => (deps.home_dir)().unwrap_or_default().join(".claude"),
    }
}

/// `parseClaudeCodeBlob` — only the `claudeAiOauth` block is read.
fn parse_claude_code_blob(blob: &Value, source: CredentialSource) -> Option<ClaudeCredential> {
    let oauth = as_object_value(field(blob, "claudeAiOauth"))?;
    let access_token = field(oauth, "accessToken").and_then(as_non_empty_string)?;
    Some(ClaudeCredential {
        access_token,
        refresh_token: field(oauth, "refreshToken").and_then(as_non_empty_string),
        expires_at: field(oauth, "expiresAt").and_then(normalize_timestamp),
        plan_label: field(oauth, "subscriptionType").and_then(as_non_empty_string),
        source,
    })
}

fn read_keychain_credential(deps: &QuotaDeps) -> Option<ClaudeCredential> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let raw = (deps.keychain)()?;
    let parsed: Value = serde_json::from_str(raw.trim()).ok()?;
    parse_claude_code_blob(&parsed, CredentialSource::Keychain)
}

fn read_credentials_file(deps: &QuotaDeps) -> Option<ClaudeCredential> {
    let blob = crate::quota::utils::read_json_file(
        &claude_config_directory(deps).join(".credentials.json"),
    )?;
    parse_claude_code_blob(&blob, CredentialSource::CredentialsFile)
}

fn read_opencode_credential(deps: &QuotaDeps) -> Result<Option<ClaudeCredential>, String> {
    let auth = deps.read_auth_value()?;
    let Some(entry) = crate::quota::utils::normalize_auth_entry(
        crate::quota::utils::get_auth_entry(&auth, &ALIASES),
    ) else {
        return Ok(None);
    };
    let Some(access_token) = field(&entry, "access")
        .and_then(as_non_empty_string)
        .or_else(|| field(&entry, "token").and_then(as_non_empty_string))
    else {
        return Ok(None);
    };
    Ok(Some(ClaudeCredential {
        access_token,
        refresh_token: field(&entry, "refresh").and_then(as_non_empty_string),
        expires_at: field(&entry, "expires").and_then(normalize_timestamp),
        plan_label: None,
        source: CredentialSource::OpencodeAuth,
    }))
}

fn read_env_credential(deps: &QuotaDeps) -> Option<ClaudeCredential> {
    let access_token = (deps.env)("CLAUDE_CODE_OAUTH_TOKEN")?;
    Some(ClaudeCredential {
        access_token,
        refresh_token: None,
        expires_at: None,
        plan_label: None,
        source: CredentialSource::Env,
    })
}

/// `loadClaudeCredential` — first source that produces a token wins. A
/// thrown `readAuthFile()` propagates like the JS (registry catch).
pub fn load_claude_credential(deps: &QuotaDeps) -> Result<Option<ClaudeCredential>, String> {
    if let Some(credential) = read_keychain_credential(deps) {
        return Ok(Some(credential));
    }
    if let Some(credential) = read_credentials_file(deps) {
        return Ok(Some(credential));
    }
    read_opencode_credential(deps)
        .map(|credential| credential.or_else(|| read_env_credential(deps)))
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_claude_credential(deps).unwrap_or(None).is_some()
}

// ============== transforms.js ==============

/// Money in Anthropic's minor-unit form (`{ amount_minor, exponent }`).
fn to_amount(value: Option<&Value>) -> Option<f64> {
    let money = as_object_value(value)?;
    let minor = to_number(field(money, "amount_minor"))?;
    let exponent = to_number(field(money, "exponent")).unwrap_or(2.0);
    Some(minor / 10f64.powf(exponent))
}

fn format_spend_label(
    used: Option<f64>,
    limit: Option<f64>,
    currency: Option<&str>,
) -> Option<String> {
    let used_label = format_money(used)?;
    let prefix = match currency {
        Some("USD") | None => "$".to_string(),
        Some(currency) => format!("{currency} "),
    };
    match format_money(limit) {
        Some(limit_label) => Some(format!("{prefix}{used_label} / {prefix}{limit_label}")),
        None => Some(format!("{prefix}{used_label}")),
    }
}

fn add_window(
    target: &mut Map<String, Value>,
    now: u64,
    key: &str,
    percent: Option<f64>,
    reset_at: Option<i64>,
    value_label: Option<&str>,
    window_seconds: Option<f64>,
) {
    if percent.is_none() && value_label.is_none() {
        return;
    }
    let reset_at = reset_at.map(|ms| json!(ms));
    target.insert(
        key.to_string(),
        to_usage_window(now, percent, window_seconds, reset_at.as_ref(), value_label),
    );
}

fn apply_limits_array(
    limits: &[Value],
    now: u64,
    windows: &mut Map<String, Value>,
    models: &mut Map<String, Value>,
) {
    for entry in limits {
        let percent = to_number(field(entry, "percent"));
        let reset_at = to_timestamp(field(entry, "resets_at"));
        let kind = field(entry, "kind").and_then(Value::as_str).unwrap_or("");
        let model_name = field(entry, "scope")
            .and_then(|scope| field(scope, "model"))
            .and_then(|model| field(model, "display_name"))
            .and_then(as_non_empty_string);

        match kind {
            "session" => {
                add_window(
                    windows,
                    now,
                    SESSION_WINDOW,
                    percent,
                    reset_at,
                    None,
                    Some(SESSION_WINDOW_SECONDS),
                );
            }
            "weekly_all" => {
                add_window(
                    windows,
                    now,
                    WEEKLY_WINDOW,
                    percent,
                    reset_at,
                    None,
                    Some(WEEKLY_WINDOW_SECONDS),
                );
            }
            "weekly_scoped" => {
                let Some(model_name) = model_name else {
                    continue;
                };
                let mut model_windows = Map::new();
                add_window(
                    &mut model_windows,
                    now,
                    WEEKLY_WINDOW,
                    percent,
                    reset_at,
                    None,
                    Some(WEEKLY_WINDOW_SECONDS),
                );
                if !model_windows.is_empty() {
                    models.insert(model_name, json!({ "windows": model_windows }));
                }
            }
            _ => {}
        }
    }
}

fn apply_legacy_fields(payload: &Value, now: u64, windows: &mut Map<String, Value>) {
    if let Some(five_hour) = as_object_value(field(payload, "five_hour")) {
        add_window(
            windows,
            now,
            SESSION_WINDOW,
            to_number(field(five_hour, "utilization")),
            to_timestamp(field(five_hour, "resets_at")),
            None,
            Some(SESSION_WINDOW_SECONDS),
        );
    }
    if let Some(seven_day) = as_object_value(field(payload, "seven_day")) {
        add_window(
            windows,
            now,
            WEEKLY_WINDOW,
            to_number(field(seven_day, "utilization")),
            to_timestamp(field(seven_day, "resets_at")),
            None,
            Some(WEEKLY_WINDOW_SECONDS),
        );
    }
}

fn apply_extra_usage(payload: &Value, now: u64, windows: &mut Map<String, Value>) {
    let Some(spend) = as_object_value(field(payload, "spend")) else {
        return;
    };
    if field(spend, "enabled") != Some(&Value::Bool(true)) {
        return;
    }
    let used = to_amount(field(spend, "used"));
    let limit = to_amount(field(spend, "limit"));
    let currency = field(spend, "used")
        .and_then(|used| field(used, "currency"))
        .and_then(as_non_empty_string);
    add_window(
        windows,
        now,
        EXTRA_USAGE_WINDOW,
        to_number(field(spend, "percent")),
        None,
        format_spend_label(used, limit, currency.as_deref()).as_deref(),
        None,
    );
}

/// `toClaudeUsage` — `{ windows, models }`.
pub fn to_claude_usage(
    raw_payload: Option<&Value>,
    now: u64,
) -> (Map<String, Value>, Map<String, Value>) {
    let mut windows = Map::new();
    let mut models = Map::new();
    let Some(payload) = as_object_value(raw_payload) else {
        return (windows, models);
    };

    let limits = field(payload, "limits").and_then(Value::as_array);
    match limits {
        Some(limits) if !limits.is_empty() => {
            apply_limits_array(limits, now, &mut windows, &mut models)
        }
        _ => apply_legacy_fields(payload, now, &mut windows),
    }
    apply_extra_usage(payload, now, &mut windows);

    (windows, models)
}

// ============== index.js rate-limit semantics ==============

fn fingerprint_of(credential: &ClaudeCredential) -> String {
    let mut hasher = Sha256::new();
    hasher.update(credential.access_token.as_bytes());
    hasher.update(b"\0");
    hasher.update(credential.refresh_token.as_deref().unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn cooldown_from_header(retry_after: Option<&str>, now: u64) -> u64 {
    if let Some(raw) = retry_after {
        if let Ok(value) = raw.trim().parse::<f64>()
            && value > 0.0
            && value.is_finite()
        {
            return ((value * 1000.0) as u64).min(MAX_COOLDOWN_MS);
        }
        if let Some(retry_at) = parse_iso_ms(raw)
            && retry_at > now as i64
        {
            return ((retry_at as u64).saturating_sub(now)).min(MAX_COOLDOWN_MS);
        }
    }
    DEFAULT_COOLDOWN_MS
}

struct CachedUsage {
    fingerprint: String,
    usage: Value,
    plan_label: Option<String>,
}

/// In-memory last-good cache + cooldown + coalesced in-flight refresh.
pub struct ClaudeCache {
    cached_usage: std::sync::Mutex<Option<CachedUsage>>,
    cooldown_until: std::sync::Mutex<u64>,
    pub pending: Arc<SharedSlot<Value>>,
}

impl Default for ClaudeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCache {
    pub fn new() -> Self {
        Self {
            cached_usage: std::sync::Mutex::new(None),
            cooldown_until: std::sync::Mutex::new(0),
            pending: Arc::new(SharedSlot::new()),
        }
    }

    fn cached_result_for(
        &self,
        fingerprint: &str,
        plan_label: Option<&str>,
        now: u64,
    ) -> Option<Value> {
        let cache = self.cached_usage.lock().unwrap_or_else(|e| e.into_inner());
        let cached = cache.as_ref()?;
        if cached.fingerprint != fingerprint {
            return None;
        }
        Some(build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(cached.usage.clone()),
            None,
            plan_label.or(cached.plan_label.as_deref()),
            now,
        ))
    }

    fn drop_stale_for(&self, fingerprint: &str) {
        let mut cache = self.cached_usage.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref()
            && cached.fingerprint != fingerprint
        {
            *cache = None;
            *self
                .cooldown_until
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = 0;
        }
    }

    /// Test seam: `resetClaudeQuotaCache`.
    pub fn reset(&self) {
        *self.cached_usage.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cooldown_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = 0;
        self.pending.clear();
    }
}
fn failure(message: &str, configured: bool, now: u64) -> Value {
    build_result(
        PROVIDER_ID,
        PROVIDER_NAME,
        false,
        configured,
        None,
        Some(message),
        None,
        now,
    )
}

fn fetch_quota_uncoalesced(rt: &Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    let rt = rt.clone();
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let credential = match load_claude_credential(&deps) {
            Ok(Some(credential)) => credential,
            Ok(None) => return failure("Not configured", false, now),
            Err(message) => return failure(&message, true, now),
        };

        let fingerprint = fingerprint_of(&credential);
        rt.claude_cache.drop_stale_for(&fingerprint);

        if now
            < *rt
                .claude_cache
                .cooldown_until
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        {
            return rt
                .claude_cache
                .cached_result_for(&fingerprint, credential.plan_label.as_deref(), now)
                .unwrap_or_else(|| failure("Rate limited. Retrying soon.", true, now));
        }

        let request = HttpRequest::get(USAGE_URL)
            .bearer(&credential.access_token)
            .header("anthropic-beta", OAUTH_BETA_HEADER);

        let response = match (deps.http)(request).await {
            Ok(response) => response,
            Err(error) => return failure(&error.message(), true, deps.now_ms()),
        };

        if response.status == 429 {
            let now = deps.now_ms();
            let cooldown = cooldown_from_header(response.header("retry-after"), now);
            *rt.claude_cache
                .cooldown_until
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = now + cooldown;
            return rt
                .claude_cache
                .cached_result_for(&fingerprint, credential.plan_label.as_deref(), now)
                .unwrap_or_else(|| failure("Rate limited. Retrying soon.", true, now));
        }

        if response.status == 401 || response.status == 403 {
            return failure(
                "Claude session expired. Open Claude Code to sign in again.",
                true,
                deps.now_ms(),
            );
        }

        if !response.ok() {
            return failure(
                &format!("API error: {}", response.status),
                true,
                deps.now_ms(),
            );
        }

        let payload = match response.json() {
            Ok(payload) => payload,
            Err(_) => return failure("Unexpected response from Anthropic", true, deps.now_ms()),
        };

        let now = deps.now_ms();
        let (windows, models) = to_claude_usage(Some(&payload), now);
        let usage = crate::quota::utils::usage_payload(windows, Some(models));

        *rt.claude_cache
            .cached_usage
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(CachedUsage {
            fingerprint,
            usage: usage.clone(),
            plan_label: credential.plan_label.clone(),
        });

        build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(usage),
            None,
            credential.plan_label.as_deref(),
            now,
        )
    })
}

pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    let pending = rt.claude_cache.pending.clone();
    let shared = pending.subscribe(move || fetch_quota_uncoalesced(&rt));
    Box::pin(shared)
}
