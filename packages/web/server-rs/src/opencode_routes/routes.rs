//! Port of the `opencode/routes.js` remainder (the parts not owned by the
//! settings / core_routes modules): behavior/AGENTS.md endpoints, provider
//! config CRUD, and the MCP OAuth parking + browser-callback routes.
//! 中文说明:本模块承载 `opencode/routes.js` 中未划入 settings 与
//! core_routes 的剩余路由:behavior/AGENTS.md 读写端点、provider 配置
//! 增删改查,以及 MCP OAuth 待授权上下文(pending)暂存与浏览器回调
//! 页面。

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value, json};

use super::ModuleState;
use super::auth;
use super::claude_cli;
use super::mutation::deferred_restart_response;
use super::project_directory::{requested_directory_hint, resolve_project_directory};
use super::providers;
use super::webutil::{engine_url, first_query_value, now_millis};
use super::{OpenCodeEnv, PendingMcpAuthContext};

/// JS: `express.json({ limit: '50mb' })` for `/api/config/*`, `/api/provider`
/// and `/api/opencode` paths (registerCommonRequestMiddleware).
/// 大请求体上限:`/api/config/*`、`/api/provider` 与 `/api/opencode`
/// 路径的 JSON body 最多 50MB(对应 JS 的 common request middleware)。
const LARGE_BODY_LIMIT: usize = 50 * 1024 * 1024;
/// JS: `express.json({ limit: '1mb' })` for `/api/behavior`.
/// `/api/behavior` 系列端点的 JSON body 上限(1MB)。
const BEHAVIOR_BODY_LIMIT: usize = 1024 * 1024;
/// JS: `express.json({ limit: '16kb' })` on `/api/mcp/auth/pending`.
/// `/api/mcp/auth/pending` 的 JSON body 上限(16KB)。
const PENDING_BODY_LIMIT: usize = 16 * 1024;
/// `routes.js` `MAX_BEHAVIOR_PROMPT_SIZE`.
/// AGENTS.md 正文的字符数上限(1MB):超出直接拒绝,防止超大 prompt
/// 写入磁盘。
const MAX_BEHAVIOR_PROMPT_SIZE: usize = 1024 * 1024;
/// `PENDING_MCP_AUTH_TTL_MS`.
/// MCP 待授权上下文(state → context)的有效期:30 分钟,过期后回调
/// 无法命中即视为会话失效。
const PENDING_MCP_AUTH_TTL_MS: u64 = 30 * 60 * 1000;

/// 注册本模块的全部路由:MCP OAuth pending 暂存/读取/清除、OAuth
/// 浏览器回调页、provider source 查询/配置写入/断开,以及 AGENTS.md
/// 的读写。
pub(crate) fn provider_routes() -> axum::Router<ModuleState> {
    axum::Router::new()
        .route(
            "/api/mcp/auth/pending",
            post(store_pending_mcp_auth)
                .get(read_pending_mcp_auth)
                .delete(clear_pending_mcp_auth),
        )
        .route("/mcp/oauth/callback", get(mcp_oauth_callback))
        .route("/api/provider/{providerId}/source", get(provider_source))
        .route("/api/provider", put(put_provider))
        .route(
            "/api/provider/{providerId}/auth",
            delete(delete_provider_auth),
        )
        .route(
            "/api/behavior/agents-md",
            get(get_behavior_agents_md).put(put_behavior_agents_md),
        )
}

use axum::routing::{delete, put};

/// Mirror `express.json({ limit })`: only JSON content types parse, empty
/// JSON bodies parse as `{}`, anything else stays `null` (`req.body ?? {}`),
/// malformed JSON is a 400, over-limit is a 413.
/// 复刻 `express.json({ limit })` 的解析语义:仅 JSON content type
/// (`application/json` 或 `application/*+json`)才解析,否则保持
/// `null`(调用方以 `?? {}` 兜底为空对象);空 JSON body 解析为
/// `{}`;JSON 格式错误返回 400,超过 limit 返回 413。`Err` 分支已是
/// 完整的 HTTP Response,可直接透传给客户端。
async fn parse_json_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Value, Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let is_json =
        mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"));
    if !is_json {
        return Ok(Value::Null);
    }

    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let status = if err.to_string().contains("length limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return Err((status, format!("{err}")).into_response());
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => Ok(value),
        Err(err) => Err((StatusCode::BAD_REQUEST, err.to_string()).into_response()),
    }
}

// ---------------------------------------------------------------------------
// Behavior / AGENTS.md
// ---------------------------------------------------------------------------

/// `legacyAgentsMdPath` — `~/.config/opencode/AGENTS.md`.
/// 旧版全局 AGENTS.md 路径:`config_dir/AGENTS.md`
/// (即 `~/.config/opencode/AGENTS.md`),仅用于读取探测是否仍有内容。
fn legacy_agents_md_path(env: &OpenCodeEnv) -> std::path::PathBuf {
    env.config_dir.join("AGENTS.md")
}

/// `resolveAgentDir`: ask the omp-host (`GET /agent-dir`), fall back to
/// `~/.omp/agent`. Never cached — the agent dir is profile-scoped.
/// 解析当前生效的 agent 目录:优先请求 omp-host 的 `GET /agent-dir`
/// (附带 engine 鉴权头),请求失败、状态非 2xx 或返回体缺失 agentDir
/// 时回退到 `~/.omp/agent`。每次调用都重新解析、不做缓存——agent
/// 目录随 profile 切换而不同。
async fn resolve_agent_dir(state: &ModuleState) -> std::path::PathBuf {
    let fetch = async {
        let url = engine_url(&state.ctx, "/agent-dir")?;
        let mut request = state.ctx.engine.http().get(&url);
        if let Some(auth_header) = state.ctx.engine.auth_header() {
            request = request.header("authorization", auth_header);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err("agent-dir not ok".to_string());
        }
        let body: Value = response.json().await.map_err(|error| error.to_string())?;
        match body.get("agentDir").and_then(Value::as_str) {
            Some(dir) if !dir.is_empty() => Ok(dir.to_string()),
            _ => Err("agentDir missing".to_string()),
        }
    };
    match fetch.await {
        Ok(dir) => std::path::PathBuf::from(dir),
        // omp-host not reachable yet: static default, never cached.
        Err(_) => state.env.home.join(".omp").join("agent"),
    }
}

/// `GET /api/behavior/agents-md`:读取 agent 目录下 AGENTS.md 的内容,
/// 同时探测旧版 `~/.config/opencode/AGENTS.md` 是否仍有非空内容;
/// 文件不存在时返回空内容与 `exists: false`,恒为 200。
async fn get_behavior_agents_md(State(state): State<ModuleState>) -> Response {
    let target_path = resolve_agent_dir(&state).await.join("AGENTS.md");
    let (content, exists) = match tokio::fs::read(&target_path).await {
        Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), true),
        Err(_) => (String::new(), false),
    };

    let legacy_path = legacy_agents_md_path(&state.env);
    let legacy_has_content = match tokio::fs::read(&legacy_path).await {
        Ok(bytes) => !String::from_utf8_lossy(&bytes).trim().is_empty(),
        Err(_) => false,
    };

    Json(json!({
        "content": content,
        "exists": exists,
        "path": target_path.to_string_lossy(),
        "legacy": {
            "path": legacy_path.to_string_lossy(),
            "hasContent": legacy_has_content,
        },
    }))
    .into_response()
}

/// `/api/config/*` entity bodies ride the 50mb JSON parser.
/// `/api/config/*` 实体路由共用的 body 解析入口:套用 50MB 大请求体
/// 上限后委托 `parse_json_body`,语义与其完全一致。
pub(crate) async fn parse_json_body_for_entity(
    headers: &HeaderMap,
    body: Body,
) -> Result<Value, Response> {
    parse_json_body(headers, body, LARGE_BODY_LIMIT).await
}

/// `PUT /api/behavior/agents-md`:保存 AGENTS.md。流程:先按
/// content-length 预检(超过 1MB 返回 413)→ 解析 JSON body 取
/// `content` 字段 → 正文超过 MAX_BEHAVIOR_PROMPT_SIZE 同样 413 →
/// 创建父目录并写入 agent 目录下的 AGENTS.md。成功返回延迟重启信封
/// (改动需重启 engine 生效);写入失败记录日志并返回 500。
async fn put_behavior_agents_md(State(state): State<ModuleState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    // The common middleware pre-checks content-length before parsing.
    if let Some(length) = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && length > BEHAVIOR_BODY_LIMIT
    {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": format!("Content exceeds maximum size of {BEHAVIOR_BODY_LIMIT} bytes") })),
        )
            .into_response();
    }
    let parsed = match parse_json_body(&parts.headers, body, BEHAVIOR_BODY_LIMIT).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let body_value = parsed.as_object().cloned().unwrap_or_default();
    let content = body_value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // JS `.length` counts UTF-16 code units; chars are the closest measure.
    if content.chars().count() > MAX_BEHAVIOR_PROMPT_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!("Content exceeds maximum size of {MAX_BEHAVIOR_PROMPT_SIZE} bytes")
            })),
        )
            .into_response();
    }

    let target_path = resolve_agent_dir(&state).await.join("AGENTS.md");
    let write = async {
        if let Some(parent) = target_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target_path, &content).await
    };
    match write.await {
        Ok(()) => Json(deferred_restart_response(
            "AGENTS.md saved. Restart the engine to apply.",
        ))
        .into_response(),
        Err(error) => {
            tracing::error!("Failed to write AGENTS.md: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to write AGENTS.md: {error}") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Provider config CRUD
// ---------------------------------------------------------------------------

/// `GET /api/provider/:providerId/source`:解析项目目录后(显式请求
/// 了目录但解析失败时返回 400)查询 provider 的配置来源;
/// `claude-code` 的 auth 存在性走 claude CLI 探测,其余 provider 走
/// 本地 auth 存储,并把结果注入 `sources.auth.exists`。查询失败记录
/// 日志并返回 500。
async fn provider_source(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(provider_id): axum::extract::Path<String>,
) -> Response {
    let requested_directory = requested_directory_hint(&headers, &uri);

    let resolved = resolve_project_directory(&state.ctx, &headers, &uri).await;
    let directory = match resolved.directory {
        Some(directory) => Some(directory),
        None => {
            if requested_directory.is_some() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": resolved.error.unwrap_or_default() })),
                )
                    .into_response();
            }
            None
        }
    };

    let result = providers::get_provider_sources(&state.env, &provider_id, directory.as_deref());
    let mut sources = match result {
        Ok(sources) => sources,
        Err(error) => {
            tracing::error!("Failed to get provider sources: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            )
                .into_response();
        }
    };

    let auth_exists = if provider_id == "claude-code" {
        claude_cli::claude_cli_auth_status().await.connected
    } else {
        auth::get_provider_auth(&state.env, &provider_id)
            .map(|value| value.is_some())
            .unwrap_or(false)
    };
    if let Some(Value::Object(auth_block)) = sources.get_mut("auth") {
        auth_block.insert("exists".to_string(), Value::Bool(auth_exists));
    }

    Json(json!({ "providerId": provider_id, "sources": sources })).into_response()
}

/// `PUT /api/provider`:写入 provider 配置。从 body 提取 `providerID`
/// (兼容 `providerId` 别名)、`config` 与 `scope`(默认 user);
/// providerID/config 缺失或 scope 非 user/project/custom 返回 400。
/// project 作用域(或显式请求目录)必须有可解析的工作目录,否则 400。
/// 随后调用 `upsert_provider_config` 落盘:成功在延迟重启信封中附上
/// providerId/path/config;校验类错误(UpsertError::Validation)返回
/// 400,其余错误返回 500。
async fn put_provider(State(state): State<ModuleState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body, LARGE_BODY_LIMIT).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let body_map = parsed.as_object().cloned().unwrap_or_default();

    let provider_id = body_map
        .get("providerID")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| {
            body_map
                .get("providerId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
        .to_string();
    let config = body_map.get("config").cloned().unwrap_or(Value::Null);
    let scope = body_map
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();

    if provider_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Provider ID is required" })),
        )
            .into_response();
    }
    if !config.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Provider config is required" })),
        )
            .into_response();
    }
    if scope != "user" && scope != "project" && scope != "custom" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid scope" })),
        )
            .into_response();
    }

    let headers = parts.headers.clone();
    let uri = parts.uri.clone();
    let requested_directory = requested_directory_hint(&headers, &uri);

    let resolved = resolve_project_directory(&state.ctx, &headers, &uri).await;
    let directory = if scope == "project" || requested_directory.is_some() {
        match resolved.directory {
            Some(directory) => Some(directory),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": resolved.error.unwrap_or_else(|| "Working directory is required".to_string()) })),
                )
                    .into_response();
            }
        }
    } else {
        resolved.directory
    };

    let has_stored_auth = auth::get_provider_auth(&state.env, &provider_id)
        .map(|value| value.is_some())
        .unwrap_or(false);

    match providers::upsert_provider_config(
        &state.env,
        &provider_id,
        &config,
        directory.as_deref(),
        &scope,
        has_stored_auth,
    ) {
        Ok(outcome) => {
            let mut payload = deferred_restart_response(&format!(
                "Provider {provider_id} saved. Restart the engine to apply."
            ));
            if let Value::Object(map) = &mut payload {
                map.insert("providerId".to_string(), json!(outcome.provider_id));
                map.insert("path".to_string(), json!(outcome.path.to_string_lossy()));
                map.insert("config".to_string(), outcome.config);
            }
            Json(payload).into_response()
        }
        Err(providers::UpsertError::Validation(message)) => {
            tracing::error!("Failed to upsert provider config: {message}");
            (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
        }
        Err(providers::UpsertError::Other(message)) => {
            tracing::error!("Failed to upsert provider config: {message}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": message })),
            )
                .into_response()
        }
    }
}

/// `DELETE /api/provider/:providerId/auth`:按 query `scope` 断开
/// provider。默认 `auth` 仅删除凭据;`user`/`project`/`custom` 删除
/// 对应配置层中的条目;`all` 依次删除 auth/user/project/custom 四处
/// (任一步失败即中断返回 500)。project 作用域要求可解析的工作目录。
/// 有实际删除时返回带 success/removed 的延迟重启信封;未删除任何
/// 内容时返回 "Provider was not connected";非法 scope 返回 400。
async fn delete_provider_auth(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(provider_id): axum::extract::Path<String>,
) -> Response {
    let scope = first_query_value(&uri, "scope").unwrap_or_else(|| "auth".to_string());
    let requested_directory = requested_directory_hint(&headers, &uri);

    let resolved = resolve_project_directory(&state.ctx, &headers, &uri).await;
    let directory = if scope == "project" || requested_directory.is_some() {
        match resolved.directory {
            Some(directory) => Some(directory),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": resolved.error.unwrap_or_default() })),
                )
                    .into_response();
            }
        }
    } else {
        resolved.directory
    };

    let removed: Result<bool, String> = match scope.as_str() {
        "auth" => auth::remove_provider_auth(&state.env, &provider_id),
        "user" | "project" | "custom" => providers::remove_provider_config(
            &state.env,
            &provider_id,
            directory.as_deref(),
            &scope,
        ),
        "all" => {
            let auth_removed = match auth::remove_provider_auth(&state.env, &provider_id) {
                Ok(removed) => removed,
                Err(message) => return provider_disconnect_error(&message),
            };
            let user_removed = match providers::remove_provider_config(
                &state.env,
                &provider_id,
                directory.as_deref(),
                "user",
            ) {
                Ok(removed) => removed,
                Err(message) => return provider_disconnect_error(&message),
            };
            let project_removed = match directory.as_deref() {
                Some(directory) => match providers::remove_provider_config(
                    &state.env,
                    &provider_id,
                    Some(directory),
                    "project",
                ) {
                    Ok(removed) => removed,
                    Err(message) => return provider_disconnect_error(&message),
                },
                None => false,
            };
            let custom_removed = match providers::remove_provider_config(
                &state.env,
                &provider_id,
                directory.as_deref(),
                "custom",
            ) {
                Ok(removed) => removed,
                Err(message) => return provider_disconnect_error(&message),
            };
            Ok(auth_removed || user_removed || project_removed || custom_removed)
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid scope" })),
            )
                .into_response();
        }
    };

    match removed {
        Ok(true) => {
            let mut payload = deferred_restart_response(
                "Provider disconnected successfully. Restart the engine to apply.",
            );
            if let Value::Object(map) = &mut payload {
                map.insert("success".to_string(), Value::Bool(true));
                map.insert("removed".to_string(), Value::Bool(true));
            }
            Json(payload).into_response()
        }
        Ok(false) => Json(json!({
            "success": true,
            "removed": false,
            "requiresReload": false,
            "message": "Provider was not connected",
        }))
        .into_response(),
        Err(message) => provider_disconnect_error(&message),
    }
}

/// 断开 provider 失败的统一响应:记录 error 日志并返回 500 +
/// `{ "error": message }`。
fn provider_disconnect_error(message: &str) -> Response {
    tracing::error!("Failed to disconnect provider: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// MCP auth pending + OAuth callback
// ---------------------------------------------------------------------------

/// 规整 pending 上下文的字符串字段:非字符串或 trim 后为空返回
/// `None`,否则返回 trim 后的字符串(用于 state/name/directory/origin)。
fn normalize_pending_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// `POST /api/mcp/auth/pending`:暂存 MCP OAuth 待授权上下文。body
/// 的 `state` 缺失时静默成功(context 返回 null);`name` 缺失返回
/// 400。以 state 为键写入内存 pending 表并附 30 分钟过期时间,响应
/// 回显 name/directory/origin 组成的 context。
async fn store_pending_mcp_auth(State(state): State<ModuleState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(&parts.headers, body, PENDING_BODY_LIMIT).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let body_map = parsed.as_object().cloned().unwrap_or_default();

    let Some(state_value) = normalize_pending_string(body_map.get("state")) else {
        return Json(json!({ "success": true, "context": Value::Null })).into_response();
    };
    let Some(name) = normalize_pending_string(body_map.get("name")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "MCP server name is required" })),
        )
            .into_response();
    };

    let entry = PendingMcpAuthContext {
        name: name.clone(),
        directory: normalize_pending_string(body_map.get("directory")),
        origin: normalize_pending_string(body_map.get("origin")),
        expires_at: now_millis() + PENDING_MCP_AUTH_TTL_MS,
    };
    let context = json!({
        "name": entry.name,
        "directory": entry.directory,
        "origin": entry.origin,
    });
    state.pending.insert(state_value, entry);

    Json(json!({ "success": true, "context": context })).into_response()
}

/// `GET /api/mcp/auth/pending?state=...`:读取待授权上下文。state
/// 缺失返回 null JSON;未命中(过期或不存在)返回 404;命中返回
/// name/directory/origin/expiresAt。
async fn read_pending_mcp_auth(State(state): State<ModuleState>, uri: Uri) -> Response {
    let state_value = first_query_value(&uri, "state")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(state_value) = state_value else {
        return Json(Value::Null).into_response();
    };
    match state.pending.get(&state_value) {
        Some(entry) => Json(json!({
            "name": entry.name,
            "directory": entry.directory,
            "origin": entry.origin,
            "expiresAt": entry.expires_at,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No pending MCP auth context" })),
        )
            .into_response(),
    }
}

/// `DELETE /api/mcp/auth/pending?state=...`:移除指定 state 的待授权
/// 上下文;state 缺失则不做任何动作。恒返回 `{ "success": true }`。
async fn clear_pending_mcp_auth(State(state): State<ModuleState>, uri: Uri) -> Response {
    if let Some(state_value) = first_query_value(&uri, "state")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        state.pending.remove(&state_value);
    }
    Json(json!({ "success": true })).into_response()
}

/// HTML 转义:替换 `&`、`<`、`>`、`"`、`'` 五个字符为对应实体,
/// 防止回调页标题/消息中的外部内容被注入为标记。
fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// `renderMcpOAuthCallbackPage` — inline self-contained page.
/// 渲染自包含的 OAuth 回调结果页(title/message 均已转义);桌面端
/// 发起的授权(desktop_return)额外渲染"返回 OMPChamber"链接并立即
/// 跳转 `ompchamber://focus/mcp-auth` 深链。
fn render_mcp_oauth_callback_page(title: &str, message: &str, desktop_return: bool) -> String {
    let return_section = if desktop_return {
        "<a class=\"return\" href=\"ompchamber://focus/mcp-auth\">Return to OMPChamber</a>\n<script>window.location.href = 'ompchamber://focus/mcp-auth';</script>"
    } else {
        ""
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{title} — OMPChamber</title>\n<style>\n  :root {{ color-scheme: light dark; }}\n  body {{ margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;\n         font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;\n         background: Canvas; color: CanvasText; }}\n  main {{ max-width: 34rem; padding: 2.5rem 2rem; text-align: center; }}\n  h1 {{ font-size: 1.25rem; margin: 0 0 0.75rem; }}\n  p {{ margin: 0; line-height: 1.5; opacity: 0.85; }}\n  a.return {{ display: inline-block; margin-top: 1.5rem; padding: 0.5rem 1.25rem; border-radius: 0.5rem;\n             border: 1px solid color-mix(in srgb, CanvasText 25%, transparent); color: inherit; text-decoration: none; }}\n</style>\n</head>\n<body>\n<main>\n<h1>{title}</h1>\n<p>{message}</p>\n{return_section}\n</main>\n</body>\n</html>",
        title = escape_html(title),
        message = escape_html(message),
    )
}

/// `GET /mcp/oauth/callback`:OAuth 提供方回跳端点。流程:读取
/// query 中的 state/code/error;provider 返回 error 或缺失 code 时以
/// 400 失败页收尾;state 无法命中 pending 上下文(过期/未知会话)时
/// 以 400 失败页收尾。校验通过后把 code 转发给 engine 的
/// `/mcp/<name>/auth/callback`(附带 directory query 与鉴权头),
/// 2xx 视为成功;上游拒绝时优先展示其 error/message,网络错误返回
/// 502。任何收尾路径都会消费(移除)对应的 pending state,并按发起
/// 来源(desktop 与否)渲染结果页。
async fn mcp_oauth_callback(State(state): State<ModuleState>, uri: Uri) -> Response {
    let query_value = |key: &str| {
        first_query_value(&uri, key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let state_value = query_value("state");
    let code = query_value("code");
    let provider_error = query_value("error");
    let provider_error_description = query_value("error_description");

    let context = state_value
        .as_deref()
        .and_then(|state_value| state.pending.get(state_value));
    let started_from_desktop = context
        .as_ref()
        .map(|entry| entry.origin.as_deref() == Some("desktop"))
        .unwrap_or(false);

    let finish = |status: StatusCode, title: &str, message: &str| -> Response {
        if let Some(state_value) = state_value.as_deref() {
            state.pending.remove(state_value);
        }
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(render_mcp_oauth_callback_page(
                title,
                message,
                started_from_desktop,
            )))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    };

    if let Some(provider_error) = provider_error {
        return finish(
            StatusCode::BAD_REQUEST,
            "Authorization Failed",
            &provider_error_description.unwrap_or(provider_error),
        );
    }
    let Some(code) = code else {
        return finish(
            StatusCode::BAD_REQUEST,
            "Authorization Failed",
            "The provider did not return an authorization code. Start authorization again from MCP Settings.",
        );
    };
    let Some(context) = context.filter(|entry| !entry.name.is_empty()) else {
        return finish(
            StatusCode::BAD_REQUEST,
            "Authorization Failed",
            "This authorization session has expired or is unknown to the running app. Return to OMPChamber and click Authorize again.",
        );
    };

    let upstream = async {
        let url_text = engine_url(
            &state.ctx,
            &format!("/mcp/{}/auth/callback", utf8_percent_encode(&context.name)),
        )?;
        let mut url = url::Url::parse(&url_text).map_err(|error| error.to_string())?;
        if let Some(directory) = context.directory.as_deref() {
            url.query_pairs_mut().append_pair("directory", directory);
        }
        let mut request = state
            .ctx
            .engine
            .http()
            .post(url.as_str())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .json(&json!({ "code": code }));
        if let Some(auth_header) = state.ctx.engine.auth_header() {
            request = request.header("authorization", auth_header);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let payload: Option<Value> = response.json().await.ok();
        Ok::<(u16, Option<Value>), String>((status, payload))
    };

    match upstream.await {
        Ok((status, _payload)) if (200..300).contains(&status) => finish(
            StatusCode::OK,
            "Authorization Complete",
            "You can close this tab and return to OMPChamber.",
        ),
        Ok((status, payload)) => {
            let message = payload
                .as_ref()
                .and_then(|payload| {
                    payload
                        .get("error")
                        .or_else(|| payload.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    format!(
                        "OpenCode rejected the authorization code ({status}). Start authorization again from MCP Settings."
                    )
                });
            finish(StatusCode::BAD_GATEWAY, "Authorization Failed", &message)
        }
        Err(error) => finish(
            StatusCode::BAD_GATEWAY,
            "Authorization Failed",
            &if error.is_empty() {
                "Failed to complete MCP authorization.".to_string()
            } else {
                error
            },
        ),
    }
}

/// `encodeURIComponent` for the MCP server name in the callback path.
/// 对 MCP server 名称做 `encodeURIComponent` 等价的百分号编码:
/// 字母、数字与 `-_.~!*'()` 保持原样,其余字节编码为 `%XX`,用于拼接
/// 回调转发 URL 的路径段。
fn utf8_percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
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
