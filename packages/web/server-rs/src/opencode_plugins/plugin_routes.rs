//! Port of `server/lib/opencode/plugin-routes.js` — the plugin config and
//! plugin-file route family plus the npm registry status endpoint.
//!
//! Wire contract:
//! - coded errors map: ENTRY_EXISTS/EEXIST (entries) and FILE_EXISTS/EEXIST
//!   (files) → 409, NOT_FOUND/ENOENT → 404,
//!   INVALID_FILENAME/INVALID_SCOPE/INVALID_SPEC/EINVAL → 400, anything else
//!   logs and answers the route's fallback message with 500.
//! - every successful mutation answers the deferred-restart response
//!   (`Plugin <past tense>. Restart the engine to apply.`) — the engine
//!   refresh is deferred exactly like the JS.
//! （中文说明）本模块是插件配置面的 HTTP 路由层：插件条目（entries）与
//! 插件文件（files）的 CRUD，以及 npm registry / 本地路径的 spec 状态
//! 查询端点。coded error 到 HTTP 状态码的映射与成功变更的延迟重启
//! 响应约定见上方英文说明。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::config_layers::CodedError;
use super::http_util::{
    QueryValues, build_deferred_restart_response, plugin_mutation_past_tense, read_json_body,
    resolve_optional_project_directory,
};
use super::npm_registry::{NpmInfo, get_npm_info};
use super::plugin_spec::{NpmSpec, is_exact_semver, parse_npm_spec, parse_path_spec};
use super::plugins::{
    create_plugin_entry, decode_plugin_id, delete_plugin_dir_file, delete_plugin_entry,
    encode_plugin_id, get_plugin_entry, list_plugin_dir_files, list_plugin_entries,
    parsed_kind_for_spec, read_plugin_dir_file, update_plugin_entry, write_plugin_dir_file,
};
use crate::context::RouterContext;

/// Injectable `getNpmInfo` (the JS tests inject a mock; production hits the
/// registry through the shared reqwest client).
/// （中文）参数为 (包名, 是否强制刷新)，返回注册表元数据；抽象成类型
/// 别名是为了让测试注入 mock 而不触碰真实网络。
pub(crate) type NpmLookup =
    Arc<dyn Fn(String, bool) -> Pin<Box<dyn Future<Output = NpmInfo> + Send>> + Send + Sync>;

/// 插件路由的共享状态：仅封装可注入的 npm 查询函数，
/// 使 registry 端点可在测试中脱离真实 registry 运行。
#[derive(Clone)]
pub(crate) struct PluginRouteState {
/// 注入的 getNpmInfo 实现（生产环境为共享 client 的真实查询）。
    npm: NpmLookup,
}

/// 生产入口：捕获 engine 的共享 HTTP client 构造真实 npm 查询闭包，
/// 再挂载全部插件路由。
pub(crate) fn routes(ctx: RouterContext) -> Router {
    let http = ctx.engine.http().clone();
    let npm: NpmLookup = Arc::new(move |name, force_refresh| {
        let http = http.clone();
        Box::pin(async move { get_npm_info(&http, &name, force_refresh).await })
    });
    router_with_state(PluginRouteState { npm })
}

/// 用给定状态构建 `/api/config/plugins*` 路由表；
/// testing 模块复用它以注入 mock npm 查询。
fn router_with_state(state: PluginRouteState) -> Router {
    Router::new()
        .route("/api/config/plugins", get(list_plugins))
        .route("/api/config/plugins/registry", get(registry_status))
        .route(
            "/api/config/plugins/entry/{id}",
            get(get_entry).patch(patch_entry).delete(delete_entry),
        )
        .route("/api/config/plugins/entry", post(post_entry))
        .route(
            "/api/config/plugins/file/{id}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .route("/api/config/plugins/file", post(post_file))
        .with_state(state)
}

/// 以状态码 + `{"error": message}` 构造 JSON 错误响应。
fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// `handlePluginError`.
/// （中文）统一错误映射：exists_kind 区分 entry/file 之后，
/// ENTRY_EXISTS/EEXIST → 409，NOT_FOUND/ENOENT → 404，
/// INVALID_FILENAME/INVALID_SCOPE/INVALID_SPEC/EINVAL → 400；
/// 其余记 error 日志并返回 500 + fallback_message。
fn handle_plugin_error(
    error: CodedError,
    fallback_message: &str,
    context: &str,
    exists_kind: Option<&str>,
) -> Response {
    let code = error.code;
    let entry_exists = matches!(code, "ENTRY_EXISTS" | "EEXIST") && exists_kind == Some("entry");
    let file_exists = matches!(code, "FILE_EXISTS" | "EEXIST") && exists_kind == Some("file");
    if entry_exists || file_exists {
        return error_response(StatusCode::CONFLICT, &error.message);
    }
    if matches!(code, "NOT_FOUND" | "ENOENT") {
        return error_response(StatusCode::NOT_FOUND, &error.message);
    }
    if matches!(
        code,
        "INVALID_FILENAME" | "INVALID_SCOPE" | "INVALID_SPEC" | "EINVAL"
    ) {
        return error_response(StatusCode::BAD_REQUEST, &error.message);
    }
    tracing::error!("{context}: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, fallback_message)
}

/// 校验 id 解码后为 `config` 前缀（插件条目）；前缀不符返回
/// NOT_FOUND coded error（最终映射 404）。
fn validate_entry_id(id: &str) -> Result<(), CodedError> {
    let (prefix, _) = decode_plugin_id(id)?;
    if prefix != "config" {
        return Err(CodedError::new("Plugin entry not found", "NOT_FOUND"));
    }
    Ok(())
}

/// 校验 id 解码后为 `file` 前缀（插件文件）；前缀不符返回
/// NOT_FOUND coded error（最终映射 404）。
fn validate_file_id(id: &str) -> Result<(), CodedError> {
    let (prefix, _) = decode_plugin_id(id)?;
    if prefix != "file" {
        return Err(CodedError::new("Plugin file not found", "NOT_FOUND"));
    }
    Ok(())
}

/// The JS `resolveDirectory` helper: 400 + `{error}` on a bad directory,
/// `None` when the request carries no directory.
/// （中文）请求未携带 directory 时返回 Ok(None)（user 级操作）；
/// 携带但非法时返回 Err(现成的 400 响应)，调用方直接透传。
fn resolve_directory(
    headers: &HeaderMap,
    query: &QueryValues,
) -> Result<Option<PathBuf>, Response> {
    match resolve_optional_project_directory(headers, query) {
        Ok(directory) => Ok(directory),
        Err(error) => Err(error_response(StatusCode::BAD_REQUEST, &error)),
    }
}

/// 构造成功变更响应：操作名转过去式并拼入
/// "Plugin {past}. Restart the engine to apply." 的延迟重启提示。
fn mutation_success(operation: &str) -> Response {
    let past_tense = plugin_mutation_past_tense(operation);
    Json(build_deferred_restart_response(&format!(
        "Plugin {past_tense}. Restart the engine to apply."
    )))
    .into_response()
}

/// `GET /api/config/plugins`：列出当前作用域的插件条目与插件目录文件；
/// 任一列举失败记日志并返回 500。
async fn list_plugins(
    State(_state): State<PluginRouteState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    let result = (|| -> Result<Value, CodedError> {
        Ok(json!({
            "entries": list_plugin_entries(directory.as_deref())?,
            "files": list_plugin_dir_files(directory.as_deref())?,
        }))
    })();
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => {
            tracing::error!("[API:GET /api/config/plugins] Failed: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list plugins")
        }
    }
}

/// 把 RawQuery 提取到的原始查询串解析为 QueryValues 的小适配。
fn parse_query_axum(raw: Option<String>) -> QueryValues {
    super::http_util::parse_query(raw.as_deref())
}

/// `GET /api/config/plugins/registry?specs=a,b&refresh=true`：批量查询
/// spec 状态。specs 逐个 URL 解码、去重，超过 100 个返回 400
/// "too many specs"；npm spec 按包名分组并发查询（经注入的 npm 函数，
/// malformed 的直接旁路），结果交由 build_registry_results 组装。
async fn registry_status(
    State(state): State<PluginRouteState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_optional_project_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };

    let raw_specs = query.joined("specs").unwrap_or_default();
    let specs: Vec<String> = if raw_specs.is_empty() {
        Vec::new()
    } else {
        raw_specs
            .split(',')
            .map(|spec| {
                super::http_util::decode_uri_component(spec).unwrap_or_else(|| spec.to_string())
            })
            .filter(|spec| !spec.is_empty())
            .collect()
    };
    let mut unique_specs: Vec<String> = Vec::new();
    for spec in specs {
        if !unique_specs.contains(&spec) {
            unique_specs.push(spec);
        }
    }
    if unique_specs.len() > 100 {
        return error_response(StatusCode::BAD_REQUEST, "too many specs");
    }

    let refresh = query.first("refresh") == Some("true");
    let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("/"));

    // Group npm specs by package name; malformed specs bypass the registry.
    let mut npm_names: Vec<String> = Vec::new();
    let mut malformed_specs: Vec<String> = Vec::new();
    for spec in &unique_specs {
        if parsed_kind_for_spec(spec) != "npm" {
            continue;
        }
        match parse_npm_spec(spec) {
            NpmSpec::Malformed { .. } => malformed_specs.push(spec.clone()),
            NpmSpec::Name { name, .. } => {
                if !npm_names.contains(&name) {
                    npm_names.push(name);
                }
            }
        }
    }

    let mut jobs = Vec::new();
    for name in &npm_names {
        let lookup = (state.npm)(name.clone(), refresh);
        jobs.push(async move { (name.clone(), lookup.await) });
    }
    let lookups = futures::future::join_all(jobs).await;

    let results = build_registry_results(
        &unique_specs,
        &malformed_specs,
        &lookups,
        &home,
        directory.as_deref(),
    );
    Json(json!({ "results": results })).into_response()
}

/// 组装逐 spec 的状态行：npm-malformed；path-ok / path-missing /
/// path-unreadable（路径以 directory 为基准解析，缺省用 home）；
/// npm-missing-package（registry 404）；npm-network；
/// npm-missing-version（锁定精确 semver 但 registry 无该版本）；
/// npm-ok（含 hasUpdate：当前为精确 semver 且不等于 latest）。
fn build_registry_results(
    unique_specs: &[String],
    malformed_specs: &[String],
    lookups: &[(String, NpmInfo)],
    home: &std::path::Path,
    directory: Option<&std::path::Path>,
) -> Vec<Value> {
    let mut results = Vec::new();
    for spec in unique_specs {
        if malformed_specs.contains(spec) {
            results.push(json!({
                "kind": "npm-malformed",
                "spec": spec,
                "error": "Spec syntax is malformed",
            }));
            continue;
        }

        if parsed_kind_for_spec(spec) == "path" {
            let cwd = directory.unwrap_or(home);
            let absolute_path = parse_path_spec(spec, home, Some(cwd));
            let display = absolute_path.to_string_lossy().into_owned();
            match std::fs::metadata(&absolute_path) {
                Err(_) => results.push(json!({
                    "kind": "path-missing",
                    "spec": spec,
                    "absolutePath": display,
                })),
                Ok(_) => {
                    let readable = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&absolute_path)
                        .is_ok();
                    let kind = if readable {
                        "path-ok"
                    } else {
                        "path-unreadable"
                    };
                    results.push(json!({
                        "kind": kind,
                        "spec": spec,
                        "absolutePath": display,
                    }));
                }
            }
            continue;
        }

        let NpmSpec::Name { name, version } = parse_npm_spec(spec) else {
            continue;
        };
        let Some((_, info)) = lookups.iter().find(|(lookup_name, _)| lookup_name == &name) else {
            continue;
        };
        if !info.ok() {
            if info.status_code() == Some(404) {
                results.push(json!({
                    "kind": "npm-missing-package",
                    "spec": spec,
                    "name": name,
                    "error": info.error_text(),
                }));
                continue;
            }
            let error = if info.is_network() {
                info.error_text().to_string()
            } else {
                format!(
                    "Registry returned {}",
                    info.status_code().unwrap_or_default()
                )
            };
            results.push(json!({
                "kind": "npm-network",
                "spec": spec,
                "error": error,
            }));
            continue;
        }

        let current_version = version.clone();
        if let Some(current) = &current_version
            && is_exact_semver(current)
            && !info.versions().iter().any(|v| v == current)
        {
            results.push(json!({
                "kind": "npm-missing-version",
                "spec": spec,
                "name": name,
                "currentVersion": current,
                "latestVersion": info.latest(),
                "versions": info.versions(),
            }));
            continue;
        }

        let has_update = current_version
            .as_deref()
            .is_some_and(|current| is_exact_semver(current) && Some(current) != info.latest());
        results.push(json!({
            "kind": "npm-ok",
            "spec": spec,
            "name": name,
            "currentVersion": current_version,
            "latestVersion": info.latest(),
            "versions": info.versions(),
            "hasUpdate": has_update,
        }));
    }
    results
}

/// `GET /api/config/plugins/entry/{id}`：读取单条插件条目；
/// id 前缀非法或条目不存在返回 404。
async fn get_entry(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_entry_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to get plugin entry",
            "[API:GET /api/config/plugins/entry/:id] Failed:",
            None,
        );
    }
    match get_plugin_entry(&id, directory.as_deref()) {
        Ok(Some(entry)) => Json(entry).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "Plugin entry not found"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to get plugin entry",
            "[API:GET /api/config/plugins/entry/:id] Failed:",
            None,
        ),
    }
}

/// `POST /api/config/plugins/entry`：按 spec/options/scope 创建插件条目；
/// 已存在 409，参数非法 400，成功返回延迟重启响应。
async fn post_entry(
    State(_state): State<PluginRouteState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    let payload = read_json_body(&headers, &body);
    let result = create_plugin_entry(
        payload.get("spec").unwrap_or(&Value::Null),
        payload.get("options"),
        payload.get("scope").and_then(Value::as_str),
        directory.as_deref(),
    );
    match result {
        Ok(()) => mutation_success("entry creation"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to create plugin entry",
            "[API:POST /api/config/plugins/entry] Failed:",
            Some("entry"),
        ),
    }
}

/// `PATCH /api/config/plugins/entry/{id}`：更新条目的 spec/options
/// （spec 变化会改变条目 id）；不存在 404，成功返回延迟重启响应。
async fn patch_entry(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_entry_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to update plugin entry",
            "[API:PATCH /api/config/plugins/entry/:id] Failed:",
            Some("entry"),
        );
    }
    let payload = read_json_body(&headers, &body);
    let result = update_plugin_entry(
        &id,
        payload.get("spec"),
        payload.get("options"),
        directory.as_deref(),
    );
    match result {
        Ok(()) => mutation_success("entry update"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to update plugin entry",
            "[API:PATCH /api/config/plugins/entry/:id] Failed:",
            Some("entry"),
        ),
    }
}

/// `DELETE /api/config/plugins/entry/{id}`：删除插件条目；
/// 成功返回延迟重启响应。
async fn delete_entry(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_entry_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to delete plugin entry",
            "[API:DELETE /api/config/plugins/entry/:id] Failed:",
            Some("entry"),
        );
    }
    match delete_plugin_entry(&id, directory.as_deref()) {
        Ok(()) => mutation_success("entry deletion"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to delete plugin entry",
            "[API:DELETE /api/config/plugins/entry/:id] Failed:",
            Some("entry"),
        ),
    }
}

/// `GET /api/config/plugins/file/{id}`：读取插件文件，返回
/// fileName/scope/content；id 前缀非法或文件不存在返回 404。
async fn get_file(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_file_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to read plugin file",
            "[API:GET /api/config/plugins/file/:id] Failed:",
            None,
        );
    }
    match read_plugin_dir_file(&id, directory.as_deref()) {
        Ok(Some(file)) => Json(json!({
            "fileName": file.file_name,
            "scope": file.scope,
            "content": file.content,
        }))
        .into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "Plugin file not found"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to read plugin file",
            "[API:GET /api/config/plugins/file/:id] Failed:",
            None,
        ),
    }
}

/// `POST /api/config/plugins/file`：按 fileName/content/scope（默认 user）
/// 创建插件文件；文件名非法 400，已存在 409。
async fn post_file(
    State(_state): State<PluginRouteState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    let payload = read_json_body(&headers, &body);
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let file_name = payload
        .get("fileName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let id = encode_plugin_id("file", &format!("{scope}:{file_name}"));
    let result = (|| -> Result<(), CodedError> {
        validate_file_id(&id)?;
        write_plugin_dir_file(
            payload.get("fileName").unwrap_or(&Value::Null),
            payload.get("content").unwrap_or(&Value::Null),
            payload.get("scope").and_then(Value::as_str),
            directory.as_deref(),
            false,
        )
    })();
    match result {
        Ok(()) => mutation_success("file creation"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to create plugin file",
            "[API:POST /api/config/plugins/file] Failed:",
            Some("file"),
        ),
    }
}

/// `PUT /api/config/plugins/file/{id}`：覆写已存在文件的内容，
/// 沿用原 fileName 与 scope；文件不存在 404。
async fn put_file(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_file_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to update plugin file",
            "[API:PUT /api/config/plugins/file/:id] Failed:",
            Some("file"),
        );
    }
    let payload = read_json_body(&headers, &body);
    let existing = match read_plugin_dir_file(&id, directory.as_deref()) {
        Ok(Some(file)) => file,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "Plugin file not found"),
        Err(error) => {
            return handle_plugin_error(
                error,
                "Failed to update plugin file",
                "[API:PUT /api/config/plugins/file/:id] Failed:",
                Some("file"),
            );
        }
    };
    let result = write_plugin_dir_file(
        &Value::from(existing.file_name),
        payload.get("content").unwrap_or(&Value::Null),
        Some(existing.scope.as_str()),
        directory.as_deref(),
        true,
    );
    match result {
        Ok(()) => mutation_success("file update"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to update plugin file",
            "[API:PUT /api/config/plugins/file/:id] Failed:",
            Some("file"),
        ),
    }
}

/// `DELETE /api/config/plugins/file/{id}`：删除插件文件；
/// 成功返回延迟重启响应。
async fn delete_file(
    State(_state): State<PluginRouteState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let query = parse_query_axum(query);
    let directory = match resolve_directory(&headers, &query) {
        Ok(directory) => directory,
        Err(response) => return response,
    };
    if let Err(error) = validate_file_id(&id) {
        return handle_plugin_error(
            error,
            "Failed to delete plugin file",
            "[API:DELETE /api/config/plugins/file/:id] Failed:",
            Some("file"),
        );
    }
    match delete_plugin_dir_file(&id, directory.as_deref()) {
        Ok(()) => mutation_success("file deletion"),
        Err(error) => handle_plugin_error(
            error,
            "Failed to delete plugin file",
            "[API:DELETE /api/config/plugins/file/:id] Failed:",
            Some("file"),
        ),
    }
}

/// 测试辅助模块：暴露可注入 npm 查询的路由构造器与现成 mock，
/// 对应 JS 测试里的依赖注入接缝。
pub(crate) mod testing {
    use super::*;

    /// Test app with an injectable npm lookup (the JS DI seam).
/// （中文）以注入的 npm 查询函数构建插件路由（不触碰真实网络）。
    pub(crate) fn routes_with_npm(npm: NpmLookup) -> Router {
        router_with_state(PluginRouteState { npm })
    }

/// 构造恒成功返回指定 latest 与 versions 的 mock npm 查询函数。
    pub(crate) fn ok_npm(latest: &str, versions: &[&str]) -> NpmLookup {
        let latest = latest.to_string();
        let versions: Vec<String> = versions.iter().map(|v| v.to_string()).collect();
        Arc::new(move |_name, _force| {
            let latest = latest.clone();
            let versions = versions.clone();
            Box::pin(async move {
                NpmInfo::Ok {
                    latest: Some(latest),
                    versions,
                    dist_tags: Default::default(),
                }
            })
        })
    }
}

/// 插件路由的行为契约测试：空列表形状、registry 各分类状态行与数量
/// 上限、entry/file 的 CRUD 往返与错误码映射。
#[cfg(test)]
mod tests {
    use super::testing::{ok_npm, routes_with_npm};
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

/// 测试环境守护：持有全局互斥锁并记录 HOME / OPENCODE_CONFIG 原值，
/// Drop 时恢复。
    struct EnvGuard {
/// 全局测试环境互斥锁守卫，串行化并发测试的环境变量修改。
        _guard: std::sync::MutexGuard<'static, ()>,
/// 测试前 HOME 的原值；None 表示原本未设置。
        previous_home: Option<String>,
/// 测试前 OPENCODE_CONFIG 的原值；None 表示原本未设置。
        previous_config: Option<String>,
    }

/// Drop 实现：恢复被测试改写的环境变量。
    impl Drop for EnvGuard {
/// 逐个恢复 HOME 与 OPENCODE_CONFIG 到测试前的取值。
        fn drop(&mut self) {
            restore("HOME", &self.previous_home);
            restore("OPENCODE_CONFIG", &self.previous_config);
        }
    }

/// 恢复单个环境变量：有记录值则设置回去，无记录则移除。
    fn restore(key: &str, value: &Option<String>) {
        match value {
            Some(value) => unsafe {
                std::env::set_var(key, value);
            },
            None => unsafe {
                std::env::remove_var(key);
            },
        }
    }

/// EnvGuard 的加锁与环境切换构造。
    impl EnvGuard {
/// 加全局锁并按 custom 切换环境：Some(path) 时 HOME 取其父目录、
/// OPENCODE_CONFIG 指向该文件；None 时两者清空。
        fn lock(custom: Option<PathBuf>) -> Self {
            let guard = super::super::http_util::TEST_ENV_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous_home = std::env::var("HOME").ok();
            let previous_config = std::env::var("OPENCODE_CONFIG").ok();
            match custom {
                Some(path) => {
                    let home = path
                        .parent()
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_default();
                    unsafe {
                        std::env::set_var("HOME", home.to_string_lossy().as_ref());
                        std::env::set_var("OPENCODE_CONFIG", path.to_string_lossy().as_ref());
                    }
                }
                None => unsafe {
                    std::env::remove_var("HOME");
                    std::env::remove_var("OPENCODE_CONFIG");
                },
            }
            Self {
                _guard: guard,
                previous_home,
                previous_config,
            }
        }
    }

/// 创建带 pid 与计数后缀的唯一临时目录。
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-plugin-routes-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

/// 进程内原子递增计数，为临时目录生成唯一后缀。
    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        // 进程内原子计数器：并行测试也能拿到不同目录名。
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

/// 读出响应 body 并解析为 JSON；失败即 panic（仅测试断言使用）。
    async fn json_body(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

/// 便捷请求封装：oneshot 执行请求并返回 (状态码, JSON body)。
    async fn send(app: &Router, request: axum::http::Request<Body>) -> (StatusCode, Value) {
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        (status, json_body(response).await)
    }

/// 构造带 JSON body 与 content-type: application/json 的请求。
    fn json_request(method: &str, uri: &str, body: Value) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).expect("json")))
            .expect("request")
    }

/// 测试夹具：临时根目录与 user 级配置文件路径，Drop 时清理目录。
    struct Fixture {
/// 测试临时根目录（内含 project 子目录）。
        root: PathBuf,
/// user 级 opencode 配置文件路径（HOME 指向 root 时生效）。
        user_config: PathBuf,
    }

/// Drop 实现：删除夹具的整个临时目录。
    impl Drop for Fixture {
/// 清理临时根目录（忽略错误）。
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

/// 创建夹具：临时根目录、user 配置文件路径，并建立 project 子目录。
    async fn fixture(tag: &str) -> Fixture {
        let root = unique_dir(tag);
        let user_config = root.join("user-opencode.json");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        Fixture {
            root,
            user_config: user_config.clone(),
        }
    }

/// 验证：无任何插件时 GET /api/config/plugins 返回空 entries/files。
    #[tokio::test]
    async fn get_plugins_empty_returns_entries_and_files() {
        let fixture = fixture("empty").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let (status, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "entries": [], "files": [] }));
    }

/// 验证：spec 锁定旧版本而 registry 存在更新版本时，
/// 返回 npm-ok 且 hasUpdate=true 并携带 latest/versions。
    #[tokio::test]
    async fn registry_reports_update_behind_latest() {
        let fixture = fixture("npm-update").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("2.0.0", &["1.0.0", "2.0.0"]));

        let (status, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=foo@1.0.0")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["results"][0],
            json!({
                "kind": "npm-ok",
                "spec": "foo@1.0.0",
                "name": "foo",
                "currentVersion": "1.0.0",
                "latestVersion": "2.0.0",
                "versions": ["1.0.0", "2.0.0"],
                "hasUpdate": true,
            })
        );
    }

/// 验证：registry 对 npm-missing-version、npm-missing-package、
/// npm-network、npm-malformed 的分类输出，以及重复 spec 去重。
    #[tokio::test]
    async fn registry_covers_missing_version_package_network_and_malformed() {
        let fixture = fixture("npm-kinds").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));

        let app = routes_with_npm(ok_npm("2.0.0", &["1.0.0", "2.0.0"]));
        let (_, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=foo@99.99.99")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(body["results"][0]["kind"], json!("npm-missing-version"));
        assert_eq!(body["results"][0]["currentVersion"], json!("99.99.99"));

        let app = routes_with_npm(Arc::new(|_name, _force| {
            Box::pin(async {
                NpmInfo::Err {
                    network: false,
                    status: Some(404),
                    error: "Package not found".into(),
                }
            })
        }));
        let (_, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=nonexistent@1.0.0")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(
            body["results"][0],
            json!({
                "kind": "npm-missing-package",
                "spec": "nonexistent@1.0.0",
                "name": "nonexistent",
                "error": "Package not found",
            })
        );

        let app = routes_with_npm(Arc::new(|_name, _force| {
            Box::pin(async {
                NpmInfo::Err {
                    network: true,
                    status: None,
                    error: "socket closed".into(),
                }
            })
        }));
        let (_, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=foo@1.0.0")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(
            body["results"][0],
            json!({ "kind": "npm-network", "spec": "foo@1.0.0", "error": "socket closed" })
        );

        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));
        let (_, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=%40%40malformed")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(
            body["results"][0],
            json!({ "kind": "npm-malformed", "spec": "@@malformed", "error": "Spec syntax is malformed" })
        );

        let (_, body) = send(
            &app,
            axum::http::Request::get("/api/config/plugins/registry?specs=foo&specs=foo")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(body["results"].as_array().expect("results").len(), 1);
        assert_eq!(body["results"][0]["currentVersion"], Value::Null);
        assert_eq!(body["results"][0]["hasUpdate"], json!(false));
    }

/// 验证：本地路径 spec 分别报告 path-ok（存在且可读）
/// 与 path-missing（不存在）。
    #[tokio::test]
    async fn registry_reports_path_status() {
        let fixture = fixture("paths").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let plugin_file = fixture.root.join("plugin.js");
        std::fs::write(&plugin_file, "// plugin").expect("write");

        let uri = format!(
            "/api/config/plugins/registry?specs={}",
            super::super::http_util::encode_uri_component(&plugin_file.to_string_lossy())
        );
        let (status, body) = send(
            &app,
            axum::http::Request::get(&uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["results"][0],
            json!({
                "kind": "path-ok",
                "spec": plugin_file.to_string_lossy().as_ref(),
                "absolutePath": plugin_file.to_string_lossy().as_ref(),
            })
        );

        let missing = "/nonexistent/__path/xyz.js";
        let uri = format!(
            "/api/config/plugins/registry?specs={}",
            super::super::http_util::encode_uri_component(missing)
        );
        let (_, body) = send(
            &app,
            axum::http::Request::get(&uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(body["results"][0]["kind"], json!("path-missing"));
        assert_eq!(body["results"][0]["absolutePath"], json!(missing));
    }

/// 验证：去重后超过 100 个 spec 时返回 400 "too many specs"。
    #[tokio::test]
    async fn registry_rejects_more_than_100_unique_specs() {
        let fixture = fixture("limit").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let specs: Vec<String> = (0..101).map(|index| format!("pkg-{index}")).collect();
        let uri = format!("/api/config/plugins/registry?specs={}", specs.join(","));
        let (status, body) = send(
            &app,
            axum::http::Request::get(&uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": "too many specs" }));
    }

/// 验证：插件条目创建/更新/删除的完整往返、重复创建 409、成功响应的
/// 延迟重启形状与磁盘配置最终为空。
    #[tokio::test]
    async fn entry_crud_round_trip_shapes() {
        let fixture = fixture("entry-crud").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let (status, body) = send(
            &app,
            json_request(
                "POST",
                "/api/config/plugins/entry",
                json!({ "spec": "a", "scope": "user" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "success": true,
                "requiresReload": false,
                "requiresRestart": true,
                "restartDeferred": true,
                "message": "Plugin entry created. Restart the engine to apply.",
            })
        );

        let (_, listed) = send(
            &app,
            axum::http::Request::get("/api/config/plugins")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(listed["entries"].as_array().expect("entries").len(), 1);
        assert_eq!(listed["entries"][0]["spec"], json!("a"));
        assert_eq!(listed["entries"][0]["scope"], json!("user"));
        let id = listed["entries"][0]["id"].as_str().expect("id").to_string();

        let (status, body) = send(
            &app,
            json_request(
                "PATCH",
                &format!(
                    "/api/config/plugins/entry/{}",
                    super::super::http_util::encode_uri_component(&id)
                ),
                json!({ "spec": "b" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["message"],
            json!("Plugin entry updated. Restart the engine to apply.")
        );

        let (status, body) = send(
            &app,
            json_request(
                "POST",
                "/api/config/plugins/entry",
                json!({ "spec": "b", "scope": "user" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("already exists")
        );

        // The PATCH rewrote the spec, so the entry id changed — re-list.
        let (_, listed) = send(
            &app,
            axum::http::Request::get("/api/config/plugins")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let id = listed["entries"][0]["id"].as_str().expect("id").to_string();
        let (status, body) = send(
            &app,
            axum::http::Request::delete(&format!(
                "/api/config/plugins/entry/{}",
                super::super::http_util::encode_uri_component(&id)
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["message"],
            json!("Plugin entry deleted. Restart the engine to apply.")
        );

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.user_config).expect("read"))
                .expect("json");
        assert!(on_disk.get("plugin").is_none());

        let (_, listed) = send(
            &app,
            axum::http::Request::get("/api/config/plugins")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(listed["entries"], json!([]));
    }

/// 验证：PATCH 不存在的条目 id 返回 404。
    #[tokio::test]
    async fn patch_unknown_entry_returns_404() {
        let fixture = fixture("patch-404").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let id = encode_plugin_id("config", "user:missing");
        let (status, body) = send(
            &app,
            json_request(
                "PATCH",
                &format!(
                    "/api/config/plugins/entry/{}",
                    super::super::http_util::encode_uri_component(&id)
                ),
                json!({ "spec": "b" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().expect("error").contains("not found"));
    }

/// 验证：插件文件创建/更新/读取/删除的往返形状、重复创建 409
/// 与磁盘内容随之变化。
    #[tokio::test]
    async fn file_crud_round_trip_shapes() {
        let fixture = fixture("file-crud").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let (status, body) = send(
            &app,
            json_request(
                "POST",
                "/api/config/plugins/file",
                json!({ "fileName": "test.js", "content": "//x", "scope": "user" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["message"],
            json!("Plugin file created. Restart the engine to apply.")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("plugins").join("test.js")).expect("read"),
            "//x"
        );

        let (status, body) = send(
            &app,
            json_request(
                "POST",
                "/api/config/plugins/file",
                json!({ "fileName": "test.js", "content": "//again", "scope": "user" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("already exists")
        );

        let (_, listed) = send(
            &app,
            axum::http::Request::get("/api/config/plugins")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        let id = listed["files"][0]["id"].as_str().expect("id").to_string();

        let (status, body) = send(
            &app,
            json_request(
                "PUT",
                &format!(
                    "/api/config/plugins/file/{}",
                    super::super::http_util::encode_uri_component(&id)
                ),
                json!({ "content": "//y" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["message"],
            json!("Plugin file updated. Restart the engine to apply.")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("plugins").join("test.js")).expect("read"),
            "//y"
        );

        let (_, file) = send(
            &app,
            axum::http::Request::get(&format!(
                "/api/config/plugins/file/{}",
                super::super::http_util::encode_uri_component(&id)
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await;
        assert_eq!(
            file,
            json!({ "fileName": "test.js", "scope": "user", "content": "//y" })
        );

        let (status, body) = send(
            &app,
            axum::http::Request::delete(&format!(
                "/api/config/plugins/file/{}",
                super::super::http_util::encode_uri_component(&id)
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["message"],
            json!("Plugin file deleted. Restart the engine to apply.")
        );
        assert!(!fixture.root.join("plugins").join("test.js").exists());
    }

/// 验证：fileName 含路径穿越（../）时创建插件文件返回 400。
    #[tokio::test]
    async fn post_file_with_invalid_name_returns_400() {
        let fixture = fixture("file-400").await;
        let _env = EnvGuard::lock(Some(fixture.user_config.clone()));
        let app = routes_with_npm(ok_npm("1.0.0", &["1.0.0"]));

        let (status, body) = send(
            &app,
            json_request(
                "POST",
                "/api/config/plugins/file",
                json!({ "fileName": "../escape.js", "content": "//x", "scope": "user" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("Plugin file name")
        );
    }
}
