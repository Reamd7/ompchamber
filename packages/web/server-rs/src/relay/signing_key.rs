//! Port of `server/lib/relay/signing-key.js` — per-server relay signing
//! identity (ECDSA P-256), shared with the push relay so both derive the SAME
//! serverId (`base64url(SHA-256(canonical public JWK))`).
//!
//! Storage format is unchanged: `settings.relaySigningKey = { privateJwk,
//! publicJwk }` — existing installs' serverId must stay stable because push
//! token binding depends on it. JWK field sets mirror Node's
//! `KeyObject.export({ format: 'jwk' })`: public `{ kty, x, y, crv }`, private
//! `{ kty, x, y, crv, d }` (base64url, unpadded — WebCrypto compatible).
//!
//! Signature form: ECDSA-SHA256, IEEE P1363 (raw `r||s`), base64url — the form
//! WebCrypto (and therefore the relay worker) verifies.
//!
//! 中文概述：管理每个实例的 relay 签名身份（ECDSA P-256）。serverId
//! 由公钥 JWK 的 canonical 串做 SHA-256 派生，同一密钥同时被 push
//! relay 信任；密钥持久化在 settings.relaySigningKey，因此 serverId
//! 对既有安装保持稳定。首次生成新密钥会打 warn 日志——serverId 变更
//! 意味着所有已配对设备需要重新配对。

use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::AppResult;
use crate::settings::SettingsStore;

use super::e2ee::{base64_url_to_bytes, bytes_to_base64_url};

/// settings 中存储签名密钥对的键名（`{ privateJwk, publicJwk }`）。
pub const SETTINGS_KEY_SIGNING: &str = "relaySigningKey";

/// 已导入/生成的签名密钥对：P-256 私钥 + 对应公钥 JWK。
pub struct RelaySigningKeypair {
    /// ECDSA 签名私钥（p256 原生类型），用于签发 relay 鉴权消息。
    pub signing_key: SigningKey,
    /// 公钥 JWK（`{ kty, x, y, crv }`），派生 serverId 的哈希输入。
    pub public_jwk: Value,
}

/// 把 base64url 的 JWK `d` 字段解码为 32 字节私钥标量；长度不恰好是
/// 32 字节时返回 None。
fn decode_scalar(d: &str) -> Option<[u8; 32]> {
    let bytes = base64_url_to_bytes(d).ok()?;
    bytes.as_slice().try_into().ok()
}

/// 从 settings 值中同时取出 privateJwk 与 publicJwk 两个对象；任一
/// 缺失或不是对象即返回 None。
fn jwk_pair(
    value: Option<&Value>,
) -> Option<(
    &serde_json::Map<String, Value>,
    &serde_json::Map<String, Value>,
)> {
    let object = value?.as_object()?;
    let private = object.get("privateJwk")?.as_object()?;
    let public = object.get("publicJwk")?.as_object()?;
    Some((private, public))
}

/// 从存储的 settings 值恢复签名密钥对：读取 `d` 标量并重建 SigningKey。
/// 任何字段缺失、格式非法或标量不在曲线上都返回 None，由调用方决定
/// 是否重新生成。
pub fn import_signing_keypair(stored: &Value) -> Option<RelaySigningKeypair> {
    let (private_jwk, public_jwk) = jwk_pair(Some(stored))?;
    let d = private_jwk.get("d")?.as_str()?;
    let scalar = decode_scalar(d)?;
    let signing_key = SigningKey::from_slice(&scalar).ok()?;
    Some(RelaySigningKeypair {
        signing_key,
        public_jwk: Value::Object(public_jwk.clone()),
    })
}

/// JS `getOrCreateRelaySigningKeypair`. The strict-reader regeneration gate is
/// preserved: a lenient read that maps a corrupt file to `{}` must never mint a
/// replacement identity (a new serverId orphans every paired device and push
/// binding); re-verify with the strict reader first and propagate its errors.
///
/// 中文补充：读取顺序为「宽松读 → 严格读 → 生成」；只有两级读取都拿
/// 不到合法密钥才生成新密钥，写回前先合并 strict 读到的全部键值，
/// 避免覆盖其它设置。生成路径必须只走一次，否则 serverId 会漂移。
pub async fn get_or_create_relay_signing_keypair(
    store: &SettingsStore,
) -> AppResult<RelaySigningKeypair> {
    let settings = store.read_migrated().await?;
    if let Some(existing) = settings
        .get(SETTINGS_KEY_SIGNING)
        .and_then(import_signing_keypair)
    {
        return Ok(existing);
    }
    let verified_settings = store.read_strict().await?;
    if let Some(verified) = verified_settings
        .get(SETTINGS_KEY_SIGNING)
        .and_then(import_signing_keypair)
    {
        return Ok(verified);
    }
    // Loud on purpose: a new signing key means a new serverId — every
    // previously paired device and push binding is orphaned. Expected exactly
    // once, on first run.
    tracing::warn!(
        "[relay-identity] Generating NEW relay signing keypair (serverId changes; previously paired devices must re-pair)"
    );
    let signing_key = generate_signing_key();
    let private_jwk = private_signing_jwk(&signing_key);
    let public_jwk = public_signing_jwk(&signing_key);
    let mut merged = settings;
    for (key, value) in verified_settings {
        merged.insert(key, value);
    }
    merged.insert(
        SETTINGS_KEY_SIGNING.to_string(),
        json!({ "privateJwk": private_jwk, "publicJwk": public_jwk }),
    );
    store.write_raw(&merged).await?;
    Ok(RelaySigningKeypair {
        signing_key,
        public_jwk,
    })
}

/// 用系统随机源生成 P-256 签名私钥；拒绝非法标量并重试（概率极低，
/// 循环通常一次退出）。
fn generate_signing_key() -> SigningKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        if let Ok(key) = SigningKey::from_slice(&bytes) {
            return key;
        }
    }
}

/// 从私钥导出公钥 JWK：未压缩点坐标 x/y 以 base64url（无填充）编码，
/// 字段集合与 Node `KeyObject.export({ format: 'jwk' })` 一致。
fn public_signing_jwk(signing_key: &SigningKey) -> Value {
    let point = VerifyingKey::from(signing_key).to_encoded_point(false);
    let bytes = point.as_bytes();
    json!({
        "kty": "EC",
        "x": bytes_to_base64_url(&bytes[1..33]),
        "y": bytes_to_base64_url(&bytes[33..65]),
        "crv": "P-256",
    })
}

/// 在公钥 JWK 基础上追加私钥标量 `d` 字段，得到完整私钥 JWK
/// （`{ kty, x, y, crv, d }`，WebCrypto 兼容）。
fn private_signing_jwk(signing_key: &SigningKey) -> Value {
    let mut jwk = public_signing_jwk(signing_key);
    if let Some(object) = jwk.as_object_mut() {
        object.insert(
            "d".to_string(),
            Value::String(bytes_to_base64_url(&signing_key.to_bytes())),
        );
    }
    jwk
}

/// Fixed key order so the hash is stable regardless of stored JSON field order.
/// Byte-for-byte mirror of `canonicalJwk` in the ompchamber-website relay-auth.
///
/// 中文补充：缺失字段以 null 参与序列化，保证同一密钥在任何存储 JSON
/// 字段顺序下哈希输入完全一致。
pub fn canonical_public_jwk_string(jwk: &Value) -> String {
    let field = |name: &str| jwk.get(name).cloned().unwrap_or(Value::Null);
    let mut parts = Vec::with_capacity(4);
    for (name, value) in [
        ("crv", field("crv")),
        ("kty", field("kty")),
        ("x", field("x")),
        ("y", field("y")),
    ] {
        parts.push(format!(
            "{}:{}",
            serde_json::to_string(name).unwrap_or_default(),
            value
        ));
    }
    format!("{{{}}}", parts.join(","))
}

/// `serverId = base64url(SHA-256(canonical public JWK))`. Must match the push
/// relay's `deriveServerId` — this id is the routing key for both relays.
///
/// 中文补充：serverId 是两套 relay 的路由键，哈希输入的任何变化都会
/// 让设备配对与推送绑定失效。
pub fn derive_server_id(public_jwk: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_public_jwk_string(public_jwk).as_bytes());
    bytes_to_base64_url(&hasher.finalize())
}

/// ECDSA-SHA256, IEEE P1363 (raw r||s) signature — the form WebCrypto verifies.
///
/// 中文补充：对 message 的 UTF-8 字节签名，输出 base64url 字符串，
/// 与 WebCrypto `subtle.verify` 期望的签名格式一致。
pub fn sign_relay_message(signing_key: &SigningKey, message: &str) -> String {
    use p256::ecdsa::signature::Signer;
    let signature: Signature = signing_key.sign(message.as_bytes());
    bytes_to_base64_url(&signature.to_bytes())
}

/// Verify helper (test surface; the relay worker verifies in production).
///
/// 中文补充：base64url 解码或签名解析失败一律返回 false，不区分错误
/// 原因（与 JS 测试面的布尔语义一致）。
pub fn verify_relay_message(
    verifying_key: &VerifyingKey,
    message: &str,
    signature_b64: &str,
) -> bool {
    use p256::ecdsa::signature::Verifier;
    let Some(raw) = base64_url_to_bytes(signature_b64).ok() else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&raw) else {
        return false;
    };
    verifying_key.verify(message.as_bytes(), &signature).is_ok()
}
