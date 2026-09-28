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
//!
//! 【中文概要】UI 认证模块：`server/lib/ui-auth/ui-auth.js` 的 Rust 移植，
//! 覆盖密码门禁、HS256 会话 JWT 的签发与校验、登录失败限流、60 秒有效的
//! `oc_url_token` 短时令牌，以及 `security/request-security.js` 的请求
//! Origin 校验与 WebSocket 升级拒绝。JS 版在路由表末尾统一用
//! `app.use('/api', requireApiAuth)` 拦截 `/api` 请求；axum 合并的是
//! 无状态 router，做不到“末尾统一挂层”，因此这里导出 `middleware` layer
//! 与 `guard`/`guard_sync` 函数，由 proxy、fs、event-stream、terminal 等
//! 模块各自套在自己的 `/api` 路由上——同路径兄弟路由结构性优先，等价于
//! JS 的注册顺序；`/api/version` 等 pre-gate 公开 GET 在层内放行。

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

/// 会话 cookie 的名称；与 UI 前端约定的固定值 `oc_ui_session`。
const SESSION_COOKIE_NAME: &str = "oc_ui_session";
/// 普通会话的存活时间：12 小时（毫秒）。
const SESSION_TTL_MS: u64 = 12 * 60 * 60 * 1000;
/// “信任此设备”会话的存活时间：7 天（毫秒）；登录体 `trustDevice === true` 时启用。
const TRUSTED_DEVICE_SESSION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// `oc_url_token` 短时令牌的存活时间：60 秒（毫秒）。
const URL_AUTH_TOKEN_TTL_MS: u64 = 60 * 1000;
/// `oc_url_token` 的字面前缀；签发与识别都以它打头。
const URL_AUTH_TOKEN_PREFIX: &str = "oc_url_";

/// 登录限流的计数窗口：5 分钟（毫秒）。
const RATE_LIMIT_WINDOW_MS: u64 = 5 * 60 * 1000;
/// 触发限流后的锁定时长：15 分钟（毫秒）。
const RATE_LIMIT_LOCKOUT_MS: u64 = 15 * 60 * 1000;
/// 限流记录的清理阈值：1 小时（毫秒）；JS 版由小时级定时器清理，这里在每次检查时顺带清扫。
const RATE_LIMIT_CLEANUP_MS: u64 = 60 * 60 * 1000;
/// 取不到客户端 IP 时使用的共享限流桶 key，配额比正常桶更小。
const RATE_LIMIT_NO_IP_KEY: &str = "rate-limit:no-ip";

/// `express.json()` default body limit.
/// 与 `express.json()` 默认上限对齐：登录请求体超过 100 KiB 即拒绝（413）。
const JSON_BODY_LIMIT_BYTES: usize = 100 * 1024;

/// 当前 Unix 时间戳（毫秒）；时钟早于 epoch 时返回 0 而不是 panic。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前 Unix 时间戳（秒），由 `now_ms` 换算而来。
fn now_secs() -> u64 {
    now_ms() / 1000
}

// ---------------------------------------------------------------------------
// Crypto primitives (no sha2/hmac crates are available to this port)
// ---------------------------------------------------------------------------

/// Manual constant-time equality (no `subtle` crate). The length check leaks
/// length only — identical to JS `crypto.timingSafeEqual`, whose throw on a
/// length mismatch is caught and mapped to `false` by `verifyPassword`.
/// 手写常数时间相等比较（无 `subtle` crate）：长度不同直接 false（只泄露
/// 长度，与 JS `timingSafeEqual` 的可观察行为一致），长度相同时用异或
/// 累积比较，耗时与具体内容无关，防计时侧信道。
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
/// FIPS 180-4 SHA-256 的纯手写实现：0x80 填充 + 长度大端附加 + 64 轮
/// 压缩；本移植没有 sha2 crate，供 HMAC 与密码 MAC 使用。
fn sha256(data: &[u8]) -> [u8; 32] {
    // SHA-256 的 64 个轮常量 K。
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
    // 初始哈希状态（前 8 个素数平方根的小数部分）。
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
/// RFC 2104 HMAC-SHA256：密钥长于 64 字节先压缩成 32 字节摘要，再按
/// ipad(0x36)/opad(0x5c) 垫块做内外两轮 SHA-256。
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

/// 用系统 CSPRNG 生成 `len` 个随机字节（盐、密钥、URL token 的熵源）。
fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// base64url 编码（URL-safe 字母表、无填充），用于 JWT 三段与令牌。
fn b64url_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// base64url 解码；输入非法时返回 `None`，由调用方按“令牌无效”处理。
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
/// 铸造会话 JWT：header 固定 `HS256`/`JWT`，payload 含
/// `type=ui-session`、`iat` 与 `exp`（签发时间 + TTL），签名覆盖
/// `header.payload` 两段。线上是标准 JWT，客户端只把它当不透明 token。
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
/// 校验会话 JWT：必须恰好三段、签名（常数时间比较）与 token 自带的
/// `header.payload` 字节匹配、payload 可解析且 `exp` 严格大于当前时刻
/// `at_secs`；任一环节失败都返回 false，不区分原因。
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
/// 等价 JS `encodeURIComponent`：字母数字与 `-_.!~*'()` 保持字面，其余
/// 字节（含 UTF-8 多字节的每个字节）一律 `%XX` 大写十六进制转义。
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
/// 等价 JS `decodeURIComponent`：逐字节还原 `%XX`；遇到非法转义时整体
/// 返回原始字符串——JS 调用方 try/catch 后也回退原值，行为一致。
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
/// 从 Cookie 头按名取值：按 `;` 分段、trim 键名、`=` 之后的部分整体作为
/// 值（值内再出现 `=` 也保留），最后 percent-decode。
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

/// 组装 Set-Cookie 值：固定 `Path=/; HttpOnly; SameSite=Strict`，
/// `Max-Age`/`Expires` 由参数计算，`secure` 时追加 `; Secure`；
/// `max_age_secs == 0` 时 Expires 固定为 epoch（等价删除 cookie）。
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
/// 判定请求是否按 HTTPS 到达：本服务只监听明文 HTTP（`req.secure` 恒
/// false），因此只看 `X-Forwarded-Proto` 首个值是否（忽略大小写）为
/// `https`——反代终止 TLS 的部署里决定 cookie 是否加 `Secure`。
fn is_secure_request(headers: &HeaderMap) -> bool {
    forwarded_first(headers, "x-forwarded-proto").eq_ignore_ascii_case("https")
}

/// 取指定请求头的第一个值：按逗号切分、去空白；头缺失或非 UTF-8 时
/// 返回空串。
fn forwarded_first(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').next().unwrap_or("").trim().to_string())
        .unwrap_or_default()
}

/// 判定是否为 WebSocket 升级请求：`Upgrade` 头 trim 后忽略大小写等于
/// `websocket`。URL token 只在升级请求上放行 `/ws` 路径，故需此判定。
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"))
}

/// `oc_url_token` from the query string (JS re-parses `req.url` against
/// `http://localhost`, so `+` and `%XX` decode the same way).
/// 从 URI 查询串中提取非空 `oc_url_token`：把 path+query 拼到
/// `http://localhost` 后用 WHATWG URL 解析，使 `+` 与 `%XX` 的解码
/// 语义和 JS 重新解析 `req.url` 的做法一致。
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
/// 归一化 Host 值（JS `normalizeHost`）：方括号 IPv6 取括号内部分，
/// 否则截到第一个 `:` 之前，结果转小写；空输入返回空串。
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

/// 推导当前请求的 WebAuthn rpID：优先 `X-Forwarded-Host`，回退 `Host`，
/// 再经 `normalize_host` 去端口并小写；passkey status 响应原样返回它。
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

/// 把 Unix 毫秒格式化为 HTTP 日期（RFC 1123 / JS `toUTCString` 形状，
/// 如 `Thu, 01 Jan 1970 00:00:00 GMT`），供 Set-Cookie 的 Expires 使用；
/// 星期由 epoch 起算的天数对 7 取余得出（1970-01-01 是周四）。
fn http_date(ms: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01
    // 月份缩写（下标 0 = 一月）。
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
/// civil-from-days（Howard Hinnant）算法：epoch 起算的天数 → (年, 月, 日)，
/// 纯整数运算、无时区；`http_date` 与 ISO 时间戳两条路径共用。
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

/// 判定是否为 `/api/projects/{project}/icon` 形状的项目图标路径：
/// 项目段必须非空且不含 `/`（JS 正则 `^/api/projects/[^/]+/icon$` 的
/// 手工等价实现）。
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
/// 允许 `oc_url_token` 授权的只读 HTTP 路径集合（事件 SSE、文件读取与
/// serve、preview proxy、项目图标），对应 JS `isUrlAuthReadableHttpPath`；
/// 不在表内的路径一律不接受 URL token。
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
/// 允许 `oc_url_token` 授权的 WebSocket 升级路径集合（事件/terminal/
/// dictation 的 `/ws` 与 preview proxy），对应 JS `isUrlAuthWebSocketPath`。
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

/// 判定 (方法, 路径) 是否落在 `oc_url_token` 的授权范围：WebSocket 升级
/// 请求只认 WS 路径表；普通请求必须是 GET 且命中只读路径表。
fn can_use_url_auth_token(method: &Method, path: &str, websocket_upgrade: bool) -> bool {
    if websocket_upgrade {
        return is_url_auth_web_socket_path(path);
    }
    *method == Method::GET && is_url_auth_readable_http_path(path)
}

// ---------------------------------------------------------------------------
// Login rate limiting (ui-auth.js module state)
// ---------------------------------------------------------------------------

/// 单个限流 key（通常是客户端 IP）的失败计数与锁定状态。
#[derive(Debug, Clone, Copy)]
struct RateLimitRecord {
    /// 窗口内的累计失败次数。
    count: u32,
    /// 最近一次失败的毫秒时间戳，用于窗口滚动判断。
    last_attempt_ms: u64,
    /// 锁定截止毫秒时间戳；`Some` 表示处于锁定期，期间一律拒绝。
    locked_until_ms: Option<u64>,
}

/// 一次限流判定的结论，同时驱动响应体与 `X-RateLimit-*`/`Retry-After` 头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateLimitDecision {
    /// 是否放行本次尝试。
    allowed: bool,
    /// 当前 key 的最大尝试次数（有 IP 与无 IP 两档）。
    limit: u32,
    /// 剩余可用次数；锁定期间为 0。
    remaining: u32,
    /// 窗口（或锁定）到期的 Unix 时间戳（秒）。
    reset_secs: u64,
    /// 锁定剩余秒数；仅拒绝时给出，写入 `Retry-After`。
    retry_after_secs: Option<u64>,
}

/// 计算限流 key：取 `X-Forwarded-For` 首项并剥离 IPv6 映射前缀
/// `::ffff:`；完全拿不到 IP 时落入共享的 `rate-limit:no-ip` 桶（上限更
/// 低，防止匿名流量打满全局配额）。
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

/// 读取正整数环境变量（两个限流上限）；未设置、解析失败或值不大于 0
/// 都视为未配置（`None`），由调用方使用默认值。
fn env_attempts(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

// ---------------------------------------------------------------------------
// UI auth state (one instance per (data_dir, password) configuration)
// ---------------------------------------------------------------------------

/// 密码门禁凭证：随机盐 + 密码的 HMAC-SHA256 期望值。盐每次启动重新
/// 生成，期望值只在内存中比较，密码原文与盐都不落盘。
struct PasswordGate {
    /// 16 字节随机盐（每次进程启动重新生成）。
    salt: [u8; 16],
    /// HMAC-SHA256(盐, 密码) 的期望值，校验时做常数时间比较。
    expected_mac: [u8; 32],
}

/// 密码凭证的构造与校验。
impl PasswordGate {
    /// 用密码构造门禁：生成随机盐并对其计算 HMAC（trim 由上层完成）。JS
    /// 原版是 scrypt；本移植没有 scrypt 实现，以随机盐 + HMAC + 常数时间
    /// 比较保持可观察契约（错密码 401、对密码通过）。
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
    /// 校验候选密码（JS `verifyPassword`）：空串或 trim 后为空直接拒绝；
    /// 否则对 trim 后的候选计算 HMAC 并与期望值做常数时间比较。与 JS 的
    /// 差异：未做 NFC 归一化（无 unicode crate，模块文档已记为缺口）。
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

/// 一张已签发的 `oc_url_token`：记录绑定的会话 token 与过期时间。
struct UrlTokenEntry {
    /// 签发时会话 cookie 中的 JWT；空串表示匿名（未启用密码）会话。
    session_token: String,
    /// 过期毫秒时间戳；过期后首次触碰即被清除。
    expires_at_ms: u64,
}

/// 一套 (data_dir, 密码) 配置对应的完整 UI 认证状态：密码门禁、JWT
/// 密钥、URL token 表、登录限流表。同一配置在整个进程内共享同一实例
/// （见 `shared_state`），会话因此能跨请求存活。
struct UiAuthState {
    /// `Some` when a UI password is configured — the enabled/disabled split of
    /// `createUiAuth`.
    /// 配置了 UI 密码时为 `Some`；`None` 即门禁关闭，所有 gate 直通。
    gate: Option<PasswordGate>,
    /// 会话 cookie 名，固定为 `oc_ui_session`。
    cookie_name: &'static str,
    /// 普通会话 TTL（毫秒）。
    session_ttl_ms: u64,
    /// 信任设备会话 TTL（毫秒）。
    trusted_ttl_ms: u64,
    /// JWT 签名密钥；全局登出时整体轮换，旧会话随即全部失效。
    jwt_secret: Mutex<Vec<u8>>,
    /// 密钥是否取自 `OPENCODE_JWT_SECRET`（环境变量密钥不允许轮换）。
    jwt_secret_from_env: bool,
    /// 数据目录，JWT 密钥文件持久化于此。
    data_dir: PathBuf,
    /// 已签发的 `oc_url_token` 表（token → 条目）。
    url_tokens: Mutex<HashMap<String, UrlTokenEntry>>,
    /// 登录限流表（key → 失败记录）。
    rate_limiter: Mutex<HashMap<String, RateLimitRecord>>,
    /// 有 IP 来源的每窗口最大失败次数（默认 10，可环境变量覆盖）。
    rate_limit_max: u32,
    /// 无 IP 共享桶的每窗口最大失败次数（默认 3，可环境变量覆盖）。
    rate_limit_no_ip_max: u32,
}

/// JWT 密钥文件路径：`<data_dir>/jwt-secret`。
fn jwt_secret_path(data_dir: &Path) -> PathBuf {
    data_dir.join("jwt-secret")
}

/// 把密钥字符串写入数据目录：Unix 下以 0o600 权限新建/截断写入（经
/// os_compat 的 OpenOptionsExt），保证密钥仅属主可读；非 Unix 直接覆写。
/// 失败以 `io::Error` 上抛。
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
/// 读取已有密钥文件（trim 后原样使用）；文件不存在则生成 32 字节随机
/// 数的 hex 字符串（64 字符）并尽力持久化——持久化失败只记 warn 日志，
/// 不阻断启动。
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

/// 会话与 URL token 的签发/校验、JWT 密钥轮换、登录限流。
impl UiAuthState {
    /// 从服务器配置构造状态：密码 trim 后非空才启用门禁；启用时优先使用
    /// `OPENCODE_JWT_SECRET`（空串视为未设置），否则从数据目录加载/生成
    /// 密钥。两个限流上限可分别由环境变量覆盖（默认 10 / 3）。
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

    /// 校验会话 cookie 中的 JWT：空串直接 false；锁内克隆当前密钥再交给
    /// `verify_session_jwt`，避免校验过程中被并发轮换影响。
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

    /// 以当前时间为 `iat`，按给定 TTL 签发一枚新的会话 JWT。
    fn issue_session_token(&self, ttl_ms: u64) -> String {
        let secret = self
            .jwt_secret
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        mint_session_jwt(&secret, now_secs(), ttl_ms)
    }

    /// JS `rotateJwtSecret` + `persistJwtSecret`.
    /// 全局登出用：生成新密钥、持久化成功后整体替换内存密钥并清空全部
    /// `oc_url_token`；持久化失败以 `Err(消息)` 上抛且不改变现有密钥。
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
    /// 签发 60 秒有效的 `oc_url_` 前缀令牌（24 随机字节的 base64url）：
    /// 先清扫已过期条目（JS 定时清理的等价物），再登记
    /// (token → 会话 token, 过期时间)，返回 (token, 过期毫秒时间戳)。
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
    /// 校验 URL token：先确认 (方法, 路径, 是否 WS 升级) 在授权范围内、
    /// query 中确有 `oc_url_` 前缀 token；命中且未过期即返回其绑定的会话
    /// token（匿名会话返回哨兵值 `url:authenticated`）；已过期的条目当场
    /// 移除，未知 token 直接略过——两者都返回 `None`。
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

    /// 该 key 的最大尝试次数：无 IP 共享桶用小配额，其余用默认配额。
    fn max_attempts_for(&self, key: &str) -> u32 {
        if key == RATE_LIMIT_NO_IP_KEY {
            self.rate_limit_no_ip_max
        } else {
            self.rate_limit_max
        }
    }

    /// JS `checkRateLimit`.
    /// 登录前的限流判定（JS `checkRateLimit`）：先顺带清扫过期记录，然后
    /// 依记录状态返回——锁定中（拒绝 + 剩余锁定秒数）、窗口内已达上限
    /// （本次升级为 15 分钟锁定）、窗口已滚过（重置满额放行）、窗口内未达
    /// 上限（放行并报剩余次数）；无记录按满额放行。
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
    /// 记录一次登录失败：窗口内累加计数并保留既有锁定状态，窗口已滚过则
    /// 从 1 重新计数；实际升级为锁定由下一次 `check_rate_limit` 完成。
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
    /// 登录成功后清除该 key 的全部失败记录（JS `clearRateLimit`）。
    fn clear_rate_limit(&self, headers: &HeaderMap) {
        let key = rate_limit_key(headers);
        self.rate_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

/// 向上取整除法；除数为 0 时返回被除数本身（调用方均为常量非 0 除数，
/// 纯防御）。
fn ceil_div(value: u64, divisor: u64) -> u64 {
    if divisor == 0 {
        return value;
    }
    value.div_ceil(divisor)
}

/// JS `loginRateLimiter` and `jwtSecret` live for the server process; the Rust
/// port caches one state per (data_dir, ui_password) configuration so sessions
/// survive across requests while isolated tests get isolated states.
/// 进程级状态缓存（JS 里 `loginRateLimiter` 与 `jwtSecret` 随进程存活）：
/// 以 (data_dir, 密码) 为 key 复用 `UiAuthState`，让同一配置的会话与
/// 密钥跨请求存活，隔离测试则拿到互相独立的状态。
fn shared_state(ctx: &RouterContext) -> Arc<UiAuthState> {
    // 状态缓存本体；LazyLock 保证进程内首次调用时初始化一次。
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

/// 组装写入会话 JWT 的 Set-Cookie：值经 `encodeURIComponent` 编码，
/// TTL 换算为 Max-Age 秒，是否加 `Secure` 取决于请求是否经 HTTPS。
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

/// 组装清除会话 cookie 的 Set-Cookie（空值、Max-Age=0、Expires=epoch）。
fn build_clear_cookie(headers: &HeaderMap, cookie_name: &str) -> String {
    build_cookie(cookie_name, "", 0, is_secure_request(headers), now_ms())
}

/// 把 Set-Cookie 值写入响应头（insert 覆盖已有值）；值含非法字符时
/// 静默跳过——cookie 值都由本模块生成，正常不会出现。
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
/// 构造 401 响应（JS `respondUnauthorized` + 清 cookie 的合并）：`/api`
/// 路径或 Accept 带 JSON 的客户端得到 JSON 错误体，其余得到纯文本；
/// 两种形态都附带 Max-Age=0 的 Set-Cookie，顺手清掉失效会话。
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
/// 当前时间加 `ms` 毫秒后的 ISO 8601 UTC 时间戳
/// （`YYYY-MM-DDTHH:MM:SS.sssZ`），与 JS `new Date(...).toISOString()`
/// 输出一致；用于登录时铸造远端客户端 token 的 `expiresAt` 字段。
fn iso_now_plus_ms(ms: u64) -> String {
    // JS: new Date(Date.now() + ttlMs).toISOString() — same civil-from-days
    // algorithm as engine_env::managed_process_registry.
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 + ms)
        .unwrap_or(ms);
    let secs = (millis / 1000) as i64;
    let frac = (millis % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{frac:03}Z")
}

/// JS `getBearerTokenFromRequest` + the transport pick from
/// `remote-clients.js` (`x-ompchamber-relay-connection` truthiness).
/// 提取 `Authorization` 请求头中的 bearer 凭证（`Bearer `/`bearer ` 两种
/// 前缀都接受），并按 `x-ompchamber-relay-connection` 头是否非空判定
/// 传输方式（Relay 或 Direct）；没有凭证或 token 为空返回 `None`。
fn bearer_credential(
    headers: &HeaderMap,
) -> Option<(String, crate::client_auth::remote_clients::Transport)> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?
        .trim();
    let token = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let transport = if headers
        .get("x-ompchamber-relay-connection")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty())
    {
        crate::client_auth::remote_clients::Transport::Relay
    } else {
        crate::client_auth::remote_clients::Transport::Direct
    };
    Some((token.to_string(), transport))
}

/// JS `authenticateClientRequest` bearer leg: a valid desktop/remote client
/// token authenticates the gate when the session cookie is absent (the
/// packaged desktop UI loads from `ompchamber-ui://app`, a cross-site origin
/// whose fetches never carry the SameSite=Strict session cookie).
/// 客户端 bearer 认证腿（JS `authenticateClientRequest`）：把 token 交给
/// 远端客户端控制器验证，通过即视为已认证——打包桌面端从
/// `ompchamber-ui://app` 这个跨站 origin 加载，带不上 SameSite=Strict
/// 的会话 cookie，全靠这条通路过 gate。无凭证或验证失败返回 false。
async fn bearer_authenticates(state: &UiAuthState, headers: &HeaderMap) -> bool {
    let Some((token, transport)) = bearer_credential(headers) else {
        return false;
    };
    let clients =
        crate::client_auth::state_for_data_dir(&state.data_dir).remote_clients.clone();
    clients
        .authenticate_bearer_token(&token, transport)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// JS `requireAuth`（密码模式）的核心判定，按序放行：未启用密码（直通）、
/// OPTIONS（CORS 预检）、有效会话 cookie、授权范围内的 `oc_url_token`、
/// 有效客户端 bearer 凭证；全部失败时返回组装好的 401（含清 cookie）。
async fn check_require_auth(
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
    // JS `requireAuth` client-auth fallback (`ui-auth.js:731`): bearer token
    // from the client-auth controller. Without this the packaged desktop UI
    // is locked out of every gated route after unlock.
    if bearer_authenticates(state, headers).await {
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
/// 更严格的“仅会话 cookie”判定（JS `requireSessionAuth`）：只认 OPTIONS
/// 与有效会话 cookie；URL token 与客户端 bearer 凭证一概不算。用于
/// passkey 管理、全局登出等管理面路由。
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
/// 供自行处理 `/api` 路由的模块（proxy、fs、event-stream、terminal）调用
/// 的独立版 `requireAuth`：`Ok(())` 放行，`Err(response)` 是可直接返回
/// 的 401。只读请求头，绝不消费 body。
pub async fn guard(ctx: &RouterContext, parts: &axum::http::request::Parts) -> Result<(), Response> {
    let state = shared_state(ctx);
    check_require_auth(&state, &parts.method, &parts.uri, &parts.headers).await
}

/// Synchronous subset (session cookie + URL token only) for call sites that
/// cannot await — the dev-tunnel auth closure. The dev-tunnel WebSocket
/// handshake authenticates with the session cookie or URL token, never a
/// client bearer credential, so this keeps JS parity for that surface.
/// `guard` 的同步子集（只认会话 cookie 与 URL token）：给无法 await 的
/// 调用点（dev-tunnel 的 WebSocket 握手闭包）使用；该握手从不接受
/// bearer 凭证，因此省略异步的客户端认证腿仍与 JS 行为一致。
pub fn guard_sync(ctx: &RouterContext, parts: &axum::http::request::Parts) -> Result<(), Response> {
    let state = shared_state(ctx);
    if state.gate.is_none() {
        return Ok(());
    }
    if parts.method == Method::OPTIONS {
        return Ok(());
    }
    if let Some(token) = cookie_value(&parts.headers, state.cookie_name)
        && state.verify_session_token(&token)
    {
        return Ok(());
    }
    if state
        .authenticate_url_token(&parts.method, parts.uri.path(), &parts.headers, &parts.uri)
        .is_some()
    {
        return Ok(());
    }
    Err(unauthorized_response(
        &parts.headers,
        parts.uri.path(),
        state.cookie_name,
    ))
}

// ---------------------------------------------------------------------------
// `/api` gate middleware (JS `app.use('/api', requireApiAuth)`)
// ---------------------------------------------------------------------------

/// Routes registered BEFORE the JS gate that stay public: the server-status
/// GETs from `registerServerStatusRoutes`.
/// JS 中注册在 `/api` gate 之前、保持公开的路由：三个 server-status
/// GET（`/api/version`、`/api/system/info`、`/api/system/free-port`）。
fn is_pre_gate_public(method: &Method, path: &str) -> bool {
    *method == Method::GET
        && matches!(
            path,
            "/api/version" | "/api/system/info" | "/api/system/free-port"
        )
}

/// 判定路径是否落在 Express `app.use('/api', ...)` 的挂载范围：
/// `/api` 与 `/api/...` 命中；`/apifoo` 这类同前缀字符串不命中。
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
/// 可克隆的 tower Layer：把 `guard` 的判定套在 `/api` 挂载路径上。用
/// `.layer(ui_auth::middleware(ctx.clone()))` 挂到某个模块 router 上，
/// 只覆盖该 router 的路由——axum 的层看不到兄弟 router，恰好保住了
/// 先注册的公开路由，对应 JS 的注册顺序。
#[derive(Clone)]
pub struct GateLayer {
    /// 网关判定所需的运行上下文（配置/引擎/事件 hub），克隆进每个 Service。
    ctx: RouterContext,
}

/// The exported gate layer (see [`GateLayer`]).
/// 构造导出的 gate layer（语义见 `GateLayer`）。
pub fn middleware(ctx: RouterContext) -> GateLayer {
    GateLayer { ctx }
}

/// tower Layer 实现：把内层 Service 包成 `GateService`。
impl<S> tower::Layer<S> for GateLayer
where
    S: tower::Service<Request> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    /// 包裹后的 Service 类型。
    type Service = GateService<S>;

    /// 用内层 Service 与上下文的克隆构造 `GateService`。
    fn layer(&self, inner: S) -> Self::Service {
        GateService {
            inner,
            ctx: self.ctx.clone(),
        }
    }
}

/// 实际执行 `/api` 门禁的 tower Service：非 `/api` 挂载路径与 pre-gate
/// 公开 GET 直通内层，其余请求先过 `check_require_auth`，未认证直接以
/// 401 短路，不再触达内层路由。
#[derive(Clone)]
pub struct GateService<S> {
    /// 被包裹的内层 Service（真正的路由处理）。
    inner: S,
    /// 网关判定所需的运行上下文。
    ctx: RouterContext,
}

/// tower `Service<Request>` 实现：就绪检查全透传，`call` 内做门禁分流。
impl<S> tower::Service<Request> for GateService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    /// 响应类型就是 axum `Response`。
    type Response = Response;
    /// 错误类型沿用内层 Service 的。
    type Error = S::Error;
    /// 门禁分支含异步认证，统一装箱为 `BoxFuture`。
    type Future = futures::future::BoxFuture<'static, Result<Response, S::Error>>;

    /// 就绪检查直接透传给内层 Service。
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    /// `/api` 挂载且非 pre-gate 公开路径时：克隆判定所需的 method/URI/
    /// headers 后异步执行 `check_require_auth`，未认证则以 401 短路；
    /// 其余请求原样转发内层 Service。内层先 clone 再进入异步块，避免
    /// 跨越 await 持有 `&mut self` 借用。
    fn call(&mut self, req: Request) -> Self::Future {
        let path = req.uri().path();
        if is_api_mount_path(path) && !is_pre_gate_public(req.method(), path) {
            let mut inner = self.inner.clone();
            let ctx = self.ctx.clone();
            let method = req.method().clone();
            let uri = req.uri().clone();
            let headers = req.headers().clone();
            return Box::pin(async move {
                let state = shared_state(&ctx);
                if let Err(response) =
                    check_require_auth(&state, &method, &uri, &headers).await
                {
                    return Ok(response);
                }
                inner.call(req).await
            });
        }
        Box::pin(self.inner.call(req))
    }
}

// ---------------------------------------------------------------------------
// request-security.js helpers
// ---------------------------------------------------------------------------

/// 提取 URL 的 origin 键：普通 origin 用 ASCII 序列化
/// （`scheme://host[:port]`），opaque origin（未知 scheme 等）归为
/// `null`——与 JS `new URL(...).origin` 的字符串形态对齐。
fn origin_key(url: &url::Url) -> String {
    match url.origin() {
        url::Origin::Opaque(_) => "null".to_string(),
        origin => origin.ascii_serialization(),
    }
}

/// WHATWG `URL.host`: lowercased host plus an explicit non-default port.
/// WHATWG `URL.host` 语义：小写 host，显式非默认端口时带 `:port`；
/// 无 host（如 opaque origin）返回 `None`。
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
/// 请求 Origin 校验（`isRequestOriginAllowed` 移植）：无 Origin 一律
/// 拒绝；打包 WebView 客户端的三个固定 origin 直接放行；其余情况由
/// `X-Forwarded-Proto`/`X-Forwarded-Host`（回退 `Host`）推导候选 origin
/// 集合（含 localhost 与 `127.0.0.1`、`[::1]` 的回环等价互换），额外允许
/// “host 相同、协议不同”的 TLS 终止于代理的场景；其它跨站 origin 拒绝。
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
/// 构造拒绝 WebSocket 升级的 HTTP 错误帧（JS `rejectWebSocketUpgrade`）：
/// 状态码非法时回退 400，正文为 reason（trim 后为空则用 "Bad Request"），
/// 附 `Connection: close` 与 `text/plain`。在完成升级前返回它，客户端
/// 就能观察到明确的错误状态而非静默失败。
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

/// 读取并解析登录等接口的 JSON 请求体（上限 100 KiB）：超限或读取失败
/// 返回 413；空 body 视为 `null`；非法 JSON 返回 400。
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

/// 把限流判定写入响应头：`X-RateLimit-Limit`、`X-RateLimit-Remaining`、
/// `X-RateLimit-Reset` 三个标准头。
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
/// `GET /auth/session`：门禁关闭返回 `{authenticated:true,disabled:true}`；
/// 带 bearer 凭证时单独裁决（成功返回 `scope:client`，失败 401 且不清
/// cookie）；否则验会话 cookie——通过返回 authenticated，未通过 401
/// `{authenticated:false,locked:true}` 并顺带清掉失效 cookie。
async fn session_status(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if state.gate.is_none() {
        // JS disabled stub (requireClientAuth is always false in this port).
        return Json(serde_json::json!({ "authenticated": true, "disabled": true }))
            .into_response();
    }
    // An explicit bearer credential decides on its own: success answers
    // `{"authenticated":true,"scope":"client"}`, failure 401 with no cookie
    // fallback (JS `ui-auth.js:751-766`).
    if bearer_credential(&parts.headers).is_some() {
        if bearer_authenticates(&state, &parts.headers).await {
            return Json(
                serde_json::json!({ "authenticated": true, "scope": "client" }),
            )
                .into_response();
        }
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
/// `POST /auth/session` 登录：未配置密码 400；先过限流（锁定中直接
/// 429 + `Retry-After`）；密码错误记一次失败并返回 401 + 清 cookie；
/// 成功则清限流记录、按 `trustDevice` 选 12 小时/7 天 TTL 签发会话
/// cookie，`issueClientToken` 为真时再铸造一枚远端客户端 bearer token
/// 随响应返回。
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
    // JS `handleSessionCreate` issueClientToken leg (`ui-auth.js:840`): mint a
    // remote-client bearer token alongside the session cookie. The packaged
    // desktop UI (cross-site `ompchamber-ui://app` origin) authenticates every
    // gated request with this token — the SameSite=Strict cookie never
    // crosses origins.
    let client_result = if payload.get("issueClientToken") == Some(&serde_json::Value::Bool(true)) {
        let clients =
            crate::client_auth::state_for_data_dir(&state.data_dir).remote_clients.clone();
        let str_field = |key: &str| {
            payload
                .get(key)
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
        };
        let expires_at = iso_now_plus_ms(ttl_ms);
        let input = crate::client_auth::remote_clients::CreateClientInput {
            fallback_label: str_field("clientLabel"),
            expires_at: Some(expires_at),
            client_kind: str_field("clientKind"),
            dedupe_key: str_field("dedupeKey"),
            auth_method: Some("password".to_string()),
            device_name: str_field("deviceName"),
            device_platform: str_field("devicePlatform"),
            device_model: str_field("deviceModel"),
            app_version: str_field("appVersion"),
            ..Default::default()
        };
        clients.create_client(input).await.ok()
    } else {
        None
    };
    let body = match &client_result {
        Some(created) => serde_json::json!({
            "authenticated": true,
            "clientToken": created.token,
            "client": created.client,
        }),
        None => serde_json::json!({ "authenticated": true }),
    };
    let mut response = (StatusCode::OK, Json(body)).into_response();
    add_rate_limit_headers(&mut response, &decision);
    set_cookie(
        &mut response,
        build_session_cookie(&parts.headers, state.cookie_name, &token, ttl_ms),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `POST /auth/url-token` → `handleUrlAuthToken`.
/// `POST /auth/url-token` 签发短时令牌：门禁关闭时复用既有匿名会话
/// cookie（缺失则现铸一枚并 Set-Cookie）；密码模式下接受有效会话
/// cookie 或客户端 bearer 凭证（桌面端场景），两者皆无则 401。成功
/// 返回 `{token, expiresAt}`（60 秒有效）。
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
        // Password mode: the session cookie, or the client bearer credential
        // (JS `resolveAuthenticatedSessionToken` → `clientSessionToken`) when
        // the cookie is absent — the packaged desktop case.
        let token = cookie_value(&parts.headers, state.cookie_name);
        if token
            .as_deref()
            .is_some_and(|token| state.verify_session_token(token))
        {
            (token.unwrap_or_default(), None)
        } else if let Some((client_token, transport)) = bearer_credential(&parts.headers) {
            let clients =
                crate::client_auth::state_for_data_dir(&state.data_dir).remote_clients.clone();
            match clients.authenticate_bearer_token(&client_token, transport).await {
                Ok(Some(auth)) => (auth.session_token, None),
                _ => {
                    return unauthorized_response(
                        &parts.headers,
                        "/auth/url-token",
                        state.cookie_name,
                    );
                }
            }
        } else {
            return unauthorized_response(&parts.headers, "/auth/url-token", state.cookie_name);
        }
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
/// `GET /auth/passkey/status`：本移植未接 WebAuthn，恒为空库形态——
/// 门禁关闭 `enabled:false`；开启时 `enabled:true, passkeyCount:0`，
/// rpID 取当前请求 host。
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
/// `POST /auth/passkey/authenticate/options`：空库下永远返回 404
/// “该主机尚无已注册 passkey”（门禁关闭时 400），与 JS
/// `beginAuthentication` 的空库分支一致。
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
/// `POST /auth/passkey/authenticate/verify`：空库下任何凭证都返回 404
/// “该 passkey 未注册”（门禁关闭时 400）。
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
/// `POST /auth/passkey/register/options`（需会话）：本构建未移植
/// WebAuthn 凭证生成，如实返回 400 “此服务器构建不支持 passkey 注册”。
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
/// `POST /auth/passkey/register/verify`（需会话）：挑战表恒空，验证
/// 永远报“已过期”（400），与 JS 空挑战分支的响应形状一致。
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
/// `GET /api/passkeys`（需会话）：空库恒返回 `{passkeys: []}`。
async fn passkey_list(State(state): State<Arc<UiAuthState>>, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if let Err(response) = require_session_auth(&state, &parts.method, &parts.uri, &parts.headers) {
        return response;
    }
    Json(serde_json::json!({ "passkeys": [] })).into_response()
}

/// `DELETE /api/passkeys/{id}` (session-gated) → empty-store revoke shapes.
/// `DELETE /api/passkeys/{id}`（需会话）：ID trim 后为空返回 400；空库
/// 下任何 ID 都是 404 “未找到该主机的 passkey”。
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
/// `POST /api/auth/reset` 全局登出（需会话）：轮换 JWT 密钥（磁盘与
/// 内存同步，所有旧会话立即失效）并清空全部 URL token；密钥取自
/// `OPENCODE_JWT_SECRET` 时拒绝轮换（400），持久化失败同样 400。成功
/// 返回 `{cleared:true, signedOutEverywhere:true}` 并清除会话 cookie。
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

/// 组装本模块路由：`/auth/*`（session、url-token、passkey 系列）加上
/// 自带 gate 层的 `/api/passkeys*` 与 `/api/auth/reset`（JS 里它们注册在
/// `/api` gate 之前且只认会话 cookie，这里挂导出层行为等价），以
/// `Arc<UiAuthState>` 作为共享 state。
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

/// ui_auth 行为测试：加密原语向量、cookie/JWT 往返、gate 中间件语义、
/// 登录限流、URL token 作用域与过期、passkey 空库形态、Origin 校验。
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

    /// 临时目录计数器：保证并行测试的 data_dir 互不冲突。
    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 构造隔离的测试上下文：每次调用新建唯一临时目录作为 data_dir，
    /// 密码可选；引擎指向不可达地址（这些测试不触引擎）。返回
    /// (ctx, 目录)，目录由调用方负责清理。
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

    /// 构造空 body 测试请求（方法、URI 与附加头）。
    fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("build request")
    }

    /// 构造 JSON body 测试请求（自动带 `Content-Type: application/json`）。
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

    /// 以 `oneshot` 单发请求（axum Router 按值消费，故先 clone）。
    async fn send(router: &axum::Router, req: Request) -> Response {
        router
            .clone()
            .oneshot(req)
            .await
            .expect("infallible oneshot")
    }

    /// 读取响应体并解析为 JSON（上限 1 MiB）。
    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// 读取响应体为 UTF-8 文本（上限 1 MiB）。
    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// 取响应头的字符串值；缺失或非 UTF-8 返回空串。
    fn header_str(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    /// 从 Set-Cookie 切出 (name, value)：取第一个 `;` 前的 `k=v` 段。
    fn cookie_pair(set_cookie: &str) -> (String, String) {
        let mut parts = set_cookie.splitn(2, ';');
        let pair = parts.next().unwrap_or("");
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name.to_string(), value.to_string())
    }

    // -- primitives ---------------------------------------------------------

    /// SHA-256 实现必须命中公开测试向量（空串、`abc`、448 位消息）。
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

    /// HMAC-SHA256 必须命中 RFC 4231 的测试向量 1 与 2。
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

    /// 常数时间比较：内容相同返回 true，任一字节不同或长度不同返回 false，空串相等。
    #[test]
    fn constant_time_compare_semantics() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sane"));
        assert!(!constant_time_eq(b"length", b"differs"));
        assert!(constant_time_eq(b"", b""));
    }

    /// 字节序列转小写 hex，用于与已知向量比对。
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// http_date 输出必须与 JS toUTCString 形状一致（epoch、正午、十亿秒三点采样）。
    #[test]
    fn http_date_matches_to_utc_string_shape() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(1_000_000_000_000),
            "Sun, 09 Sep 2001 01:46:40 GMT"
        );
        assert_eq!(http_date(43200 * 1000), "Thu, 01 Jan 1970 12:00:00 GMT");
    }

    /// build_cookie 属性串与 JS buildCookie 完全一致；secure 追加 ; Secure，清 cookie 用 epoch Expires。
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

    /// cookie_value 按名取值：支持 percent-decode（非法转义回退原值）且保留值中的等号。
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

    /// encode/decodeURIComponent 的保留字符与转义往返保持一致。
    #[test]
    fn uri_component_roundtrip() {
        assert_eq!(encode_uri_component("abcXYZ-_12"), "abcXYZ-_12");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(decode_uri_component("a%20b%2Fc"), "a b/c");
    }

    /// URL token 的只读 HTTP 路径表与 WS 路径表逐项命中/排除，与 JS 判定一致。
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

    /// 会话 JWT：正确签名且未过期才通过；密钥不符、payload 篡改、过期与畸形 token 一律拒绝。
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

    // -- desktop client bearer auth (packaged `ompchamber-ui://app` origin) --

    /// 搭建“打包桌面端”测试环境：带密码的上下文 + 预先创建的一枚远端
    /// 客户端 token。返回 (router, ctx, 目录, bearer token)。
    async fn desktop_bearer(password: &str) -> (axum::Router, RouterContext, PathBuf, String) {
        let (ctx, dir) = test_context(Some(password));
        let clients = crate::client_auth::state_for_data_dir(&dir).remote_clients.clone();
        let created = clients
            .create_client(crate::client_auth::remote_clients::CreateClientInput {
                fallback_label: Some("OMPChamber Desktop".to_string()),
                ..Default::default()
            })
            .await
            .expect("create client");
        (router(ctx.clone()), ctx, dir, created.token)
    }

    /// gate 在无会话 cookie 时接受有效客户端 bearer token；无效 token 仍被拒。
    #[tokio::test]
    async fn gate_accepts_client_bearer_without_session_cookie() {
        let (app, ctx, dir, token) = desktop_bearer("secret").await;
        let gated = app
            .clone()
            .route("/api/session/status", any(ok_handler))
            .layer(middleware(ctx.clone()));

        // The exact desktop case: no cookie, only the bearer credential.
        let response = send(
            &gated,
            request(
                Method::GET,
                "/api/session/status",
                &[("authorization", &format!("Bearer {token}"))],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // A bad token stays locked out.
        let response = send(
            &gated,
            request(
                Method::GET,
                "/api/session/status",
                &[("authorization", "Bearer oc_client_wrong")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// GET /auth/session 带 bearer 凭证时自行裁决：成功返回 scope=client，失败 401。
    #[tokio::test]
    async fn session_status_bearer_decides_on_its_own() {
        let (app, _ctx, dir, token) = desktop_bearer("secret").await;
        let auth = format!("Bearer {token}");
        let response = send(
            &app,
            request(Method::GET, "/auth/session", &[("authorization", &auth)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "authenticated": true, "scope": "client" })
        );

        let response = send(
            &app,
            request(
                Method::GET,
                "/auth/session",
                &[("authorization", "Bearer oc_client_wrong")],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 登录带 issueClientToken 时随 cookie 铸出 oc_client_ 前缀 bearer，且该 token 能单独过 gate。
    #[tokio::test]
    async fn login_issues_client_token_on_request() {
        let (app, ctx, dir, _token) = desktop_bearer("secret").await;

        // Desktop unlock: issueClientToken mints a usable bearer alongside
        // the session cookie.
        let response = send(
            &app,
            json_request(
                Method::POST,
                "/auth/session",
                &serde_json::json!({
                    "password": "secret",
                    "issueClientToken": true,
                    "clientLabel": "OMPChamber Desktop"
                }),
                &[],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        let minted = payload
            .get("clientToken")
            .and_then(|v| v.as_str())
            .expect("clientToken in login response")
            .to_string();
        assert!(minted.starts_with("oc_client_"));

        // The minted token opens the gate without a cookie.
        let gated = app
            .route("/api/session/status", any(ok_handler))
            .layer(middleware(ctx.clone()));
        let response = send(
            &gated,
            request(
                Method::GET,
                "/api/session/status",
                &[("authorization", &format!("Bearer {minted}"))],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// POST /auth/url-token 接受客户端 bearer 凭证替代会话 cookie 并返回 token。
    #[tokio::test]
    async fn url_token_endpoint_accepts_client_bearer() {
        let (app, _ctx, dir, token) = desktop_bearer("secret").await;
        let auth = format!("Bearer {token}");
        let response = send(
            &app,
            request(Method::POST, "/auth/url-token", &[("authorization", &auth)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert!(payload.get("token").and_then(|v| v.as_str()).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// gate 只拦 /api 挂载路径：pre-gate 公开 GET、OPTIONS、/apifoo 放行；401 为 JS JSON 形状并清 cookie；未设密码全放行。
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

    /// 恒 200 的兜底 handler，挂在被 gate 包裹的测试路由上。
    async fn ok_handler() -> Response {
        (StatusCode::OK, "ok").into_response()
    }

    /// 错误密码登录返回 401 Invalid credentials 并清除会话 cookie（配置密码两侧空白被 trim）。
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

    /// 登录 cookie 全属性断言并解锁状态查询与 gate；无 cookie 401、无效 bearer 401、trustDevice 升级 7 天 TTL。
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

    /// 同 IP 连续 10 次失败后进入 15 分钟锁定：429 + retryAfter 900，锁定期间连正确密码也被拒。
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

    /// URL token 需会话铸造、60 秒有效、只放行只读 GET 与 WS 升级；写操作/越权路径/伪造或过期 token 均被拒。
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
        assert!(guard(&ctx, &parts).await.is_ok());
        let parts = request(
            Method::GET,
            &format!("/api/terminal/ws?oc_url_token={token}"),
            &[],
        )
        .into_parts()
        .0;
        assert!(guard(&ctx, &parts).await.is_err());

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
        assert!(guard(&ctx, &parts).await.is_err());

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

    /// 未配置密码的端点形态：status 报 disabled、登录 400、url-token 铸匿名会话、管理路由直通、guard 放行。
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
        assert!(guard(&ctx, &parts).await.is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// POST /api/auth/reset 轮换磁盘与内存密钥并清 cookie，旧会话立即失效；未认证的 reset 得到 gate 的 401。
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

    /// passkey 各路由空库形态全景（status/options/verify/register/list/revoke）；register 系列需会话，未认证走纯文本 401 分支。
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

    /// 构造仅含 Origin 与附加头的请求 Parts，用于 Origin 校验测试。
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

    /// Origin 校验：打包客户端、同源、回环等价与 TLS 终止转发放行；跨站、缺失或畸形 Origin 拒绝。
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

    /// reject_websocket_upgrade 响应形状：Connection:close、text/plain、空 reason 回退 Bad Request、任意合法状态码可用。
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

    /// 非 JSON 登录体在密码校验前即返回 400 Invalid JSON body。
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

    /// X-Forwarded-Proto 为 https 的登录会签发带 ; Secure 的会话 cookie。
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
