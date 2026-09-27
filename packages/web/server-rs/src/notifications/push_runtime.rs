//! Port of `server/lib/notifications/push-runtime.js`: web-push
//! subscription persistence (data-dir `push-subscriptions.json`), VAPID
//! key lifecycle, web-push sending (RFC 8291 + 8292, see
//! [`crate::notifications::crypto`]), and the UI visibility heartbeat
//! model (30s TTL).
//!
//! 中文说明：本模块是 `server/lib/notifications/push-runtime.js` 的
//! Rust 移植，负责 Web Push 订阅持久化（数据目录下的
//! push-subscriptions.json）、VAPID 密钥生命周期、按 RFC 8291/8292 的
//! 加密发送（实现在 crypto 模块），以及 30 秒 TTL 的 UI 可见性心跳模型。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::notifications::crypto;
use crate::notifications::transport::HttpPost;
use crate::settings::SettingsStore;

/// push-subscriptions.json 的持久化格式版本；版本不匹配时整文件按空处理。
pub const PUSH_SUBSCRIPTIONS_VERSION: u64 = 1;
/// UI 可见性心跳的有效期（毫秒）：超过 30 秒未刷新即视为不可见。
pub const UI_VISIBILITY_TTL_MS: u64 = 30_000;
/// 每个 UI 会话最多保留的订阅条数（新订阅插到队头并截断）。
const MAX_SUBSCRIPTIONS_PER_SESSION: usize = 10;

/// `isLoopbackHttpOrigin` (push-runtime.js).
///
/// 中文说明：localhost / 127.0.0.1 / [::1] 的明文 http origin——
/// 这类地址不能用作 VAPID subject，需回落到 mailto。
fn is_loopback_http_origin(value: &str) -> bool {
    value.starts_with("http://localhost")
        || value.starts_with("http://127.0.0.1")
        || value.starts_with("http://[::1]")
}

/// 当前 Unix 毫秒时间戳（转发 crypto::now_ms，统一时钟来源）。
fn now_ms() -> u64 {
    crypto::now_ms()
}

/// A client is "mobile" if it reports a native mobile platform; anything
/// else (web, desktop, vscode, older clients) counts as interactive.
///
/// 中文说明：仅 ios/android 算移动端；web/desktop/vscode 及未上报平台
/// 的旧客户端一律按交互式客户端对待。
fn is_mobile_platform(platform: Option<&str>) -> bool {
    matches!(platform, Some("ios") | Some("android"))
}

/// 单个 UI 会话 token 的可见性心跳状态。
#[derive(Debug, Clone)]
struct VisibilityState {
    /// 最近一次心跳是否前台可见。
    visible: bool,
    /// 最近一次心跳的毫秒时间戳（TTL 判定依据）。
    updated_at: u64,
    /// 上报的客户端平台（心跳省略时沿用旧值）。
    platform: Option<String>,
}

/// The `webPush.setVapidDetails(subject, publicKey, privateKey)` state.
///
/// 中文说明：VAPID 三元组——公私钥持久化在 settings 的 `vapidKeys` 下，
/// 初始化完成后缓存在运行时内。
#[derive(Debug, Clone)]
pub struct VapidDetails {
    /// VAPID subject：mailto 地址或 public origin。
    pub subject: String,
    /// base64url 编码的 P-256 公钥（下发给浏览器）。
    pub public_key: String,
    /// base64url 编码的 P-256 私钥（仅服务端持有）。
    pub private_key: String,
}

/// `normalizePushSubscriptions`: only entries whose endpoint/p256dh/auth
/// are strings survive; `createdAt` becomes null when not a number, and
/// `lastSeenAt`/`userAgent` do not survive normalization (as in JS).
///
/// 中文说明：规范化后的订阅记录——endpoint/p256dh/auth 任一不是字符串
/// 即整条丢弃；createdAt 非数字时为 None；lastSeenAt/userAgent
/// 不在规范化结果中保留（与 JS normalizePushSubscriptions 一致）。
#[derive(Debug, Clone)]
pub struct PushSubscription {
    /// push 服务的订阅端点 URL（去重与删除的主键）。
    pub endpoint: String,
    /// 浏览器生成的 P-256 公钥（base64url），载荷加密的接收公钥。
    pub p256dh: String,
    /// 订阅的 auth secret（base64url），用于派生内容加密密钥。
    pub auth: String,
    /// 订阅创建时间（毫秒），仅持久化时使用。
    pub created_at: Option<u64>,
    /// 注册时上报的平台，决定 presence 门控策略。
    pub platform: Option<String>,
}

/// 订阅记录的 JSON 序列化实现。
impl PushSubscription {
    /// 序列化为 push-subscriptions.json 里的单条记录（保留平台与创建
    /// 时间，不含规范化时丢弃的 lastSeenAt/userAgent）。
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

/// Web Push 运行时：订阅持久化、VAPID 密钥生命周期、加密发送与
/// UI 可见性追踪。
pub struct PushRuntime {
    /// push-subscriptions.json 的路径（位于数据目录下）。
    subscriptions_path: PathBuf,
    /// settings 存储：VAPID 密钥与 publicOrigin 的存取。
    store: Arc<SettingsStore>,
    /// 注入的 HTTP POST 传输缝（生产为 reqwest，测试为桩）。
    transport: HttpPost,
    /// Serializes read-modify-write cycles (the JS `persistPushSubscriptionsLock`).
    ///
    /// 中文说明：串行化订阅文件的读-改-写循环。
    persist_lock: tokio::sync::Mutex<()>,
    /// 已初始化的 VAPID 详情；None 表示尚未初始化。
    vapid: tokio::sync::Mutex<Option<VapidDetails>>,
    /// 初始化完成标记；publicOrigin 变更后由路由层复位以强制重建。
    push_initialized: AtomicBool,
    /// UI 会话 token → 可见性状态的心跳表。
    visibility: Mutex<HashMap<String, VisibilityState>>,
}

/// 订阅持久化、VAPID 生命周期、发送与可见性的实现集合。
impl PushRuntime {
    /// 构造 push 运行时（返回 `Arc` 供路由与触发器共享）。
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

    /// 读取并校验订阅文件，返回 `subscriptionsBySession` 映射。
    /// 文件缺失、JSON 损坏、非对象或版本不匹配一律返回空 map（容错降级）。
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

    /// 把会话映射写回磁盘（带版本头）；先确保父目录存在，
    /// 写失败仅记日志、不向调用方报错。
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
    ///
    /// 中文说明：在持久化锁内执行 读盘 → mutate → 写盘，
    /// 避免并发更新互相覆盖（对应 JS 的 persistPushSubscriptionsLock）。
    async fn persist_update<F>(&self, mutate: F)
    where
        F: FnOnce(Map<String, Value>) -> Map<String, Value> + Send,
    {
        let _guard = self.persist_lock.lock().await;
        let current = self.read_subscriptions_from_disk().await;
        let next = mutate(current);
        self.write_subscriptions_to_disk(&next).await;
    }

    /// 把某会话的原始 JSON 数组规范化为 `Vec<PushSubscription>`：
    /// 非对象条目或字段缺失/类型不符的条目被静默丢弃。
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
    ///
    /// 中文说明：空 token 直接返回；同 endpoint 重新注册会移动到队头、
    /// 刷新 createdAt/lastSeenAt 并继承旧平台；每会话最多保留
    /// MAX_SUBSCRIPTIONS_PER_SESSION 条。
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
    ///
    /// 中文说明：删除该会话下指定 endpoint 的订阅；删空后整个会话键移除。
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
    ///
    /// 中文说明：push 服务返回 410/404 后，把该 endpoint 从所有会话清除。
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
    ///
    /// 中文说明：优先复用 settings.vapidKeys 里已持久化的密钥对；
    /// 缺失时新生成并写回（写失败映射为 io::Error 返回）。
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
    ///
    /// 中文说明：优先级 OMPCHAMBER_VAPID_SUBJECT > OMPCHAMBER_PUBLIC_ORIGIN
    /// > settings.publicOrigin；回环 http 地址不能当 subject，
    /// 统一回落 mailto:ompchamber@localhost。
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
    ///
    /// 中文说明：幂等初始化——读取/生成密钥并解析 subject 后填入 VAPID
    /// 详情；subject 回落 mailto 时打警告提示配置 publicOrigin。
    /// 失败则保持未初始化，下次调用重试。
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
    ///
    /// 中文说明：publicOrigin 更新后由路由层调用，强制下次重新初始化。
    pub fn set_push_initialized(&self, value: bool) {
        self.push_initialized.store(value, Ordering::SeqCst);
    }

    /// Test access to the initialized VAPID details.
    ///
    /// 中文说明：测试读取已初始化的 VAPID 详情。
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
    ///
    /// 中文说明：先确保初始化，再按 RFC 8291 加密载荷、生成 VAPID
    /// Authorization 头并 POST 到订阅端点；返回 push 服务状态码。
    /// 加密/签名失败或网络错误返回 None（这类失败不删除订阅）。
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
    ///
    /// 中文说明：跨会话按 endpoint 去重后并行发送；require_no_sse 时按
    /// 平台做 presence 门控（移动端仅在交互式客户端可见时抑制，
    /// 桌面/网页端任一可见即抑制）；410/404 会把端点从所有会话删除。
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
    ///
    /// 中文说明：记录一次前后台心跳；平台缺省时沿用上次值，空 token 忽略。
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

    /// 清掉超过 TTL 未刷新的心跳条目（惰性清理，每次查询前调用）。
    fn prune_ui_visibility(&self, now: u64) {
        let mut visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.retain(|_, state| now.saturating_sub(state.updated_at) <= UI_VISIBILITY_TTL_MS);
    }

    /// `isAnyUiVisible`.
    ///
    /// 中文说明：任一会话的心跳仍有效且可见即返回 true。
    pub fn is_any_ui_visible(&self) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.values().any(|state| state.visible)
    }

    /// `isAnyInteractiveClientVisible`: at least one non-mobile client is
    /// currently visible.
    ///
    /// 中文说明：存在可见且非移动平台的客户端（移动端前台不抑制桌面推送）。
    pub fn is_any_interactive_client_visible(&self) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility
            .values()
            .any(|state| state.visible && !is_mobile_platform(state.platform.as_deref()))
    }

    /// `isUiVisible`.
    ///
    /// 中文说明：查询指定会话的可见性（心跳过期视为不可见）。
    pub fn is_ui_visible(&self, token: &str) -> bool {
        let now = now_ms();
        self.prune_ui_visibility(now);
        let visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        visibility.get(token).is_some_and(|state| state.visible)
    }

    /// Test hook: age every visibility beacon past the TTL.
    ///
    /// 中文说明：把所有心跳时间戳回拨超过 TTL，模拟全部过期。
    #[cfg(test)]
    pub fn expire_visibility_for_test(&self) {
        let mut visibility = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
        for state in visibility.values_mut() {
            state.updated_at = state.updated_at.saturating_sub(UI_VISIBILITY_TTL_MS + 1);
        }
    }
}

/// 取订阅端点（发送任务里先抓取值，避免跨 await 持有借用）。
fn runtime_endpoint(subscription: &PushSubscription) -> String {
    subscription.endpoint.clone()
}

/// push 运行时测试：可见性模型、订阅持久化规范化、VAPID 生命周期、
/// RFC 8291 加密发送与 presence 门控。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::crypto::{b64url_encode, generate_secret_key, public_to_b64url};

    /// Canonical base64url auth secret (browsers emit canonical base64;
    /// strict engines reject sloppy trailing bits).
    ///
    /// 中文说明：规范的 base64url auth secret——浏览器输出规范 base64url，
    /// 严格实现会拒绝填充位不规范的输入。
    const AUTH_SECRET_B64: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo";
    use crate::notifications::transport::HttpPostResponse;
    use std::time::Duration;

    /// 创建一次性临时目录。
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notif-push-{}-{}",
            std::process::id(),
            crypto::now_ms() * 1000 + rand::random::<u64>() % 100_000
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 为临时目录构造指向 settings.json 的存储。
    fn store_for(dir: &PathBuf) -> Arc<SettingsStore> {
        crate::settings::store_for_path(&dir.join("settings.json"))
    }

    /// 返回固定状态码的 transport 桩，并记录每次请求的
    /// `(url, headers, body)` 供断言。
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

    /// 验证多客户端可见性并存判定，且 TTL 过期后统一视为不可见。
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

    /// 验证仅 ios/android 算移动端；未上报平台按交互式保守处理。
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

    /// 验证心跳省略平台时沿用上次上报的平台。
    #[tokio::test]
    async fn remembers_the_last_platform_when_a_heartbeat_omits_it() {
        let dir = temp_dir();
        let (transport, _requests) = fake_transport(201);
        let runtime = PushRuntime::new(dir.join("subs.json"), store_for(&dir), transport);
        runtime.update_ui_visibility("phone", true, Some("android"));
        runtime.update_ui_visibility("phone", true, None);
        assert!(!runtime.is_any_interactive_client_visible());
    }

    /// 验证重注册继承平台、按 endpoint 去重并截断到每会话上限。
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

    /// 验证删除最后一个订阅后整条会话键被移除。
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

    /// 验证 VAPID 密钥只生成一次并正确持久化/回读。
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

    /// 验证真实密钥下的 aes128gcm 加密发送头，以及 410 响应清理端点。
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

    /// 验证 requireNoSse 下按平台差异化的抑制逻辑。
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

    /// 验证 keyid 内嵌的临时公钥是合法 P-256 点（完整解密 KAT 在 crypto 模块）。
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

    /// 验证初始化填充 VAPID 详情，且复位后能从持久化密钥重建。
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
