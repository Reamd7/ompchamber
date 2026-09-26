//! Port of `server/lib/agent-tool/runtime.js`.
//!
//! Exposes OpenChamber to agents as typed OpenCode custom tools:
//!
//! - [`AgentToolRuntime::prepare_managed_opencode_env`] materializes the
//!   generated JS plugin (see [`plugin`]) under `<data-dir>/agent-tool/`,
//!   appends its `file://` URL to `OPENCODE_CONFIG_CONTENT` without replacing
//!   existing plugin entries, and mints a fresh per-child token plus loopback
//!   callback URL. The main entry point calls it when spawning the managed
//!   OpenCode engine.
//! - `POST /api/ompchamber/agent-tool` accepts loopback requests only and
//!   requires the current per-child bearer token (timing-safe compare), then
//!   dispatches a fixed action allowlist through the shared OpenChamber
//!   control service (the `executeAction` dependency in the JS DI).
//!
//! Wiring notes (main owns these):
//! - serve with `into_make_service_with_connect_info::<SocketAddr>()` so the
//!   loopback gate can see the peer address (missing `ConnectInfo` fails
//!   closed, mirroring `req.socket?.remoteAddress` being undefined);
//! - call `prepare_managed_opencode_env` at managed-engine spawn time, with
//!   the include flags from the persisted settings
//!   (`agentControlToolEnabled` / `agentWebToolEnabled` /
//!   `agentMemoryToolEnabled`, each enabled while not `false`), and merge the
//!   returned values into the child environment;
//! - compose the control service (`ControlDeps::for_engine` + the
//!   scheduled-task service from `scheduled_tasks::build_runtime`), wrap it
//!   with [`executor_for_control_service`], and build the router via
//!   [`router_with_runtime`] — the JS wires the same shared service into the
//!   CLI adapter and this runtime.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, RwLock};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::context::RouterContext;
use crate::openchamber_control::actions as control_actions;

mod plugin;

#[cfg(test)]
mod tests;

/// The callback route every generated tool posts to.
pub const ROUTE_PATH: &str = "/api/ompchamber/agent-tool";

/// `express.json({ limit: '1mb' })` parity.
const BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// The wire envelope version (`TOOL_SCHEMA_VERSION`).
const TOOL_SCHEMA_VERSION: u32 = 1;

pub type PortProvider = Arc<dyn Fn() -> Option<u16> + Send + Sync>;
pub type ConfigContentProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Mirror of the JS `executeAction(action, input, contextDirectory, options)`
/// dependency; `server/index.js` wires the shared OpenChamber control service
/// here. `context_directory` is forwarded exactly as sent (a string or
/// absent), and the signal is the request-abort signal from `options`.
pub type ExecuteActionFn = Arc<
    dyn Fn(
            &str,
            Value,
            Option<Value>,
            AbortSignal,
        ) -> Pin<Box<dyn Future<Output = Result<Value, ActionExecutionError>> + Send>>
        + Send
        + Sync,
>;

/// The error fields `execute`'s catch reads off a thrown control-service
/// error: `message`, `statusCode`, `partial` plus the partial-failure
/// metadata surfaced to the agent.
#[derive(Debug, Clone, Default)]
pub struct ActionExecutionError {
    pub message: String,
    pub status_code: Option<i64>,
    pub partial: bool,
    pub partial_action: Option<Value>,
    pub session_id: Option<Value>,
    pub directory: Option<Value>,
}

/// Request-cancellation signal handed to the control service (JS:
/// `AbortController` aborted from the request's `aborted`/`close` events).
///
/// Dropping the [`AbortHandle`] aborts unless the response completed
/// (`res.writableEnded` parity): in Rust the handler future itself is dropped
/// on client disconnect, so the drop guard is what fires the abort.
#[derive(Clone)]
pub struct AbortSignal {
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl AbortSignal {
    pub fn is_aborted(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolves once the signal is (or becomes) aborted, or the sender is gone.
    pub async fn aborted(&mut self) {
        loop {
            if *self.receiver.borrow() {
                return;
            }
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

pub struct AbortHandle {
    sender: tokio::sync::watch::Sender<bool>,
    finished: bool,
}

impl AbortHandle {
    pub fn new() -> Self {
        let (sender, _) = tokio::sync::watch::channel(false);
        Self {
            sender,
            finished: false,
        }
    }

    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            receiver: self.sender.subscribe(),
        }
    }

    pub fn abort(&self) {
        let _ = self.sender.send(true);
    }

    /// Marks the response as complete so dropping the guard no longer aborts.
    pub fn finish(&mut self) {
        self.finished = true;
    }
}

impl Default for AbortHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AbortHandle {
    fn drop(&mut self) {
        if !self.finished {
            self.abort();
        }
    }
}

/// Which managed tools to include in the materialized plugin
/// (`prepareManagedOpenCodeEnv`'s include flags; all default to on).
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolIncludes {
    pub control: bool,
    pub web: bool,
    pub memory: bool,
}

impl ToolIncludes {
    pub fn all() -> Self {
        Self {
            control: true,
            web: true,
            memory: true,
        }
    }

    pub fn none(&self) -> bool {
        !self.control && !self.web && !self.memory
    }
}

/// The environment additions `prepareManagedOpenCodeEnv` returns — spread into
/// the managed OpenCode child environment only.
#[derive(Debug, Clone)]
pub struct PreparedManagedEnv {
    pub opencode_config_content: String,
    pub agent_tool_url: String,
    pub agent_tool_token: String,
}

/// The plugin callback payload: the merged tool input, the authoritative
/// session directory, and the calling tool's name.
#[derive(Debug, Default, Deserialize)]
pub struct AgentToolPayload {
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default, rename = "contextDirectory")]
    pub context_directory: Option<Value>,
    #[serde(default)]
    pub tool: Option<Value>,
}

/// `{schemaVersion, ok, action, data?, error?}` — the result contract every
/// completed call answers with (serde keeps the declared key order).
#[derive(Debug, Serialize)]
pub struct ToolResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub ok: bool,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
}

#[derive(Debug, Serialize)]
pub struct ToolError {
    pub message: String,
    pub kind: String,
}

impl ToolResult {
    fn with_error(action: Option<&str>, message: String, kind: &str) -> Self {
        Self {
            schema_version: TOOL_SCHEMA_VERSION,
            ok: false,
            action: action.unwrap_or("unknown").to_string(),
            data: None,
            error: Some(ToolError {
                message,
                kind: kind.to_string(),
            }),
        }
    }

    fn usage_error(action: Option<&str>, message: impl Into<String>) -> Self {
        Self::with_error(action, message.into(), "usage")
    }

    fn runtime_error(action: Option<&str>, message: impl Into<String>) -> Self {
        Self::with_error(action, message.into(), "runtime")
    }
}

/// `asNonEmptyString` core: a string with non-empty trimmed content.
fn as_non_empty_str(value: Option<&str>) -> Option<&str> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// `asNonEmptyString` for JSON values.
fn as_non_empty_string(value: Option<&Value>) -> Option<String> {
    as_non_empty_str(value?.as_str()).map(str::to_string)
}

/// `ACTIONS`: everything either managed tool may ask for — the agent allowlist
/// stays narrower than the full control surface (no `schedule.status`).
fn dispatchable_actions() -> &'static Vec<&'static str> {
    static ACTIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        let mut actions = Vec::new();
        actions.extend(control_actions::AGENT_TOOL_ACTIONS);
        actions.extend(control_actions::WEB_ACTIONS);
        actions.extend(control_actions::MEMORY_ACTIONS);
        actions
    });
    &ACTIONS
}

/// The full `execute` flow, shared by the route and direct callers.
pub async fn execute_with(
    execute_action: Option<&ExecuteActionFn>,
    payload: &AgentToolPayload,
    signal: AbortSignal,
) -> ToolResult {
    let action_value = payload.input.as_ref().and_then(|input| input.get("action"));
    let requested = as_non_empty_string(action_value);
    // Resolved against the calling tool's own actions: models drop the
    // namespace that the tool's name already implies, and answering "read"
    // with a bare "unsupported" leaves them to guess a second wrong name.
    let tool_name = as_non_empty_string(payload.tool.as_ref());
    let action = match control_actions::resolve_agent_tool_action(
        requested.as_deref(),
        tool_name.as_deref(),
    ) {
        Ok(action) => action,
        Err(message) => return ToolResult::usage_error(requested.as_deref(), message),
    };
    if !dispatchable_actions().contains(&action) {
        return ToolResult::usage_error(
            Some(action),
            format!("Unsupported OMPChamber action: {action}"),
        );
    }
    let Some(execute_action) = execute_action else {
        return ToolResult::runtime_error(
            Some(action),
            "OMPChamber control service is unavailable",
        );
    };

    let mut input_map = match payload.input.as_ref() {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    input_map.insert("action".to_string(), Value::String(action.to_string()));
    let input = Value::Object(input_map);

    match execute_action(action, input, payload.context_directory.clone(), signal).await {
        Ok(data) => ToolResult {
            schema_version: TOOL_SCHEMA_VERSION,
            ok: true,
            action: action.to_string(),
            data: Some(data),
            error: None,
        },
        Err(error) => {
            let kind = match error.status_code {
                Some(code) if (400..499).contains(&code) => "usage",
                _ => "runtime",
            };
            let data = if error.partial {
                let mut map = Map::new();
                map.insert("partial".to_string(), Value::Bool(true));
                if let Some(value) = error.partial_action {
                    map.insert("partialAction".to_string(), value);
                }
                if let Some(value) = error.session_id {
                    map.insert("sessionId".to_string(), value);
                }
                if let Some(value) = error.directory {
                    map.insert("directory".to_string(), value);
                }
                Some(Value::Object(map))
            } else {
                None
            };
            ToolResult {
                schema_version: TOOL_SCHEMA_VERSION,
                ok: false,
                action: action.to_string(),
                data,
                error: Some(ToolError {
                    message: error.message,
                    kind: kind.to_string(),
                }),
            }
        }
    }
}

/// `isLoopbackAddress` — accepts exactly the three spellings Node reports for
/// loopback peers.
fn is_loopback_address(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let address = value.to_ascii_lowercase();
    address == "127.0.0.1" || address == "::1" || address == "::ffff:127.0.0.1"
}

/// Constant-time byte comparison (the `crypto.timingSafeEqual` half; the
/// length check mirrors the JS precondition).
fn timing_safe_equal(provided: &[u8], expected: &[u8]) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in provided.iter().zip(expected.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// `authorize(req)`: loopback peer + current per-child bearer token.
pub fn authorize(
    active_token: Option<&str>,
    remote_address: Option<&str>,
    authorization: Option<&str>,
) -> bool {
    let Some(expected) = active_token else {
        return false;
    };
    if !is_loopback_address(remote_address) {
        return false;
    }
    let Some(header) = as_non_empty_str(authorization) else {
        return false;
    };
    let Some(provided) = header.strip_prefix("Bearer ") else {
        return false;
    };
    timing_safe_equal(provided.as_bytes(), expected.as_bytes())
}

/// 32 random bytes, base64url-encoded without padding — `crypto.randomBytes(32)
/// .toString('base64url')`. Never logged or persisted.
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn write_plugin_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await?;
        file.write_all(contents.as_bytes()).await?;
        // tokio::fs::File buffers writes internally and flushes from a
        // background task on drop — without an explicit flush a same-process
        // std::fs reader can observe an empty file after prepare returns.
        file.flush().await?;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(path, contents).await?;
    }
    Ok(())
}

/// The ported `createAgentToolRuntime`. Clones share the active token (the
/// JS closes over one mutable `activeToken`).
#[derive(Clone)]
pub struct AgentToolRuntime {
    plugin_directory: PathBuf,
    plugin_path: PathBuf,
    get_active_port: PortProvider,
    execute_action: Option<ExecuteActionFn>,
    existing_config_content: ConfigContentProvider,
    active_token: Arc<RwLock<Option<String>>>,
}

impl AgentToolRuntime {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        get_active_port: PortProvider,
        execute_action: Option<ExecuteActionFn>,
        existing_config_content: ConfigContentProvider,
    ) -> Self {
        let plugin_directory = data_dir.into().join("agent-tool");
        let plugin_path = plugin_directory.join("ompchamber-plugin.js");
        Self {
            plugin_directory,
            plugin_path,
            get_active_port,
            execute_action,
            existing_config_content,
            active_token: Arc::new(RwLock::new(None)),
        }
    }

    /// Default construction from the router context: the configured port (the
    /// JS reads the authoritative bound port — main should override with the
    /// listener's once wiring lands) and the process `OPENCODE_CONFIG_CONTENT`.
    pub fn for_context(ctx: &RouterContext, execute_action: Option<ExecuteActionFn>) -> Self {
        let port = ctx.config.port;
        Self::new(
            ctx.config.data_dir.clone(),
            Arc::new(move || Some(port)),
            execute_action,
            Arc::new(|| std::env::var("OPENCODE_CONFIG_CONTENT").ok()),
        )
    }

    /// `prepareManagedOpenCodeEnv` — materialize the plugin, rotate the
    /// per-child token, and produce the managed-child env additions.
    pub async fn prepare_managed_opencode_env(
        &self,
        includes: ToolIncludes,
    ) -> Result<PreparedManagedEnv, String> {
        let port = (self.get_active_port)().unwrap_or(0);
        if port == 0 {
            return Err(
                "OMPChamber listener port is unavailable for managed tool injection".to_string(),
            );
        }
        if includes.none() {
            return Err(
                "At least one OMPChamber managed tool must be enabled to inject the plugin"
                    .to_string(),
            );
        }
        tokio::fs::create_dir_all(&self.plugin_directory)
            .await
            .map_err(|error| error.to_string())?;
        let source = plugin::create_plugin_source(&plugin::tool_specs(
            includes.control,
            includes.web,
            includes.memory,
        ));
        write_plugin_file(&self.plugin_path, &source)
            .await
            .map_err(|error| error.to_string())?;
        let token = generate_token();
        *self
            .active_token
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(token.clone());
        let plugin_url = Url::from_file_path(&self.plugin_path)
            .map_err(|()| {
                format!(
                    "plugin path is not absolute: {}",
                    self.plugin_path.display()
                )
            })?
            .to_string();
        let raw_config = (self.existing_config_content)();
        let merged = plugin::merge_plugin_config(raw_config.as_deref(), &plugin_url)?;
        Ok(PreparedManagedEnv {
            opencode_config_content: merged,
            agent_tool_url: format!("http://127.0.0.1:{port}{ROUTE_PATH}"),
            agent_tool_token: token,
        })
    }

    /// `execute(payload, options)` — allowlist resolution + dispatch.
    pub async fn execute(&self, payload: &AgentToolPayload, signal: AbortSignal) -> ToolResult {
        execute_with(self.execute_action.as_ref(), payload, signal).await
    }

    fn active_token(&self) -> Option<String> {
        self.active_token
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Adapter for the shared control service (`server/index.js` wires
/// `openChamberControlService.execute` into the agent-tool runtime the same
/// way). The landed service takes no abort signal; cancellation propagates
/// by dropping the in-flight future when the request goes away.
pub fn executor_for_control_service(
    service: Arc<crate::openchamber_control::service::ControlService>,
) -> ExecuteActionFn {
    Arc::new(move |action, input, context_directory, _signal| {
        let service = Arc::clone(&service);
        let action = action.to_string();
        Box::pin(async move {
            let context = context_directory.as_ref().and_then(Value::as_str);
            service
                .execute(&action, &input, context)
                .await
                .map_err(|error| ActionExecutionError {
                    message: error.message,
                    status_code: Some(i64::from(error.status)),
                    partial: error.partial,
                    partial_action: error.partial_action.map(Value::String),
                    session_id: error.session_id.map(Value::String),
                    directory: error.directory.map(Value::String),
                })
        })
    })
}

#[derive(Clone)]
struct ModuleState {
    runtime: Arc<AgentToolRuntime>,
}

/// `express.json({ limit: '1mb' })` parity: only `application/json` (or
/// `+json`) bodies are parsed; anything else leaves `req.body = {}`.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

fn parse_payload(headers: &HeaderMap, body: &[u8]) -> Result<AgentToolPayload, String> {
    if !is_json_content_type(headers) || body.is_empty() {
        return Ok(AgentToolPayload::default());
    }
    serde_json::from_slice::<AgentToolPayload>(body).map_err(|error| error.to_string())
}

async fn agent_tool_route(State(state): State<ModuleState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let remote_address = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let headers = parts.headers;
    // express.json({ limit: '1mb' }): oversized bodies are rejected before
    // anything else runs.
    let body = match axum::body::to_bytes(body, BODY_LIMIT_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "request entity too large" })),
            )
                .into_response();
        }
    };
    let payload = match parse_payload(&headers, &body) {
        Ok(payload) => payload,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
        }
    };
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let active_token = state.runtime.active_token();
    if !authorize(
        active_token.as_deref(),
        remote_address.as_deref(),
        authorization.as_deref(),
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }
    let mut abort = AbortHandle::new();
    let signal = abort.signal();
    let result = state.runtime.execute(&payload, signal).await;
    abort.finish();
    (StatusCode::OK, Json(result)).into_response()
}

/// Module router with a default runtime. Constructing the control service
/// needs the scheduled-task runtime, which `RouterContext` does not carry, so
/// the default runtime has no executor: until main composes one
/// (`ControlService` + [`executor_for_control_service`] +
/// [`router_with_runtime`]), valid actions answer the runtime-unavailable
/// envelope — the JS parity for an `executeAction` dependency that is absent.
pub fn router(ctx: RouterContext) -> Router {
    router_with_runtime(Arc::new(AgentToolRuntime::for_context(&ctx, None)))
}

pub fn router_with_runtime(runtime: Arc<AgentToolRuntime>) -> Router {
    Router::new()
        .route(ROUTE_PATH, post(agent_tool_route))
        .with_state(ModuleState { runtime })
}
