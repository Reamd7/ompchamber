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

use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::AppResult;
use crate::settings::SettingsStore;

use super::e2ee::{base64_url_to_bytes, bytes_to_base64_url};

pub const SETTINGS_KEY_SIGNING: &str = "relaySigningKey";

pub struct RelaySigningKeypair {
    pub signing_key: SigningKey,
    pub public_jwk: Value,
}

fn decode_scalar(d: &str) -> Option<[u8; 32]> {
    let bytes = base64_url_to_bytes(d).ok()?;
    bytes.as_slice().try_into().ok()
}

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

fn generate_signing_key() -> SigningKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        if let Ok(key) = SigningKey::from_slice(&bytes) {
            return key;
        }
    }
}

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
pub fn derive_server_id(public_jwk: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_public_jwk_string(public_jwk).as_bytes());
    bytes_to_base64_url(&hasher.finalize())
}

/// ECDSA-SHA256, IEEE P1363 (raw r||s) signature — the form WebCrypto verifies.
pub fn sign_relay_message(signing_key: &SigningKey, message: &str) -> String {
    use p256::ecdsa::signature::Signer;
    let signature: Signature = signing_key.sign(message.as_bytes());
    bytes_to_base64_url(&signature.to_bytes())
}

/// Verify helper (test surface; the relay worker verifies in production).
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
