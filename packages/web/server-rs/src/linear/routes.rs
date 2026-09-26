//! Port of `server/lib/linear/routes.js` — the public OAuth callback page and
//! the `/api/linear/*` endpoints. JSON bodies on these routes use a 16kb
//! limit (they are not on the server-wide `/api` 50mb allowlist).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};

use super::auth::SetLinearAuthInput;
use super::client::{fetch_linear_identity, get_valid_linear_access_token};
use super::issues::{
    ListIssuesParams, get_linear_issue, list_linear_issue_states, list_linear_issues,
    update_linear_issue,
};
use super::mapping::{merge_linear_mapping_view, teams_from_json};
use super::oauth::AuthOrigin;
use super::parse::read_trimmed_string;
use super::status::{SessionStatusInput, post_linear_session_status};
use super::teams::list_linear_teams;
use super::{LinearError, LinearState};

/// JS `express.json({ limit: '16kb' })`.
const PENDING_JSON_LIMIT: usize = 16 * 1024;

pub fn router(state: Arc<LinearState>) -> axum::Router {
    axum::Router::new()
        .route("/linear/oauth/callback", get(oauth_callback))
        .route("/api/linear/auth/status", get(auth_status))
        .route("/api/linear/auth/start", post(auth_start))
        .route("/api/linear/auth/activate", post(auth_activate))
        .route("/api/linear/auth", delete(auth_delete))
        .route("/api/linear/issues/list", get(issues_list))
        .route("/api/linear/issues/get", get(issues_get))
        .route("/api/linear/issues/states", get(issues_states))
        .route("/api/linear/issues/update", post(issues_update))
        .route("/api/linear/mapping", get(mapping_get).put(mapping_put))
        .route("/api/linear/session-status", post(session_status))
        .route(
            "/api/linear/preferences",
            get(preferences_get).put(preferences_put),
        )
        .with_state(state)
}
/// JS `queryValue`: first value of a repeated query parameter wins.
fn query_value(raw_query: &str, key: &str) -> String {
    for (name, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        if name == key {
            return value.into_owned();
        }
    }
    String::new()
}

/// JS `isLinearUserError`.
fn is_linear_user_error(error: &LinearError) -> bool {
    error.code.as_deref() == Some("INVALID") || error.user_error
}

fn error_message_or(error: &LinearError, fallback: &str) -> String {
    if error.message.is_empty() {
        fallback.to_string()
    } else {
        error.message.clone()
    }
}

/// JS `storeAuthorizationResult`: best-effort identity lookup, then persist.
/// A persistence failure propagates to the route's error path.
async fn store_authorization_result(
    state: &Arc<LinearState>,
    result: &super::oauth::AuthorizationResult,
) -> Result<super::auth::LinearAuthEntry, LinearError> {
    let identity = match fetch_linear_identity(state, &result.tokens.access_token).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::error!("Failed to load Linear identity after OAuth: {error}");
            super::client::Identity::default()
        }
    };
    state.set_auth(
        SetLinearAuthInput {
            access_token: result.tokens.access_token.clone(),
            refresh_token: Some(result.tokens.refresh_token.clone()),
            token_type: Some(result.tokens.token_type.clone()),
            expires_at: Some(result.tokens.expires_at),
            scope: Some(result.tokens.scope.clone()),
            user: Some(identity.user),
            organization: Some(identity.organization),
            workspace_id: None,
        },
        true,
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// JS `renderLinearOAuthCallbackPage`.
fn render_linear_oauth_callback_page(title: &str, message: &str, desktop_return: bool) -> String {
    let desktop = if desktop_return {
        "<a class=\"return\" href=\"openchamber://focus/linear-auth\">Return to OpenChamber</a>\n<script>window.location.href = 'openchamber://focus/linear-auth';</script>"
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — OpenChamber</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
         font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
         background: Canvas; color: CanvasText; }}
  main {{ max-width: 34rem; padding: 2.5rem 2rem; text-align: center; }}
  h1 {{ font-size: 1.25rem; margin: 0 0 0.75rem; }}
  p {{ margin: 0; line-height: 1.5; opacity: 0.85; }}
  a.return {{ display: inline-block; margin-top: 1.5rem; padding: 0.5rem 1.25rem; border-radius: 0.5rem;
             border: 1px solid color-mix(in srgb, CanvasText 25%, transparent); color: inherit; text-decoration: none; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
<p>{message}</p>
{desktop}
</main>
</body>
</html>"#,
        title = escape_html(title),
        message = escape_html(message),
        desktop = desktop,
    )
}

fn html_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /linear/oauth/callback
// ---------------------------------------------------------------------------

async fn oauth_callback(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let raw_query = request.uri().query().unwrap_or_default().to_string();
    let query = super::oauth::CallbackQuery {
        code: query_value(&raw_query, "code"),
        state: query_value(&raw_query, "state"),
        error: query_value(&raw_query, "error"),
        error_description: query_value(&raw_query, "error_description"),
    };
    let outcome = match state.consume_authorization_callback(&query).await {
        Ok(result) => {
            let desktop_return = result.origin == AuthOrigin::Desktop;
            store_authorization_result(&state, &result)
                .await
                .map(|_| desktop_return)
        }
        Err(error) => Err(error),
    };
    match outcome {
        Ok(desktop_return) => html_response(
            StatusCode::OK,
            render_linear_oauth_callback_page(
                "Authorization Complete",
                "You can close this tab and return to OpenChamber.",
                desktop_return,
            ),
        ),
        Err(error) => {
            let status = match error.code.as_deref() {
                Some("UNKNOWN_STATE") | Some("MISSING_CODE") | Some("ACCESS_DENIED") => {
                    StatusCode::BAD_REQUEST
                }
                _ => StatusCode::BAD_GATEWAY,
            };
            html_response(
                status,
                render_linear_oauth_callback_page(
                    "Authorization Failed",
                    &error_message_or(
                        &error,
                        "Linear authorization failed. Return to OpenChamber and click Connect again.",
                    ),
                    error.origin == Some(AuthOrigin::Desktop),
                ),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/linear/auth/status
// ---------------------------------------------------------------------------

async fn auth_status(State(state): State<Arc<LinearState>>) -> Response {
    match state.poll_authorization_broker().await {
        Ok(Some(result)) => {
            if let Err(error) = store_authorization_result(&state, &result).await {
                tracing::error!("Failed to complete Linear authorization through broker: {error}");
            } else if let Some(receipt) = result.broker_receipt.as_ref()
                && let Err(error) = state.complete_authorization_broker(receipt).await
            {
                tracing::warn!("Failed to acknowledge Linear authorization broker result: {error}");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!("Failed to complete Linear authorization through broker: {error}");
        }
    }

    let access_token = match get_valid_linear_access_token(&state, None).await {
        Ok(Some(token)) => token,
        Ok(None) => return Json(json!({ "connected": false })).into_response(),
        Err(error) => return auth_status_failure(&error),
    };

    let auth = state.get_auth();
    match fetch_linear_identity(&state, &access_token).await {
        Ok(identity) => match state.set_auth(
            SetLinearAuthInput {
                access_token: access_token.clone(),
                refresh_token: Some(auth.as_ref().and_then(|entry| entry.refresh_token.clone())),
                token_type: auth.as_ref().map(|entry| entry.token_type.clone()),
                expires_at: auth.as_ref().and_then(|entry| entry.expires_at),
                scope: auth.as_ref().map(|entry| entry.scope.clone()),
                user: Some(identity.user),
                organization: Some(identity.organization),
                workspace_id: auth.as_ref().map(|entry| entry.workspace_id.clone()),
            },
            false,
        ) {
            Ok(next) => Json(state.to_public_status(Some(&next), None)).into_response(),
            Err(error) => auth_status_failure(&error),
        },
        Err(error) => {
            if error.http_status() == Some(401) {
                state.clear_auth(auth.as_ref().map(|entry| entry.workspace_id.as_str()));
                let remaining = state.get_auth();
                if remaining.is_none() {
                    return Json(json!({ "connected": false })).into_response();
                }
                return Json(state.to_public_status(remaining.as_ref(), None)).into_response();
            }
            if let Some(auth) = auth {
                return Json(state.to_public_status(Some(&auth), None)).into_response();
            }
            auth_status_failure(&error)
        }
    }
}

fn auth_status_failure(error: &LinearError) -> Response {
    tracing::error!("Failed to get Linear auth status: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": error_message_or(error, "Failed to get Linear auth status") })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/linear/auth/start
// ---------------------------------------------------------------------------

async fn auth_start(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let origin = if parsed["origin"] == json!("desktop") {
        "desktop"
    } else {
        "web"
    };
    match state.start_authorization(origin).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => {
            let status = if error.code.as_deref() == Some("LINEAR_CLIENT_ID_MISSING") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::error!("Failed to start Linear authorization: {error}");
            (
                status,
                Json(json!({ "error": error_message_or(&error, "Failed to start Linear authorization") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/linear/issues/*
// ---------------------------------------------------------------------------

async fn issues_list(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let raw_query = request.uri().query().unwrap_or_default().to_string();
    let params = ListIssuesParams {
        query: query_value(&raw_query, "query"),
        cursor: query_value(&raw_query, "cursor"),
        status: query_value(&raw_query, "status"),
        assignee: query_value(&raw_query, "assignee"),
        team_id: query_value(&raw_query, "teamId"),
        priority: query_value(&raw_query, "priority"),
    };
    match list_linear_issues(&state, &params).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => issues_failure(&error, "Failed to list Linear issues"),
    }
}

fn issues_failure(error: &LinearError, fallback: &str) -> Response {
    if is_linear_user_error(error) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.message })),
        )
            .into_response();
    }
    tracing::error!("{fallback}: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": error_message_or(error, fallback) })),
    )
        .into_response()
}

async fn issues_get(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let raw_query = request.uri().query().unwrap_or_default().to_string();
    let id = query_value(&raw_query, "id");
    if id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "id is required" })),
        )
            .into_response();
    }
    match get_linear_issue(&state, &id).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => issues_failure(&error, "Failed to load Linear issue"),
    }
}

async fn issues_states(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let raw_query = request.uri().query().unwrap_or_default().to_string();
    let team_id = query_value(&raw_query, "teamId");
    if team_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "teamId is required" })),
        )
            .into_response();
    }
    match list_linear_issue_states(&state, &team_id).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => issues_failure(&error, "Failed to load Linear workflow states"),
    }
}

async fn issues_update(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let id = parsed["id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_default();
    let state_id = parsed["stateId"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_default();
    match update_linear_issue(&state, &id, &state_id).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => issues_failure(&error, "Failed to update Linear issue"),
    }
}

// ---------------------------------------------------------------------------
// GET/PUT /api/linear/mapping
// ---------------------------------------------------------------------------

async fn mapping_get(State(state): State<Arc<LinearState>>) -> Response {
    let teams_result = match list_linear_teams(&state).await {
        Ok(result) => result,
        Err(error) => return mapping_failure(&error, "Failed to load Linear mapping"),
    };
    if teams_result["connected"] == json!(false) {
        return Json(json!({ "connected": false })).into_response();
    }
    let stored = match state.read_stored_mapping() {
        Ok(stored) => stored,
        Err(error) if error.code.as_deref() == Some("MALFORMED") => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.message })),
            )
                .into_response();
        }
        Err(error) => return mapping_failure(&error, "Failed to load Linear mapping"),
    };
    let teams = teams_from_json(&teams_result["teams"]);
    let mut payload = json!({ "connected": true });
    let view = merge_linear_mapping_view(Some(&stored), &teams);
    let view_json = view.to_json();
    if let Value::Object(fields) = view_json {
        for (key, value) in fields {
            payload[key] = value;
        }
    }
    Json(payload).into_response()
}

fn mapping_failure(error: &LinearError, fallback: &str) -> Response {
    tracing::error!("{fallback}: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": error_message_or(error, fallback) })),
    )
        .into_response()
}

async fn mapping_put(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    match get_valid_linear_access_token(&state, None).await {
        Ok(Some(_token)) => {}
        Ok(None) => return Json(json!({ "connected": false })).into_response(),
        Err(error) => return mapping_failure(&error, "Failed to save Linear mapping"),
    }
    let stored = match state.set_stored_mapping(&parsed) {
        Ok(stored) => stored,
        Err(error) if error.code.as_deref() == Some("INVALID") => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.message })),
            )
                .into_response();
        }
        Err(error) => return mapping_failure(&error, "Failed to save Linear mapping"),
    };
    let teams_result = match list_linear_teams(&state).await {
        Ok(result) => result,
        Err(error) => return mapping_failure(&error, "Failed to save Linear mapping"),
    };
    let teams = if teams_result["connected"] == json!(false) {
        Vec::new()
    } else {
        teams_from_json(&teams_result["teams"])
    };
    let mut payload = json!({ "connected": true });
    let view = merge_linear_mapping_view(Some(&stored), &teams);
    let view_json = view.to_json();
    if let Value::Object(fields) = view_json {
        for (key, value) in fields {
            payload[key] = value;
        }
    }
    Json(payload).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/linear/session-status
// ---------------------------------------------------------------------------

async fn session_status(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let input = SessionStatusInput {
        kind: parsed["kind"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default(),
        session_id: parsed["sessionId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default(),
        issue_identifier: parsed["issueIdentifier"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default(),
        session_origin: parsed["sessionOrigin"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default(),
        organization_id: parsed["organizationId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default(),
    };
    match post_linear_session_status(&state, &input).await {
        Ok(result) => Json(result).into_response(),
        Err(error) if error.code.as_deref() == Some("INVALID") => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.message })),
        )
            .into_response(),
        Err(error) if error.code.as_deref() == Some("MALFORMED") => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.message })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!("Failed to post Linear session status: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error_message_or(&error, "Failed to post Linear session status") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// GET/PUT /api/linear/preferences
// ---------------------------------------------------------------------------

async fn preferences_get(State(state): State<Arc<LinearState>>) -> Response {
    Json(json!({ "sessionComments": state.session_comments_enabled() })).into_response()
}

async fn preferences_put(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let session_comments = match &parsed["sessionComments"] {
        Value::Bool(value) => *value,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "sessionComments must be a boolean" })),
            )
                .into_response();
        }
    };
    Json(json!({ "sessionComments": state.set_session_comments_enabled(session_comments) }))
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/linear/auth/activate + DELETE /api/linear/auth
// ---------------------------------------------------------------------------

async fn auth_activate(State(state): State<Arc<LinearState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let parsed = match parse_json_body(parts.headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let organization_id = read_trimmed_string(&parsed["organizationId"]);
    if organization_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "organizationId is required" })),
        )
            .into_response();
    }
    if !state.activate_auth(&organization_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Linear workspace not found" })),
        )
            .into_response();
    }
    let auth = state.get_auth();
    let Some(auth) = auth else {
        return Json(json!({ "connected": false })).into_response();
    };
    Json(state.to_public_status(Some(&auth), None)).into_response()
}

async fn auth_delete(State(state): State<Arc<LinearState>>) -> Response {
    let auth = state.get_auth();
    if let Some(auth) = &auth {
        if auth.refresh_token.is_some() {
            state
                .revoke_token(
                    auth.refresh_token.as_deref().unwrap_or_default(),
                    "refresh_token",
                )
                .await;
        } else {
            state.revoke_token(&auth.access_token, "access_token").await;
        }
    }
    let removed = state.clear_auth(auth.as_ref().map(|entry| entry.workspace_id.as_str()));
    Json(json!({ "success": true, "removed": removed })).into_response()
}

/// Mirror `express.json({ limit: '16kb' })`: JSON content types parse (empty
/// body becomes `{}`), anything else stays undefined, malformed JSON is a 400,
/// and an over-limit body is a 413.
async fn parse_json_body(headers: HeaderMap, body: Body) -> Result<Value, Response> {
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
        // express.json skips the request entirely → `req.body ?? {}`.
        return Ok(Value::Null);
    }

    let bytes = match axum::body::to_bytes(body, PENDING_JSON_LIMIT).await {
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
        // body-parser: an empty JSON body yields `{}`.
        return Ok(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => Ok(value),
        Err(err) => Err((StatusCode::BAD_REQUEST, err.to_string()).into_response()),
    }
}
