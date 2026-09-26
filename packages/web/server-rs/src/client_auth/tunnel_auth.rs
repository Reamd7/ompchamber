//! Port of `server/lib/opencode/tunnel-auth.js` (`createTunnelAuth`): the
//! tunnel session auth surface. The tunnels module owns tunnel processes;
//! this controller owns request-scope classification, the one-time bootstrap
//! token exchange (`/connect?t=…`), in-memory tunnel sessions (cookie
//! `oc_tunnel_session`), and the connect rate limiter.
//!
//! Express `req`/`res` are collapsed into [`TunnelRequestContext`] (the
//! request facts the JS reads) and returned header strings for `Set-Cookie`,
//! so handlers stay in charge of the wire.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use serde::Serialize;
use serde_json::json;

use super::time::{Clock, http_date_from_unix_millis};
use super::util::{constant_time_equal_bytes, random_base64url, sha256_hex};
use crate::error::{AppError, AppResult};

pub const TUNNEL_SESSION_COOKIE_NAME: &str = "oc_tunnel_session";
pub(crate) const BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES: usize = 32;
pub(crate) const CONNECT_RATE_LIMIT_WINDOW_MS: i64 = 5 * 60 * 1000;
pub(crate) const CONNECT_RATE_LIMIT_LOCK_MS: i64 = 10 * 60 * 1000;
pub(crate) const CONNECT_RATE_LIMIT_MAX_ATTEMPTS: u64 = 20;
pub(crate) const CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS: u64 = 5;
/// JS rate-limit bucket used when no client IP is derivable.
pub const NO_IP_RATE_LIMIT_KEY: &str = "connect-rate-limit:no-ip";

/// The request facts `tunnel-auth.js` reads off `req`.
pub struct TunnelRequestContext<'a> {
    pub headers: &'a HeaderMap,
    /// Express `req.hostname` (usually derived from the Host header).
    pub hostname: Option<&'a str>,
    /// `req.socket.remoteAddress`.
    pub socket_remote_address: Option<&'a str>,
    /// Express `req.secure` (`req.protocol === 'https'`).
    pub secure: bool,
    /// Express `req.ip` (with `trust proxy` it is XFF-derived).
    pub ip: Option<&'a str>,
}

impl<'a> TunnelRequestContext<'a> {
    pub fn from_headers(headers: &'a HeaderMap) -> Self {
        Self {
            headers,
            hostname: None,
            socket_remote_address: None,
            secure: false,
            ip: None,
        }
    }
}

/// JS `classifyRequestScope` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestScope {
    Tunnel,
    Local,
    UnknownPublic,
}

impl RequestScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestScope::Tunnel => "tunnel",
            RequestScope::Local => "local",
            RequestScope::UnknownPublic => "unknown-public",
        }
    }
}

/// JS `bootstrapRecord` (the JS `id`/`issuedAt` fields are write-only there
/// and are dropped here).
#[derive(Clone, Debug)]
struct BootstrapRecord {
    tunnel_id: String,
    token_hash: String,
    expires_at: Option<i64>,
    used_at: Option<i64>,
    revoked_at: Option<i64>,
}

#[derive(Clone, Debug)]
struct TunnelSessionRecord {
    session_id: String,
    tunnel_id: String,
    mode: Option<String>,
    public_url: Option<String>,
    created_at: i64,
    last_seen_at: i64,
    expires_at: i64,
    revoked_at: Option<i64>,
    revoked_reason: Option<String>,
    expired_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
struct RateRecord {
    count: u64,
    last_attempt: i64,
    locked_until: Option<i64>,
}

#[derive(Default)]
struct TunnelAuthInner {
    active_tunnel_id: Option<String>,
    active_tunnel_host: Option<String>,
    active_tunnel_mode: Option<String>,
    active_tunnel_public_url: Option<String>,
    bootstrap_record: Option<BootstrapRecord>,
    tunnel_sessions: Vec<TunnelSessionRecord>,
    connect_rate_limiter: HashMap<String, RateRecord>,
}

/// Snapshot returned to callers (JS hands back the live record; consumers
/// only read fields).
#[derive(Clone, Debug)]
pub struct TunnelSessionInfo {
    pub session_id: String,
    pub tunnel_id: String,
    pub mode: Option<String>,
    pub public_url: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
}

/// `listTunnelSessions` entry.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTunnelSession {
    pub session_id: String,
    pub tunnel_id: String,
    pub mode: Option<String>,
    pub public_url: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub status: &'static str,
    pub inactive_reason: Option<String>,
}

/// `issueBootstrapToken` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedBootstrapToken {
    pub token: String,
    pub expires_at: Option<i64>,
}

/// `getBootstrapStatus` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapStatus {
    pub has_bootstrap_token: bool,
    pub bootstrap_expires_at: Option<i64>,
}

/// `revokeTunnelArtifacts` result.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokedTunnelArtifacts {
    pub revoked_bootstrap_count: usize,
    pub invalidated_session_count: usize,
}

/// `exchangeBootstrapToken` result.
#[derive(Clone, Debug)]
pub struct ExchangeOutcome {
    pub ok: bool,
    /// Failure reason (`rate-limited`, `inactive`, `missing-token`,
    /// `expired`, `tunnel-mismatch`, `invalid-token`).
    pub reason: Option<&'static str>,
    /// Retry-after hint in seconds when rate limited.
    pub retry_after: Option<i64>,
    pub session_expires_at: Option<i64>,
    /// `Set-Cookie` header value to attach on success.
    pub set_cookie: Option<String>,
}

/// `requireTunnelSession` rejection: the JS middleware's 401 + cookie clear.
#[derive(Clone, Debug)]
pub struct TunnelAuthRejection {
    pub set_cookie: String,
}

impl TunnelAuthRejection {
    pub fn into_response(self) -> axum::response::Response {
        let mut response = (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({
                "error": "Tunnel authentication required",
                "locked": true,
                "tunnelLocked": true,
            })),
        )
            .into_response();
        if let Ok(value) = header::HeaderValue::from_str(&self.set_cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        response
    }
}

pub struct TunnelAuth {
    inner: Mutex<TunnelAuthInner>,
    clock: Clock,
}

impl TunnelAuth {
    pub fn new(clock: Clock) -> Self {
        Self {
            inner: Mutex::new(TunnelAuthInner::default()),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TunnelAuthInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// JS `setActiveTunnel`: record the active tunnel and derive its host
    /// from the public URL.
    pub fn set_active_tunnel(&self, tunnel_id: &str, public_url: Option<&str>, mode: Option<&str>) {
        let mut inner = self.lock();
        inner.active_tunnel_id = Some(tunnel_id.to_string());
        inner.active_tunnel_mode = mode.map(str::to_string);
        inner.active_tunnel_public_url = public_url.map(str::to_string);
        inner.active_tunnel_host = public_url.and_then(host_from_url);
    }

    /// JS `clearActiveTunnel`: revoke artifacts, then forget everything.
    pub fn clear_active_tunnel(&self) {
        let mut inner = self.lock();
        if let Some(tunnel_id) = inner.active_tunnel_id.clone() {
            revoke_tunnel_artifacts_inner(&mut inner, &tunnel_id, self.clock.clone());
        }
        inner.active_tunnel_id = None;
        inner.active_tunnel_host = None;
        inner.active_tunnel_mode = None;
        inner.active_tunnel_public_url = None;
        inner.bootstrap_record = None;
    }

    /// JS `revokeTunnelArtifacts`.
    pub fn revoke_tunnel_artifacts(&self, tunnel_id: &str) -> RevokedTunnelArtifacts {
        let mut inner = self.lock();
        revoke_tunnel_artifacts_inner(&mut inner, tunnel_id, self.clock.clone())
    }

    pub fn active_tunnel_id(&self) -> Option<String> {
        self.lock().active_tunnel_id.clone()
    }

    pub fn active_tunnel_host(&self) -> Option<String> {
        self.lock().active_tunnel_host.clone()
    }

    pub fn active_tunnel_mode(&self) -> Option<String> {
        self.lock().active_tunnel_mode.clone()
    }

    /// JS `issueBootstrapToken`: single-use token bound to the active tunnel.
    pub fn issue_bootstrap_token(&self, ttl_ms: Option<i64>) -> AppResult<IssuedBootstrapToken> {
        let mut inner = self.lock();
        let Some(tunnel_id) = inner.active_tunnel_id.clone() else {
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Tunnel is not active",
            ));
        };
        revoke_bootstrap_token(&mut inner, self.clock.clone());
        let token = random_base64url(BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES);
        let issued_at = (self.clock)();
        let expires_at = ttl_ms.filter(|ttl| *ttl > 0).map(|ttl| issued_at + ttl);
        inner.bootstrap_record = Some(BootstrapRecord {
            tunnel_id,
            token_hash: sha256_hex(token.as_bytes()),
            expires_at,
            used_at: None,
            revoked_at: None,
        });
        Ok(IssuedBootstrapToken { token, expires_at })
    }

    /// JS `getBootstrapStatus`.
    pub fn bootstrap_status(&self) -> BootstrapStatus {
        let inner = self.lock();
        let now = (self.clock)();
        match inner.bootstrap_record.as_ref() {
            Some(record) if is_bootstrap_record_usable(record, now) => BootstrapStatus {
                has_bootstrap_token: true,
                bootstrap_expires_at: record.expires_at,
            },
            _ => BootstrapStatus {
                has_bootstrap_token: false,
                bootstrap_expires_at: None,
            },
        }
    }

    /// JS `classifyRequestScope`.
    pub fn classify_request_scope(&self, ctx: &TunnelRequestContext) -> RequestScope {
        let inner = self.lock();
        let host_header =
            header_str(ctx.headers, header::HOST).and_then(|host| normalize_host(Some(host)));
        let req_host = ctx
            .hostname
            .and_then(|host| normalize_host(Some(host)))
            .or(host_header);
        if let (Some(active_host), Some(host)) =
            (inner.active_tunnel_host.as_deref(), req_host.as_deref())
            && active_host == host
        {
            return RequestScope::Tunnel;
        }
        if is_local_host(req_host.as_deref(), ctx.socket_remote_address) {
            return RequestScope::Local;
        }
        if inner.active_tunnel_id.is_none() {
            return RequestScope::Local;
        }
        RequestScope::UnknownPublic
    }

    /// JS `getTunnelSessionFromRequest`: validate the session cookie against
    /// the in-memory sessions, stamping last-seen on success.
    pub fn get_tunnel_session_from_request(
        &self,
        ctx: &TunnelRequestContext,
    ) -> Option<TunnelSessionInfo> {
        let mut inner = self.lock();
        let now = (self.clock)();
        let active_tunnel_id = inner.active_tunnel_id.clone();
        let token = parse_cookies(header_str(ctx.headers, header::COOKIE))
            .get(TUNNEL_SESSION_COOKIE_NAME)?
            .clone();
        let record = inner
            .tunnel_sessions
            .iter_mut()
            .find(|record| record.session_id == token)?;
        if record.revoked_at.is_some() {
            return None;
        }
        if record.expires_at <= now {
            if record.expired_at.is_none() {
                record.expired_at = Some(now);
            }
            return None;
        }
        if record.tunnel_id != active_tunnel_id.unwrap_or_default() {
            return None;
        }
        record.last_seen_at = now;
        Some(TunnelSessionInfo {
            session_id: record.session_id.clone(),
            tunnel_id: record.tunnel_id.clone(),
            mode: record.mode.clone(),
            public_url: record.public_url.clone(),
            created_at: record.created_at,
            last_seen_at: record.last_seen_at,
            expires_at: record.expires_at,
        })
    }

    /// JS `requireTunnelSession`: session or the cookie-clearing 401.
    pub fn require_tunnel_session(
        &self,
        ctx: &TunnelRequestContext,
    ) -> Result<TunnelSessionInfo, TunnelAuthRejection> {
        match self.get_tunnel_session_from_request(ctx) {
            Some(session) => Ok(session),
            None => Err(TunnelAuthRejection {
                set_cookie: self.clear_tunnel_session_cookie(ctx),
            }),
        }
    }

    /// JS `exchangeBootstrapToken`: rate-limited one-time token exchange for
    /// a tunnel session cookie.
    pub fn exchange_bootstrap_token(
        &self,
        ctx: &TunnelRequestContext,
        token: Option<&str>,
        session_ttl_ms: i64,
    ) -> ExchangeOutcome {
        let rate_limit = self.check_connect_rate_limit(ctx);
        if !rate_limit.allowed {
            return ExchangeOutcome {
                ok: false,
                reason: Some("rate-limited"),
                retry_after: Some(rate_limit.retry_after),
                session_expires_at: None,
                set_cookie: None,
            };
        }

        let mut inner = self.lock();
        if inner.active_tunnel_id.is_none() || inner.bootstrap_record.is_none() {
            drop(inner);
            self.record_connect_failed_attempt(ctx);
            return ExchangeOutcome {
                ok: false,
                reason: Some("inactive"),
                retry_after: None,
                session_expires_at: None,
                set_cookie: None,
            };
        }

        if token.is_none_or(str::is_empty) {
            drop(inner);
            self.record_connect_failed_attempt(ctx);
            return exchange_failure("missing-token");
        }

        let now = (self.clock)();
        if !inner
            .bootstrap_record
            .as_ref()
            .is_some_and(|record| is_bootstrap_record_usable(record, now))
        {
            drop(inner);
            self.record_connect_failed_attempt(ctx);
            return exchange_failure("expired");
        }

        if inner.bootstrap_record.as_ref().map(|r| r.tunnel_id.clone())
            != inner.active_tunnel_id.clone()
        {
            drop(inner);
            self.record_connect_failed_attempt(ctx);
            return exchange_failure("tunnel-mismatch");
        }

        let incoming_hash = sha256_hex(token.unwrap_or_default().as_bytes());
        let expected = inner
            .bootstrap_record
            .as_ref()
            .map(|record| record.token_hash.clone())
            .unwrap_or_default();
        if incoming_hash.len() != expected.len()
            || !constant_time_equal_bytes(incoming_hash.as_bytes(), expected.as_bytes())
        {
            drop(inner);
            self.record_connect_failed_attempt(ctx);
            return exchange_failure("invalid-token");
        }

        if let Some(record) = inner.bootstrap_record.as_mut() {
            record.used_at = Some((self.clock)());
        }
        inner.connect_rate_limiter.remove(&rate_limit_key(ctx));

        let session_id = random_base64url(32);
        let created_at = (self.clock)();
        let expires_at = created_at + session_ttl_ms;
        let record = TunnelSessionRecord {
            session_id: session_id.clone(),
            tunnel_id: inner.active_tunnel_id.clone().unwrap_or_default(),
            mode: inner.active_tunnel_mode.clone(),
            public_url: inner.active_tunnel_public_url.clone(),
            created_at,
            last_seen_at: created_at,
            expires_at,
            revoked_at: None,
            revoked_reason: None,
            expired_at: None,
        };
        inner.tunnel_sessions.push(record);
        drop(inner);

        ExchangeOutcome {
            ok: true,
            reason: None,
            retry_after: None,
            session_expires_at: Some(expires_at),
            set_cookie: Some(tunnel_session_cookie_header(
                &session_id,
                session_ttl_ms,
                is_secure_request(ctx),
                (self.clock)(),
            )),
        }
    }

    /// JS `listTunnelSessions` (newest first).
    pub fn list_tunnel_sessions(&self) -> Vec<PublicTunnelSession> {
        let mut inner = self.lock();
        let now = (self.clock)();
        let active_tunnel_id = inner.active_tunnel_id.clone();
        let mut sessions = Vec::new();
        for record in inner.tunnel_sessions.iter_mut() {
            let is_expired = record.expires_at <= now;
            if is_expired && record.expired_at.is_none() {
                record.expired_at = Some(now);
            }
            let active = record.revoked_at.is_none()
                && !is_expired
                && Some(record.tunnel_id.clone()) == active_tunnel_id;
            let status = if active { "active" } else { "inactive" };
            let inactive_reason = if record.revoked_at.is_some() {
                Some(
                    record
                        .revoked_reason
                        .clone()
                        .unwrap_or_else(|| "revoked".to_string()),
                )
            } else if is_expired {
                Some("expired".to_string())
            } else if !active {
                Some("inactive".to_string())
            } else {
                None
            };
            sessions.push(PublicTunnelSession {
                session_id: record.session_id.clone(),
                tunnel_id: record.tunnel_id.clone(),
                mode: record.mode.clone(),
                public_url: record.public_url.clone(),
                created_at: record.created_at,
                last_seen_at: record.last_seen_at,
                expires_at: record.expires_at,
                revoked_at: record.revoked_at,
                status,
                inactive_reason: if status == "inactive" {
                    inactive_reason
                } else {
                    None
                },
            });
        }
        sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        sessions
    }

    /// JS `clearTunnelSessionCookie` → the `Set-Cookie` header value.
    pub fn clear_tunnel_session_cookie(&self, ctx: &TunnelRequestContext) -> String {
        build_cookie(
            TUNNEL_SESSION_COOKIE_NAME,
            "",
            0,
            is_secure_request(ctx),
            (self.clock)(),
        )
    }

    // -- connect rate limiter (JS `checkConnectRateLimit` family) ------------

    fn check_connect_rate_limit(&self, ctx: &TunnelRequestContext) -> RateLimitDecision {
        let key = rate_limit_key(ctx);
        let now = (self.clock)();
        let max_attempts = rate_limit_max_for_key(&key);
        let mut inner = self.lock();
        let record = inner.connect_rate_limiter.get(&key).cloned();

        if let Some(locked_until) = record.as_ref().and_then(|r| r.locked_until)
            && now < locked_until
        {
            return RateLimitDecision {
                allowed: false,
                retry_after: (locked_until - now + 999) / 1000,
            };
        }

        match record {
            None => RateLimitDecision {
                allowed: true,
                retry_after: 0,
            },
            Some(record) if now - record.last_attempt > CONNECT_RATE_LIMIT_WINDOW_MS => {
                RateLimitDecision {
                    allowed: true,
                    retry_after: 0,
                }
            }
            Some(record) => {
                if record.count >= max_attempts {
                    let locked_until = now + CONNECT_RATE_LIMIT_LOCK_MS;
                    inner.connect_rate_limiter.insert(
                        key,
                        RateRecord {
                            count: record.count + 1,
                            last_attempt: now,
                            locked_until: Some(locked_until),
                        },
                    );
                    RateLimitDecision {
                        allowed: false,
                        retry_after: (CONNECT_RATE_LIMIT_LOCK_MS + 999) / 1000,
                    }
                } else {
                    RateLimitDecision {
                        allowed: true,
                        retry_after: 0,
                    }
                }
            }
        }
    }

    fn record_connect_failed_attempt(&self, ctx: &TunnelRequestContext) {
        let key = rate_limit_key(ctx);
        let now = (self.clock)();
        let mut inner = self.lock();
        let record = inner.connect_rate_limiter.get(&key).cloned();
        let next = match record {
            Some(record) if now - record.last_attempt <= CONNECT_RATE_LIMIT_WINDOW_MS => {
                RateRecord {
                    count: record.count + 1,
                    last_attempt: now,
                    locked_until: record.locked_until,
                }
            }
            _ => RateRecord {
                count: 1,
                last_attempt: now,
                locked_until: None,
            },
        };
        inner.connect_rate_limiter.insert(key, next);
    }
}

struct RateLimitDecision {
    allowed: bool,
    retry_after: i64,
}

fn exchange_failure(reason: &'static str) -> ExchangeOutcome {
    ExchangeOutcome {
        ok: false,
        reason: Some(reason),
        retry_after: None,
        session_expires_at: None,
        set_cookie: None,
    }
}

fn revoke_tunnel_artifacts_inner(
    inner: &mut TunnelAuthInner,
    tunnel_id: &str,
    clock: Clock,
) -> RevokedTunnelArtifacts {
    let revoked_bootstrap_count = if inner
        .bootstrap_record
        .as_ref()
        .is_some_and(|record| record.tunnel_id == tunnel_id)
    {
        revoke_bootstrap_token(inner, clock.clone())
    } else {
        0
    };
    let revoked_at = clock();
    let mut invalidated_session_count = 0;
    for record in inner.tunnel_sessions.iter_mut() {
        if record.tunnel_id == tunnel_id && record.revoked_at.is_none() {
            record.revoked_at = Some(revoked_at);
            record.revoked_reason = Some("tunnel-revoked".to_string());
            invalidated_session_count += 1;
        }
    }
    RevokedTunnelArtifacts {
        revoked_bootstrap_count,
        invalidated_session_count,
    }
}

fn revoke_bootstrap_token(inner: &mut TunnelAuthInner, clock: Clock) -> usize {
    let Some(record) = inner.bootstrap_record.as_mut() else {
        return 0;
    };
    if record.revoked_at.is_some() {
        return 0;
    }
    record.revoked_at = Some(clock());
    1
}

fn is_bootstrap_record_usable(record: &BootstrapRecord, now: i64) -> bool {
    if record.revoked_at.is_some() || record.used_at.is_some() {
        return false;
    }
    if let Some(expires_at) = record.expires_at
        && now >= expires_at
    {
        return false;
    }
    true
}

/// JS `setTunnelSessionCookie` → the `Set-Cookie` header value.
fn tunnel_session_cookie_header(session_id: &str, ttl_ms: i64, secure: bool, now: i64) -> String {
    let max_age = (ttl_ms / 1000).max(0);
    build_cookie(
        TUNNEL_SESSION_COOKIE_NAME,
        &percent_encode(session_id),
        max_age,
        secure,
        now,
    )
}

/// JS `buildCookie`.
fn build_cookie(name: &str, value: &str, max_age_secs: i64, secure: bool, now: i64) -> String {
    let mut attributes = vec![
        format!("{name}={value}"),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];
    attributes.push(format!("Max-Age={}", max_age_secs.max(0)));
    let expires = if max_age_secs == 0 {
        0
    } else {
        now + max_age_secs * 1000
    };
    attributes.push(format!("Expires={}", http_date_from_unix_millis(expires)));
    if secure {
        attributes.push("Secure".to_string());
    }
    attributes.join("; ")
}

/// JS `isSecureRequest`.
fn is_secure_request(ctx: &TunnelRequestContext) -> bool {
    if ctx.secure {
        return true;
    }
    header_str(
        ctx.headers,
        header::HeaderName::from_static("x-forwarded-proto"),
    )
    .map(|forwarded| {
        forwarded
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("https")
    })
    .unwrap_or(false)
}

fn header_str(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// JS `parseCookies` (percent-decoding is lenient where
/// `decodeURIComponent` would throw — invalid escapes stay literal).
fn parse_cookies(cookie_header: Option<&str>) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    let Some(header) = cookie_header else {
        return cookies;
    };
    for segment in header.split(';') {
        let mut parts = segment.splitn(2, '=');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let value = parts.next().unwrap_or("").trim();
        cookies.insert(name.to_string(), percent_decode(value));
    }
    cookies
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = value
                .get(i + 1..i + 3)
                .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// JS `normalizeHost`: trim, lowercase, strip a trailing `:port`.
fn normalize_host(candidate: Option<&str>) -> Option<String> {
    let trimmed = candidate?.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .map(|(host, _)| host.to_string())
        .unwrap_or(trimmed);
    Some(stripped)
}

/// `new URL(publicUrl).host` — bracket IPv6 literals like the JS `.host`
/// property does.
fn host_from_url(public_url: &str) -> Option<String> {
    let url = url::Url::parse(public_url).ok()?;
    let host = url.host_str()?;
    // `new URL(...).host` keeps IPv6 brackets; `host_str()` does not.
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    normalize_host(Some(&host))
}

/// JS `normalizeIpCandidate`.
fn normalize_ip_candidate(candidate: Option<&str>) -> Option<String> {
    let trimmed = candidate?.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let without_brackets = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map(str::to_string)
        .unwrap_or(trimmed);
    let without_zone = without_brackets
        .split('%')
        .next()
        .filter(|zone| !zone.is_empty())?
        .to_string();
    if let Some(mapped) = without_zone.strip_prefix("::ffff:")
        && is_dotted_quad(mapped)
    {
        return Some(mapped.to_string());
    }
    Some(without_zone)
}

fn is_dotted_quad(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && part.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// JS `isPrivateOrLoopbackIpv4`.
fn is_private_or_loopback_ipv4(candidate: &str) -> bool {
    let parts: Vec<u16> = candidate
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    if parts.len() != 4
        || !candidate
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let [first, second, ..] = parts[..] else {
        return false;
    };
    first == 127
        || first == 10
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
        || (first == 169 && second == 254)
}

/// JS `isPrivateOrLoopbackIpv6`.
fn is_private_or_loopback_ipv6(candidate: &str) -> bool {
    if candidate == "::1" {
        return true;
    }
    if candidate.starts_with("fc") || candidate.starts_with("fd") {
        return true;
    }
    candidate.starts_with("fe8")
        || candidate.starts_with("fe9")
        || candidate.starts_with("fea")
        || candidate.starts_with("feb")
}

fn is_private_or_loopback_ip(candidate: Option<&str>) -> bool {
    let Some(normalized) = normalize_ip_candidate(candidate) else {
        return false;
    };
    if normalized.contains(':') {
        is_private_or_loopback_ipv6(&normalized)
    } else {
        is_private_or_loopback_ipv4(&normalized)
    }
}

/// JS `isLocalHost`: a local-looking Host header only counts when the socket
/// peer is also private/loopback (a public peer spoofing Host stays public).
fn is_local_host(host: Option<&str>, socket_remote_address: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let is_local_hostname = host == "localhost"
        || host == "host.docker.internal"
        || is_private_or_loopback_ip(Some(host));
    is_local_hostname && is_private_or_loopback_ip(socket_remote_address)
}

/// JS `getClientIp`: first `X-Forwarded-For` entry, else `req.ip` / socket
/// address; `::ffff:` mappings unwrapped.
fn get_client_ip(ctx: &TunnelRequestContext) -> Option<String> {
    if let Some(forwarded) = header_str(
        ctx.headers,
        header::HeaderName::from_static("x-forwarded-for"),
    ) {
        let ip = forwarded.split(',').next().unwrap_or("").trim();
        return Some(strip_ipv6_mapped(ip));
    }
    ctx.ip
        .or(ctx.socket_remote_address)
        .map(|ip| strip_ipv6_mapped(ip.trim()))
}

fn strip_ipv6_mapped(ip: &str) -> String {
    ip.strip_prefix("::ffff:").unwrap_or(ip).to_string()
}

fn rate_limit_key(ctx: &TunnelRequestContext) -> String {
    get_client_ip(ctx).unwrap_or_else(|| NO_IP_RATE_LIMIT_KEY.to_string())
}

fn rate_limit_max_for_key(key: &str) -> u64 {
    if key == NO_IP_RATE_LIMIT_KEY {
        CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS
    } else {
        CONNECT_RATE_LIMIT_MAX_ATTEMPTS
    }
}
