//! Port of `server/lib/opencode/tunnel-auth.js` (`createTunnelAuth`): the
//! tunnel session auth surface. The tunnels module owns tunnel processes;
//! this controller owns request-scope classification, the one-time bootstrap
//! token exchange (`/connect?t=…`), in-memory tunnel sessions (cookie
//! `oc_tunnel_session`), and the connect rate limiter.
//!
//! Express `req`/`res` are collapsed into [`TunnelRequestContext`] (the
//! request facts the JS reads) and returned header strings for `Set-Cookie`,
//! so handlers stay in charge of the wire.
//!
//! 中文说明：移植自 `server/lib/opencode/tunnel-auth.js` 的
//! `createTunnelAuth`——tunnel 会话认证面。tunnel 进程本身归 tunnels
//! 模块管理；本控制器负责请求范围分类、一次性 bootstrap token 兑换
//! （`/connect?t=…`）、内存中的 tunnel 会话（cookie `oc_tunnel_session`）
//! 以及 connect 限流器。Express 的 req/res 在此收敛为 TunnelRequestContext
//! （JS 读取的请求事实）与返回的 Set-Cookie 字符串，wire 细节仍由
//! handler 掌握。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use serde::Serialize;
use serde_json::json;

use super::time::{Clock, http_date_from_unix_millis};
use super::util::{constant_time_equal_bytes, random_base64url, sha256_hex};
use crate::error::{AppError, AppResult};

/// tunnel 会话 cookie 名（与 JS 版一致）。
pub const TUNNEL_SESSION_COOKIE_NAME: &str = "oc_tunnel_session";
/// bootstrap token 的随机字节数（32 字节，base64url 后 43 字符）。
pub(crate) const BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES: usize = 32;
/// connect 兑换限流的计数窗口（5 分钟）。
pub(crate) const CONNECT_RATE_LIMIT_WINDOW_MS: i64 = 5 * 60 * 1000;
/// 触发限流后的锁定时长（10 分钟）。
pub(crate) const CONNECT_RATE_LIMIT_LOCK_MS: i64 = 10 * 60 * 1000;
/// 有客户端 IP 时的失败次数上限（20 次）。
pub(crate) const CONNECT_RATE_LIMIT_MAX_ATTEMPTS: u64 = 20;
/// 无 IP 请求（no-ip 桶）更严的失败次数上限（5 次）。
pub(crate) const CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS: u64 = 5;
/// JS rate-limit bucket used when no client IP is derivable.
/// 中文：无法推导出客户端 IP 时使用的共享限流桶 key。
pub const NO_IP_RATE_LIMIT_KEY: &str = "connect-rate-limit:no-ip";

/// The request facts `tunnel-auth.js` reads off `req`.
/// 中文：分类/兑换所需的全部请求事实。
pub struct TunnelRequestContext<'a> {
    /// 原始请求头（Host/Cookie/X-Forwarded-* 从这里读取）。
    pub headers: &'a HeaderMap,
    /// Express `req.hostname` (usually derived from the Host header).
    /// 中文：Express req.hostname（通常来自 Host 头）。
    pub hostname: Option<&'a str>,
    /// `req.socket.remoteAddress`.
    /// 中文：对端 socket 地址，用于防止伪造 Host 冒充本地。
    pub socket_remote_address: Option<&'a str>,
    /// Express `req.secure` (`req.protocol === 'https'`).
    /// 中文：Express req.secure（协议是否 https）。
    pub secure: bool,
    /// Express `req.ip` (with `trust proxy` it is XFF-derived).
    /// 中文：Express req.ip（trust proxy 时取自 XFF）。
    pub ip: Option<&'a str>,
}

/// 构造辅助。
impl<'a> TunnelRequestContext<'a> {
    /// 仅带 headers、其余字段留空的上下文（拿不到更多请求事实时使用）。
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
/// 中文：tunnel/local/未知公网三分类，决定请求是否需要 tunnel 会话。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestScope {
    /// 请求 Host 命中活动 tunnel 的公网域名。
    Tunnel,
    /// 本地回环/私有来源请求。
    Local,
    /// 有活动 tunnel 时的未知公网请求——tunnel 域名之外一律按公网对待。
    UnknownPublic,
}

/// 序列化辅助。
impl RequestScope {
    /// 与 JS 一致的小写字符串表示。
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
/// 中文：一次性 bootstrap token 的存储记录（只存 hash，不存明文）。
#[derive(Clone, Debug)]
struct BootstrapRecord {
    /// 发号时绑定的活动 tunnel id，兑换时必须匹配。
    tunnel_id: String,
    /// token 的 SHA-256 hex；兑换走常数时间比较。
    token_hash: String,
    /// 过期时刻（毫秒）；None 表示不过期。
    expires_at: Option<i64>,
    /// 已使用时刻；一旦置位即不可再用。
    used_at: Option<i64>,
    /// 撤销时刻；撤销后不可兑换。
    revoked_at: Option<i64>,
}

/// 内存中的 tunnel 会话记录（JS 的 sessionRecord）。
#[derive(Clone, Debug)]
struct TunnelSessionRecord {
    /// 会话 id，即 cookie oc_tunnel_session 的值（随机 base64url）。
    session_id: String,
    /// 所属 tunnel；活动 tunnel 变更后旧会话失效。
    tunnel_id: String,
    /// tunnel 模式（如 cloudflare）。
    mode: Option<String>,
    /// 建立会话时的 tunnel 公网 URL 快照。
    public_url: Option<String>,
    /// 创建时刻（毫秒）。
    created_at: i64,
    /// 最近一次通过认证的时刻（毫秒）。
    last_seen_at: i64,
    /// 过期时刻（毫秒）。
    expires_at: i64,
    /// 撤销时刻；None 表示未撤销。
    revoked_at: Option<i64>,
    /// 撤销原因（如 tunnel-revoked），对外展示用。
    revoked_reason: Option<String>,
    /// 惰性标记的过期时刻（首次检查/列表时补写）。
    expired_at: Option<i64>,
}

/// 单个限流 key 的失败计数与锁定状态。
#[derive(Clone, Debug, Default)]
struct RateRecord {
    /// 窗口内失败次数。
    count: u64,
    /// 最近一次失败时刻（毫秒）。
    last_attempt: i64,
    /// 锁定截止时刻；None 表示未锁定。
    locked_until: Option<i64>,
}

/// Mutex 保护的全部可变状态：活动 tunnel、bootstrap 记录、会话表、限流表。
#[derive(Default)]
struct TunnelAuthInner {
    /// 当前活动 tunnel id；None 表示无活动 tunnel。
    active_tunnel_id: Option<String>,
    /// 由 public URL 推导的 host，用于请求分类。
    active_tunnel_host: Option<String>,
    /// 当前 tunnel 模式。
    active_tunnel_mode: Option<String>,
    /// 当前 tunnel 公网 URL。
    active_tunnel_public_url: Option<String>,
    /// 当前 bootstrap token 记录（单发单用）。
    bootstrap_record: Option<BootstrapRecord>,
    /// 历史 + 活动的 tunnel 会话列表。
    tunnel_sessions: Vec<TunnelSessionRecord>,
    /// 按 key（客户端 IP 或 no-ip）的 connect 兑换限流状态。
    connect_rate_limiter: HashMap<String, RateRecord>,
}

/// Snapshot returned to callers (JS hands back the live record; consumers
/// only read fields).
/// 中文：给调用方的会话快照（JS 返回活记录；这里克隆只读字段）。
#[derive(Clone, Debug)]
pub struct TunnelSessionInfo {
    /// 会话 id（即 cookie 值）。
    pub session_id: String,
    /// 所属 tunnel id。
    pub tunnel_id: String,
    /// tunnel 模式。
    pub mode: Option<String>,
    /// tunnel 公网 URL。
    pub public_url: Option<String>,
    /// 创建时刻（毫秒）。
    pub created_at: i64,
    /// 最近活跃时刻（毫秒）。
    pub last_seen_at: i64,
    /// 过期时刻（毫秒）。
    pub expires_at: i64,
}

/// `listTunnelSessions` entry.
/// 中文：listTunnelSessions 的对外条目（camelCase 序列化）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicTunnelSession {
    /// 会话 id。
    pub session_id: String,
    /// 所属 tunnel id。
    pub tunnel_id: String,
    /// tunnel 模式。
    pub mode: Option<String>,
    /// tunnel 公网 URL。
    pub public_url: Option<String>,
    /// 创建时刻（毫秒）。
    pub created_at: i64,
    /// 最近活跃时刻（毫秒）。
    pub last_seen_at: i64,
    /// 过期时刻（毫秒）。
    pub expires_at: i64,
    /// 撤销时刻；未撤销为 None。
    pub revoked_at: Option<i64>,
    /// "active" 或 "inactive"。
    pub status: &'static str,
    /// inactive 时的原因（revoked/expired/inactive）。
    pub inactive_reason: Option<String>,
}

/// `issueBootstrapToken` result.
/// 中文：issueBootstrapToken 的返回。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedBootstrapToken {
    /// 明文 token（只在发号响应中出现一次）。
    pub token: String,
    /// 过期时刻（毫秒）；None 表示不过期。
    pub expires_at: Option<i64>,
}

/// `getBootstrapStatus` result.
/// 中文：getBootstrapStatus 的返回。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapStatus {
    /// 是否存在可用（未用/未撤销/未过期）的 bootstrap token。
    pub has_bootstrap_token: bool,
    /// token 过期时刻（毫秒）。
    pub bootstrap_expires_at: Option<i64>,
}

/// `revokeTunnelArtifacts` result.
/// 中文：revokeTunnelArtifacts 的返回计数。
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokedTunnelArtifacts {
    /// 撤销的 bootstrap token 数（0 或 1）。
    pub revoked_bootstrap_count: usize,
    /// 因此失效的会话数。
    pub invalidated_session_count: usize,
}

/// `exchangeBootstrapToken` result.
/// 中文：exchangeBootstrapToken 的结果（成功/失败原因及 cookie）。
#[derive(Clone, Debug)]
pub struct ExchangeOutcome {
    /// 是否兑换成功。
    pub ok: bool,
    /// Failure reason (`rate-limited`, `inactive`, `missing-token`,
    /// `expired`, `tunnel-mismatch`, `invalid-token`).
    /// 中文：失败原因字符串，与 JS 一致（rate-limited/inactive/
    /// missing-token/expired/tunnel-mismatch/invalid-token）。
    pub reason: Option<&'static str>,
    /// Retry-after hint in seconds when rate limited.
    /// 中文：限流时的 Retry-After 秒数。
    pub retry_after: Option<i64>,
    /// 成功时新会话的过期时刻（毫秒）。
    pub session_expires_at: Option<i64>,
    /// `Set-Cookie` header value to attach on success.
    /// 中文：成功时需附带的 Set-Cookie 值。
    pub set_cookie: Option<String>,
}

/// `requireTunnelSession` rejection: the JS middleware's 401 + cookie clear.
/// 中文：requireTunnelSession 的拒绝：401 + 清除会话 cookie。
#[derive(Clone, Debug)]
pub struct TunnelAuthRejection {
    /// 清除会话 cookie 的 Set-Cookie 值。
    pub set_cookie: String,
}

/// 拒绝转换为 axum 响应。
impl TunnelAuthRejection {
    /// 渲染 JS 中间件同款 401 JSON（locked/tunnelLocked）并附 Set-Cookie。
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

/// tunnel 会话认证控制器（JS createTunnelAuth 的 Rust 对应物）。
pub struct TunnelAuth {
    /// 可变状态（活动 tunnel、bootstrap、会话、限流）。
    inner: Mutex<TunnelAuthInner>,
    /// 注入时钟，测试可控。
    clock: Clock,
}

/// 控制器公开操作与内部限流实现。
impl TunnelAuth {
    /// 以给定时钟构造空状态的控制器。
    pub fn new(clock: Clock) -> Self {
        Self {
            inner: Mutex::new(TunnelAuthInner::default()),
            clock,
        }
    }

    /// 获取内部状态锁；锁中毒时恢复数据（持有者不会破坏不变量）。
    fn lock(&self) -> std::sync::MutexGuard<'_, TunnelAuthInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// JS `setActiveTunnel`: record the active tunnel and derive its host
    /// from the public URL.
    /// 中文：记录活动 tunnel，host 由 public URL 推导。
    pub fn set_active_tunnel(&self, tunnel_id: &str, public_url: Option<&str>, mode: Option<&str>) {
        let mut inner = self.lock();
        inner.active_tunnel_id = Some(tunnel_id.to_string());
        inner.active_tunnel_mode = mode.map(str::to_string);
        inner.active_tunnel_public_url = public_url.map(str::to_string);
        inner.active_tunnel_host = public_url.and_then(host_from_url);
    }

    /// JS `clearActiveTunnel`: revoke artifacts, then forget everything.
    /// 中文：先撤销该 tunnel 的工件，再清空全部活动状态与 bootstrap 记录。
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
    /// 中文：撤销指定 tunnel 的 bootstrap token 与全部会话。
    pub fn revoke_tunnel_artifacts(&self, tunnel_id: &str) -> RevokedTunnelArtifacts {
        let mut inner = self.lock();
        revoke_tunnel_artifacts_inner(&mut inner, tunnel_id, self.clock.clone())
    }

    /// 当前活动 tunnel id（无则 None）。
    pub fn active_tunnel_id(&self) -> Option<String> {
        self.lock().active_tunnel_id.clone()
    }

    /// 当前活动 tunnel 的 host（无则 None）。
    pub fn active_tunnel_host(&self) -> Option<String> {
        self.lock().active_tunnel_host.clone()
    }

    /// 当前活动 tunnel 的模式（无则 None）。
    pub fn active_tunnel_mode(&self) -> Option<String> {
        self.lock().active_tunnel_mode.clone()
    }

    /// JS `issueBootstrapToken`: single-use token bound to the active tunnel.
    /// 中文：签发绑定活动 tunnel 的单次 token；先撤销旧 token，
    /// 无活动 tunnel 时报 500。
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
    /// 中文：是否存在可用（未用/未撤销/未过期）的 bootstrap token。
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
    /// 中文：Host 命中活动 tunnel → Tunnel；本地 Host 且对端也是私有/
    /// 回环 → Local；否则有活动 tunnel 时为 UnknownPublic，无活动
    /// tunnel 回退 Local。
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
    /// 中文：校验会话 cookie——查找会话、拒绝已撤销/已过期/不属于
    /// 活动 tunnel 的记录，成功时刷新 last_seen。
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
    /// 中文：会话守卫：无有效会话则返回 401 拒绝（内含清 cookie 的 Set-Cookie）。
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
    /// 中文：限流下的一次性 token 兑换；通过后清除该 key 的限流计数、
    /// 创建会话并返回 Set-Cookie。任何失败（inactive/missing-token/
    /// expired/tunnel-mismatch/invalid-token）都计入失败次数。
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
    /// 中文：列出会话（最新在前），惰性标记过期并计算 active/inactive 与原因。
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
    /// 中文：构造清除会话 cookie 的 Set-Cookie（Max-Age=0）。
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

    /// JS checkConnectRateLimit：锁定期内拒绝并给出 Retry-After；
    /// 窗口过期则重置；达到上限时置 10 分钟锁定。
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

    /// 记录一次失败兑换：窗口内累加计数，窗口外重置为 1。
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

/// 限流判定结果。
struct RateLimitDecision {
    /// 是否放行本次尝试。
    allowed: bool,
    /// 拒绝时的 Retry-After 秒数（放行为 0）。
    retry_after: i64,
}

/// 构造统一形状的失败 ExchangeOutcome（无 cookie、无重试提示）。
fn exchange_failure(reason: &'static str) -> ExchangeOutcome {
    ExchangeOutcome {
        ok: false,
        reason: Some(reason),
        retry_after: None,
        session_expires_at: None,
        set_cookie: None,
    }
}

/// 撤销指定 tunnel 的 bootstrap（若绑定同一 tunnel），并把其全部
/// 未撤销会话标记为 tunnel-revoked。
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

/// 标记当前 bootstrap 记录为已撤销；无记录或已撤销返回 0，否则 1。
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

/// bootstrap 记录是否仍可兑换：未撤销、未使用、未过期。
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
/// 中文：构造会话 cookie 的 Set-Cookie 值（value 做 percent-encode）。
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
/// 中文：拼 Set-Cookie：Path=/、HttpOnly、SameSite=Lax、Max-Age/Expires，
/// secure 时追加 Secure。
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
/// 中文：req.secure，或首个 x-forwarded-proto 为 https。
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

/// 取指定头的字符串值（非法 UTF-8 视为缺失）。
fn header_str(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// JS `parseCookies` (percent-decoding is lenient where
/// `decodeURIComponent` would throw — invalid escapes stay literal).
/// 中文：把 Cookie 头解析为键值表；百分号解码宽松处理（非法转义保持原样）。
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

/// 宽松的百分号解码：仅解码合法的 %XX，其余字节原样保留。
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

/// URI 百分号编码：保留 unreserved 字符（字母数字与 -_.~）。
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
/// 中文：host 规范化——trim、小写、剥掉尾部数字端口。
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
/// 中文：等价 new URL(publicUrl).host；IPv6 字面量补回方括号。
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
/// 中文：IP 候选规范化——小写、去方括号、去 zone id、::ffff: 映射
/// 还原为 IPv4。
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

/// 是否为点分十进制 IPv4（四段、纯数字、每段 ≤255）。
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
/// 中文：IPv4 私有/回环段（127/8、10/8、172.16-31、192.168、169.254）。
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
/// 中文：IPv6 回环（::1）、ULA（fc/fd）与链路本地（fe8-feb）。
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

/// 规范化后按是否含冒号分派到 v4/v6 判定；无法规范化视为非私有。
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
/// 中文：Host 看起来是本地（localhost/host.docker.internal/私有 IP）
/// 且对端 socket 也是私有/回环——公网对端伪造 Host 不算本地。
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
/// 中文：客户端 IP——取 XFF 首项，否则 req.ip / socket 地址；
/// 剥掉 ::ffff: 前缀。
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

/// 剥掉 IPv4-mapped IPv6 前缀 ::ffff:。
fn strip_ipv6_mapped(ip: &str) -> String {
    ip.strip_prefix("::ffff:").unwrap_or(ip).to_string()
}

/// 限流 key：客户端 IP，取不到时用共享的 no-ip 桶。
fn rate_limit_key(ctx: &TunnelRequestContext) -> String {
    get_client_ip(ctx).unwrap_or_else(|| NO_IP_RATE_LIMIT_KEY.to_string())
}

/// 该 key 的失败上限：no-ip 桶更严（5 次 vs 20 次）。
fn rate_limit_max_for_key(key: &str) -> u64 {
    if key == NO_IP_RATE_LIMIT_KEY {
        CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS
    } else {
        CONNECT_RATE_LIMIT_MAX_ATTEMPTS
    }
}
