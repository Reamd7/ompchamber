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

const SETTINGS_KEY: &str = "permissionAutoAccept";
const SESSION_CACHE_LIMIT: usize = 10_000;
const REQUEST_TIMEOUT_MS: u64 = 5_000;
const RETRY_DELAYS_MS: [u64; 3] = [0, 250, 1000];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    pub sessions: HashMap<String, bool>,
    pub revision: u64,
}

impl Policy {
    fn snapshot_json(&self) -> Value {
        json!({ "sessions": self.sessions, "revision": self.revision })
    }
}

#[derive(Debug, Clone)]
struct SessionInfo {
    parent_id: Option<String>,
    directory: Option<String>,
}

/// Error carrying the upstream HTTP status (JS attaches `error.status`).
#[derive(Debug, thiserror::Error)]
#[error("OpenCode request failed ({status})")]
pub struct EngineRequestError {
    pub status: u16,
}

pub type FetchFuture =
    Pin<Box<dyn Future<Output = Result<Option<Value>, EngineRequestError>> + Send>>;
pub type Fetch = Arc<dyn Fn(&str, Option<&str>, &str, Option<&Value>) -> FetchFuture + Send + Sync>;

/// Settings read/persist seam. The settings module owns migrations and
/// full-document persistence; [`FileSettingsAccess`] is the interim honest
/// default (missing file = defaults, malformed = error, write via
/// temp+rename).
pub trait SettingsAccess: Send + Sync {
    fn read_settings(&self) -> std::io::Result<Value>;
    /// Merge `key: value` into the persisted settings document.
    fn persist_key(&self, key: &str, value: &Value) -> std::io::Result<()>;
}

pub struct FileSettingsAccess {
    path: std::path::PathBuf,
}

impl FileSettingsAccess {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("settings.json"),
        }
    }
}

impl SettingsAccess for FileSettingsAccess {
    fn read_settings(&self) -> std::io::Result<Value> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| std::io::Error::other(format!("malformed settings.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
            Err(e) => Err(e),
        }
    }

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

struct Inner {
    policy: Policy,
    loaded: bool,
    sessions: HashMap<String, SessionInfo>,
    session_order: VecDeque<String>,
}

pub struct PermissionAutoAccept {
    inner: tokio::sync::Mutex<Inner>,
    fetch: Fetch,
    settings: Arc<dyn SettingsAccess>,
    hub: Option<Arc<crate::hub::EventHub>>,
}

impl PermissionAutoAccept {
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

#[derive(Debug, thiserror::Error)]
pub enum SetPolicyError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Load(String),
}

/// `encodeURIComponent` parity (unreserved set A-Za-z0-9 - _ . ! ~ * ' ( )).
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

#[derive(Clone)]
struct ModuleState {
    runtime: Arc<PermissionAutoAccept>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    struct MemorySettings {
        document: std::sync::Mutex<Value>,
    }

    impl SettingsAccess for MemorySettings {
        fn read_settings(&self) -> std::io::Result<Value> {
            Ok(self.document.lock().expect("settings lock").clone())
        }
        fn persist_key(&self, key: &str, value: &Value) -> std::io::Result<()> {
            let mut guard = self.document.lock().expect("settings lock");
            if let Some(map) = guard.as_object_mut() {
                map.insert(key.to_string(), value.clone());
            }
            Ok(())
        }
    }

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

    #[test]
    fn encode_uri_component_matches_js_unreserved_set() {
        assert_eq!(encode_uri_component("aZ0-_.!~*'()"), "aZ0-_.!~*'()");
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
    }
}
