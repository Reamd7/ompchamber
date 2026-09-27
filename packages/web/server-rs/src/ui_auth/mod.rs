//! Port of `server/lib/ui-auth/ui-auth.js` (UI password gate, session tokens,
//! login rate limiting, `oc_url_token` minting) plus the request-origin check
//! and WebSocket-upgrade rejection from `server/lib/security/request-security.js`.
//!
//! Route ownership (JS `registerAuthAndAccessRoutes` in
//! `server/lib/opencode/core-routes.js`):
//! - `GET/POST /auth/session`
//! - `POST /auth/url-token`
//! - `GET  /auth/passkey/status`
//! - `POST /auth/passkey/authenticate/{options,verify}`
//! - `POST /auth/passkey/register/{options,verify}` (session-gated)
//! - `GET/DELETE /api/passkeys[/{id}]`, `POST /api/auth/reset` (session-gated)
//!
//! In JS the password gate is `app.use('/api', requireApiAuth)` at the END of
//! `registerAuthAndAccessRoutes`: every `/api` request not already terminated
//! by an earlier-registered route passes through it. The equivalent in this
//! port is the exported [`middleware`] layer / [`guard`] function, which the
//! proxy, fs and event-stream modules apply to their own `/api` routes
//! (axum merges stateless routers, so a module layer only covers its own
//! routes — exact sibling routes structurally take precedence, matching the
//! JS registration order). Public pre-gate GETs (`/api/version`,
//! `/api/system/info`, `/api/system/free-port`) are exempt inside the layer.
//!
//! Known gaps (see PORT-MANIFEST.md):
//! - Passkeys (`ui-passkeys.js`) are deferred: registration options cannot be
//!   generated without WebAuthn; status/list/revoke mirror the empty-store
//!   shapes, verify routes mirror the expired-challenge shapes.
//! - The remote client auth controller (`remote-clients.js`) is not ported, so
//!   `Authorization: Bearer` client credentials never authenticate here
//!   (JS parity when no controller is injected).
//! - `settings.publicOrigin` is not yet an allowed-origin candidate (settings
//!   runtime not wired into this module).
//! - Tunnel-scope branches (`tunnelLocked` responses) are skipped; the tunnels
//!   module is unported, so every request classifies as local scope.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::RngCore;

use crate::config::ServerConfig;
use crate::context::RouterContext;

const SESSION_COOKIE_NAME: &str = "oc_ui_session";
const SESSION_TTL_MS: u64 = 12 * 60 * 60 * 1000;
const TRUSTED_DEVICE_SESSION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const URL_AUTH_TOKEN_TTL_MS: u64 = 60 * 1000;
const URL_AUTH_TOKEN_PREFIX: &str = "oc_url_";

const RATE_LIMIT_WINDOW_MS: u64 = 5 * 60 * 1000;
const RATE_LIMIT_LOCKOUT_MS: u64 = 15 * 60 * 1000;
const RATE_LIMIT_CLEANUP_MS: u64 = 60 * 60 * 1000;
const RATE_LIMIT_NO_IP_KEY: &str = "rate-limit:no-ip";

/// `express.json()` default body limit.
const JSON_BODY_LIMIT_BYTES: usize = 100 * 1024;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

// ---------------------------------------------------------------------------
// Crypto primitives (no sha2/hmac crates are available to this port)
// ---------------------------------------------------------------------------

/// Manual constant-time equality (no `subtle` crate). The length check leaks
/// length only — identical to JS `crypto.timingSafeEqual`, whose throw on a
/// length mismatch is caught and mapped to `false` by `verifyPassword`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// FIPS 180-4 SHA-256.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }

    let mut digest = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// RFC 2104 HMAC-SHA256.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block_key = [0u8; 64];
    if key.len() > 64 {
        block_key[..32].copy_from_slice(&sha256(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut inner_key = [0x36u8; 64];
    let mut outer_key = [0x5cu8; 64];
    for i in 0..64 {
        inner_key[i] ^= block_key[i];
        outer_key[i] ^= block_key[i];
    }

    let mut inner = Vec::with_capacity(64 + message.len());
    inner.extend_from_slice(&inner_key);
    inner.extend_from_slice(message);
    let inner_digest = sha256(&inner);

    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&outer_key);
    outer.extend_from_slice(&inner_digest);
    sha256(&outer)
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

fn b64url_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn b64url_decode(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()
}

// ---------------------------------------------------------------------------
// Session JWT (HS256 compact JWS, verified only by this server)
// ---------------------------------------------------------------------------

/// Mirrors `new SignJWT({ type: 'ui-session' }).setIssuedAt().setExpirationTime(...)`
/// signed with HS256 — a real JWT on the wire so tokens stay opaque to clients.
fn mint_session_jwt(secret: &[u8], issued_at_secs: u64, ttl_ms: u64) -> String {
    let header = "{\"alg\":\"HS256\",\"typ\":\"JWT\"}";
    let payload = format!(
        "{{\"type\":\"ui-session\",\"iat\":{issued_at_secs},\"exp\":{}}}",
        issued_at_secs + ttl_ms / 1000
    );
    let signing_input = format!(
        "{}.{}",
        b64url_encode(header.as_bytes()),
        b64url_encode(payload.as_bytes())
    );
    let signature = hmac_sha256(secret, signing_input.as_bytes());
    format!("{signing_input}.{}", b64url_encode(&signature))
}

/// Mirrors `jwtVerify(token, jwtSecret)`: signature must verify over the token's
/// own header.payload bytes and `exp` must still be in the future.
fn verify_session_jwt(secret: &[u8], token: &str, at_secs: u64) -> bool {
    let mut parts = token.split('.');
    let (Some(encoded_header), Some(encoded_payload), Some(encoded_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Some(signature) = b64url_decode(encoded_signature) else {
        return false;
    };
    let expected = hmac_sha256(
        secret,
        format!("{encoded_header}.{encoded_payload}").as_bytes(),
    );
    if !constant_time_eq(&signature, &expected) {
        return false;
    }
    let Some(payload) = b64url_decode(encoded_payload) else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&payload) else {
        return false;
    };
    claims
        .get("exp")
        .and_then(|v| v.as_u64())
        .is_some_and(|exp| at_secs < exp)
}

// ---------------------------------------------------------------------------
// Cookies, query, headers
// ---------------------------------------------------------------------------

/// JS `encodeURIComponent`: unreserved characters stay literal, everything
/// else becomes `%XX`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// JS `decodeURIComponent`; falls back to the raw value on an invalid escape
/// (JS callers wrap in try/catch and use the raw string).
fn decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                index += 3;
                continue;
            }
            return value.to_string();
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// JS `parseCookies`: find `name` in the Cookie header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for segment in raw.split(';') {
        let segment = segment.trim_start();
        let (raw_key, rest) = match segment.split_once('=') {
            Some((key, rest)) => (key, rest),
            None => (segment, ""),
        };
        let key = raw_key.trim();
        if key.is_empty() || key != name {
            continue;
        }
        return Some(decode_uri_component(rest.trim()));
    }
    None
}

fn build_cookie(name: &str, value: &str, max_age_secs: u64, secure: bool, at_ms: u64) -> String {
    let expires = if max_age_secs == 0 {
        "Thu, 01 Jan 1970 00:00:00 GMT".to_string()
    } else {
        http_date(at_ms + max_age_secs * 1000)
    };
    let mut cookie = format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}; Expires={expires}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// JS `isSecureRequest`: `req.secure` is impossible on this plain-HTTP server,
/// so only the forwarded proto decides.
fn is_secure_request(headers: &HeaderMap) -> bool {
    forwarded_first(headers, "x-forwarded-proto").eq_ignore_ascii_case("https")
}

fn forwarded_first(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').next().unwrap_or("").trim().to_string())
        .unwrap_or_default()
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"))
}

/// `oc_url_token` from the query string (JS re-parses `req.url` against
/// `http://localhost`, so `+` and `%XX` decode the same way).
fn url_auth_token_from_uri(uri: &Uri) -> Option<String> {
    let path_and_query = uri.path_and_query()?.as_str();
    let parsed = url::Url::parse(&format!("http://localhost{path_and_query}")).ok()?;
    for (key, value) in parsed.query_pairs() {
        if key == "oc_url_token" {
            let token = value.trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// JS `normalizeHost` (bracketed IPv6 → inside the brackets, else cut at the
/// first colon, lowercased).
fn normalize_host(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(inner) = trimmed.strip_prefix('[') {
        return match inner.find(']') {
            Some(end) => inner[..end].to_ascii_lowercase(),
            None => trimmed.to_ascii_lowercase(),
        };
    }
    match trimmed.find(':') {
        Some(index) => trimmed[..index].to_ascii_lowercase(),
        None => trimmed.to_ascii_lowercase(),
    }
}

fn current_rp_id(headers: &HeaderMap) -> String {
    let mut host = forwarded_first(headers, "x-forwarded-host");
    if host.is_empty() {
        host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .unwrap_or("")
            .to_string();
    }
    normalize_host(&host)
}

// ---------------------------------------------------------------------------
// HTTP date (`Date.prototype.toUTCString` shape for Set-Cookie Expires)
// ---------------------------------------------------------------------------

fn http_date(ms: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (year, month, day) = epoch_days_to_ymd(days);
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        weekday,
        day,
        MONTHS[(month - 1) as usize],
        year,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Civil-from-days (Howard Hinnant).
fn epoch_days_to_ymd(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// `oc_url_token` path scopes (ui-auth.js)
// ---------------------------------------------------------------------------

fn is_project_icon_path(path: &str) -> bool {
    // /^\/api\/projects\/[^/]+\/icon$/
    let Some(rest) = path.strip_prefix("/api/projects/") else {
        return false;
    };
    let Some(project) = rest.strip_suffix("/icon") else {
        return false;
    };
    !project.is_empty() && !project.contains('/')
}

/// HTTP GET paths that may authorize with a short-lived `oc_url_token`
/// (`isUrlAuthReadableHttpPath`).
pub fn is_url_auth_readable_http_path(path: &str) -> bool {
    matches!(
        path,
        "/api/event"
            | "/api/global/event"
            | "/api/ompchamber/events"
            | "/api/ompchamber/realtime-proxy/sse"
            | "/api/notifications/stream"
            | "/api/fs/raw"
            | "/api/fs/serve"
    ) || path.starts_with("/api/fs/serve/")
        || path.starts_with("/api/preview/proxy/")
        || is_project_icon_path(path)
}

/// WebSocket upgrade paths that may authorize with a short-lived `oc_url_token`
/// (`isUrlAuthWebSocketPath`).
pub fn is_url_auth_web_socket_path(path: &str) -> bool {
    matches!(
        path,
        "/api/event/ws"
            | "/api/global/event/ws"
            | "/api/ompchamber/realtime-proxy/ws"
            | "/api/terminal/ws"
            | "/api/dictation/ws"
    ) || path.starts_with("/api/preview/proxy/")
}

fn can_use_url_auth_token(method: &Method, path: &str, websocket_upgrade: bool) -> bool {
    if websocket_upgrade {
        return is_url_auth_web_socket_path(path);
    }
    *method == Method::GET && is_url_auth_readable_http_path(path)
}

// ---------------------------------------------------------------------------
// Login rate limiting (ui-auth.js module state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct RateLimitRecord {
    count: u32,
    last_attempt_ms: u64,
    locked_until_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateLimitDecision {
    allowed: bool,
    limit: u32,
    remaining: u32,
    reset_secs: u64,
    retry_after_secs: Option<u64>,
}

fn rate_limit_key(headers: &HeaderMap) -> String {
    // JS getClientIp: first X-Forwarded-For entry with the IPv6-mapped prefix
    // stripped; without a socket address the no-ip bucket applies (max 3).
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let ip = forwarded.split(',').next().unwrap_or("").trim();
        let ip = ip.strip_prefix("::ffff:").unwrap_or(ip);
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    RATE_LIMIT_NO_IP_KEY.to_string()
}

fn env_attempts(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

// ---------------------------------------------------------------------------
// UI auth state (one instance per (data_dir, password) configuration)
// ---------------------------------------------------------------------------

struct PasswordGate {
    salt: [u8; 16],
    expected_mac: [u8; 32],
}

impl PasswordGate {
    fn new(password: &str) -> Self {
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        // JS: scrypt(password, salt) compared with timingSafeEqual. No scrypt
        // implementation is available to this port; a random per-boot salt plus
        // HMAC-SHA256 with constant-time comparison preserves the observable
        // contract (wrong passwords 401, correct passwords pass).
        Self {
            salt,
            expected_mac: hmac_sha256(&salt, password.as_bytes()),
        }
    }

    /// JS `verifyPassword` (password NFC-normalization gap noted in the module
    /// docs; trimming is preserved).
    fn verify(&self, candidate: &str) -> bool {
        if candidate.is_empty() {
            return false;
        }
        let normalized = candidate.trim();
        if normalized.is_empty() {
            return false;
        }
        let candidate_mac = hmac_sha256(&self.salt, normalized.as_bytes());
        constant_time_eq(&candidate_mac, &self.expected_mac)
    }
}

struct UrlTokenEntry {
    session_token: String,
    expires_at_ms: u64,
}

struct UiAuthState {
    /// `Some` when a UI password is configured — the enabled/disabled split of
    /// `createUiAuth`.
    gate: Option<PasswordGate>,
    cookie_name: &'static str,
    session_ttl_ms: u64,
    trusted_ttl_ms: u64,
    jwt_secret: Mutex<Vec<u8>>,
    jwt_secret_from_env: bool,
    data_dir: PathBuf,
    url_tokens: Mutex<HashMap<String, UrlTokenEntry>>,
    rate_limiter: Mutex<HashMap<String, RateLimitRecord>>,
    rate_limit_max: u32,
    rate_limit_no_ip_max: u32,
}

fn jwt_secret_path(data_dir: &Path) -> PathBuf {
    data_dir.join("jwt-secret")
}

fn persist_secret_file(data_dir: &Path, secret: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let file = jwt_secret_path(data_dir);
    #[cfg(unix)]
    {
        use std::io::Write;
use crate::os_compat::OpenOptionsExt;
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file)?;
        handle.write_all(secret.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&file, secret)?;
    }
    Ok(())
}

/// JS `getOrCreateJwtSecret`.
fn load_or_persist_jwt_secret(data_dir: &Path) -> Vec<u8> {
    let file = jwt_secret_path(data_dir);
    if file.is_file()
        && let Ok(contents) = std::fs::read_to_string(&file)
    {
        return contents.trim().to_string().into_bytes();
    }
    let secret = random_bytes(32)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    match persist_secret_file(data_dir, &secret) {
        Ok(()) => tracing::info!(
            "[JWT] Generated and persisted new secret to {}",
            file.display()
        ),
        Err(err) => tracing::warn!("[JWT] Failed to persist secret: {err}"),
    }
    secret.into_bytes()
}

impl UiAuthState {
    fn new(config: &ServerConfig) -> Self {
        // JS normalizePassword: `.normalize().trim()` — NFC normalization is not
        // available without a unicode crate; trimming is preserved.
        let normalized_password = config
            .ui_password
            .as_deref()
            .map(str::trim)
            .filter(|password| !password.is_empty());
        let gate = normalized_password.map(PasswordGate::new);
        let (jwt_secret, jwt_secret_from_env) = if gate.is_some() {
            // JS truthiness: an empty OPENCODE_JWT_SECRET is treated as unset.
            match std::env::var("OPENCODE_JWT_SECRET")
                .ok()
                .filter(|v| !v.is_empty())
            {
                Some(value) => (value.into_bytes(), true),
                None => (load_or_persist_jwt_secret(&config.data_dir), false),
            }
        } else {
            (Vec::new(), false)
        };
        Self {
            gate,
            cookie_name: SESSION_COOKIE_NAME,
            session_ttl_ms: SESSION_TTL_MS,
            trusted_ttl_ms: TRUSTED_DEVICE_SESSION_TTL_MS,
            jwt_secret: Mutex::new(jwt_secret),
            jwt_secret_from_env,
            data_dir: config.data_dir.clone(),
            url_tokens: Mutex::new(HashMap::new()),
            rate_limiter: Mutex::new(HashMap::new()),
            rate_limit_max: env_attempts("OMPCHAMBER_RATE_LIMIT_MAX_ATTEMPTS").unwrap_or(10),
            rate_limit_no_ip_max: env_attempts("OMPCHAMBER_RATE_LIMIT_NO_IP_MAX_ATTEMPTS")
                .unwrap_or(3),
        }
    }

    fn verify_session_token(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let secret = self
            .jwt_secret
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        verify_session_jwt(&secret, token, now_secs())
    }

    fn issue_session_token(&self, ttl_ms: u64) -> String {
        let secret = self
            .jwt_secret
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        mint_session_jwt(&secret, now_secs(), ttl_ms)
    }

    /// JS `rotateJwtSecret` + `persistJwtSecret`.
    fn rotate_jwt_secret(&self) -> Result<(), String> {
        let secret = random_bytes(32)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        persist_secret_file(&self.data_dir, &secret).map_err(|err| err.to_string())?;
        *self.jwt_secret.lock().unwrap_or_else(|e| e.into_inner()) = secret.into_bytes();
        self.url_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
    }

    /// JS `issueUrlAuthTokenForSession`: sweep expired entries, mint
    /// `oc_url_<24 random bytes base64url>`, TTL 60s.
    fn issue_url_token(&self, session_token: &str) -> (String, u64) {
        let now = now_ms();
        let mut tokens = self.url_tokens.lock().unwrap_or_else(|e| e.into_inner());
        tokens.retain(|_, entry| entry.expires_at_ms > now);
        let token = format!(
            "{URL_AUTH_TOKEN_PREFIX}{}",
            b64url_encode(&random_bytes(24))
        );
        let expires_at = now + URL_AUTH_TOKEN_TTL_MS;
        tokens.insert(
            token.clone(),
            UrlTokenEntry {
                session_token: session_token.to_string(),
                expires_at_ms: expires_at,
            },
        );
        (token, expires_at)
    }

    /// JS `authenticateUrlAuthToken` — path/method-scoped single use lookup.
    fn authenticate_url_token(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        uri: &Uri,
    ) -> Option<String> {
        if !can_use_url_auth_token(method, path, is_websocket_upgrade(headers)) {
            return None;
        }
        let token = url_auth_token_from_uri(uri)?;
        if !token.starts_with(URL_AUTH_TOKEN_PREFIX) {
            return None;
        }
        let mut tokens = self.url_tokens.lock().unwrap_or_else(|e| e.into_inner());
        match tokens.get(&token) {
            Some(entry) if entry.expires_at_ms > now_ms() => {
                Some(if entry.session_token.is_empty() {
                    "url:authenticated".to_string()
                } else {
                    entry.session_token.clone()
                })
            }
            Some(_) => {
                tokens.remove(&token);
                None
            }
            None => None,
        }
    }

    fn max_attempts_for(&self, key: &str) -> u32 {
        if key == RATE_LIMIT_NO_IP_KEY {
            self.rate_limit_no_ip_max
        } else {
            self.rate_limit_max
        }
    }

    /// JS `checkRateLimit`.
    fn check_rate_limit(&self, headers: &HeaderMap) -> RateLimitDecision {
        let key = rate_limit_key(headers);
        let now = now_ms();
        let limit = self.max_attempts_for(&key);
        let mut records = self.rate_limiter.lock().unwrap_or_else(|e| e.into_inner());

        // The JS hourly cleanup timer is memory hygiene only; opportunistically
        // purge stale records here to the same effect.
        records.retain(|_, record| {
            let expired_lock = record.locked_until_ms.is_some_and(|until| now >= until);
            let stale = now.saturating_sub(record.last_attempt_ms) > RATE_LIMIT_CLEANUP_MS;
            !expired_lock && !stale
        });

        let record = records.get(&key).copied();
        if let Some(record) = record {
            if let Some(locked_until) = record.locked_until_ms
                && now < locked_until
            {
                return RateLimitDecision {
                    allowed: false,
                    limit,
                    remaining: 0,
                    reset_secs: ceil_div(locked_until, 1000),
                    retry_after_secs: Some(ceil_div(locked_until - now, 1000)),
                };
            }
            if record.last_attempt_ms + RATE_LIMIT_WINDOW_MS >= now && record.count >= limit {
                let locked_until = now + RATE_LIMIT_LOCKOUT_MS;
                records.insert(
                    key.clone(),
                    RateLimitRecord {
                        count: record.count + 1,
                        last_attempt_ms: now,
                        locked_until_ms: Some(locked_until),
                    },
                );
                return RateLimitDecision {
                    allowed: false,
                    limit,
                    remaining: 0,
                    reset_secs: ceil_div(locked_until, 1000),
                    retry_after_secs: Some(ceil_div(RATE_LIMIT_LOCKOUT_MS, 1000)),
                };
            }
            if now.saturating_sub(record.last_attempt_ms) > RATE_LIMIT_WINDOW_MS {
                return RateLimitDecision {
                    allowed: true,
                    limit,
                    remaining: limit,
                    reset_secs: ceil_div(now + RATE_LIMIT_WINDOW_MS, 1000),
                    retry_after_secs: None,
                };
            }
            return RateLimitDecision {
                allowed: true,
                limit,
                remaining: limit - record.count,
                reset_secs: ceil_div(record.last_attempt_ms + RATE_LIMIT_WINDOW_MS, 1000),
                retry_after_secs: None,
            };
        }
        RateLimitDecision {
            allowed: true,
            limit,
            remaining: limit,
            reset_secs: ceil_div(now + RATE_LIMIT_WINDOW_MS, 1000),
            retry_after_secs: None,
        }
    }

    /// JS `recordFailedAttempt`.
    fn record_failed_attempt(&self, headers: &HeaderMap) {
        let key = rate_limit_key(headers);
        let now = now_ms();
        let mut records = self.rate_limiter.lock().unwrap_or_else(|e| e.into_inner());
        let next = match records.get(&key).copied() {
            Some(record) if now.saturating_sub(record.last_attempt_ms) <= RATE_LIMIT_WINDOW_MS => {
                RateLimitRecord {
                    count: record.count + 1,
                    last_attempt_ms: now,
                    locked_until_ms: record.locked_until_ms,
                }
            }
            _ => RateLimitRecord {
                count: 1,
                last_attempt_ms: now,
                locked_until_ms: None,
            },
        };
        records.insert(key, next);
    }

    /// JS `clearRateLimit`.
    fn clear_rate_limit(&self, headers: &HeaderMap) {
        let key = rate_limit_key(headers);
        self.rate_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return value;
    }
    value.div_ceil(divisor)
}

/// JS `loginRateLimiter` and `jwtSecret` live for the server process; the Rust
/// port caches one state per (data_dir, ui_password) configuration so sessions
/// survive across requests while isolated tests get isolated states.
fn shared_state(ctx: &RouterContext) -> Arc<UiAuthState> {
    static CACHE: LazyLock<Mutex<HashMap<(PathBuf, Option<String>), Arc<UiAuthState>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let key = (ctx.config.data_dir.clone(), ctx.config.ui_password.clone());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(key)
        .or_insert_with(|| Arc::new(UiAuthState::new(&ctx.config)))
        .clone()
}

// ---------------------------------------------------------------------------
// Authorization core
// ---------------------------------------------------------------------------

fn build_session_cookie(
    headers: &HeaderMap,
    cookie_name: &str,
    token: &str,
    ttl_ms: u64,
) -> String {
    build_cookie(
        cookie_name,
        &encode_uri_component(token),
        ttl_ms / 1000,
        is_secure_request(headers),
        now_ms(),
    )
}

fn build_clear_cookie(headers: &HeaderMap, cookie_name: &str) -> String {
    build_cookie(cookie_name, "", 0, is_secure_request(headers), now_ms())
}

fn set_cookie(response: &mut Response, value: String) {
    if let Ok(header_value) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(header::SET_COOKIE, header_value);
    }
}

/// JS `respondUnauthorized`: JSON for API paths / JSON-accepting clients,
/// plain text otherwise. Callers add the clearing Set-Cookie (JS
/// `clearSessionCookie` runs before it in both middlewares).
fn unauthorized_response(headers: &HeaderMap, path: &str, cookie_name: &str) -> Response {
    let accepts_json = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json"));
    let mut response = if accepts_json || path.starts_with("/api") {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "UI authentication required", "locked": true })),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Authentication required",
        )
            .into_response()
    };
    set_cookie(&mut response, build_clear_cookie(headers, cookie_name));
    response
}

/// JS `requireAuth` (password mode): OPTIONS passes, a valid session cookie
/// passes, a scoped `oc_url_token` passes; client bearer credentials need the
/// unported remote-client controller and never authenticate. Disabled (no
/// password) is a pass-through because `requireClientAuth` is false in this
/// port.
fn check_require_auth(
    state: &UiAuthState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if state.gate.is_none() {
        return Ok(());
    }
    if *method == Method::OPTIONS {
        return Ok(());
    }
    if let Some(token) = cookie_value(headers, state.cookie_name)
        && state.verify_session_token(&token)
    {
        return Ok(());
    }
    if state
        .authenticate_url_token(method, uri.path(), headers, uri)
        .is_some()
    {
        return Ok(());
    }
    Err(unauthorized_response(
        headers,
        uri.path(),
        state.cookie_name,
    ))
}

/// JS `requireSessionAuth`: session cookie only — never the URL token, never
/// client bearer credentials.
fn require_session_auth(
    state: &UiAuthState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if state.gate.is_none() {
        return Ok(());
    }
    if *method == Method::OPTIONS {
        return Ok(());
    }
    if let Some(token) = cookie_value(headers, state.cookie_name)
        && state.verify_session_token(&token)
    {
        return Ok(());
    }
    Err(unauthorized_response(
        headers,
        uri.path(),
        state.cookie_name,
    ))
}

/// Standalone JS `requireAuth` equivalent for modules that handle `/api`
/// routes themselves (proxy, fs, event-stream, terminal): `Err(response)` is
/// the ready-to-send 401. Consumes only headers — never the body.
pub fn guard(ctx: &RouterContext, parts: &axum::http::request::Parts) -> Result<(), Response> {
    let state = shared_state(ctx);
    check_require_auth(&state, &parts.method, &parts.uri, &parts.headers)
}

// ---------------------------------------------------------------------------
// `/api` gate middleware (JS `app.use('/api', requireApiAuth)`)
// ---------------------------------------------------------------------------

/// Routes registered BEFORE the JS gate that stay public: the server-status
/// GETs from `registerServerStatusRoutes`.
fn is_pre_gate_public(method: &Method, path: &str) -> bool {
    *method == Method::GET
        && matches!(
            path,
            "/api/version" | "/api/system/info" | "/api/system/free-port"
        )
}

fn is_api_mount_path(path: &str) -> bool {
    // Express mounts `app.use('/api', ...)` on the path segment, so `/api` and
    // `/api/...` match but `/apifoo` does not.
    path == "/api" || path.starts_with("/api/")
}

/// Cloneable [`tower::Layer`] applying [`guard`] semantics to `/api`-mounted
/// requests only. Apply with `.layer(ui_auth::middleware(ctx.clone()))` on a
/// module router (covers that router's routes — axum layers never see sibling
/// routers, which is what keeps pre-gate sibling routes exempt, mirroring the
/// JS registration order).
#[derive(Clone)]
pub struct GateLayer {
    ctx: RouterContext,
}

/// The exported gate layer (see [`GateLayer`]).
pub fn middleware(ctx: RouterContext) -> GateLayer {
    GateLayer { ctx }
}

impl<S> tower::Layer<S> for GateLayer
where
    S: tower::Service<Request> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Service = GateService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GateService {
            inner,
            ctx: self.ctx.clone(),
        }
    }
}

#[derive(Clone)]
pub struct GateService<S> {
    inner: S,
    ctx: RouterContext,
}

impl<S> tower::Service<Request> for GateService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let path = req.uri().path();
        if is_api_mount_path(path) && !is_pre_gate_public(req.method(), path) {
            let state = shared_state(&self.ctx);
            if let Err(response) =
                check_require_auth(&state, req.method(), req.uri(), req.headers())
            {
                return Box::pin(async move { Ok(response) });
            }
        }
        Box::pin(self.inner.call(req))
    }
}

// ---------------------------------------------------------------------------
// request-security.js helpers
// ---------------------------------------------------------------------------

fn origin_key(url: &url::Url) -> String {
    match url.origin() {
        url::Origin::Opaque(_) => "null".to_string(),
        origin => origin.ascii_serialization(),
    }
}

/// WHATWG `URL.host`: lowercased host plus an explicit non-default port.
fn js_host_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_string()),
    }
}

/// Port of `isRequestOriginAllowed` from `security/request-security.js`.
/// Same-origin requests (Host/X-Forwarded-Host derived), loopback equivalents,
/// and the packaged WebView client origins are allowed; everything else is
/// rejected. `settings.publicOrigin` is not yet considered (gap noted above).
pub fn is_request_origin_allowed(parts: &axum::http::request::Parts) -> bool {
    let origin_header = parts
        .headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    if origin_header.is_empty() {
        return false;
    }
    // Packaged (non-browser) clients whose WebView origin never matches the host.
    if matches!(
        origin_header,
        "ompchamber-ui://app" | "capacitor://localhost" | "https://localhost"
    ) {
        return true;
    }
    let Ok(origin_url) = url::Url::parse(origin_header) else {
        return false;
    };
    let origin = origin_key(&origin_url);
    let origin_host = js_host_of(&origin_url);

    let protocol = {
        let forwarded = forwarded_first(&parts.headers, "x-forwarded-proto").to_ascii_lowercase();
        if forwarded.is_empty() {
            "http".to_string()
        } else {
            forwarded
        }
    };
    let mut host = forwarded_first(&parts.headers, "x-forwarded-host");
    if host.is_empty() {
        host = parts
            .headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .unwrap_or("")
            .to_string();
    }
    if host.is_empty() {
        return false;
    }

    let host_lower = host.to_ascii_lowercase();
    let mut candidate_origins: HashSet<String> = HashSet::new();
    candidate_origins.insert(format!("{protocol}://{host}"));
    // JS `const [hostname, port] = host.split(':')` — first segment + the
    // remainder as the port suffix (quirks included).
    let mut segments = host_lower.splitn(2, ':');
    let hostname = segments.next().unwrap_or("").to_string();
    let port_suffix = segments
        .next()
        .filter(|port| !port.is_empty())
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    if hostname == "localhost" {
        candidate_origins.insert(format!("{protocol}://127.0.0.1{port_suffix}"));
        candidate_origins.insert(format!("{protocol}://[::1]{port_suffix}"));
    } else if hostname == "127.0.0.1" || hostname == "[::1]" {
        candidate_origins.insert(format!("{protocol}://localhost{port_suffix}"));
    }

    if candidate_origins.contains(&origin) {
        return true;
    }
    // TLS commonly ends at a cloud edge before an HTTP hop: compare the
    // external host directly instead of requiring protocol preservation.
    if let Some(origin_host) = origin_host
        && host_lower == origin_host
    {
        return true;
    }
    false
}

/// Port of `rejectWebSocketUpgrade` from `security/request-security.js`: the
/// HTTP error frame written to the upgrading socket (status text per the JS
/// map, `Connection: close`, `text/plain` body). Return it from a handler (or
/// send it before completing an upgrade) to reject with an observable status.
pub fn reject_websocket_upgrade(status: u16, reason: &str) -> Response {
    let trimmed = reason.trim();
    let message = if trimmed.is_empty() {
        "Bad Request"
    } else {
        trimmed
    };
    // The JS map (400/401/403/404/500 → text, else "Bad Request") only feeds
    // the raw status line; hyper renders the canonical reason phrase itself.
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    Response::builder()
        .status(status)
        .header(header::CONNECTION, "close")
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(message.to_string()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

async fn read_json_body(body: Body) -> Result<serde_json::Value, Response> {
    let bytes = axum::body::to_bytes(body, JSON_BODY_LIMIT_BYTES)
        .await
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({ "error": "Payload too large" })),
            )
                .into_response()
        })?;
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Invalid JSON body" })),
        )
            .into_response()
    })
}

fn add_rate_limit_headers(response: &mut Response, decision: &RateLimitDecision) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&decision.limit.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-limit"), value);
    }
    if let Ok(value) = HeaderValue::from_str(&decision.remaining.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-remaining"), value);
    }
    if let Ok(value) = HeaderValue::from_str(&decision.reset_secs.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-reset"), value);
    }
}

/// `GET /auth/session` → `handleSessionStatus`.
async fn session_status(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if state.gate.is_none() {
        // JS disabled stub (requireClientAuth is always false in this port).
        return Json(serde_json::json!({ "authenticated": true, "disabled": true }))
            .into_response();
    }
    // An explicit bearer credential decides on its own; the remote client auth
    // controller is not ported, so a bearer probe is always unauthenticated.
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if authorization.to_ascii_lowercase().starts_with("bearer ") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "authenticated": false, "locked": true })),
        )
            .into_response();
    }
    let token = cookie_value(&parts.headers, state.cookie_name);
    if token
        .as_deref()
        .is_some_and(|token| state.verify_session_token(token))
    {
        return Json(serde_json::json!({ "authenticated": true })).into_response();
    }
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "authenticated": false, "locked": true })),
    )
        .into_response();
    set_cookie(
        &mut response,
        build_clear_cookie(&parts.headers, state.cookie_name),
    );
    response
}

/// `POST /auth/session` → `handleSessionCreate` (login; "logout" is the
/// cookie clear on 401 responses plus `POST /api/auth/reset`).
async fn session_create(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Some(gate) = state.gate.as_ref() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    };

    let decision = state.check_rate_limit(&parts.headers);
    if !decision.allowed {
        let retry_after = decision.retry_after_secs.unwrap_or(0);
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "Too many login attempts, please try again later",
                "retryAfter": retry_after,
            })),
        )
            .into_response();
        add_rate_limit_headers(&mut response, &decision);
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let payload = match read_json_body(body).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let candidate = payload
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !gate.verify(candidate) {
        state.record_failed_attempt(&parts.headers);
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Invalid credentials" })),
        )
            .into_response();
        add_rate_limit_headers(&mut response, &decision);
        set_cookie(
            &mut response,
            build_clear_cookie(&parts.headers, state.cookie_name),
        );
        return response;
    }

    state.clear_rate_limit(&parts.headers);

    // JS: `trustDevice === true` upgrades the TTL to 7 days.
    let trust_device = payload.get("trustDevice") == Some(&serde_json::Value::Bool(true));
    let ttl_ms = if trust_device {
        state.trusted_ttl_ms
    } else {
        state.session_ttl_ms
    };
    let token = state.issue_session_token(ttl_ms);
    let mut response = (
        StatusCode::OK,
        Json(serde_json::json!({ "authenticated": true })),
    )
        .into_response();
    add_rate_limit_headers(&mut response, &decision);
    set_cookie(
        &mut response,
        build_session_cookie(&parts.headers, state.cookie_name, &token, ttl_ms),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // issueClientToken requires the remote client auth controller (unported).
    response
}

/// `POST /auth/url-token` → `handleUrlAuthToken`.
async fn url_token(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();

    let (session_token, minted_cookie) = if state.gate.is_none() {
        // JS disabled stub: reuse the ambient session cookie value verbatim,
        // minting one when absent.
        match cookie_value(&parts.headers, state.cookie_name) {
            Some(value) if !value.is_empty() => (value, None),
            _ => {
                let token = b64url_encode(&random_bytes(32));
                let cookie = build_session_cookie(
                    &parts.headers,
                    state.cookie_name,
                    &token,
                    state.session_ttl_ms,
                );
                (token, Some(cookie))
            }
        }
    } else {
        // Password mode: only the session cookie (client bearer and URL token
        // issuance paths need the unported controller).
        let token = cookie_value(&parts.headers, state.cookie_name);
        if !token
            .as_deref()
            .is_some_and(|token| state.verify_session_token(token))
        {
            return unauthorized_response(&parts.headers, "/auth/url-token", state.cookie_name);
        }
        (token.unwrap_or_default(), None)
    };

    let (token, expires_at) = state.issue_url_token(&session_token);
    let mut response = (
        StatusCode::OK,
        Json(serde_json::json!({ "token": token, "expiresAt": expires_at })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(cookie) = minted_cookie {
        set_cookie(&mut response, cookie);
    }
    response
}

/// `GET /auth/passkey/status` → `handlePasskeyStatus` (passkeys deferred: the
/// empty-store shapes).
async fn passkey_status(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if state.gate.is_none() {
        return Json(serde_json::json!({
            "enabled": false,
            "hasPasskeys": false,
            "passkeyCount": 0,
            "rpID": serde_json::Value::Null,
        }))
        .into_response();
    }
    let rp_id = current_rp_id(&parts.headers);
    Json(serde_json::json!({
        "enabled": true,
        "hasPasskeys": false,
        "passkeyCount": 0,
        "rpID": rp_id,
    }))
    .into_response()
}

/// `POST /auth/passkey/authenticate/options` → `beginAuthentication` against
/// an always-empty store.
async fn passkey_auth_options(State(state): State<Arc<UiAuthState>>, _req: Request) -> Response {
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "No passkeys are registered for this host yet" })),
    )
        .into_response()
}

/// `POST /auth/passkey/authenticate/verify` → `finishAuthentication` against
/// an always-empty store.
async fn passkey_auth_verify(State(state): State<Arc<UiAuthState>>, _req: Request) -> Response {
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "That passkey is not registered for this OMPChamber instance" })),
    )
        .into_response()
}

/// `POST /auth/passkey/register/options` (session-gated). WebAuthn credential
/// generation is not ported — honest 400 instead of options.
async fn passkey_register_options(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "Passkey registration is not available in this server build" })),
    )
        .into_response()
}

/// `POST /auth/passkey/register/verify` (session-gated): the challenge map is
/// always empty, so verification always reports expiry — the exact JS shape.
async fn passkey_register_verify(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "Passkey setup has expired. Please try again." })),
    )
        .into_response()
}

/// `GET /api/passkeys` (session-gated) → always-empty list.
async fn passkey_list(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    Json(serde_json::json!({ "passkeys": [] })).into_response()
}

/// `DELETE /api/passkeys/{id}` (session-gated) → empty-store revoke shapes.
async fn passkey_revoke(
    State(state): State<Arc<UiAuthState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    req: Request,
) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    if id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Passkey ID is required" })),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Passkey not found for this host" })),
    )
        .into_response()
}

/// `POST /api/auth/reset` (session-gated) → global sign-out: rotate the JWT
/// secret (invalidating every session), clear URL tokens, clear the cookie.
async fn reset_auth(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    if state.gate.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "UI password not configured" })),
        )
            .into_response();
    }
    // JS `persistJwtSecret` refuses to rotate an env-provided secret.
    if state.jwt_secret_from_env {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Global sign-out is unavailable while OPENCODE_JWT_SECRET is set" })),
        )
            .into_response();
    }
    if let Err(message) = state.rotate_jwt_secret() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response();
    }
    let mut response = Json(serde_json::json!({
        "cleared": true,
        "clearedPasskeys": 0,
        "signedOutEverywhere": true,
    }))
    .into_response();
    set_cookie(
        &mut response,
        build_clear_cookie(&parts.headers, state.cookie_name),
    );
    response
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(ctx: RouterContext) -> axum::Router {
    let state = shared_state(&ctx);
    let auth_routes = Router::new()
        .route("/auth/session", get(session_status).post(session_create))
        .route("/auth/url-token", post(url_token))
        .route("/auth/passkey/status", get(passkey_status))
        .route(
            "/auth/passkey/authenticate/options",
            post(passkey_auth_options),
        )
        .route(
            "/auth/passkey/authenticate/verify",
            post(passkey_auth_verify),
        )
        .route(
            "/auth/passkey/register/options",
            post(passkey_register_options),
        )
        .route(
            "/auth/passkey/register/verify",
            post(passkey_register_verify),
        );

    // Registered before the JS `/api` gate with their own session-only auth.
    // Layering `middleware` here is behavior-equivalent for these paths (the
    // gate accepts only session cookies for them) and exercises the exported
    // layer in-tree.
    let api_routes = Router::new()
        .route("/api/passkeys", get(passkey_list))
        .route("/api/passkeys/{id}", delete(passkey_revoke))
        .route("/api/auth/reset", post(reset_auth))
        .layer(middleware(ctx));

    auth_routes.merge(api_routes).with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::any;
    use serde_json::Value;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    use crate::config::EngineConfig;
    use crate::engine::EngineState;
    use crate::hub::EventHub;

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_context(password: Option<&str>) -> (RouterContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-uiauth-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = ServerConfig {
            port: 3999,
            host: None,
            lan: false,
            ui_password: password.map(|p| p.to_string()),
            api_only: false,
            data_dir: dir.clone(),
            dist_dir: dir.join("dist"),
            tunnel: Default::default(),
            engine: EngineConfig::External {
                base_url: "http://127.0.0.1:1".to_string(),
            },
        };
        let ctx = RouterContext {
            config: Arc::new(config),
            engine: EngineState::external("http://127.0.0.1:1".to_string(), None),
            hub: EventHub::new(),
        };
        (ctx, dir)
    }

    fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("build request")
    }

    fn json_request(method: Method, uri: &str, body: &Value, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build json request")
    }

    async fn send(router: &axum::Router, req: Request) -> Response {
        router
            .clone()
            .oneshot(req)
            .await
            .expect("infallible oneshot")
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn header_str(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    fn cookie_pair(set_cookie: &str) -> (String, String) {
        let mut parts = set_cookie.splitn(2, ';');
        let pair = parts.next().unwrap_or("");
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name.to_string(), value.to_string())
    }

    // -- primitives ---------------------------------------------------------

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_vectors() {
        // RFC 4231 test case 1
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // RFC 4231 test case 2
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn constant_time_compare_semantics() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sane"));
        assert!(!constant_time_eq(b"length", b"differs"));
        assert!(constant_time_eq(b"", b""));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn http_date_matches_to_utc_string_shape() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(1_000_000_000_000),
            "Sun, 09 Sep 2001 01:46:40 GMT"
        );
        assert_eq!(http_date(43200 * 1000), "Thu, 01 Jan 1970 12:00:00 GMT");
    }

    #[test]
    fn cookie_attributes_mirror_js_buildcookie() {
        assert_eq!(
            build_cookie("oc_ui_session", "tok", 43200, false, 0),
            "oc_ui_session=tok; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200; Expires=Thu, 01 Jan 1970 12:00:00 GMT"
        );
        assert!(build_cookie("oc_ui_session", "tok", 43200, true, 0).ends_with("; Secure"));
        assert_eq!(
            build_cookie("oc_ui_session", "", 0, false, 1_700_000_000_000),
            "oc_ui_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn cookie_parsing_finds_named_cookie() {
        let headers = HeaderMap::new();
        assert!(cookie_value(&headers, "oc_ui_session").is_none());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; oc_ui_session=abc.def; b=2"),
        );
        assert_eq!(
            cookie_value(&headers, "oc_ui_session").as_deref(),
            Some("abc.def")
        );
        // Percent-decoding with a fallback for invalid escapes.
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("oc_ui_session=a%2Bb"),
        );
        assert_eq!(
            cookie_value(&headers, "oc_ui_session").as_deref(),
            Some("a+b")
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("oc_ui_session=bad%zz"),
        );
        assert_eq!(
            cookie_value(&headers, "oc_ui_session").as_deref(),
            Some("bad%zz")
        );
        // '=' inside the value survives (JS `rest.join('=')`).
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("oc_ui_session=a=b=c"),
        );
        assert_eq!(
            cookie_value(&headers, "oc_ui_session").as_deref(),
            Some("a=b=c")
        );
    }

    #[test]
    fn uri_component_roundtrip() {
        assert_eq!(encode_uri_component("abcXYZ-_12"), "abcXYZ-_12");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(decode_uri_component("a%20b%2Fc"), "a b/c");
    }

    #[test]
    fn url_auth_path_predicates_match_js() {
        for path in [
            "/api/event",
            "/api/global/event",
            "/api/ompchamber/events",
            "/api/ompchamber/realtime-proxy/sse",
            "/api/notifications/stream",
            "/api/fs/raw",
            "/api/fs/serve",
            "/api/fs/serve/tmp/index.html",
            "/api/preview/proxy/http://x/y",
            "/api/projects/p1/icon",
        ] {
            assert!(is_url_auth_readable_http_path(path), "readable: {path}");
        }
        for path in [
            "/api/event/ws",
            "/api/global/event/ws",
            "/api/ompchamber/realtime-proxy/ws",
            "/api/terminal/ws",
            "/api/dictation/ws",
            "/api/preview/proxy/ws-upgrade",
        ] {
            assert!(is_url_auth_web_socket_path(path), "ws: {path}");
        }
        assert!(!is_url_auth_readable_http_path("/api/config/settings"));
        assert!(!is_url_auth_readable_http_path("/api/event/ws"));
        assert!(!is_url_auth_readable_http_path("/api/projects//icon"));
        assert!(!is_url_auth_readable_http_path("/api/projects/a/b/icon"));
        assert!(!is_url_auth_web_socket_path("/api/event"));
    }

    #[test]
    fn session_jwt_verifies_tampering_and_expiry() {
        let secret = b"topsecret";
        let token = mint_session_jwt(secret.as_slice(), 1_000, 60_000);
        assert!(token.starts_with("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9."));
        assert!(verify_session_jwt(secret.as_slice(), &token, 1_000));
        assert!(!verify_session_jwt(b"other", &token, 1_000));

        // Tampered payload invalidates the signature.
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[1] = "eyJ0eXBlIjoidWktc2Vzc2lvbiIsImlhdCI6MTAwMCwiZXhwIjoyMDAwfQ";
        let tampered = parts.join(".");
        assert!(!verify_session_jwt(secret.as_slice(), &tampered, 1_000));

        // Expired token (exp <= now).
        let expired = mint_session_jwt(secret.as_slice(), 1_000, 1_000);
        assert!(!verify_session_jwt(secret.as_slice(), &expired, 1_500));

        // Malformed tokens never verify.
        for bad in ["", "a.b", "a.b.c.d", "!!!.!!.!!"] {
            assert!(!verify_session_jwt(secret.as_slice(), bad, 1_000));
        }
    }

    // -- gate middleware ----------------------------------------------------

    #[tokio::test]
    async fn gate_protects_only_api_mount_paths_like_js() {
        let (ctx, dir) = test_context(Some("secret"));
        let inner = Router::new()
            .route("/api/version", any(ok_handler))
            .route("/api/{*rest}", any(ok_handler))
            .route("/{*rest}", any(ok_handler))
            .layer(middleware(ctx.clone()));

        // Non-/api paths are outside the JS mount.
        let response = send(&inner, request(Method::GET, "/health", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Pre-gate public GETs stay public; other methods on them do not
        // (JS: only `app.get` was registered before the gate).
        let response = send(&inner, request(Method::GET, "/api/version", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = send(&inner, request(Method::POST, "/api/version", &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Ungated /api request → JS respondUnauthorized JSON shape.
        let response = send(&inner, request(Method::GET, "/api/session", &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "UI authentication required", "locked": true })
        );

        // OPTIONS passes through the gate (CORS preflight).
        let response = send(&inner, request(Method::OPTIONS, "/api/session", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);

        // `/apifoo` is not `/api`-mounted.
        let response = send(&inner, request(Method::GET, "/apifoo", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);

        // The gate's 401 clears the session cookie.
        let response = send(&inner, request(Method::GET, "/api/session", &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let set_cookie = header_str(response.headers(), "set-cookie");
        assert!(
            set_cookie.starts_with("oc_ui_session=; Path=/"),
            "cleared: {set_cookie}"
        );
        assert!(set_cookie.contains("Max-Age=0"));

        // Without a password the gate is a pass-through.
        let (open_ctx, open_dir) = test_context(None);
        let open = Router::new()
            .route("/api/{*rest}", any(ok_handler))
            .layer(middleware(open_ctx));
        let response = send(&open, request(Method::GET, "/api/session", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(open_dir);
    }

    async fn ok_handler() -> Response {
        (StatusCode::OK, "ok").into_response()
    }

    #[tokio::test]
    async fn wrong_password_is_rejected_with_cleared_cookie() {
        let (ctx, dir) = test_context(Some("  hunter2  ")); // trailing/leading spaces trim
        let app = router(ctx);

        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "wrong" }),
                &[("x-forwarded-for", "198.51.100.7")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let set_cookie = header_str(response.headers(), "set-cookie");
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Invalid credentials" })
        );
        assert!(set_cookie.starts_with("oc_ui_session=;"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn session_cookie_roundtrip_unlocks_status_and_gate() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx.clone());
        let gated = Router::new()
            .route("/api/{*rest}", any(ok_handler))
            .layer(middleware(ctx.clone()));

        // Login: trims the configured password, sets the session cookie.
        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &[],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cache_control = header_str(response.headers(), "cache-control");
        let set_cookie = header_str(response.headers(), "set-cookie");
        let has_rate_limit_header = header_str(response.headers(), "x-ratelimit-limit").len() > 0;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": true })
        );
        assert_eq!(cache_control, "no-store");
        let (name, token) = cookie_pair(&set_cookie);
        assert_eq!(name, "oc_ui_session");
        assert!(token.starts_with("eyJ"), "JWT cookie value: {token}");
        assert!(set_cookie.contains("Path=/; HttpOnly; SameSite=Strict"));
        assert!(
            set_cookie.contains("Max-Age=43200"),
            "12h TTL in {set_cookie}"
        );
        assert!(set_cookie.contains("Expires="));
        assert!(has_rate_limit_header);

        // Status with the cookie → authenticated; without → locked.
        let response = send(
            &app,
            request(
                Method::GET,
                "/auth/session",
                &[("cookie", &format!("oc_ui_session={token}"))],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": true })
        );

        let response = send(&app, request(Method::GET, "/auth/session", &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": false, "locked": true })
        );

        // The minted cookie passes the exported gate.
        let response = send(
            &gated,
            request(
                Method::GET,
                "/api/session",
                &[("cookie", &format!("oc_ui_session={token}"))],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Bearer probes are always unauthenticated without the client controller.
        let response = send(
            &app,
            request(
                Method::GET,
                "/auth/session",
                &[("authorization", "Bearer oc_client_x")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": false, "locked": true })
        );

        // trustedDevice upgrades to the 7-day TTL.
        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2", "trustDevice": true }),
                &[("x-forwarded-for", "198.51.100.8")],
            ),
        )
        .await;
        assert!(header_str(response.headers(), "set-cookie").contains("Max-Age=604800"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn login_rate_limit_locks_out_after_max_attempts() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx);
        let headers = [("x-forwarded-for", "203.0.113.9")];

        for attempt in 1..=10 {
            let response = send(
                &app,
                json_request(
                    Method::POST,
                    "/auth/session",
                    &serde_json::json!({ "password": "wrong" }),
                    &headers,
                ),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "attempt {attempt}"
            );
        }

        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "wrong" }),
                &headers,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = header_str(response.headers(), "retry-after");
        let remaining = header_str(response.headers(), "x-ratelimit-remaining");
        assert_eq!(
            body_json(response).await,
            serde_json::json!({
                "error": "Too many login attempts, please try again later",
                "retryAfter": 900,
            })
        );
        assert_eq!(retry_after, "900");
        assert_eq!(remaining, "0");

        // Even the correct password is locked out.
        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &headers,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn url_token_mint_scope_and_expiry() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let state = shared_state(&ctx);
        let app = router(ctx.clone());
        let gated = Router::new()
            .route("/api/{*rest}", any(ok_handler))
            .layer(middleware(ctx.clone()));

        let login = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &[],
            ),
        )
        .await;
        let cookie = format!(
            "oc_ui_session={}",
            cookie_pair(&header_str(login.headers(), "set-cookie")).1
        );

        // Mint via the session cookie.
        let response = send(
            &app,
            request(
                Method::POST,
                "/auth/url-token",
                &[("cookie", &cookie), ("accept", "application/json")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_str(response.headers(), "cache-control"), "no-store");
        let payload = body_json(response).await;
        let token = payload["token"].as_str().expect("token").to_string();
        assert!(token.starts_with("oc_url_"));
        assert!(token.len() > "oc_url_".len() + 20);
        let expires_at = payload["expiresAt"].as_u64().expect("expiresAt");
        assert!(expires_at > now_ms());

        // The token authorizes readable GET paths...
        let uri = format!("/api/fs/raw?path=/tmp/x.png&oc_url_token={token}");
        let response = send(&gated, request(Method::GET, &uri, &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        let uri = format!("/api/fs/serve/Users/proj/index.html?oc_url_token={token}");
        let response = send(&gated, request(Method::GET, &uri, &[])).await;
        assert_eq!(response.status(), StatusCode::OK);

        // ...but not writes, arbitrary paths, or unknown tokens.
        let uri = format!("/api/fs/raw?oc_url_token={token}");
        let response = send(&gated, request(Method::POST, &uri, &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let uri = format!("/api/config/settings?oc_url_token={token}");
        let response = send(&gated, request(Method::GET, &uri, &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let uri = "/api/fs/raw?oc_url_token=oc_url_forged".to_string();
        let response = send(&gated, request(Method::GET, &uri, &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // WebSocket upgrade paths accept the token only on upgrade.
        let parts = request(
            Method::GET,
            &format!("/api/terminal/ws?oc_url_token={token}"),
            &[("upgrade", "websocket")],
        )
        .into_parts()
        .0;
        assert!(guard(&ctx, &parts).is_ok());
        let parts = request(
            Method::GET,
            &format!("/api/terminal/ws?oc_url_token={token}"),
            &[],
        )
        .into_parts()
        .0;
        assert!(guard(&ctx, &parts).is_err());

        // Expiry: force the entry past its TTL.
        let (token, _) = state.issue_url_token("session-x");
        {
            let mut tokens = state.url_tokens.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = tokens.get_mut(&token) {
                entry.expires_at_ms = now_ms().saturating_sub(1);
            }
        }
        let parts = request(
            Method::GET,
            &format!("/api/fs/raw?oc_url_token={token}"),
            &[],
        )
        .into_parts()
        .0;
        assert!(guard(&ctx, &parts).is_err());

        // URL-token minting requires a session (unauthenticated 401 shape).
        let response = send(
            &app,
            request(
                Method::POST,
                "/auth/url-token",
                &[("accept", "application/json")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "UI authentication required", "locked": true })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_password_mode_shapes() {
        let (ctx, dir) = test_context(None);
        let app = router(ctx.clone());

        let response = send(&app, request(Method::GET, "/auth/session", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": true, "disabled": true })
        );

        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "x" }),
                &[],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "UI password not configured" })
        );

        // URL tokens mint against an implicit session cookie.
        let first = send(&app, request(Method::POST, "/auth/url-token", &[])).await;
        assert_eq!(first.status(), StatusCode::OK);
        let first_cookie = header_str(first.headers(), "set-cookie");
        let payload = body_json(first).await;
        assert!(first_cookie.starts_with("oc_ui_session="));
        assert!(
            payload["token"]
                .as_str()
                .is_some_and(|t| t.starts_with("oc_url_"))
        );

        // With the ambient cookie present no new Set-Cookie is issued.
        let cookie_value = cookie_pair(&first_cookie).1;
        let second = send(
            &app,
            request(
                Method::POST,
                "/auth/url-token",
                &[("cookie", &format!("oc_ui_session={cookie_value}"))],
            ),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(header_str(second.headers(), "set-cookie"), "");

        // Session-only routes pass through (requireClientAuth is false).
        let response = send(&app, request(Method::GET, "/api/passkeys", &[])).await;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "passkeys": [] })
        );
        let response = send(&app, request(Method::DELETE, "/api/passkeys/xyz", &[])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = send(&app, request(Method::POST, "/api/auth/reset", &[])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = send(&app, request(Method::GET, "/auth/passkey/status", &[])).await;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "enabled": false, "hasPasskeys": false, "passkeyCount": 0, "rpID": null })
        );

        // guard() is a pass-through.
        let parts = request(Method::GET, "/api/session", &[]).into_parts().0;
        assert!(guard(&ctx, &parts).is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reset_rotates_secret_and_invalidates_sessions() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx);
        let secret_file = jwt_secret_path(&dir);
        assert!(secret_file.is_file(), "secret persisted at creation");
        let secret_before = std::fs::read_to_string(&secret_file).expect("read secret");

        let login = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &[],
            ),
        )
        .await;
        let cookie = format!(
            "oc_ui_session={}",
            cookie_pair(&header_str(login.headers(), "set-cookie")).1
        );

        // Unauthenticated reset is rejected with the gate's 401 shape.
        let response = send(&app, request(Method::POST, "/api/auth/reset", &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "UI authentication required", "locked": true })
        );

        let response = send(
            &app,
            request(Method::POST, "/api/auth/reset", &[("cookie", &cookie)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let reset_cookie = header_str(response.headers(), "set-cookie");
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "cleared": true, "clearedPasskeys": 0, "signedOutEverywhere": true })
        );
        assert!(reset_cookie.starts_with("oc_ui_session=;"));

        // The rotated secret invalidates the old session and the on-disk secret.
        let secret_after = std::fs::read_to_string(&secret_file).expect("read secret");
        assert_ne!(secret_before, secret_after);
        assert_eq!(secret_after.trim().len(), 64);
        let response = send(
            &app,
            request(Method::GET, "/auth/session", &[("cookie", &cookie)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn passkey_routes_mirror_empty_store_shapes() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx);

        let response = send(
            &app,
            request(
                Method::GET,
                "/auth/passkey/status",
                &[("host", "localhost:3000")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "enabled": true, "hasPasskeys": false, "passkeyCount": 0, "rpID": "localhost" })
        );

        let response = send(
            &app,
            request(Method::POST, "/auth/passkey/authenticate/options", &[]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "No passkeys are registered for this host yet" })
        );

        let response = send(
            &app,
            request(Method::POST, "/auth/passkey/authenticate/verify", &[]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "That passkey is not registered for this OMPChamber instance" })
        );

        // Register routes are session-gated; the text/plain 401 branch applies
        // for non-API paths without a JSON Accept header.
        let response = send(
            &app,
            request(Method::POST, "/auth/passkey/register/options", &[]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            header_str(response.headers(), "content-type"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(body_text(response).await, "Authentication required");

        let login = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &[],
            ),
        )
        .await;
        let cookie = format!(
            "oc_ui_session={}",
            cookie_pair(&header_str(login.headers(), "set-cookie")).1
        );
        let authed = [("cookie", cookie.as_str())];

        let response = send(
            &app,
            request(Method::POST, "/auth/passkey/register/options", &authed),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Passkey registration is not available in this server build" })
        );

        let response = send(
            &app,
            request(Method::POST, "/auth/passkey/register/verify", &authed),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Passkey setup has expired. Please try again." })
        );

        let response = send(&app, request(Method::GET, "/api/passkeys", &authed)).await;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "passkeys": [] })
        );

        let response = send(&app, request(Method::DELETE, "/api/passkeys/%20", &authed)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Passkey ID is required" })
        );

        let response = send(&app, request(Method::DELETE, "/api/passkeys/abc", &authed)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Passkey not found for this host" })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    fn origin_parts(origin: Option<&str>, headers: &[(&str, &str)]) -> axum::http::request::Parts {
        let mut builder = Request::builder().method(Method::GET).uri("/");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("request").into_parts().0
    }

    #[test]
    fn origin_check_allows_packaged_loopback_and_forwarded_hosts() {
        // Packaged client origins.
        for origin in [
            "ompchamber-ui://app",
            "capacitor://localhost",
            "https://localhost",
        ] {
            let parts = origin_parts(Some(origin), &[("host", "192.168.1.130:1202")]);
            assert!(is_request_origin_allowed(&parts), "packaged {origin}");
        }

        // Same-origin.
        let parts = origin_parts(Some("http://localhost:3000"), &[("host", "localhost:3000")]);
        assert!(is_request_origin_allowed(&parts));

        // Loopback equivalents of the Host header.
        let parts = origin_parts(Some("http://127.0.0.1:3000"), &[("host", "localhost:3000")]);
        assert!(is_request_origin_allowed(&parts));
        let parts = origin_parts(Some("http://localhost:3000"), &[("host", "127.0.0.1:3000")]);
        assert!(is_request_origin_allowed(&parts));

        // External host with TLS terminated before an HTTP proxy hop.
        let parts = origin_parts(
            Some("https://devchamber.example.com"),
            &[
                ("host", "devchamber.example.com"),
                ("x-forwarded-proto", "http"),
            ],
        );
        assert!(is_request_origin_allowed(&parts));

        // Forwarded host is authoritative.
        let parts = origin_parts(
            Some("https://devchamber.example.com"),
            &[
                ("host", "127.0.0.1:3000"),
                ("x-forwarded-host", "devchamber.example.com"),
                ("x-forwarded-proto", "http"),
            ],
        );
        assert!(is_request_origin_allowed(&parts));

        // Unknown / cross-origin (non-loopback) requests are rejected.
        let parts = origin_parts(
            Some("https://evil.example.com"),
            &[("host", "192.168.1.130:1202")],
        );
        assert!(!is_request_origin_allowed(&parts));
        let parts = origin_parts(
            Some("https://evil.example.com"),
            &[
                ("host", "127.0.0.1:3000"),
                ("x-forwarded-host", "devchamber.example.com"),
                ("x-forwarded-proto", "http"),
            ],
        );
        assert!(!is_request_origin_allowed(&parts));
        let parts = origin_parts(
            Some("http://localhost:3000"),
            &[("host", "192.168.1.130:1202")],
        );
        assert!(!is_request_origin_allowed(&parts));

        // Missing or malformed origins never pass.
        let parts = origin_parts(None, &[("host", "localhost:3000")]);
        assert!(!is_request_origin_allowed(&parts));
        let parts = origin_parts(Some("not a url"), &[("host", "localhost:3000")]);
        assert!(!is_request_origin_allowed(&parts));
    }

    #[tokio::test]
    async fn ws_rejection_response_shape() {
        let response = reject_websocket_upgrade(401, "UI authentication required");
        assert_eq!(header_str(response.headers(), "connection"), "close");
        assert_eq!(
            header_str(response.headers(), "content-type"),
            "text/plain; charset=utf-8"
        );

        let response = reject_websocket_upgrade(403, "   ");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_text(response).await, "Bad Request");

        let response = reject_websocket_upgrade(418, "teapot");
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(body_text(response).await, "teapot");
    }

    #[tokio::test]
    async fn malformed_login_body_is_rejected_before_verification() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/auth/session")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{not json"))
            .expect("request");
        let response = send(&app, req).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Invalid JSON body" })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn secure_requests_mark_cookies_secure() {
        let (ctx, dir) = test_context(Some("hunter2"));
        let app = router(ctx);
        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({ "password": "hunter2" }),
                &[
                    ("x-forwarded-for", "198.51.100.99"),
                    ("x-forwarded-proto", "https"),
                ],
            ),
        )
        .await;
        assert!(header_str(response.headers(), "set-cookie").ends_with("; Secure"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
