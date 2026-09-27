//! Port of the `server/lib/terminal/` module's HTTP + WebSocket surface
//! (`runtime.js` route registrations and the `/api/terminal/ws` transport).
//!
//! Ownership follows `terminal/DOCUMENTATION.md`: this module owns terminal
//! identity, PTY processes, status, ordered output, bounded scrollback,
//! WebSocket attachments, and lifecycle routes. The WS path stays in the
//! ui-auth URL-token set and the relay allow-list; the gate layer below is the
//! same `ui_auth::middleware` the proxy/fs/event-stream modules apply, and the
//! upgrade handler mirrors the JS `upgradeHandler` origin check (only while a
//! UI password is configured — no origin shortcuts added or skipped).

//!
//! 中文说明：本模块移植 JS 版 `server/lib/terminal/` 的 HTTP 与 WebSocket
//! 接口（runtime.js 的路由注册与 `/api/terminal/ws` 传输层）。本文件负责
//! 路由装配、请求体/查询参数的 express 语义提取，以及各生命周期端点的
//! 处理器；会话状态机与 socket 循环在 `runtime` 子模块，PTY 抽象在
//! `pty` 子模块。鉴权沿用与 proxy/fs/event-stream 相同的
//! `ui_auth::middleware` 门层，WS 升级仅在配置了 UI 密码时做 origin 校验。

/// 终端网格解析（GridCore 移植）：把 PTY 字节流解析为 full/rows/cursor 差分帧。
pub mod grid;
/// 有界滚动历史与重放缓冲：剥离渲染器无法应答的查询序列，供快照恢复与重连对账。
pub mod history;
/// WS 二进制控制帧协议：单 tag 字节 + UTF-8 JSON 文档的编解码与常量。
pub mod protocol;
/// PTY 依赖注入缝：portable-pty 真实后端、测试 fake，以及真实文件系统/PATH 依赖。
pub mod pty;
/// 会话运行时核心：身份、状态机、会话泵、流控、视口协商与 WS socket 循环。
pub mod runtime;
/// OSC 133 shell 集成：注入的包装脚本发出命令边界标记，驱动 command-finished 事件。
pub mod shell_integration;
/// shell 家族发现与持久化 shell id 解析（createTerminalShellResolver 移植）。
pub mod shells;
/// 主题/能力应答：PTY 直接回应 DA1、OSC 10/11 等查询，启动握手不依赖渲染器。
pub mod theme;

/// 运行时集成测试：Router::oneshot 驱动 HTTP 路由 + 真实回环 WS 传输的最小客户端。
#[cfg(test)]
mod testing;

use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, RawQuery, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;
use pty::RealPtyProvider;
use runtime::{CreateSessionRequest, JsField, SessionStatus, TerminalOptions, TerminalState};
use shells::{Platform, ShellDeps};

/// `MAX_INPUT_CHARS` (runtime.js): input cap for one `write` frame.
/// 中文补充：单个 `write` 帧输入的字符上限，超出即拒绝，防止滥用 WS 写入。
pub const MAX_INPUT_CHARS: usize = 65_536;
/// `express.json()` default body limit.
/// 中文补充：JSON 请求体上限 100 KiB，对齐 express.json() 的默认限制。
const JSON_BODY_LIMIT_BYTES: usize = 100 * 1024;
/// Module state: the terminal runtime plus whether UI auth is configured
/// (JS `uiAuthController?.enabled` — governs the upgrade origin check).
/// 中文补充：运行时状态 + 鉴权开关的二元组，本模块所有处理器的共享 state。
type ModuleState = (Arc<TerminalState>, bool);

/// 组装生产路由：创建 TerminalState（真实 PTY provider 与真实 shell 依赖），
/// 并按 JS 的注册顺序把共享 ui_auth 门层叠加在本模块路由表之上。
pub fn router(ctx: RouterContext) -> axum::Router {
    // The JS auth gate applies to every `/api` route registered after
    // `registerAuthAndAccessRoutes`; layering the shared gate over this
    // module's routes mirrors that registration order.
    let auth_enabled = ctx
        .config
        .ui_password
        .as_deref()
        .map(str::trim)
        .is_some_and(|password| !password.is_empty());
    let state = TerminalState::new(
        TerminalOptions::default(),
        Arc::new(RealPtyProvider::new()),
        real_shell_deps(),
    );
    routes()
        .layer(crate::ui_auth::middleware(ctx))
        .with_state((state, auth_enabled))
}

/// Route table shared by the production router (gated) and the test harness
/// (auth disabled, fake PTY provider).
/// 中文补充：生产（带门层）与测试装配（无鉴权 + fake PTY）共用的路由表本体。
fn routes() -> Router<ModuleState> {
    Router::new()
        .route("/api/terminal/ws", get(terminal_ws))
        .route("/api/terminal/shells", get(list_shells))
        .route("/api/terminal/sessions", get(list_sessions))
        .route("/api/terminal/touch", post(touch_sessions))
        .route("/api/terminal/create", post(create_terminal))
        .route("/api/terminal/force-kill", post(force_kill))
        .route("/api/terminal/{sessionId}/resize", post(resize_terminal))
        .route(
            "/api/terminal/{sessionId}/appearance",
            post(update_appearance),
        )
        .route("/api/terminal/{sessionId}/restart", post(restart_terminal))
        .route("/api/terminal/{sessionId}", delete(close_terminal))
}

/// 测试装配：复用同一张路由表，但关闭鉴权，state 由调用方注入（通常挂 fake PTY）。
#[cfg(test)]
pub(crate) fn test_router(state: Arc<TerminalState>) -> Router {
    routes().with_state((state, false))
}
/// Real shell-discovery deps (env-runtime.js subset; the login-shell PATH
/// augmentation lands with that port — PATH passes through unchanged).
/// 中文补充：环境查询用 std::env（空值视为缺省），PATH 与文件系统检测走 pty 模块的真实实现。
fn real_shell_deps() -> ShellDeps {
    ShellDeps {
        platform: Platform::current(),
        env: Box::new(|key: &str| std::env::var(key).ok().filter(|value| !value.is_empty())),
        build_augmented_path: Box::new(pty::real_build_augmented_path),
        search_path_for: Box::new(pty::real_search_path_for),
        is_executable: Box::new(pty::real_is_executable),
        read_etc_shells: Box::new(pty::real_read_etc_shells),
    }
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

/// `GET /api/terminal/ws` 升级处理器：仅在配置了 UI 鉴权时做 origin 校验
/// （对齐 JS upgradeHandler；401 token 检查已在门层完成），随后限制单帧
/// 上限并把 socket 移交 runtime::run_socket。
async fn terminal_ws(
    State(state): State<ModuleState>,
    ws: WebSocketUpgrade,
    parts: Parts,
) -> Response {
    let (terminal, auth_enabled) = state;
    // JS `upgradeHandler`: only while `uiAuthController.enabled` — the 401
    // session-token check already ran in the gate layer for the upgrade
    // request; the origin check is this module's own responsibility.
    // (Non-upgrade GETs answer axum's upgrade-rejection status; the JS had no
    // express route on this path at all.)
    if auth_enabled && !crate::ui_auth::is_request_origin_allowed(&parts) {
        return crate::ui_auth::reject_websocket_upgrade(403, "Invalid origin");
    }
    ws.max_message_size(protocol::TERMINAL_WS_MAX_PAYLOAD_BYTES)
        .on_upgrade(move |socket| async move {
            runtime::run_socket(Arc::clone(&terminal), socket).await
        })
}

// ---------------------------------------------------------------------------
// HTTP command plane
// ---------------------------------------------------------------------------

/// `GET /api/terminal/shells` — available shell ids on the active server.
/// 中文补充：返回当前服务器可用 shell 的 id、展示名与是否支持登录模式。
async fn list_shells(State(state): State<ModuleState>) -> Response {
    let (terminal, _) = state;
    let shells: Vec<Value> = terminal
        .shell_resolver
        .list()
        .into_iter()
        .map(|shell| {
            json!({ "id": shell.id, "name": shell.name, "supportsLogin": shell.supports_login })
        })
        .collect();
    Json(shells).into_response()
}

/// `GET /api/terminal/sessions?cwd=` — live sessions, optionally filtered by
/// resolved cwd so clients can adopt terminals from other devices.
/// 中文补充：cwd 过滤按解析后的路径比较，客户端借此认领同一仓库的终端。
async fn list_sessions(State(state): State<ModuleState>, RawQuery(raw): RawQuery) -> Response {
    let (terminal, _) = state;
    let cwd_filter = single_query_value(raw.as_deref(), "cwd")
        .filter(|cwd| !cwd.trim().is_empty())
        .map(|cwd| runtime::resolve_path(cwd.trim()));
    let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = Vec::new();
    for session in sessions.values() {
        let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(filter) = &cwd_filter
            && runtime::resolve_path(&inner.cwd) != *filter
        {
            continue;
        }
        list.push(json!({
            "sessionId": session.id,
            "cwd": inner.cwd,
            "status": inner.status.as_str(),
            "createdAt": inner.created_at,
        }));
    }
    Json(json!({ "sessions": list })).into_response()
}

/// `POST /api/terminal/touch` — refresh `lastActivity` for listed ids; a
/// `claimant` records a per-window claim that conditional deletes release.
/// 中文补充：claimant 长度限制 128 并 trim；非字符串 id 与未知 id 静默跳过。
async fn touch_sessions(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let raw_ids = body
        .0
        .get("sessionIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let claimant = body
        .0
        .get("claimant")
        .and_then(Value::as_str)
        .filter(|claimant| !claimant.trim().is_empty() && claimant.len() <= 128)
        .map(|claimant| claimant.trim().to_string());
    let now = runtime::now_ms();
    let mut touched = 0u64;
    let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
    for id in raw_ids {
        let Value::String(id) = id else { continue };
        let Some(session) = sessions.get(&id) else {
            continue;
        };
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last_activity = now;
        if let Some(claimant) = &claimant {
            inner.claims.insert(claimant.clone(), now);
        }
        touched += 1;
    }
    Json(json!({ "touched": touched })).into_response()
}

/// `POST /api/terminal/create`.
/// 中文补充：成功回显会话 id/尺寸/状态；会话数达上限映射 429，其余错误映射 400。
async fn create_terminal(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let request = parse_create_request(&body.0);
    match runtime::create_session(&terminal, request).await {
        Ok(session) => {
            let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            Json(json!({
                "sessionId": session.id,
                "cols": inner.cols,
                "rows": inner.rows,
                "status": inner.status.as_str(),
            }))
            .into_response()
        }
        Err(message) => {
            let status = if message == "Maximum terminal sessions reached" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// JS field presence semantics for typed extraction: parameter defaults apply
/// only to `undefined`; `null` and mismatched types read as invalid so the
/// validators reject them with the exact JS messages.
/// 中文补充：三态区分缺省、合法值与非法值，让校验器产出与 JS 逐字一致的报错。
fn js_string_field(body: &Value, key: &str) -> JsField<String> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::String(value)) => JsField::Present(value.clone()),
        Some(_) => JsField::Invalid,
    }
}

/// 提取数字字段（f64）；非数字类型记为 Invalid，供校验器以 JS 文案拒绝。
fn js_number_field(body: &Value, key: &str) -> JsField<f64> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::Number(number)) => JsField::Present(number.as_f64().unwrap_or(f64::NAN)),
        Some(_) => JsField::Invalid,
    }
}

/// 提取布尔字段；非布尔类型记为 Invalid，与 JS presence 语义一致。
fn js_bool_field(body: &Value, key: &str) -> JsField<bool> {
    match body.get(key) {
        None => JsField::Absent,
        Some(Value::Bool(value)) => JsField::Present(*value),
        Some(_) => JsField::Invalid,
    }
}

/// 把 create 请求体映射为 CreateSessionRequest：非字符串 sessionId 回退为
/// 生成的 UUID，各可选字段经三态提取器保留缺省/非法信息。
fn parse_create_request(body: &Value) -> CreateSessionRequest {
    CreateSessionRequest {
        // Non-string sessionIds fall back to a generated UUID, like the JS.
        session_id: body
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        cwd: body.get("cwd").and_then(Value::as_str).map(str::to_string),
        cols: js_number_field(body, "cols"),
        rows: js_number_field(body, "rows"),
        theme_mode: body
            .get("themeMode")
            .and_then(Value::as_str)
            .map(str::to_string),
        terminal_background: body
            .get("terminalBackground")
            .and_then(Value::as_str)
            .map(str::to_string),
        terminal_foreground: body
            .get("terminalForeground")
            .and_then(Value::as_str)
            .map(str::to_string),
        shell: js_string_field(body, "shell"),
        login_shell: js_bool_field(body, "loginShell"),
    }
}

/// `POST /api/terminal/:sessionId/resize`.
/// 中文补充：尺寸必须是 1..=1000 列、1..=500 行的整数；本路由只应用尺寸，
/// 不做下限保护——尺寸策略归协商模型所有，改动会广播给所有附件。
async fn resize_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let cols = js_number_field(&body.0, "cols");
    let rows = js_number_field(&body.0, "rows");
    let valid = |field: &JsField<f64>, max: u16| match field {
        JsField::Present(value) => value.fract() == 0.0 && *value >= 1.0 && *value <= max as f64,
        _ => false,
    };
    if !valid(&cols, 1000) || !valid(&rows, 500) {
        return bad_request("Invalid terminal dimensions");
    }
    let (cols, rows) = (
        match cols {
            JsField::Present(value) => value as u16,
            _ => 0,
        },
        match rows {
            JsField::Present(value) => value as u16,
            _ => 0,
        },
    );
    // Direct resize still broadcasts so other devices follow; no floor — the
    // negotiation model owns sizing policy, this route just applies.
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    if inner.status == SessionStatus::Running
        && let Some(process) = inner.process.clone()
    {
        process.resize(cols, rows);
    }
    inner.grid.resize(cols, rows);
    inner.cols = cols;
    inner.rows = rows;
    runtime::publish(
        &terminal,
        &session,
        &mut inner,
        json!({"t": "resized", "cols": cols, "rows": rows}),
    );
    runtime::schedule_grid_drain(&terminal, &session, &mut inner);
    Json(json!({ "success": true, "cols": cols, "rows": rows })).into_response()
}

/// `POST /api/terminal/:sessionId/appearance`.
/// 中文补充：主题/前景/背景由 runtime::apply_appearance 应用并推送给已订阅的 TUI。
async fn update_appearance(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    runtime::apply_appearance(&mut inner, &body.0);
    Json(json!({ "success": true })).into_response()
}

/// `POST /api/terminal/:sessionId/restart`.
/// 中文补充：restart_session 先验证替代配置再原子替换；失败时旧进程原样保留。
async fn restart_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    body: JsonBody,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    match runtime::restart_session(&terminal, session.clone(), &body.0).await {
        Ok(()) => {
            let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            Json(json!({
                "sessionId": session.id,
                "cols": inner.cols,
                "rows": inner.rows,
                "status": inner.status.as_str(),
            }))
            .into_response()
        }
        Err(message) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// `DELETE /api/terminal/:sessionId?claimant=` — a claimant-scoped delete is a
/// tab close (release exactly that claim, kill only when no live claim
/// remains); without a claimant it is an explicit destructive kill.
/// 中文补充：剩余认领还需未超 IDLE_TIMEOUT_MS 才算存活，过期认领不阻止击杀。
async fn close_terminal(
    State(state): State<ModuleState>,
    Path(session_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let (terminal, _) = state;
    let Some(session) = lookup_session(&terminal, &session_id) else {
        return not_found("Terminal session not found");
    };
    let claimant = single_query_value(raw.as_deref(), "claimant")
        .filter(|claimant| !claimant.trim().is_empty() && claimant.len() <= 128)
        .map(|claimant| claimant.trim().to_string());
    if let Some(claimant) = &claimant {
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.claims.remove(claimant);
        let now = runtime::now_ms();
        inner
            .claims
            .retain(|_, touched_at| now.saturating_sub(*touched_at) <= runtime::IDLE_TIMEOUT_MS);
        if !inner.claims.is_empty() {
            return Json(json!({ "success": true, "released": true, "killed": false }))
                .into_response();
        }
    }
    let termination = terminal.remove_session(&session_id, "CLOSED", "Terminal closed", false);
    if let Some(termination) = termination {
        termination.await;
    }
    Json(json!({ "success": true, "released": true, "killed": true })).into_response()
}

/// `POST /api/terminal/force-kill` — terminate matching sessions immediately.
/// 中文补充：sessionId 优先于 cwd，二者皆缺省时匹配全部会话；终止异步进行，
/// 响应立即返回被杀 id 列表。
async fn force_kill(State(state): State<ModuleState>, body: JsonBody) -> Response {
    let (terminal, _) = state;
    let session_id = body
        .0
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = body
        .0
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let matching: Vec<String> = {
        let sessions = terminal.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .iter()
            .filter(|(id, session)| {
                if let Some(session_id) = &session_id {
                    return id.as_str() == session_id.as_str();
                }
                if let Some(cwd) = &cwd {
                    let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                    return inner.cwd == *cwd;
                }
                true
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut killed_session_ids = Vec::new();
    for id in matching {
        if let Some(termination) =
            terminal.remove_session(&id, "KILLED", "Terminal was killed", true)
        {
            tokio::spawn(termination);
            killed_session_ids.push(id);
        }
    }
    Json(json!({
        "success": true,
        "killedCount": killed_session_ids.len(),
        "killedSessionIds": killed_session_ids,
    }))
    .into_response()
}

/// 按 id 从会话表查找并克隆 Arc 句柄；不存在返回 None。
fn lookup_session(
    terminal: &Arc<TerminalState>,
    session_id: &str,
) -> Option<Arc<runtime::Session>> {
    terminal
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
}

/// 构造 404 JSON 错误响应。
fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

/// 构造 400 JSON 错误响应。
fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// First value for `key` in a raw query string. Express turns duplicates into
/// arrays (which its consumers treat as non-strings), so repeated keys read as
/// absent here.
/// 中文补充：重复键第一次命中即返回，但 seen 计数使其后置为缺失，模拟 Express 行为。
fn single_query_value(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    let mut seen = 0;
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if name == key {
            seen += 1;
            if seen > 1 {
                return None;
            }
            return Some(value.into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Body extractor (express.json semantics)
// ---------------------------------------------------------------------------

/// `express.json()` semantics: bodies are parsed only for `application/json`
/// content types; anything else reads as an absent body (handlers treat it as
/// `{}`). Malformed JSON is a 400 JSON error.
/// 中文补充：超限返回 413，坏 JSON 返回 400，均带 JSON 错误体。
struct JsonBody(Value);

/// JsonBody 的固有辅助方法。
impl JsonBody {
    /// 以 Null 表示“无请求体”，处理器按字段缺省语义对待。
    fn absent() -> Self {
        Self(Value::Null)
    }
}

/// express.json() 语义的 axum FromRequest 实现。
impl<S> axum::extract::FromRequest<S> for JsonBody
where
    S: Send + Sync,
{
    /// 拒绝类型直接是完整 Response（错误响应在此构造完毕）。
    type Rejection = Response;

    /// 提取流程：读体（100 KiB 上限）→ 非 application/json 或空体视为缺省 →
    /// 解析 JSON，失败返回 400。
    async fn from_request(
        request: axum::http::Request<axum::body::Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let bytes = match axum::body::to_bytes(request.into_body(), JSON_BODY_LIMIT_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "Payload too large" })),
                )
                    .into_response());
            }
        };
        if content_type != "application/json" || bytes.is_empty() {
            return Ok(Self::absent());
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(Self(value)),
            Err(error) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON body: {error}") })),
            )
                .into_response()),
        }
    }
}
