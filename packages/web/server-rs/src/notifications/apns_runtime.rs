//! Port of `server/lib/notifications/apns-runtime.js`: native iOS APNs
//! device-token persistence (data-dir `apns-tokens.json`) and delivery.
//!
//! Two modes, chosen at send time (APNS.md):
//! - **Relay (default)** — POST tokens + generic text to the central relay
//!   (`https://api.openchamber.dev/v1/push/send`), signing every request
//!   with the auto-generated ECDSA P-256 keypair persisted under
//!   `settings.relaySigningKey` (shared identity with the private relay).
//! - **Direct (fallback)** — sign an ES256 JWT with the configured `.p8`
//!   key and POST to `api.push.apple.com` over HTTP/2 ourselves when
//!   `OMPCHAMBER_PUSH_RELAY_DISABLED=true`.
//!
//! Device tokens are only persisted to `apns-tokens.json` (the JS's own
//! store) and relayed to the bound push relay; they are never logged.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use p256::SecretKey;
use serde_json::{Map, Value, json};

use crate::notifications::crypto;
use crate::notifications::transport::HttpPost;
use crate::settings::SettingsStore;

pub const APNS_TOKENS_VERSION: u64 = 1;
pub const APNS_HOST_PRODUCTION: &str = "https://api.push.apple.com";
pub const APNS_HOST_SANDBOX: &str = "https://api.sandbox.push.apple.com";
/// APNs rejects auth tokens older than 1h; refresh well inside the window.
pub const JWT_TTL_MS: u64 = 50 * 60 * 1000;
pub const DEFAULT_BUNDLE_ID: &str = "com.openchamber.app";
pub const DEFAULT_RELAY_URL: &str = "https://api.openchamber.dev/v1/push/send";
pub const MAX_TOKENS_PER_SESSION: usize = 10;
const MAX_TOKENS_PER_RELAY_SEND: usize = 100;
/// APNs reasons meaning the token is permanently invalid → drop it.
const DEAD_TOKEN_REASONS: [&str; 3] = ["BadDeviceToken", "Unregistered", "DeviceTokenNotForTopic"];

fn now_ms() -> u64 {
    crypto::now_ms()
}

fn trimmed_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Env vars commonly store the .p8 with literal `\n` sequences; restore
/// real newlines.
fn normalize_pem(value: &str) -> String {
    value.replace("\\n", "\n").trim().to_string()
}

fn normalize_platform(platform: Option<&str>) -> &'static str {
    if platform == Some("android") {
        "android"
    } else {
        "ios"
    }
}

fn normalize_environment(environment: Option<&str>) -> &'static str {
    if environment == Some("sandbox") {
        "sandbox"
    } else {
        "production"
    }
}

#[derive(Debug, Clone)]
pub struct ApnsConfig {
    pub key_id: String,
    pub team_id: String,
    pub p8: String,
    pub bundle_id: String,
    /// Explicit override forcing every send to one environment; `None`
    /// routes each token to the environment it registered with.
    pub environment: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct RelayConfig {
    url: String,
    register_url: String,
    environment: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct TokenEntry {
    device_token: String,
    created_at: Option<u64>,
    last_seen_at: Option<u64>,
    user_agent: Option<String>,
    platform: &'static str,
    environment: &'static str,
}

impl TokenEntry {
    fn to_json(&self) -> Value {
        json!({
            "deviceToken": self.device_token,
            "createdAt": self.created_at,
            "lastSeenAt": self.last_seen_at,
            "userAgent": self.user_agent,
            "platform": self.platform,
            "environment": self.environment,
        })
    }
}

struct CachedJwt {
    token: String,
    issued_at_ms: u64,
    key_id: String,
}

pub struct ApnsRuntime {
    tokens_path: PathBuf,
    store: Arc<SettingsStore>,
    transport: HttpPost,
    persist_lock: tokio::sync::Mutex<()>,
    cached_jwt: Mutex<Option<CachedJwt>>,
    cached_relay_key: Mutex<Option<(SecretKey, Value)>>,
    warned_unconfigured: AtomicBool,
}

impl ApnsRuntime {
    pub fn new(tokens_path: PathBuf, store: Arc<SettingsStore>, transport: HttpPost) -> Arc<Self> {
        Arc::new(Self {
            tokens_path,
            store,
            transport,
            persist_lock: tokio::sync::Mutex::new(()),
            cached_jwt: Mutex::new(None),
            cached_relay_key: Mutex::new(None),
            warned_unconfigured: AtomicBool::new(false),
        })
    }

    // -----------------------------------------------------------------------
    // Relay signing identity (relay/signing-key.js)
    // -----------------------------------------------------------------------

    /// `getOrCreateRelaySigningKeypair`: `settings.relaySigningKey =
    /// { privateJwk, publicJwk }`. The regeneration gate re-verifies with
    /// the strict settings reader before minting a new keypair (a new key
    /// means a new serverId — every bound device would be orphaned).
    async fn get_or_create_relay_keypair(&self) -> Result<(SecretKey, Value), String> {
        if let Some(cached) = self
            .cached_relay_key
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(cached);
        }
        let settings = self.store.read_migrated().await.unwrap_or_default();
        let existing = settings.get("relaySigningKey");
        let valid = |value: Option<&Value>| -> Option<(SecretKey, Value)> {
            let jwk = value?;
            let secret = crypto::jwk_secret_key(jwk)?;
            let public = crypto::jwk_public_key(jwk)?;
            Some((secret, crypto::public_jwk_value(&public)))
        };
        if let Some(existing) = valid(existing) {
            *self
                .cached_relay_key
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(existing.clone());
            return Ok(existing);
        }
        // Strict re-read: corruption must not be mistaken for a first run.
        let verified_settings = match self.store.read_strict().await {
            Ok(strict) => strict,
            Err(_) => settings.clone(),
        };
        if let Some(verified) = valid(verified_settings.get("relaySigningKey")) {
            *self
                .cached_relay_key
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(verified.clone());
            return Ok(verified);
        }
        tracing::warn!(
            "[relay-identity] Generating NEW relay signing keypair (serverId changes; previously paired devices must re-pair)"
        );
        let secret = crypto::generate_secret_key();
        let public_jwk = crypto::public_jwk_value(&secret.public_key());
        let private_jwk = crypto::private_jwk_value(&secret);
        let mut next = settings;
        for (key, value) in verified_settings {
            next.insert(key, value);
        }
        next.insert(
            "relaySigningKey".to_string(),
            json!({ "privateJwk": private_jwk, "publicJwk": public_jwk }),
        );
        self.store
            .write_raw(&next)
            .await
            .map_err(|error| error.to_string())?;
        let pair = (secret, public_jwk);
        *self
            .cached_relay_key
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(pair.clone());
        Ok(pair)
    }

    fn relay_public_jwk(public_jwk: &Value) -> Value {
        json!({
            "kty": public_jwk.get("kty").cloned().unwrap_or(Value::Null),
            "crv": public_jwk.get("crv").cloned().unwrap_or(Value::Null),
            "x": public_jwk.get("x").cloned().unwrap_or(Value::Null),
            "y": public_jwk.get("y").cloned().unwrap_or(Value::Null),
        })
    }

    /// `registerTokenWithRelay`: (re)bind the token to this server so only
    /// we can push to it. Failure is a warning, never a registration error.
    async fn register_token_with_relay(&self, token: &str, platform: &str) {
        let Some(relay) = Self::resolve_relay_config() else {
            return; // direct mode — no relay binding needed
        };
        let (secret, public_jwk) = match self.get_or_create_relay_keypair().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("[Push relay] register-token request failed: {error}");
                return;
            }
        };
        let ts = now_ms();
        // Platform is part of the signed message so it can't be tampered.
        let message = format!("{ts}.{token}.{platform}");
        let sig = crypto::b64url_encode(&crypto::sign_p1363(&secret, message.as_bytes()));
        let body = serde_json::to_vec(&json!({
            "token": token,
            "platform": platform,
            "publicKeyJwk": Self::relay_public_jwk(&public_jwk),
            "ts": ts,
            "sig": sig,
        }))
        .unwrap_or_default();
        match (self.transport)(
            &relay.register_url,
            vec![("content-type".into(), "application/json".into())],
            body,
        )
        .await
        {
            Ok(response) if response.status >= 200 && response.status < 300 => {}
            Ok(response) => {
                tracing::warn!(
                    "[Push relay] register-token failed status={}",
                    response.status
                );
            }
            Err(error) => {
                tracing::warn!("[Push relay] register-token request failed: {error}");
            }
        }
    }

    /// `resolveRelayConfig`: `None` in direct mode.
    fn resolve_relay_config() -> Option<RelayConfig> {
        if trimmed_env("OMPCHAMBER_PUSH_RELAY_DISABLED").as_deref() == Some("true") {
            return None;
        }
        let url = trimmed_env("OMPCHAMBER_PUSH_RELAY_URL")
            .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
        let register_url = url
            .strip_suffix("/send")
            .map(|base| format!("{base}/register-token"))
            .unwrap_or_else(|| url.clone());
        let override_env = trimmed_env("OMPCHAMBER_APNS_ENVIRONMENT")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        Some(RelayConfig {
            url,
            register_url,
            environment: match override_env.as_str() {
                "sandbox" => Some("sandbox"),
                "production" => Some("production"),
                _ => None,
            },
        })
    }

    // -----------------------------------------------------------------------
    // Token persistence
    // -----------------------------------------------------------------------

    async fn read_tokens_from_disk(&self) -> Map<String, Value> {
        let raw = match tokio::fs::read_to_string(&self.tokens_path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Map::new(),
            Err(error) => {
                tracing::warn!("Failed to read APNs tokens file: {error}");
                return Map::new();
            }
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!("Failed to read APNs tokens file: {error}");
                return Map::new();
            }
        };
        let Some(object) = parsed.as_object() else {
            return Map::new();
        };
        if object.get("version").and_then(Value::as_u64) != Some(APNS_TOKENS_VERSION) {
            return Map::new();
        }
        object
            .get("tokensBySession")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    async fn write_tokens_to_disk(&self, tokens_by_session: &Map<String, Value>) {
        let document = json!({
            "version": APNS_TOKENS_VERSION,
            "tokensBySession": Value::Object(tokens_by_session.clone()),
        });
        let body = serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_string());
        if let Some(parent) = self.tokens_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(error) = tokio::fs::write(&self.tokens_path, body).await {
            tracing::warn!("Failed to write APNs tokens file: {error}");
        }
    }

    async fn persist_token_update<F>(&self, mutate: F)
    where
        F: FnOnce(Map<String, Value>) -> Map<String, Value> + Send,
    {
        let _guard = self.persist_lock.lock().await;
        let current = self.read_tokens_from_disk().await;
        let next = mutate(current);
        self.write_tokens_to_disk(&next).await;
    }

    fn normalize_tokens(record: &Value) -> Vec<TokenEntry> {
        let Some(entries) = record.as_array() else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| {
                if !entry.is_object() {
                    return None;
                }
                let device_token = entry.get("deviceToken")?.as_str()?.trim().to_string();
                if device_token.is_empty() {
                    return None;
                }
                Some(TokenEntry {
                    device_token,
                    created_at: entry.get("createdAt").and_then(Value::as_u64),
                    last_seen_at: entry.get("lastSeenAt").and_then(Value::as_u64),
                    user_agent: entry
                        .get("userAgent")
                        .and_then(Value::as_str)
                        .map(String::from),
                    platform: normalize_platform(entry.get("platform").and_then(Value::as_str)),
                    environment: normalize_environment(
                        entry.get("environment").and_then(Value::as_str),
                    ),
                })
            })
            .collect()
    }

    /// `addOrUpdateApnsToken`: upsert scoped to the UI session, then bind
    /// the token on the relay (idempotent, signed).
    pub async fn add_or_update_apns_token(
        &self,
        ui_session_token: &str,
        device_token: &str,
        user_agent: Option<&str>,
        platform: Option<&str>,
        environment: Option<&str>,
    ) {
        if ui_session_token.is_empty() {
            return;
        }
        let token = device_token.trim();
        if token.is_empty() {
            return;
        }
        let token_platform = normalize_platform(platform);
        let token_environment = normalize_environment(environment);
        let now = now_ms();

        self.persist_token_update(move |mut tokens_by_session| {
            let existing = tokens_by_session
                .get(ui_session_token)
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            let mut filtered: Vec<Value> = Self::normalize_tokens(&existing)
                .into_iter()
                .filter(|entry| entry.device_token != token)
                .map(|entry| entry.to_json())
                .collect();
            filtered.insert(
                0,
                TokenEntry {
                    device_token: token.to_string(),
                    created_at: Some(now),
                    last_seen_at: Some(now),
                    user_agent: user_agent
                        .filter(|agent| !agent.is_empty())
                        .map(String::from),
                    platform: token_platform,
                    environment: token_environment,
                }
                .to_json(),
            );
            filtered.truncate(MAX_TOKENS_PER_SESSION);
            tokens_by_session.insert(ui_session_token.to_string(), Value::Array(filtered));
            tokens_by_session
        })
        .await;

        self.register_token_with_relay(token, token_platform).await;
    }

    /// `removeApnsToken`.
    pub async fn remove_apns_token(&self, ui_session_token: &str, device_token: &str) {
        if ui_session_token.is_empty() || device_token.is_empty() {
            return;
        }
        self.persist_token_update(move |mut tokens_by_session| {
            let Some(record) = tokens_by_session.get(ui_session_token).cloned() else {
                return tokens_by_session;
            };
            let kept: Vec<Value> = Self::normalize_tokens(&record)
                .into_iter()
                .filter(|entry| entry.device_token != device_token)
                .map(|entry| entry.to_json())
                .collect();
            if kept.is_empty() {
                tokens_by_session.remove(ui_session_token);
            } else {
                tokens_by_session.insert(ui_session_token.to_string(), Value::Array(kept));
            }
            tokens_by_session
        })
        .await;
    }

    /// `removeApnsTokenFromAllSessions`.
    pub async fn remove_apns_token_from_all_sessions(&self, device_token: &str) {
        if device_token.is_empty() {
            return;
        }
        self.persist_token_update(move |mut tokens_by_session| {
            let keys: Vec<String> = tokens_by_session.keys().cloned().collect();
            for key in keys {
                let Some(record) = tokens_by_session.get(&key).cloned() else {
                    continue;
                };
                if !record.is_array() {
                    continue;
                }
                let kept: Vec<Value> = Self::normalize_tokens(&record)
                    .into_iter()
                    .filter(|entry| entry.device_token != device_token)
                    .map(|entry| entry.to_json())
                    .collect();
                if kept.is_empty() {
                    tokens_by_session.remove(&key);
                } else {
                    tokens_by_session.insert(key, Value::Array(kept));
                }
            }
            tokens_by_session
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // Config + JWT
    // -----------------------------------------------------------------------

    /// `resolveApnsConfig`: env first, then `settings.apnsConfig`.
    pub async fn resolve_apns_config(&self) -> Option<ApnsConfig> {
        let mut key_id = trimmed_env("OMPCHAMBER_APNS_KEY_ID");
        let mut team_id = trimmed_env("OMPCHAMBER_APNS_TEAM_ID");
        let mut bundle_id = trimmed_env("OMPCHAMBER_APNS_BUNDLE_ID");
        let mut environment = trimmed_env("OMPCHAMBER_APNS_ENVIRONMENT")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        let mut p8 = normalize_pem(&std::env::var("OMPCHAMBER_APNS_P8").unwrap_or_default());

        let p8_path = trimmed_env("OMPCHAMBER_APNS_P8_PATH");
        if p8.is_empty() {
            if let Some(p8_path) = p8_path {
                match tokio::fs::read_to_string(&p8_path).await {
                    Ok(contents) => p8 = contents.trim().to_string(),
                    Err(error) => {
                        tracing::warn!("[APNs] Failed to read OMPCHAMBER_APNS_P8_PATH: {error}");
                    }
                }
            }
        }

        if key_id.is_none() || team_id.is_none() || p8.is_empty() {
            if let Ok(settings) = self.store.read_migrated().await {
                if let Some(stored) = settings.get("apnsConfig").filter(|value| value.is_object()) {
                    key_id = key_id.or_else(|| {
                        stored
                            .get("keyId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(String::from)
                    });
                    team_id = team_id.or_else(|| {
                        stored
                            .get("teamId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(String::from)
                    });
                    bundle_id = bundle_id.or_else(|| {
                        stored
                            .get("bundleId")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(String::from)
                    });
                    if environment.is_empty() {
                        environment = stored
                            .get("environment")
                            .and_then(Value::as_str)
                            .map(|value| value.trim().to_ascii_lowercase())
                            .unwrap_or_default();
                    }
                    if p8.is_empty() {
                        if let Some(stored_p8) = stored.get("p8").and_then(Value::as_str) {
                            p8 = normalize_pem(stored_p8);
                        }
                    }
                }
            }
        }

        let key_id = key_id?;
        let team_id = team_id?;
        if p8.is_empty() {
            return None;
        }
        Some(ApnsConfig {
            key_id,
            team_id,
            p8,
            bundle_id: bundle_id.unwrap_or_else(|| DEFAULT_BUNDLE_ID.to_string()),
            environment: match environment.as_str() {
                "sandbox" => Some("sandbox"),
                "production" => Some("production"),
                _ => None,
            },
        })
    }

    /// `signApnsJwt`: ES256 JWT with `{alg, kid}` / `{iss, iat}`.
    pub fn sign_apns_jwt(config: &ApnsConfig) -> Option<String> {
        let scalar = crypto::extract_p256_scalar_from_pem(&config.p8)?;
        let secret = SecretKey::from_slice(&scalar).ok()?;
        let header = json!({ "alg": "ES256", "kid": config.key_id });
        let claims = json!({ "iss": config.team_id, "iat": now_ms() / 1000 });
        Some(crypto::sign_es256_jwt(&secret, &header, &claims))
    }

    /// `getJwt`: cached per keyId inside the TTL.
    fn get_jwt(&self, config: &ApnsConfig) -> Option<String> {
        let now = now_ms();
        let mut cached = self.cached_jwt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = cached.as_ref() {
            if cache.key_id == config.key_id && now.saturating_sub(cache.issued_at_ms) < JWT_TTL_MS
            {
                return Some(cache.token.clone());
            }
        }
        let token = Self::sign_apns_jwt(config)?;
        *cached = Some(CachedJwt {
            token: token.clone(),
            issued_at_ms: now,
            key_id: config.key_id.clone(),
        });
        Some(token)
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    /// `buildBody`: the APNs alert JSON (fields with undefined values are
    /// omitted, as with JSON.stringify).
    fn build_body(payload: &Value) -> String {
        let string_field = |name: &str| -> Option<Value> {
            payload
                .get(name)
                .and_then(Value::as_str)
                .map(|value| Value::String(value.to_string()))
        };
        let badge = payload
            .get("badge")
            .and_then(Value::as_f64)
            .filter(|badge| badge.is_finite() && *badge >= 0.0)
            .map(|badge| json!((badge.trunc() as i64) as u64));
        let tag = string_field("tag");
        let data = payload
            .get("data")
            .filter(|data| data.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));

        let mut aps = Map::new();
        let mut alert = Map::new();
        if let Some(title) = string_field("title") {
            alert.insert("title".into(), title);
        }
        if let Some(body) = string_field("body") {
            alert.insert("body".into(), body);
        }
        aps.insert("alert".into(), Value::Object(alert));
        if let Some(badge) = badge {
            aps.insert("badge".into(), badge);
        }
        aps.insert("sound".into(), json!("default"));
        if let Some(tag) = tag {
            aps.insert("thread-id".into(), tag);
        }
        // Wakes the Notification Service Extension so widgets refresh even
        // when the app is closed.
        aps.insert("mutable-content".into(), json!(1));

        let mut body = Map::new();
        body.insert("aps".into(), Value::Object(aps));
        if let Value::Object(data) = data {
            for (key, value) in data {
                body.insert(key, value);
            }
        }
        serde_json::to_string(&Value::Object(body)).unwrap_or_default()
    }

    /// `sendOne`: one direct-mode POST. 200 → delivered; 410 / dead-token
    /// reason → drop the token; anything else → warn.
    async fn send_one(
        &self,
        host: &str,
        device_token: &str,
        body: &str,
        jwt: &str,
        bundle_id: &str,
        tag: Option<&str>,
    ) {
        let collapse_id = tag
            .map(|tag| tag.chars().take(64).collect::<String>())
            .filter(|tag| !tag.is_empty());
        let mut headers = vec![
            ("authorization".to_string(), format!("bearer {jwt}")),
            ("apns-topic".to_string(), bundle_id.to_string()),
            ("apns-push-type".to_string(), "alert".to_string()),
            ("apns-priority".to_string(), "10".to_string()),
        ];
        if let Some(collapse_id) = collapse_id {
            headers.push(("apns-collapse-id".to_string(), collapse_id));
        }
        let url = format!("{host}/3/device/{device_token}");
        match (self.transport)(&url, headers, body.as_bytes().to_vec()).await {
            Ok(response) if response.status == 200 => {}
            Ok(response) => {
                let status = response.status;
                let reason = serde_json::from_str::<Value>(&response.body)
                    .ok()
                    .and_then(|parsed| {
                        parsed
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .unwrap_or_default();
                if status == 410 || DEAD_TOKEN_REASONS.contains(&reason.as_str()) {
                    self.remove_apns_token_from_all_sessions(device_token).await;
                } else {
                    let reason_text = if reason.is_empty() {
                        "unknown".to_string()
                    } else {
                        reason
                    };
                    tracing::warn!("[APNs] push failed status={status} reason={reason_text}");
                }
            }
            Err(error) => {
                tracing::warn!("[APNs] request error: {error}");
            }
        }
    }

    /// `sendViaRelay`: sign `ts.sortedTokens.title` and POST; drop tokens
    /// the relay flags.
    async fn send_via_relay(
        &self,
        device_tokens: &[String],
        payload: &Value,
        relay: &RelayConfig,
        environment: &str,
    ) {
        let tokens: Vec<String> = device_tokens
            .iter()
            .take(MAX_TOKENS_PER_RELAY_SEND)
            .cloned()
            .collect();
        let title = payload
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or("OMPChamber")
            .to_string();
        let (secret, public_jwk) = match self.get_or_create_relay_keypair().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("[APNs relay] request failed: {error}");
                return;
            }
        };
        let ts = now_ms();
        let mut sorted = tokens.clone();
        sorted.sort();
        let message = format!("{ts}.{}.{title}", sorted.join(","));
        let sig = crypto::b64url_encode(&crypto::sign_p1363(&secret, message.as_bytes()));
        let badge = payload
            .get("badge")
            .and_then(Value::as_f64)
            .filter(|badge| badge.is_finite() && *badge >= 0.0)
            .map(|badge| json!((badge.trunc() as i64) as u64));
        let collapse_id = payload
            .get("tag")
            .and_then(Value::as_str)
            .map(|tag| tag.chars().take(64).collect::<String>())
            .filter(|tag| !tag.is_empty());
        // JSON.stringify omits undefined fields — mirror that (the relay
        // schema rejects explicit nulls).
        let mut body = Map::new();
        body.insert("tokens".into(), json!(tokens));
        body.insert("title".into(), json!(title));
        body.insert(
            "body".into(),
            json!(payload.get("body").and_then(Value::as_str).unwrap_or("")),
        );
        if let Some(badge) = badge {
            body.insert("badge".into(), badge);
        }
        if let Some(collapse_id) = collapse_id {
            body.insert("collapseId".into(), json!(collapse_id));
        }
        body.insert("env".into(), json!(environment));
        if let Some(data) = payload.get("data").filter(|data| data.is_object()) {
            body.insert("data".into(), data.clone());
        }
        body.insert("publicKeyJwk".into(), Self::relay_public_jwk(&public_jwk));
        body.insert("ts".into(), json!(ts));
        body.insert("sig".into(), json!(sig));
        let serialized = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
        match (self.transport)(
            &relay.url,
            vec![("content-type".into(), "application/json".into())],
            serialized,
        )
        .await
        {
            Ok(response) if (200..300).contains(&response.status) => {
                let parsed: Option<Value> = serde_json::from_str(&response.body).ok();
                let results = parsed
                    .as_ref()
                    .and_then(|parsed| parsed.get("results"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for result in results {
                    let drop = result.get("drop") == Some(&Value::Bool(true));
                    let token = result.get("token").and_then(Value::as_str);
                    if drop && let Some(token) = token {
                        self.remove_apns_token_from_all_sessions(token).await;
                    }
                }
            }
            Ok(response) => {
                tracing::warn!("[APNs relay] send failed status={}", response.status);
            }
            Err(error) => {
                tracing::warn!("[APNs relay] request failed: {error}");
            }
        }
    }

    /// `sendViaDirectApns`.
    async fn send_via_direct_apns(
        &self,
        token_groups: &BTreeMap<&'static str, Vec<String>>,
        payload: &Value,
    ) {
        let Some(config) = self.resolve_apns_config().await else {
            if !self.warned_unconfigured.swap(true, Ordering::SeqCst) {
                tracing::warn!(
                    "[APNs] Relay disabled and no direct config; set OMPCHAMBER_APNS_KEY_ID / OMPCHAMBER_APNS_TEAM_ID / OMPCHAMBER_APNS_P8 for direct send."
                );
            }
            return;
        };
        let Some(jwt) = self.get_jwt(&config) else {
            tracing::warn!("[APNs] failed to sign provider token (invalid p8)");
            return;
        };
        let body = Self::build_body(payload);
        let tag = payload.get("tag").and_then(Value::as_str).map(String::from);
        for (environment, device_tokens) in token_groups {
            // One HTTP/2 session per environment: a sandbox token sent to
            // the production host gets BadDeviceToken and would be wrongly
            // dropped as dead.
            let effective_environment = config.environment.unwrap_or(environment);
            let host = if effective_environment == "sandbox" {
                APNS_HOST_SANDBOX
            } else {
                APNS_HOST_PRODUCTION
            };
            for token in device_tokens {
                self.send_one(host, token, &body, &jwt, &config.bundle_id, tag.as_deref())
                    .await;
            }
        }
    }

    /// `sendApnsToAllUiSessions`: NOT gated on UI visibility (a backgrounded
    /// WKWebView can't reliably report hidden; iOS suppresses the
    /// foreground banner instead — see APNS.md).
    pub async fn send_apns_to_all_ui_sessions(&self, payload: &Value) {
        let store = self.read_tokens_from_disk().await;
        let mut tokens_by_environment: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for record in store.values() {
            for entry in Self::normalize_tokens(record) {
                if seen.contains(&entry.device_token) {
                    continue;
                }
                seen.insert(entry.device_token.clone());
                tokens_by_environment
                    .entry(entry.environment)
                    .or_default()
                    .push(entry.device_token);
            }
        }
        if seen.is_empty() {
            return;
        }
        if let Some(relay) = Self::resolve_relay_config() {
            for (environment, device_tokens) in tokens_by_environment {
                let effective = relay.environment.unwrap_or(environment);
                self.send_via_relay(&device_tokens, payload, &relay, effective)
                    .await;
            }
            return;
        }
        self.send_via_direct_apns(&tokens_by_environment, payload)
            .await;
    }

    /// Test hook: inspect the persisted tokens store.
    #[cfg(test)]
    pub async fn tokens_for_test(&self) -> Map<String, Value> {
        self.read_tokens_from_disk().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::ENV_LOCK;
    use crate::notifications::transport::HttpPostResponse;

    type Recorded = Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notif-apns-{}-{}",
            std::process::id(),
            crypto::now_ms() * 1000 + rand::random::<u64>() % 100_000
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn store_for(dir: &PathBuf) -> Arc<SettingsStore> {
        crate::settings::store_for_path(&dir.join("settings.json"))
    }

    fn recording_transport(responses: Vec<(String, u16, Value)>) -> (HttpPost, Recorded) {
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded_for_transport = Arc::clone(&recorded);
        let transport: HttpPost = Arc::new(move |url, headers, body| {
            let recorded = Arc::clone(&recorded_for_transport);
            let responses = responses.clone();
            let url = url.to_string();
            Box::pin(async move {
                recorded.lock().unwrap_or_else(|e| e.into_inner()).push((
                    url.clone(),
                    headers,
                    body,
                ));
                let status = responses
                    .iter()
                    .find(|(prefix, _, _)| url.starts_with(prefix.as_str()))
                    .map(|(_, status, _)| *status)
                    .unwrap_or(200);
                let body = responses
                    .iter()
                    .find(|(prefix, _, _)| url.starts_with(prefix.as_str()))
                    .map(|(_, _, body)| body.clone())
                    .unwrap_or_else(|| json!({}));
                Ok(HttpPostResponse {
                    status,
                    body: serde_json::to_string(&body).unwrap_or_default(),
                })
            })
        });
        (transport, recorded)
    }

    fn post_bodies(recorded: &Recorded, prefix: &str) -> Vec<Value> {
        recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(url, _, _)| url.starts_with(prefix))
            .map(|(_, _, body)| serde_json::from_slice::<Value>(body).expect("json body"))
            .collect()
    }

    /// Mirror of the relay's verifier: prove the server's P-1363
    /// signatures verify against the published JWK.
    fn verify_relay_signature(jwk: &Value, message: &str, sig_b64: &str) -> bool {
        let public = crypto::jwk_public_key(jwk).expect("valid jwk");
        let sig = crypto::b64url_decode(sig_b64).expect("sig");
        crypto::verify_p1363(&public, message.as_bytes(), &sig)
    }

    fn set_env(name: &str, value: Option<&str>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    /// Clears the direct-mode flag on drop (panics included).
    struct DirectModeGuard;
    impl Drop for DirectModeGuard {
        fn drop(&mut self) {
            set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", None);
        }
    }

    struct RelayEnvGuard(());
    impl RelayEnvGuard {
        fn new(relay_url: Option<&str>) -> Self {
            set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", None);
            set_env("OMPCHAMBER_APNS_ENVIRONMENT", None);
            set_env("OMPCHAMBER_PUSH_RELAY_URL", relay_url);
            RelayEnvGuard(())
        }
    }
    impl Drop for RelayEnvGuard {
        fn drop(&mut self) {
            set_env("OMPCHAMBER_PUSH_RELAY_URL", None);
            set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", None);
            set_env("OMPCHAMBER_APNS_ENVIRONMENT", None);
        }
    }

    #[tokio::test]
    async fn registers_tokens_signed_and_posts_signed_generic_text_dropping_dead_tokens() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        let dir = temp_dir();
        let (transport, recorded) = recording_transport(vec![
            (
                "https://relay.test/v1/push/register-token".to_string(),
                200,
                json!({ "ok": true }),
            ),
            (
                "https://relay.test/v1/push/send".to_string(),
                200,
                json!({
                    "results": [
                        { "token": "tokenA", "ok": true, "drop": false },
                        { "token": "tokenDead", "ok": false, "drop": true },
                    ]
                }),
            ),
        ]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store_for(&dir), transport);

        runtime
            .add_or_update_apns_token("s1", "tokenA", None, None, None)
            .await;
        runtime
            .add_or_update_apns_token("s2", "tokenDead", None, None, None)
            .await;

        // Each new token is bound on the relay with a signed register call.
        let register_bodies = post_bodies(&recorded, "https://relay.test/v1/push/register-token");
        assert_eq!(register_bodies.len(), 2);
        for body in &register_bodies {
            assert_eq!(body["platform"], json!("ios"));
            assert!(body["ts"].as_u64().is_some());
            assert_eq!(body["publicKeyJwk"]["kty"], json!("EC"));
            assert_eq!(body["publicKeyJwk"]["crv"], json!("P-256"));
            let ts = body["ts"].as_u64().unwrap();
            let token = body["token"].as_str().unwrap();
            let platform = body["platform"].as_str().unwrap();
            let message = format!("{ts}.{token}.{platform}");
            assert!(verify_relay_signature(
                &body["publicKeyJwk"],
                &message,
                body["sig"].as_str().unwrap()
            ));
        }

        recorded.lock().unwrap_or_else(|e| e.into_inner()).clear();
        runtime
            .send_apns_to_all_ui_sessions(&json!({
                "title": "Agent response is ready",
                "body": "My session",
                "badge": 3,
                "tag": "ready-x",
                "data": { "sessionId": "sess1" },
            }))
            .await;

        let sends = post_bodies(&recorded, "https://relay.test/v1/push/send");
        assert_eq!(sends.len(), 1);
        let sent = &sends[0];
        let mut sorted_tokens: Vec<String> = sent["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token.as_str().unwrap().to_string())
            .collect();
        sorted_tokens.sort();
        assert_eq!(
            sorted_tokens,
            vec!["tokenA".to_string(), "tokenDead".to_string()]
        );
        assert_eq!(sent["title"], json!("Agent response is ready"));
        assert_eq!(sent["body"], json!("My session"));
        assert_eq!(sent["badge"], json!(3));
        assert_eq!(sent["env"], json!("production"));
        assert_eq!(sent["data"], json!({ "sessionId": "sess1" }));
        assert_eq!(sent["collapseId"], json!("ready-x"));
        assert_eq!(sent["publicKeyJwk"]["kty"], json!("EC"));
        let ts = sent["ts"].as_u64().unwrap();
        let sorted: Vec<String> = sent["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token.as_str().unwrap().to_string())
            .collect();
        let mut sorted = sorted;
        sorted.sort();
        let send_message = format!(
            "{ts}.{}.{title}",
            sorted.join(","),
            title = sent["title"].as_str().unwrap()
        );
        assert!(verify_relay_signature(
            &sent["publicKeyJwk"],
            &send_message,
            sent["sig"].as_str().unwrap()
        ));

        // tokenDead was dropped → the next send targets only tokenA.
        recorded.lock().unwrap_or_else(|e| e.into_inner()).clear();
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "x", "body": "y", "tag": "t" }))
            .await;
        let sends = post_bodies(&recorded, "https://relay.test/v1/push/send");
        assert_eq!(sends[0]["tokens"], json!(["tokenA"]));
    }

    #[tokio::test]
    async fn reuses_one_persisted_keypair_across_register_and_send() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        let dir = temp_dir();
        let store = store_for(&dir);
        let (transport, recorded) = recording_transport(vec![(
            "https://relay.test".to_string(),
            200,
            json!({ "ok": true, "results": [] }),
        )]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), Arc::clone(&store), transport);
        runtime
            .add_or_update_apns_token("s1", "tokenA", None, None, None)
            .await;
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b", "tag": "x" }))
            .await;

        let jwks: Vec<String> = post_bodies(&recorded, "https://relay.test")
            .iter()
            .map(|body| serde_json::to_string(&body["publicKeyJwk"]).unwrap_or_default())
            .collect();
        assert!(jwks.len() >= 2);
        assert!(jwks.iter().all(|jwk| *jwk == jwks[0]));
        // The keypair persisted once and reads back.
        let settings = store.read_migrated().await.unwrap_or_default();
        assert!(settings.get("relaySigningKey").is_some());
        let (secret, public_jwk) = runtime.get_or_create_relay_keypair().await.expect("pair");
        assert_eq!(
            serde_json::to_string(&crypto::public_jwk_value(&secret.public_key())).unwrap(),
            serde_json::to_string(&public_jwk).unwrap()
        );
    }

    #[tokio::test]
    async fn honors_an_explicit_sandbox_environment_override_for_every_token() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        set_env("OMPCHAMBER_APNS_ENVIRONMENT", Some("sandbox"));
        let dir = temp_dir();
        let (transport, recorded) = recording_transport(vec![(
            "https://relay.test".to_string(),
            200,
            json!({ "ok": true, "results": [] }),
        )]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store_for(&dir), transport);
        runtime
            .add_or_update_apns_token("s1", "tokenA", None, Some("ios"), Some("production"))
            .await;
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b" }))
            .await;
        let sends = post_bodies(&recorded, "https://relay.test/v1/push/send");
        assert_eq!(sends[0]["env"], json!("sandbox"));
    }

    #[tokio::test]
    async fn routes_each_token_to_its_registered_environment() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        let dir = temp_dir();
        let (transport, recorded) = recording_transport(vec![(
            "https://relay.test".to_string(),
            200,
            json!({ "ok": true, "results": [] }),
        )]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store_for(&dir), transport);
        runtime
            .add_or_update_apns_token("s1", "tokenXcode", None, Some("ios"), Some("sandbox"))
            .await;
        runtime
            .add_or_update_apns_token("s2", "tokenStore", None, Some("ios"), Some("production"))
            .await;
        // No environment → production (legacy entries).
        runtime
            .add_or_update_apns_token("s3", "tokenLegacy", None, None, None)
            .await;

        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b" }))
            .await;
        let sends = post_bodies(&recorded, "https://relay.test/v1/push/send");
        assert_eq!(sends.len(), 2);
        let by_env = |env: &str| {
            sends
                .iter()
                .find(|body| body["env"] == json!(env))
                .map(|body| body["tokens"].clone())
        };
        assert_eq!(by_env("sandbox"), Some(json!(["tokenXcode"])));
        let production = by_env("production").expect("production group");
        let mut tokens: Vec<String> = production
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token.as_str().unwrap().to_string())
            .collect();
        tokens.sort();
        assert_eq!(
            tokens,
            vec!["tokenLegacy".to_string(), "tokenStore".to_string()]
        );
    }

    #[tokio::test]
    async fn no_ops_when_no_tokens_are_registered() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        let dir = temp_dir();
        let (transport, recorded) =
            recording_transport(vec![("https://relay.test".to_string(), 200, json!({}))]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store_for(&dir), transport);
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b" }))
            .await;
        assert!(
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    fn test_p8_config() -> (ApnsConfig, &'static str) {
        // A real P-256 key exported as a PKCS#8 PEM so the ES256 signing
        // path runs for real (mirrors the JS test setup).
        let secret = crypto::generate_secret_key();
        let scalar = secret.to_bytes();
        let mut sec1: Vec<u8> = vec![0x30];
        let mut inner: Vec<u8> = vec![0x02, 0x01, 0x01, 0x04, 0x20];
        inner.extend_from_slice(scalar.as_slice());
        inner.extend_from_slice(&[0xA0, 0x07, 0x06, 0x05, 0x2B, 0x81, 0x04, 0x00, 0x22]);
        sec1.push(inner.len() as u8);
        sec1.extend_from_slice(&inner);
        let mut pkcs8: Vec<u8> = vec![0x30];
        let mut inner8: Vec<u8> = vec![0x02, 0x01, 0x00];
        inner8.extend_from_slice(&[0x30, 0x13]);
        inner8.extend_from_slice(&[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]);
        inner8.extend_from_slice(&[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]);
        inner8.push(0x04);
        inner8.push(sec1.len() as u8);
        inner8.extend_from_slice(&sec1);
        pkcs8.push(inner8.len() as u8);
        pkcs8.extend_from_slice(&inner8);
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&pkcs8)
        };
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n");
        (
            ApnsConfig {
                key_id: "KEY123".to_string(),
                team_id: "TEAM123".to_string(),
                p8: pem,
                bundle_id: "com.openchamber.app".to_string(),
                environment: Some("sandbox"),
            },
            "direct",
        )
    }

    #[tokio::test]
    async fn direct_mode_environment_defaults_to_per_token_routing() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir();
        let store = store_for(&dir);
        let (config, _) = test_p8_config();
        let mut no_environment = config.clone();
        no_environment.environment = None;
        let mut settings = Map::new();
        settings.insert(
            "apnsConfig".to_string(),
            json!({
                "keyId": no_environment.key_id,
                "teamId": no_environment.team_id,
                "p8": no_environment.p8,
                "bundleId": no_environment.bundle_id,
            }),
        );
        store.write_raw(&settings).await.expect("settings");
        let (transport, recorded) =
            recording_transport(vec![("https://api".to_string(), 200, json!({}))]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store, transport);
        let resolved = runtime.resolve_apns_config().await.expect("config");
        assert_eq!(resolved.environment, None);

        // Relay disabled → direct mode with per-token environment hosts
        // (guard-equivalent clearing of relay state first).
        let _direct = DirectModeGuard;
        set_env("OMPCHAMBER_PUSH_RELAY_URL", None);
        set_env("OMPCHAMBER_APNS_ENVIRONMENT", None);
        set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", Some("true"));
        runtime
            .add_or_update_apns_token("s1", "tokenXcode", None, Some("ios"), Some("sandbox"))
            .await;
        runtime
            .add_or_update_apns_token("s2", "tokenStore", None, Some("ios"), Some("production"))
            .await;
        // Relay-disabled register calls no-op.
        assert!(
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b", "tag": "ready-x" }))
            .await;
        let calls = recorded.lock().unwrap_or_else(|e| e.into_inner());
        let by_host = |host: &str| {
            calls
                .iter()
                .filter(|(url, _, _)| url.starts_with(host))
                .map(|(url, _, _)| {
                    url.rsplit("/3/device/")
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(by_host(APNS_HOST_SANDBOX), vec!["tokenXcode"]);
        assert_eq!(by_host(APNS_HOST_PRODUCTION), vec!["tokenStore"]);
        // Direct sends carry the APNs headers and a signed bearer JWT.
        let (_, headers, body) = &calls[0];
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert!(
            header("authorization")
                .expect("bearer")
                .starts_with("bearer ey")
        );
        assert_eq!(header("apns-topic").as_deref(), Some("com.openchamber.app"));
        assert_eq!(header("apns-push-type").as_deref(), Some("alert"));
        assert_eq!(header("apns-priority").as_deref(), Some("10"));
        assert_eq!(header("apns-collapse-id").as_deref(), Some("ready-x"));
        let parsed: Value = serde_json::from_slice(body).expect("body json");
        assert_eq!(parsed["aps"]["alert"]["title"], json!("t"));
        assert_eq!(parsed["aps"]["sound"], json!("default"));
        assert_eq!(parsed["aps"]["mutable-content"], json!(1));
        assert_eq!(parsed["aps"]["thread-id"], json!("ready-x"));
    }

    #[tokio::test]
    async fn drops_dead_direct_tokens_on_410_and_bad_reasons() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir();
        let store = store_for(&dir);
        let (config, _) = test_p8_config();
        let mut settings = Map::new();
        settings.insert(
            "apnsConfig".to_string(),
            json!({
                "keyId": config.key_id,
                "teamId": config.team_id,
                "p8": config.p8,
                "bundleId": config.bundle_id,
            }),
        );
        store.write_raw(&settings).await.expect("settings");
        let _direct = DirectModeGuard;
        set_env("OMPCHAMBER_PUSH_RELAY_URL", None);
        set_env("OMPCHAMBER_APNS_ENVIRONMENT", None);
        set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", Some("true"));

        let (transport, recorded) = recording_transport(vec![
            (
                "https://api.push.apple.com/3/device/tokenBad".to_string(),
                400,
                json!({ "reason": "BadDeviceToken" }),
            ),
            (
                "https://api.push.apple.com/3/device/tokenOk".to_string(),
                200,
                json!({}),
            ),
            (
                "https://api.sandbox.push.apple.com".to_string(),
                200,
                json!({}),
            ),
        ]);
        let runtime = ApnsRuntime::new(dir.join("apns-tokens.json"), store, transport);
        runtime
            .add_or_update_apns_token("s1", "tokenBad", None, None, None)
            .await;
        runtime
            .add_or_update_apns_token("s2", "tokenOk", None, None, None)
            .await;
        runtime
            .send_apns_to_all_ui_sessions(&json!({ "title": "t", "body": "b" }))
            .await;
        let tokens = runtime.tokens_for_test().await;
        let all_tokens: Vec<String> = tokens
            .values()
            .flat_map(|record| {
                ApnsRuntime::normalize_tokens(record)
                    .into_iter()
                    .map(|entry| entry.device_token)
            })
            .collect();
        assert_eq!(all_tokens, vec!["tokenOk".to_string()]);
        let _ = recorded;
    }

    #[test]
    fn sign_apns_jwt_produces_a_three_part_es256_token() {
        let (config, _) = test_p8_config();
        let jwt = ApnsRuntime::sign_apns_jwt(&config).expect("jwt");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&crypto::b64url_decode(parts[0]).expect("header bytes"))
                .expect("header");
        let claims: Value =
            serde_json::from_slice(&crypto::b64url_decode(parts[1]).expect("claims bytes"))
                .expect("claims");
        assert_eq!(header, json!({ "alg": "ES256", "kid": "KEY123" }));
        assert_eq!(claims["iss"], json!("TEAM123"));
        assert!(claims["iat"].as_u64().is_some());
    }

    #[test]
    fn relay_url_derivation_strips_the_send_suffix() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = RelayEnvGuard::new(Some("https://relay.test/v1/push/send"));
        let relay = ApnsRuntime::resolve_relay_config().expect("relay");
        assert_eq!(relay.url, "https://relay.test/v1/push/send");
        assert_eq!(
            relay.register_url,
            "https://relay.test/v1/push/register-token"
        );
        assert_eq!(relay.environment, None);

        set_env("OMPCHAMBER_APNS_ENVIRONMENT", Some("production"));
        assert_eq!(
            ApnsRuntime::resolve_relay_config()
                .expect("relay")
                .environment,
            Some("production")
        );
        set_env("OMPCHAMBER_PUSH_RELAY_DISABLED", Some("true"));
        assert!(ApnsRuntime::resolve_relay_config().is_none());
    }

    #[test]
    fn badge_and_body_omitted_fields_mirror_json_stringify() {
        let body = ApnsRuntime::build_body(&json!({
            "title": "T",
            "badge": 3.7,
            "tag": "tag-1",
            "data": { "sessionId": "s" },
        }));
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["aps"]["alert"]["title"], json!("T"));
        // body absent → key omitted.
        assert!(parsed["aps"]["alert"].get("body").is_none());
        assert_eq!(parsed["aps"]["badge"], json!(3));
        assert_eq!(parsed["sessionId"], json!("s"));

        let minimal = ApnsRuntime::build_body(&json!({}));
        let parsed: Value = serde_json::from_str(&minimal).expect("json");
        assert!(parsed["aps"].get("badge").is_none());
        assert!(parsed["aps"].get("thread-id").is_none());
        assert_eq!(parsed["aps"]["sound"], json!("default"));
    }
}
