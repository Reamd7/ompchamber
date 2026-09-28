//! Web-push + APNs crypto, hand-rolled on the staged crates (p256, sha2,
//! hkdf, aes-gcm, base64) per RFC 8291 (aes128gcm payload encryption), RFC
//! 8292 (VAPID ES256 JWT), and the APNs provider-token JWT (ES256).
//!
//! Also hosts the shared relay signing identity primitives ported from
//! `server/lib/relay/signing-key.js` (ECDSA P-256 P1363 signatures, the
//! canonical public JWK string, and `serverId = base64url(SHA-256(jwk))`)
//! so the (still-pending) relay module can consume them without a second
//! implementation.
//!
//! Secret handling: private scalars are only ever written to the settings
//! store under `vapidKeys` / `relaySigningKey` (the JS's own persistence
//! files); nothing here logs key material.
//!
//! 中文概述：本模块手工实现 Web-push 与 APNs 所需的全部密码学原语：
//! - RFC 8291 的 aes128gcm 载荷加密（ECDH P-256 + HKDF-SHA256 +
//!   AES-128-GCM，单记录模式）；
//! - RFC 8292 的 VAPID ES256 JWT Authorization 头；
//! - APNs provider token 的 ES256 JWT 签名；
//! - Apple `.p8` 私钥（PEM/DER）中 P-256 标量的提取；
//! - relay 共享签名身份原语（P1363 签名、规范 JWK 字符串、serverId 派生），
//!   供后续 relay 模块直接复用，避免出现第二份实现。
//!
//! 除随机数与系统时间外全部为纯计算，无 I/O 副作用；解析或解密失败
//! 一律返回 `Option::None` 而非 panic（HKDF 固定长度断言是唯一例外）。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::ecdh::SharedSecret;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// RFC 8291 §4: one record only, so `rs` must exceed plaintext + delimiter
/// + tag. 4096 is the value used by the reference implementations (and the
/// RFC's own example).
/// 中文：aes128gcm 头部的 rs（record size）字段值。单记录加密要求
/// rs > 明文 + 0x02 定界符 + 16 字节 GCM tag，4096 与参考实现一致。
const WEB_PUSH_RECORD_SIZE: u32 = 4096;
/// Node `web-push` default TTL (28 days) sent on every notification.
/// 中文：每次 web-push 发送携带的 TTL（Node web-push 默认的 28 天），
/// 超期后推送服务可安全丢弃消息；公开常量，push_runtime 组装请求头时使用。
pub const WEB_PUSH_TTL_SECONDS: u64 = 2419200;
/// Node `web-push` VAPID JWT lifetime: 12 hours.
/// 中文：VAPID JWT 有效期（12 小时），exp = 当前秒级时间 + 该值。
const VAPID_JWT_TTL_SECONDS: u64 = 12 * 60 * 60;

/// 当前 Unix 毫秒时间戳；时钟早于 epoch 时返回 0（安全回退而非 panic）。
/// VAPID exp 与测试目录命名均以此为基准。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// base64url（URL-safe 字母表、无填充）编码：VAPID 密钥、JWK 坐标、
/// JWT 段与 serverId 共用的线上/存储格式。
pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url 无填充解码；非法输入返回 None，调用方据此判定密钥串损坏。
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// 用系统 CSPRNG 生成 `len` 字节随机数：加密 salt、临时密钥与测试
/// auth secret 的来源。
fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Random P-256 secret key (rejection sampling on fresh random bytes; the
/// invalid-range probability is ~2^-32 and retried).
/// 中文：循环拒绝采样直到 32 字节落在 P-256 标量合法区间；用于生成
/// VAPID 密钥对、每次发送的临时应用服务器密钥以及测试 UA 密钥。
pub fn generate_secret_key() -> SecretKey {
    loop {
        let bytes = random_bytes(32);
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

/// 私钥标量（32 字节大端）转 base64url 字符串，即 VAPID privateKey
/// 的存储/传输格式。
pub fn secret_to_b64url(secret: &SecretKey) -> String {
    b64url_encode(secret.to_bytes().as_slice())
}

/// Uncompressed SEC1 point (65 bytes: 0x04 || X || Y) — the wire form used
/// by both VAPID keys and web-push subscription `p256dh` keys.
/// 中文：输出即浏览器 PushSubscription 的 `p256dh` 与 VAPID publicKey
/// 公用的线上格式，push_runtime 原样透传。
pub fn public_to_b64url(public: &PublicKey) -> String {
    b64url_encode(&public.to_sec1_bytes())
}

/// 从 base64url 私钥串恢复 SecretKey；长度或标量区间不合法返回 None。
pub fn secret_from_b64url(text: &str) -> Option<SecretKey> {
    SecretKey::from_slice(&b64url_decode(text)?).ok()
}

/// 从 base64url 公钥串（65 字节非压缩 SEC1）恢复 PublicKey；格式错误
/// 返回 None。
pub fn public_from_b64url(text: &str) -> Option<PublicKey> {
    PublicKey::from_sec1_bytes(&b64url_decode(text)?).ok()
}

// ---------------------------------------------------------------------------
// Relay signing identity (relay/signing-key.js)
// ---------------------------------------------------------------------------

/// Public JWK in the Node/WebCrypto shape: `{kty, crv, x, y}` (EC P-256,
/// base64url coordinates). Insertion order is irrelevant — serde_json maps
/// serialize alphabetically, which for these four keys is exactly the
/// canonical order below.
/// 中文：输出直接写入 settings 并可被 relay Worker / JS 端读取，
/// 字段集合与顺序与 Node WebCrypto 导出的 EC JWK 完全兼容。
pub fn public_jwk_value(public: &PublicKey) -> serde_json::Value {
    let encoded = public.to_encoded_point(false);
    serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "x": b64url_encode(encoded.x().map(|x| x.as_slice()).unwrap_or_default()),
        "y": b64url_encode(encoded.y().map(|y| y.as_slice()).unwrap_or_default()),
    })
}

/// Private JWK: public fields plus the `d` scalar (persisted under
/// `settings.relaySigningKey`; never logged).
/// 中文：在公钥 JWK 之上追加 `d`（base64url 私钥标量）；该对象只进
/// settings 持久化，任何日志路径都不得输出其内容。
pub fn private_jwk_value(secret: &SecretKey) -> serde_json::Value {
    let mut jwk = public_jwk_value(&secret_public_key(secret));
    if let serde_json::Value::Object(map) = &mut jwk {
        map.insert(
            "d".to_string(),
            serde_json::Value::String(secret_to_b64url(secret)),
        );
    }
    jwk
}

/// 由私钥推导对应公钥（P-256 公开点），避免调用方直接依赖 curve API。
pub fn secret_public_key(secret: &SecretKey) -> PublicKey {
    secret.public_key()
}

/// 从 JWK 重组 PublicKey：要求 `kty=EC` 且 `crv=P-256`，把 x/y 坐标
/// 拼成 `0x04||x||y` 后按 SEC1 解析；字段缺失、长度非 32 或点不合法
/// 均返回 None。
pub fn jwk_public_key(jwk: &serde_json::Value) -> Option<PublicKey> {
    if jwk.get("kty").and_then(|v| v.as_str()) != Some("EC") {
        return None;
    }
    if jwk.get("crv").and_then(|v| v.as_str()) != Some("P-256") {
        return None;
    }
    let x = b64url_decode(jwk.get("x")?.as_str()?)?;
    let y = b64url_decode(jwk.get("y")?.as_str()?)?;
    if x.len() != 32 || y.len() != 32 {
        return None;
    }
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    PublicKey::from_sec1_bytes(&sec1).ok()
}

/// 从 JWK 的 `d` 字段恢复 SecretKey；字段缺失或非合法标量返回 None。
pub fn jwk_secret_key(jwk: &serde_json::Value) -> Option<SecretKey> {
    let d = b64url_decode(jwk.get("d")?.as_str()?)?;
    SecretKey::from_slice(&d).ok()
}

/// Byte-for-byte mirror of `canonicalJwkString` (and the relay Worker's
/// `canonicalJwk`): fixed key order `crv,kty,x,y`.
/// 中文：逐字节复刻 JS 端拼接逻辑（缺字段输出空串），因为 serverId
/// 的哈希输入必须两端完全一致，不能依赖 serde 的序列化顺序。
pub fn canonical_public_jwk_string(jwk: &serde_json::Value) -> String {
    let field = |name: &str| jwk.get(name).and_then(|v| v.as_str()).unwrap_or("");
    format!(
        "{{\"crv\":{},\"kty\":{},\"x\":{},\"y\":{}}}",
        serde_json::to_string(field("crv")).unwrap_or_default(),
        serde_json::to_string(field("kty")).unwrap_or_default(),
        serde_json::to_string(field("x")).unwrap_or_default(),
        serde_json::to_string(field("y")).unwrap_or_default()
    )
}

/// `serverId = base64url(SHA-256(canonical public JWK))` — the routing key
/// for both relays. Self-certifying: the relay stores no secret.
/// 中文：对规范 JWK 字符串做 SHA-256 再 base64url（43 字符）；
/// 自证明性质意味着 relay 侧不需要保存任何密钥即可路由。
pub fn derive_server_id(public_jwk: &serde_json::Value) -> String {
    let digest = Sha256::digest(canonical_public_jwk_string(public_jwk).as_bytes());
    b64url_encode(&digest)
}

/// ECDSA-SHA256 with the IEEE P1363 (raw `r||s`, 64 bytes) encoding — the
/// form Node's `dsaEncoding: 'ieee-p1363'` produces and WebCrypto verifies.
/// 中文：底层走 p256 的确定性签名（RFC 6979），同一私钥同一消息
/// 签名结果确定，便于与官方测试向量逐位比对。
pub fn sign_p1363(secret: &SecretKey, message: &[u8]) -> Vec<u8> {
    let signing = SigningKey::from_bytes(&secret.to_bytes()).expect("valid secret scalar");
    let signature: Signature = signing.sign(message);
    signature.to_bytes().as_slice().to_vec()
}

/// 验证 P1363 裸签名（64 字节 `r||s`）；公钥或签名格式非法、验签失败
/// 一律返回 false，不向调用方传播错误。
pub fn verify_p1363(public: &PublicKey, message: &[u8], signature: &[u8]) -> bool {
    let Ok(verifying) = VerifyingKey::from_sec1_bytes(&public.to_sec1_bytes()) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    verifying.verify(message, &signature).is_ok()
}

/// ES256 JWT: `b64url(header).b64url(claims).b64url(p1363-signature)` over
/// the first two segments. Used for both VAPID (RFC 8292) and APNs
/// provider tokens.
/// 中文：header/claims 由调用方给出（VAPID 与 APNs 字段集不同）；
/// 序列化失败会签出空段，调用方须保证传入合法 JSON 对象。
pub fn sign_es256_jwt(
    secret: &SecretKey,
    header: &serde_json::Value,
    claims: &serde_json::Value,
) -> String {
    let header_b64 = b64url_encode(serde_json::to_string(header).unwrap_or_default().as_bytes());
    let claims_b64 = b64url_encode(serde_json::to_string(claims).unwrap_or_default().as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");
    let signature = sign_p1363(secret, signing_input.as_bytes());
    format!("{signing_input}.{}", b64url_encode(&signature))
}

// ---------------------------------------------------------------------------
// VAPID (RFC 8292)
// ---------------------------------------------------------------------------

/// `webPush.generateVAPIDKeys()`: a P-256 keypair serialized as
/// base64url(public SEC1 point) / base64url(private scalar).
/// 中文：返回 (publicKey, privateKey) 的 base64url 对，直接写入
/// settings 的 `vapidKeys` 供后续每次发送复用。
pub fn generate_vapid_keys() -> (String, String) {
    let secret = generate_secret_key();
    let public_b64 = public_to_b64url(&secret.public_key());
    let private_b64 = secret_to_b64url(&secret);
    (public_b64, private_b64)
}

/// `vapid t=<jwt>, k=<b64url public key>` — the Authorization header for a
/// push send. The JWT audience is the push endpoint's origin.
/// 中文：私钥解析失败或 endpoint 不是合法 URL 时返回 None，调用方
/// 跳过该次发送；`aud` 取 endpoint 的 origin（scheme://host[:port]）。
pub fn vapid_authorization_header(
    vapid_private_b64url: &str,
    subject: &str,
    endpoint: &str,
    now_ms: u64,
) -> Option<String> {
    let secret = secret_from_b64url(vapid_private_b64url)?;
    let audience = url::Url::parse(endpoint)
        .ok()
        .map(|url| {
            let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
            if let Some(port) = url.port() {
                origin.push_str(&format!(":{port}"));
            }
            origin
        })
        .unwrap_or_default();
    let header = serde_json::json!({ "typ": "JWT", "alg": "ES256" });
    let claims = serde_json::json!({
        "aud": audience,
        "exp": now_ms / 1000 + VAPID_JWT_TTL_SECONDS,
        "sub": subject,
    });
    let jwt = sign_es256_jwt(&secret, &header, &claims);
    let public_b64 = public_to_b64url(&secret.public_key());
    Some(format!("vapid t={jwt}, k={public_b64}"))
}

// ---------------------------------------------------------------------------
// aes128gcm payload encryption (RFC 8291)
// ---------------------------------------------------------------------------

/// HKDF-Expand 便捷封装：以 `info` 上下文从 PRK 派生 `len` 字节 OKM。
/// 长度固定且合法，expand 失败属不可达分支，直接 panic。
fn hkdf_expand_info(prk: &Hkdf<Sha256>, info: &[u8], len: usize) -> Vec<u8> {
    let mut okm = vec![0u8; len];
    // Length is fixed and small; expand cannot fail for valid HKDF outputs.
    prk.expand(info, &mut okm).expect("valid hkdf length");
    okm
}

/// 计算 ECDH P-256 共享秘密；当前曲线实现不会产生非法点错误，恒返回
/// Some，保留 Option 以匹配调用方的错误传播风格。
fn ecdh_secret(secret: &SecretKey, public: &PublicKey) -> Option<SharedSecret> {
    Some(p256::ecdh::diffie_hellman(
        secret.to_nonzero_scalar(),
        public.as_affine(),
    ))
}

/// Encrypt one push payload. The ephemeral application-server key is a
/// parameter so the RFC 8291 known-answer test can pin it; production
/// callers generate a fresh key per send (`encrypt_web_push_payload`).
/// 中文：完整 RFC 8291 单记录流程：auth_secret 作盐 Extract 出 PRK_key
/// → 「WebPush: info||ua公钥||as公钥」Expand 出 IKM → 再以 salt Extract
/// → 派生 16 字节 CEK 与 12 字节 nonce → GCM 加密「明文||0x02」→
/// 拼接 salt(16)|rs(4)|idlen=65|as公钥(65)|密文。任一输入非法返回 None。
pub fn encrypt_web_push_with_ephemeral(
    as_secret: &SecretKey,
    salt: &[u8; 16],
    ua_public_b64url: &str,
    auth_secret_b64url: &str,
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    let ua_public = public_from_b64url(ua_public_b64url)?;
    let auth_secret = b64url_decode(auth_secret_b64url)?;
    let as_public = as_secret.public_key();

    // HKDF-Extract(salt=auth_secret, IKM=ecdh_secret) then
    // HKDF-Expand(PRK_key, key_info, 32) — the "WebPush: info" combine.
    let ecdh = ecdh_secret(as_secret, &ua_public)?;
    let prk_key = Hkdf::<Sha256>::new(Some(&auth_secret), ecdh.raw_secret_bytes());
    let mut key_info = b"WebPush: info".to_vec();
    key_info.push(0x00);
    key_info.extend_from_slice(&ua_public.to_sec1_bytes());
    key_info.extend_from_slice(&as_public.to_sec1_bytes());
    let ikm = hkdf_expand_info(&prk_key, &key_info, 32);

    // RFC 8188 derivations from (salt, IKM).
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let cek = hkdf_expand_info(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf_expand_info(&prk, b"Content-Encoding: nonce\0", 12);

    // Single record: plaintext || 0x02 delimiter (no padding), per §4.
    let mut padded = plaintext.to_vec();
    padded.push(0x02);

    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &padded,
                aad: &[],
            },
        )
        .ok()?;

    // aes128gcm header: salt(16) | rs(4, big endian) | idlen(1) | keyid.
    let mut body = Vec::with_capacity(86 + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&WEB_PUSH_RECORD_SIZE.to_be_bytes());
    body.push(65);
    body.extend_from_slice(&as_public.to_sec1_bytes());
    body.extend_from_slice(&ciphertext);
    Some(body)
}

/// Production encryption: fresh random salt + ephemeral application-server
/// key for every send (matching the Node `web-push` behavior).
/// 中文：每次调用随机生成临时服务器密钥与 16 字节 salt，因此同一明文
/// 两次加密的输出必然不同（前向安全）。
pub fn encrypt_web_push_payload(
    ua_public_b64url: &str,
    auth_secret_b64url: &str,
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    let as_secret = generate_secret_key();
    let salt: [u8; 16] = random_bytes(16).try_into().expect("16 random bytes");
    encrypt_web_push_with_ephemeral(
        &as_secret,
        &salt,
        ua_public_b64url,
        auth_secret_b64url,
        plaintext,
    )
}

// ---------------------------------------------------------------------------
// APNs .p8 parsing
// ---------------------------------------------------------------------------

/// Minimal DER walk to the 32-byte P-256 scalar inside a PEM-encoded EC
/// private key. Apple `.p8` files are PKCS#8 (`PRIVATE KEY`), whose inner
/// value embeds a SEC1 ECPrivateKey; some tools emit SEC1 directly
/// (`EC PRIVATE KEY`). Both shapes carry exactly one 32-byte OCTET STRING
/// holding the scalar (the PKCS#8 wrapper's OCTET STRING is longer, so
/// scanning for the first 32-byte OCTET STRING resolves both).
/// 中文：返回 Apple 签名所需的 32 字节标量；非 PEM、base64 损坏或
/// 找不到 32 字节 OCTET STRING 均返回 None。
pub fn extract_p256_scalar_from_pem(pem: &str) -> Option<[u8; 32]> {
    let body = strip_pem_armor(pem)?;
    let der = b64_standard_decode_ignoring_whitespace(&body)?;
    find_der_octet_string(&der, 32).and_then(|bytes| bytes.try_into().ok())
}

/// 剥离 PEM armor：跳过 BEGIN 行、逐行拼接（顺带去除行内空白）直到
/// END 行；缺 BEGIN 或 END 返回 None。
fn strip_pem_armor(pem: &str) -> Option<String> {
    let mut lines = pem.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next()?;
    if !first.contains("-----BEGIN") {
        return None;
    }
    let mut body = String::new();
    for line in lines {
        if line.contains("-----END") {
            return Some(body);
        }
        body.push_str(line.trim());
    }
    None
}

/// 清除全部 ASCII 空白后做标准 base64 解码（PEM 体允许折行/缩进）。
fn b64_standard_decode_ignoring_whitespace(text: &str) -> Option<Vec<u8>> {
    let cleaned: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()
}

/// Scan DER for `04 20 <32 bytes>` (OCTET STRING, length 32).
/// 中文：不做完整 DER 解析，直接线性扫描定位第一个「tag=04 且长度
/// 恰为 len」的 OCTET STRING，PKCS#8 与 SEC1 两种包装因此通用。
fn find_der_octet_string(der: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut i = 0;
    while i + 1 < der.len() {
        if der[i] == 0x04 && der[i + 1] as usize == len && i + 2 + len <= der.len() {
            return Some(der[i + 2..i + 2 + len].to_vec());
        }
        i += 1;
    }
    None
}

/// 加密原语单元测试：全部对齐 RFC 官方测试向量或做往返一致性验证。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

/// 十六进制字符串转字节向量（测试向量输入）；非法字符 panic。
    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digit"))
            .collect()
    }

/// 验证签名实现与 RFC 6979 A.2.5 官方向量逐位一致，且配套公钥可验签、
/// 篡改消息后验签失败。
    #[test]
    fn es256_rfc6979_p256_sha256_sample_vector() {
        // RFC 6979 A.2.5 (P-256), SHA-256, message "sample": deterministic
        // ECDSA must reproduce the published r/s exactly (P1363 = r||s).
        let x = hex("C9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721");
        let secret = SecretKey::from_slice(&x).expect("valid scalar");
        let expected_r = "EFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716";
        let expected_s = "F7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8";
        let signature = sign_p1363(&secret, b"sample");
        assert_eq!(signature.len(), 64);
        assert_eq!(to_hex(&signature[..32]), expected_r);
        assert_eq!(to_hex(&signature[32..]), expected_s);

        // And the matching public key verifies it.
        let public = secret.public_key();
        assert!(verify_p1363(&public, b"sample", &signature));
        assert!(!verify_p1363(&public, b"tampered", &signature));
    }

/// 字节转大写十六进制字符串，用于断言官方测试向量。
    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

/// 验证 ES256 JWT 的三段结构、header/claims 可解码回读，且签名能被
/// 对应公钥在「头.体」原文上验签通过。
    #[test]
    fn vapid_jwt_shape_and_roundtrip() {
        let secret = generate_secret_key();
        let header = json!({ "typ": "JWT", "alg": "ES256" });
        let claims =
            json!({ "aud": "https://push.example.net", "exp": 123_456, "sub": "mailto:a@b.c" });
        let jwt = sign_es256_jwt(&secret, &header, &claims);

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decoded_header: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[0]).expect("header b64"))
                .expect("header json");
        let decoded_claims: serde_json::Value =
            serde_json::from_slice(&b64url_decode(parts[1]).expect("claims b64"))
                .expect("claims json");
        assert_eq!(decoded_header["alg"], "ES256");
        assert_eq!(decoded_header["typ"], "JWT");
        assert_eq!(decoded_claims["aud"], "https://push.example.net");
        assert_eq!(decoded_claims["sub"], "mailto:a@b.c");

        // Signature verifies over the exact signing input.
        let signature = b64url_decode(parts[2]).expect("signature b64");
        assert!(verify_p1363(
            &secret.public_key(),
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &signature
        ));
    }

/// 验证 VAPID Authorization 头格式（`vapid t=<jwt>, k=<公钥>`），
/// 且 JWT claims 携带 endpoint origin 受众、subject 与 12 小时 exp。
    #[test]
    fn vapid_authorization_header_carries_key_and_jwt() {
        let (public_b64, private_b64) = generate_vapid_keys();
        let header = vapid_authorization_header(
            &private_b64,
            "mailto:ompchamber@localhost",
            "https://fcm.googleapis.com/fcm/send/abc123",
            1_700_000_000_000,
        )
        .expect("header");
        assert!(header.starts_with("vapid t="));
        assert!(header.ends_with(&format!(", k={public_b64}")));
        // JWT claims carry the endpoint origin and 12h expiry.
        let jwt = header
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split(", k=").next())
            .expect("jwt");
        let claims: serde_json::Value = serde_json::from_slice(
            &b64url_decode(jwt.split('.').nth(1).expect("claims segment")).expect("claims bytes"),
        )
        .expect("claims json");
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:ompchamber@localhost");
        assert_eq!(claims["exp"], 1_700_000_000 + VAPID_JWT_TTL_SECONDS);
    }

/// 验证 RFC 8291 附录 A 已知答案：固定密钥/salt/明文必须逐字节复现
/// 发布密文，且接收端（ua 私钥）能解密还原明文。
    #[test]
    fn aes128gcm_rfc8291_known_answer() {
        // RFC 8291 §5 / Appendix A: fixed keys, salt, and plaintext produce
        // exactly the published body (header + ciphertext, base64url).
        let as_private = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
        let ua_public = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        let auth_secret = "BTBZMqHH6r4Tts7J_aSIgg";
        let salt_bytes = b64url_decode("DGv6ra1nlYgDCS1FRnbzlw").expect("salt");
        let salt: [u8; 16] = salt_bytes.try_into().expect("16 salt bytes");
        let plaintext = b"When I grow up, I want to be a watermelon";

        let secret = secret_from_b64url(as_private).expect("as private");
        let body =
            encrypt_web_push_with_ephemeral(&secret, &salt, ua_public, auth_secret, plaintext)
                .expect("encryption");
        let expected = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";
        assert_eq!(b64url_encode(&body), expected);

        // Round-trip: decrypt with the receiver side (ua private key).
        let ua_private =
            secret_from_b64url("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94").expect("ua private");
        assert_eq!(decrypt_for_test(&ua_private, &body).unwrap(), plaintext);
    }

    /// Minimal receiver-side aes128gcm decryption used to prove the KAT
    /// round-trips (production only ever encrypts).
/// 中文：断言解出的定界符必须是 0x02，其余密钥派生与发送端对称；
/// 仅测试使用，生产代码只加密。
    fn decrypt_for_test(ua_private: &SecretKey, body: &[u8]) -> Option<Vec<u8>> {
        if body.len() < 86 || body[20] != 65 {
            return None;
        }
        let salt = &body[..16];
        let as_public = PublicKey::from_sec1_bytes(&body[21..86]).ok()?;
        let ciphertext = &body[86..];
        let ecdh = ecdh_secret(ua_private, &as_public)?;
        // Receiver-side key_info orders (ua || as) — same concatenation as
        // the sender because both sides use ua_public || as_public.
        let prk_key = Hkdf::<Sha256>::new(
            Some(&b64url_decode("BTBZMqHH6r4Tts7J_aSIgg")?),
            ecdh.raw_secret_bytes(),
        );
        let mut key_info = b"WebPush: info".to_vec();
        key_info.push(0x00);
        key_info.extend_from_slice(&ua_private.public_key().to_sec1_bytes());
        key_info.extend_from_slice(&as_public.to_sec1_bytes());
        let ikm = hkdf_expand_info(&prk_key, &key_info, 32);
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let cek = hkdf_expand_info(&prk, b"Content-Encoding: aes128gcm\0", 16);
        let nonce = hkdf_expand_info(&prk, b"Content-Encoding: nonce\0", 12);
        let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
        let padded = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &[],
                },
            )
            .ok()?;
        let mut plaintext = padded;
        let delimiter = plaintext.pop()?;
        assert_eq!(delimiter, 0x02);
        Some(plaintext)
    }

/// 验证生产加密路径的随机性契约：两次输出不同、总长符合
/// 86 字节头 + 明文 + 定界符 + 16 字节 tag、keyid 恒为 65 字节 SEC1 点。
    #[test]
    fn random_encryption_varies_and_round_trips() {
        let ua_secret = generate_secret_key();
        let ua_public = public_to_b64url(&ua_secret.public_key());
        let auth = b64url_encode(&random_bytes(16));
        let first = encrypt_web_push_payload(&ua_public, &auth, b"hello push").expect("body");
        let second = encrypt_web_push_payload(&ua_public, &auth, b"hello push").expect("body");
        assert_ne!(first, second, "fresh salt/ephemeral key per send");
        assert_eq!(first.len(), 86 + "hello push".len() + 1 + 16);
        // The 65-byte keyid always follows the fixed header.
        assert_eq!(first[20], 65);
        assert_eq!(&first[21..22], &[0x04]);
    }

/// 验证 JWK 私钥/公钥往返无损、规范字符串键序为 crv,kty,x,y、
/// serverId 派生确定且长度恒为 43 字符。
    #[test]
    fn jwk_roundtrip_and_server_id_are_stable() {
        let secret = generate_secret_key();
        let jwk = private_jwk_value(&secret);
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], "P-256");
        let public = jwk_public_key(&jwk).expect("public from jwk");
        assert_eq!(public, secret.public_key());
        let restored = jwk_secret_key(&jwk).expect("secret from jwk");
        assert_eq!(restored.to_bytes(), secret.to_bytes());

        let public_jwk = public_jwk_value(&secret.public_key());
        // Canonical string key order is crv,kty,x,y.
        assert!(
            canonical_public_jwk_string(&public_jwk)
                .starts_with("{\"crv\":\"P-256\",\"kty\":\"EC\"")
        );
        // serverId is deterministic.
        assert_eq!(derive_server_id(&public_jwk), derive_server_id(&public_jwk));
        assert_eq!(derive_server_id(&public_jwk).len(), 43);
    }

/// 验证手工构造的 PKCS#8 与 SEC1 两种 PEM 都能提取出同一标量，
/// 非 PEM 输入返回 None。
    #[test]
    fn p8_pem_scalar_extraction() {
        // Build a PKCS#8 PEM by hand-wrapping a SEC1 structure.
        let secret = generate_secret_key();
        let scalar = secret.to_bytes();
        let mut sec1: Vec<u8> = vec![0x30];
        let mut inner: Vec<u8> = vec![0x02, 0x01, 0x01, 0x04, 0x20];
        inner.extend_from_slice(scalar.as_slice());
        // [0] parameters: prime256v1 OID
        inner.extend_from_slice(&[0xA0, 0x07, 0x06, 0x05, 0x2B, 0x81, 0x04, 0x00, 0x22]);
        sec1.push(inner.len() as u8);
        sec1.extend_from_slice(&inner);

        let mut pkcs8: Vec<u8> = vec![0x30];
        let mut inner8: Vec<u8> = vec![0x02, 0x01, 0x00];
        inner8.extend_from_slice(&[0x30, 0x13]); // SEQUENCE
        inner8.extend_from_slice(&[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]); // ecPublicKey
        inner8.extend_from_slice(&[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]); // prime256v1
        inner8.push(0x04);
        inner8.push(sec1.len() as u8);
        inner8.extend_from_slice(&sec1);
        pkcs8.push(inner8.len() as u8);
        pkcs8.extend_from_slice(&inner8);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&pkcs8);
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n");
        let extracted = extract_p256_scalar_from_pem(&pem).expect("scalar");
        assert_eq!(extracted.as_slice(), scalar.as_slice());

        // Env-style literal-\n input is normalized by the caller; a plain
        // SEC1 PEM also works.
        let sec1_pem = format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----",
            base64::engine::general_purpose::STANDARD.encode(&sec1)
        );
        assert_eq!(
            extract_p256_scalar_from_pem(&sec1_pem)
                .expect("scalar")
                .as_slice(),
            scalar.as_slice()
        );
        assert!(extract_p256_scalar_from_pem("not a pem").is_none());
    }
}
