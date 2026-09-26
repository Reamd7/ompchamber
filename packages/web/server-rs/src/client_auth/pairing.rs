//! Port of `server/lib/client-auth/pairing.js`
//! (`createClientPairingRuntime`): short-lived one-time Pairing v2 sessions
//! persisted at `<data_dir>/client-pairing-sessions.json`.
//!
//! A session is created with a secret shown once (stored hashed), can be
//! cancelled, expires after the TTL (default 10 minutes), and is redeemed
//! exactly once into a remote-client token via the [`ClientIssuer`] seam
//! (the JS `remoteClientAuthRuntime` dependency).

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

pub(crate) const STORE_VERSION: i64 = 1;
pub const PAIRING_ID_PREFIX: &str = "pair_";
pub(crate) const SECRET_BYTES: usize = 32;
pub(crate) const FINGERPRINT_BYTES: usize = 4;
pub const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;
pub(crate) const MAX_LABEL_LENGTH: usize = 80;
/// JS `GENERIC_REDEEM_ERROR` — matched by message in `core-routes.js`'s
/// redeem catch, so the exact string is load-bearing.
pub const GENERIC_REDEEM_ERROR: &str = "Invalid or expired pairing session";
/// JS `PAIRING_LABEL_PLACEHOLDER` — display default only; the stored label
/// stays None so redeem can fall back to the device's reported name.
pub const PAIRING_LABEL_PLACEHOLDER: &str = "Pair new device";

const VALID_CLIENT_KINDS: [&str; 2] = ["mobile", "desktop"];

/// What [`ClientIssuer::create_client`] hands back (JS `{ client, token }`).
pub struct CreatedClientRef {
    pub client_id: Option<String>,
    pub client: PublicClient,
    pub token: String,
}

/// What [`ClientIssuer::create_client`] hands back (JS `{ client, token }`).
/// The JS `remoteClientAuthRuntime` dependency: the single method pairing
/// needs. Implemented by
/// [`RemoteClientAuth`](super::remote_clients::RemoteClientAuth); tests
/// inject fakes at this seam like the JS DI point.
pub trait ClientIssuer: Send + Sync {
    fn create_client<'a>(
        &'a self,
        input: CreateClientInput,
    ) -> BoxFuture<'a, AppResult<CreatedClientRef>>;
}

impl ClientIssuer for super::remote_clients::RemoteClientAuth {
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
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredSession {
    pub id: String,
    pub secret_hash: String,
    pub created_at: String,
    pub expires_at: String,
    pub used_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub client_id: Option<String>,
    pub label: Option<String>,
    pub fingerprint: String,
    pub allowed_client_kinds: Vec<String>,
    pub created_by_client_id: Option<String>,
    pub uses_relay: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredPairingStore {
    pub version: i64,
    pub sessions: Vec<StoredSession>,
}

/// JS `publicSession` (the wire shape; never carries `secretHash`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSession {
    pub id: String,
    pub created_at: String,
    pub expires_at: String,
    pub used_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub client_id: Option<String>,
    pub label: String,
    pub fingerprint: String,
    pub allowed_client_kinds: Vec<String>,
    pub created_by_client_id: Option<String>,
    pub uses_relay: bool,
}

/// JS `createPairingSession` result: `{ pairing: publicSession + secret }` —
/// the secret is shown once.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedPairingSession {
    pub pairing: PublicPairingWithSecret,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicPairingWithSecret {
    #[serde(flatten)]
    pub session: PublicSession,
    pub secret: String,
}

/// JS `cancelPairingSession` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelResult {
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PublicSession>,
}

/// JS `redeemPairingSession` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemedPairing {
    pub pairing: PublicSession,
    pub client: PublicClient,
    pub token: String,
}

/// JS `sweepExpiredSessions` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepResult {
    pub purged: usize,
}

/// JS `createPairingSession` input.
#[derive(Clone, Debug, Default)]
pub struct CreatePairingInput {
    pub label: Option<String>,
    pub allowed_client_kinds: Option<Vec<String>>,
    pub created_by_client_id: Option<String>,
    pub uses_relay: bool,
}

/// JS `redeemPairingSession` input.
#[derive(Clone, Debug, Default)]
pub struct RedeemPairingInput {
    pub pairing_id: Option<String>,
    pub secret: Option<String>,
    pub client_label: Option<String>,
    pub client_kind: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
    pub dedupe_key: Option<String>,
}

pub struct ClientPairing {
    store_path: PathBuf,
    clock: Clock,
    ttl_ms: i64,
    issuer: Arc<dyn ClientIssuer>,
    /// JS `storeMutationQueue`.
    mutation_lock: tokio::sync::Mutex<()>,
}

impl ClientPairing {
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
    pub async fn has_active_relay_session(&self) -> AppResult<bool> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store
            .sessions
            .iter()
            .any(|session| session.uses_relay && is_pending_session(session, self.clock.clone())))
    }

    /// JS `getPairingSession`.
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
pub fn redeem_error() -> AppError {
    AppError::new(axum::http::StatusCode::BAD_REQUEST, GENERIC_REDEEM_ERROR)
}

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
pub(crate) fn is_pending_session(session: &StoredSession, clock: Clock) -> bool {
    session.used_at.is_none()
        && session.cancelled_at.is_none()
        && parse_iso_ms(&session.expires_at).is_some_and(|expires| expires > clock())
}

/// JS `sweepExpiredSessionsFromStore`: keep used/cancelled sessions for the
/// TTL window, drop expired never-redeemed ones.
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
fn normalize_stored_label(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    if normalized.chars().count() > MAX_LABEL_LENGTH {
        Some(normalized.chars().take(MAX_LABEL_LENGTH).collect())
    } else {
        Some(normalized)
    }
}

/// JS `normalizeClientKind`: valid kind or None.
fn normalize_client_kind(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    VALID_CLIENT_KINDS
        .contains(&normalized.as_str())
        .then_some(normalized)
}

/// JS `normalizeAllowedClientKinds`: default `["mobile", "desktop"]`, deduped
/// in first-seen order.
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

fn generate_id() -> String {
    format!("{PAIRING_ID_PREFIX}{}", random_hex(12))
}

fn generate_secret() -> String {
    random_base64url(SECRET_BYTES)
}

/// 4 random bytes as upper-case hex formatted `XXXX-XXXX`.
fn generate_fingerprint() -> String {
    let hex = random_hex(FINGERPRINT_BYTES).to_uppercase();
    format!("{}-{}", &hex[0..4], &hex[4..8])
}
