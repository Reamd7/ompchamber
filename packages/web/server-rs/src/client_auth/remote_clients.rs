//! Port of `server/lib/client-auth/remote-clients.js`
//! (`createRemoteClientAuthRuntime`): the trusted-device registry persisted at
//! `<data_dir>/remote-clients.json`.
//!
//! Tokens (`oc_client_…`) are returned once, stored only as SHA-256 hashes,
//! and authenticated in constant time. Every public operation runs inside the
//! mutation queue (JS `withStoreMutation`), so concurrent auth traffic and
//! revocation cannot interleave read-modify-write cycles.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use super::time::{Clock, iso_utc_from_unix_millis, parse_iso_ms};
use super::util::{constant_time_equal_hex, random_base64url, random_hex, sha256_hex};
use crate::error::{AppError, AppResult};

pub(crate) const STORE_VERSION: i64 = 1;
pub const TOKEN_PREFIX: &str = "oc_client_";
pub(crate) const TOKEN_BYTES: usize = 32;
pub(crate) const MAX_LABEL_LENGTH: usize = 80;
pub(crate) const LAST_USED_WRITE_INTERVAL_MS: i64 = 60_000;

/// Which transport carried a request (JS reads
/// `x-ompchamber-relay-connection` truthiness).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Relay,
    Direct,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Relay => "relay",
            Transport::Direct => "direct",
        }
    }
}

/// One stored client record. Field order matches the JS `normalizeStore`
/// object literal so serialized JSON bytes line up.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredClient {
    pub id: String,
    pub label: String,
    pub token_hash: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub expires_at: Option<String>,
    pub client_kind: Option<String>,
    pub dedupe_key: Option<String>,
    pub uses_relay: bool,
    pub last_transport: Option<String>,
    pub auth_method: Option<String>,
    pub pairing_id: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredRemoteClientStore {
    pub version: i64,
    pub clients: Vec<StoredClient>,
}

/// JS `publicClient` — the wire shape; never carries `tokenHash`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicClient {
    pub id: String,
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub expires_at: Option<String>,
    pub client_kind: Option<String>,
    pub auth_method: Option<String>,
    pub pairing_id: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
    pub uses_relay: bool,
    pub last_transport: Option<String>,
}

/// JS `createClient` input object (destructured with all-optional fields).
#[derive(Clone, Debug, Default)]
pub struct CreateClientInput {
    pub label: Option<String>,
    pub fallback_label: Option<String>,
    pub expires_at: Option<String>,
    pub client_kind: Option<String>,
    pub dedupe_key: Option<String>,
    pub auth_method: Option<String>,
    pub pairing_id: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
    pub uses_relay: bool,
}

/// `createClient` result: `{ client, token }` — the token is shown once.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedClient {
    pub client: PublicClient,
    pub token: String,
}

/// `revokeClient` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeResult {
    pub revoked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<PublicClient>,
}

/// `purgeRevokedClients` result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    pub purged: usize,
}

/// `authenticateBearerToken` result: `{ ok, clientId, sessionToken, client }`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedClient {
    pub ok: bool,
    pub client_id: String,
    pub session_token: String,
    pub client: PublicClient,
}

pub struct RemoteClientAuth {
    store_path: PathBuf,
    clock: Clock,
    /// JS `storeMutationQueue`: serializes every read-modify-write.
    mutation_lock: tokio::sync::Mutex<()>,
}

impl RemoteClientAuth {
    pub fn new(store_path: PathBuf, clock: Clock) -> Self {
        Self {
            store_path,
            clock,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// JS `listClients`.
    pub async fn list_clients(&self) -> AppResult<Vec<PublicClient>> {
        let _guard = self.mutation_lock.lock().await;
        let store = self.read_store().await?;
        Ok(store.clients.iter().map(public_client).collect())
    }

    /// Relay-transport demand from paired devices: any non-revoked,
    /// non-expired client paired over the relay OR observed connecting through
    /// the relay tunnel (`lastTransport`). The observed transport is the
    /// authoritative signal.
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
    pub async fn is_valid_client_token(&self, token: &str) -> bool {
        self.authenticate_bearer_token(token, Transport::Direct)
            .await
            .unwrap_or(None)
            .is_some()
    }

    /// JS `readStore`: ENOENT or unparseable JSON both yield the empty store.
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

pub(crate) fn now_iso(clock: Clock) -> String {
    iso_utc_from_unix_millis(clock())
}

/// JS `normalizeLabel`: trimmed, capped, defaulted to "Remote client".
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
pub(crate) fn normalize_timestamp(value: Option<&str>) -> Option<String> {
    let normalized = normalize_optional_string(value)?;
    parse_iso_ms(&normalized).map(iso_utc_from_unix_millis)
}

/// JS `normalizeOptionalString`.
pub(crate) fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn string_or_none(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn generate_id() -> String {
    random_hex(12)
}

fn generate_token() -> String {
    format!("{TOKEN_PREFIX}{}", random_base64url(TOKEN_BYTES))
}

/// JS `writeStore` file handling: recursive parent (0o700), write (0o600),
/// best-effort chmod. Plain write, like the JS `fsPromises.writeFile`.
pub(crate) async fn write_file_private(path: &std::path::Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
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
