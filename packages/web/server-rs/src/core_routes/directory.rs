//! `POST /api/opencode/directory` from `opencode/routes.js`, backed by the
//! `project-directory-runtime.js` subset it needs (`validateDirectoryPath`,
//! `resolveDirectoryCandidate`) and a minimal honest settings persistence
//! (`settings-runtime.js` read/write shape). The canonical settings runtime is
//! owned by the `settings` module port; until it lands this module reads and
//! writes `settings.json` directly (atomic temp+rename, `0600`, unknown-field
//! preserving) for exactly the three keys the route touches:
//! `projects`, `activeProjectId`, `lastDirectory`.
//! opencode/routes.js 的 `POST /api/opencode/directory`，依赖
//! project-directory-runtime.js 的所需子集（validateDirectoryPath、
//! resolveDirectoryCandidate）与最小可用的 settings 持久化
//! （settings-runtime.js 的读写形态）。正式 settings 运行时归 settings
//! 模块移植；落地前这里直接读写 `settings.json`（临时文件 + rename
//! 原子写、0600 权限、保留未知字段），且只触碰路由涉及的三个键：
//! `projects`、`activeProjectId`、`lastDirectory`。

use std::path::{Component, Path, PathBuf};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Map, Value};

use crate::context::RouterContext;
use crate::error::AppError;

/// Lenient JSON body read: a blank/absent body is `{}` (Express leaves
/// `req.body` undefined for non-JSON requests); a malformed JSON body is 400.
/// 宽松读取 JSON 请求体：空白/缺省体视为 `{}`（Express 对非 JSON 请求
/// 把 req.body 留空）；非法 JSON 映射为 400。
pub(crate) async fn read_json_body(request: Request) -> Result<Value, AppError> {
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|error| AppError::bad_request(format!("failed to read request body: {error}")))?;
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::bad_request(format!("invalid JSON body: {error}")))
}

/// `normalizeDirectoryPath` (settings-normalization-runtime.js): trim, strip
/// wrapping quotes, expand a leading `~`.
/// settings-normalization-runtime.js 的 `normalizeDirectoryPath`：trim、
/// 剥掉成对的包裹引号、展开开头的 `~`。
pub(crate) fn normalize_directory_path(value: &str) -> String {
    let mut trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed = trimmed[1..trimmed.len() - 1].trim();
    }
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    if trimmed == "~" {
        return crate::config::home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        && let Some(home) = crate::config::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    trimmed.to_string()
}

/// `path.resolve` — absolute against the cwd with lexical `.`/`..`
/// normalization (no symlink resolution).
/// `path.resolve` 的等价实现：相对路径基于 cwd，词法归一 `.`/`..`
/// 段（不解析符号链接）。
pub(crate) fn lexical_resolve(path: &str) -> PathBuf {
    let raw = Path::new(path);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(raw)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// `createProjectIdFromPath` (projects/project-id.js): `path_` +
/// base64url(normalized path).
/// projects/project-id.js 的 `createProjectIdFromPath`：反斜杠折算为
/// `/`、去掉尾部斜杠后输出 `path_` + base64url(路径)。
pub(crate) fn create_project_id_from_path(project_path: &str) -> String {
    let normalized = project_path.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/').to_string();
    let normalized = if normalized.is_empty() {
        project_path.to_string()
    } else {
        normalized
    };
    let trimmed = normalized.trim().to_string();
    if trimmed.is_empty() {
        return String::new();
    }
    format!(
        "path_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(trimmed.as_bytes())
    )
}

/// validate_directory_path 的成功结果。
#[derive(Debug)]
pub struct ValidatedDirectory {
    /// Canonical (realpath) directory — what gets persisted.
    /// 规范（realpath）目录——持久化使用的最终形态。
    pub directory: PathBuf,
    /// Pre-realpath candidate the caller asked for.
    /// 调用方原始请求（realpath 之前）的词法解析结果。
    pub requested_directory: PathBuf,
}

/// `validateDirectoryPath` (project-directory-runtime.js): required, must be an
/// existing directory; returns the realpath.
/// project-directory-runtime.js 的 `validateDirectoryPath`：路径必填且
/// 必须是已存在的目录，成功时返回 realpath；错误消息与 JS 逐字对齐
/// （not found / permission denied / not a directory）。
pub fn validate_directory_path(candidate: &str) -> Result<ValidatedDirectory, String> {
    let normalized = normalize_directory_path(candidate);
    if normalized.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = lexical_resolve(&normalized);
    match std::fs::metadata(&resolved) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err("Specified path is not a directory".to_string());
            }
        }
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => "Directory not found".to_string(),
                std::io::ErrorKind::PermissionDenied => "Access to directory denied".to_string(),
                _ => "Failed to validate directory".to_string(),
            });
        }
    }
    match std::fs::canonicalize(&resolved) {
        Ok(directory) => Ok(ValidatedDirectory {
            directory,
            requested_directory: resolved,
        }),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

/// `normalizePathForPersistence` subset: resolve to the canonical path when it
/// exists, falling back to the lexical resolution (matches `safeRealpathSync`).
/// `normalizePathForPersistence` 子集：路径存在则取 canonical 形态，
/// 否则回退词法解析（对齐 `safeRealpathSync`）；Windows 下统一反斜杠
/// 并大写盘符。
fn normalize_path_for_persistence(raw: &str) -> String {
    let normalized = normalize_directory_path(raw);
    if normalized.is_empty() {
        return String::new();
    }
    let resolved = lexical_resolve(&normalized);
    let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    let text = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        uppercase_drive_letter(&text.replace('/', "\\"))
    } else {
        text
    }
}

/// Windows 盘符小写时转为大写（`c:\x` → `C:\x`），其余原样返回。
fn uppercase_drive_letter(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
        let mut out = path.to_string();
        let upper = bytes[0].to_ascii_uppercase() as char;
        out.replace_range(0..1, &upper.to_string());
        out
    } else {
        path.to_string()
    }
}

/// 取 JSON 字符串字段：trim 后非空返回 Some，否则 None。
fn non_empty_trimmed(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|value| value.as_str())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// 取 JSON 数值字段：有限且非负才返回 Some。
fn finite_non_negative_number(value: Option<&Value>) -> Option<f64> {
    let number = value.and_then(|value| value.as_f64())?;
    if number.is_finite() && number >= 0.0 {
        Some(number)
    } else {
        None
    }
}

/// JS `Date.now()` timestamps serialize as integers — keep integral values
/// integral instead of rendering `123.0`.
/// JS `Date.now()` 时间戳序列化为整数——整值保持整数，避免渲染成
/// `123.0`。
fn timestamp_json_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// `sanitizeProjects` (settings-normalization-runtime.js): normalize ids/paths,
/// drop entries without both, dedupe by id and path, keep the known optional
/// fields in their documented shape.
/// settings-normalization-runtime.js 的 `sanitizeProjects`：归一
/// id/path，缺任一即丢弃，按 id 与 path 去重，已知可选字段按文档形态
/// 保留，未知字段剔除。
pub(crate) fn sanitize_projects(input: Option<&Value>) -> Vec<Value> {
    let Some(entries) = input.and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut result: Vec<Value> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut seen_paths: Vec<String> = Vec::new();
    for entry in entries {
        let Some(candidate) = entry.as_object() else {
            continue;
        };
        let id = non_empty_trimmed(candidate.get("id"));
        let normalized_path = non_empty_trimmed(candidate.get("path"))
            .map(|raw| normalize_path_for_persistence(&raw))
            .filter(|path| !path.is_empty());
        let (Some(id), Some(normalized_path)) = (id, normalized_path) else {
            continue;
        };
        if seen_ids.contains(&id) || seen_paths.contains(&normalized_path) {
            continue;
        }
        seen_ids.push(id.clone());
        seen_paths.push(normalized_path.clone());

        let mut project = Map::new();
        project.insert("id".into(), Value::String(id));
        project.insert("path".into(), Value::String(normalized_path));
        if let Some(label) = non_empty_trimmed(candidate.get("label")) {
            project.insert("label".into(), Value::String(label));
        }
        if let Some(icon) = non_empty_trimmed(candidate.get("icon")) {
            project.insert("icon".into(), Value::String(icon));
        }
        // Hex colors lowercase; anything else drops the field unless the input
        // explicitly nulls it.
        let icon_background = candidate.get("iconBackground");
        if icon_background.is_some_and(|value| value.is_null()) {
            project.insert("iconBackground".into(), Value::Null);
        } else if let Some(color) = non_empty_trimmed(icon_background).filter(|value| {
            let bytes = value.as_bytes();
            (bytes.len() == 4 || bytes.len() == 7)
                && bytes[0] == b'#'
                && bytes[1..].iter().all(|byte| byte.is_ascii_hexdigit())
        }) {
            project.insert(
                "iconBackground".into(),
                Value::String(color.to_ascii_lowercase()),
            );
        }
        if let Some(color) = non_empty_trimmed(candidate.get("color")) {
            project.insert("color".into(), Value::String(color));
        }
        if let Some(default_model) =
            non_empty_trimmed(candidate.get("defaultModel")).filter(|model| model.contains('/'))
        {
            project.insert("defaultModel".into(), Value::String(default_model.clone()));
            if let Some(default_variant) = non_empty_trimmed(candidate.get("defaultVariant")) {
                project.insert("defaultVariant".into(), Value::String(default_variant));
            }
        }
        if let Some(added_at) = finite_non_negative_number(candidate.get("addedAt")) {
            project.insert("addedAt".into(), timestamp_json_number(added_at));
        }
        if let Some(last_opened_at) = finite_non_negative_number(candidate.get("lastOpenedAt")) {
            project.insert("lastOpenedAt".into(), timestamp_json_number(last_opened_at));
        }
        if let Some(icon_image) = candidate.get("iconImage") {
            if icon_image.is_null() {
                project.insert("iconImage".into(), Value::Null);
            } else if let Some(record) = icon_image.as_object() {
                let mime = non_empty_trimmed(record.get("mime"));
                let updated_at = finite_non_negative_number(record.get("updatedAt"))
                    .filter(|value| *value > 0.0);
                let source = non_empty_trimmed(record.get("source"))
                    .filter(|value| value == "custom" || value == "auto");
                if let (Some(mime), Some(updated_at), Some(source)) = (mime, updated_at, source) {
                    project.insert(
                        "iconImage".into(),
                        serde_json::json!({
                            "mime": mime,
                            "updatedAt": updated_at,
                            "source": source,
                        }),
                    );
                }
            }
        }
        if let Some(collapsed) = candidate
            .get("sidebarCollapsed")
            .filter(|value| value.is_boolean())
        {
            project.insert("sidebarCollapsed".into(), collapsed.clone());
        }
        result.push(Value::Object(project));
    }
    result
}

/// `validateProjectEntries` (settings-runtime.js): drop entries whose path is
/// missing or not a directory; permission/transient fs errors KEEP the entry
/// rather than silently losing it from the user's list.
/// settings-runtime.js 的 `validateProjectEntries`：丢弃 path 缺失或
/// 不是目录的条目；权限/瞬时 fs 错误则保留条目，避免用户的列表被
/// 静默清空。
fn validate_project_entries(projects: &[Value]) -> Vec<Value> {
    projects
        .iter()
        .filter(|project| {
            let Some(path) = project.get("path").and_then(|value| value.as_str()) else {
                tracing::warn!("[validateProjectEntries] Dropping project entry with missing or empty path");
                return false;
            };
            if path.is_empty() {
                tracing::warn!("[validateProjectEntries] Dropping project entry with missing or empty path");
                return false;
            }
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => true,
                Ok(_) => {
                    tracing::warn!("[validateProjectEntries] Dropping project — path is not a directory: {path}");
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!("[validateProjectEntries] Dropping project — directory no longer exists: {path}");
                    false
                }
                Err(_) => true,
            }
        })
        .cloned()
        .collect()
}
/// `readSettingsFromDisk` (lenient): every failure maps to `{}`.
/// `readSettingsFromDisk`（宽松语义）：读取失败、解析失败或非对象都
/// 归一为空对象。
pub(crate) fn read_settings(settings_path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|parsed| match parsed {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// `writeSettingsToDisk`: mkdir `0700`, atomic tmp+rename, `0600` file mode,
/// 2-space pretty JSON like `JSON.stringify(settings, null, 2)`.
/// `writeSettingsToDisk`：mkdir 0700、临时文件 + rename 原子写、0600
/// 文件权限、两空格缩进的 pretty JSON（同 JSON.stringify(settings,
/// null, 2)）；rename 失败时清理临时文件。
pub(crate) fn write_settings(
    settings_path: &Path,
    settings: &Map<String, Value>,
) -> std::io::Result<()> {
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = settings_path.with_file_name(format!(
        "{}.tmp-{}-{}-{}",
        settings_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        crate::core_routes::now_unix_millis(),
        format!("{:x}", rand::random::<u64>())
    ));
    let payload = serde_json::to_string_pretty(&Value::Object(settings.clone()))
        .unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&tmp, payload)?;
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    let rename_result = std::fs::rename(&tmp, settings_path);
    if rename_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        let _ = std::fs::set_permissions(settings_path, std::fs::Permissions::from_mode(0o600));
    }
    rename_result
}

/// 该 data 目录下 settings.json 的路径。
fn settings_path(ctx: &RouterContext) -> PathBuf {
    ctx.config.data_dir.join("settings.json")
}

/// `POST /api/opencode/directory`.
/// `POST /api/opencode/directory`：校验路径（可选先递归创建目录）→
/// 更新 projects/activeProjectId/lastDirectory → 原子写盘 → 返回完整
/// settings；校验失败 400、建目录或写盘失败 500。
pub(crate) async fn set_directory(State(ctx): State<RouterContext>, request: Request) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };

    let requested_path = body
        .get("path")
        .and_then(|value| value.as_str())
        .map(|path| path.trim().to_string())
        .unwrap_or_default();
    if requested_path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Path is required" })),
        )
            .into_response();
    }

    if body.get("create") == Some(&Value::Bool(true)) {
        let target = lexical_resolve(&requested_path);
        if let Err(error) = std::fs::create_dir_all(&target) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    }

    let validated = match validate_directory_path(&requested_path) {
        Ok(validated) => validated,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error })),
            )
                .into_response();
        }
    };
    let resolved_path = validated.directory.to_string_lossy().into_owned();

    let path = settings_path(&ctx);
    let current = read_settings(&path);
    let mut projects = sanitize_projects(current.get("projects"));
    let existing = projects
        .iter()
        .find(|project| {
            project.get("path").and_then(|value| value.as_str()) == Some(resolved_path.as_str())
        })
        .cloned();

    let active_project_id = match &existing {
        Some(project) => project.get("id").cloned().unwrap_or(Value::Null),
        None => {
            let now = crate::core_routes::now_unix_millis();
            let id = create_project_id_from_path(&resolved_path);
            projects.push(serde_json::json!({
                "id": id,
                "path": resolved_path,
                "addedAt": now,
                "lastOpenedAt": now,
            }));
            projects
                .last()
                .and_then(|project| project.get("id").cloned())
                .unwrap_or(Value::Null)
        }
    };
    // persistSettings validates every project entry whenever an update touches
    // the projects list (`validateProjectEntries`), then re-points a dropped
    // active project at the first survivor (or clears it when none remain).
    let projects = validate_project_entries(&projects);
    let active_project_id = if projects
        .iter()
        .any(|project| project.get("id") == Some(&active_project_id))
    {
        active_project_id
    } else {
        projects
            .first()
            .and_then(|project| project.get("id").cloned())
            .unwrap_or(Value::Null)
    };

    let mut next = current.clone();
    next.insert("projects".into(), Value::Array(projects));
    if !active_project_id.is_null() {
        next.insert("activeProjectId".into(), active_project_id);
    }
    next.insert("lastDirectory".into(), Value::String(resolved_path.clone()));

    if let Err(error) = write_settings(&path, &next) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": if error.to_string().is_empty() { "Failed to update working directory".to_string() } else { error.to_string() },
            })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "restarted": false,
            "path": resolved_path,
            "settings": Value::Object(next),
        })),
    )
        .into_response()
}

/// directory 路由与路径工具的测试。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::router;
    use crate::core_routes::tests::{json_response, temp_dir, test_ctx};
    use crate::engine::EngineState;
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};

/// 以 JSON body POST /api/opencode/directory 并返回 (状态码, body)。
    async fn post_directory(ctx: crate::context::RouterContext, body: &str) -> (StatusCode, Value) {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/api/opencode/directory")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        json_response(router(ctx), request).await
    }

/// 验证路径归一：剥引号、展开 `~`、纯空白返回空串。
    #[test]
    fn normalize_directory_path_expands_quotes_and_home() {
        assert_eq!(normalize_directory_path("  '/tmp/x' "), "/tmp/x");
        assert_eq!(
            normalize_directory_path("\"~/proj\""),
            format!(
                "{}/proj",
                crate::config::home_dir()
                    .map(|home| home.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        );
        assert_eq!(normalize_directory_path("   "), "");
        assert_eq!(normalize_directory_path("/plain/path"), "/plain/path");
    }

/// 验证词法解析消化 `.`/`..` 段并基于 cwd 补全相对路径。
    #[test]
    fn lexical_resolve_normalizes_relative_segments() {
        let base = std::env::current_dir().expect("cwd");
        let resolved = lexical_resolve("a/./b/../c");
        assert_eq!(resolved, base.join("a/c"));
        assert_eq!(
            lexical_resolve("/var/tmp/../log"),
            PathBuf::from("/var/log")
        );
    }

/// 验证项目 id 是归一路径的 base64url 编码（预置期望值，防止实现
/// 回声）。
    #[test]
    fn project_id_is_base64url_of_normalized_path() {
        // base64url("/projects/one") — precomputed so the test pins the
        // encoding instead of echoing the implementation.
        assert_eq!(
            create_project_id_from_path("/projects/one/"),
            "path_L3Byb2plY3RzL29uZQ"
        );
        assert_eq!(create_project_id_from_path("  "), "");
        // Backslashes are folded to slashes before encoding.
        assert_eq!(
            create_project_id_from_path("C:\\work\\repo"),
            create_project_id_from_path("C:/work/repo")
        );
        // Encoding alphabet pins: URL-safe chars, no padding.
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"/projects/one"),
            "L3Byb2plY3RzL29uZQ"
        );
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&[251u8, 255]),
            "-_8"
        );
    }

/// 验证目录校验的错误消息与 JS 逐字一致，成功时返回 realpath 与
/// 原始请求路径。
    #[test]
    fn validate_directory_path_errors_match_js_shapes() {
        let root = temp_dir("validate");
        let missing = root.join("definitely-missing-dir");
        assert_eq!(
            validate_directory_path(missing.to_str().unwrap()).unwrap_err(),
            "Directory not found"
        );
        assert_eq!(
            validate_directory_path("").unwrap_err(),
            "Directory parameter is required"
        );
        assert_eq!(
            validate_directory_path("   ").unwrap_err(),
            "Directory parameter is required"
        );

        let file = root.join("plain-file.txt");
        std::fs::write(&file, b"data").expect("write file");
        assert_eq!(
            validate_directory_path(file.to_str().unwrap()).unwrap_err(),
            "Specified path is not a directory"
        );

        let dir = root.join("project");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let validated = validate_directory_path(dir.to_str().unwrap()).expect("valid dir");
        // macOS temp dirs live behind a /private symlink — validate resolves it.
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(validated.directory, canonical);
        assert_eq!(validated.requested_directory, dir);
    }

/// 验证 sanitize_projects 的去重、未知字段剔除与可选字段保留行为。
    #[test]
    fn sanitize_projects_dedupes_and_preserves_known_fields() {
        let input = serde_json::json!([
            { "id": "p1", "path": "/tmp/a", "label": " Alpha ", "addedAt": 123, "unknownJunk": true },
            { "id": "p1", "path": "/tmp/other" },
            { "id": "p2", "path": "/tmp/a" },
            { "id": "", "path": "/tmp/b" },
            { "id": "p3", "path": "   " },
            { "id": "p4", "path": "/tmp/b", "defaultModel": "anthropic/claude", "defaultVariant": "high",
              "iconBackground": "#AA BB CC", "sidebarCollapsed": true, "iconImage": null },
            "not-an-object"
        ]);
        let projects = sanitize_projects(Some(&input));
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0]["id"], "p1");
        assert_eq!(projects[0]["path"], "/tmp/a");
        assert_eq!(projects[0]["label"], "Alpha");
        assert_eq!(projects[0]["addedAt"], 123);
        assert!(projects[0].get("unknownJunk").is_none());
        assert_eq!(projects[1]["id"], "p4");
        assert_eq!(projects[1]["defaultModel"], "anthropic/claude");
        assert_eq!(projects[1]["defaultVariant"], "high");
        // "#AA BB CC" is not a hex color and is not null: dropped.
        assert!(projects[1].get("iconBackground").is_none());
        assert_eq!(projects[1]["iconImage"], Value::Null);
        assert_eq!(projects[1]["sidebarCollapsed"], true);
        assert!(sanitize_projects(None).is_empty());
        assert!(sanitize_projects(Some(&Value::Null)).is_empty());
    }

/// 验证缺失/空白路径的请求在激活前被 400 拦截（错误消息与路由守卫
/// 一致）。
    #[tokio::test]
    async fn directory_route_validates_before_activating() {
        let data_dir = temp_dir("dir-route-missing");
        let ctx = test_ctx(
            data_dir,
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(
            ctx,
            &serde_json::json!({ "path": "/definitely/not/a/real/directory" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, serde_json::json!({ "error": "Directory not found" }));

        // Empty and absent paths hit the route-level guard.
        let data_dir = temp_dir("dir-route-empty");
        let ctx = test_ctx(
            data_dir,
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(ctx, r#"{"path":"   "}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, serde_json::json!({ "error": "Path is required" }));

        let data_dir = temp_dir("dir-route-nobody");
        let ctx = test_ctx(
            data_dir,
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(ctx, "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, serde_json::json!({ "error": "Path is required" }));
    }

/// 验证激活已有目录会新建/复用项目条目并写盘，重复激活不产生重复
/// 条目。
    #[tokio::test]
    async fn directory_route_activates_existing_project_and_persists_settings() {
        let data_dir = temp_dir("dir-route-activate");
        let project = data_dir.join("workspace").join("alpha");
        std::fs::create_dir_all(&project).expect("project dir");
        let ctx = test_ctx(
            data_dir.clone(),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );

        let canonical = std::fs::canonicalize(&project).unwrap();
        let (status, body) = post_directory(
            ctx,
            &serde_json::json!({ "path": project.to_string_lossy() }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["restarted"], false);
        assert_eq!(body["path"], canonical.to_string_lossy().as_ref());
        assert_eq!(
            body["settings"]["activeProjectId"],
            body["settings"]["projects"][0]["id"]
        );
        assert_eq!(
            body["settings"]["lastDirectory"],
            canonical.to_string_lossy().as_ref()
        );
        assert_eq!(
            body["settings"]["projects"][0]["id"],
            create_project_id_from_path(&canonical.to_string_lossy())
        );

        // The persisted file round-trips the same settings.
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(data_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted["activeProjectId"],
            body["settings"]["activeProjectId"]
        );
        assert_eq!(
            persisted["lastDirectory"],
            canonical.to_string_lossy().as_ref()
        );

        // Re-activating the same path does not duplicate the project.
        let ctx = test_ctx(
            data_dir.clone(),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(
            ctx,
            &serde_json::json!({ "path": canonical.to_string_lossy() }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["settings"]["projects"].as_array().map(Vec::len),
            Some(1)
        );
    }

/// 验证 create:true 先递归创建目录再激活。
    #[tokio::test]
    async fn directory_route_create_flag_makes_missing_directory() {
        let data_dir = temp_dir("dir-route-create");
        let target = data_dir.join("fresh").join("nested");
        let ctx = test_ctx(
            data_dir,
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(
            ctx,
            &serde_json::json!({ "path": target.to_string_lossy(), "create": true }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(
            target.is_dir(),
            "create:true must mkdir -p the requested path"
        );
        assert_eq!(
            body["path"],
            std::fs::canonicalize(&target)
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
    }

/// 验证写盘保留 settings.json 中的未知字段，且失效的项目条目被剔除。
    #[tokio::test]
    async fn directory_route_preserves_unknown_settings_fields() {
        let data_dir = temp_dir("dir-route-preserve");
        let project = data_dir.join("keepme");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            data_dir.join("settings.json"),
            r#"{ "themePreference": "dark", "projects": [{ "id": "old", "path": "/no/longer/here" }] }"#,
        )
        .unwrap();
        let ctx = test_ctx(
            data_dir.clone(),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = post_directory(
            ctx,
            &serde_json::json!({ "path": project.to_string_lossy() }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["settings"]["themePreference"], "dark");
        // The stale project entry survives sanitization only if its path
        // exists; here it does not, so the list is just the new project.
        assert_eq!(
            body["settings"]["projects"].as_array().map(Vec::len),
            Some(1)
        );
    }
}
