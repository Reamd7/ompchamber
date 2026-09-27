//! Port of `packages/web/server/lib/linear/` — Linear OAuth (PKCE + public
//! callback broker), token storage, GraphQL issue access, team-to-project
//! mapping, session status comments, and the `/linear` + `/api/linear/*`
//! routes. See `DOCUMENTATION.md` in the JS module for the product contract.
//! 本模块是 `packages/web/server/lib/linear/` 的 Rust 移植：Linear OAuth
//! （PKCE + 公共 callback broker）、token 存储、GraphQL issue 访问、
//! team 到 project 的映射、会话状态评论，以及 `/linear` 与
//! `/api/linear/*` 路由。产品契约见 JS 模块内的 DOCUMENTATION.md。

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::context::RouterContext;

/// Linear 授权存储子模块：授权条目（token/workspace）读写与 data dir 解析。
pub mod auth;
/// GraphQL 客户端子模块：请求封装、viewer/organization 身份查询与 token 刷新。
pub mod client;
/// 出站 HTTP 传输子模块：可注入的 transport 抽象（生产 reqwest，测试 fake）。
pub mod http;
/// Linear issue/team 的读取与 JSON 解析子模块。
pub mod issues;
/// team 到 project 路径映射的持久化子模块。
pub mod mapping;
/// OAuth 授权流程子模块：PKCE 授权、pending 状态与公共 callback broker 轮询。
pub mod oauth;
/// 复刻 JS 隐式转换语义的 JSON 值解析辅助子模块。
pub mod parse;
/// Linear 的 axum 路由子模块（/linear/* 与 /api/linear/*）。
pub mod routes;
/// 会话状态评论子模块：completed/failure 评论的构造与发送。
pub mod status;
/// 事件 hub 消费者子模块：把 session 事件转换为 Linear 评论。
pub mod status_runtime;
/// 分页拉取 Linear team 列表的子模块。
pub mod teams;

/// 本模块的单元测试。
#[cfg(test)]
mod tests;

/// Linear 模块按数据目录共享的状态（对应 JS 的模块级单例）：每个解析出的
/// Linear data dir 全进程只有一份实例，路由与 hub 消费者因此看到同一份
/// pending 授权表和各 in-flight 去重表。
/// Shared per-data-dir state (JS module-level singletons). One instance per
/// resolved Linear data dir, process-wide, so the router and the hub
/// consumer see the same pending authorizations and in-flight dedupe maps.
pub struct LinearState {
    /// Linear 数据目录（授权文件与映射文件的存放位置）。
    data_dir: std::path::PathBuf,
    /// 仅测试使用的环境变量覆盖表（生产路径每次读进程环境，与 JS readEnv 一致）。
    /// Test-only env overrides (production reads the process environment on
    /// every call, exactly like the JS `readEnv`).
    env: Option<HashMap<String, String>>,
    /// 出站 HTTP 传输层：生产为 reqwest 实现，测试可注入 fake。
    transport: Arc<dyn http::HttpTransport>,
    /// 按 OAuth state 索引的 pending 授权表（JS 的 pendingByState）。
    /// JS `pendingByState`.
    pending: Mutex<HashMap<String, oauth::PendingAuthorization>>,
    /// 按 state 索引的 broker 轮询共享 future 表（JS 的 brokerPollsByState）。
    /// JS `brokerPollsByState`.
    broker_polls: Mutex<
        HashMap<String, SharedFuture<Result<Option<oauth::AuthorizationResult>, LinearError>>>,
    >,
    /// 按 workspace 索引的在途 token 刷新共享 future 表（JS 的 inFlightRefreshByWorkspace）。
    /// JS `inFlightRefreshByWorkspace`.
    refresh_inflight: Mutex<HashMap<String, SharedFuture<Result<Option<String>, LinearError>>>>,
    /// 按去重键索引的在途状态评论共享 future 表（status.js 的 inflight）。
    /// JS `inflight` in status.js.
    status_inflight: Mutex<HashMap<String, SharedFuture<Result<serde_json::Value, LinearError>>>>,
}

/// 可克隆的共享 future：把同一个在途结果发给所有并发调用方
///（对应 JS 多处共享同一个 promise 的模式）。
/// A cloneable shared future (the JS pattern of handing out the same
/// in-flight promise to concurrent callers).
pub type SharedFuture<T> = futures::future::Shared<Pin<Box<dyn Future<Output = T> + Send>>>;

/// 将一个 boxed future 转为 SharedFuture，作为在途去重（in-flight dedupe）的最小构建块。
pub fn shared<T: Clone>(future: Pin<Box<dyn Future<Output = T> + Send>>) -> SharedFuture<T> {
    futures::FutureExt::shared(future)
}

/// LinearState 的构造与环境变量读取。
impl LinearState {
    /// 以数据目录、可选的环境变量覆盖表和 HTTP 传输层构造空状态（各表初始为空）。
    pub fn new(
        data_dir: std::path::PathBuf,
        env: Option<HashMap<String, String>>,
        transport: Arc<dyn http::HttpTransport>,
    ) -> Self {
        Self {
            data_dir,
            env,
            transport,
            pending: Mutex::new(HashMap::new()),
            broker_polls: Mutex::new(HashMap::new()),
            refresh_inflight: Mutex::new(HashMap::new()),
            status_inflight: Mutex::new(HashMap::new()),
        }
    }

    /// 对应 JS readEnv：优先读测试覆盖表，未命中时回落到进程环境变量（返回值已 trim）。
    /// JS `readEnv` honoring the test overrides.
    fn env_value(&self, name: &str) -> String {
        match &self.env {
            Some(overrides) => overrides
                .get(name)
                .map(|v| v.trim().to_string())
                .unwrap_or_default(),
            None => env_value_raw(name),
        }
    }
}

/// 直接读进程环境变量并 trim；未设置或全空白时返回空串（JS readEnv 语义）。
/// JS `readEnv` against the process environment.
pub fn env_value_raw(name: &str) -> String {
    std::env::var(name)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// 当前 Unix 时间戳（毫秒，f64），对应 JS 的 Date.now()。
/// JS `Date.now()` in milliseconds.
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// 每个 data dir 返回同一份共享状态：注册表以 Weak 缓存，命中即复用；
/// 未命中时用引擎共享的 reqwest 客户端新建并登记。
/// One shared state per resolved data dir (the JS module state is global).
pub fn shared_state(ctx: &RouterContext) -> Arc<LinearState> {
    // 按 data dir 索引的 Weak 注册表：跨调用复用同一份 LinearState，
    // 无强引用时允许被回收。
    static REGISTRY: LazyLock<Mutex<HashMap<std::path::PathBuf, Weak<LinearState>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let data_dir = auth::resolve_data_dir();
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&data_dir).and_then(Weak::upgrade) {
        return existing;
    }
    let state = Arc::new(LinearState::new(
        data_dir.clone(),
        None,
        Arc::new(http::ReqwestTransport::new(ctx.engine.http().clone())),
    ));
    registry.insert(data_dir, Arc::downgrade(&state));
    state
}

/// 构建 Linear 模块路由：`/linear/*` 保持公开（供 Linear 重定向回跳），
/// `/api/linear/*` 由 main.rs 中共享的 `/api` UI 鉴权网关统一拦截，
/// 与 JS 侧的路由注册顺序一致。
/// The linear module router. `/linear/*` stays public (Linear's redirect);
/// `/api/linear/*` sits behind the shared `/api` UI-auth gate applied in
/// `main.rs`, exactly like the JS route registration order.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(shared_state(&ctx))
}

/// hub 消费者入口（JS index.js 的 createLinearSessionRuntime）；
/// 接入全局消息流的接线随 index.rs 的移植落地。
/// Hub consumer entry (JS `createLinearSessionRuntime` in `index.js`).
/// Wiring into the global message stream lands with the index.rs port.
pub fn session_status_runtime(ctx: RouterContext) -> status_runtime::LinearSessionStatusRuntime {
    status_runtime::LinearSessionStatusRuntime::new(shared_state(&ctx))
}

/// Linear 模块唯一的错误类型：携带 JS 错误的 code、数值 status、
/// userError 标记与 OAuth origin。
/// The single error type for the linear module: carries the JS error's
/// `code`, numeric `status`, `userError` flag, and OAuth `origin`.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearError {
    /// 人类可读的错误消息（对应 JS 的 error.message）。
    pub message: String,
    /// 机器可读错误码（如 INVALID、MALFORMED、LINEAR_OAUTH_FAILED）。
    pub code: Option<String>,
    /// 数值 HTTP 状态码（对应 JS 的 error.status）；无则为 None。
    pub status: Option<u16>,
    /// 是否为用户输入引发的错误（API 层据此映射 4xx 而非 5xx）。
    pub user_error: bool,
    /// OAuth 失败的来源标记（区分发起端）。
    pub origin: Option<oauth::AuthOrigin>,
}

/// 各 JS 错误形态对应的构造器与状态读取。
impl LinearError {
    /// 普通 JS Error 形态：无 code、无 status。
    /// A plain JS `Error` (no code, no status).
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            status: None,
            user_error: false,
            origin: None,
        }
    }

    /// LinearOAuthError 形态：默认 code 为 LINEAR_OAUTH_FAILED。
    /// A `LinearOAuthError` with its default `LINEAR_OAUTH_FAILED` code.
    pub fn oauth(message: impl Into<String>, code: &str) -> Self {
        Self {
            message: message.into(),
            code: Some(code.to_string()),
            status: None,
            user_error: false,
            origin: None,
        }
    }

    /// 在 oauth 形态基础上附带数值 HTTP status。
    pub fn oauth_with_status(message: impl Into<String>, code: &str, status: Option<u16>) -> Self {
        Self {
            status,
            ..Self::oauth(message, code)
        }
    }

    /// 在 oauth 形态基础上附带 OAuth origin。
    pub fn oauth_with_origin(
        message: impl Into<String>,
        code: &str,
        origin: Option<oauth::AuthOrigin>,
    ) -> Self {
        Self {
            origin,
            ..Self::oauth(message, code)
        }
    }

    /// LinearApiError 形态：携带 HTTP status。
    /// A `LinearApiError` with its HTTP status.
    pub fn api(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            code: None,
            status: Some(status),
            user_error: false,
            origin: None,
        }
    }

    /// api 形态并显式指定 userError 标记。
    pub fn api_with_user_flag(message: impl Into<String>, status: u16, user_error: bool) -> Self {
        Self {
            user_error,
            ..Self::api(message, status)
        }
    }

    /// error.code = 'INVALID'：面向用户的参数校验失败。
    /// `error.code = 'INVALID'` (user-facing validation failure).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: Some("INVALID".to_string()),
            ..Self::plain(message)
        }
    }

    /// LinearMappingError/LinearSessionStatusError 形态：code 为 MALFORMED。
    /// `LinearMappingError`/`LinearSessionStatusError` with `MALFORMED`.
    pub fn mapping(message: impl Into<String>) -> Self {
        Self {
            code: Some("MALFORMED".to_string()),
            ..Self::plain(message)
        }
    }

    /// 会话数据解析失败（MALFORMED）的便捷构造器，等价于 mapping。
    pub fn session_malformed(message: impl Into<String>) -> Self {
        Self::mapping(message)
    }

    /// 读取数值 HTTP 状态码（对应 JS 的 error.status）。
    /// Numeric HTTP status like JS `error.status`.
    pub fn http_status(&self) -> Option<u16> {
        self.status
    }
}

/// Display 直接输出 message，与 JS 错误的字符串形态一致。
impl std::fmt::Display for LinearError {
    /// 输出 message 本身，与 JS 错误的字符串形态一致。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 实现 std::error::Error，使其可作为通用错误类型传播。
impl std::error::Error for LinearError {}

/// 对应 JS writeJsonFile：先写同目录临时文件并设 0600 权限，再原子
/// rename 覆盖目标文件；父目录缺失时自动创建，rename 后再次确认 0600。
/// JS `writeJsonFile`: atomic tmp-file write with 0600 permissions.
pub fn write_file_atomic_600(path: &Path, body: &str) -> std::io::Result<()> {
use crate::os_compat::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_file = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id(),
        now_ms() as u64
    ));
    std::fs::write(&tmp_file, body)?;
    let _ = std::fs::set_permissions(&tmp_file, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp_file, path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// 按 JS Object.keys 的插入顺序提取 JSON 文档的顶层键序列；
/// session-status 去重文件“保留最新 500 条”的裁剪依赖这个顺序。
/// Extract the top-level key order of a JSON object document the way JS
/// `Object.keys` reports insertion order. Used by the session-status dedupe
/// file, whose "keep the newest 500" pruning depends on it.
pub fn top_level_key_order(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = text.as_bytes();
    let mut position = 0usize;
    let mut depth = 0usize;
    let mut expect_key = false;
    while position < bytes.len() {
        match bytes[position] {
            b'{' => {
                depth += 1;
                expect_key = depth == 1;
                position += 1;
            }
            b'[' => {
                depth += 1;
                expect_key = false;
                position += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                expect_key = false;
                position += 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                position += 1;
            }
            b'"' if depth == 1 && expect_key => {
                let (key, next) = read_json_string(bytes, position);
                let mut scan = next;
                while scan < bytes.len() && (bytes[scan] as char).is_ascii_whitespace() {
                    scan += 1;
                }
                if scan < bytes.len() && bytes[scan] == b':' {
                    keys.push(key);
                }
                position = scan;
                expect_key = false;
            }
            b'"' => {
                let (_, next) = read_json_string(bytes, position);
                position = next;
            }
            _ => position += 1,
        }
    }
    keys
}

/// 从 position（指向开引号）处读取一个 JSON 字符串，返回解码后的内容
/// 与闭引号之后第一个字节的索引。
/// Read a JSON string starting at `position` (the opening quote); returns the
/// decoded contents and the index just past the closing quote.
fn read_json_string(bytes: &[u8], position: usize) -> (String, usize) {
    let mut out = String::new();
    let mut index = position + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() => {
                match bytes[index + 1] {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = bytes
                            .get(index + 2..index + 6)
                            .and_then(|slice| std::str::from_utf8(slice).ok())
                            .and_then(|slice| u32::from_str_radix(slice, 16).ok());
                        if let Some(code) = hex
                            && let Some(character) = char::from_u32(code)
                        {
                            out.push(character);
                        }
                        index += 4;
                    }
                    other => out.push(other as char),
                }
                index += 2;
            }
            b'"' => return (out, index + 1),
            _ => {
                let start = index;
                while index < bytes.len() && bytes[index] != b'"' && bytes[index] != b'\\' {
                    index += 1;
                }
                out.push_str(&String::from_utf8_lossy(&bytes[start..index]));
            }
        }
    }
    (out, index)
}
