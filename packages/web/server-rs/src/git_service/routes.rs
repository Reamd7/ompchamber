//! Route surface — port of `server/lib/git/routes.js` (`registerGitRoutes`):
//! every `/api/git/*` path, verb, status code, JSON shape, and error envelope.
//! Handlers mirror the JS control flow exactly, including the soft
//! non-repository payloads on status/check/worktrees and the
//! `X-OMPChamber-Warning` header on worktree-list failures.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post, put};
use serde_json::{Value, json};

use super::identity::{IdentityStorage, discover_git_credentials};
use super::service::GitService;
use super::worktrees as wt;

/// Module-local state (`Router::with_state` before returning).
#[derive(Clone)]
pub struct GitState {
    pub service: Arc<GitService>,
    pub identity: Arc<IdentityStorage>,
    pub hub: Arc<crate::hub::EventHub>,
    pub data_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Request helpers (express query/body semantics)
// ---------------------------------------------------------------------------

/// Express `req.query.<name>` / `resolveDirectoryQuery`: duplicate keys
/// produce an array in express and the JS takes the first element — take the
/// first value here for both shapes.
fn query_first(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) =
                    u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
                {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `resolveDirectoryQuery` — trimmed first value or `None`.
fn resolve_directory_query(query: Option<&str>) -> Option<String> {
    query_first(query, "directory")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// express.json(): a JSON body parses to its value; missing/empty bodies are
/// `undefined` (callers apply `|| {}`); malformed JSON is a 400 like express.
async fn read_json_body(body: Body) -> Result<Value, Response> {
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"))?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"))
}

fn body_object(body: &Value) -> &Value {
    static NULL_VALUE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| Value::Null);
    if body.is_object() { body } else { &NULL_VALUE }
}

fn str_field<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

/// Truthy string field (JS `if (!branch)` rejects empty strings too).
fn truthy_str(body: &Value, field: &str) -> Option<String> {
    str_field(body, field)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn internal_error(message: &str, fallback: &str) -> Response {
    let text = if message.is_empty() {
        fallback.to_string()
    } else {
        message.to_string()
    };
    error_response(StatusCode::INTERNAL_SERVER_ERROR, text)
}

fn is_non_repo_message(message: &str) -> bool {
    message.to_lowercase().contains("not a git repository")
}

fn non_repo_status_payload() -> Value {
    json!({
        "isGitRepository": false,
        "files": [],
        "branch": null,
        "ahead": 0,
        "behind": 0,
    })
}

fn ok_json(value: Value) -> Response {
    Json(value).into_response()
}

fn require_directory(query: Option<&str>) -> Result<String, Response> {
    let Some(directory) = resolve_directory_query(query) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "directory parameter is required",
        ));
    };
    Ok(directory)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn identities_list(State(state): State<GitState>, _req: Request) -> Response {
    let profiles = state.identity.get_profiles();
    ok_json(Value::Array(profiles))
}

async fn identities_create(State(state): State<GitState>, req: Request) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.identity.create_profile(&body) {
        Ok(profile) => ok_json(profile),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

async fn identities_update(
    State(state): State<GitState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    req: Request,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.identity.update_profile(&id, &body) {
        Ok(profile) => ok_json(profile),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

async fn identities_delete(
    State(state): State<GitState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    _req: Request,
) -> Response {
    match state.identity.delete_profile(&id) {
        Ok(_) => ok_json(json!({ "success": true })),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

async fn global_identity(State(state): State<GitState>, _req: Request) -> Response {
    ok_json(state.service.get_global_identity().await)
}

async fn discover_credentials(State(_state): State<GitState>, _req: Request) -> Response {
    ok_json(Value::Array(discover_git_credentials(None)))
}

async fn git_check(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    // JS: a non-repo path answers `{ isGitRepository: false }` (200); a hard
    // failure inside git is the 500 path.
    if state.service.is_git_repository(&directory).await {
        ok_json(json!({ "isGitRepository": true }))
    } else {
        ok_json(json!({ "isGitRepository": false }))
    }
}

async fn remote_url(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let remote = query_first(query, "remote").unwrap_or_else(|| "origin".to_string());
    let url = state.service.get_remote_url(&directory, &remote).await;
    ok_json(json!({ "url": url }))
}

async fn current_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(state.service.get_current_identity(&directory).await)
}

async fn has_local_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let has_local = state.service.has_local_identity(&directory).await;
    ok_json(json!({ "hasLocalIdentity": has_local }))
}

async fn set_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(profile_id) = truthy_str(&body, "profileId") else {
        return error_response(StatusCode::BAD_REQUEST, "profileId is required");
    };

    let profile: Value;
    if profile_id == "global" {
        let identity = state.service.get_global_identity().await;
        let user_name = identity
            .get("userName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let user_email = identity
            .get("userEmail")
            .and_then(Value::as_str)
            .unwrap_or("");
        if user_name.is_empty() || user_email.is_empty() {
            return error_response(StatusCode::NOT_FOUND, "Global identity is not configured");
        }
        let ssh_key = identity
            .get("sshCommand")
            .and_then(Value::as_str)
            .and_then(|command| command.strip_prefix("ssh -i "))
            .map(str::to_string);
        profile = json!({
            "id": "global",
            "name": "Global Identity",
            "userName": user_name,
            "userEmail": user_email,
            "sshKey": ssh_key,
        });
    } else {
        match state.identity.get_profile(&profile_id) {
            Some(found) => profile = found,
            None => return error_response(StatusCode::NOT_FOUND, "Profile not found"),
        }
    }

    match state.service.set_local_identity(&directory, &profile).await {
        Ok(_) => ok_json(json!({ "success": true, "profile": profile })),
        Err(message) => internal_error(&message, "Failed to set git identity"),
    }
}

async fn status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };

    if !state.service.is_git_repository(&directory).await {
        return ok_json(non_repo_status_payload());
    }

    let mode = if query_first(query, "mode").as_deref() == Some("light") {
        Some("light")
    } else {
        None
    };
    match state.service.get_status(&directory, mode).await {
        Ok(value) => ok_json(value),
        Err(message) if is_non_repo_message(&message) => ok_json(non_repo_status_payload()),
        Err(message) => internal_error(&message, "Failed to get git status"),
    }
}

async fn primary_root(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(
        state
            .service
            .resolve_primary_worktree_root(&directory)
            .await,
    )
}

async fn toplevel(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(state.service.resolve_worktree_top_level(&directory).await)
}

async fn commit_summaries(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state
        .service
        .get_commit_summaries(&directory, body.get("shas"))
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

async fn integrate_plan(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "plan", |state, body| {
        Box::pin(
            async move { super::integrate::compute_integrate_plan(&state.service, &body).await },
        )
    })
    .await
}

async fn integrate_conflict_details(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "conflict-details", |state, body| {
        Box::pin(async move {
            super::integrate::get_integrate_conflict_details(
                &state.service,
                body.get("tempWorktreePath"),
            )
            .await
        })
    })
    .await
}

async fn integrate_cherry_pick_status(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "cherry-pick-status", |state, body| {
        Box::pin(async move {
            super::integrate::is_cherry_pick_in_progress(
                &state.service,
                body.get("tempWorktreePath"),
            )
            .await
        })
    })
    .await
}

async fn integrate_run(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "run", |state, body| {
        Box::pin(async move {
            super::integrate::integrate_worktree_commits(
                &state.service,
                body.get("plan").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

async fn integrate_abort(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "abort", |state, body| {
        Box::pin(async move {
            super::integrate::abort_integrate(
                &state.service,
                body.get("state").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

async fn integrate_continue(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "continue", |state, body| {
        Box::pin(async move {
            super::integrate::continue_integrate(
                &state.service,
                body.get("state").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

async fn integrate_action(
    req: Request,
    state: GitState,
    action: &str,
    handler: impl FnOnce(GitState, Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
    + Send,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match handler(state, body).await {
        Ok(value) => ok_json(value),
        Err(message) => {
            let fallback = format!("Failed to run git integrate {}", action);
            let text = if message.is_empty() {
                fallback
            } else {
                message
            };
            error_response(StatusCode::BAD_REQUEST, text)
        }
    }
}

async fn diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let staged = query_first(query, "staged").as_deref() == Some("true");
    let context_lines = query_first(query, "context")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(3);

    match state
        .service
        .get_diff(&directory, Some(&path), staged, Some(context_lines))
        .await
    {
        Ok(diff) => ok_json(json!({ "diff": diff })),
        Err(message) => internal_error(&message, "Failed to get git diff"),
    }
}

async fn file_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let staged = query_first(query, "staged").as_deref() == Some("true");

    match state.service.get_file_diff(&directory, &path, staged).await {
        Ok(result) => ok_json(json!({
            "original": result.get("original").cloned().unwrap_or(Value::Null),
            "modified": result.get("modified").cloned().unwrap_or(Value::Null),
            "path": result.get("path").cloned().unwrap_or(Value::Null),
            "isBinary": result.get("isBinary") == Some(&Value::Bool(true)),
        })),
        Err(message) => internal_error(&message, "Failed to get git file diff"),
    }
}

async fn range_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let base = query_first(query, "base");
    let head = query_first(query, "head");
    if base.as_deref().map(str::is_empty).unwrap_or(true)
        || head.as_deref().map(str::is_empty).unwrap_or(true)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "base and head parameters are required",
        );
    }
    let path = query_first(query, "path").filter(|p| !p.is_empty());
    let context_lines = query_first(query, "context")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(3);

    match state
        .service
        .get_range_diff(
            &directory,
            base.as_deref().unwrap(),
            head.as_deref().unwrap(),
            path.as_deref(),
            context_lines,
        )
        .await
    {
        Ok(diff) => ok_json(json!({ "diff": diff })),
        Err(message) => internal_error(&message, "Failed to get git range diff"),
    }
}

async fn branch_base(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(branch) = resolve_directory_query_named(query, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch parameter is required");
    };
    match state.service.get_branch_base(&directory, &branch).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branch base"),
    }
}

fn resolve_directory_query_named(query: Option<&str>, name: &str) -> Option<String> {
    query_first(query, name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn range_files(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let base = resolve_directory_query_named(query, "base");
    let head = resolve_directory_query_named(query, "head");
    if base.is_none() || head.is_none() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "base and head parameters are required",
        );
    }
    match state
        .service
        .get_range_files(
            &directory,
            base.as_deref().unwrap(),
            head.as_deref().unwrap(),
        )
        .await
    {
        Ok(files) => ok_json(json!({ "files": files })),
        Err(message) => internal_error(&message, "Failed to get git range files"),
    }
}

async fn revert(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(path) = truthy_str(body, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let scope = str_field(body, "scope");
    match state.service.revert_file(&directory, &path, scope).await {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(&message, "Failed to revert git file"),
    }
}

async fn stage(State(state): State<GitState>, req: Request) -> Response {
    stage_unstage(state, req, true).await
}

async fn unstage(State(state): State<GitState>, req: Request) -> Response {
    stage_unstage(state, req, false).await
}

async fn stage_unstage(state: GitState, req: Request, staging: bool) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let file_paths: Vec<Value> = match body.get("paths").and_then(Value::as_array) {
        Some(paths) => paths.clone(),
        None => vec![body.get("path").cloned().unwrap_or(Value::Null)],
    };
    let has_valid = file_paths.iter().any(|value| {
        value
            .as_str()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    });
    if !has_valid {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    }

    let outcome = if staging {
        state.service.stage_files(&directory, &file_paths).await
    } else {
        state.service.unstage_files(&directory, &file_paths).await
    };
    match outcome {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(
            &message,
            if staging {
                "Failed to stage git file"
            } else {
                "Failed to unstage git file"
            },
        ),
    }
}

async fn apply_hunk(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(file_path) = truthy_str(body, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let Some(patch) = str_field(body, "patch").filter(|p| !p.trim().is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "patch is required");
    };
    let action = str_field(body, "action").unwrap_or("");
    if !matches!(action, "stage" | "unstage" | "discard") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "action must be stage, unstage, or discard",
        );
    }

    match state
        .service
        .apply_hunk(&directory, &file_path, patch, action)
        .await
    {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(&message, "Failed to apply git hunk"),
    }
}

async fn pull(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.pull(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to pull from remote"),
    }
}

async fn push(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.push(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to push to remote"),
    }
}

async fn stashes_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.list_stashes(&directory).await {
        Ok(stashes) => ok_json(json!({ "stashes": stashes })),
        Err(message) => internal_error(&message, "Failed to list stashes"),
    }
}

async fn stashes_file_counts(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state
        .service
        .count_stash_files(&directory, body.get("refs"))
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to count stash files"),
    }
}

async fn stash_push_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_push(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to stash changes"),
    }
}

async fn stash_apply_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_apply(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to apply stash"),
    }
}

async fn stash_pop_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_pop(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to pop stash"),
    }
}

async fn stash_drop_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_drop(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to drop stash"),
    }
}

async fn fetch_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.fetch(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to fetch from remote"),
    }
}

async fn remotes_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_remotes(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get remotes"),
    }
}

async fn remotes_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let remote = str_field(body, "remote").unwrap_or("").trim().to_string();
    if remote.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "remote is required");
    }
    match state.service.remove_remote(&directory, &remote).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to remove remote"),
    }
}

async fn rebase_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.rebase(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to rebase"),
    }
}

async fn rebase_abort(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.abort_rebase(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to abort rebase"),
    }
}

async fn merge_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.merge(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to merge"),
    }
}

async fn merge_abort(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.abort_merge(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to abort merge"),
    }
}

async fn rebase_continue(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.continue_rebase(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to continue rebase"),
    }
}

async fn merge_continue(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.continue_merge(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to continue merge"),
    }
}

async fn conflict_details(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_conflict_details(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get conflict details"),
    }
}

async fn commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(message) = truthy_str(body, "message") else {
        return error_response(StatusCode::BAD_REQUEST, "message is required");
    };
    match state.service.commit(&directory, &message, body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create commit"),
    }
}

async fn branches_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_branches(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branches"),
    }
}

async fn branch_push_status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branches) = body.get("branches").and_then(Value::as_array) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "branches must be an array of branch names",
        );
    };
    if branches.iter().any(|branch| !branch.is_string()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "branches must be an array of branch names",
        );
    }
    match state
        .service
        .get_unpushed_branch_counts(&directory, branches)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branch push status"),
    }
}

async fn branches_create(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(name) = truthy_str(body, "name") else {
        return error_response(StatusCode::BAD_REQUEST, "name is required");
    };
    let start_point = str_field(body, "startPoint").filter(|s| !s.is_empty());
    match state
        .service
        .create_branch(&directory, &name, start_point)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create branch"),
    }
}

async fn branches_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branch) = truthy_str(body, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    };
    let force = body.get("force") == Some(&Value::Bool(true));
    match state
        .service
        .delete_branch(&directory, &branch, force)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to delete branch"),
    }
}

async fn branches_rename(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(old_name) = truthy_str(body, "oldName") else {
        return error_response(StatusCode::BAD_REQUEST, "oldName is required");
    };
    let Some(new_name) = truthy_str(body, "newName") else {
        return error_response(StatusCode::BAD_REQUEST, "newName is required");
    };
    match state
        .service
        .rename_branch(&directory, &old_name, &new_name)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to rename branch"),
    }
}

async fn remote_branches_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    if truthy_str(body, "branch").is_none() {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    }
    match state.service.delete_remote_branch(&directory, body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to delete remote branch"),
    }
}
async fn worktrees_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    // `getWorktrees` swallows non-repo/git failures and answers `[]`; the JS
    // route adds the warning header when even that fails, which cannot happen
    // with the swallowing implementation — kept for the rare error shape.
    let worktrees = wt::get_worktrees(&state.service, &directory).await;
    ok_json(Value::Array(worktrees))
}

async fn worktrees_validate(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    ok_json(wt::validate_worktree_create(&state.service, &directory, &body).await)
}

async fn checkout_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branch) = truthy_str(body, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    };
    match state.service.checkout_branch(&directory, &branch).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to checkout branch"),
    }
}

fn invalid_hash_response() -> Response {
    error_response(StatusCode::BAD_REQUEST, "Invalid commit hash")
}

async fn checkout_commit(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.checkout_commit(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to checkout commit"),
    }
}

async fn cherry_pick_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.cherry_pick(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to cherry-pick"),
    }
}

async fn revert_commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.revert_commit(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to revert commit"),
    }
}

async fn reset_to_commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    let mode = str_field(body, "mode").unwrap_or("");
    if !matches!(mode, "soft" | "mixed" | "hard") {
        return error_response(StatusCode::BAD_REQUEST, "mode must be soft, mixed, or hard");
    }
    let force = body.get("force") == Some(&Value::Bool(true));
    match state
        .service
        .reset_to_commit(&directory, hash, mode, force)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to reset"),
    }
}

async fn worktrees_create(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match wt::create_worktree(&state.service, &directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create worktree"),
    }
}

async fn worktrees_preview(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match wt::preview_worktree_create(&state.service, &directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to preview worktree"),
    }
}

async fn worktrees_bootstrap_status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match wt::get_worktree_bootstrap_status(&state.service, &directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get worktree bootstrap status"),
    }
}

async fn worktrees_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let worktree_directory = str_field(body, "directory").unwrap_or("");
    if worktree_directory.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "worktree directory is required");
    }
    let input = json!({
        "directory": worktree_directory,
        "deleteLocalBranch": body.get("deleteLocalBranch") == Some(&Value::Bool(true)),
    });
    match wt::remove_worktree(&state.service, &directory, &input).await {
        Ok(result) => ok_json(json!({ "success": result })),
        Err(message) => internal_error(&message, "Failed to remove worktree"),
    }
}

async fn worktree_type(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let linked = state.service.is_linked_worktree(&directory).await;
    ok_json(json!({ "linked": linked }))
}

async fn validate_directory_route(State(state): State<GitState>, req: Request) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(directory) = truthy_str(body, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    };
    let Some(worktree_root) = truthy_str(body, "worktreeRoot") else {
        return error_response(StatusCode::BAD_REQUEST, "worktreeRoot is required");
    };
    ok_json(
        state
            .service
            .validate_worktree_directory(&directory, &worktree_root)
            .await,
    )
}

async fn canonicalize_worktree_state_route(
    State(state): State<GitState>,
    req: Request,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(directory) = truthy_str(body, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    };
    ok_json(state.service.canonicalize_worktree_state(&directory).await)
}

async fn log_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let options = super::service_ops::LogOptions {
        max_count: query_first(query, "maxCount").and_then(|value| value.parse::<i64>().ok()),
        from: query_first(query, "from"),
        to: query_first(query, "to"),
        file: query_first(query, "file"),
        all: query_first(query, "all").as_deref() == Some("true"),
    };
    match state.service.get_log(&directory, &options).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit log"),
    }
}

async fn commit_files(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(hash) = query_first(query, "hash") else {
        return error_response(StatusCode::BAD_REQUEST, "hash parameter is required");
    };
    match state.service.get_commit_files(&directory, &hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit files"),
    }
}

async fn commit_file_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(hash) = query_first(query, "hash") else {
        return error_response(StatusCode::BAD_REQUEST, "hash parameter is required");
    };
    if !super::paths::is_valid_commit_hash(&hash) {
        return error_response(StatusCode::BAD_REQUEST, "hash must be a valid commit SHA");
    }
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let is_binary = query_first(query, "binary").as_deref() == Some("true");
    match state
        .service
        .get_commit_file_diff(&directory, &hash, &path, is_binary)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit file diff"),
    }
}

// ---------------------------------------------------------------------------
// Router assembly
// ---------------------------------------------------------------------------

pub fn routes() -> Router<GitState> {
    Router::new()
        .route(
            "/api/git/identities",
            get(identities_list).post(identities_create),
        )
        .route(
            "/api/git/identities/{id}",
            put(identities_update).delete(identities_delete),
        )
        .route("/api/git/global-identity", get(global_identity))
        .route("/api/git/discover-credentials", get(discover_credentials))
        .route("/api/git/check", get(git_check))
        .route("/api/git/remote-url", get(remote_url))
        .route("/api/git/current-identity", get(current_identity))
        .route("/api/git/has-local-identity", get(has_local_identity))
        .route("/api/git/set-identity", post(set_identity))
        .route("/api/git/status", get(status))
        .route("/api/git/primary-root", get(primary_root))
        .route("/api/git/toplevel", get(toplevel))
        .route("/api/git/commit-summaries", post(commit_summaries))
        .route("/api/git/integrate/plan", post(integrate_plan))
        .route(
            "/api/git/integrate/conflict-details",
            post(integrate_conflict_details),
        )
        .route(
            "/api/git/integrate/cherry-pick-status",
            post(integrate_cherry_pick_status),
        )
        .route("/api/git/integrate/run", post(integrate_run))
        .route("/api/git/integrate/abort", post(integrate_abort))
        .route("/api/git/integrate/continue", post(integrate_continue))
        .route("/api/git/diff", get(diff))
        .route("/api/git/file-diff", get(file_diff))
        .route("/api/git/range-diff", get(range_diff))
        .route("/api/git/branch-base", get(branch_base))
        .route("/api/git/range-files", get(range_files))
        .route("/api/git/revert", post(revert))
        .route("/api/git/stage", post(stage))
        .route("/api/git/unstage", post(unstage))
        .route("/api/git/apply-hunk", post(apply_hunk))
        .route("/api/git/pull", post(pull))
        .route("/api/git/push", post(push))
        .route("/api/git/stashes", get(stashes_list))
        .route("/api/git/stashes/file-counts", post(stashes_file_counts))
        .route("/api/git/stash", post(stash_push_route))
        .route("/api/git/stash/apply", post(stash_apply_route))
        .route("/api/git/stash/pop", post(stash_pop_route))
        .route("/api/git/stash/drop", post(stash_drop_route))
        .route("/api/git/fetch", post(fetch_route))
        .route("/api/git/remotes", get(remotes_list).delete(remotes_delete))
        .route("/api/git/rebase", post(rebase_route))
        .route("/api/git/rebase/abort", post(rebase_abort))
        .route("/api/git/rebase/continue", post(rebase_continue))
        .route("/api/git/merge", post(merge_route))
        .route("/api/git/merge/abort", post(merge_abort))
        .route("/api/git/merge/continue", post(merge_continue))
        .route("/api/git/conflict-details", get(conflict_details))
        .route("/api/git/commit", post(commit_route))
        .route(
            "/api/git/branches",
            get(branches_list)
                .post(branches_create)
                .delete(branches_delete),
        )
        .route("/api/git/branches/rename", put(branches_rename))
        .route("/api/git/branch-push-status", post(branch_push_status))
        .route("/api/git/remote-branches", delete(remote_branches_delete))
        .route("/api/git/checkout", post(checkout_route))
        .route("/api/git/checkout-commit", post(checkout_commit))
        .route("/api/git/cherry-pick", post(cherry_pick_route))
        .route("/api/git/revert-commit", post(revert_commit_route))
        .route("/api/git/reset-to-commit", post(reset_to_commit_route))
        .route(
            "/api/git/worktrees",
            get(worktrees_list)
                .post(worktrees_create)
                .delete(worktrees_delete),
        )
        .route("/api/git/worktrees/validate", post(worktrees_validate))
        .route("/api/git/worktrees/preview", post(worktrees_preview))
        .route(
            "/api/git/worktrees/bootstrap-status",
            get(worktrees_bootstrap_status),
        )
        .route("/api/git/worktree-type", get(worktree_type))
        .route(
            "/api/git/validate-directory",
            post(validate_directory_route),
        )
        .route(
            "/api/git/canonicalize-worktree-state",
            post(canonicalize_worktree_state_route),
        )
        .route("/api/git/log", get(log_route))
        .route("/api/git/commit-files", get(commit_files))
        .route("/api/git/commit-file-diff", get(commit_file_diff))
}
