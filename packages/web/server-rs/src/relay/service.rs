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
//!
//! 中文说明：本模块为私有 relay 服务，移植自 `server/lib/relay/service.js`：
//! 负责配置持久化（存于 settings.json 的 `settings.privateRelay`）、relay
//! 主机客户端生命周期管理，以及 `/api/ompchamber/relay/*` 管理路由；路由
//! 与其它 OMPChamber 功能路由一同注册，受同一全局 UI 认证门禁保护。
//! 跨运行时说明：v1 中 relay 主机模式仅面向 web server 运行时，VS Code
//! 运行时不承载 relay。

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

/// 未配置或配置无效时使用的官方 relay 端点。
pub const DEFAULT_RELAY_URL: &str = "wss://relay.ompchamber.dev/ws";
/// relay 配置在 settings.json 中的存储键。
const SETTINGS_KEY_PRIVATE_RELAY: &str = "privateRelay";

/// 校验是否为合法 relay 地址：必须是 ws:// 或 wss:// 的 URL。
pub fn is_valid_relay_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value.trim()) else {
        return false;
    };
    matches!(url.scheme(), "ws" | "wss")
}

/// 归一化 relay 地址：空、缺失或非法时回落 `DEFAULT_RELAY_URL`，否则去
/// 空白后原样返回。
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
/// 中文：部署可通过环境变量 `OMPCHAMBER_RELAY_URL` 固定 relay 端点
/// （如自建 relay）；设置且合法时完全覆盖已存储设置——主机连接、配对
/// offer 与状态都指向它，客户端随后从 offer 自动继承。
fn env_relay_url_override() -> Option<String> {
    let raw = std::env::var("OMPCHAMBER_RELAY_URL").ok()?;
    if raw.trim().is_empty() || !is_valid_relay_url(&raw) {
        return None;
    }
    Some(raw.trim().to_string())
}

/// 从设置文件（含环境变量覆盖）解析出的 relay 配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayConfig {
    /// 是否启用 relay 主机。
    pub enabled: bool,
    /// 生效的 relay 端点地址。
    pub relay_url: String,
    /// True when the endpoint is pinned by `OMPCHAMBER_RELAY_URL` (a
    /// self-hosted relay); the stored setting is ignored while it is set.
    /// 中文：端点被 `OMPCHAMBER_RELAY_URL` 固定时为 true，期间忽略存储
    /// 设置。
    pub relay_url_locked: bool,
}

/// 服务层状态快照（与主机客户端状态同构）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ServiceStatus {
    /// 状态：disabled / standby / connecting / connected / reconnecting。
    state: String,
    /// 最近错误（standby 时为持锁进程提示）。
    last_error: Option<String>,
    /// 已连接客户端数。
    connected_clients: usize,
}

/// `ServiceStatus` 的两个特殊构造。
impl ServiceStatus {
    /// 构造 disabled 状态：无错误、无连接。
    fn disabled() -> Self {
        Self {
            state: "disabled".into(),
            last_error: None,
            connected_clients: 0,
        }
    }

    /// 构造 standby 状态：另一本地进程持有主机锁，last_error 携带其 pid。
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

/// 主机锁获取策略。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// 常规尝试获取锁；失败则进入 standby。
    Try,
    /// 强制夺取锁（显式用户意图，如创建配对链接）。
    Force,
}

/// relay 服务：配置读写、主机客户端生命周期与状态聚合。
pub struct RelayService {
    /// settings.json 存储句柄。
    store: Arc<SettingsStore>,
    /// relay 身份运行时（签名密钥与 server id）。
    identity: Arc<RelayIdentityRuntime>,
    /// 查询本地 loopback 端口的回调。
    get_local_port: super::host_client::GetLocalPortFn,
    /// 跨进程主机锁；无锁环境（如测试）为 None。
    host_lock: Option<Arc<RelayHostLock>>,
    /// 是否允许被动承载 relay（非显式启用的实例不得抢跑）。
    allow_passive_host: bool,
    /// Relay demand = any paired device or pending pairing session that uses
    /// the relay transport. The relay lifecycle is driven purely by this
    /// demand (client-auth stores for this data dir).
    /// 中文：relay 需求来源——本数据目录下使用 relay 传输的已配对设备或
    /// 待完成配对会话；relay 生命周期完全由该需求驱动。
    client_auth: Arc<crate::client_auth::ClientAuthState>,
    /// 当前运行中的主机客户端；None 表示未运行。
    state: std::sync::Mutex<Option<Arc<RelayHostClient>>>,
    /// 主机客户端未运行时的兜底状态。
    status: std::sync::Mutex<ServiceStatus>,
}

/// `RelayService` 的配置读写、生命周期与对外状态和配对 API。
impl RelayService {
    /// 创建服务实例：以设置存储构造身份运行时，绑定 client-auth 状态、
    /// 端口回调、主机锁与被动托管开关。
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

    /// 读取并解析 relay 配置：settings.privateRelay 中的 enabled 与
    /// relayUrl，叠加环境变量覆盖。
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

    /// 写回 relay 配置（relayUrl 先归一化）。
    async fn write_config(&self, enabled: bool, relay_url: &str) -> AppResult<()> {
        let mut settings = self.store.read_migrated().await?;
        settings.insert(
            SETTINGS_KEY_PRIVATE_RELAY.to_string(),
            json!({ "enabled": enabled, "relayUrl": normalize_relay_url(Some(relay_url)) }),
        );
        self.store.write_raw(&settings).await
    }

    /// 停止并清空运行中的主机客户端（不动主机锁与状态）。
    fn stop_host_client(&self) {
        if let Some(host) = self.state.lock().unwrap_or_else(|e| e.into_inner()).take() {
            host.stop();
        }
    }

    /// 启动主机客户端：已在运行直接返回；非 Force 且禁止被动托管时进入
    /// standby；按策略获取主机锁，失败进入 standby（记录持锁 pid）；
    /// 成功则启动 `RelayHostClient` 并同步其初始状态。
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

    /// 停止主机客户端、释放主机锁并把状态置为 disabled。
    pub fn stop(&self) {
        self.stop_host_client();
        if let Some(host_lock) = &self.host_lock {
            host_lock.release();
        }
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = ServiceStatus::disabled();
    }

    /// 启动时调用：配置启用则以 Try 策略启动；读取失败仅告警不阻断启动。
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
    /// 中文：按需求驱动 relay 生命周期：有设备或待配对会话使用 relay 时
    /// 运行，需求清零时停止；在启动与配对/设备变化后调用。读取失败绝不
    /// 冒充无需求——否则会误写 enabled=false 并切断已配对设备。
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

    /// 判断是否存在 relay 需求：待完成的 relay 配对会话或活跃 relay 设备
    /// 任一为真即有需求；两者皆失败时向上传递错误以中止 reconcile。
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
    /// 中文：稳定的服务端标识（规范签名公钥 JWK 的 base64url SHA-256）；
    /// 由公钥派生而非秘密，且与 relay 是否启用无关。
    pub async fn get_server_id(&self) -> AppResult<String> {
        Ok(self.identity.get_relay_identity().await?.server_id.clone())
    }

    /// 聚合当前状态：主机客户端运行中取其活跃状态，否则取兜底状态。
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

    /// 组装状态 JSON：enabled、state（运行中取实时，否则按
    /// standby/disabled 归并）、serverId、connectedClients、relayUrl、
    /// relayUrlLocked；lastError 仅在存在时携带。
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

    /// 构造 relay 配对候选 JSON（type/relayUrl/serverId/hostEncPubJwk/
    /// priority）。
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
    /// 中文：统一连接载荷（配对 v2）中的 relay 配对候选；主机 relay 关闭
    /// 时返回 None，仅在实际可达时对外通告。优先级高（排在 LAN/tunnel
    /// 之后），relay 是最后手段传输。
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
    /// 中文：按需启用 relay 主机并返回配对候选——创建 relay 配对链接本身
    /// 就是需求信号；幂等：已启用且运行中时为 no-op。
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
    /// 中文：POST /api/ompchamber/relay/enable——显式用户动作：接受可选
    /// relayUrl，写配置后以 Force 策略（与配对相同）夺取主机锁并启动，
    /// 返回最新状态。
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
    /// 中文：POST /api/ompchamber/relay/disable——写 enabled=false、停止
    /// 主机并返回最新状态。
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
/// 中文：主机锁监视循环（对应 JS `ensureClaimWatch`）：启用期间，standby
/// 实例在持锁者死亡后接管，运行中的主机在他人夺锁时让位；2 分钟宽限
/// 防止 standby 实例抢走前一台主机干净重启释放的锁。
async fn run_claim_watch(service: Arc<RelayService>, host_lock: Arc<RelayHostLock>) {
    // 锁监视巡检间隔。
    const CLAIM_WATCH_INTERVAL_MS: u64 = 30_000;
    // 接管宽限：防止抢走前一台主机干净重启释放的锁。
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
/// 中文：按数据目录的进程级 relay 服务注册表（JS 为每个 server 接一个
/// relayService；测试复用注册表，使状态反映同一主机）。
pub fn service_for_data_dir(
    data_dir: &std::path::Path,
    settings_path: &std::path::Path,
    get_local_port: super::host_client::GetLocalPortFn,
    spawn_background: bool,
) -> Arc<RelayService> {
    // 数据目录 → 服务实例的进程级注册表；LazyLock 保证首次调用时初始化。
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

/// 从路由上下文获取（或创建）本数据目录的 relay 服务，端口取自配置，
/// 并派生后台 reconcile 与锁监视任务。
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
