//! Port of `server/lib/opencode/skill-routes.js` — the skill configuration
//! HTTP surface: listing/discovery (local scan + engine `GET /skill`
//! passthrough merge), per-skill sources, supporting files, CRUD + rename,
//! and the catalog scan/install endpoints (delegated to the
//! `skills_catalog` module).
//!
//! Divergence note: the JS PATCH rename path calls an undeclared
//! `refreshOpenCodeAfterConfigChange` free variable (a latent ReferenceError
//! that 500s a *successful* rename); this port implements the intended
//! behavior — rename, then answer the reload response. The refresh hook
//! itself is a logged no-op until the lifecycle module exposes it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use super::http_util::{
    QueryValues, build_deferred_restart_response, is_env_flag_enabled, parse_query, read_json_body,
    resolve_optional_project_directory,
};
use super::skills::{
    AccessError, BUILT_IN_SKILL_LOCATION, DiscoveredSkill, create_skill, delete_skill,
    delete_skill_supporting_file, discover_skills, get_skill_sources, is_managed_skill_path,
    merge_discovered_skills, read_skill_supporting_file, rename_skill, skill_dir, update_skill,
    write_skill_supporting_file,
};
use crate::context::RouterContext;
use crate::skills_catalog::{
    CatalogError, CuratedSource, GitIdentity, InstallParams, InstallSelection, ScanParams,
    fetch_github_repo_metas, get_cache_key, get_curated_skills_sources,
    install_skills_from_repository, parse_skill_repo_source, scan_skills_repository,
    scan_with_cache,
};

/// `CLIENT_RELOAD_DELAY_MS` from `server/index.js`.
const CLIENT_RELOAD_DELAY_MS: u64 = 800;
const ENGINE_SKILLS_TIMEOUT: Duration = Duration::from_millis(8_000);
const SKILL_SCOPE_PROJECT: &str = "project";

pub(crate) fn routes(ctx: RouterContext) -> Router {
    Router::new()
        .route("/api/config/skills", get(list_skills))
        .route("/api/config/skills/catalog", get(catalog_list))
        .route("/api/config/skills/catalog/source", get(catalog_source))
        .route("/api/config/skills/scan", post(scan_repo))
        .route("/api/config/skills/install", post(install_repo))
        .route(
            "/api/config/skills/{name}",
            get(get_skill)
                .post(create_skill_route)
                .patch(patch_skill)
                .delete(delete_skill_route),
        )
        .route(
            "/api/config/skills/{name}/files/{*file_path}",
            get(get_skill_file)
                .put(put_skill_file)
                .delete(delete_skill_file),
        )
        .with_state(ctx)
}

fn error_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    error_response(status, json!({ "error": message }))
}

fn home_dir() -> PathBuf {
    crate::config::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `resolveSkillsDirectory` — optional first, then the active-project/last
/// directory fallback; fallback errors are ignored (user-scoped listing
/// without a project is valid).
async fn resolve_skills_directory(
    headers: &HeaderMap,
    query: &QueryValues,
) -> Result<Option<PathBuf>, String> {
    let optional = resolve_optional_project_directory(headers, query)?;
    if optional.is_some() {
        return Ok(optional);
    }
    let fallback = super::http_util::resolve_project_directory(headers, query).await;
    Ok(fallback.directory)
}

/// `fetchOpenCodeDiscoveredSkills` — engine `GET /skill` passthrough.
async fn fetch_engine_skills(
    ctx: &RouterContext,
    directory: Option<&Path>,
) -> Vec<DiscoveredSkill> {
    let Some(base) = ctx.engine.base_url() else {
        return Vec::new();
    };
    let base = base.trim_end_matches('/');
    let mut url = format!("{base}/skill");
    if let Some(directory) = directory {
        url.push_str("?directory=");
        url.push_str(&super::http_util::encode_uri_component(
            &directory.to_string_lossy(),
        ));
    }
    let mut request = ctx
        .engine
        .http()
        .get(&url)
        .header("accept", "application/json")
        .timeout(ENGINE_SKILLS_TIMEOUT);
    if let Some(auth) = ctx.engine.auth_header() {
        request = request.header("authorization", auth);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!("Failed to list OpenCode skills: {error}");
            return Vec::new();
        }
    };
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!("Failed to list OpenCode skills: {error}");
            return Vec::new();
        }
    };
    let Value::Array(rows) = payload else {
        return Vec::new();
    };
    let home = home_dir();
    rows.into_iter()
        .filter_map(|row| {
            let name = row.get("name").and_then(Value::as_str)?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let location = row
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if location.is_empty() {
                return None;
            }
            let description = row
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let content = row
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if location == BUILT_IN_SKILL_LOCATION {
                return Some(DiscoveredSkill {
                    name,
                    path: Some(location),
                    scope: Some("user".into()),
                    source: Some("opencode".into()),
                    description: Some(description),
                    content: Some(content),
                });
            }
            let (scope, source) =
                infer_skill_scope_and_source_from_path(&location, directory, &home);
            let content = (!content.is_empty()).then_some(content);
            Some(DiscoveredSkill {
                name,
                path: Some(location),
                scope: Some(scope),
                source: Some(source),
                description: Some(description),
                content,
            })
        })
        .collect()
}

/// `inferSkillScopeAndSourceFromPath` — `.omp/skills`-aware project/user
/// classification used for engine-reported locations.
fn infer_skill_scope_and_source_from_path(
    skill_path: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> (String, String) {
    let resolved = super::http_util::resolve_path(skill_path);
    let resolved_display = resolved.to_string_lossy().into_owned();
    let source = if resolved_display.contains("/.agents/skills/") {
        "agents"
    } else if resolved_display.contains("/.claude/skills/") {
        "claude"
    } else {
        "opencode"
    };

    // Locations may be reported lexically while directories resolve through
    // symlinks (macOS `/var` → `/private/var`); compare both forms.
    let canonical_display = std::fs::canonicalize(&resolved)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved_display.clone());
    if let Some(working_directory) = working_directory {
        let mut current = super::http_util::resolve_path(&working_directory.to_string_lossy());
        let mut stop =
            super::skills::find_worktree_root(working_directory).unwrap_or_else(|| current.clone());
        stop = super::http_util::resolve_path(&stop.to_string_lossy());
        loop {
            let candidates = [
                current.join(".opencode"),
                current.join(".claude").join("skills"),
                current.join(".agents").join("skills"),
                current.join(".omp").join("skills"),
            ];
            if candidates.iter().any(|candidate| {
                let display = candidate.to_string_lossy();
                super::skills::is_path_inside(&resolved_display, &display)
                    || super::skills::is_path_inside(&canonical_display, &display)
            }) {
                return (SKILL_SCOPE_PROJECT.to_string(), source.to_string());
            }
            if current == stop {
                break;
            }
            let Some(parent) = current.parent() else {
                break;
            };
            let parent = parent.to_path_buf();
            if parent == current {
                break;
            }
            current = parent;
        }
    }

    let mut user_roots = vec![
        home.join(".config").join("opencode"),
        home.join(".opencode"),
        home.join(".omp").join("agent").join("skills"),
        home.join(".claude").join("skills"),
        home.join(".agents").join("skills"),
    ];
    if let Ok(custom) = std::env::var("OPENCODE_CONFIG_DIR") {
        user_roots.push(super::http_util::resolve_path(&custom));
    }
    if user_roots.iter().any(|root| {
        let display = root.to_string_lossy();
        super::skills::is_path_inside(&resolved_display, &display)
            || super::skills::is_path_inside(&canonical_display, &display)
    }) {
        return ("user".to_string(), source.to_string());
    }
    ("user".to_string(), source.to_string())
}

async fn list_skills(
    State(ctx): State<RouterContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let directory = match resolve_skills_directory(&headers, &query).await {
        Ok(directory) => directory,
        Err(error) => return plain_error(StatusCode::BAD_REQUEST, &error),
    };
    let result: Result<Value, String> = async {
        let home = home_dir();
        let engine_skills = fetch_engine_skills(&ctx, directory.as_deref()).await;
        let local_skills = discover_skills(directory.as_deref(), &home);
        let merged = merge_discovered_skills(&engine_skills, &local_skills);
        let mut enriched = Vec::with_capacity(merged.len());
        for skill in &merged {
            let sources = get_skill_sources(&skill.name, directory.as_deref(), Some(skill), &home)
                .map_err(|error| error.to_string())?;
            let skill_path = skill.path.as_deref();
            let renamable = skill_path
                .is_some_and(|path| path != BUILT_IN_SKILL_LOCATION && is_managed_skill_path(path, directory.as_deref(), &home));
            let mut body = skill.to_value();
            if let Value::Object(map) = &mut body {
                map.insert("sources".into(), sources);
                map.insert("renamable".into(), Value::Bool(renamable));
            }
            enriched.push(body);
        }
        Ok(json!({
            "skills": enriched,
            "externalSkills": {
                "claudeDisabled": is_env_flag_enabled(std::env::var("OPENCODE_DISABLE_CLAUDE_CODE").ok().as_deref())
                    || is_env_flag_enabled(std::env::var("OPENCODE_DISABLE_CLAUDE_CODE_SKILLS").ok().as_deref()),
                "allDisabled": is_env_flag_enabled(std::env::var("OPENCODE_DISABLE_EXTERNAL_SKILLS").ok().as_deref()),
            },
        }))
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => {
            tracing::error!("Failed to list skills: {error}");
            plain_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list skills")
        }
    }
}

/// Custom + curated source rows (shared by /catalog and /catalog/source).
async fn catalog_sources(ctx: &RouterContext) -> Result<Vec<Value>, String> {
    let curated = get_curated_skills_sources();
    let settings = crate::settings::store(ctx)
        .read_typed()
        .await
        .map_err(|error| error.to_string())?;
    let raw_catalogs = serde_json::to_value(settings.skill_catalogs.unwrap_or_default())
        .map_err(|error| error.to_string())?;
    let sanitized =
        crate::settings::normalization::sanitize_skill_catalogs(&raw_catalogs).unwrap_or_default();

    let mut sources: Vec<Value> = curated
        .into_iter()
        .map(|source: CuratedSource| serde_json::to_value(source).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    for entry in sanitized {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let mut row = Map::new();
        let string_field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
        row.insert(
            "id".into(),
            Value::from(string_field("id").unwrap_or_default()),
        );
        row.insert(
            "label".into(),
            Value::from(string_field("label").unwrap_or_default()),
        );
        row.insert(
            "description".into(),
            Value::from(string_field("source").unwrap_or_default()),
        );
        row.insert(
            "source".into(),
            Value::from(string_field("source").unwrap_or_default()),
        );
        if let Some(subpath) = string_field("subpath") {
            row.insert("defaultSubpath".into(), Value::from(subpath));
        }
        if let Some(identity) = string_field("gitIdentityId") {
            row.insert("gitIdentityId".into(), Value::from(identity));
        }
        sources.push(Value::Object(row));
    }
    Ok(sources)
}

fn identities_for_response() -> Vec<Value> {
    super::http_util::git_identity_profiles()
        .into_iter()
        .map(|profile| {
            json!({
                "id": profile.get("id").cloned().unwrap_or(Value::Null),
                "name": profile.get("name").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn resolve_git_identity(profile_id: Option<&str>) -> Option<GitIdentity> {
    let profile_id = profile_id?;
    let profile = super::http_util::git_identity_profile(profile_id)?;
    let ssh_key = profile
        .get("sshKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string);
    Some(GitIdentity { ssh_key })
}

async fn catalog_list(
    State(ctx): State<RouterContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    if let Err(error) = resolve_optional_project_directory(&headers, &query) {
        return plain_error(StatusCode::BAD_REQUEST, &error);
    }
    let result: Result<Value, String> = async {
        let sources = catalog_sources(&ctx).await?;
        let github_repos: Vec<String> = sources
            .iter()
            .filter_map(|src| {
                let parsed = parse_skill_repo_source(
                    src.get("source")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    None,
                );
                match parsed {
                    crate::skills_catalog::SourceParseResult::Ok(parsed)
                        if parsed.host == "github.com" =>
                    {
                        Some(parsed.normalized_repo)
                    }
                    _ => None,
                }
            })
            .collect();
        let metas = fetch_github_repo_metas(&github_repos).await;
        let sources_for_ui: Vec<Value> = sources
            .into_iter()
            .map(|mut src| {
                if let Value::Object(map) = &mut src {
                    map.remove("gitIdentityId");
                }
                let source_str = src
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let parsed = parse_skill_repo_source(&source_str, None);
                let meta = match parsed {
                    crate::skills_catalog::SourceParseResult::Ok(parsed)
                        if parsed.host == "github.com" =>
                    {
                        metas.get(&parsed.normalized_repo).cloned().flatten()
                    }
                    _ => None,
                };
                if let Value::Object(map) = &mut src {
                    match meta {
                        Some(meta) => {
                            map.insert(
                                "stars".into(),
                                serde_json::to_value(meta.stars).unwrap_or(Value::Null),
                            );
                            map.insert(
                                "repoUpdatedAt".into(),
                                meta.repo_updated_at
                                    .clone()
                                    .map(Value::from)
                                    .unwrap_or(Value::Null),
                            );
                        }
                        None => {
                            map.insert("stars".into(), Value::Null);
                            map.insert("repoUpdatedAt".into(), Value::Null);
                        }
                    }
                }
                src
            })
            .collect();
        Ok(json!({ "ok": true, "sources": sources_for_ui, "itemsBySource": {} }))
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => {
            tracing::error!("Failed to load skills catalog: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "ok": false, "error": { "kind": "unknown", "message": "Failed to load catalog" } }),
            )
        }
    }
}

async fn catalog_source(
    State(ctx): State<RouterContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let directory = match resolve_skills_directory(&headers, &query).await {
        Ok(directory) => directory,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": { "kind": "invalidSource", "message": error } }),
            );
        }
    };
    let Some(source_id) = query.first("sourceId") else {
        return error_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": { "kind": "invalidSource", "message": "Missing sourceId" } }),
        );
    };
    let refresh = query.first("refresh").unwrap_or_default().to_lowercase() == "true";

    let result: Result<Value, String> = async {
        let sources = catalog_sources(&ctx).await?;
        let src = sources
            .into_iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(source_id))
            .ok_or_else(|| "Unknown source".to_string())?;
        Ok(src)
    }
    .await;
    let src = match result {
        Ok(src) => src,
        Err(message) if message == "Unknown source" => {
            return error_response(
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "error": { "kind": "invalidSource", "message": "Unknown source" } }),
            );
        }
        Err(error) => {
            tracing::error!("Failed to load catalog source: {error}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "ok": false, "error": { "kind": "unknown", "message": "Failed to load catalog source" } }),
            );
        }
    };

    let git_identity_id = src
        .get("gitIdentityId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let source_str = src
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let default_subpath = src
        .get("defaultSubpath")
        .and_then(Value::as_str)
        .map(str::to_string);

    let parsed = parse_skill_repo_source(&source_str, None);
    let crate::skills_catalog::SourceParseResult::Ok(parsed) = parsed else {
        let crate::skills_catalog::SourceParseResult::Err(error) =
            parse_skill_repo_source(&source_str, None)
        else {
            unreachable!("parse result re-checked");
        };
        return error_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": serde_json::to_value(error).unwrap_or(Value::Null) }),
        );
    };

    let effective_subpath = default_subpath
        .clone()
        .or_else(|| parsed.effective_subpath.clone());
    let cache_key = get_cache_key(
        &parsed.normalized_repo,
        effective_subpath.as_deref().unwrap_or_default(),
        git_identity_id.as_deref().unwrap_or_default(),
    );
    let identity = resolve_git_identity(git_identity_id.as_deref());
    let scan_params = ScanParams {
        source: Some(source_str.clone()),
        subpath: default_subpath.clone(),
        default_subpath: default_subpath.clone(),
        identity,
        git_runner: None,
    };
    let scan_result =
        scan_with_cache(&cache_key, scan_skills_repository(scan_params), refresh).await;

    if !scan_result.ok {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": serde_json::to_value(scan_result.error).unwrap_or(Value::Null) }),
        );
    }

    let home = home_dir();
    let installed_by_name = merge_discovered_skills(
        &fetch_engine_skills(&ctx, directory.as_deref()).await,
        &discover_skills(directory.as_deref(), &home),
    )
    .into_iter()
    .map(|skill| (skill.name.clone(), skill))
    .collect::<HashMap<_, _>>();

    let items: Vec<Value> = scan_result
        .items
        .unwrap_or_default()
        .into_iter()
        .map(|item| {
            let mut row = serde_json::to_value(&item).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut row {
                if let Some(identity) = &git_identity_id {
                    map.insert("gitIdentityId".into(), Value::from(identity.clone()));
                }
                let installed = installed_by_name.get(&item.skill_name);
                map.insert(
                    "installed".into(),
                    match installed {
                        Some(skill) => json!({
                            "isInstalled": true,
                            "scope": skill.scope.clone().map(Value::from).unwrap_or(Value::Null),
                            "source": skill.source.clone().map(Value::from).unwrap_or(Value::Null),
                        }),
                        None => json!({ "isInstalled": false }),
                    },
                );
            }
            row
        })
        .collect();

    Json(json!({ "ok": true, "items": items })).into_response()
}

async fn get_skill(
    State(ctx): State<RouterContext>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let directory = match resolve_skills_directory(&headers, &query).await {
        Ok(directory) => directory,
        Err(error) => return plain_error(StatusCode::BAD_REQUEST, &error),
    };
    let home = home_dir();
    let result: Result<Value, String> = async {
        let discovered = fetch_engine_skill_by_name(&ctx, &name, directory.as_deref()).await;
        let sources = get_skill_sources(&name, directory.as_deref(), discovered.as_ref(), &home)
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "name": name,
            "sources": sources,
            "scope": sources["md"]["scope"].clone(),
            "source": sources["md"]["source"].clone(),
            "exists": sources["md"]["exists"].clone(),
        }))
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => {
            tracing::error!("Failed to get skill sources: {error}");
            plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get skill configuration metadata",
            )
        }
    }
}

async fn scan_repo(
    State(_ctx): State<RouterContext>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let payload = read_json_body(&headers, &body);
    let identity = resolve_git_identity(payload.get("gitIdentityId").and_then(Value::as_str));
    let result = scan_skills_repository(ScanParams {
        source: payload
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
        subpath: payload
            .get("subpath")
            .and_then(Value::as_str)
            .map(str::to_string),
        default_subpath: None,
        identity,
        git_runner: None,
    })
    .await;

    if !result.ok {
        let error = result
            .error
            .unwrap_or_else(|| CatalogError::unknown("Failed to scan repository"));
        if error.kind == "authRequired" {
            let mut error_payload = serde_json::to_value(&error).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut error_payload {
                map.insert("identities".into(), Value::Array(identities_for_response()));
            }
            return error_response(
                StatusCode::UNAUTHORIZED,
                json!({ "ok": false, "error": error_payload }),
            );
        }
        return error_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": serde_json::to_value(&error).unwrap_or(Value::Null) }),
        );
    }
    let items =
        serde_json::to_value(result.items.unwrap_or_default()).unwrap_or(Value::Array(Vec::new()));
    Json(json!({ "ok": true, "items": items })).into_response()
}

async fn install_repo(
    State(_ctx): State<RouterContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query(query.as_deref());
    let payload = read_json_body(&headers, &body);
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut working_directory: Option<String> = None;
    if scope.as_deref() == Some(SKILL_SCOPE_PROJECT) {
        let resolved = super::http_util::resolve_project_directory(&headers, &query).await;
        match resolved.directory {
            Some(directory) => working_directory = Some(directory.to_string_lossy().into_owned()),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "ok": false,
                        "error": {
                            "kind": "invalidSource",
                            "message": resolved
                                .error
                                .unwrap_or_else(|| "Project installs require a directory parameter".to_string()),
                        }
                    }),
                );
            }
        }
    }

    let identity = resolve_git_identity(payload.get("gitIdentityId").and_then(Value::as_str));
    let selections: Option<Vec<InstallSelection>> = payload
        .get("selections")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| InstallSelection {
                    skill_dir: item
                        .get("skillDir")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        });
    let conflict_decisions: Option<HashMap<String, String>> = payload
        .get("conflictDecisions")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
                .collect()
        });

    let home = home_dir();
    let result = install_skills_from_repository(InstallParams {
        source: payload
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
        subpath: payload
            .get("subpath")
            .and_then(Value::as_str)
            .map(str::to_string),
        default_subpath: None,
        identity,
        scope: scope.clone(),
        target_source: payload
            .get("targetSource")
            .and_then(Value::as_str)
            .map(str::to_string),
        working_directory: working_directory.clone(),
        user_skill_dir: Some(skill_dir(&home).to_string_lossy().into_owned()),
        selections,
        conflict_policy: payload
            .get("conflictPolicy")
            .and_then(Value::as_str)
            .map(str::to_string),
        conflict_decisions,
        git_runner: None,
    })
    .await;

    if !result.ok {
        let error = result
            .error
            .unwrap_or_else(|| CatalogError::unknown("Failed to install skills"));
        if error.kind == "conflicts" {
            return error_response(
                StatusCode::CONFLICT,
                json!({ "ok": false, "error": serde_json::to_value(&error).unwrap_or(Value::Null) }),
            );
        }
        if error.kind == "authRequired" {
            let mut error_payload = serde_json::to_value(&error).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut error_payload {
                map.insert("identities".into(), Value::Array(identities_for_response()));
            }
            return error_response(
                StatusCode::UNAUTHORIZED,
                json!({ "ok": false, "error": error_payload }),
            );
        }
        return error_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": serde_json::to_value(&error).unwrap_or(Value::Null) }),
        );
    }

    let installed = result.installed.unwrap_or_default();
    let skipped = result.skipped.unwrap_or_default();
    let requires_restart = !installed.is_empty();
    let mut body = Map::new();
    body.insert("ok".into(), Value::Bool(true));
    body.insert(
        "installed".into(),
        serde_json::to_value(&installed).unwrap_or(Value::Array(Vec::new())),
    );
    body.insert(
        "skipped".into(),
        serde_json::to_value(&skipped).unwrap_or(Value::Array(Vec::new())),
    );
    if requires_restart {
        let restart = build_deferred_restart_response(
            "Skills installed successfully. Restart the engine to apply.",
        );
        if let Value::Object(restart_map) = restart {
            for (key, value) in restart_map {
                body.insert(key, value);
            }
        }
    } else {
        body.insert("requiresReload".into(), Value::Bool(false));
        body.insert("message".into(), Value::from("No skills were installed"));
    }
    Json(Value::Object(body)).into_response()
}

async fn fetch_engine_skill_by_name(
    ctx: &RouterContext,
    name: &str,
    directory: Option<&Path>,
) -> Option<DiscoveredSkill> {
    fetch_engine_skills(ctx, directory)
        .await
        .into_iter()
        .find(|skill| skill.name == name)
}

fn decode_file_path(raw: &str) -> Result<String, ()> {
    super::http_util::decode_uri_component(raw).ok_or(())
}

/// Shared preamble for the supporting-file handlers: validate the path,
/// resolve the directory, load sources; `None` ⇒ skill missing (404).
async fn skill_file_context(
    ctx: &RouterContext,
    name: &str,
    raw_file_path: &str,
    headers: &HeaderMap,
    query: &QueryValues,
    decode_failure_message: &str,
) -> Result<(PathBuf, String), Response> {
    let file_path = match decode_file_path(raw_file_path) {
        Ok(file_path) => file_path,
        Err(_) => {
            return Err(plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                decode_failure_message,
            ));
        }
    };
    if crate::settings::normalization::is_unsafe_skill_relative_path(&file_path) {
        return Err(plain_error(StatusCode::BAD_REQUEST, "Invalid file path"));
    }
    let directory = match resolve_skills_directory(headers, query).await {
        Ok(directory) => directory,
        Err(error) => return Err(plain_error(StatusCode::BAD_REQUEST, &error)),
    };
    let home = home_dir();
    let discovered = fetch_engine_skill_by_name(ctx, name, directory.as_deref()).await;
    let sources = match get_skill_sources(name, directory.as_deref(), discovered.as_ref(), &home) {
        Ok(sources) => sources,
        Err(error) => {
            tracing::error!("Failed to read skill file: {error}");
            return Err(plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read skill file",
            ));
        }
    };
    let md_dir = sources["md"]["dir"].as_str().map(str::to_string);
    let exists = sources["md"]["exists"].as_bool().unwrap_or(false);
    match (exists, md_dir) {
        (true, Some(dir)) => Ok((PathBuf::from(dir), file_path)),
        _ => Err(plain_error(StatusCode::NOT_FOUND, "Skill not found")),
    }
}

async fn get_skill_file(
    State(ctx): State<RouterContext>,
    AxumPath((name, file_path)): AxumPath<(String, String)>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let (dir, file_path) = match skill_file_context(
        &ctx,
        &name,
        &file_path,
        &headers,
        &query,
        "Failed to read skill file",
    )
    .await
    {
        Ok(context) => context,
        Err(response) => return response,
    };
    match read_skill_supporting_file(&dir, &file_path) {
        Ok(Some(content)) => Json(json!({ "path": file_path, "content": content })).into_response(),
        Ok(None) => plain_error(StatusCode::NOT_FOUND, "File not found"),
        Err(AccessError) => plain_error(StatusCode::FORBIDDEN, "Access to file denied"),
    }
}
async fn put_skill_file(
    State(ctx): State<RouterContext>,
    AxumPath((name, file_path)): AxumPath<(String, String)>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query(query.as_deref());
    let payload = read_json_body(&headers, &body);
    let (dir, file_path) = match skill_file_context(
        &ctx,
        &name,
        &file_path,
        &headers,
        &query,
        "Failed to write skill file",
    )
    .await
    {
        Ok(context) => context,
        Err(response) => return response,
    };
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match write_skill_supporting_file(&dir, &file_path, content) {
        Ok(()) => Json(json!({
            "success": true,
            "message": format!("File {file_path} saved successfully"),
        }))
        .into_response(),
        Err(AccessError) => plain_error(StatusCode::FORBIDDEN, "Access to file denied"),
    }
}

async fn delete_skill_file(
    State(ctx): State<RouterContext>,
    AxumPath((name, file_path)): AxumPath<(String, String)>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let (dir, file_path) = match skill_file_context(
        &ctx,
        &name,
        &file_path,
        &headers,
        &query,
        "Failed to delete skill file",
    )
    .await
    {
        Ok(context) => context,
        Err(response) => return response,
    };
    match delete_skill_supporting_file(&dir, &file_path) {
        Ok(()) => Json(json!({
            "success": true,
            "message": format!("File {file_path} deleted successfully"),
        }))
        .into_response(),
        Err(AccessError) => plain_error(StatusCode::FORBIDDEN, "Access to file denied"),
    }
}

async fn create_skill_route(
    State(_ctx): State<RouterContext>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query(query.as_deref());
    let payload = read_json_body(&headers, &body);
    let Some(config) = payload.as_object().cloned() else {
        return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create skill");
    };
    let scope = config
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string);

    let directory = if scope.as_deref() == Some(SKILL_SCOPE_PROJECT) {
        let resolved = super::http_util::resolve_project_directory(&headers, &query).await;
        match resolved.directory {
            Some(directory) => Some(directory),
            None => {
                return plain_error(
                    StatusCode::BAD_REQUEST,
                    &resolved.error.unwrap_or_else(|| {
                        "Project skill creation requires a directory".to_string()
                    }),
                );
            }
        }
    } else {
        match resolve_skills_directory(&headers, &query).await {
            Ok(directory) => directory,
            Err(error) => return plain_error(StatusCode::BAD_REQUEST, &error),
        }
    };

    let skill_config = config;
    let result = create_skill(
        &name,
        &Value::Object(skill_config),
        directory.as_deref(),
        scope.as_deref(),
        &home_dir(),
    );
    match result {
        Ok(()) => Json(build_deferred_restart_response(&format!(
            "Skill {name} created successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

async fn patch_skill(
    State(_ctx): State<RouterContext>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query(query.as_deref());
    let payload = read_json_body(&headers, &body);
    let Some(updates) = payload.as_object().cloned() else {
        return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update skill");
    };
    let directory = match resolve_skills_directory(&headers, &query).await {
        Ok(directory) => directory,
        Err(error) => return plain_error(StatusCode::BAD_REQUEST, &error),
    };
    let home = home_dir();

    if let Some(new_name) = updates.get("renameTo").and_then(Value::as_str) {
        let new_name = new_name.trim();
        let result = rename_skill(&name, new_name, directory.as_deref(), &home);
        return match result {
            Ok(()) => {
                // JS intent: refresh the engine, then prompt a UI reload. The
                // JS call site references an undeclared variable (bug); the
                // port performs the intended no-op refresh.
                tracing::info!("Refreshing OpenCode after skill rename");
                Json(json!({
                    "success": true,
                    "name": new_name,
                    "requiresReload": true,
                    "message": format!("Skill renamed to {new_name} successfully. Reloading interface\u{2026}"),
                    "reloadDelayMs": CLIENT_RELOAD_DELAY_MS,
                }))
                .into_response()
            }
            Err(error) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
        };
    }

    let target_path = updates
        .get("targetPath")
        .and_then(Value::as_str)
        .map(str::to_string);
    let result = update_skill(
        &name,
        &Value::Object(updates),
        directory.as_deref(),
        target_path.as_deref(),
        &home,
    );
    match result {
        Ok(()) => Json(build_deferred_restart_response(&format!(
            "Skill {name} updated successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) if error == "Access to file denied" => {
            plain_error(StatusCode::FORBIDDEN, "Access to file denied")
        }
        Err(error) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

async fn delete_skill_route(
    State(_ctx): State<RouterContext>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query(query.as_deref());
    let directory = match resolve_skills_directory(&headers, &query).await {
        Ok(directory) => directory,
        Err(error) => return plain_error(StatusCode::BAD_REQUEST, &error),
    };
    match delete_skill(&name, directory.as_deref(), &home_dir()) {
        Ok(()) => Json(build_deferred_restart_response(&format!(
            "Skill {name} deleted successfully. Restart the engine to apply."
        )))
        .into_response(),
        Err(error) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode_plugins::http_util::encode_uri_component;
    use axum::body::Body;
    use tower::ServiceExt;

    struct EnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous_home: Option<String>,
    }

    impl EnvGuard {
        fn lock(home: &Path) -> Self {
            let guard = super::super::http_util::TEST_ENV_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous_home = std::env::var("HOME").ok();
            unsafe {
                std::env::set_var("HOME", home.to_string_lossy().as_ref());
                std::env::remove_var("OPENCODE_CONFIG_DIR");
                std::env::remove_var("OPENCODE_CONFIG");
            }
            Self {
                _guard: guard,
                previous_home,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous_home {
                Some(home) => unsafe {
                    std::env::set_var("HOME", home);
                },
                None => unsafe {
                    std::env::remove_var("HOME");
                },
            }
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-skill-routes-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    async fn json_body(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn test_ctx(
        engine: std::sync::Arc<crate::engine::EngineState>,
        data_dir: PathBuf,
    ) -> RouterContext {
        RouterContext {
            config: std::sync::Arc::new(crate::config::ServerConfig {
                port: 0,
                host: None,
                lan: false,
                ui_password: None,
                api_only: false,
                data_dir,
                dist_dir: PathBuf::from("/nonexistent-dist"),
                tunnel: Default::default(),
                engine: crate::config::EngineConfig::External {
                    base_url: "http://127.0.0.1:1".to_string(),
                },
            }),
            engine,
            hub: crate::hub::EventHub::new(),
        }
    }

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        let path = skill_dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("write");
        path
    }

    #[tokio::test]
    async fn create_then_lists_repository_local_agents_skills_without_directory() {
        let root = temp_root("create-list");
        let _env = EnvGuard::lock(&root);
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).expect("git");
        // The JS test injects an active project; the port resolves it from
        // settings (`lastDirectory`), so persist that.
        std::fs::create_dir_all(root.join(".config").join("ompchamber")).expect("settings dir");
        std::fs::write(
            root.join(".config")
                .join("ompchamber")
                .join("settings.json"),
            serde_json::to_string(&json!({ "lastDirectory": project.to_string_lossy() }))
                .expect("json"),
        )
        .expect("settings");

        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);

        let created = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/config/skills/repo-local-skill")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "description": "Created without list directory",
                            "instructions": "Do the thing.",
                            "scope": "project",
                            "source": "agents",
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(created.status(), StatusCode::OK);
        assert!(
            project
                .join(".agents")
                .join("skills")
                .join("repo-local-skill")
                .join("SKILL.md")
                .exists()
        );

        let listed = app
            .clone()
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills?directory={}",
                    encode_uri_component(&project.to_string_lossy())
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(listed.status(), StatusCode::OK);
        let payload = json_body(listed).await;
        let skill = payload["skills"]
            .as_array()
            .expect("skills")
            .iter()
            .find(|skill| skill["name"] == json!("repo-local-skill"))
            .expect("repo-local-skill listed");
        assert_eq!(skill["scope"], json!("project"));
        assert_eq!(skill["source"], json!("agents"));
        assert!(skill["sources"]["md"]["exists"] == json!(true));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn marks_managed_skills_renamable_but_cache_skills_not() {
        let root = temp_root("renamable");
        let _env = EnvGuard::lock(&root);
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).expect("git");
        write_skill(
            &project.join(".opencode").join("skills"),
            "managed-list-skill",
            "Managed list skill",
            "Managed body",
        );
        write_skill(
            &root
                .join(".cache")
                .join("opencode")
                .join("skills")
                .join("stamp"),
            "cache-list-skill",
            "Cache skill",
            "Cache body",
        );

        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);
        let listed = app
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills?directory={}",
                    encode_uri_component(&project.to_string_lossy())
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(listed.status(), StatusCode::OK);
        let payload = json_body(listed).await;
        let find = |name: &str| {
            payload["skills"]
                .as_array()
                .expect("skills")
                .iter()
                .find(|skill| skill["name"] == json!(name))
                .cloned()
                .expect(name)
        };
        assert_eq!(find("managed-list-skill")["renamable"], json!(true));
        assert_eq!(find("cache-list-skill")["renamable"], json!(false));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn merges_engine_rows_and_infers_omp_scopes() {
        let root = temp_root("engine-merge");
        let fake_home = root.join("home");
        let _env = EnvGuard::lock(&fake_home);
        std::fs::create_dir_all(&fake_home).expect("home");
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).expect("git");
        let project_skill_path = project
            .join(".omp")
            .join("skills")
            .join("deploy-helper")
            .join("SKILL.md");
        let user_skill_path = fake_home
            .join(".omp")
            .join("agent")
            .join("skills")
            .join("global-helper")
            .join("SKILL.md");
        std::fs::create_dir_all(project_skill_path.parent().unwrap()).expect("dir");
        std::fs::write(&project_skill_path, "body").expect("write");
        std::fs::create_dir_all(user_skill_path.parent().unwrap()).expect("dir");
        std::fs::write(&user_skill_path, "body").expect("write");

        let project_location = project_skill_path.to_string_lossy().into_owned();
        let user_location = user_skill_path.to_string_lossy().into_owned();
        let engine_app = axum::Router::new().route(
            "/skill",
            get(move || async move {
                Json(json!([
                    {
                        "name": "deploy-helper",
                        "description": "Project omp skill",
                        "location": project_location,
                        "content": "Project omp body",
                    },
                    {
                        "name": "global-helper",
                        "description": "User omp skill",
                        "location": user_location,
                        "content": "User omp body",
                    }
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let engine_port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, engine_app)
                .await
                .expect("engine server");
        });

        let ctx = test_ctx(
            crate::engine::EngineState::external(format!("http://127.0.0.1:{engine_port}"), None),
            root.join("data"),
        );
        let app = routes(ctx);
        let listed = app
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills?directory={}",
                    encode_uri_component(&project.to_string_lossy())
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(listed.status(), StatusCode::OK);
        let payload = json_body(listed).await;
        let find = |name: &str| {
            payload["skills"]
                .as_array()
                .expect("skills")
                .iter()
                .find(|skill| skill["name"] == json!(name))
                .cloned()
                .expect(name)
        };
        assert_eq!(find("deploy-helper")["scope"], json!("project"));
        assert_eq!(find("global-helper")["scope"], json!("user"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn skill_crud_and_supporting_files_shapes() {
        let root = temp_root("crud");
        let _env = EnvGuard::lock(&root);
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        let directory_param = format!(
            "?directory={}",
            encode_uri_component(&project.to_string_lossy())
        );

        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);
        let skill_dir = project.join(".opencode").join("skills").join("doc-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: doc-skill\ndescription: Docs\n---\n\nBody\n",
        )
        .expect("write");

        let got = app
            .clone()
            .oneshot(
                axum::http::Request::get(&format!("/api/config/skills/doc-skill{directory_param}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(got.status(), StatusCode::OK);
        let payload = json_body(got).await;
        assert_eq!(payload["name"], json!("doc-skill"));
        assert_eq!(payload["exists"], json!(true));
        assert_eq!(payload["scope"], json!("project"));
        assert_eq!(payload["sources"]["md"]["instructions"], json!("Body"));

        let saved = app
            .clone()
            .oneshot(
                axum::http::Request::put(&format!(
                    "/api/config/skills/doc-skill/files/notes{}",
                    directory_param
                ))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "content": "inner notes" }).to_string()))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(
            json_body(saved).await,
            json!({ "success": true, "message": "File notes saved successfully" })
        );

        let read = app
            .clone()
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills/doc-skill/files/notes{}",
                    directory_param
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(
            json_body(read).await,
            json!({ "path": "notes", "content": "inner notes" })
        );

        let unsafe_path = app
            .clone()
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills/doc-skill/files/{}{}",
                    encode_uri_component("../escape"),
                    directory_param
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unsafe_path.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(unsafe_path).await,
            json!({ "error": "Invalid file path" })
        );

        let missing_file = app
            .clone()
            .oneshot(
                axum::http::Request::get(&format!(
                    "/api/config/skills/doc-skill/files/gone{}",
                    directory_param
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing_file.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(missing_file).await,
            json!({ "error": "File not found" })
        );

        let renamed = app
            .clone()
            .oneshot(
                axum::http::Request::patch(&format!(
                    "/api/config/skills/doc-skill{directory_param}"
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "renameTo": "doc-skill-v2" }).to_string(),
                ))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(renamed.status(), StatusCode::OK);
        let payload = json_body(renamed).await;
        assert_eq!(payload["success"], json!(true));
        assert_eq!(payload["name"], json!("doc-skill-v2"));
        assert_eq!(payload["requiresReload"], json!(true));
        assert_eq!(payload["reloadDelayMs"], json!(800));
        assert!(
            project
                .join(".opencode")
                .join("skills")
                .join("doc-skill-v2")
                .join("SKILL.md")
                .exists()
        );

        let deleted = app
            .clone()
            .oneshot(
                axum::http::Request::delete(&format!(
                    "/api/config/skills/doc-skill-v2{directory_param}"
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(deleted.status(), StatusCode::OK);
        let payload = json_body(deleted).await;
        assert_eq!(
            payload["message"],
            json!("Skill doc-skill-v2 deleted successfully. Restart the engine to apply.")
        );
        assert!(
            !project
                .join(".opencode")
                .join("skills")
                .join("doc-skill-v2")
                .exists()
        );

        let delete_missing = app
            .oneshot(
                axum::http::Request::delete(&format!(
                    "/api/config/skills/doc-skill-v2{directory_param}"
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(delete_missing.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            json_body(delete_missing).await["error"]
                .as_str()
                .expect("error")
                .contains("not found")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn catalog_lists_curated_sources_with_metadata_shape() {
        let root = temp_root("catalog");
        let _env = EnvGuard::lock(&root);

        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);
        let listed = app
            .oneshot(
                axum::http::Request::get("/api/config/skills/catalog")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(listed.status(), StatusCode::OK);
        let payload = json_body(listed).await;
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["itemsBySource"], json!({}));
        let sources = payload["sources"].as_array().expect("sources");
        assert!(sources.len() >= 4, "curated sources present");
        let anthropic = sources
            .iter()
            .find(|source| source["id"] == json!("anthropic"))
            .expect("anthropic curated");
        assert_eq!(anthropic["label"], json!("Anthropic"));
        assert!(
            anthropic.get("gitIdentityId").is_none(),
            "gitIdentityId stripped"
        );
        assert!(
            anthropic.get("stars").is_some(),
            "stars/repoUpdatedAt present"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn catalog_source_requires_known_source_id() {
        let root = temp_root("catalog-source");
        let _env = EnvGuard::lock(&root);

        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);

        let missing = app
            .clone()
            .oneshot(
                axum::http::Request::get("/api/config/skills/catalog/source")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(missing).await,
            json!({ "ok": false, "error": { "kind": "invalidSource", "message": "Missing sourceId" } })
        );

        let unknown = app
            .oneshot(
                axum::http::Request::get("/api/config/skills/catalog/source?sourceId=nope")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(unknown).await,
            json!({ "ok": false, "error": { "kind": "invalidSource", "message": "Unknown source" } })
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn scan_rejects_missing_source_with_invalid_source() {
        let root = temp_root("scan");
        let _env = EnvGuard::lock(&root);
        let ctx = test_ctx(
            crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
            root.join("data"),
        );
        let app = routes(ctx);
        let scanned = app
            .oneshot(
                axum::http::Request::post("/api/config/skills/scan")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({}).to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(scanned.status(), StatusCode::BAD_REQUEST);
        let payload = json_body(scanned).await;
        assert_eq!(payload["ok"], json!(false));
        assert_eq!(payload["error"]["kind"], json!("invalidSource"));
        assert_eq!(
            payload["error"]["message"],
            json!("Repository source is required")
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
