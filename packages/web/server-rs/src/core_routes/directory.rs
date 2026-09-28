//! `POST /api/opencode/directory` from `opencode/routes.js`, backed by the
//! canonical `settings` module port: `settings::normalization` owns every
//! path/sanitize semantic (heal, realpath form, id encoding) and
//! `settings::runtime` owns project-entry validation. This module keeps only
//! the route shape plus the surgical settings persistence it needs:
//! unknown-field-preserving read + atomic write of `settings.json` for exactly
//! the three keys the route touches: `projects`, `activeProjectId`,
//! `lastDirectory`.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::context::RouterContext;
use crate::error::AppError;
use crate::settings::normalization::{
    create_project_id_from_path, normalize_directory_path, path_resolve, sanitize_projects,
};

/// Lenient JSON body read: a blank/absent body is `{}` (Express leaves
/// `req.body` undefined for non-JSON requests); a malformed JSON body is 400.
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

#[derive(Debug)]
pub struct ValidatedDirectory {
    /// Canonical (realpath) directory — what gets persisted.
    pub directory: PathBuf,
    /// Pre-realpath candidate the caller asked for.
    pub requested_directory: PathBuf,
}

/// `validateDirectoryPath` (project-directory-runtime.js): required, must be an
/// existing directory; returns the realpath.
pub fn validate_directory_path(candidate: &str) -> Result<ValidatedDirectory, String> {
    let normalized = normalize_directory_path(candidate);
    if normalized.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = path_resolve(&normalized);
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
            directory: crate::settings::normalization::strip_verbatim_prefix(directory),
            requested_directory: resolved,
        }),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

/// `readSettingsFromDisk` (lenient): every failure maps to `{}`.
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

fn settings_path(ctx: &RouterContext) -> PathBuf {
    ctx.config.data_dir.join("settings.json")
}

/// `POST /api/opencode/directory`.
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
        let target = path_resolve(&requested_path);
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
    let mut projects = sanitize_projects(current.get("projects")).unwrap_or_default();
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
    let projects = crate::settings::runtime::validate_project_entries(&projects).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::router;
    use crate::core_routes::tests::{json_response, temp_dir, test_ctx};
    use crate::engine::EngineState;
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};

    async fn post_directory(ctx: crate::context::RouterContext, body: &str) -> (StatusCode, Value) {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/api/opencode/directory")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        json_response(router(ctx), request).await
    }

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
        // Windows: canonicalize yields a `\\?\` verbatim prefix; validate strips
        // it (Node realpathSync form), so compare against the stripped form.
        let canonical = crate::settings::normalization::strip_verbatim_prefix(
            std::fs::canonicalize(&dir).unwrap(),
        );
        assert_eq!(validated.directory, canonical);
        assert_eq!(validated.requested_directory, dir);
    }

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

    #[tokio::test]
    async fn directory_route_activates_existing_project_and_persists_settings() {
        let data_dir = temp_dir("dir-route-activate");
        let project = data_dir.join("workspace").join("alpha");
        std::fs::create_dir_all(&project).expect("project dir");
        let ctx = test_ctx(
            data_dir.clone(),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );

        let canonical = crate::settings::normalization::strip_verbatim_prefix(
            std::fs::canonicalize(&project).unwrap(),
        );
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
            crate::settings::normalization::strip_verbatim_prefix(
                std::fs::canonicalize(&target).unwrap()
            )
            .to_string_lossy()
            .as_ref()
        );
    }

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
