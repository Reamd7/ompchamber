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
//!
//! 中文说明：本模块是 `server/lib/quota/providers/claude/` 的移植，从
//! `GET https://api.anthropic.com/api/oauth/usage` 拉取 Claude 订阅配额。
//! 凭据按顺序发现（全部只读——在此刷新会把 Claude Code 登出）：
//! macOS Keychain → `${CLAUDE_CONFIG_DIR:-~/.claude}/.credentials.json` →
//! OpenCode `auth.json` 条目 → `CLAUDE_CODE_OAUTH_TOKEN` 环境变量。
//! 转换层把 `limits` 数组按 `kind` 映射为窗口（session → `5h`、
//! weekly_all → `7d`、weekly_scoped → 按模型的 `7d`），并支持遗留命名字段
//! 与 `spend` 门槛的 extra_usage 金额窗口。运行时维护内存中的
//! last-good 缓存与 429 冷却（优先 Retry-After，缺省 5 分钟、封顶 1 小时），
//! 缓存键为 access+refresh token 的 SHA-256 指纹，切换账号即失效。

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

/// provider 注册 ID（注册表与 API 路径中使用）。
pub const PROVIDER_ID: &str = "claude";
/// provider 展示名。
pub const PROVIDER_NAME: &str = "Claude";
/// provider 别名（`anthropic` 与 `claude` 均指向本 provider）。
pub const ALIASES: [&str; 2] = ["anthropic", "claude"];

/// Anthropic OAuth 用量端点。
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// OAuth beta 协议头的取值（`anthropic-beta: oauth-2025-04-20`）。
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// 429 后无 Retry-After 时的默认冷却毫秒数（5 分钟）。
const DEFAULT_COOLDOWN_MS: u64 = 5 * 60 * 1000;
/// 冷却毫秒数上限（1 小时），Retry-After 再大也会被截断。
const MAX_COOLDOWN_MS: u64 = 60 * 60 * 1000;

/// 会话窗口的信封键名（5 小时滚动窗口）。
const SESSION_WINDOW: &str = "5h";
/// 周窗口的信封键名（7 天窗口）。
const WEEKLY_WINDOW: &str = "7d";
/// extra usage（付费加量）金额窗口的信封键名。
const EXTRA_USAGE_WINDOW: &str = "extra_usage";
/// 会话窗口的秒数（5 小时），用于计算窗口起点。
const SESSION_WINDOW_SECONDS: f64 = 5.0 * 60.0 * 60.0;
/// 周窗口的秒数（7 天）。
const WEEKLY_WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

/// 凭据来源：发现链中的哪一环产出了当前凭据（仅用于展示/调试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// macOS Keychain 中的 `Claude Code-credentials` 条目。
    Keychain,
    /// `${CLAUDE_CONFIG_DIR:-~/.claude}/.credentials.json` 文件。
    CredentialsFile,
    /// OpenCode `auth.json` 中的 claude 条目。
    OpencodeAuth,
    /// `CLAUDE_CODE_OAUTH_TOKEN` 环境变量。
    Env,
}

/// 解析后的 Claude 凭据：access token 为主，其余字段可选。
#[derive(Debug, Clone)]
pub struct ClaudeCredential {
    /// OAuth access token（调用用量接口的 bearer token）。
    pub access_token: String,
    /// OAuth refresh token（仅参与缓存指纹计算，本模块绝不刷新它）。
    pub refresh_token: Option<String>,
    /// 过期时间的 epoch 毫秒（可能缺失）。
    pub expires_at: Option<i64>,
    /// 订阅计划展示名（如 Pro/Max，可能缺失）。
    pub plan_label: Option<String>,
    /// 本凭据的发现来源。
    pub source: CredentialSource,
}

/// `claudeConfigDirectory`.
///
/// 中文说明：解析 Claude 配置目录（对应 JS `claudeConfigDirectory`）：
/// 优先 `CLAUDE_CONFIG_DIR`，否则 `<home>/.claude`。
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
///
/// 中文说明：解析 Keychain blob / credentials 文件中的 JSON（对应 JS
/// `parseClaudeCodeBlob`）：只读取 `claudeAiOauth` 块，缺失或结构不符
/// 返回 `None`；access token 为空同样视为无效。
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

/// 从 macOS Keychain 读取并解析 Claude Code 凭据；非 macOS、Keychain
/// 不可用或解析失败都返回 `None`（继续走下一来源）。
fn read_keychain_credential(deps: &QuotaDeps) -> Option<ClaudeCredential> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let raw = (deps.keychain)()?;
    let parsed: Value = serde_json::from_str(raw.trim()).ok()?;
    parse_claude_code_blob(&parsed, CredentialSource::Keychain)
}

/// 从 `${CLAUDE_CONFIG_DIR:-~/.claude}/.credentials.json` 读取并解析凭据；
/// 任何失败都返回 `None`。
fn read_credentials_file(deps: &QuotaDeps) -> Option<ClaudeCredential> {
    let blob = crate::quota::utils::read_json_file(
        &claude_config_directory(deps).join(".credentials.json"),
    )?;
    parse_claude_code_blob(&blob, CredentialSource::CredentialsFile)
}

/// 从 OpenCode `auth.json` 的 claude 条目读取凭据：要求 type 为 `oauth`
/// 且有非空 access token；不满足返回 `Ok(None)`，仅 auth 读取失败透传
/// `Err`。
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

/// 从 `CLAUDE_CODE_OAUTH_TOKEN` 环境变量构造仅含 access token 的凭据；
/// 缺失或为空返回 `None`。
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
///
/// 中文说明：按 Keychain → credentials 文件 → OpenCode auth.json →
/// 环境变量的顺序加载凭据（对应 JS `loadClaudeCredential`）：第一个产出
/// token 的来源胜出；`readAuthFile()` 抛出的错误原样上抛（由注册表的
/// catch 转成错误信封）。
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

/// provider 是否已配置：凭据发现链任一来源产出 token 即为 true（读取
/// 失败按未配置处理）。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    load_claude_credential(deps).unwrap_or(None).is_some()
}

// ============== transforms.js ==============

/// Money in Anthropic's minor-unit form (`{ amount_minor, exponent }`).
///
/// 中文说明：把 Anthropic 的小数金额形式 `{ amount_minor, exponent }`
/// 换算为浮点金额；缺失或不可解析返回 `None`。
fn to_amount(value: Option<&Value>) -> Option<f64> {
    let money = as_object_value(value)?;
    let minor = to_number(field(money, "amount_minor"))?;
    let exponent = to_number(field(money, "exponent")).unwrap_or(2.0);
    Some(minor / 10f64.powf(exponent))
}

/// 格式化 spend 标签（如 `$12.34 / $100.00`）：used/limit 任一缺失即返回
/// `None`；货币符号按常见代码映射（usd → $ 等），未知货币原样显示。
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

/// 向 `windows`/`models` 目标写入一个用量窗口条目：键名、百分比、重置
/// 时间、展示标签与窗口秒数（用于推导窗口起点），字段缺失时省略对应项。
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

/// 应用新版 `limits` 数组：按 `kind` 分派——`session` 写入 `5h` 窗口、
/// `weekly_all` 写入 `7d` 窗口、`weekly_scoped` 按 model 写入 `models` 的
/// `7d` 窗口；未知 kind 忽略。
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

/// 应用遗留的顶层命名字段（`five_hour_window`/`seven_day_window` 等）：
/// 仅在响应没有 `limits` 数组时作为兜底写入对应窗口。
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

/// 应用 extra usage 金额窗口：`spend` 的 used/limit 均存在且超过门槛时
/// 才写入 `extra_usage` 窗口（含 spend 标签，无百分比）。
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
///
/// 中文说明：把原始用量 payload 转为 `{ windows, models }`（对应 JS
/// `toClaudeUsage`）：有 `limits` 数组走新版路径，否则回落到遗留命名字段；
/// 最后叠加 extra usage 金额窗口。两个 map 均可能为空。
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

/// 计算凭据指纹：access token 与 refresh token 以 `\0` 连接后的 SHA-256
/// 十六进制串；切换账号（token 变化）即得到新指纹，使缓存自然失效。
fn fingerprint_of(credential: &ClaudeCredential) -> String {
    let mut hasher = Sha256::new();
    hasher.update(credential.access_token.as_bytes());
    hasher.update(b"\0");
    hasher.update(credential.refresh_token.as_deref().unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 由 `Retry-After` 头计算冷却毫秒数：先按秒数解析，再按 ISO 时间戳解析，
/// 均封顶 [`MAX_COOLDOWN_MS`]；头缺失或不可解析时用 [`DEFAULT_COOLDOWN_MS`]。
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

/// 缓存的 last-good 结果及其归属指纹。
struct CachedUsage {
    /// 产出该用量的凭据指纹（不匹配即视为陈旧）。
    fingerprint: String,
    /// 上次成功的 usage payload（原样缓存，返回时克隆）。
    usage: Value,
    /// 缓存时捕获的订阅计划名（可能缺失）。
    plan_label: Option<String>,
}

/// In-memory last-good cache + cooldown + coalesced in-flight refresh.
///
/// 中文说明：Claude 用量的内存缓存状态（对应 JS `index.js`）：last-good
/// 结果缓存、429 冷却截止时间，以及合并并发刷新的在途槽位；由
/// [`QuotaRuntime`] 持有、跨请求共享。
pub struct ClaudeCache {
    /// last-good 用量缓存（含指纹校验）。
    cached_usage: std::sync::Mutex<Option<CachedUsage>>,
    /// 冷却截止的 epoch 毫秒；0 表示不在冷却中。
    cooldown_until: std::sync::Mutex<u64>,
    /// 在途刷新的合并槽位：并发请求共享同一个 future。
    pub pending: Arc<SharedSlot<Value>>,
}

/// [`ClaudeCache`] 的 `Default` 委托给 [`ClaudeCache::new`]。
impl Default for ClaudeCache {
    /// 等价于 [`ClaudeCache::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// [`ClaudeCache`] 的缓存读写、陈旧清理与重置。
impl ClaudeCache {
    /// 创建空缓存（无缓存项、不在冷却、空槽位）。
    pub fn new() -> Self {
        Self {
            cached_usage: std::sync::Mutex::new(None),
            cooldown_until: std::sync::Mutex::new(0),
            pending: Arc::new(SharedSlot::new()),
        }
    }

    /// 指纹匹配时返回缓存命中的结果信封（`ok/configured=true` + 缓存
    /// payload，plan 标签优先用当前凭据的）；指纹不匹配或无缓存返回
    /// `None`。
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

    /// 凭据指纹变化时丢弃旧缓存并清零冷却（换账号即彻底失效）。
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
    ///
    /// 中文说明：测试 seam，对应 JS 的 `resetClaudeQuotaCache`：清空
    /// 缓存、冷却与在途槽位。
    pub fn reset(&self) {
        *self.cached_usage.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cooldown_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = 0;
        self.pending.clear();
    }
}
/// 构造 Claude 的错误结果信封：`ok=false`、`configured` 由调用方指定、
/// `error=message`。
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

/// 实际执行一次（未合并的）配额抓取：加载凭据（未配置/读取失败产出
/// 对应信封）→ 命中缓存或冷却直接返回 → 否则请求用量接口（带 OAuth
/// beta 头与超时），2xx 时转换 payload 并写入缓存，429 时按
/// `Retry-After` 进入冷却并返回 last-good（若有），其余错误返回错误信封。
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

/// 配额抓取主入口（注册表 `fetch` 指向此处）：经 runtime 持有的
/// [`SharedSlot`] 合并并发请求，实际逻辑在 [`fetch_quota_uncoalesced`]
/// 中执行，所有调用方共享同一结果。
pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    let pending = rt.claude_cache.pending.clone();
    let shared = pending.subscribe(move || fetch_quota_uncoalesced(&rt));
    Box::pin(shared)
}
