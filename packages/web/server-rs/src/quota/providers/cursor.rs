//! Port of `server/lib/quota/providers/cursor.js` — Cursor dashboard quota via
//! the `aiserver.v1.DashboardService` Connect endpoints, with JWT-aware token
//! refresh against `https://api2.cursor.sh/oauth/token`.
//!
//! Auth resolution: env tokens, token files, the Settings-managed credential,
//! or the one-time explicit import from Cursor's `state.vscdb` (never the
//! browser cookie store, and Cursor's database is only ever read).
//!
//! 中文说明：本模块是 `server/lib/quota/providers/cursor.js` 的移植：通过
//! `aiserver.v1.DashboardService` Connect 端点获取 Cursor 面板配额，并
//! 针对 `https://api2.cursor.sh/oauth/token` 做 JWT 感知的 token 刷新。
//! 凭据解析顺序：环境变量 token、token 文件、Settings 管理的受管凭据，
//! 或从 Cursor `state.vscdb` 一次性显式导入（绝不读取浏览器 cookie，
//! 且对 Cursor 数据库只读）。

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::credentials::{read_managed_credential, write_managed_credential};
use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::QuotaRuntime;
use crate::quota::utils::{
    build_result, field, format_money, to_number, to_timestamp, to_usage_window, usage_payload,
};

/// provider 注册 ID（注册表与 API 路径中使用）。
pub const PROVIDER_ID: &str = "cursor";
/// provider 展示名。
pub const PROVIDER_NAME: &str = "Cursor";
/// provider 别名（仅 `cursor`）。
pub const ALIASES: [&str; 1] = ["cursor"];

/// 当前计费周期用量端点。
const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
/// 订阅计划信息端点。
const PLAN_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo";
/// 付费 credit 余额端点。
const CREDITS_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance";
/// OAuth token 刷新端点。
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// OAuth 刷新使用的公开 client_id（与 Cursor 客户端一致）。
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
/// 判定 token 是否需要刷新的提前量（5 分钟）。
const REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;

/// 凭据来源：环境变量、token 文件、受管存储、一次性导入或凭据校验流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthSource {
    /// `CURSOR_TOKEN`/`CURSOR_ACCESS_TOKEN`/`CURSOR_REFRESH_TOKEN` 环境变量。
    Env,
    /// `CURSOR_TOKEN_FILE`/`CURSOR_REFRESH_TOKEN_FILE` 指向的文件。
    File,
    /// Settings 管理的受管凭据（quota 目录下落盘的 JSON）。
    Managed,
    /// 从 Cursor `state.vscdb` 的一次性导入。
    Import,
    /// 凭据校验（validate/import）流程中构造的临时状态。
    Validation,
}

/// 当前解析出的认证状态：access/refresh token 与来源。
struct AuthState {
    /// OAuth access token（可能缺失，等待刷新或导入）。
    access_token: Option<String>,
    /// OAuth refresh token（用于换取新 access token）。
    refresh_token: Option<String>,
    /// 本状态的来源（决定刷新后 token 持久化到哪）。
    source: AuthSource,
}

/// `STATE_DB` — Cursor's globalStorage SQLite database (read-only import).
///
/// 中文说明：Cursor 的 globalStorage SQLite 数据库路径（仅用于只读导入）：
/// `<home>/Library/Application Support/Cursor/User/globalStorage/state.vscdb`；
/// home 目录未知时返回 `None`。
fn state_db_path(deps: &QuotaDeps) -> Option<PathBuf> {
    (deps.home_dir)().map(|home| {
        home.join("Library")
            .join("Application Support")
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    })
}

/// `readJwtPayload` — base64url-decoded JWT claims.
///
/// 中文说明：解码 JWT payload 段（base64url）为 claims 对象；分割、解码
/// 或 JSON 解析任一失败都返回 `None`。
pub fn read_jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.as_object().cloned()
}

/// 读取 token 文件内容并 trim；路径缺失、读取失败或内容为空都返回
/// `None`。
fn read_file_token(path: Option<&str>) -> Option<String> {
    let path = path?;
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// `loadAuthState` — env, then token files, then the managed credential.
///
/// 中文说明：解析认证状态（对应 JS `loadAuthState`）：先看环境变量
/// （`CURSOR_TOKEN`/`CURSOR_ACCESS_TOKEN` 与 `CURSOR_REFRESH_TOKEN`），
/// 再看 token 文件（`*_FILE` 指向的文件），最后回落到 Settings 管理的
/// 受管凭据；任一层产出 token 即确定来源。
fn load_auth_state(deps: &QuotaDeps) -> AuthState {
    let env_access = (deps.env)("CURSOR_TOKEN").or_else(|| (deps.env)("CURSOR_ACCESS_TOKEN"));
    let env_refresh = (deps.env)("CURSOR_REFRESH_TOKEN");
    if env_access.is_some() || env_refresh.is_some() {
        return AuthState {
            access_token: env_access,
            refresh_token: env_refresh,
            source: AuthSource::Env,
        };
    }

    let file_access = read_file_token((deps.env)("CURSOR_TOKEN_FILE").as_deref());
    let file_refresh = read_file_token((deps.env)("CURSOR_REFRESH_TOKEN").as_deref());
    if file_access.is_some() || file_refresh.is_some() {
        return AuthState {
            access_token: file_access,
            refresh_token: file_refresh,
            source: AuthSource::File,
        };
    }

    let managed = read_managed_credential(deps, PROVIDER_ID);
    AuthState {
        access_token: managed
            .as_ref()
            .and_then(|credential| field(credential, "accessToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
        refresh_token: managed
            .as_ref()
            .and_then(|credential| field(credential, "refreshToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
        source: AuthSource::Managed,
    }
}

/// `tokenNeedsRefresh` — missing token, missing exp, or exp within 5 minutes.
///
/// 中文说明：token 是否需要刷新（对应 JS `tokenNeedsRefresh`）：token
/// 缺失、JWT 无 `exp`，或 `exp` 距当前不足 [`REFRESH_BUFFER_MS`]
///（5 分钟）都返回 true。
fn token_needs_refresh(deps: &QuotaDeps, token: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    let Some(claims) = read_jwt_payload(token) else {
        return true;
    };
    let Some(expires_at) = claims.get("exp").and_then(Value::as_f64) else {
        return true;
    };
    let expires_at_ms = (expires_at * 1000.0) as i64;
    expires_at_ms - deps.now_ms() as i64 <= REFRESH_BUFFER_MS
}

/// 把刷新得到的 access token 持久化回其来源：环境变量/文件/导入来源
/// 无法写回则跳过；受管来源写回受管凭据存储。
fn persist_access_token(deps: &QuotaDeps, auth: &AuthState, access_token: &str) {
    if auth.source == AuthSource::Managed {
        let _ = write_managed_credential(
            deps,
            PROVIDER_ID,
            &json!({
                "accessToken": access_token,
                "refreshToken": auth.refresh_token.clone().unwrap_or_default(),
            }),
        );
    }
}

/// `refreshAccessToken`.
///
/// 中文说明：刷新 access token（对应 JS `refreshAccessToken`）：向
/// [`REFRESH_URL`] 发 JSON POST（client_id + refresh_token + 固定
/// session），要求 2xx 且响应含非空 `access_token`，否则返回具体错误
/// 消息；成功返回新 token（刷新结果由调用方按来源持久化）。
async fn refresh_access_token(deps: &QuotaDeps, auth: &AuthState) -> Result<String, String> {
    let Some(refresh_token) = auth.refresh_token.as_deref() else {
        return Ok(auth.access_token.clone().unwrap_or_default());
    };

    let request = HttpRequest::post(REFRESH_URL).json_body(&json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    }));

    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;
    let body = response.json().unwrap_or(Value::Null);

    if field(&body, "shouldLogout") == Some(&Value::Bool(true)) {
        return Err("Session expired - please sign in to Cursor again".to_string());
    }
    if !response.ok() {
        return Err(if response.status == 401 {
            "Cursor session expired".to_string()
        } else {
            format!("API error: {}", response.status)
        });
    }
    let Some(access_token) = field(&body, "access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return Err("Cursor refresh response did not include an access token".to_string());
    };
    let access_token = access_token.to_string();
    persist_access_token(deps, auth, &access_token);
    Ok(access_token)
}

/// `resolveCredentialAccessToken`.
///
/// 中文说明：解析可用的 access token（对应 JS
/// `resolveCredentialAccessToken`）：状态里已有仍有效的 token 直接用；
/// 有 refresh token 但 token 缺失/临期则先刷新（并持久化）；仅有受管
/// refresh token（如只填了 refreshToken）也走刷新。返回 `Ok(None)`
/// 表示该来源完全无凭据。
async fn resolve_credential_access_token(
    deps: &QuotaDeps,
    auth: &AuthState,
) -> Result<Option<String>, String> {
    if auth.access_token.is_none() && auth.refresh_token.is_none() {
        return Ok(None);
    }
    if !token_needs_refresh(deps, auth.access_token.as_deref()) {
        return Ok(auth.access_token.clone());
    }
    if auth.refresh_token.is_none() {
        // JS refreshAccessToken returns auth.accessToken when no refresh
        // token exists (possibly undefined → None).
        return Ok(auth.access_token.clone().filter(|token| !token.is_empty()));
    }
    refresh_access_token(deps, auth).await.map(Some)
}

/// `readStateValue` — sqlite3 read of Cursor's state.vscdb.
///
/// 中文说明：用注入的 `sqlite3 -json` 查询 Cursor 的 state.vscdb（只读），
/// 取 ItemTable 中 `key = <key>` 行的 `value` 列。
fn read_state_value(deps: &QuotaDeps, key: &str) -> Option<String> {
    let db = state_db_path(deps)?;
    if !db.exists() {
        return None;
    }
    let escaped_key = key.replace('\'', "''");
    let query = format!("SELECT value FROM ItemTable WHERE key = '{escaped_key}' LIMIT 1;");
    (deps.sqlite_value)(&db, &query)
}

/// `importCursorCredential` — explicit one-time import from Cursor storage.
///
/// 中文说明：从 Cursor 本机存储显式导入凭据（对应 JS
/// `importCursorCredential`）：从 `state.vscdb` 读出 cookie 中的
/// WorkosCursorSessionToken，拆出 access/refresh token，写入受管凭据
/// 存储并返回 status 掩码载荷；数据库缺失或 token 不可解析时返回错误。
pub async fn import_cursor_credential(deps: &QuotaDeps) -> Result<Value, String> {
    let credential = json!({
        "accessToken": read_state_value(deps, "cursorAuth/accessToken").unwrap_or_default(),
        "refreshToken": read_state_value(deps, "cursorAuth/refreshToken").unwrap_or_default(),
    });
    let access = field(&credential, "accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let refresh = field(&credential, "refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if access.is_empty() && refresh.is_empty() {
        return Err("Cursor credentials are unavailable".to_string());
    }
    let auth = AuthState {
        access_token: (!access.is_empty()).then(|| access.to_string()),
        refresh_token: (!refresh.is_empty()).then(|| refresh.to_string()),
        source: AuthSource::Import,
    };
    let access_token = resolve_credential_access_token(deps, &auth)
        .await?
        .filter(|token| !token.is_empty())
        .ok_or("Cursor credentials are invalid")?;
    // JS stores `{ ...credential, accessToken }` with the resolved token.
    write_managed_credential(
        deps,
        PROVIDER_ID,
        &json!({ "accessToken": access_token, "refreshToken": refresh }),
    )
}

/// `validateCursorCredential` — the credential must mint a working token and
/// reach the usage endpoint.
///
/// 中文说明：校验受管凭据（对应 JS `validateCursorCredential`）：把
/// 归一化后的凭据组装成临时 AuthState，刷新/换取可用 access token，
/// 并真实调用一次用量端点；两步任一失败都返回 `Err`（转成 400）。
pub async fn validate_cursor_credential(
    deps: &QuotaDeps,
    credential: &Value,
) -> Result<(), String> {
    let auth = AuthState {
        access_token: field(credential, "accessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        refresh_token: field(credential, "refreshToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        source: AuthSource::Validation,
    };
    let access_token = resolve_credential_access_token(deps, &auth)
        .await?
        .filter(|token| !token.is_empty())
        .ok_or("Cursor credentials are invalid")?;
    connect_post(deps, USAGE_URL, &access_token)
        .await
        .map(|_| ())
}

/// `connectPost` — Connect-protocol POST with an empty JSON body.
///
/// 中文说明：Connect 协议 POST（对应 JS `connectPost`）：空 JSON body
/// `{}`、bearer token 与 JSON 头；非 2xx 报 HTTP 状态错误，body 解析
/// 失败报 invalid JSON 错误。
async fn connect_post(deps: &QuotaDeps, url: &str, access_token: &str) -> Result<Value, String> {
    let request = HttpRequest::post(url)
        .bearer(access_token)
        .header("Connect-Protocol-Version", "1")
        .json_body(&json!({}));

    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;
    if !response.ok() {
        return Err(if response.status == 401 {
            "Cursor session expired".to_string()
        } else {
            format!("API error: {}", response.status)
        });
    }
    response
        .json()
        .map_err(|_| "Invalid response from provider".to_string())
}

/// `centsLabel` — integer cents to a `$x.yy` label.
///
/// 中文说明：整数美分转 `$x.yy` 展示标签（对应 JS `centsLabel`）；
/// 输入缺失或非有限值返回 `None`。
fn cents_label(cents: Option<f64>) -> Option<String> {
    let cents = cents?;
    Some(format!(
        "${}",
        format_money(Some(cents / 100.0)).unwrap_or_default()
    ))
}

/// `percentFromSpend`.
///
/// 中文说明：由计划用量金额反推已用百分比（对应 JS `percentFromSpend`）：
/// `usedCents`/`priceCents` 均可解析且价格 > 0 时计算
/// `used/price*100`，否则 `None`。
fn percent_from_spend(plan_usage: &Value) -> Option<f64> {
    if let Some(explicit) = to_number(field(plan_usage, "totalPercentUsed")) {
        return Some(explicit);
    }
    let limit = to_number(field(plan_usage, "limit"))?;
    let remaining = to_number(field(plan_usage, "remaining"))?;
    if limit == 0.0 {
        return None;
    }
    Some((((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0))
}

/// `buildWindows` — billing_cycle/auto/api/plan_limit/on_demand windows.
///
/// 中文说明：组装用量窗口（对应 JS `buildWindows`）：从 usage payload
/// 提取 `billing_cycle`（百分比 + 重置时间）、`auto`（自动配额条目，
/// 可能多条）、`api`（API 请求条目）、`plan_limit`（按模型的条目上限）
/// 与 `on_demand`（按需付费金额）窗口；条目缺失即跳过。
pub fn build_windows(usage: &Value, plan: Option<&Value>, now: u64) -> Map<String, Value> {
    let plan_usage = field(usage, "planUsage").unwrap_or(&Value::Null);
    let spend_limit_usage = field(usage, "spendLimitUsage").unwrap_or(&Value::Null);
    let reset_at = to_timestamp(field(usage, "billingCycleEnd")).or_else(|| {
        plan.and_then(|plan| {
            to_timestamp(field(plan, "planInfo").and_then(|info| field(info, "billingCycleEnd")))
        })
    });
    let window_seconds = reset_at.map(|ms| (((ms - now as i64) / 1000).max(0)) as f64);
    let reset_at_value = reset_at.map(|ms| json!(ms));

    let mut windows = Map::new();
    windows.insert(
        "billing_cycle".into(),
        to_usage_window(
            now,
            percent_from_spend(plan_usage),
            window_seconds,
            reset_at_value.as_ref(),
            cents_label(to_number(field(plan_usage, "totalSpend"))).as_deref(),
        ),
    );

    if let Some(auto_percent) = to_number(field(plan_usage, "autoPercentUsed")) {
        windows.insert(
            "auto".into(),
            to_usage_window(
                now,
                Some(auto_percent),
                window_seconds,
                reset_at_value.as_ref(),
                None,
            ),
        );
    }
    if let Some(api_percent) = to_number(field(plan_usage, "apiPercentUsed")) {
        windows.insert(
            "api".into(),
            to_usage_window(
                now,
                Some(api_percent),
                window_seconds,
                reset_at_value.as_ref(),
                None,
            ),
        );
    }

    let plan_limit_label = cents_label(to_number(field(plan_usage, "limit")));
    if let Some(plan_limit) = plan_limit_label.as_deref() {
        let limit = to_number(field(plan_usage, "limit"));
        let remaining = to_number(field(plan_usage, "remaining"));
        let used_percent = match (limit, remaining) {
            (Some(limit), Some(remaining)) if limit != 0.0 => {
                Some((((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        };
        let remaining_label = cents_label(remaining).unwrap_or_else(|| "$0.00".to_string());
        windows.insert(
            "plan_limit".into(),
            to_usage_window(
                now,
                used_percent,
                window_seconds,
                reset_at_value.as_ref(),
                Some(&format!("{remaining_label} remaining of {plan_limit}")),
            ),
        );
    }

    let on_demand_limit = to_number(field(spend_limit_usage, "individualLimit"))
        .or_else(|| to_number(field(spend_limit_usage, "pooledLimit")));
    if on_demand_limit.is_some_and(|limit| limit > 0.0) {
        let limit = on_demand_limit.unwrap_or_default();
        let remaining = to_number(field(spend_limit_usage, "individualRemaining"))
            .or_else(|| to_number(field(spend_limit_usage, "pooledRemaining")))
            .unwrap_or(0.0);
        let remaining_label = cents_label(Some(remaining)).unwrap_or_else(|| "$0.00".to_string());
        windows.insert(
            "on_demand".into(),
            to_usage_window(
                now,
                Some((((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0)),
                window_seconds,
                reset_at_value.as_ref(),
                Some(&format!(
                    "{remaining_label} remaining of {}",
                    cents_label(Some(limit)).unwrap_or_default()
                )),
            ),
        );
    }

    windows
}

/// `appendCreditsWindow`.
///
/// 中文说明：追加 credit 余额窗口（对应 JS `appendCreditsWindow`）：
/// 有余额数据时写入 `credits` 窗口（剩余额度标签，无百分比）。
fn append_credits_window(windows: &mut Map<String, Value>, credits: Option<&Value>, now: u64) {
    let Some(credits) = credits else { return };
    let balance = to_number(field(credits, "balanceCents"))
        .or_else(|| to_number(field(credits, "totalBalanceCents")))
        .or_else(|| to_number(field(credits, "amountCents")));
    let Some(balance) = balance else { return };
    windows.insert(
        "credits".into(),
        to_usage_window(now, None, None, None, cents_label(Some(balance)).as_deref()),
    );
}

/// provider 是否已配置：凭据解析链（env → 文件 → 受管存储）任一层产出
/// token 或 refresh token 即为 true。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    let auth = load_auth_state(deps);
    auth.access_token.is_some() || auth.refresh_token.is_some()
}

/// 配额抓取主入口（注册表 `fetch` 指向此处）：解析认证状态（未配置返回
/// "Not configured" 信封）→ 换取可用 access token → 并发拉取用量与计划
///（以及受管凭据时的 credit 余额），组装各用量窗口后产出结果信封；
/// 失败路径返回 `ok=false` 的错误信封。
pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let auth = load_auth_state(&deps);
        let access_token = match resolve_credential_access_token(&deps, &auth).await {
            Ok(Some(token)) if !token.is_empty() => token,
            Ok(_) => {
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

        let usage = connect_post(&deps, USAGE_URL, &access_token).await;
        let plan = connect_post(&deps, PLAN_URL, &access_token).await.ok();
        let credits = connect_post(&deps, CREDITS_URL, &access_token).await.ok();

        let usage = match usage {
            Ok(usage) => usage,
            Err(message) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    true,
                    None,
                    Some(&message),
                    None,
                    deps.now_ms(),
                );
            }
        };

        if field(&usage, "enabled") == Some(&Value::Bool(false))
            || field(&usage, "planUsage").is_none()
        {
            return build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some("No active Cursor subscription"),
                None,
                deps.now_ms(),
            );
        }

        let now = deps.now_ms();
        let mut windows = build_windows(&usage, plan.as_ref(), now);
        append_credits_window(&mut windows, credits.as_ref(), now);

        let provider_name = plan
            .as_ref()
            .and_then(|plan| field(plan, "planInfo"))
            .and_then(|info| field(info, "planName"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| format!("Cursor {name}"))
            .unwrap_or_else(|| PROVIDER_NAME.to_string());

        build_result(
            PROVIDER_ID,
            &provider_name,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}
