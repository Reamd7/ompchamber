//! Port of `server/lib/notifications/push-runtime.js`: web-push
//! subscription persistence (data-dir `push-subscriptions.json`), VAPID
//! key lifecycle, web-push sending (RFC 8291 + 8292, see
//! [`crate::notifications::crypto`]), and the UI visibility heartbeat
//! model (30s TTL).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::notifications::crypto;
use crate::notifications::transport::HttpPost;
use crate::settings::SettingsStore;

pub const PUSH_SUBSCRIPTIONS_VERSION: u64 = 1;
pub const UI_VISIBILITY_TTL_MS: u64 = 30_000;
const MAX_SUBSCRIPTIONS_PER_SESSION: usize = 10;

/// `isLoopbackHttpOrigin` (push-runtime.js).
fn is_loopback_http_origin(value: &str) -> bool {
    value.starts_with("http://localhost")
        || value.starts_with("http://127.0.0.1")
        || value.starts_with("http://[::1]")
}

fn now_ms() -> u64 {
    crypto::now_ms()
}

/// A client is "mobile" if it reports a native mobile platform; anything
/// else (web, desktop, vscode, older clients) counts as interactive.
fn is_mobile_platform(platform: Option<&str>) -> bool {
    matches!(platform, Some("ios") | Some("android"))
}

#[derive(Debug, Clone)]
struct VisibilityState {
    visible: bool,
    updated_at: u64,
    platform: Option<String>,
}

/// The `webPush.setVapidDetails(subject, publicKey, privateKey)` state.
#[derive(Debug, Clone)]
pub struct VapidDetails {
    pub subject: String,
    pub public_key: String,
    pub private_key: String,
}

/// `normalizePushSubscriptions`: only entries whose endpoint/p256dh/auth
/// are strings survive; `createdAt` becomes null when not a number, and
/// `lastSeenAt`/`userAgent` do not survive normalization (as in JS).
#[derive(Debug, Clone)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub created_at: Option<u64>,
    pub platform: Option<String>,
}

impl PushSubscription {
    fn to_json(&self) -> Value {
        json!({
            "endpoint": self.endpoint,
            "p256dh": self.p256dh,
            "auth": self.auth,
            "createdAt": self.created_at,
            "platform": self.platform,
        })
    }
}

pub struct PushRuntime {
    subscriptions_path: PathBuf,
    store: Arc<SettingsStore>,
    transport: HttpPost,
    /// Serializes read-modify-write cycles (the JS `persistPushSubscriptionsLock`).
    persist_lock: tokio::sync::Mutex<()>,
    vapid: tokio::sync::Mutex<Option<VapidDetails>>,
    push_initialized: AtomicBool,
    visibility: Mutex<HashMap<String, VisibilityState>>,
}

impl PushRuntime {
    pub fn new(
        subscriptions_path: PathBuf,
        store: Arc<SettingsStore>,
        transport: HttpPost,
    ) -> Arc<Self> {
        Arc::new(Self {
            subscriptions_path,
            store,
            transport,
            persist_lock: tokio::sync::Mutex::new(()),
            vapid: tokio::sync::Mutex::new(None),
            push_initialized: AtomicBool::new(false),
            visibility: Mutex::new(HashMap::new()),
        })
    }

    // -----------------------------------------------------------------------
    // Subscription persistence
    // -----------------------------------------------------------------------

    async fn read_subscriptions_from_disk(&self) -> Map<String, Value> {
        let raw = match tokio::fs::read_to_string(&self.subscriptions_path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Map::new(),
            Err(error) => {
                tracing::warn!("Failed to read push subscriptions file: {error}");
                return Map::new();
            }
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!("Failed to read push subscriptions file: {error}");
                return Map::new();
            }
        };
        let Some(object) = parsed.as_object() else {
            return Map::new();
        };
        if object.get("version").and_then(Value::as_u64) != Some(PUSH_SUBSCRIPTIONS_VERSION) {
            return Map::new();
        }
        match object
            .get("subscriptionsBySession")
            .and_then(Value::as_object)
        {
            Some(sessions) => sessions.clone(),
            None => Map::new(),
        }
    }

    async fn write_subscriptions_to_disk(&self, sessions: &Map<String, Value>) {
        let document = json!({
            "version": PUSH_SUBSCRIPTIONS_VERSION,
            "subscriptionsBySession": Value::Object(sessions.clone()),
        });
        let body = serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_string());
        if let Some(parent) = self.subscriptions_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(error) = tokio::fs::write(&self.subscriptions_path, body).await {
            tracing::warn!("Failed to write push subscriptions file: {error}");
        }
    }

    /// `persistPushSubscriptionUpdate`: serialized read → mutate → write.
    async fn persist_update<F>(&self, mutate: F)
    where
        F: FnOnce(Map<String, Value>) -> Map<String, Value> + Send,
    {
        let _guard = self.persist_lock.lock().await;
        let current = self.read_subscriptions_from_disk().await;
        let next = mutate(current);
        self.write_subscriptions_to_disk(&next).await;
    }

    fn normalize_subscriptions(record: &Value) -> Vec<PushSubscription> {
        let Some(entries) = record.as_array() else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| {
                if !entry.is_object() {
                    return None;
                }
                Some(PushSubscription {
                    endpoint: entry.get("endpoint")?.as_str()?.to_string(),
                    p256dh: entry.get("p256dh")?.as_str()?.to_string(),
                    auth: entry.get("auth")?.as_str()?.to_string(),
                    created_at: entry.get("createdAt").and_then(Value::as_u64),
                    platform: entry
                        .get("platform")
                        .and_then(Value::as_str)
                        .map(String::from),
                })
            })
            .collect()
    }

    /// `addOrUpdatePushSubscription`.
    pub async fn add_or_update_push_subscription(
        &self,
        ui_session_token: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        user_agent: Option<&str>,
        platform: Option<&str>,
    ) {
        if ui_session_token.is_empty() {
            return;
        }
        self.ensure_push_initialized().await;
        let now = now_ms();

        self.persist_update(move |mut sessions| {
            let existing = sessions
                .get(ui_session_token)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // Keep every other entry; remember the same-endpoint platform.
            let mut filtered: Vec<Value> = Vec::with_capacity(existing.len() + 1);
            let mut previous_platform: Option<String> = None;
            for entry in &existing {
                match entry.get("endpoint").and_then(Value::as_str) {
                    Some(entry_endpoint) if entry_endpoint == endpoint => {
                        previous_platform = entry
                            .get("platform")
                            .and_then(Value::as_str)
                            .map(String::from);
                    }
                    Some(_) => filtered.push(entry.clone()),
                    None => {} // entries without a string endpoint are dropped
                }
            }
            let platform_value = platform
                .filter(|platform| !platform.is_empty())
                .map(String::from)
                .or(previous_platform);
            let mut entry = Map::new();
            entry.insert("endpoint".into(), Value::String(endpoint.to_string()));
            entry.insert("p256dh".into(), Value::String(p256dh.to_string()));
            entry.insert("auth".into(), Value::String(auth.to_string()));
            entry.insert("createdAt".into(), json!(now));
            entry.insert("lastSeenAt".into(), json!(now));
            if let Some(user_agent) = user_agent.filter(|agent| !agent.is_empty()) {
                entry.insert("userAgent".into(), Value::String(user_agent.to_string()));
            }
            if let Some(platform_value) = platform_value {
                entry.insert("platform".into(), Value::String(platform_value));
            }
            filtered.insert(0, Value::Object(entry));
            filtered.truncate(MAX_SUBSCRIPTIONS_PER_SESSION);
            sessions.insert(ui_session_token.to_string(), Value::Array(filtered));
            sessions
        })
        .await;
    }

    /// `removePushSubscription`.
    pub async fn remove_push_subscription(&self, ui_session_token: &str, endpoint: &str) {
        if ui_session_token.is_empty() || endpoint.is_empty() {
            return;
        }
        self.ensure_push_initialized().await;
        self.persist_update(move |mut sessions| {
            let Some(record) = sessions.get(ui_session_token).cloned() else {
                return sessions;
            };
            let kept: Vec<Value> = Self::normalize_subscriptions(&record)
                .into_iter()
                .filter(|subscription| subscription.endpoint != endpoint)
                .map(|subscription| subscription.to_json())
                .collect();
            if kept.is_empty() {
                sessions.remove(ui_session_token);
            } else {
                sessions.insert(ui_session_token.to_string(), Value::Array(kept));
            }
            sessions
        })
        .await;
    }

    /// `removePushSubscriptionFromAllSessions`.
    pub async fn remove_push_subscription_from_all_sessions(&self, endpoint: &str) {
        if endpoint.is_empty() {
            return;
        }
        self.persist_update(move |mut sessions| {
            let keys: Vec<String> = sessions.keys().cloned().collect();
            for key in keys {
                let Some(record) = sessions.get(&key).cloned() else {
                    continue;
                };
                if !record.is_array() {
                    continue;
                }
                let kept: Vec<Value> = Self::normalize_subscriptions(&record)
                    .into_iter()
                    .filter(|subscription| subscription.endpoint != endpoint)
                    .map(|subscription| subscription.to_json())
                    .collect();
                if kept.is_empty() {
                    sessions.remove(&key);
                } else {
                    sessions.insert(key, Value::Array(kept));
                }
            }
            sessions
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // VAPID
    // -----------------------------------------------------------------------

    /// `getOrCreateVapidKeys`: persisted under `settings.vapidKeys`,
    /// generated on first use.
    pub async fn get_or_create_vapid_keys(&self) -> Result<(String, String), std::io::Error> {
        let settings = self.store.read_migrated().await.unwrap_or_default();
        if let Some(keys) = settings.get("vapidKeys").and_then(Value::as_object) {
            let public_key = keys.get("publicKey").and_then(Value::as_str);
            let private_key = keys.get("privateKey").and_then(Value::as_str);
            if let (Some(public_key), Some(private_key)) = (public_key, private_key) {
                return Ok((public_key.to_string(), private_key.to_string()));
            }
        }
        let (public_key, private_key) = crypto::generate_vapid_keys();
        let mut next = settings;
        next.insert(
            "vapidKeys".to_string(),
            json!({
                "publicKey": public_key,
                "privateKey": private_key,
            }),
        );
        self.store
            .write_raw(&next)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok((public_key, private_key))
    }

    /// `resolveVapidSubject`.
    async fn resolve_vapid_subject(&self) -> String {
        let trimmed_env = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        if let Some(subject) = trimmed_env("OMPCHAMBER_VAPID_SUBJECT") {
            return subject;
        }
        if let Some(origin) = trimmed_env("OMPCHAMBER_PUBLIC_ORIGIN") {
            if is_loopback_http_origin(&origin) {
                return "mailto:ompchamber@localhost".to_string();
            }
            return origin;
        }
        let settings = self.store.read_migrated().await.unwrap_or_default();
        if let Some(origin) = settings
            .get("publicOrigin")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
        {
            if is_loopback_http_origin(origin) {
                return "mailto:ompchamber@localhost".to_string();
            }
            return origin.to_string();
        }
        "mailto:ompchamber@localhost".to_string()
    }

    /// `ensurePushInitialized`.
    pub async fn ensure_push_initialized(&self) {
        if self.push_initialized.load(Ordering::SeqCst) {
            return;
        }
        let keys = match self.get_or_create_vapid_keys().await {
            Ok(keys) => keys,
            Err(error) => {
                tracing::warn!("[Push] Failed to load VAPID key: {error}");
                return;
            }
        };
        let subject = self.resolve_vapid_subject().await;
        if subject == "mailto:ompchamber@localhost" {
            tracing::warn!(
                "[Push] No public origin configured for VAPID; set OMPCHAMBER_VAPID_SUBJECT or enable push once from a real origin."
            );
        }
        *self.vapid.lock().await = Some(VapidDetails {
            subject,
            public_key: keys.0,
            private_key: keys.1,
        });
        self.push_initialized.store(true, Ordering::SeqCst);
    }

    /// `setPushInitialized`.
    pub fn set_push_initialized(&self, value: bool) {
        self.push_initialized.store(value, Ordering::SeqCst);
    }

    /// Test access to the initialized VAPID details.
    #[cfg(test)]
    pub async fn vapid_details_for_test(&self) -> Option<VapidDetails> {
        self.vapid.lock().await.clone()
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    /// `sendPushToSubscription`: encrypt + POST. Returns the push-service
    /// status code, or `None` for pre-send failures and transport errors
    /// (which never drop the subscription).
    async fn send_push_to_subscription(
        &self,
        subscription: &PushSubscription,
        payload: &str,
    ) -> Option<u16> {
        self.ensure_push_initialized().await;
        let vapid = self.vapid.lock().await.clone()?;
        let body = crypto::encrypt_web_push_payload(
            &subscription.p256dh,
            &subscription.auth,
            payload.as_bytes(),
        )?;
        let authorization = crypto::vapid_authorization_header(
            &vapid.private_key,
            &vapid.subject,
            &subscription.endpoint,
            now_ms(),
        )?;
        let headers = vec![
            ("TTL".to_string(), crypto::WEB_PUSH_TTL_SECONDS.to_string()),
            ("Content-Encoding".to_string(), "aes128gcm".to_string()),
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("Authorization".to_string(), authorization),
        ];
        let result = (self.transport)(&subscription.endpoint, headers, body).await;
        match result {
            Ok(response) => Some(response.status),
            Err(error) => {
                tracing::warn!("[Push] Failed to send notification: {error}");
                None
            }
        }
    }

    /// `sendPushToAllUiSessions`: dedupe by endpoint across sessions, apply
    /// the `requireNoSse` presence gate per platform, send in parallel.
    /// 410/404 responses remove the endpoint from every session.
    pub async fn send_push_to_all_ui_sessions(
        self: &Arc<Self>,
        payload: &Value,
        require_no_sse: bool,
    ) {
        let sessions = self.read_subscriptions_from_disk().await;
        let mut by_endpoint: Vec<PushSubscription> = Vec::new();
        for record in sessions.values() {
            for subscription in Self::normalize_subscriptions(record) {
                if !by_endpoint
                    .iter()
                    .any(|existing| existing.endpoint == subscription.endpoint)
                {
                    by_endpoint.push(subscription);
                }
            }
        }
        let payload_text = serde_json::to_string(payload).unwrap_or_default();
        let mut sends: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for subscription in by_endpoint {
            if require_no_sse {
                // Mobile PWA subscriptions follow the native presence model
                // (suppress only when an interactive client is visible);
                // desktop/web keep the any-visible gate.
                let suppressed = if is_mobile_platform(subscription.platform.as_deref()) {
                    self.is_any_interactive_client_visible()
                } else {
                    self.is_any_ui_visible()
                };
                if suppressed {
                    continue;
                }
            }
            let runtime = Arc::clone(self);
            let payload_text = payload_text.clone();
            sends.push(tokio::spawn(async move {
                let endpoint = runtime_endpoint(&subscription);
                match runtime
                    .send_push_to_subscription(&subscription, &payload_text)
                    .await
                {
                    Some(410 | 404) => {
                        runtime
                            .remove_push_subscription_from_all_sessions(&endpoint)
                            .await;
                    }
                    Some(status) if !(200..300).contains(&status) => {
                        tracing::warn!("[Push] Failed to send notification: status={status}");
                    }
                    _ => {}
                }
            }));
        }
        for send in sends {
            let _ = send.await;
        }
    }

    // -----------------------------------------------------------------------
    // Visibility
    // -----------------------------------------------------------------------

    /// `updateUiVisibility`.
    pub fn update_ui_visibility(&self, token: &str, visible: bool, platform: Option<&str>) {
        if token.is_empty() {
            return;
        }
        let now = now_ms();
        let mut visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        let existing = visibility.get(token);
        // Keep the last known platform when the beacon omits one.
        let next_platform = platform
            .filter(|platform| !platform.is_empty())
            .map(String::from)
            .or_else(|| existing.and_then(|state| state.platform.clone()));
        visibility.insert(
            token.to_string(),
            VisibilityState {
                visible,
                updated_at: now,
                platform: next_platform,
            },
        );
    }

    fn prune_ui_visibility(&self, now: u64) {
        let mut visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.retain(|_, state| now.saturating_sub(state.updated_at) <= UI_VISIBILITY_TTL_MS);
    }

    /// `isAnyUiVisible`.
    pub fn is_any_ui_visible(&self) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.values().any(|state| state.visible)
    }

    /// `isAnyInteractiveClientVisible`: at least one non-mobile client is
    /// currently visible.
    pub fn is_any_interactive_client_visible(&self) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility
            .values()
            .any(|state| state.visible && !is_mobile_platform(state.platform.as_deref()))
    }

    /// `isUiVisible`.
    pub fn is_ui_visible(&self, token: &str) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.get(token).is_some_and(|state| state.visible)
    }

    /// Test hook: age every visibility beacon past the TTL.
    #[cfg(test)]
    pub fn expire_visibility_for_test(&self) {
        let mut visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        for state in visibility.values_mut() {
            state.updated_at = state.updated_at.saturating_sub(UI_VISIBILITY_TTL_MS + 1);
        }
    }
}

fn runtime_endpoint(subscription: &PushSubscription) -> String {
    subscription.endpoint.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::crypto::{b64url_encode, generate_secret_key, public_to_b64url};

    /// Canonical base64url auth secret (browsers emit canonical base64;
    /// strict engines reject sloppy trailing bits).
    const AUTH_SECRET_B64: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo";
    use crate::notifications::transport::HttpPostResponse;
    use std::time::Duration;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notif-push-{}-{}",
            std::process::id(),
            crypto::now_ms() * 1000 + rand::random::<u64>() % 100_000
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn store_for(dir: &PathBuf) -> Arc<SettingsStore> {
        crate::settings::store_for_path(&dir.join("settings.json"))
    }

    fn fake_transport(
        status: u16,
    ) -> (
        HttpPost,
        Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>,
    ) {
        let requests: Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let requests_for_transport = Arc::clone(&requests);
        let transport: HttpPost = Arc::new(move |url, headers, body| {
            let requests = Arc::clone(&requests_for_transport);
            let url = url.to_string();
            Box::pin(async move {
                requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((url, headers, body));
                Ok(HttpPostResponse {
                    status,
                    body: String::new(),
                })
            })
        });
        (transport, requests)
    }

    #[tokio::test]
    async fn keeps_visible_ui_state_when_another_client_reports_hidden() {
        let dir = temp_dir();
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);

        runtime.update_ui_visibility("visible-client", true, None);
        runtime.update_ui_visibility("hidden-client", false, None);

        assert!(runtime.is_any_ui_visible());
        assert!(runtime.is_ui_visible("visible-client"));
        assert!(!runtime.is_ui_visible("hidden-client"));

        runtime.expire_visibility_for_test();
        assert!(!runtime.is_any_ui_visible());
        assert!(!runtime.is_ui_visible("visible-client"));
    }

    #[tokio::test]
    async fn only_mobile_platforms_are_non_interactive() {
        let dir = temp_dir();
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);

        runtime.update_ui_visibility("phone", true, Some("ios"));
        assert!(runtime.is_any_ui_visible());
        assert!(!runtime.is_any_interactive_client_visible());

        runtime.update_ui_visibility("desktop", true, Some("desktop"));
        assert!(runtime.is_any_interactive_client_visible());

        runtime.update_ui_visibility("desktop", false, Some("desktop"));
        assert!(!runtime.is_any_interactive_client_visible());

        // No platform reported → conservative: interactive.
        runtime.update_ui_visibility("legacy", true, None);
        assert!(runtime.is_any_interactive_client_visible());
    }

    #[tokio::test]
    async fn remembers_the_last_platform_when_a_heartbeat_omits_it() {
        let dir = temp_dir();
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);
        runtime.update_ui_visibility("phone", true, Some("android"));
        runtime.update_ui_visibility("phone", true, None);
        assert!(!runtime.is_any_interactive_client_visible());
    }

    #[tokio::test]
    async fn persists_subscriptions_with_platform_inheritance_and_cap() {
        let dir = temp_dir();
        let path = dir.join("subs.json");
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(path.clone(), store_for(&dir), transport);

        runtime
            .add_or_update_push_subscription(
                "ui-1",
                "https://e/1",
                "pk1",
                "auth1",
                Some("UA"),
                Some("ios"),
            )
            .await;
        // Same endpoint re-registered without a platform → keep ios.
        runtime
            .add_or_update_push_subscription("ui-1", "https://e/1", "pk1", "auth1", None, None)
            .await;
        for index in 0..8 {
            runtime
                .add_or_update_push_subscription(
                    "ui-1",
                    &format!("https://e/{index}"),
                    "pk",
                    AUTH_SECRET_B64,
                    None,
                    None,
                )
                .await;
        }

        let raw = tokio::fs::read_to_string(&path).await.expect("file");
        let parsed: Value = serde_json::from_str(&raw).expect("json");
        let entries = parsed["subscriptionsBySession"]["ui-1"]
            .as_array()
            .expect("entries");
        // e/1 is within e/0..e/7, so the store holds exactly the 8 distinct
        // endpoints (e/1 itself re-registered and moved to the head once).
        assert_eq!(entries.len(), 8, "eight distinct endpoints");
        assert_eq!(entries[0]["endpoint"], json!("https://e/7"));
        let reregistered = entries
            .iter()
            .find(|entry| entry["endpoint"] == json!("https://e/1"))
            .expect("re-registered entry");
        assert_eq!(reregistered["platform"], json!("ios"));
        assert_eq!(reregistered["createdAt"], reregistered["lastSeenAt"]);
        assert_eq!(parsed["version"], json!(1));
    }

    #[tokio::test]
    async fn removal_drops_the_session_key_when_empty() {
        let dir = temp_dir();
        let path = dir.join("subs.json");
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(path.clone(), store_for(&dir), transport);
        runtime
            .add_or_update_push_subscription("ui-1", "https://e/1", "pk", "auth", None, None)
            .await;
        runtime
            .remove_push_subscription("ui-1", "https://e/1")
            .await;
        let parsed: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.expect("file"))
                .expect("json");
        assert!(
            parsed["subscriptionsBySession"]
                .as_object()
                .expect("map")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn vapid_keys_generate_once_and_round_trip_through_settings() {
        let dir = temp_dir();
        let store = store_for(&dir);
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), Arc::clone(&store), transport);
        let (public_key, private_key) = runtime.get_or_create_vapid_keys().await.expect("keys");
        assert!(!public_key.is_empty() && !private_key.is_empty());
        let (public_key_2, _) = runtime.get_or_create_vapid_keys().await.expect("keys");
        assert_eq!(public_key, public_key_2);
        let settings = store.read_migrated().await.unwrap_or_default();
        assert_eq!(settings["vapidKeys"]["publicKey"], json!(public_key));
        assert_eq!(settings["vapidKeys"]["privateKey"], json!(private_key));
    }

    #[tokio::test]
    async fn sends_encrypted_web_push_and_drops_dead_endpoints() {
        let dir = temp_dir();
        let status = Arc::new(Mutex::new(201u16));
        let requests: Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let requests_for_transport = Arc::clone(&requests);
        let status_for_transport = Arc::clone(&status);
        let transport: HttpPost = Arc::new(move |url, headers, body| {
            let requests = Arc::clone(&requests_for_transport);
            let status = Arc::clone(&status_for_transport);
            let url = url.to_string();
            Box::pin(async move {
                requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((url, headers, body));
                Ok(HttpPostResponse {
                    status: *status.lock().unwrap_or_else(|e| e.into_inner()),
                    body: String::new(),
                })
            })
        });
        let path = dir.join("subs.json");
        let runtime = PushRuntime::new(path.clone(), store_for(&dir), transport);

        let ua_secret = generate_secret_key();
        let ua_public = public_to_b64url(&ua_secret.public_key());
        runtime
            .add_or_update_push_subscription(
                "ui-1",
                "https://push/e1",
                &ua_public,
                AUTH_SECRET_B64,
                None,
                None,
            )
            .await;

        let payload = json!({ "title": "Ready", "data": { "type": "ready" } });
        runtime.send_push_to_all_ui_sessions(&payload, false).await;

        let recorded = requests.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(recorded.len(), 1);
        let (url, headers, body) = &recorded[0];
        assert_eq!(url, "https://push/e1");
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(header("TTL").as_deref(), Some("2419200"));
        assert_eq!(header("Content-Encoding").as_deref(), Some("aes128gcm"));
        assert!(
            header("Authorization")
                .expect("vapid auth")
                .starts_with("vapid t=")
        );
        // aes128gcm frame: 20-byte fixed header, 65-byte keyid.
        assert_eq!(body[20], 65);
        assert_eq!(body[21], 0x04);
        assert!(body.len() > 86);

        // A 410 response drops the endpoint everywhere.
        drop(recorded);
        *status.lock().unwrap_or_else(|e| e.into_inner()) = 410;
        runtime.send_push_to_all_ui_sessions(&payload, false).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sessions = runtime.read_subscriptions_from_disk().await;
        assert!(sessions.is_empty(), "410 removes the subscription");
    }

    #[tokio::test]
    async fn presence_gate_suppresses_only_matching_platforms() {
        let dir = temp_dir();
        let (transport, requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);
        let ua_secret = generate_secret_key();
        let ua_public = public_to_b64url(&ua_secret.public_key());
        runtime
            .add_or_update_push_subscription(
                "phone",
                "https://push/m",
                &ua_public,
                AUTH_SECRET_B64,
                None,
                Some("ios"),
            )
            .await;
        runtime
            .add_or_update_push_subscription(
                "desk",
                "https://push/d",
                &ua_public,
                AUTH_SECRET_B64,
                None,
                Some("mac"),
            )
            .await;
        // A visible interactive client suppresses both channels.
        runtime.update_ui_visibility("desk", true, Some("mac"));
        runtime
            .send_push_to_all_ui_sessions(&json!({ "title": "t" }), true)
            .await;
        assert!(
            requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );

        // Hidden desktop + foreground phone: only the mobile sub sends.
        runtime.update_ui_visibility("desk", false, Some("mac"));
        runtime.update_ui_visibility("phone", true, Some("ios"));
        runtime
            .send_push_to_all_ui_sessions(&json!({ "title": "t" }), true)
            .await;
        let recorded = requests.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "https://push/m");
    }

    #[tokio::test]
    async fn web_push_send_uses_real_subscription_keys() {
        // A receiver built from the subscription keys can parse the frame
        // (keyid is a valid P-256 point) — full decryption is proven by the
        // RFC 8291 KAT in the crypto module.
        let ua_secret = generate_secret_key();
        let ua_public = public_to_b64url(&ua_secret.public_key());

        let dir = temp_dir();
        let (transport, requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);
        runtime
            .add_or_update_push_subscription(
                "ui",
                "https://push/x",
                &ua_public,
                AUTH_SECRET_B64,
                None,
                None,
            )
            .await;
        runtime
            .send_push_to_all_ui_sessions(&json!({ "title": "Hi" }), false)
            .await;
        let recorded = requests.lock().unwrap_or_else(|e| e.into_inner());
        let body = &recorded[0].2;
        assert!(body.len() > 86);
        // The ephemeral application-server key in the keyid parses as a
        // P-256 public key.
        assert!(p256::PublicKey::from_sec1_bytes(&body[21..86]).is_ok());
    }

    #[tokio::test]
    async fn ensure_push_initialized_populates_vapid_details() {
        let dir = temp_dir();
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);
        assert!(runtime.vapid_details_for_test().await.is_none());
        runtime.ensure_push_initialized().await;
        let details = runtime.vapid_details_for_test().await.expect("details");
        assert_eq!(details.subject, "mailto:ompchamber@localhost");
        assert!(!details.public_key.is_empty());
        // Re-initialization after a reset re-reads the persisted keys.
        runtime.set_push_initialized(false);
        runtime.ensure_push_initialized().await;
        let again = runtime.vapid_details_for_test().await.expect("details");
        assert_eq!(again.public_key, details.public_key);
    }
}
