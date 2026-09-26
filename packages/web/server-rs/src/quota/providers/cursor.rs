//! Port of `server/lib/quota/providers/cursor.js` — Cursor dashboard quota via
//! the `aiserver.v1.DashboardService` Connect endpoints, with JWT-aware token
//! refresh against `https://api2.cursor.sh/oauth/token`.
//!
//! Auth resolution: env tokens, token files, the Settings-managed credential,
//! or the one-time explicit import from Cursor's `state.vscdb` (never the
//! browser cookie store, and Cursor's database is only ever read).

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

pub const PROVIDER_ID: &str = "cursor";
pub const PROVIDER_NAME: &str = "Cursor";
pub const ALIASES: [&str; 1] = ["cursor"];

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const PLAN_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo";
const CREDITS_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthSource {
    Env,
    File,
    Managed,
    Import,
    Validation,
}

struct AuthState {
    access_token: Option<String>,
    refresh_token: Option<String>,
    source: AuthSource,
}

/// `STATE_DB` — Cursor's globalStorage SQLite database (read-only import).
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
pub fn read_jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.as_object().cloned()
}

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
fn cents_label(cents: Option<f64>) -> Option<String> {
    let cents = cents?;
    Some(format!(
        "${}",
        format_money(Some(cents / 100.0)).unwrap_or_default()
    ))
}

/// `percentFromSpend`.
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

pub fn is_configured(deps: &QuotaDeps) -> bool {
    let auth = load_auth_state(deps);
    auth.access_token.is_some() || auth.refresh_token.is_some()
}

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
