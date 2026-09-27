//! Port of `server/lib/client-auth/remote-clients.js`
//! (`createRemoteClientAuthRuntime`): the trusted-device registry persisted at
//! `<data_dir>/remote-clients.json`.
//!
//! Tokens (`oc_client_…`) are returned once, stored only as SHA-256 hashes,
//! and authenticated in constant time. Every public operation runs inside the
//! mutation queue (JS `withStoreMutation`), so concurrent auth traffic and
//! revocation cannot interleave read-modify-write cycles.
//! server/lib/client-auth/remote-clients.js 的
//! `createRemoteClientAuthRuntime` 移植：受信设备注册表，持久化在
//! `<data_dir>/remote-clients.json`。
//!
//! 令牌（`oc_client_…` 前缀）只在签发时返回一次，落盘仅保存 SHA-256
//! 哈希，认证比较在常数时间内完成。所有公开操作都在互斥队列（JS 的
//! `withStoreMutation`）内执行，因此并发的认证流量与吊销操作不会交错
//! 出现读-改-写竞争。

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use super::time::{Clock, iso_utc_from_unix_millis, parse_iso_ms};
use super::util::{constant_time_equal_hex, random_base64url, random_hex, sha256_hex};
use crate::error::{AppError, AppResult};

/// store schema 版本；normalize 后写盘固定为该值。
pub(crate) const STORE_VERSION: i64 = 1;
/// 远程客户端令牌的固定前缀；认证阶段先做一次廉价的前缀过滤。
pub const TOKEN_PREFIX: &str = "oc_client_";
/// 生成令牌时注入的随机字节数（base64url 编码后为 43 个字符）。
pub(crate) const TOKEN_BYTES: usize = 32;
/// 客户端显示名称的最大长度（按字符数截断）。
pub(crate) const MAX_LABEL_LENGTH: usize = 80;
/// lastUsedAt 落盘的最小间隔；节流以降低注册表文件的写放大。
pub(crate) const LAST_USED_WRITE_INTERVAL_MS: i64 = 60_000;

/// Which transport carried a request (JS reads
/// `x-ompchamber-relay-connection` truthiness).
/// 请求到达服务端所经由的传输通道；认证时由调用方按
/// `x-ompchamber-relay-connection` 头的有无传入，决定是否触发
/// usesRelay 自愈与 relay 需求统计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// 经由 relay 隧道转发到达。
    Relay,
    /// 客户端直连（LAN 或本机）。
    Direct,
}

/// Transport 的字符串表示与持久化形态。
impl Transport {
    /// 返回写入 lastTransport 字段的小写字符串（"relay" / "direct"）。
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Relay => "relay",
            Transport::Direct => "direct",
        }
    }
}

/// One stored client record. Field order matches the JS `normalizeStore`
/// object literal so serialized JSON bytes line up.
/// 注册表中一条客户端记录的落盘形态。字段顺序与 JS `normalizeStore`
/// 的对象字面量一致，保证序列化后的 JSON 字节逐位对齐。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredClient {
    /// 客户端唯一标识（12 字节随机 hex），同时充当 session token。
    pub id: String,
    /// 显示名称；归一后非空且不超过 80 字符。
    pub label: String,
    /// 令牌的 SHA-256 hex 摘要；明文令牌永不落盘。
    pub token_hash: String,
    /// 创建时间（ISO-8601 UTC）。
    pub created_at: String,
    /// 最近一次认证成功的时间；按写间隔节流更新。
    pub last_used_at: Option<String>,
    /// 吊销时间；Some 表示已吊销，认证一律拒绝。
    pub revoked_at: Option<String>,
    /// 过期时间；到期后认证按无效处理。
    pub expires_at: Option<String>,
    /// 客户端种类标签（如 desktop/cli），参与旧记录迁移替换。
    pub client_kind: Option<String>,
    /// 设备去重键；同 key 重复签发会替换旧记录。
    pub dedupe_key: Option<String>,
    /// 配对时声明的 relay 使用意向；粘性标志，可被 relay 流量自愈为 true。
    pub uses_relay: bool,
    /// 最近一次认证实际观测到的传输通道（"relay" 或 "direct"）。
    pub last_transport: Option<String>,
    /// 签发途径审计字段（如 pairing）。
    pub auth_method: Option<String>,
    /// 签发该令牌的配对会话 id（若经由 Pairing v2）。
    pub pairing_id: Option<String>,
    /// 客户端上报的设备名。
    pub device_name: Option<String>,
    /// 客户端上报的平台。
    pub device_platform: Option<String>,
    /// 客户端上报的设备型号。
    pub device_model: Option<String>,
    /// 客户端上报的应用版本。
    pub app_version: Option<String>,
}

/// 整个注册表文件的落盘形态：schema 版本 + 客户端记录列表。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredRemoteClientStore {
    /// store schema 版本；读取归一时统一改写为 STORE_VERSION。
    pub version: i64,
    /// 全部客户端记录（含已吊销，直到被 purge 物理删除）。
    pub clients: Vec<StoredClient>,
}

/// JS `publicClient` — the wire shape; never carries `tokenHash`.
/// 对外暴露的客户端视图（JS `publicClient`），由 StoredClient 投影而
/// 来；绝不携带 tokenHash。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicClient {
    /// 客户端唯一标识。
    pub id: String,
    /// 显示名称。
    pub label: String,
    /// 创建时间（ISO-8601 UTC）。
    pub created_at: String,
    /// 最近一次成功认证时间。
    pub last_used_at: Option<String>,
    /// 吊销时间；None 表示未吊销。
    pub revoked_at: Option<String>,
    /// 过期时间；None 表示永不过期。
    pub expires_at: Option<String>,
    /// 客户端种类标签。
    pub client_kind: Option<String>,
    /// 签发途径审计字段。
    pub auth_method: Option<String>,
    /// 签发该令牌的配对会话 id。
    pub pairing_id: Option<String>,
    /// 设备名。
    pub device_name: Option<String>,
    /// 设备平台。
    pub device_platform: Option<String>,
    /// 设备型号。
    pub device_model: Option<String>,
    /// 客户端应用版本。
    pub app_version: Option<String>,
    /// 是否（曾）声明经由 relay 使用；relay 需求判断的输入之一。
    pub uses_relay: bool,
    /// 最近一次认证观测到的实际传输通道。
    pub last_transport: Option<String>,
}

/// JS `createClient` input object (destructured with all-optional fields).
/// `createClient` 的输入对象（对应 JS 全可选字段的解构形态）。
#[derive(Clone, Debug, Default)]
pub struct CreateClientInput {
    /// 调用方显式指定的显示名称，优先级最高。
    pub label: Option<String>,
    /// 首次签发且无 label 时使用的兜底名称。
    pub fallback_label: Option<String>,
    /// 过期时间字符串；归一为 canonical ISO 或 None。
    pub expires_at: Option<String>,
    /// 客户端种类标签，参与旧记录迁移替换。
    pub client_kind: Option<String>,
    /// 设备去重键；命中同 key 旧记录时执行替换。
    pub dedupe_key: Option<String>,
    /// 签发途径（如 pairing）。
    pub auth_method: Option<String>,
    /// 关联的配对会话 id。
    pub pairing_id: Option<String>,
    /// 客户端上报的设备名。
    pub device_name: Option<String>,
    /// 客户端上报的平台。
    pub device_platform: Option<String>,
    /// 客户端上报的设备型号。
    pub device_model: Option<String>,
    /// 客户端上报的应用版本。
    pub app_version: Option<String>,
    /// 配对时是否声明使用 relay。
    pub uses_relay: bool,
}

/// `createClient` result: `{ client, token }` — the token is shown once.
/// `createClient` 的返回 `{ client, token }`——明文令牌仅此一次可见。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedClient {
    /// 新建客户端的公开视图。
    pub client: PublicClient,
    /// 明文 bearer token；此后只以哈希形态存在。
    pub token: String,
}

/// `revokeClient` result.
/// `revokeClient` 的返回。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeResult {
    /// 是否找到并吊销了记录；id 缺失或未命中时为 false。
    pub revoked: bool,
    /// 吊销后的客户端视图；未命中时为 None（序列化省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<PublicClient>,
}

/// `purgeRevokedClients` result.
/// `purgeRevokedClients` 的返回。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    /// 本次物理删除的已吊销记录数。
    pub purged: usize,
}

/// `authenticateBearerToken` result: `{ ok, clientId, sessionToken, client }`.
/// `authenticateBearerToken` 成功的返回 `{ ok, clientId, sessionToken,
/// client }`。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedClient {
    /// 恒为 true；失败路径直接返回 None。
    pub ok: bool,
    /// 客户端 id。
    pub client_id: String,
    /// 会话令牌；当前实现即客户端 id。
    pub session_token: String,
    /// 认证通过的客户端公开视图。
    pub client: PublicClient,
}

/// 受信设备注册表运行时（JS `createRemoteClientAuthRuntime` 的返回
/// 对象）：持有 store 路径、可注入时钟与串行化互斥锁。
pub struct RemoteClientAuth {
    /// 注册表 JSON 文件路径（<data_dir>/remote-clients.json）。
    store_path: PathBuf,
    /// 可注入墙钟（unix 毫秒）；测试用它固定时间。
    clock: Clock,
    /// JS `storeMutationQueue`: serializes every read-modify-write.
    /// 串行化所有读-改-写，防止并发认证与吊销交错写盘。
    mutation_lock: tokio::sync::Mutex<()>,
}

/// 注册表的全部操作；每个方法先取 mutation_lock 再读改写 store 文件。
impl RemoteClientAuth {
    /// 构造运行时；store 文件延迟到首次读写时才创建。
    pub fn new(store_path: PathBuf, clock: Clock) -> Self {
        Self {
            store_path,
            clock,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// JS `listClients`.
    /// JS `listClients`：读取归一后的 store，返回全部客户端（含已
    /// 吊销）的公开视图。
    pub async fn list_clients(&self) -> AppResult<Vec<PublicClient>> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store.clients.iter().map(public_client).collect())
    }

    /// Relay-transport demand from paired devices: any non-revoked,
    /// non-expired client paired over the relay OR observed connecting through
    /// the relay tunnel (`lastTransport`). The observed transport is the
    /// authoritative signal.
    /// relay 传输需求判断：存在任一未吊销、未过期，且（配对时声明
    /// 使用 relay 或最近经 relay 隧道连接，以 lastTransport 为准）的
    /// 客户端即返回 true；结果供隧道保活/关闭决策使用。
    pub async fn has_active_relay_clients(&self) -> AppResult<bool> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        let now = (self.clock)();
        Ok(store.clients.iter().any(|client| {
            if !client.uses_relay && client.last_transport.as_deref() != Some("relay") {
                return false;
            }
            if client.revoked_at.is_some() {
                return false;
            }
            match client.expires_at.as_deref().and_then(parse_iso_ms) {
                Some(expires) => expires > now,
                None => true,
            }
        }))
    }

    /// JS `createClient`. A dedupe-keyed mint replaces the previous record for
    /// the same device; an explicit label wins, otherwise the replaced
    /// record's label is kept, and only a first-ever mint falls back to the
    /// client-reported default.
    /// JS `createClient`：签发新令牌（明文只随本次返回一次），落盘
    /// SHA-256 哈希。带去重键的签发会替换同 key 旧记录；label 优先取
    /// 显式值，其次继承被替换记录，只有首次签发才回退到客户端上报的
    /// 默认名。
    pub async fn create_client(&self, input: CreateClientInput) -> AppResult<CreatedClient> {
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let normalized_dedupe_key = normalize_optional_string(input.dedupe_key.as_deref());
        let token = generate_token();
        let existing = normalized_dedupe_key.as_ref().and_then(|key| {
            store
                .clients
                .iter()
                .find(|entry| entry.dedupe_key.as_deref() == Some(key.as_str()))
        });
        let effective_label = normalize_optional_string(input.label.as_deref())
            .or_else(|| existing.map(|entry| entry.label.clone()))
            .or(input.fallback_label.clone());
        let client = StoredClient {
            id: generate_id(),
            label: normalize_label(effective_label.as_deref()),
            token_hash: sha256_hex(token.as_bytes()),
            created_at: now_iso(self.clock.clone()),
            last_used_at: None,
            revoked_at: None,
            expires_at: normalize_timestamp(input.expires_at.as_deref()),
            client_kind: normalize_optional_string(input.client_kind.as_deref()),
            dedupe_key: normalized_dedupe_key,
            auth_method: normalize_optional_string(input.auth_method.as_deref()),
            pairing_id: normalize_optional_string(input.pairing_id.as_deref()),
            device_name: normalize_optional_string(input.device_name.as_deref()),
            device_platform: normalize_optional_string(input.device_platform.as_deref()),
            device_model: normalize_optional_string(input.device_model.as_deref()),
            app_version: normalize_optional_string(input.app_version.as_deref()),
            uses_relay: input.uses_relay,
            last_transport: None,
        };
        if let Some(key) = &client.dedupe_key {
            store
                .clients
                .retain(|entry| entry.dedupe_key.as_deref() != Some(key.as_str()));
            // Migrate pre-clientKind desktop tokens: a deduped, kind-tagged
            // mint supersedes legacy records with the same label that carry
            // neither a kind nor a dedupe key.
            if client.client_kind.is_some() {
                let label = client.label.clone();
                store.clients.retain(|entry| {
                    !(entry.label == label
                        && entry.client_kind.is_none()
                        && entry.dedupe_key.is_none())
                });
            }
        }
        let client_public = public_client(&client);
        store.clients.push(client);
        self.write_store(&store).await?;
        Ok(CreatedClient {
            client: client_public,
            token,
        })
    }

    /// JS `revokeClient`.
    /// JS `revokeClient`：按 id 幂等吊销（已有 revokedAt 保持原值）；
    /// id 缺失或未命中返回 revoked=false 且不写盘。
    pub async fn revoke_client(&self, id: Option<&str>) -> AppResult<RevokeResult> {
        let id = normalize_optional_string(id);
        let Some(id) = id else {
            return Ok(RevokeResult {
                revoked: false,
                client: None,
            });
        };
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let Some(client) = store.clients.iter_mut().find(|entry| entry.id == id) else {
            return Ok(RevokeResult {
                revoked: false,
                client: None,
            });
        };
        if client.revoked_at.is_none() {
            client.revoked_at = Some(now_iso(self.clock.clone()));
        }
        let public = public_client(client);
        self.write_store(&store).await?;
        Ok(RevokeResult {
            revoked: true,
            client: Some(public),
        })
    }

    /// JS `purgeRevokedClients`.
    /// JS `purgeRevokedClients`：物理删除全部已吊销记录；只在确有
    /// 删除时写盘。
    pub async fn purge_revoked_clients(&self) -> AppResult<PurgeResult> {
        let _guard = self.mutation_lock.lock().await;
        let mut store = self.read_store().await?;
        let before = store.clients.len();
        store.clients.retain(|entry| entry.revoked_at.is_none());
        let purged = before - store.clients.len();
        if purged > 0 {
            self.write_store(&store).await?;
        }
        Ok(PurgeResult { purged })
    }

    /// JS `authenticateBearerToken(token, req)`. `transport` mirrors the
    /// presence of the `x-ompchamber-relay-connection` request header: relay
    /// requests self-heal the sticky `usesRelay` flag and are recorded as
    /// relay demand.
    /// JS `authenticateBearerToken(token, req)`：校验 bearer token。前缀
    /// 不符、无可匹配的未吊销哈希、或已过期都返回 None。transport 对应
    /// `x-ompchamber-relay-connection` 头的有无：relay 请求会自愈粘性的
    /// usesRelay 标志并计入 relay 需求；lastUsedAt 按写间隔节流，但传输
    /// 通道变化时立即落盘，使 LAN⇄relay 切换即时可见。
    pub async fn authenticate_bearer_token(
        &self,
        token: &str,
        transport: Transport,
    ) -> AppResult<Option<AuthenticatedClient>> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let _guard = self.mutation_lock.lock().await;
        let token_hash = sha256_hex(token.as_bytes());
        let mut store = self.read_store().await?;
        let now = (self.clock)();
        let Some(client) = store.clients.iter_mut().find(|entry| {
            entry.revoked_at.is_none() && constant_time_equal_hex(&entry.token_hash, &token_hash)
        }) else {
            return Ok(None);
        };
        if let Some(expires) = client.expires_at.as_deref().and_then(parse_iso_ms)
            && expires <= now
        {
            return Ok(None);
        }
        let last_used_at = client.last_used_at.as_deref().and_then(parse_iso_ms);
        // Self-heal the paired-over-relay flag from the authoritative signal;
        // sticky on purpose.
        let heal_uses_relay = transport == Transport::Relay && !client.uses_relay;
        if heal_uses_relay {
            client.uses_relay = true;
        }
        // Write on the throttle interval — or immediately when the transport
        // changed, so a LAN⇄relay switch is visible right away.
        let last_used_due = match last_used_at {
            Some(last) => now - last >= LAST_USED_WRITE_INTERVAL_MS,
            None => true,
        };
        let should_write = heal_uses_relay
            || last_used_due
            || client.last_transport.as_deref() != Some(transport.as_str());
        if should_write {
            client.last_used_at = Some(iso_utc_from_unix_millis(now));
            client.last_transport = Some(transport.as_str().to_string());
        }
        let public = public_client(client);
        if should_write {
            self.write_store(&store).await?;
        }
        Ok(Some(AuthenticatedClient {
            ok: true,
            session_token: public.id.clone(),
            client_id: public.id.clone(),
            client: public,
        }))
    }

    /// Boolean validity check for consumers that only need the gate answer
    /// (the JS bearer branch of `authenticateClientRequest`). Requests that
    /// arrived through the relay tunnel should call
    /// [`RemoteClientAuth::authenticate_bearer_token`] with
    /// [`Transport::Relay`] instead so transport healing and last-used
    /// tracking fire.
    /// 只需要布尔结果的调用方入口（JS `authenticateClientRequest` 的
    /// bearer 分支），以 Direct 传输执行完整认证；经 relay 到达的请求
    /// 应改调 authenticate_bearer_token 并传 Transport::Relay。
    pub async fn is_valid_client_token(&self, token: &str) -> bool {
        self.authenticate_bearer_token(token, Transport::Direct)
            .await
            .unwrap_or(None)
            .is_some()
    }

    /// JS `readStore`: ENOENT or unparseable JSON both yield the empty store.
    /// JS `readStore`：ENOENT 或 JSON 解析失败都归一为空 store，认证
    /// 路径不因坏文件而报错。
    async fn read_store(&self) -> AppResult<StoredRemoteClientStore> {
        let raw = match tokio::fs::read_to_string(&self.store_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(normalize_store(&Value::Null, &self.clock));
            }
            Err(err) => return Err(err.into()),
        };
        let payload = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
        Ok(normalize_store(&payload, &self.clock))
    }

    /// JS `writeStore`: mkdir -p (0o700), pretty JSON, 0o600 file.
    /// JS `writeStore`：先经 normalize_store 归一，再以 pretty JSON 与
    /// 0600 权限写入（见 write_file_private）。
    async fn write_store(&self, store: &StoredRemoteClientStore) -> AppResult<()> {
        let normalized = normalize_store(
            &serde_json::to_value(store)
                .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?,
            &self.clock,
        );
        let body = serde_json::to_string_pretty(&normalized)
            .map_err(|err| AppError::internal(format!("serialize failed: {err}")))?;
        write_file_private(&self.store_path, body.as_bytes()).await
    }
}

/// JS `normalizeStore` applied to a raw JSON payload.
/// JS `normalizeStore`：把任意 JSON 载荷归一为合法 store——补齐
/// id/createdAt、清洗 label/timestamp、过滤非法 lastTransport，丢弃缺
/// tokenHash 的记录。
pub(crate) fn normalize_store(payload: &Value, clock: &Clock) -> StoredRemoteClientStore {
    let now_iso = || now_iso(clock.clone());
    let mut clients = Vec::new();
    if let Some(raw_clients) = payload.get("clients").and_then(Value::as_array) {
        for client in raw_clients.iter().filter(|c| c.is_object()) {
            let normalized = StoredClient {
                id: match client.get("id").and_then(Value::as_str) {
                    Some(id) => id.to_string(),
                    None => generate_id(),
                },
                label: normalize_label(client.get("label").and_then(Value::as_str)),
                token_hash: client
                    .get("tokenHash")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                created_at: match client.get("createdAt").and_then(Value::as_str) {
                    Some(at) => at.to_string(),
                    None => now_iso(),
                },
                last_used_at: string_or_none(client.get("lastUsedAt")),
                revoked_at: string_or_none(client.get("revokedAt")),
                expires_at: normalize_timestamp(client.get("expiresAt").and_then(Value::as_str)),
                client_kind: normalize_optional_string(
                    client.get("clientKind").and_then(Value::as_str),
                ),
                dedupe_key: normalize_optional_string(
                    client.get("dedupeKey").and_then(Value::as_str),
                ),
                uses_relay: client.get("usesRelay") == Some(&Value::Bool(true)),
                last_transport: client
                    .get("lastTransport")
                    .and_then(Value::as_str)
                    .filter(|transport| matches!(*transport, "relay" | "direct"))
                    .map(str::to_string),
                auth_method: normalize_optional_string(
                    client.get("authMethod").and_then(Value::as_str),
                ),
                pairing_id: normalize_optional_string(
                    client.get("pairingId").and_then(Value::as_str),
                ),
                device_name: normalize_optional_string(
                    client.get("deviceName").and_then(Value::as_str),
                ),
                device_platform: normalize_optional_string(
                    client.get("devicePlatform").and_then(Value::as_str),
                ),
                device_model: normalize_optional_string(
                    client.get("deviceModel").and_then(Value::as_str),
                ),
                app_version: normalize_optional_string(
                    client.get("appVersion").and_then(Value::as_str),
                ),
            };
            if !normalized.token_hash.is_empty() {
                clients.push(normalized);
            }
        }
    }
    StoredRemoteClientStore {
        version: STORE_VERSION,
        clients,
    }
}

/// StoredClient 到 PublicClient 的投影：剥离 tokenHash 等内部/敏感字段。
fn public_client(client: &StoredClient) -> PublicClient {
    PublicClient {
        id: client.id.clone(),
        label: client.label.clone(),
        created_at: client.created_at.clone(),
        last_used_at: client.last_used_at.clone(),
        revoked_at: client.revoked_at.clone(),
        expires_at: client.expires_at.clone(),
        client_kind: client.client_kind.clone(),
        auth_method: client.auth_method.clone(),
        pairing_id: client.pairing_id.clone(),
        device_name: client.device_name.clone(),
        device_platform: client.device_platform.clone(),
        device_model: client.device_model.clone(),
        app_version: client.app_version.clone(),
        uses_relay: client.uses_relay,
        last_transport: client.last_transport.clone(),
    }
}

/// 用注入时钟生成当前时刻的 ISO-8601 UTC 字符串。
pub(crate) fn now_iso(clock: Clock) -> String {
    iso_utc_from_unix_millis(clock())
}

/// JS `normalizeLabel`: trimmed, capped, defaulted to "Remote client".
/// JS `normalizeLabel`：trim、按字符数截断到上限、空值回退
/// "Remote client"。
pub(crate) fn normalize_label(value: Option<&str>) -> String {
    match normalize_optional_string(value) {
        Some(trimmed) if trimmed.chars().count() > MAX_LABEL_LENGTH => {
            trimmed.chars().take(MAX_LABEL_LENGTH).collect()
        }
        Some(trimmed) => trimmed,
        None => "Remote client".to_string(),
    }
}

/// JS `normalizeTimestamp`: parse and re-emit canonical ISO, or None.
/// JS `normalizeTimestamp`：解析后重新输出 canonical ISO-8601；解析
/// 失败返回 None。
pub(crate) fn normalize_timestamp(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    parse_iso_ms(&normalized).map(iso_utc_from_unix_millis)
}

/// JS `normalizeOptionalString`.
/// JS `normalizeOptionalString`：trim 后非空返回 Some，否则 None。
pub(crate) fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 从 JSON 值取字符串字段；缺失或非字符串返回 None。
fn string_or_none(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

/// 生成 12 字节随机 hex 作为客户端 id。
fn generate_id() -> String {
    random_hex(12)
}

/// 生成 `oc_client_` 前缀 + 32 字节 base64url 随机串的明文令牌。
fn generate_token() -> String {
    format!("{TOKEN_PREFIX}{}", random_base64url(TOKEN_BYTES))
}

/// JS `writeStore` file handling: recursive parent (0o700), write (0o600),
/// best-effort chmod. Plain write, like the JS `fsPromises.writeFile`.
/// JS `writeStore` 的文件处理：递归建父目录（0700）、以 0600 写入并
/// 尽力补 chmod；普通覆写（等价 `fsPromises.writeFile`），非原子
/// rename。
pub(crate) async fn write_file_private(path: &std::path::Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await?;
        file.write_all(bytes).await?;
        // tokio buffers writes; flush so same-process readers never see a
        // truncated file.
        file.flush().await?;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(path, bytes).await?;
    }
    Ok(())
}
