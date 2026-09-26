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

pub const SETTINGS_KEY_ENCRYPTION: &str = "relayEncryptionKey";

#[derive(Clone)]
pub struct RelayAuth {
    pub ts: i64,
    pub sig: String,
    pub pk: String,
}

pub struct RelayIdentity {
    pub server_id: String,
    pub host_enc_pub_jwk: Value,
    pub host_enc_private_key: SecretKey,
    /// Relay-layer auth for host-control / host-data upgrades. Signature
    /// payload string is `${ts}.${serverId}.${role}.${connectionId ?? ""}`
    /// (spec Layer 1).
    pub sign_relay_auth: Arc<dyn Fn(&str, Option<&str>) -> RelayAuth + Send + Sync>,
}

/// Current wall-clock milliseconds (JS `Date.now()`); injectable for tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

fn is_jwk_pair(value: Option<&Value>) -> bool {
    let Some(object) = value.and_then(Value::as_object) else {
        return false;
    };
    // JS truthiness: both members must be present, non-null objects.
    object.get("privateJwk").is_some_and(|v| !v.is_null())
        && object.get("publicJwk").is_some_and(|v| !v.is_null())
}

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

pub struct RelayIdentityRuntime {
    store: Arc<SettingsStore>,
    clock: Clock,
    cached: Mutex<Option<Arc<RelayIdentity>>>,
}

impl RelayIdentityRuntime {
    pub fn new(store: Arc<SettingsStore>, clock: Clock) -> Self {
        Self {
            store,
            clock,
            cached: Mutex::new(None),
        }
    }

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
