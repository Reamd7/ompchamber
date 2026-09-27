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
//! （中文说明）本模块是 skill 配置面的 HTTP 路由层：列表合并（本地扫描
//! + engine `GET /skill` 透传）、单 skill 来源元数据、辅助文件读写、
//! skill CRUD 与重命名，以及委托 `skills_catalog` 模块的 catalog /
//! scan / install 端点。除重命名外的成功变更统一返回
//! "Restart the engine to apply." 的延迟重启响应。

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
/// 重命名成功后前端延迟刷新界面的毫秒数，随响应的 `reloadDelayMs` 字段下发。
const CLIENT_RELOAD_DELAY_MS: u64 = 800;
/// 透传 engine `GET /skill` 请求的超时时间（8 秒）；超时按空结果降级。
const ENGINE_SKILLS_TIMEOUT: Duration = Duration::from_millis(8_000);
/// scope 取值 "project"：表示 skill 创建/安装到项目目录下的受管理路径。
const SKILL_SCOPE_PROJECT: &str = "project";

/// 挂载 `/api/config/skills*` 前缀的全部 skill 配置路由，
/// 以 `RouterContext` 作为共享状态。
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

/// 以指定状态码与任意 JSON body 构造错误响应（结构化错误体也走这里）。
fn error_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// 构造 `{"error": <message>}` 形式的简单 JSON 错误响应。
fn plain_error(status: StatusCode, message: &str) -> Response {
    error_response(status, json!({ "error": message }))
}

/// 当前用户 home 目录；取不到时回退为 `/`，仅用于拼接 user 级路径。
fn home_dir() -> PathBuf {
    crate::config::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `resolveSkillsDirectory` — optional first, then the active-project/last
/// directory fallback; fallback errors are ignored (user-scoped listing
/// without a project is valid).
/// （中文）目录解析优先级：请求显式携带的 directory（参数或头）优先；
/// 缺失时回退到活动项目（settings 的 lastDirectory），回退阶段的错误
/// 被忽略——无项目上下文时返回 `Ok(None)` 表示仅枚举 user 级 skill。
/// 显式 directory 非法时返回 `Err(消息)`（调用方映射 400）。
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
/// （中文）engine 不可用（无 base_url）、请求失败、超时或响应不是数组时
/// 一律记日志并返回空列表，不向调用方报错；有效行要求 name 与 location
/// 非空，内置位置直接标记 user/opencode，其余按路径推断 scope/source。
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
/// （中文）返回 (scope, source)：source 由路径片段判定——`.agents/skills`
/// → "agents"、`.claude/skills` → "claude"、其余 → "opencode"。scope 先
/// 从 working_directory 逐级向上（到 worktree 根为止）匹配 `.opencode`、
/// `.claude/skills`、`.agents/skills`、`.omp/skills`，命中即 "project"；
/// 否则对照 home 下各 user 级根目录（含 OPENCODE_CONFIG_DIR）；
/// 两种路径形式都不匹配时兜底仍返回 "user"。
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

/// `GET /api/config/skills`：合并 engine 透传与本地扫描的 skill，逐个附加
/// `sources`（各来源元数据）与 `renamable`（路径受管理且非内置才为 true），
/// 并按环境变量报告外部 skill（Claude 等）的禁用状态。目录解析失败 400；
/// 任一 skill 的来源读取失败记日志并整体返回 500。
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
/// （中文）输出统一行形状：id/label/description(取 source)/source，可选
/// defaultSubpath 与 gitIdentityId。curated 序列化或 settings 读取失败时
/// 返回 `Err`（调用方映射 500）。
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

/// 供 authRequired 错误响应使用的 git identity 档案列表（仅保留 id/name）。
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

/// 按 profile id 解析 git identity，仅提取非空 sshKey；id 缺失、档案
/// 不存在或 sshKey 为空白都返回 `None`（匿名访问仓库）。
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

/// `GET /api/config/skills/catalog`：先校验可选 directory（非法 400），再
/// 返回 curated + 自定义来源；对 github.com 来源并发拉取 star 数与仓库
/// 更新时间（取不到补 null），并从返回行剥离 gitIdentityId。
/// `itemsBySource` 恒为空对象——条目由 /catalog/source 按需加载。
/// 任意步骤失败记日志并返回 500（ok:false 结构）。
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

/// `GET /api/config/skills/catalog/source?sourceId=&refresh=`：按 id 定位
/// 来源（缺参数或目录错误 400、未知 id 404，均携带 invalidSource kind），
/// 解析 repo source（失败 400），经 `scan_with_cache` 扫描（refresh=true
/// 绕过缓存；失败 500），最后把扫描条目与本地已安装 skill（engine + 本地
/// 合并结果，按名称匹配）比对并附加 installed 元数据。
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

/// `GET /api/config/skills/{name}`：返回单 skill 的来源元数据，外层
/// scope/source/exists 直接取自 md 来源行。目录解析失败 400；来源读取
/// 失败记日志并返回 500。
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

/// `POST /api/config/skills/scan`：按 body 的 source/subpath/gitIdentityId
/// 即时扫描远端仓库（不走缓存）。authRequired 错误附带可选 identities
/// 列表返回 401，其余失败 400；成功返回 `{ok, items}`。
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

/// `POST /api/config/skills/install`：安装选中 skill 到 user 或 project
/// scope；project 必须能解析出工作目录（否则 400 invalidSource）。支持
/// selections 与 conflictPolicy/conflictDecisions：冲突返回 409（前端
/// 决策后重试），authRequired 附 identities 返回 401，其余失败 400。
/// 有实际安装时返回延迟重启响应，全部跳过则 requiresReload=false。
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

/// 在 engine `GET /skill` 结果中按名称查找单个 skill；engine 不可达或
/// 未命中都返回 `None`。
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

/// 对 URL 路径段做 decodeURIComponent；解码失败以 `Err(())` 表示。
fn decode_file_path(raw: &str) -> Result<String, ()> {
    super::http_util::decode_uri_component(raw).ok_or(())
}

/// Shared preamble for the supporting-file handlers: validate the path,
/// resolve the directory, load sources; `None` ⇒ skill missing (404).
/// （中文）成功返回 (skill 的 md 目录, 解码后的相对路径)；文件路径不安全
/// 返回 400 "Invalid file path"；skill 不存在返回 404；解码失败与来源
/// 读取失败分别按调用方给定的消息返回 500。
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

/// `GET /api/config/skills/{name}/files/{*file_path}`：读取 skill 辅助
/// 文件内容；文件不存在 404，越权路径 403。
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
/// `PUT .../files/{*file_path}`：以 body 的 content 覆写辅助文件；
/// 越权路径 403。
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

/// `DELETE .../files/{*file_path}`：删除辅助文件；越权路径 403。
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

/// `POST /api/config/skills/{name}`：创建 skill。body 须为 JSON 对象
/// （否则 500）；scope=project 时必须能解析出项目目录（否则 400），
/// 其余 scope 走通用目录解析。成功返回延迟重启响应；失败 500。
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

/// `PATCH /api/config/skills/{name}`：body 含 `renameTo` 时执行重命名，
/// 成功返回 requiresReload 响应（携带 reloadDelayMs）；否则按 updates
/// （含可选 targetPath）更新，成功返回延迟重启响应；错误消息为
/// "Access to file denied" 时映射 403，其余 500。
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

/// `DELETE /api/config/skills/{name}`：删除 skill；成功返回延迟重启响应，
/// 失败（如不存在）500。
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

/// skill 路由的行为契约测试：目录解析与列表合并、renamable 判定、
/// engine 行合并的 scope 推断、CRUD/辅助文件/重命名的响应形状，以及
/// catalog 与 scan 的参数校验。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode_plugins::http_util::encode_uri_component;
    use axum::body::Body;
    use tower::ServiceExt;

/// 测试环境守护：持有全局互斥锁并把 HOME 等环境变量切到临时目录，
/// Drop 时自动恢复原值。
    struct EnvGuard {
/// 全局测试环境互斥锁守卫：持有期间其它测试不能修改环境变量。
        _guard: std::sync::MutexGuard<'static, ()>,
/// 进入测试前 HOME 的原值；None 表示原本未设置。
        previous_home: Option<String>,
    }

/// EnvGuard 的加锁与环境切换构造。
    impl EnvGuard {
/// 加全局锁，把 HOME 指向给定目录并清除 OPENCODE_CONFIG_DIR /
/// OPENCODE_CONFIG，返回可自动恢复原值的 guard。
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

/// Drop 实现：测试结束时恢复 HOME。
    impl Drop for EnvGuard {
/// 恢复测试前记录的 HOME（原值缺失则移除该变量）。
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

/// 创建带 pid 与计数后缀的唯一临时根目录，保证测试之间互不干扰。
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-skill-routes-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

/// 生成进程内单调递增的唯一后缀（配合 pid 一起使用）。
    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        // 进程内原子计数器：同一进程并行测试也能拿到不同目录名。
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

/// 读出整个响应 body 并解析为 JSON；失败即 panic（仅测试断言使用）。
    async fn json_body(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

/// 构造指向不可达 engine（127.0.0.1:1）的 RouterContext，
/// 让测试聚焦本地文件系统行为。
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

/// 在 dir/<name>/SKILL.md 写入带 name/description frontmatter 的
/// skill 文件，返回其路径。
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

/// 验证：创建 project skill 时即便请求未带 directory（经 settings 的
/// lastDirectory 回退解析），后续列表也能发现 `.agents/skills` 下的
/// 该 skill 并正确标注 scope/source/exists。
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

/// 验证：受管理路径（项目 .opencode/skills）下的 skill 列表 renamable
/// 为 true，而缓存目录（~/.cache/opencode/skills）中的为 false。
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

/// 验证：engine `GET /skill` 返回的 `.omp/skills` 行合并进列表后，
/// 项目内路径推断为 project scope、home 下路径推断为 user scope。
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

/// 验证：单 skill 元数据读取、辅助文件写/读、不安全路径 400、缺失文件
/// 404、重命名响应（requiresReload + reloadDelayMs=800）与删除的延迟
/// 重启响应的完整形状。
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

/// 验证：catalog 列表包含 curated 来源（如 anthropic），行内剥离
/// gitIdentityId 且带 stars/repoUpdatedAt 字段，itemsBySource 为空。
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

/// 验证：/catalog/source 缺 sourceId 返回 400 invalidSource，
/// 未知 sourceId 返回 404 invalidSource。
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

/// 验证：scan 请求缺 source 时返回 400，错误 kind 为 invalidSource，
/// 消息为 "Repository source is required"。
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
