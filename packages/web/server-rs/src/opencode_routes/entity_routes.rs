//! Port of `opencode/config-entity-routes.js` for the agent, command, and
//! MCP entities (the snippet routes remain with the snippets port). Writes
//! persist to disk immediately and answer with the deferred-restart
//! envelope; the engine restart is deferred to `POST /api/config/reload`
//! (owned by core_routes).
//! 中文说明:本模块移植自 `opencode/config-entity-routes.js`,承载
//! agent、command、MCP 三类实体的 HTTP CRUD 路由(snippet 路由归属
//! 另一个移植模块)。写入操作立即持久化到磁盘,并统一返回"延迟重启"
//! 信封——engine 的重启被推迟到 `POST /api/config/reload`(由
//! core_routes 模块提供)。

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};

use super::ModuleState;
use super::agents;
use super::commands;
use super::mcp;
use super::mutation::deferred_restart_response;
use super::project_directory::{resolve_optional_project_directory, resolve_project_directory};
use super::routes::parse_json_body_for_entity;

/// 注册三类实体的路由树:agent(含 config 子路由)、MCP(列表 + 单条)
/// 与 command,各自挂 GET/POST/PATCH/DELETE 处理器。
pub(crate) fn entity_routes() -> axum::Router<ModuleState> {
    axum::Router::new()
        .route(
            "/api/config/agents/{name}",
            get(get_agent_sources_route)
                .post(create_agent_route)
                .patch(update_agent_route)
                .delete(delete_agent_route),
        )
        .route(
            "/api/config/agents/{name}/config",
            get(get_agent_config_route),
        )
        .route("/api/config/mcp", get(list_mcp_route))
        .route(
            "/api/config/mcp/{name}",
            get(get_mcp_route)
                .post(create_mcp_route)
                .patch(update_mcp_route)
                .delete(delete_mcp_route),
        )
        .route(
            "/api/config/commands/{name}",
            get(get_command_sources_route)
                .post(create_command_route)
                .patch(update_command_route)
                .delete(delete_command_route),
        )
}

/// 构造统一的错误响应:指定 HTTP 状态码 + `{ "error": message }` 的
/// JSON body。
fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Resolve the required project directory, answering the JS 400 on failure.
/// 解析必需的项目目录:复用 project_directory 的解析逻辑,无法解析
/// 出目录时直接返回 JS 对应的 400 错误响应;Ok 携带目录路径。
async fn required_directory(
    state: &ModuleState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<std::path::PathBuf, Response> {
    let resolved = resolve_project_directory(&state.ctx, headers, uri).await;
    match resolved.directory {
        Some(directory) => Ok(directory),
        None => Err(error_response(
            StatusCode::BAD_REQUEST,
            &resolved.error.unwrap_or_default(),
        )),
    }
}

/// Resolve the optional project directory (`resolveOptionalProjectDirectory`).
/// 解析可选的项目目录:目录允许不存在(返回 None),但解析本身出错
/// (例如显式请求了无效目录)仍返回 400。
async fn optional_directory(
    _state: &ModuleState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Option<std::path::PathBuf>, Response> {
    let resolved = resolve_optional_project_directory(headers, uri).await;
    match (&resolved.directory, &resolved.error) {
        (directory, None) => Ok(directory.clone()),
        (_, Some(error)) => Err(error_response(StatusCode::BAD_REQUEST, error)),
    }
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// `GET /api/config/agents/:name`:返回 agent 的来源元数据(透传
/// agents::get_agent_sources 的四组信息),并额外汇总顶层 `scope`
/// (优先 `.md`,其次 JSON,都没有为 null)与 `isBuiltIn`(两类来源
/// 均不存在时为真)。内部错误统一 500 并记录日志。
async fn get_agent_sources_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match required_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match agents::get_agent_sources(&state.env, &name, Some(&directory)) {
        Ok(sources) => {
            let md_exists = sources
                .get("md")
                .and_then(|md| md.get("exists"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let json_exists = sources
                .get("json")
                .and_then(|json| json.get("exists"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let scope = if md_exists {
                sources
                    .get("md")
                    .and_then(|md| md.get("scope"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else if json_exists {
                sources
                    .get("json")
                    .and_then(|json| json.get("scope"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            Json(json!({
                "name": name,
                "sources": sources,
                "scope": scope,
                "isBuiltIn": !md_exists && !json_exists,
            }))
            .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to get agent sources: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get agent configuration metadata",
            )
        }
    }
}

/// `GET /api/config/agents/:name/config`:返回 agent 的生效配置
/// (source/scope/config 三元组);内部错误 500 并记录日志。
async fn get_agent_config_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match required_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match agents::get_agent_config(&state.env, &name, Some(&directory)) {
        Ok(config) => Json(config).into_response(),
        Err(error) => {
            tracing::error!("Failed to get agent config: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get agent configuration",
            )
        }
    }
}

/// `POST /api/config/agents/:name`:创建 agent。body 经 50MB 解析器
/// 后拆出 `scope` 与其余配置字段;成功返回延迟重启信封,失败(含
/// 三处重名检查未过)返回 500 并附具体错误消息。
async fn create_agent_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (scope, config) = split_scope_and_config(parsed);

    let directory = match required_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match agents::create_agent(
        &state.env,
        &name,
        &config,
        Some(&directory),
        scope.as_deref(),
    ) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Agent {name} created successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("Failed to create agent: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to create agent".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `PATCH /api/config/agents/:name`:按字段更新 agent;body 必须是
/// JSON 对象(否则直接 500)。成功返回延迟重启信封,失败 500。
async fn update_agent_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(updates) = parsed.as_object().cloned() else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update agent");
    };

    let directory = match required_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match agents::update_agent(&state.env, &name, &updates, Some(&directory)) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Agent {name} updated successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("[Server] Failed to update agent: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to update agent".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `DELETE /api/config/agents/:name`:删除 agent;body 中可选的
/// `scope` 限定删除层级(缺省时项目/用户/JSON 依次尝试)。成功返回
/// 延迟重启信封,失败返回 500。
async fn delete_agent_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let scope = parsed
        .as_object()
        .and_then(|body| body.get("scope"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let directory = match required_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match agents::delete_agent(&state.env, &name, Some(&directory), scope.as_deref()) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Agent {name} deleted successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("Failed to delete agent: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to delete agent".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// 把创建请求的 body 拆为 (scope, 其余字段):提取顶层 `scope`
/// 字符串并从配置映射中剔除;非对象 body 视为空配置(scope 为 None)。
fn split_scope_and_config(parsed: Value) -> (Option<String>, Map<String, Value>) {
    let map = parsed.as_object().cloned().unwrap_or_default();
    let scope = map.get("scope").and_then(Value::as_str).map(str::to_string);
    let config: Map<String, Value> = map.into_iter().filter(|(key, _)| key != "scope").collect();
    (scope, config)
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

/// `GET /api/config/mcp`:列出所有 MCP server 配置(项目目录可选);
/// 失败返回 500 并记录日志。
async fn list_mcp_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let directory = match optional_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match mcp::list_mcp_configs(&state.env, directory.as_deref()) {
        Ok(configs) => Json(Value::Array(configs)).into_response(),
        Err(error) => {
            tracing::error!("[API:GET /api/config/mcp] Failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to list MCP configs".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `GET /api/config/mcp/:name`:读取单个 MCP server 配置;条目不
/// 存在返回 404,其余错误返回 500 并记录日志。
async fn get_mcp_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match optional_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match mcp::get_mcp_config(&state.env, &name, directory.as_deref()) {
        Ok(Some(config)) => Json(config).into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            &format!("MCP server \"{name}\" not found"),
        ),
        Err(error) => {
            tracing::error!("[API:GET /api/config/mcp/:name] Failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to get MCP config".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `POST /api/config/mcp/:name`:创建 MCP server 配置;body 拆出
/// `scope` 后整体作为配置传入。成功返回延迟重启信封,失败 500。
async fn create_mcp_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (scope, config) = split_scope_and_config(parsed);

    let directory = match optional_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match mcp::create_mcp_config(
        &state.env,
        &name,
        &Value::Object(config),
        directory.as_deref(),
        scope.as_deref(),
    ) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "MCP server \"{name}\" created. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("[API:POST /api/config/mcp/:name] Failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to create MCP server".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `PATCH /api/config/mcp/:name`:整体更新 MCP server 配置(body
/// 原样传入);"not found" 类错误映射为 404,其余错误返回 500。
async fn update_mcp_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };

    let directory = match optional_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match mcp::update_mcp_config(&state.env, &name, &parsed, directory.as_deref()) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "MCP server \"{name}\" updated. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) if error == format!("MCP server \"{name}\" not found") => {
            error_response(StatusCode::NOT_FOUND, &error)
        }
        Err(error) => {
            tracing::error!("[API:PATCH /api/config/mcp/:name] Failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to update MCP server".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `DELETE /api/config/mcp/:name`:删除 MCP server 配置;成功返回
/// 延迟重启信封,失败 500 并记录日志。
async fn delete_mcp_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match optional_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match mcp::delete_mcp_config(&state.env, &name, directory.as_deref()) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "MCP server \"{name}\" deleted. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("[API:DELETE /api/config/mcp/:name] Failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to delete MCP server".to_string()
                } else {
                    error
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `GET /api/config/commands/:name`:返回 command 的来源元数据,
/// 响应结构与 agent sources 路由一致(scope 汇总 + isBuiltIn 标记);
/// 内部错误 500 并记录日志。
async fn get_command_sources_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match required_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match commands::get_command_sources(&state.env, &name, Some(&directory)) {
        Ok(sources) => {
            let md_exists = sources
                .get("md")
                .and_then(|md| md.get("exists"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let json_exists = sources
                .get("json")
                .and_then(|json| json.get("exists"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let scope = if md_exists {
                sources
                    .get("md")
                    .and_then(|md| md.get("scope"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else if json_exists {
                sources
                    .get("json")
                    .and_then(|json| json.get("scope"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            Json(json!({
                "name": name,
                "sources": sources,
                "scope": scope,
                "isBuiltIn": !md_exists && !json_exists,
            }))
            .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to get command sources: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get command configuration metadata",
            )
        }
    }
}

/// `POST /api/config/commands/:name`:创建 command;body 拆出
/// `scope` 后作为配置传入。成功返回延迟重启信封,失败 500。
async fn create_command_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (scope, config) = split_scope_and_config(parsed);

    let directory = match required_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match commands::create_command(
        &state.env,
        &name,
        &config,
        Some(&directory),
        scope.as_deref(),
    ) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Command {name} created successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("Failed to create command: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to create command".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `PATCH /api/config/commands/:name`:按字段更新 command;body
/// 必须是 JSON 对象(否则 500)。成功返回延迟重启信封,失败 500。
async fn update_command_route(
    State(state): State<ModuleState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body_for_entity(&parts.headers, body).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(updates) = parsed.as_object().cloned() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update command",
        );
    };

    let directory = match required_directory(&state, &parts.headers, &parts.uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    match commands::update_command(&state.env, &name, &updates, Some(&directory)) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Command {name} updated successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("[Server] Failed to update command: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to update command".to_string()
                } else {
                    error
                },
            )
        }
    }
}

/// `DELETE /api/config/commands/:name`:删除 command(底层会依次
/// 清理项目 `.md`、用户 `.md` 与 JSON 层条目);成功返回延迟重启
/// 信封,全部未命中时返回 500。
async fn delete_command_route(
    State(state): State<ModuleState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    let directory = match required_directory(&state, &headers, &uri).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    match commands::delete_command(&state.env, &name, Some(&directory)) {
        Ok(()) => Json(deferred_restart_response(&format!(
            "Command {name} deleted successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => {
            tracing::error!("Failed to delete command: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &if error.is_empty() {
                    "Failed to delete command".to_string()
                } else {
                    error
                },
            )
        }
    }
}
