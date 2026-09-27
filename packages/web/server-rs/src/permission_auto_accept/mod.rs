//! Port of `server/lib/permission-auto-accept/runtime.js`.
//!
//! Auto-accepts pending OpenCode permission requests for sessions whose
//! policy entry is `true`, walking the session parent chain (a subagent
//! session inherits its parent's policy). Policy persists under the
//! `permissionAutoAccept` key of settings.json and every mutation bumps
//! `revision` and broadcasts `ompchamber:permission-auto-accept.updated`.
//!
//! JS dependency injection (`fetchImpl`, settings runtime) maps to the
//! [`Fetch`] and [`SettingsAccess`] seams so tests can stub both.
//!
//! 中文说明：核心策略是「沿父链继承」——子代理会话沿用父会话的开关；
//! 策略持久化于 settings.json 的 `permissionAutoAccept` 键，每次变更递增
//! `revision` 并广播 `ompchamber:permission-auto-accept.updated`。
//! [`Fetch`] 与 [`SettingsAccess`] 两个接缝分别对应 JS 的 `fetchImpl`
//! 与 settings 运行时注入，测试可整体替换。

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::engine::EngineState;

/// settings.json 中存放本策略的键名。
const SETTINGS_KEY: &str = "permissionAutoAccept";
/// 会话信息缓存的容量上限（超出后按 FIFO 淘汰最旧条目）。
const SESSION_CACHE_LIMIT: usize = 10_000;
/// 单次 OpenCode HTTP 请求的超时（毫秒）。
const REQUEST_TIMEOUT_MS: u64 = 5_000;
/// 权限应答的重试间隔序列（立即、250ms、1s），最多三连试。
const RETRY_DELAYS_MS: [u64; 3] = [0, 250, 1000];

/// 会话 ID → 是否自动应答；未显式设置的会话沿父链继承。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    /// 各会话的开关状态。
    pub sessions: HashMap<String, bool>,
    /// 每次持久化变更 +1，供客户端检测策略版本。
    pub revision: u64,
}

/// 序列化辅助。
impl Policy {
    /// 生成对外暴露的 `{ sessions, revision }` JSON 形状。
    fn snapshot_json(&self) -> Value {
        json!({ "sessions": self.sessions, "revision": self.revision })
    }
}

/// 从引擎 `GET /session/{id}` 响应中提取的最小字段集。
#[derive(Debug, Clone)]
struct SessionInfo {
    /// 父会话 ID（子代理链）；顶层会话为 `None`。
    parent_id: Option<String>,
    /// 会话工作目录，用于带 directory 参数的引擎请求。
    directory: Option<String>,
}

/// Error carrying the upstream HTTP status (JS attaches `error.status`).
/// 502/503 为本层合成的网络/无引擎错误，其余为上游原样状态码。
#[derive(Debug, thiserror::Error)]
#[error("OpenCode request failed ({status})")]
pub struct EngineRequestError {
    /// 上游（或本层合成）的 HTTP 状态码。
    pub status: u16,
}

/// `Ok(None)` 表示 2xx 但 body 非 JSON（JS `response.json().catch(() => null)`）。
pub type FetchFuture =
    Pin<Box<dyn Future<Output = Result<Option<Value>, EngineRequestError>> + Send>>;
/// 参数依次为 (path, directory, method, body)；生产绑定 `EngineState`。
pub type Fetch = Arc<dyn Fn(&str, Option<&str>, &str, Option<&Value>) -> FetchFuture + Send + Sync>;

/// Settings read/persist seam. The settings module owns migrations and
/// full-document persistence; [`FileSettingsAccess`] is the interim honest
/// default (missing file = defaults, malformed = error, write via
/// temp+rename).
/// 读写均可能失败；实现需保证 `persist_key` 不破坏文档中的其它键。
pub trait SettingsAccess: Send + Sync {
    /// 读取整份 settings 文档；文件缺失应返回空 JSON 对象。
    fn read_settings(&self) -> std::io::Result<Value>;
    /// Merge `key: value` into the persisted settings document.
    /// 只覆盖给定键，其余内容保持不变。
    fn persist_key(&self, key: &str, value: &Value) -> std::io::Result<()>;
}

/// 过渡期实现：直接读写磁盘文件，不经过 settings 模块的迁移管线。
pub struct FileSettingsAccess {
    /// settings.json 的完整路径。
    path: std::path::PathBuf,
}

/// 构造器。
impl FileSettingsAccess {
    /// 以数据目录定位 `settings.json`。
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("settings.json"),
        }
    }
}

/// 文件版读写实现。
impl SettingsAccess for FileSettingsAccess {
    /// NotFound 视作空文档；解析失败转为 `io::Error`（带说明）上抛。
    fn read_settings(&self) -> std::io::Result<Value> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| std::io::Error::other(format!("malformed settings.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
            Err(e) => Err(e),
        }
    }

    /// 读-改-写：合并键后经 `.json.tmp` 临时文件原子替换，避免半写损坏。
    fn persist_key(&self, key: &str, value: &Value) -> std::io::Result<()> {
        let mut document = self.read_settings()?;
        if let Some(map) = document.as_object_mut() {
            map.insert(key.to_string(), value.clone());
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(&document)?)?;
        std::fs::rename(&temp, &self.path)
    }
}

/// 由 `tokio::sync::Mutex` 保护，所有公开方法串行访问。
struct Inner {
    /// 当前策略（首次加载后常驻内存）。
    policy: Policy,
    /// 是否已从 settings 完成首次加载（懒加载标记）。
    loaded: bool,
    /// 会话 ID → 元数据缓存。
    sessions: HashMap<String, SessionInfo>,
    /// 会话插入顺序（FIFO），配合上限做淘汰。
    session_order: VecDeque<String>,
}

/// 自动应答运行时；hub 为 `None`（部分测试场景）时跳过事件广播。
pub struct PermissionAutoAccept {
    /// 互斥保护的可变状态（策略与会话缓存）。
    inner: tokio::sync::Mutex<Inner>,
    /// 引擎 HTTP 接缝。
    fetch: Fetch,
    /// settings 读写接缝。
    settings: Arc<dyn SettingsAccess>,
    /// 事件广播中心。
    hub: Option<Arc<crate::hub::EventHub>>,
}

/// 构造、策略读写、会话记忆与权限应答处理。
impl PermissionAutoAccept {
    /// fetch 闭包自带鉴权头与 5 秒超时，测试无法触达真实网络。
    pub fn new(
        engine: Arc<EngineState>,
        settings: Arc<dyn SettingsAccess>,
        hub: Option<Arc<crate::hub::EventHub>>,
    ) -> Arc<Self> {
        let engine_for_fetch = Arc::clone(&engine);
        let fetch: Fetch = Arc::new(
            move |path: &str, directory: Option<&str>, method: &str, body: Option<&Value>| {
                let engine = Arc::clone(&engine_for_fetch);
                let path = path.to_string();
                let directory = directory.map(String::from);
                let method = method.to_string();
                let body = body.cloned();
                Box::pin(async move {
                    let Some(base) = engine.base_url() else {
                        return Err(EngineRequestError { status: 503 });
                    };
                    let mut url = format!("{base}{path}");
                    if let Some(directory) = directory.as_deref() {
                        let separator = if url.contains('?') { '&' } else { '?' };
                        url = format!(
                            "{url}{separator}directory={}",
                            encode_uri_component(directory)
                        );
                    }
                    let method = reqwest::Method::from_bytes(method.as_bytes())
                        .unwrap_or(reqwest::Method::GET);
                    let mut request = engine
                        .http()
                        .request(method, &url)
                        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
                        .header("accept", "application/json");
                    if let Some(auth) = engine.auth_header() {
                        request = request.header("authorization", auth);
                    }
                    if let Some(body) = body.as_ref() {
                        request = request.json(body);
                    }
                    let response = request
                        .send()
                        .await
                        .map_err(|_| EngineRequestError { status: 502 })?;
                    let status = response.status().as_u16();
                    if !(200..300).contains(&status) {
                        return Err(EngineRequestError { status });
                    }
                    // JS: response.json().catch(() => null) — a non-JSON body
                    // is a null payload, not a failure.
                    let text = response
                        .text()
                        .await
                        .map_err(|_| EngineRequestError { status: 502 })?;
                    Ok(serde_json::from_str(&text).ok())
                })
            },
        );
        Arc::new(Self::with_fetch(fetch, settings, hub))
    }

    /// 测试主入口：直接注入 fetch 接缝，绕过 `EngineState` 绑定。
    fn with_fetch(
        fetch: Fetch,
        settings: Arc<dyn SettingsAccess>,
        hub: Option<Arc<crate::hub::EventHub>>,
    ) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(Inner {
                policy: Policy::default(),
                loaded: false,
                sessions: HashMap::new(),
                session_order: VecDeque::new(),
            }),
            fetch,
            settings,
            hub,
        }
    }

    /// 首次调用从 settings 读取，此后返回内存副本；失败以字符串错误返回。
    async fn load(&self) -> Result<Policy, String> {
        let mut inner = self.inner.lock().await;
        if inner.loaded {
            return Ok(inner.policy.clone());
        }
        let document = self
            .settings
            .read_settings()
            .map_err(|e| format!("failed to load permission auto-accept policy: {e}"))?;
        inner.policy = normalize_policy(document.get(SETTINGS_KEY).unwrap_or(&Value::Null));
        inner.loaded = true;
        Ok(inner.policy.clone())
    }

    /// Serialized read-modify-write-broadcast (JS `persistUpdate` chain).
    /// 锁内完成更新与持久化，锁释放后再广播，避免持锁做 IO 等待广播方。
    async fn persist_update<F>(&self, update: F) -> Result<Policy, String>
    where
        F: FnOnce(Policy) -> Policy,
    {
        let mut inner = self.inner.lock().await;
        let next = update(inner.policy.clone());
        self.settings
            .persist_key(SETTINGS_KEY, &next.snapshot_json())
            .map_err(|e| format!("failed to persist permission auto-accept policy: {e}"))?;
        inner.policy = next.clone();
        inner.loaded = true;
        let snapshot = inner.policy.snapshot_json();
        drop(inner);
        if let Some(hub) = &self.hub {
            hub.publish_json(
                "ompchamber:permission-auto-accept.updated",
                &json!({
                    "type": "ompchamber:permission-auto-accept.updated",
                    "properties": snapshot,
                }),
            );
        }
        Ok(next)
    }

    /// 校验输入（sessionId 非空、enabled 必须是布尔）→ 写入并递增
    /// revision → 开启时立即对给定 directory 补一次 pending 对账。
    pub async fn set_session_policy(
        &self,
        session_id: &str,
        enabled: Option<bool>,
        directory: Option<&str>,
    ) -> Result<Policy, SetPolicyError> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(SetPolicyError::Invalid("sessionId is required".into()));
        }
        let Some(enabled) = enabled else {
            return Err(SetPolicyError::Invalid("enabled must be a boolean".into()));
        };
        self.load().await.map_err(SetPolicyError::Load)?;
        let key = session_id.to_string();
        let policy = self
            .persist_update(|current| Policy {
                sessions: {
                    let mut sessions = current.sessions;
                    sessions.insert(key.clone(), enabled);
                    sessions
                },
                revision: current.revision + 1,
            })
            .await
            .map_err(SetPolicyError::Load)?;
        if enabled {
            let _ = self
                .reconcile_pending(&[directory.unwrap_or_default()])
                .await;
        }
        Ok(policy)
    }

    /// info 缺 id 时静默忽略；同时维护缓存与 FIFO 顺序，超限淘汰最旧。
    fn remember_session(&self, inner: &mut Inner, info: &Value, directory_hint: Option<&str>) {
        let Some(id) = info
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let parent_id = info
            .get("parentID")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from);
        let directory = info
            .get("directory")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .or_else(|| directory_hint.map(String::from));
        if inner
            .sessions
            .insert(
                id.to_string(),
                SessionInfo {
                    parent_id,
                    directory,
                },
            )
            .is_none()
        {
            inner.session_order.push_back(id.to_string());
        }
        while inner.session_order.len() > SESSION_CACHE_LIMIT {
            if let Some(oldest) = inner.session_order.pop_front() {
                inner.sessions.remove(&oldest);
            }
        }
    }

    /// 未命中经引擎 `GET /session/{id}` 拉取并回填缓存。
    async fn get_session(
        &self,
        inner: &mut Inner,
        session_id: &str,
        directory: Option<&str>,
    ) -> Option<SessionInfo> {
        if let Some(cached) = inner.sessions.get(session_id) {
            return Some(cached.clone());
        }
        let path = format!("/session/{}", encode_uri_component(session_id));
        let result = (self.fetch)(&path, directory, "GET", None).await.ok()??;
        let info = result.get("data").unwrap_or(&result);
        self.remember_session(inner, info, directory);
        inner.sessions.get(session_id).cloned()
    }

    /// Walk the parent chain until a policy entry answers; a fetch failure or
    /// a cycle resolves to `false` (JS semantics).
    /// 用 seen 集合检测父链环；每层继承更具体的 directory（若有）。
    pub async fn is_session_auto_accepting(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> bool {
        if self.load().await.is_err() {
            return false;
        }
        let mut inner = self.inner.lock().await;
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = Some(session_id.to_string());
        let mut current_directory = directory.map(String::from);
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break;
            }
            if let Some(enabled) = inner.policy.sessions.get(&id) {
                return *enabled;
            }
            let info = match self
                .get_session(&mut inner, &id, current_directory.as_deref())
                .await
            {
                Some(info) => info,
                None => return false,
            };
            current = info.parent_id;
            if let Some(directory) = info.directory {
                current_directory = Some(directory);
            }
        }
        false
    }

    /// 缺 id / sessionID 或策略未开启时返回 `Ok(false)`（无需应答）。
    async fn reply_once(
        &self,
        permission: &Value,
        directory: Option<&str>,
    ) -> Result<bool, EngineRequestError> {
        let Some(id) = permission.get("id").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(session_id) = permission.get("sessionID").and_then(Value::as_str) else {
            return Ok(false);
        };
        if !self.is_session_auto_accepting(session_id, directory).await {
            return Ok(false);
        }
        let path = format!("/permission/{}/reply", encode_uri_component(id));
        (self.fetch)(&path, directory, "POST", Some(&json!({ "reply": "once" }))).await?;
        Ok(true)
    }

    /// Bounded retries per permission id; a 404 during reply counts as
    /// handled (the permission already settled).
    /// 返回是否成功应答；重试耗尽仍失败返回 false。
    pub async fn process_permission(&self, permission: Value, directory: Option<String>) -> bool {
        let Some(id) = permission
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return false;
        };
        for delay in RETRY_DELAYS_MS {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match self.reply_once(&permission, directory.as_deref()).await {
                Ok(handled) => return handled,
                Err(error) if error.status == 404 => {
                    tracing::debug!("[permission-auto-accept] permission {id} already settled");
                    return true;
                }
                Err(_) => continue,
            }
        }
        false
    }

    /// Reconcile pending permissions for the given directories plus the
    /// global scope. One failed scope never blocks the others.
    /// 目录先 trim/排序/去重，再加全局作用域（None）；按权限 id 去重后并发处理。
    pub async fn reconcile_pending(&self, directories: &[&str]) {
        if self.load().await.is_err() {
            return;
        }
        let mut normalized: Vec<String> = directories
            .iter()
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
            .map(String::from)
            .collect();
        normalized.sort();
        normalized.dedup();

        let mut scopes: Vec<Option<String>> = vec![None];
        scopes.extend(normalized.into_iter().map(Some));

        let mut pending_by_id: HashMap<String, (Value, Option<String>)> = HashMap::new();
        for directory in scopes {
            let payload = match (self.fetch)("/permission", directory.as_deref(), "GET", None).await
            {
                Ok(Some(payload)) => payload,
                _ => continue,
            };
            let pending = payload
                .as_array()
                .cloned()
                .or_else(|| payload.get("data").and_then(Value::as_array).cloned());
            let Some(pending) = pending else { continue };
            for permission in pending {
                let Some(id) = permission.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let scope = permission
                    .get("directory")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| directory.clone());
                pending_by_id.insert(id.to_string(), (permission, scope));
            }
        }

        futures::future::join_all(pending_by_id.into_values().map(|(permission, directory)| {
            let this: &PermissionAutoAccept = self;
            async move { this.process_permission(permission, directory).await }
        }))
        .await;
    }

    /// Event ingestion (JS `processEvent` via the global hub). Wiring to the
    /// event stream lands with that module's integration.
    /// directory 为 "global" 时按无目录处理（与 JS 的全局作用域一致）。
    pub async fn process_event(&self, event: &Value, directory: Option<&str>) {
        let raw = event.get("payload").unwrap_or(event);
        let payload = raw.get("payload").filter(|p| p.is_object()).unwrap_or(raw);
        let directory = directory.filter(|d| *d != "global");
        match payload.get("type").and_then(Value::as_str) {
            Some("session.created" | "session.updated") => {
                let mut inner = self.inner.lock().await;
                if let Some(info) = payload.pointer("/properties/info") {
                    self.remember_session(&mut inner, info, directory);
                }
            }
            Some("permission.asked") => {
                if let Some(permission) = payload.get("properties").cloned() {
                    let _ = self
                        .process_permission(permission, directory.map(String::from))
                        .await;
                }
            }
            _ => {}
        }
    }
}

/// 两类失败分别映射 400 与 500。
#[derive(Debug, thiserror::Error)]
pub enum SetPolicyError {
    /// 请求参数非法（400）：sessionId 为空或 enabled 非布尔。
    #[error("{0}")]
    Invalid(String),
    /// settings 读/写失败（500）。
    #[error("{0}")]
    Load(String),
}

/// `encodeURIComponent` parity (unreserved set A-Za-z0-9 - _ . ! ~ * ' ( )).
/// 非 UTF-8 输入按字节逐个转义（与 JS 按 UTF-16 码元的边界差异可忽略）。
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// axum 路由的共享状态载体。
#[derive(Clone)]
struct ModuleState {
    /// 自动应答运行时单例。
    runtime: Arc<PermissionAutoAccept>,
}

/// `GET /api/permission-auto-accept`：返回策略 JSON 快照；加载失败返回 500。
async fn get_policy(State(state): State<ModuleState>) -> Response {
    match state.runtime.load().await {
        Ok(policy) => Json(policy.snapshot_json()).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `PUT /api/permission-auto-accept/sessions/{sessionId}`：设置开关并返回
/// 新策略；参数非法 400、持久化失败 500。
async fn put_session(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    let enabled = body.get("enabled").and_then(Value::as_bool);
    let directory = body.get("directory").and_then(Value::as_str);
    match state
        .runtime
        .set_session_policy(&session_id, enabled, directory)
        .await
    {
        Ok(policy) => Json(policy.snapshot_json()).into_response(),
        Err(SetPolicyError::Invalid(message)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
        }
        Err(SetPolicyError::Load(message)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
    }
}

/// 注册 get_policy / put_session 两条路由，状态为运行时 `Arc`。
pub fn router(ctx: RouterContext) -> Router {
    let settings = Arc::new(FileSettingsAccess::new(&ctx.config.data_dir));
    let runtime = PermissionAutoAccept::new(
        Arc::clone(&ctx.engine),
        settings,
        Some(Arc::clone(&ctx.hub)),
    );
    Router::new()
        .route("/api/permission-auto-accept", get(get_policy))
        .route(
            "/api/permission-auto-accept/sessions/{sessionId}",
            put(put_session),
        )
        .with_state(ModuleState { runtime })
}

/// 丢弃空 ID 与非布尔条目；revision 只接受 0..=i32::MAX，其余回退 0。
fn normalize_policy(value: &Value) -> Policy {
    let mut sessions = HashMap::new();
    if let Some(map) = value.get("sessions").and_then(Value::as_object) {
        for (session_id, enabled) in map {
            if session_id.is_empty() {
                continue;
            }
            if let Some(enabled) = enabled.as_bool() {
                sessions.insert(session_id.clone(), enabled);
            }
        }
    }
    let revision = value
        .get("revision")
        .and_then(Value::as_i64)
        .filter(|r| *r >= 0 && *r <= i64::from(i32::MAX))
        .unwrap_or(0) as u64;
    Policy { sessions, revision }
}

/// 对齐 JS vitest 套件的行为测试。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// 纯内存读写，永不触盘。
    struct MemorySettings {
        /// 当前文档内容。
        document: std::sync::Mutex<Value>,
    }

    /// 纯内存读写。
    impl SettingsAccess for MemorySettings {
        /// 返回文档深拷贝，避免借用泄漏。
        fn read_settings(&self) -> std::io::Result<Value> {
            Ok(self.document.lock().expect("settings lock").clone())
        }
        /// 就地合并键值，永不失败。
        fn persist_key(&self, key: &str, value: &Value) -> std::io::Result<()> {
            let mut guard = self.document.lock().expect("settings lock");
            if let Some(map) = guard.as_object_mut() {
                map.insert(key.to_string(), value.clone());
            }
            Ok(())
        }
    }

    /// fetch stub 只响应 `/session/{id}`，未命中一律 404。
    fn runtime_with(policy: Value, sessions: Vec<(String, Value)>) -> Arc<PermissionAutoAccept> {
        let settings: Arc<dyn SettingsAccess> = Arc::new(MemorySettings {
            document: std::sync::Mutex::new(policy),
        });
        let sessions = Arc::new(sessions);
        let fetch: Fetch = Arc::new(
            move |path: &str, _directory: Option<&str>, _method: &str, _body: Option<&Value>| {
                let sessions = Arc::clone(&sessions);
                let path = path.to_string();
                Box::pin(async move {
                    if let Some(id) = path
                        .strip_prefix("/session/")
                        .map(|p| p.split('?').next().unwrap_or(p))
                    {
                        if let Some((_, info)) = sessions.iter().find(|(sid, _)| sid == id) {
                            return Ok(Some(info.clone()));
                        }
                    }
                    Err(EngineRequestError { status: 404 })
                })
            },
        );
        Arc::new(PermissionAutoAccept::with_fetch(fetch, settings, None))
    }

    /// 验证 normalize_policy 过滤非法条目、负 revision 回退 0、Null 得默认策略。
    #[test]
    fn normalize_policy_filters_and_defaults_revision() {
        let policy = normalize_policy(&json!({
            "sessions": { "s1": true, "s2": "nope", "": true },
            "revision": 3
        }));
        assert_eq!(policy.sessions, HashMap::from([("s1".to_string(), true)]));
        assert_eq!(policy.revision, 3);

        assert_eq!(normalize_policy(&json!({ "revision": -5 })).revision, 0);
        assert_eq!(normalize_policy(&Value::Null), Policy::default());
    }

    /// 验证子会话继承父策略 true；父链成环与父缺失（404）均终止于 false。
    #[tokio::test]
    async fn inherits_parent_policy_and_cycles_terminate() {
        // s1 has policy true; s2 is a child of s1; s3/s4 form a cycle; s5's
        // parent is missing (fetch 404 ⇒ walk ends false).
        let runtime = runtime_with(
            json!({ "permissionAutoAccept": { "sessions": { "s1": true }, "revision": 1 } }),
            vec![
                (
                    "s2".to_string(),
                    json!({ "id": "s2", "parentID": "s1", "directory": "/repo" }),
                ),
                ("s3".to_string(), json!({ "id": "s3", "parentID": "s4" })),
                ("s4".to_string(), json!({ "id": "s4", "parentID": "s3" })),
                (
                    "s5".to_string(),
                    json!({ "id": "s5", "parentID": "missing" }),
                ),
            ],
        );
        runtime.load().await.expect("load");
        assert!(runtime.is_session_auto_accepting("s2", None).await);
        assert!(!runtime.is_session_auto_accepting("s3", None).await);
        assert!(!runtime.is_session_auto_accepting("s5", None).await);
    }

    /// 验证空 sessionId 与缺失 enabled 被 JS 同款地拒绝为 Invalid。
    #[tokio::test]
    async fn set_policy_rejects_invalid_input_like_js() {
        let runtime = runtime_with(json!({}), vec![]);
        assert!(matches!(
            runtime.set_session_policy("  ", Some(true), None).await,
            Err(SetPolicyError::Invalid(_))
        ));
        assert!(matches!(
            runtime.set_session_policy("s1", None, None).await,
            Err(SetPolicyError::Invalid(_))
        ));
    }

    /// 验证设置策略后 revision 递增且持久化；新运行时重载仍能读到该开关。
    #[tokio::test]
    async fn set_policy_persists_and_bumps_revision() {
        let runtime = runtime_with(json!({}), vec![]);
        let policy = runtime
            .set_session_policy("abc", Some(true), None)
            .await
            .expect("policy");
        assert_eq!(policy.revision, 1);
        assert_eq!(policy.sessions.get("abc"), Some(&true));
        // Reload from the settings document proves persistence.
        let runtime2 = {
            let document = runtime.settings.read_settings().expect("read persisted");
            let settings: Arc<dyn SettingsAccess> = Arc::new(MemorySettings {
                document: std::sync::Mutex::new(document),
            });
            Arc::new(PermissionAutoAccept::with_fetch(
                Arc::new(|_, _, _, _| Box::pin(async { Err(EngineRequestError { status: 404 }) })),
                settings,
                None,
            ))
        };
        assert!(runtime2.is_session_auto_accepting("abc", None).await);
    }

    /// 验证 session.updated 事件记忆会话元数据，后续判定无需再拉引擎。
    #[tokio::test]
    async fn process_event_remembers_sessions_and_asks_permissions() {
        let runtime = runtime_with(
            json!({ "permissionAutoAccept": { "sessions": { "s1": true }, "revision": 1 } }),
            vec![],
        );
        runtime
            .process_event(
                &json!({ "payload": { "type": "session.updated", "properties": { "info": {
                    "id": "s9", "parentID": "s1", "directory": "/repo"
                } } } }),
                Some("/repo"),
            )
            .await;
        assert!(runtime.is_session_auto_accepting("s9", None).await);
    }

    /// 验证两条路由返回 JS 同款状态码与响应体形状（400/200、sessions+revision）。
    #[tokio::test]
    async fn routes_answer_js_shapes() {
        let settings: Arc<dyn SettingsAccess> = Arc::new(MemorySettings {
            document: std::sync::Mutex::new(json!({})),
        });
        let engine = EngineState::external("http://127.0.0.1:9".into(), None);
        let runtime = PermissionAutoAccept::new(engine, settings, None);
        let app: Router = Router::new()
            .route("/api/permission-auto-accept", get(get_policy))
            .route(
                "/api/permission-auto-accept/sessions/{sessionId}",
                put(put_session),
            )
            .with_state(ModuleState { runtime });

        let invalid = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/permission-auto-accept/sessions/abc")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled": "yes"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let valid = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/permission-auto-accept/sessions/abc")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled": true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::OK);
        let body = axum::body::to_bytes(valid.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["revision"], 1);
        assert_eq!(value["sessions"]["abc"], true);
    }

    /// 验证转义结果与 JS 非保留字符集逐字符一致。
    #[test]
    fn encode_uri_component_matches_js_unreserved_set() {
        assert_eq!(encode_uri_component("aZ0-_.!~*'()"), "aZ0-_.!~*'()");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
    }
}
