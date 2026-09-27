//! Port of `server/lib/client-auth/pairing.js`
//! (`createClientPairingRuntime`): short-lived one-time Pairing v2 sessions
//! persisted at `<data_dir>/client-pairing-sessions.json`.
//!
//! A session is created with a secret shown once (stored hashed), can be
//! cancelled, expires after the TTL (default 10 minutes), and is redeemed
//! exactly once into a remote-client token via the [`ClientIssuer`] seam
//! (the JS `remoteClientAuthRuntime` dependency).
//!
//! 中文说明：移植自 `server/lib/client-auth/pairing.js` 的
//! `createClientPairingRuntime`：短期一次性 Pairing v2 会话，持久化在
//! `<data_dir>/client-pairing-sessions.json`。会话创建时生成仅展示一次
//! 的 secret（存储时只存 hash），可取消、按 TTL（默认 10 分钟）过期，
//! 并通过 ClientIssuer 接缝（对应 JS 的 remoteClientAuthRuntime 依赖）
//! 恰好一次地兑换成 remote-client token。

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;

use super::remote_clients::{
    CreateClientInput, PublicClient, normalize_optional_string, normalize_timestamp, now_iso,
    write_file_private,
};
use super::time::{Clock, iso_utc_from_unix_millis, parse_iso_ms};
use super::util::{constant_time_equal_hex, random_base64url, random_hex, sha256_hex};
use crate::error::{AppError, AppResult};

/// 配对存储的 schema 版本。
pub(crate) const STORE_VERSION: i64 = 1;
/// 配对会话 id 前缀（pair_）。
pub const PAIRING_ID_PREFIX: &str = "pair_";
/// 配对 secret 的随机字节数（32 字节）。
pub(crate) const SECRET_BYTES: usize = 32;
/// 指纹的随机字节数（4 字节，展示为 XXXX-XXXX）。
pub(crate) const FINGERPRINT_BYTES: usize = 4;
/// 会话默认 TTL（10 分钟）。
pub const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;
/// 标签最大长度（按字符计，超出截断）。
pub(crate) const MAX_LABEL_LENGTH: usize = 80;
/// JS `GENERIC_REDEEM_ERROR` — matched by message in `core-routes.js`'s
/// redeem catch, so the exact string is load-bearing.
/// 中文：所有兑换拒绝共用的通用文案；core-routes.js 按消息匹配捕获，
/// 字符串本身是承重的。
pub const GENERIC_REDEEM_ERROR: &str = "Invalid or expired pairing session";
/// JS `PAIRING_LABEL_PLACEHOLDER` — display default only; the stored label
/// stays None so redeem can fall back to the device's reported name.
/// 中文：仅用于展示的默认标签；存储中 label 保持 None，兑换时才回退
/// 到设备上报名。
pub const PAIRING_LABEL_PLACEHOLDER: &str = "Pair new device";

/// 合法的客户端类型（mobile/desktop）。
const VALID_CLIENT_KINDS: [&str; 2] = ["mobile", "desktop"];

/// What [`ClientIssuer::create_client`] hands back (JS `{ client, token }`).
/// 中文：发号结果——新客户端公开形状与其 token。
pub struct CreatedClientRef {
    /// 新客户端 id；测试假实现可为 None。
    pub client_id: Option<String>,
    /// 新客户端的公开形状。
    pub client: PublicClient,
    /// 新客户端的 bearer token（oc_client_ 前缀）。
    pub token: String,
}

/// What [`ClientIssuer::create_client`] hands back (JS `{ client, token }`).
/// The JS `remoteClientAuthRuntime` dependency: the single method pairing
/// needs. Implemented by
/// [`RemoteClientAuth`](super::remote_clients::RemoteClientAuth); tests
/// inject fakes at this seam like the JS DI point.
/// 中文：配对与 remote-client 存储之间的接缝；对应 JS 的
/// remoteClientAuthRuntime 依赖注入点。
pub trait ClientIssuer: Send + Sync {
    /// 按输入发号：成功返回新客户端 id/公开形状与 token，失败返回错误
    /// （由 redeem 决定是否消费会话）。
    fn create_client<'a>(
        &'a self,
        input: CreateClientInput,
    ) -> BoxFuture<'a, AppResult<CreatedClientRef>>;
}

/// 生产实现：直接转发到 RemoteClientAuth::create_client。
impl ClientIssuer for super::remote_clients::RemoteClientAuth {
    /// 转发发号请求，把结果补上 client_id 包装为 CreatedClientRef。
    fn create_client<'a>(
        &'a self,
        input: CreateClientInput,
    ) -> BoxFuture<'a, AppResult<CreatedClientRef>> {
        Box::pin(async move {
            self.create_client(input)
                .await
                .map(|created| CreatedClientRef {
                    client_id: Some(created.client.id.clone()),
                    client: created.client,
                    token: created.token,
                })
        })
    }
}
/// 持久化的配对会话记录（camelCase 序列化，含 secretHash）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredSession {
    /// 会话 id（pair_ 前缀）。
    pub id: String,
    /// secret 的 SHA-256 hex；明文只在创建响应中出现一次。
    pub secret_hash: String,
    /// 创建时刻（ISO）。
    pub created_at: String,
    /// 过期时刻（ISO）。
    pub expires_at: String,
    /// 兑换时刻；置位后不可再兑换。
    pub used_at: Option<String>,
    /// 取消时刻；置位后不可再兑换。
    pub cancelled_at: Option<String>,
    /// 兑换成功的客户端 id。
    pub client_id: Option<String>,
    /// 操作者输入的标签；None 时展示用占位符、兑换走 fallback。
    pub label: Option<String>,
    /// 展示用短指纹（XXXX-XXXX）。
    pub fingerprint: String,
    /// 允许兑换的客户端类型白名单。
    pub allowed_client_kinds: Vec<String>,
    /// 发起配对的客户端 id（客户端代发场景）。
    pub created_by_client_id: Option<String>,
    /// 会话是否面向 relay 传输（计入 relay 需求）。
    pub uses_relay: bool,
}

/// 存储文件顶层形状。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredPairingStore {
    /// schema 版本（1）。
    pub version: i64,
    /// 全部会话（含历史，由清扫策略淘汰）。
    pub sessions: Vec<StoredSession>,
}

/// JS `publicSession` (the wire shape; never carries `secretHash`).
/// 中文：对外会话形状，永不携带 secret/secretHash。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSession {
    /// 会话 id。
    pub id: String,
    /// 创建时刻（ISO）。
    pub created_at: String,
    /// 过期时刻（ISO）。
    pub expires_at: String,
    /// 兑换时刻；未兑换为 None。
    pub used_at: Option<String>,
    /// 取消时刻；未取消为 None。
    pub cancelled_at: Option<String>,
    /// 兑换出的客户端 id。
    pub client_id: Option<String>,
    /// 展示标签（无输入时为占位符）。
    pub label: String,
    /// 短指纹。
    pub fingerprint: String,
    /// 客户端类型白名单。
    pub allowed_client_kinds: Vec<String>,
    /// 发起配对的客户端 id。
    pub created_by_client_id: Option<String>,
    /// 是否面向 relay 传输。
    pub uses_relay: bool,
}

/// JS `createPairingSession` result: `{ pairing: publicSession + secret }` —
/// the secret is shown once.
/// 中文：createPairingSession 的结果——公开会话 + 仅此一次的 secret。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedPairingSession {
    /// 公开会话形状与一次性 secret 的合并体。
    pub pairing: PublicPairingWithSecret,
}

/// 创建响应中的会话形状：PublicSession 展开 + 顶层 secret。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicPairingWithSecret {
    /// 展开序列化的公开会话形状。
    #[serde(flatten)]
    pub session: PublicSession,
    /// 明文 secret，仅在创建响应中出现一次。
    pub secret: String,
}

/// JS `cancelPairingSession` result.
/// 中文：cancelPairingSession 的结果。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelResult {
    /// 是否真正取消（会话存在才算）。
    pub cancelled: bool,
    /// 取消后的公开会话形状；未找到会话为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PublicSession>,
}

/// JS `redeemPairingSession` result.
/// 中文：redeemPairingSession 的结果——会话 + 新客户端 + token。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemedPairing {
    /// 兑换后的公开会话形状（已带 usedAt/clientId）。
    pub pairing: PublicSession,
    /// 新建的客户端公开形状。
    pub client: PublicClient,
    /// 客户端 bearer token。
    pub token: String,
}

/// JS `sweepExpiredSessions` result.
/// 中文：sweepExpiredSessions 的结果。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepResult {
    /// 本次清扫删除的会话数。
    pub purged: usize,
}

/// JS `createPairingSession` input.
/// 中文：createPairingSession 的输入。
#[derive(Clone, Debug, Default)]
pub struct CreatePairingInput {
    /// 操作者输入的标签（可空）。
    pub label: Option<String>,
    /// 客户端类型白名单；空时默认 mobile+desktop。
    pub allowed_client_kinds: Option<Vec<String>>,
    /// 发起配对的客户端 id（客户端代发场景）。
    pub created_by_client_id: Option<String>,
    /// 是否面向 relay 传输。
    pub uses_relay: bool,
}

/// JS `redeemPairingSession` input.
/// 中文：redeemPairingSession 的输入。
#[derive(Clone, Debug, Default)]
pub struct RedeemPairingInput {
    /// 目标会话 id。
    pub pairing_id: Option<String>,
    /// 创建时下发的一次性 secret。
    pub secret: Option<String>,
    /// 设备标签（仅作 fallback）。
    pub client_label: Option<String>,
    /// 客户端类型；缺省按 mobile。
    pub client_kind: Option<String>,
    /// 设备自报名（fallback 标签与元数据）。
    pub device_name: Option<String>,
    /// 设备平台。
    pub device_platform: Option<String>,
    /// 设备型号。
    pub device_model: Option<String>,
    /// 客户端应用版本。
    pub app_version: Option<String>,
    /// 去重键；缺省为 pairing:<会话 id>。
    pub dedupe_key: Option<String>,
}

/// 配对运行时（JS createClientPairingRuntime 的对应物）。
pub struct ClientPairing {
    /// 持久化文件路径。
    store_path: PathBuf,
    /// 注入时钟。
    clock: Clock,
    /// 会话 TTL（毫秒）。
    ttl_ms: i64,
    /// 发号接缝（生产为 RemoteClientAuth）。
    issuer: Arc<dyn ClientIssuer>,
    /// JS `storeMutationQueue`.
    /// 中文：对应 JS 的 storeMutationQueue，串行化读-改-写。
    mutation_lock: tokio::sync::Mutex<()>,
}

/// 运行时操作：创建/列举/查询/取消/兑换/清扫，及私有的存储读写。
impl ClientPairing {
    /// 构造运行时（store 路径 + 时钟 + TTL + 发号器）。
    pub fn new(
        store_path: PathBuf,
        clock: Clock,
        ttl_ms: i64,
        issuer: Arc<dyn ClientIssuer>,
    ) -> Self {
        Self {
            store_path,
            clock,
            ttl_ms,
            issuer,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// JS `createPairingSession`.
    /// 中文：创建会话——先顺带清扫过期会话，再生成一次性 secret（只存
    /// hash）、id、指纹与白名单后落盘。
    pub async fn create_pairing_session(
        &self,
        input: CreatePairingInput,
    ) -> AppResult<CreatedPairingSession> {
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        sweep_expired_sessions_from_store(&mut store, self.ttl_ms, self.clock.clone());
        let secret = generate_secret();
        let session = StoredSession {
            id: generate_id(),
            secret_hash: sha256_hex(secret.as_bytes()),
            created_at: now_iso(self.clock.clone()),
            expires_at: iso_utc_from_unix_millis((self.clock)() + self.ttl_ms),
            used_at: None,
            cancelled_at: None,
            client_id: None,
            label: normalize_stored_label(input.label.as_deref()),
            fingerprint: generate_fingerprint(),
            allowed_client_kinds: normalize_allowed_client_kinds(
                input.allowed_client_kinds.as_deref(),
            ),
            created_by_client_id: normalize_optional_string(input.created_by_client_id.as_deref()),
            uses_relay: input.uses_relay,
        };
        let public = public_session(&session);
        store.sessions.push(session);
        self.write_store(&store).await?;
        Ok(CreatedPairingSession {
            pairing: PublicPairingWithSecret {
                session: public,
                secret,
            },
        })
    }

    /// Sessions that can still be redeemed (link created, device not yet
    /// connected).
    /// 中文：仍可兑换的会话（已创建、设备未连接）。
    pub async fn list_pending_sessions(&self) -> AppResult<Vec<PublicSession>> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store
            .sessions
            .iter()
            .filter(|session| is_pending_session(session, self.clock.clone()))
            .map(public_session)
            .collect())
    }

    /// Relay-transport demand from pairing: any still-redeemable relay
    /// session.
    /// 中文：是否存在仍可兑换的 relay 会话（relay 需求信号）。
    pub async fn has_active_relay_session(&self) -> AppResult<bool> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store
            .sessions
            .iter()
            .any(|session| session.uses_relay && is_pending_session(session, self.clock.clone())))
    }

    /// JS `getPairingSession`.
    /// 中文：按 id 查公开会话；id 缺失或未找到返回 None。
    pub async fn get_pairing_session(&self, id: Option<&str>) -> AppResult<Option<PublicSession>> {
        let Some(id) = normalize_optional_string(id) else {
            return Ok(None);
        };
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store
            .sessions
            .iter()
            .find(|entry| entry.id == id)
            .map(public_session))
    }

    /// JS `cancelPairingSession`.
    /// 中文：标记 cancelled_at（幂等）；会话不存在时 cancelled=false。
    pub async fn cancel_pairing_session(&self, id: Option<&str>) -> AppResult<CancelResult> {
        let Some(id) = normalize_optional_string(id) else {
            return Ok(CancelResult {
                cancelled: false,
                pairing: None,
            });
        };
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let Some(session) = store.sessions.iter_mut().find(|entry| entry.id == id) else {
            return Ok(CancelResult {
                cancelled: false,
                pairing: None,
            });
        };
        if session.cancelled_at.is_none() {
            session.cancelled_at = Some(now_iso(self.clock.clone()));
        }
        let public = public_session(session);
        self.write_store(&store).await?;
        Ok(CancelResult {
            cancelled: true,
            pairing: Some(public),
        })
    }

    /// JS `redeemPairingSession`: one-time secret redemption into a remote
    /// client token. Generic `400 Invalid or expired pairing session` for
    /// every rejection; issuance failures propagate unchanged and leave the
    /// session unconsumed.
    /// 中文：一次性兑换——校验存在/未用/未取消/未过期/类型白名单/
    /// secret hash（常数时间比较）；任何拒绝都返回通用 400，发号失败
    /// 原样上抛且会话不被消费，成功后写 used_at/client_id 并落盘。
    pub async fn redeem_pairing_session(
        &self,
        input: RedeemPairingInput,
    ) -> AppResult<RedeemedPairing> {
        let normalized_id = normalize_optional_string(input.pairing_id.as_deref());
        let normalized_secret = normalize_optional_string(input.secret.as_deref());
        let normalized_kind = normalize_client_kind(input.client_kind.as_deref())
            .unwrap_or_else(|| "mobile".to_string());
        let (Some(normalized_id), Some(normalized_secret)) = (normalized_id, normalized_secret)
        else {
            return Err(redeem_error());
        };

        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let Some(session) = store
            .sessions
            .iter_mut()
            .find(|entry| entry.id == normalized_id)
        else {
            return Err(redeem_error());
        };
        if session.cancelled_at.is_some() || session.used_at.is_some() {
            return Err(redeem_error());
        }
        if parse_iso_ms(&session.expires_at).unwrap_or(0) <= (self.clock)() {
            return Err(redeem_error());
        }
        if !session.allowed_client_kinds.contains(&normalized_kind) {
            return Err(redeem_error());
        }
        if !constant_time_equal_hex(
            &session.secret_hash,
            &sha256_hex(normalized_secret.as_bytes()),
        ) {
            return Err(redeem_error());
        }

        // The operator's typed pairing label is THIS server's name for the
        // device and wins outright; the device's self-reported label is only
        // a fallback (a re-pair with the same dedupeKey keeps the replaced
        // record's label over it).
        let fallback_label = normalize_optional_string(input.client_label.as_deref())
            .or_else(|| normalize_optional_string(input.device_name.as_deref()))
            .unwrap_or_else(|| "Remote client".to_string());
        let result = self
            .issuer
            .create_client(CreateClientInput {
                label: normalize_optional_string(session.label.as_deref()),
                fallback_label: Some(fallback_label),
                client_kind: Some(normalized_kind),
                dedupe_key: Some(
                    normalize_optional_string(input.dedupe_key.as_deref())
                        .unwrap_or_else(|| format!("pairing:{}", session.id)),
                ),
                auth_method: Some("pairing".to_string()),
                pairing_id: Some(session.id.clone()),
                device_name: input.device_name.clone(),
                device_platform: input.device_platform.clone(),
                device_model: input.device_model.clone(),
                app_version: input.app_version.clone(),
                uses_relay: session.uses_relay,
                expires_at: None,
            })
            .await?;

        session.used_at = Some(now_iso(self.clock.clone()));
        session.client_id = result.client_id.clone();
        let public = public_session(session);
        self.write_store(&store).await?;
        Ok(RedeemedPairing {
            pairing: public,
            client: result.client,
            token: result.token,
        })
    }

    /// JS `sweepExpiredSessions`.
    /// 中文：清扫存储并返回删除数量（无删除不写盘）。
    pub async fn sweep_expired_sessions(&self) -> AppResult<SweepResult> {
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let before = store.sessions.len();
        sweep_expired_sessions_from_store(&mut store, self.ttl_ms, self.clock.clone());
        let purged = before - store.sessions.len();
        if purged > 0 {
            self.write_store(&store).await?;
        }
        Ok(SweepResult { purged })
    }

    /// 读取并规范化存储；文件缺失返回空存储，非法 JSON 按 Null 规范化。
    async fn read_store(&self) -> AppResult<StoredPairingStore> {
        let raw = match tokio::fs::read_to_string(&self.store_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(normalize_pairing_store(
                    &Value::Null,
                    &self.clock,
                    self.ttl_ms,
                ));
            }
            Err(err) => return Err(err.into()),
        };
        let payload = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
        Ok(normalize_pairing_store(&payload, &self.clock, self.ttl_ms))
    }

    /// 序列化（规范化后 pretty JSON），经 write_file_private 以私有权限写盘。
    async fn write_store(&self, store: &StoredPairingStore) -> AppResult<()> {
        let normalized = normalize_pairing_store(
            &serde_json::to_value(store)
                .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?,
            &self.clock,
            self.ttl_ms,
        );
        let body = serde_json::to_string_pretty(&normalized)
            .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?;
        write_file_private(&self.store_path, body.as_bytes()).await
    }
}

/// JS `redeemError`: a 400 carrying the exact generic message.
/// 中文：统一的兑换拒绝错误——400 + GENERIC_REDEEM_ERROR。
pub fn redeem_error() -> AppError {
    AppError::new(axum::http::StatusCode::BAD_REQUEST, GENERIC_REDEEM_ERROR)
}

/// 存储记录转公开形状：剥掉 secretHash，label 空时填占位符。
pub(crate) fn public_session(session: &StoredSession) -> PublicSession {
    PublicSession {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        expires_at: session.expires_at.clone(),
        used_at: session.used_at.clone(),
        cancelled_at: session.cancelled_at.clone(),
        client_id: session.client_id.clone(),
        label: session
            .label
            .clone()
            .unwrap_or_else(|| PAIRING_LABEL_PLACEHOLDER.to_string()),
        fingerprint: session.fingerprint.clone(),
        allowed_client_kinds: session.allowed_client_kinds.clone(),
        created_by_client_id: session.created_by_client_id.clone(),
        uses_relay: session.uses_relay,
    }
}

/// A pending session is one that can still be redeemed: not used, not
/// cancelled, not expired.
/// 中文：pending = 未使用、未取消、未过期（expires 可解析）。
pub(crate) fn is_pending_session(session: &StoredSession, clock: Clock) -> bool {
    session.used_at.is_none()
        && session.cancelled_at.is_none()
        && parse_iso_ms(&session.expires_at).is_some_and(|expires| expires > clock())
}

/// JS `sweepExpiredSessionsFromStore`: keep used/cancelled sessions for the
/// TTL window, drop expired never-redeemed ones.
/// 中文：已用/已取消的会话保留一个 TTL 窗口；从未使用的过期会话删除
/// （expires 无法解析的保留）。
pub(crate) fn sweep_expired_sessions_from_store(
    store: &mut StoredPairingStore,
    ttl_ms: i64,
    clock: Clock,
) {
    let now = clock();
    let cutoff = now - ttl_ms;
    store.sessions.retain(|session| {
        let used_at = session.used_at.as_deref().and_then(parse_iso_ms);
        let cancelled_at = session.cancelled_at.as_deref().and_then(parse_iso_ms);
        let inactive_at = used_at.or(cancelled_at);
        if let Some(inactive_at) = inactive_at {
            return inactive_at >= cutoff;
        }
        // Never used or cancelled: drop once the session itself has expired.
        match parse_iso_ms(&session.expires_at) {
            Some(expires_at) => expires_at > now,
            None => true,
        }
    });
}

/// 把任意 JSON 规范化为存储形状：补默认 id/createdAt/expires/指纹/
/// 白名单，丢弃缺 secretHash 的条目，并重置 version。
pub(crate) fn normalize_pairing_store(
    payload: &Value,
    clock: &Clock,
    ttl_ms: i64,
) -> StoredPairingStore {
    let now_iso = || now_iso(clock.clone());
    let mut sessions = Vec::new();
    if let Some(raw_sessions) = payload.get("sessions").and_then(Value::as_array) {
        for session in raw_sessions.iter().filter(|s| s.is_object()) {
            let normalized = StoredSession {
                id: match session.get("id").and_then(Value::as_str) {
                    Some(id) => id.to_string(),
                    None => generate_id(),
                },
                secret_hash: session
                    .get("secretHash")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                created_at: match session.get("createdAt").and_then(Value::as_str) {
                    Some(at) => at.to_string(),
                    None => now_iso(),
                },
                expires_at: normalize_timestamp(session.get("expiresAt").and_then(Value::as_str))
                    .unwrap_or_else(|| iso_utc_from_unix_millis(clock() + ttl_ms)),
                used_at: normalize_timestamp(session.get("usedAt").and_then(Value::as_str)),
                cancelled_at: normalize_timestamp(
                    session.get("cancelledAt").and_then(Value::as_str),
                ),
                client_id: normalize_optional_string(
                    session.get("clientId").and_then(Value::as_str),
                ),
                label: normalize_stored_label(session.get("label").and_then(Value::as_str)),
                fingerprint: normalize_optional_string(
                    session.get("fingerprint").and_then(Value::as_str),
                )
                .unwrap_or_else(generate_fingerprint),
                allowed_client_kinds: normalize_allowed_client_kinds(
                    session
                        .get("allowedClientKinds")
                        .and_then(Value::as_array)
                        .map(|kinds| {
                            kinds
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .as_deref(),
                ),
                created_by_client_id: normalize_optional_string(
                    session.get("createdByClientId").and_then(Value::as_str),
                ),
                uses_relay: session.get("usesRelay") == Some(&Value::Bool(true)),
            };
            if !normalized.secret_hash.is_empty() {
                sessions.push(normalized);
            }
        }
    }
    StoredPairingStore {
        version: STORE_VERSION,
        sessions,
    }
}

/// JS `normalizeStoredLabel`: trimmed, capped, None when unset.
/// 中文：trim 后按字符截断到 MAX_LABEL_LENGTH；空白返回 None。
fn normalize_stored_label(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    if normalized.chars().count() > MAX_LABEL_LENGTH {
        Some(normalized.chars().take(MAX_LABEL_LENGTH).collect())
    } else {
        Some(normalized)
    }
}

/// JS `normalizeClientKind`: valid kind or None.
/// 中文：仅接受 mobile/desktop，其余（含空白）返回 None。
fn normalize_client_kind(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    VALID_CLIENT_KINDS
        .contains(&normalized.as_str())
        .then_some(normalized)
}

/// JS `normalizeAllowedClientKinds`: default `["mobile", "desktop"]`, deduped
/// in first-seen order.
/// 中文：默认 mobile+desktop；否则按首次出现顺序去重，全非法时回退默认。
fn normalize_allowed_client_kinds(value: Option<&[String]>) -> Vec<String> {
    let Some(values) = value else {
        return VALID_CLIENT_KINDS.iter().map(|s| s.to_string()).collect();
    };
    let mut kinds = Vec::new();
    for kind in values.iter().filter_map(|k| normalize_client_kind(Some(k))) {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    if kinds.is_empty() {
        VALID_CLIENT_KINDS.iter().map(|s| s.to_string()).collect()
    } else {
        kinds
    }
}

/// 生成会话 id：pair_ + 12 字节随机 hex。
fn generate_id() -> String {
    format!("{PAIRING_ID_PREFIX}{}", random_hex(12))
}

/// 生成一次性 secret：32 字节随机数的 base64url（43 字符）。
fn generate_secret() -> String {
    random_base64url(SECRET_BYTES)
}

/// 4 random bytes as upper-case hex formatted `XXXX-XXXX`.
/// 中文：4 字节随机 hex 大写，格式化为 XXXX-XXXX。
fn generate_fingerprint() -> String {
    let hex = random_hex(FINGERPRINT_BYTES).to_uppercase();
    format!("{}-{}", &hex[0..4], &hex[4..8])
}
