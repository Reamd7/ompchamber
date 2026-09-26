//! Port of the `opencode/routes.js` remainder (the parts not owned by the
//! settings / core_routes modules): behavior/AGENTS.md endpoints, provider
//! config CRUD, and the MCP OAuth parking + browser-callback routes.

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
const LARGE_BODY_LIMIT: usize = 50 * 1024 * 1024;
/// JS: `express.json({ limit: '1mb' })` for `/api/behavior`.
const BEHAVIOR_BODY_LIMIT: usize = 1024 * 1024;
/// JS: `express.json({ limit: '16kb' })` on `/api/mcp/auth/pending`.
const PENDING_BODY_LIMIT: usize = 16 * 1024;
/// `routes.js` `MAX_BEHAVIOR_PROMPT_SIZE`.
const MAX_BEHAVIOR_PROMPT_SIZE: usize = 1024 * 1024;
/// `PENDING_MCP_AUTH_TTL_MS`.
const PENDING_MCP_AUTH_TTL_MS: u64 = 30 * 60 * 1000;

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
fn legacy_agents_md_path(env: &OpenCodeEnv) -> std::path::PathBuf {
    env.config_dir.join("AGENTS.md")
}

/// `resolveAgentDir`: ask the omp-host (`GET /agent-dir`), fall back to
/// `~/.omp/agent`. Never cached — the agent dir is profile-scoped.
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
pub(crate) async fn parse_json_body_for_entity(
    headers: &HeaderMap,
    body: Body,
) -> Result<Value, Response> {
    parse_json_body(headers, body, LARGE_BODY_LIMIT).await
}

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

fn normalize_pending_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

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

async fn clear_pending_mcp_auth(State(state): State<ModuleState>, uri: Uri) -> Response {
    if let Some(state_value) = first_query_value(&uri, "state")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        state.pending.remove(&state_value);
    }
    Json(json!({ "success": true })).into_response()
}

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
