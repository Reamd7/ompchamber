//! Port of `server/lib/relay/service.js` — private relay service: config
//! persistence, lifecycle of the relay host client, and the
//! `/api/ompchamber/relay/*` management routes.
//!
//! Config lives in the server settings file as `settings.privateRelay =
//! { enabled, relayUrl }` (same storage precedent as tunnels/notifications).
//! Routes are registered with the other OMPChamber feature routes and are
//! covered by the same global UI auth gate.
//!
//! Cross-runtime parity note: relay host mode intentionally targets the web
//! server runtime only in v1. The VS Code runtime does not host a relay.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client_auth::state_for_data_dir;
use crate::context::RouterContext;
use crate::error::AppResult;
use crate::settings::SettingsStore;

use super::host_client::{RelayHostClient, RelayHostOptions, RelayHostStatus};
use super::host_lock::RelayHostLock;
use super::identity::{RelayIdentityRuntime, system_clock};

pub const DEFAULT_RELAY_URL: &str = "wss://relay.ompchamber.dev/ws";
const SETTINGS_KEY_PRIVATE_RELAY: &str = "privateRelay";

pub fn is_valid_relay_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value.trim()) else {
        return false;
    };
    matches!(url.scheme(), "ws" | "wss")
}

pub fn normalize_relay_url(value: Option<&str>) -> String {
    let Some(value) = value else {
        return DEFAULT_RELAY_URL.to_string();
    };
    let trimmed = value.trim();
    if trimmed.is_empty() || !is_valid_relay_url(trimmed) {
        return DEFAULT_RELAY_URL.to_string();
    }
    trimmed.to_string()
}

/// A deployment can pin the relay endpoint via env (e.g. a self-hosted relay).
/// When set and valid it overrides the stored setting entirely, so the host
/// connection, the pairing offer, and the status all point at it — clients
/// then inherit it from the offer automatically.
fn env_relay_url_override() -> Option<String> {
    let raw = std::env::var("OMPCHAMBER_RELAY_URL").ok()?;
    if raw.trim().is_empty() || !is_valid_relay_url(&raw) {
        return None;
    }
    Some(raw.trim().to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayConfig {
    pub enabled: bool,
    pub relay_url: String,
    /// True when the endpoint is pinned by `OMPCHAMBER_RELAY_URL` (a
    /// self-hosted relay); the stored setting is ignored while it is set.
    pub relay_url_locked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServiceStatus {
    state: String,
    last_error: Option<String>,
    connected_clients: usize,
}

impl ServiceStatus {
    fn disabled() -> Self {
        Self {
            state: "disabled".into(),
            last_error: None,
            connected_clients: 0,
        }
    }

    fn standby(holder_pid: u32) -> Self {
        Self {
            state: "standby".into(),
            last_error: Some(format!(
                "relay host is owned by another local OMPChamber process (pid {holder_pid})"
            )),
            connected_clients: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    Try,
    Force,
}

pub struct RelayService {
    store: Arc<SettingsStore>,
    identity: Arc<RelayIdentityRuntime>,
    get_local_port: super::host_client::GetLocalPortFn,
    host_lock: Option<Arc<RelayHostLock>>,
    allow_passive_host: bool,
    /// Relay demand = any paired device or pending pairing session that uses
    /// the relay transport. The relay lifecycle is driven purely by this
    /// demand (client-auth stores for this data dir).
    client_auth: Arc<crate::client_auth::ClientAuthState>,
    state: std::sync::Mutex<Option<Arc<RelayHostClient>>>,
    status: std::sync::Mutex<ServiceStatus>,
}

impl RelayService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<SettingsStore>,
        data_dir: std::path::PathBuf,
        get_local_port: super::host_client::GetLocalPortFn,
        host_lock: Option<Arc<RelayHostLock>>,
        allow_passive_host: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: Arc::new(RelayIdentityRuntime::new(store.clone(), system_clock())),
            client_auth: state_for_data_dir(&data_dir),
            store,
            get_local_port,
            host_lock,
            allow_passive_host,
            state: std::sync::Mutex::new(None),
            status: std::sync::Mutex::new(ServiceStatus::disabled()),
        })
    }

    async fn read_config(&self) -> AppResult<RelayConfig> {
        let settings = self.store.read_migrated().await?;
        let stored = settings.get(SETTINGS_KEY_PRIVATE_RELAY);
        let enabled = stored
            .and_then(|value| value.get("enabled"))
            .map(|value| value == &Value::Bool(true))
            .unwrap_or(false);
        let stored_url = stored
            .and_then(|value| value.get("relayUrl"))
            .and_then(Value::as_str);
        let override_url = env_relay_url_override();
        Ok(RelayConfig {
            enabled,
            relay_url: override_url
                .clone()
                .unwrap_or_else(|| normalize_relay_url(stored_url)),
            relay_url_locked: override_url.is_some(),
        })
    }

    async fn write_config(&self, enabled: bool, relay_url: &str) -> AppResult<()> {
        let mut settings = self.store.read_migrated().await?;
        settings.insert(
            SETTINGS_KEY_PRIVATE_RELAY.to_string(),
            json!({ "enabled": enabled, "relayUrl": normalize_relay_url(Some(relay_url)) }),
        );
        self.store.write_raw(&settings).await
    }

    fn stop_host_client(&self) {
        if let Some(host) = self.state.lock().unwrap_or_else(|e| e.into_inner()).take() {
            host.stop();
        }
    }

    async fn start(&self, relay_url: &str, claim: Claim) -> AppResult<()> {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Ok(());
        }
        if claim != Claim::Force && !self.allow_passive_host {
            *self.status.lock().unwrap_or_else(|e| e.into_inner()) = ServiceStatus {
                state: "standby".into(),
                last_error: Some(
                    "passive relay hosting is disabled on this instance — enable the relay or create a pairing link to host here"
                        .to_string(),
                ),
                connected_clients: 0,
            };
            return Ok(());
        }
        if let Some(host_lock) = &self.host_lock {
            let claimed = match claim {
                Claim::Force => host_lock.force_claim(),
                Claim::Try => host_lock.try_claim(),
            };
            if !claimed {
                *self.status.lock().unwrap_or_else(|e| e.into_inner()) =
                    ServiceStatus::standby(host_lock.live_claimant_pid().unwrap_or_default());
                return Ok(());
            }
        }
        let identity = self.identity.get_relay_identity().await?;
        let host = RelayHostClient::start(RelayHostOptions {
            relay_url: relay_url.to_string(),
            identity,
            get_local_port: self.get_local_port.clone(),
            on_status: None,
            batch_window_ms: None,
            batch: true,
        });
        let live = host.get_status();
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = ServiceStatus {
            state: live.state,
            last_error: live.last_error,
            connected_clients: live.connected_clients,
        };
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_host_client();
        if let Some(host_lock) = &self.host_lock {
            host_lock.release();
        }
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = ServiceStatus::disabled();
    }

    pub async fn start_if_enabled(&self) -> AppResult<()> {
        match self.read_config().await {
            Ok(config) if config.enabled => {
                self.start(&config.relay_url, Claim::Try).await.map(|_| ())
            }
            Ok(_) => Ok(()),
            Err(error) => {
                tracing::warn!("[Relay] startup failed: {error}");
                Ok(())
            }
        }
    }

    /// Drive the relay lifecycle from demand: run it when a device or pending
    /// session uses the relay, stop it when none remain. Called on startup and
    /// after pairing/device changes, so the operator never toggles it manually.
    pub async fn reconcile(&self) -> AppResult<()> {
        let outcome = async {
            // A store read failure must NOT masquerade as "no demand":
            // reconcile persists enabled=false and severs paired devices.
            // Any affirmative answer wins; otherwise a failed check aborts
            // reconcile so the relay keeps its current state.
            let demand = self.has_relay_demand().await?;
            let config = self.read_config().await?;
            if demand {
                if !config.enabled {
                    self.write_config(true, &config.relay_url).await?;
                }
                if self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    let next = self.read_config().await?;
                    self.start(&next.relay_url, Claim::Try).await?;
                }
            } else {
                if config.enabled {
                    self.write_config(false, &config.relay_url).await?;
                }
                self.stop();
            }
            Ok(())
        }
        .await;
        if let Err(error) = &outcome {
            tracing::warn!("[Relay] reconcile failed: {error}");
        }
        outcome
    }

    async fn has_relay_demand(&self) -> AppResult<bool> {
        // JS Promise.allSettled: any affirmative answer wins; otherwise a
        // failed check aborts reconcile so the relay keeps its current state.
        let pending = self.client_auth.pairing.has_active_relay_session().await;
        let device = self
            .client_auth
            .remote_clients
            .has_active_relay_clients()
            .await;
        if matches!(pending, Ok(true)) || matches!(device, Ok(true)) {
            return Ok(true);
        }
        if let Err(error) = pending {
            return Err(error);
        }
        if let Err(error) = device {
            return Err(error);
        }
        Ok(false)
    }

    /// Stable server identity (base64url SHA-256 of the canonical public
    /// signing JWK). Derived from a public key, so it is not a secret.
    /// Independent of whether the relay host is currently enabled.
    pub async fn get_server_id(&self) -> AppResult<String> {
        Ok(self.identity.get_relay_identity().await?.server_id.clone())
    }

    fn live_host_status(&self) -> ServiceStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.as_ref() {
            Some(host) => {
                let live: RelayHostStatus = host.get_status();
                ServiceStatus {
                    state: live.state,
                    last_error: live.last_error,
                    connected_clients: live.connected_clients,
                }
            }
            None => self
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    pub async fn get_status(&self) -> AppResult<Value> {
        let config = self.read_config().await?;
        let identity = self.identity.get_relay_identity().await?;
        let live = self.live_host_status();
        let has_host_client = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let state = if has_host_client {
            live.state.clone()
        } else if live.state == "standby" {
            "standby".to_string()
        } else {
            "disabled".to_string()
        };
        let mut payload = json!({
            "enabled": config.enabled,
            // Without a host client the service is either off or standing by
            // while another local process owns the machine's relay host claim.
            "state": state,
            "serverId": identity.server_id,
            "connectedClients": live.connected_clients,
            "relayUrl": config.relay_url,
            "relayUrlLocked": config.relay_url_locked,
        });
        if let Some(last_error) = live.last_error {
            if let Some(object) = payload.as_object_mut() {
                object.insert("lastError".to_string(), Value::String(last_error));
            }
        }
        Ok(payload)
    }

    async fn build_pairing_candidate(&self) -> AppResult<Value> {
        let config = self.read_config().await?;
        let identity = self.identity.get_relay_identity().await?;
        Ok(json!({
            "type": "relay",
            "relayUrl": config.relay_url,
            "serverId": identity.server_id,
            "hostEncPubJwk": identity.host_enc_pub_jwk,
            "priority": 30,
        }))
    }

    /// Pairing candidate for the unified connection payload (pairing v2).
    /// Returns `None` when the host relay is off, so callers only advertise
    /// relay when it is actually reachable. Priority is high (tried after
    /// LAN/tunnel) since the relay path is the last-resort transport.
    pub async fn get_pairing_candidate(&self) -> AppResult<Option<Value>> {
        let config = self.read_config().await?;
        if !config.enabled {
            return Ok(None);
        }
        Ok(Some(self.build_pairing_candidate().await?))
    }

    /// Enable the relay host on demand and return its pairing candidate.
    /// Creating a relay pairing link IS the demand signal. Idempotent: a no-op
    /// when the relay is already enabled and running.
    pub async fn ensure_enabled_for_pairing(&self) -> AppResult<Value> {
        let config = self.read_config().await?;
        if !config.enabled {
            self.write_config(true, &config.relay_url).await?;
        }
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            let next = self.read_config().await?;
            // Force-claim: creating a pairing link is explicit user intent —
            // the instance the user is pairing against MUST be the one devices
            // reach, even if another local process currently holds the
            // machine's claim (its claim watcher sees the takeover and stands
            // down).
            self.start(&next.relay_url, Claim::Force).await?;
        }
        self.build_pairing_candidate().await
    }

    /// POST /api/ompchamber/relay/enable — explicit user action: take the
    /// machine's host claim like pairing does.
    pub async fn enable(&self, body: &Value) -> AppResult<Value> {
        let current = self.read_config().await?;
        let relay_url = match body.get("relayUrl").and_then(Value::as_str) {
            Some(submitted) => normalize_relay_url(Some(submitted)),
            None => current.relay_url.clone(),
        };
        self.write_config(true, &relay_url).await?;
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            self.stop();
        }
        self.start(&relay_url, Claim::Force).await?;
        self.get_status().await
    }

    /// POST /api/ompchamber/relay/disable
    pub async fn disable(&self) -> AppResult<Value> {
        let current = self.read_config().await?;
        self.write_config(false, &current.relay_url).await?;
        self.stop();
        self.get_status().await
    }
}

/// Claim-watch loop (JS `ensureClaimWatch`): while enabled, a standby instance
/// takes over when the claimant dies and a running host stands down when
/// another process claims. The 2-minute grace keeps a standby instance from
/// grabbing the claim freed by a clean restart of the previous host.
async fn run_claim_watch(service: Arc<RelayService>, host_lock: Arc<RelayHostLock>) {
    const CLAIM_WATCH_INTERVAL_MS: u64 = 30_000;
    const CLAIM_TAKEOVER_GRACE_MS: u64 = 120_000;
    let mut tick = tokio::time::interval(Duration::from_millis(CLAIM_WATCH_INTERVAL_MS));
    tick.tick().await;
    let mut claim_free_since: Option<std::time::Instant> = None;
    loop {
        tick.tick().await;
        let relay_url = match service.read_config().await {
            Ok(config) => config.relay_url,
            Err(error) => {
                tracing::warn!("[Relay] claim watch failed: {error}");
                continue;
            }
        };
        let running = service
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if running {
            if !host_lock.holds_claim() && host_lock.live_claimant_pid().is_some() {
                tracing::warn!(
                    "[Relay] host claim taken by another local instance — standing down"
                );
                let holder = host_lock.live_claimant_pid();
                service.stop_host_client();
                *service.status.lock().unwrap_or_else(|e| e.into_inner()) =
                    ServiceStatus::standby(holder.unwrap_or_default());
            }
            claim_free_since = None;
            continue;
        }
        let standby = service
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            == "standby";
        if !standby || !service.allow_passive_host {
            claim_free_since = None;
            continue;
        }
        if host_lock.live_claimant_pid().is_some() {
            claim_free_since = None;
            continue;
        }
        let now = std::time::Instant::now();
        claim_free_since.get_or_insert(now);
        if now.duration_since(claim_free_since.unwrap_or(now))
            < Duration::from_millis(CLAIM_TAKEOVER_GRACE_MS)
        {
            continue;
        }
        if host_lock.try_claim() {
            claim_free_since = None;
            tracing::warn!("[Relay] host claim stayed free — taking over the relay host");
            if let Err(error) = service.start(&relay_url, Claim::Try).await {
                tracing::warn!("[Relay] claim watch failed: {error}");
            }
        }
    }
}

/// Process-wide relay service per data directory (JS wires one relayService
/// per server; tests reuse the registry so status reflects the same host).
pub fn service_for_data_dir(
    data_dir: &std::path::Path,
    settings_path: &std::path::Path,
    get_local_port: super::host_client::GetLocalPortFn,
    spawn_background: bool,
) -> Arc<RelayService> {
    static REGISTRY: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<RelayService>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(data_dir) {
        return existing.clone();
    }
    let store = crate::settings::store_for_path(settings_path);
    // Dev/debug instances share the data dir (and thus the relay identity)
    // with the production instance, so they must not host the relay on their
    // own — paired devices would land on them. OMPCHAMBER_RELAY_HOST=off
    // disables passive hosting explicitly; =on overrides both.
    let allow_passive_host = std::env::var("OMPCHAMBER_RELAY_HOST").as_deref() == Ok("on")
        || (std::env::var("OMPCHAMBER_RELAY_HOST").as_deref() != Ok("off")
            && std::env::var("OMPCHAMBER_ELECTRON_DEV").as_deref() != Ok("1"));
    let host_lock = Arc::new(RelayHostLock::for_data_dir(data_dir));
    let service = RelayService::new(
        store,
        data_dir.to_path_buf(),
        get_local_port,
        Some(host_lock.clone()),
        allow_passive_host,
    );
    registry.insert(data_dir.to_path_buf(), service.clone());
    if spawn_background {
        // JS index.js: reconcile on startup and every 60s (demand can change
        // outside our routes — pending pairing sessions expire without any
        // request hitting us).
        let reconcile_service = service.clone();
        let watch_service = service.clone();
        tokio::spawn(async move {
            let _ = reconcile_service.reconcile().await;
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _ = reconcile_service.reconcile().await;
            }
        });
        tokio::spawn(run_claim_watch(watch_service, host_lock));
    }
    service
}

pub fn service(ctx: &RouterContext) -> Arc<RelayService> {
    service_for_data_dir(
        &ctx.config.data_dir,
        &ctx.config.data_dir.join("settings.json"),
        {
            let port = ctx.config.port;
            Arc::new(move || port)
        },
        true,
    )
}
