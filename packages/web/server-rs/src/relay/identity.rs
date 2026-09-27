//! Port of `server/lib/relay/identity.js` — host relay identity.
//!
//! The EXISTING ECDSA P-256 signing keypair (shared with the push relay via
//! `signing_key.rs` — same storage, same serverId) plus a NEW long-lived ECDH
//! P-256 encryption keypair for the E2EE channel (signing and encryption keys
//! must differ). The encryption keypair is persisted as
//! `settings.relayEncryptionKey = { privateJwk, publicJwk }`, mirroring the
//! `relaySigningKey` precedent; the private JWK field set matches WebCrypto's
//! `subtle.exportKey('jwk', ...)` shape so both runtimes read each other's
//! settings.
//!
//! 中文概述：组装 host 侧 relay 身份——签名密钥对（serverId 的来源）
//! 加上独立的 ECDH 加密密钥对（E2EE 信任锚）。`RelayIdentityRuntime`
//! 负责首次生成/导入并缓存结果；`sign_relay_auth` 闭包按需签发
//! relay 层鉴权三元组。

use std::sync::Arc;

use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::error::AppResult;
use crate::settings::SettingsStore;

use super::e2ee::{
    bytes_to_base64_url, export_public_key_jwk, generate_ecdh_key_pair, import_ecdh_private_key,
};
use super::signing_key::{
    canonical_public_jwk_string, derive_server_id, get_or_create_relay_signing_keypair,
    sign_relay_message,
};

/// settings 中存储 E2EE 加密密钥对的键名（`{ privateJwk, publicJwk }`）。
pub const SETTINGS_KEY_ENCRYPTION: &str = "relayEncryptionKey";

/// relay 层鉴权三元组（host-control / host-data 升级握手时上送）。
#[derive(Clone)]
pub struct RelayAuth {
    /// 签名时间戳（unix ms），参与签名消息的拼接。
    pub ts: i64,
    /// base64url 编码的 ECDSA 签名（IEEE P1363 裸 r||s）。
    pub sig: String,
    /// 签名者公钥指纹：canonical JWK 串字节的 base64url。
    pub pk: String,
}

/// 一次 relay 会话所需的完整 host 身份：serverId、E2EE 密钥对与
/// 鉴权签名闭包。
pub struct RelayIdentity {
    /// 由签名公钥派生的稳定服务器标识（两套 relay 共用的路由键）。
    pub server_id: String,
    /// E2EE 公钥 JWK，握手时发给对端用于派生会话密钥。
    pub host_enc_pub_jwk: Value,
    /// E2EE 私钥（P-256），仅本地参与密钥派生，绝不外发。
    pub host_enc_private_key: SecretKey,
    /// Relay-layer auth for host-control / host-data upgrades. Signature
    /// payload string is `${ts}.${serverId}.${role}.${connectionId ?? ""}`
    /// (spec Layer 1).
    ///
    /// 中文补充：闭包捕获签名私钥、serverId 与时钟，每次调用生成新的
    /// ts 并签名，直接返回可上送的 RelayAuth。
    pub sign_relay_auth: Arc<dyn Fn(&str, Option<&str>) -> RelayAuth + Send + Sync>,
}

/// Current wall-clock milliseconds (JS `Date.now()`); injectable for tests.
///
/// 中文补充：签名时间戳取自该时钟，测试注入固定时钟即可复现签名。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 生产时钟：SystemTime 换算的 unix 毫秒（早于 epoch 时取 0）。
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// 判断存储值是否形如 `{ privateJwk, publicJwk }` 且两个成员都存在、
/// 非 null（对应 JS 的真值检查语义，仅做形状检查不验证密钥本身）。
fn is_jwk_pair(value: Option<&Value>) -> bool {
    let Some(object) = value.and_then(Value::as_object) else {
        return false;
    };
    // JS truthiness: both members must be present, non-null objects.
    object.get("privateJwk").is_some_and(|v| !v.is_null())
        && object.get("publicJwk").is_some_and(|v| !v.is_null())
}

/// 读取或生成 E2EE 加密密钥对，返回存储形状的 JSON。与签名密钥相同的
/// 防误生成闸门：宽松读失败后必须用严格读复核，两级都拿不到才生成
/// 新密钥并合并写回——避免一次读失败就换掉所有已配对设备信任的
/// E2EE 锚点。私有 JWK 字段集合与 WebCrypto `subtle.exportKey('jwk')`
/// 一致，保证两种运行时可互读。
async fn get_or_create_encryption_keypair(store: &SettingsStore) -> AppResult<Value> {
    let settings = store.read_migrated().await?;
    if let Some(existing) = settings
        .get(SETTINGS_KEY_ENCRYPTION)
        .filter(|v| is_jwk_pair(Some(*v)))
    {
        return Ok(existing.clone());
    }
    // Same regeneration gate as the signing key: never mint a replacement
    // identity key off a swallowed read failure — a new encryption key breaks
    // the E2EE trust anchor pinned by every paired device.
    let verified_settings = store.read_strict().await?;
    if let Some(verified) = verified_settings
        .get(SETTINGS_KEY_ENCRYPTION)
        .filter(|v| is_jwk_pair(Some(*v)))
    {
        return Ok(verified.clone());
    }
    // Loud on purpose: a new encryption key invalidates the E2EE trust anchor
    // of every paired device. Expected exactly once, on first relay use.
    tracing::warn!(
        "[relay-identity] Generating NEW relay encryption keypair (E2EE trust anchor changes; previously paired devices must re-pair)"
    );
    let (secret, public) =
        generate_ecdh_key_pair().map_err(|e| crate::error::AppError::internal(e.0))?;
    let point = public.to_encoded_point(false).as_bytes().to_vec();
    let private_jwk = json!({
        "key_ops": ["deriveBits"],
        "ext": true,
        "kty": "EC",
        "x": bytes_to_base64_url(&point[1..33]),
        "y": bytes_to_base64_url(&point[33..65]),
        "crv": "P-256",
        "d": bytes_to_base64_url(&secret.to_bytes()),
    });
    let public_jwk = export_public_key_jwk(&public);
    let stored = json!({ "privateJwk": private_jwk, "publicJwk": public_jwk });
    let mut merged = settings;
    for (key, value) in verified_settings {
        merged.insert(key, value);
    }
    merged.insert(SETTINGS_KEY_ENCRYPTION.to_string(), stored.clone());
    store.write_raw(&merged).await?;
    Ok(stored)
}

/// 身份运行时：持有 settings store 与时钟，缓存首次组装好的身份。
pub struct RelayIdentityRuntime {
    /// settings 存储（读写 relayEncryptionKey / relaySigningKey）。
    store: Arc<SettingsStore>,
    /// 可注入时钟，签名时间戳来源。
    clock: Clock,
    /// 身份缓存；None 表示尚未组装过（此后每次命中都跳过全部 I/O）。
    cached: Mutex<Option<Arc<RelayIdentity>>>,
}

/// 身份的构造与缓存访问。
impl RelayIdentityRuntime {
    /// 以给定的 store 与时钟构造（缓存初始为空）。
    pub fn new(store: Arc<SettingsStore>, clock: Clock) -> Self {
        Self {
            store,
            clock,
            cached: Mutex::new(None),
        }
    }

    /// 获取（必要时组装并缓存）relay 身份：先签名密钥对 → serverId，
    /// 再加密密钥对，同时构造签名闭包。任何 settings 读写或密钥导入
    /// 失败都会向上返回错误且不写缓存。
    pub async fn get_relay_identity(&self) -> AppResult<Arc<RelayIdentity>> {
        if let Some(cached) = self.cached.lock().await.clone() {
            return Ok(cached);
        }
        let signing = get_or_create_relay_signing_keypair(&self.store).await?;
        let server_id = derive_server_id(&signing.public_jwk);
        let encryption = get_or_create_encryption_keypair(&self.store).await?;
        let host_enc_private_key =
            import_ecdh_private_key(encryption.get("privateJwk").unwrap_or(&Value::Null))
                .map_err(|e| crate::error::AppError::internal(e.0))?;
        let host_enc_pub_jwk = encryption.get("publicJwk").cloned().ok_or_else(|| {
            crate::error::AppError::internal("missing relay encryption public jwk")
        })?;
        let pk = bytes_to_base64_url(canonical_public_jwk_string(&signing.public_jwk).as_bytes());
        let signing_key = signing.signing_key.clone();
        let server_id_for_auth = server_id.clone();
        let clock = self.clock.clone();
        let sign_relay_auth: Arc<dyn Fn(&str, Option<&str>) -> RelayAuth + Send + Sync> =
            Arc::new(move |role, connection_id| {
                let ts = clock();
                let message = format!(
                    "{ts}.{server_id_for_auth}.{role}.{}",
                    connection_id.unwrap_or("")
                );
                RelayAuth {
                    ts,
                    sig: sign_relay_message(&signing_key, &message),
                    pk: pk.clone(),
                }
            });
        let identity = Arc::new(RelayIdentity {
            server_id,
            host_enc_pub_jwk,
            host_enc_private_key,
            sign_relay_auth,
        });
        *self.cached.lock().await = Some(identity.clone());
        Ok(identity)
    }
}
