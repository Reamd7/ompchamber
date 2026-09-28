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
//!
//! 中文说明：本模块是 `server/lib/agent-tool/runtime.js` 的 Rust 移植，把 OpenChamber
//! 以类型化 OpenCode 自定义工具的形式暴露给 agent。两条主线：一是
//! [`AgentToolRuntime::prepare_managed_opencode_env`] 物化生成的 JS 插件（见 [`plugin`]）、
//! 把其 `file://` URL 不覆盖既有条目地并入 `OPENCODE_CONFIG_CONTENT`，并轮换出全新的
//! per-child token 与 loopback 回调 URL；二是 `ROUTE_PATH` 上的 POST 回调，仅接受
//! loopback 请求且要求当前 per-child bearer token（常量时间比较），随后把固定 action
//! 白名单分发给共享的 OpenChamber 控制服务。

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

/// 插件生成子模块：物化到磁盘的 JS 插件源码与 `OPENCODE_CONFIG_CONTENT` 的合并逻辑
///（`server/lib/agent-tool/runtime.js` 生成侧的移植）。
mod plugin;

/// 单元测试：覆盖鉴权、action 白名单分发、token 轮换与插件环境准备等行为契约。
#[cfg(test)]
mod tests;

/// The callback route every generated tool posts to.
/// 生成的每个托管工具都向该回调路由 POST；也是拼进子进程环境变量的回调 URL 后缀。
pub const ROUTE_PATH: &str = "/api/ompchamber/agent-tool";

/// `express.json({ limit: '1mb' })` parity.
/// 请求体大小上限，与 JS 端 `express.json({ limit: '1mb' })` 一致；超限直接 413。
const BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// The wire envelope version (`TOOL_SCHEMA_VERSION`).
/// 回调信封的线上协议版本号；成功与失败响应均携带，供插件端做兼容判断。
const TOOL_SCHEMA_VERSION: u32 = 1;

/// 端口提供者：返回服务器当前实际监听端口（`None` 表示不可用），用于拼接回调 URL。
pub type PortProvider = Arc<dyn Fn() -> Option<u16> + Send + Sync>;
/// 配置内容提供者：读取既有 `OPENCODE_CONFIG_CONTENT`（通常来自进程环境变量），
/// 合并插件 URL 时保持原有内容不丢失。
pub type ConfigContentProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Mirror of the JS `executeAction(action, input, contextDirectory, options)`
/// dependency; `server/index.js` wires the shared OpenChamber control service
/// here. `context_directory` is forwarded exactly as sent (a string or
/// absent), and the signal is the request-abort signal from `options`.
/// 中文：JS 侧 `executeAction` 依赖的镜像类型，`server/index.js` 在此接入共享控制服务；
/// `context_directory` 原样转发（字符串或缺省），信号取自请求的 abort 信号。
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
/// 中文：控制服务抛错时 `execute` 的 catch 分支读取的字段集合——`message`、
/// `statusCode`、`partial` 及呈现给 agent 的部分失败元数据。
#[derive(Debug, Clone, Default)]
pub struct ActionExecutionError {
    /// 人类可读的错误消息，原样进入 `ToolError::message`。
    pub message: String,
    /// 控制服务报告的 HTTP 风格状态码；4xx 映射为 `usage` 错误，其余映射为 `runtime`。
    pub status_code: Option<i64>,
    /// 是否为部分成功（例如会话已创建但后续步骤失败）。
    pub partial: bool,
    /// 部分失败时已完成的动作名。
    pub partial_action: Option<Value>,
    /// 部分失败时关联的会话 id。
    pub session_id: Option<Value>,
    /// 部分失败时关联的目录。
    pub directory: Option<Value>,
}

/// Request-cancellation signal handed to the control service (JS:
/// `AbortController` aborted from the request's `aborted`/`close` events).
///
/// Dropping the [`AbortHandle`] aborts unless the response completed
/// (`res.writableEnded` parity): in Rust the handler future itself is dropped
/// on client disconnect, so the drop guard is what fires the abort.
/// 中文：基于 tokio watch channel 的请求取消信号；除非已调用 `finish`，丢弃
/// [`AbortHandle`] 即触发 abort。Rust 中客户端断开会直接 drop handler future，
/// 因此 drop 守卫就是 abort 的触发点。
#[derive(Clone)]
pub struct AbortSignal {
    /// watch channel 接收端；`true` 表示已请求取消。
    receiver: tokio::sync::watch::Receiver<bool>,
}

/// 取消信号的查询与等待。
impl AbortSignal {
    /// 非阻塞快照：当前是否已请求取消。
    pub fn is_aborted(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolves once the signal is (or becomes) aborted, or the sender is gone.
    /// 中文：异步等待，直到信号已取消或发送端消失时返回。
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

/// 请求取消的控制端：持有 watch channel 发送端；drop 时若未 `finish` 则自动 abort，
/// 对应 JS 侧响应写出完毕（`writableEnded`）之前的连接中断。
pub struct AbortHandle {
    /// watch channel 发送端；`send(true)` 即触发取消。
    sender: tokio::sync::watch::Sender<bool>,
    /// 响应是否已完整写出；完成后 drop 不再触发取消。
    finished: bool,
}

/// 构造、派生信号与触发取消。
impl AbortHandle {
    /// 创建未取消的句柄及配套 watch channel。
    pub fn new() -> Self {
        let (sender, _) = tokio::sync::watch::channel(false);
        Self {
            sender,
            finished: false,
        }
    }

    /// 订阅一个 [`AbortSignal`]，与该句柄共享同一取消状态。
    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            receiver: self.sender.subscribe(),
        }
    }

    /// 主动触发取消；重复调用无副作用。
    pub fn abort(&self) {
        let _ = self.sender.send(true);
    }

    /// Marks the response as complete so dropping the guard no longer aborts.
    /// 中文：此后 drop 守卫不再触发取消。
    pub fn finish(&mut self) {
        self.finished = true;
    }
}

/// `Default` 委托到 [`AbortHandle::new`]。
impl Default for AbortHandle {
    /// 等价于 [`AbortHandle::new`]：创建未取消的句柄。
    fn default() -> Self {
        Self::new()
    }
}

/// drop 守卫：响应未完成时触发 abort，把客户端断开传播给控制服务。
impl Drop for AbortHandle {
    /// 响应未完成时触发 abort，模拟 JS 侧请求中断的传播。
    fn drop(&mut self) {
        if !self.finished {
            self.abort();
        }
    }
}

/// Which managed tools to include in the materialized plugin
/// (`prepareManagedOpenCodeEnv`'s include flags; all default to on).
/// 中文：三个布尔开关对应 `prepareManagedOpenCodeEnv` 的 include flags，默认全开。
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolIncludes {
    /// 是否包含 `ompchamber` 控制工具。
    pub control: bool,
    /// 是否包含 `ompchamber_web` 浏览器工具。
    pub web: bool,
    /// 是否包含 `ompchamber_memory` 记忆工具。
    pub memory: bool,
}

/// 常量构造与整体判断。
impl ToolIncludes {
    /// 三个工具全部启用。
    pub fn all() -> Self {
        Self {
            control: true,
            web: true,
            memory: true,
        }
    }

    /// 是否全部禁用（此时环境准备会直接报错）。
    pub fn none(&self) -> bool {
        !self.control && !self.web && !self.memory
    }
}

/// The environment additions `prepareManagedOpenCodeEnv` returns — spread into
/// the managed OpenCode child environment only.
/// 中文：仅展开进受管 OpenCode 子进程的环境增量，不影响服务器自身环境。
#[derive(Debug, Clone)]
pub struct PreparedManagedEnv {
    /// 合并了插件 URL 的新 `OPENCODE_CONFIG_CONTENT`（紧凑 JSON）。
    pub opencode_config_content: String,
    /// 物化插件的 `file://` URL。
    pub agent_tool_url: String,
    /// 新轮换的 per-child bearer token，仅该子进程持有。
    pub agent_tool_token: String,
}

/// The plugin callback payload: the merged tool input, the authoritative
/// session directory, and the calling tool's name.
/// 中文：插件回调请求体——合并后的工具输入、权威会话目录与调用方工具名。
#[derive(Debug, Default, Deserialize)]
pub struct AgentToolPayload {
    /// 工具入参对象，应含 `action` 字段；缺失按空对象处理。
    #[serde(default)]
    pub input: Option<Value>,
    /// 调用工具所在会话的权威目录（字符串或缺省，原样转发给控制服务）。
    #[serde(default, rename = "contextDirectory")]
    pub context_directory: Option<Value>,
    /// 发起调用的工具名（如 `ompchamber_web`），用于 action 命名空间解析。
    #[serde(default)]
    pub tool: Option<Value>,
}

/// `{schemaVersion, ok, action, data?, error?}` — the result contract every
/// completed call answers with (serde keeps the declared key order).
/// 中文：serde 按声明顺序输出字段，与 JS 侧的结果契约一致。
#[derive(Debug, Serialize)]
pub struct ToolResult {
    /// 线上协议版本（`TOOL_SCHEMA_VERSION`）。
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// 动作是否成功。
    pub ok: bool,
    /// 已解析并分发的动作名；解析失败时回退为请求原值或 `"unknown"`。
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 成功数据或部分失败元数据；无内容时缺省。
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 失败详情；成功时缺省。
    pub error: Option<ToolError>,
}

/// 失败负载：`message` 给出可直接呈现的原因，`kind` 区分 `usage`（调用方错误）
/// 与 `runtime`（环境或服务错误）。
#[derive(Debug, Serialize)]
pub struct ToolError {
    /// 人类可读的错误消息。
    pub message: String,
    /// 错误种类：`usage` 或 `runtime`。
    pub kind: String,
}

/// 失败结果的构造助手。
impl ToolResult {
    /// 统一失败构造：填入协议版本、action 与错误种类。
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

    /// 构造 `usage` 类错误（action 名缺失或入参不合法）。
    fn usage_error(action: Option<&str>, message: impl Into<String>) -> Self {
        Self::with_error(action, message.into(), "usage")
    }

    /// 构造 `runtime` 类错误（控制服务不可用等环境问题）。
    fn runtime_error(action: Option<&str>, message: impl Into<String>) -> Self {
        Self::with_error(action, message.into(), "runtime")
    }
}

/// `asNonEmptyString` core: a string with non-empty trimmed content.
/// 中文：trim 后非空才返回 `Some`。
fn as_non_empty_str(value: Option<&str>) -> Option<&str> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// `asNonEmptyString` for JSON values.
/// 中文：对 JSON 值取非空字符串；非字符串或全空白返回 `None`。
fn as_non_empty_string(value: Option<&Value>) -> Option<String> {
    as_non_empty_str(value?.as_str()).map(str::to_string)
}

/// `ACTIONS`: everything either managed tool may ask for — the agent allowlist
/// stays narrower than the full control surface (no `schedule.status`).
/// 中文：懒加载合并控制、web、memory 三组 action；刻意不含 `schedule.status`，
/// agent 可请求面窄于完整控制面。
fn dispatchable_actions() -> &'static Vec<&'static str> {
    /// 懒加载合并三组 action 的静态白名单，仅初始化一次。
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
/// 中文：完整执行流程——结合调用工具的命名空间解析 action、校验白名单、组装输入并
/// 调用控制服务；成功与失败（含 partial 元数据）都归一为 [`ToolResult`]。
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
/// 中文：接受 Node 对 loopback 对端的三种写法：`127.0.0.1`、`::1`、`::ffff:127.0.0.1`。
fn is_loopback_address(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let address = value.to_ascii_lowercase();
    address == "127.0.0.1" || address == "::1" || address == "::ffff:127.0.0.1"
}

/// Constant-time byte comparison (the `crypto.timingSafeEqual` half; the
/// length check mirrors the JS precondition).
/// 中文：逐字节异或累积后比较，避免时序侧信道；长度不等先短路。
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
/// 中文：三项全过才放行——存在活动 token、对端为 loopback、Bearer token 常量时间相等。
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
/// 中文：仅存在于内存与子进程环境变量中，绝不写日志或持久化。
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 把生成的插件源码写入目标路径；Unix 下以 0o600 权限创建并显式 flush，避免同进程
/// 读者在后台刷新前观察到空文件。
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
/// 中文：`createAgentToolRuntime` 的移植；clone 共享同一活动 token（对应 JS 闭包中
/// 单一可变的 `activeToken`）。
#[derive(Clone)]
pub struct AgentToolRuntime {
    /// 插件物化目录（`<data-dir>/agent-tool/`）。
    plugin_directory: PathBuf,
    /// 插件文件完整路径（`ompchamber-plugin.js`）。
    plugin_path: PathBuf,
    /// 当前监听端口提供者。
    get_active_port: PortProvider,
    /// 控制服务执行器；未接线时有效 action 返回 runtime-unavailable 信封。
    execute_action: Option<ExecuteActionFn>,
    /// 既有 `OPENCODE_CONFIG_CONTENT` 提供者。
    existing_config_content: ConfigContentProvider,
    /// 当前 per-child token；跨 clone 共享，每次环境准备轮换。
    active_token: Arc<RwLock<Option<String>>>,
}

/// 环境准备、请求执行与 token 管理。
impl AgentToolRuntime {
    /// 以数据目录与注入依赖构造运行时；插件目录约定为 `<data-dir>/agent-tool/`。
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
    /// 中文：端口取配置值（接线完成后 main 应改传实际绑定端口），配置内容取进程环境变量。
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
    /// 中文：端口不可用或所有工具被禁用时报错；成功时写插件、轮换 token、合并配置，
    /// 返回仅适用于该子进程的环境增量。
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
    /// 中文：经 [`execute_with`] 完成 action 解析与分发。
    pub async fn execute(&self, payload: &AgentToolPayload, signal: AbortSignal) -> ToolResult {
        execute_with(self.execute_action.as_ref(), payload, signal).await
    }

    /// 读取当前活动 token 的克隆；锁中毒时恢复内部值继续。
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
/// 中文：把共享 [`ControlService`] 适配为 [`ExecuteActionFn`]；服务本身不接收 abort
/// 信号，取消通过请求离开时 drop 进行中的 future 传播。
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

/// 路由级状态：持有共享的 [`AgentToolRuntime`]。
#[derive(Clone)]
struct ModuleState {
    /// 每次 clone 共享的运行时实例。
    runtime: Arc<AgentToolRuntime>,
}

/// `express.json({ limit: '1mb' })` parity: only `application/json` (or
/// `+json`) bodies are parsed; anything else leaves `req.body = {}`.
/// 中文：仅 `application/json` 与 `application/*+json` 视为 JSON，其余类型不解析。
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

/// 按 Express `express.json` 语义解析请求体：非 JSON 类型或空体返回默认 payload，
/// 声称是 JSON 却解析失败则返回错误消息（路由层转 400）。
fn parse_payload(headers: &HeaderMap, body: &[u8]) -> Result<AgentToolPayload, String> {
    if !is_json_content_type(headers) || body.is_empty() {
        return Ok(AgentToolPayload::default());
    }
    serde_json::from_slice::<AgentToolPayload>(body).map_err(|error| error.to_string())
}

/// 回调路由主处理器：读取对端地址 → 限额读取请求体（超限 413）→ 解析 payload →
/// 鉴权（失败 401）→ 执行并返回 `ToolResult`；abort 守卫在响应完成后释放。
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
/// 中文：默认运行时没有执行器，直到 main 完成组合；与 JS 侧缺 `executeAction`
/// 依赖时的行为对齐。
pub fn router(ctx: RouterContext) -> Router {
    router_with_runtime(Arc::new(AgentToolRuntime::for_context(&ctx, None)))
}

/// 以指定运行时构建模块路由：在 `ROUTE_PATH` 上注册 POST 处理器并注入模块状态。
pub fn router_with_runtime(runtime: Arc<AgentToolRuntime>) -> Router {
    Router::new()
        .route(ROUTE_PATH, post(agent_tool_route))
        .with_state(ModuleState { runtime })
}
