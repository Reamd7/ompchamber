//! Workspace boundary enforcement — port of routes.js
//! `resolveWorkspacePath` / `resolveWorkspacePathFromContext` /
//! `resolveWorkspacePathFromWorktrees` / `resolveReadPathFromContext` plus
//! the outside-file grant store and an inline port of the injected
//! `resolveProjectDirectory` (opencode/project-directory-runtime.js).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

use super::paths::{
    is_path_within_root, normalize_directory_path, realpath, resolve_path, user_config_root,
};

pub const OUTSIDE_FILE_GRANT_TTL_MS: u64 = 10 * 60 * 1000;

const ERR_OUTSIDE_WORKSPACE: &str = "Path is outside of active workspace";

/// Result shape of the JS `resolveWorkspacePath*` helpers. `granted` is only
/// true for outside-workspace reads riding an exact-path grant.
#[derive(Debug, Clone)]
pub struct WorkspacePath {
    pub base: PathBuf,
    pub resolved: PathBuf,
    pub granted: bool,
}

#[derive(Debug, Default)]
pub struct ProjectDirectory {
    pub directory: Option<PathBuf>,
    pub requested_directory: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct OutsideGrant {
    canonical_path: PathBuf,
    base: PathBuf,
    scopes: HashSet<String>,
    expires_at: u64,
}

#[derive(Default)]
pub struct OutsideGrantStore {
    grants: Mutex<HashMap<String, OutsideGrant>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl OutsideGrantStore {
    fn prune(&self) {
        let mut grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        grants.retain(|_, grant| grant.expires_at > now);
    }

    /// routes.js `mintOutsideFileGrant`. Cross-module callers (feature
    /// routes mint grants after an explicit user file pick) use this too.
    pub async fn mint(
        &self,
        target_path: &str,
        scopes: &[&str],
    ) -> Result<serde_json::Value, String> {
        let raw = target_path.trim();
        if raw.is_empty() {
            return Err("Path is required".to_string());
        }
        let canonical_path = realpath(Path::new(raw)).map_err(|e| e.to_string())?;
        let stats = tokio::fs::metadata(&canonical_path)
            .await
            .map_err(|e| e.to_string())?;
        if !stats.is_file() {
            return Err("Outside file grants require a file path".to_string());
        }
        self.prune();
        let token = super::paths::random_uuid();
        let mut normalized_scopes: HashSet<String> = scopes
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if normalized_scopes.is_empty() {
            normalized_scopes.insert("read".to_string());
        }
        let base = super::paths::dirname(&canonical_path);
        let expires_at = now_ms() + OUTSIDE_FILE_GRANT_TTL_MS;
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token.clone(),
                OutsideGrant {
                    canonical_path: canonical_path.clone(),
                    base: base.clone(),
                    scopes: normalized_scopes,
                    expires_at,
                },
            );
        Ok(serde_json::json!({
            "path": canonical_path.to_string_lossy(),
            "outsideFileGrant": token,
            "expiresAt": expires_at,
        }))
    }

    /// routes.js `resolveOutsideFileGrant`.
    pub async fn resolve(
        &self,
        token: Option<&str>,
        target_path: &Path,
        scope: &str,
    ) -> Result<WorkspacePath, GrantError> {
        self.prune();
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        let Some(token) = token else {
            return Err(GrantError::Denied(
                "Outside workspace file access requires a grant".to_string(),
            ));
        };
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let Some(grant) = grants.get(token) else {
            return Err(GrantError::Denied(
                "Outside workspace file grant is invalid or expired".to_string(),
            ));
        };
        if !grant.scopes.contains(scope) {
            return Err(GrantError::Denied(
                "Outside workspace file grant does not allow this operation".to_string(),
            ));
        }
        let canonical_path = realpath(target_path).map_err(GrantError::Io)?;
        if canonical_path != grant.canonical_path {
            return Err(GrantError::Denied(
                "Outside workspace file grant does not match requested path".to_string(),
            ));
        }
        Ok(WorkspacePath {
            base: grant.base.clone(),
            resolved: canonical_path,
            granted: true,
        })
    }
}

#[derive(Debug)]
pub enum GrantError {
    /// 400 with the JS message (`{ error: ... }`).
    Denied(String),
    /// realpath failure — maps through the route's generic io error handling.
    Io(std::io::Error),
}

/// routes.js `resolveWorkspacePath`: the active project root and the user
/// config root (`~/.config/ompchamber`) are the two always-admitted bases.
pub fn resolve_workspace_path(
    target_path: &str,
    base_directory: Option<&Path>,
) -> Result<WorkspacePath, String> {
    let normalized = normalize_directory_path(target_path);
    if normalized.is_empty() {
        return Err("Path is required".to_string());
    }
    let resolved = resolve_path(&normalized);
    let base = match base_directory {
        Some(dir) => resolve_path(&dir.to_string_lossy()),
        None => resolve_path(
            &crate::config::home_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        ),
    };
    if is_path_within_root(&resolved, &base) {
        return Ok(WorkspacePath {
            base,
            resolved,
            granted: false,
        });
    }
    let config_root = resolve_path(&user_config_root().to_string_lossy());
    if is_path_within_root(&resolved, &config_root) {
        return Ok(WorkspacePath {
            base: config_root,
            resolved,
            granted: false,
        });
    }
    Err(ERR_OUTSIDE_WORKSPACE.to_string())
}

/// routes.js `resolveWorkspacePathFromWorktrees`: last-chance fallback that
/// admits paths inside any `git worktree list` entry of the active project.
async fn resolve_workspace_path_from_worktrees(
    target_path: &str,
    base_directory: &Path,
) -> Result<WorkspacePath, String> {
    let normalized = normalize_directory_path(target_path);
    if normalized.is_empty() {
        return Err("Path is required".to_string());
    }
    let resolved = resolve_path(&normalized);
    let resolved_base = resolve_path(&base_directory.to_string_lossy());

    for worktree in list_worktree_roots(&resolved_base).await {
        let candidate = resolve_path(&worktree.to_string_lossy());
        if is_path_within_root(&resolved, &candidate) {
            return Ok(WorkspacePath {
                base: candidate,
                resolved,
                granted: false,
            });
        }
    }
    Err(ERR_OUTSIDE_WORKSPACE.to_string())
}

/// Minimal inline port of git/service.js `getWorktrees`: spawn
/// `git worktree list --porcelain` in `directory` and collect the
/// `worktree <path>` lines. Any failure yields an empty list (the JS treats
/// non-repositories as authoritatively empty).
async fn list_worktree_roots(directory: &Path) -> Vec<PathBuf> {
    let output = run_worktree_list(directory).await;
    let stdout = match output {
        Ok(stdout) => stdout,
        Err(_) => return Vec::new(),
    };
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| !path.trim().is_empty())
        .map(|path| PathBuf::from(path.trim()))
        .collect()
}

async fn run_worktree_list(directory: &Path) -> Result<String, std::io::Error> {
    let output = tokio::process::Command::new(git_binary())
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(std::io::Error::other("git worktree list failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// env-runtime.js `resolveGitBinaryForSpawn` — plain `git` off POSIX.
pub fn git_binary() -> &'static str {
    "git"
}

/// routes.js `resolveWorkspacePathFromContext`: project root first, then the
/// raw (lexical) directory the client asked for when the project root itself
/// is a symlink, then worktree roots.
pub async fn resolve_workspace_path_from_context(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    target_path: &str,
) -> Result<WorkspacePath, String> {
    let project = resolve_project_directory(headers, query).await;
    let Some(directory) = project.directory.clone() else {
        return Err(project
            .error
            .unwrap_or_else(|| "Active workspace is required".to_string()));
    };

    let resolved = resolve_workspace_path(target_path, Some(&directory));
    match resolved {
        Ok(path) => Ok(path),
        Err(error) if error != ERR_OUTSIDE_WORKSPACE => Err(error),
        Err(_) => {
            // The validated project directory is canonical; retry against the
            // raw directory the client requested so file-tree paths under a
            // symlinked project root stay addressable.
            let requested_base = project.requested_directory.clone();
            if let Some(requested_base) = requested_base
                && requested_base != directory
            {
                let lexical = resolve_workspace_path(target_path, Some(&requested_base));
                if let Ok(path) = lexical {
                    return Ok(path);
                }
            }
            resolve_workspace_path_from_worktrees(target_path, &directory).await
        }
    }
}

/// routes.js `resolveReadPathFromContext`: read-family routes may instead
/// ride an exact-path outside-file grant when `allowOutsideWorkspace=true`.
pub async fn resolve_read_path_from_context(
    grants: &OutsideGrantStore,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    target_path: &str,
    scope: &str,
) -> Result<WorkspacePath, ReadPathError> {
    if query.get("allowOutsideWorkspace").map(String::as_str) == Some("true") {
        let normalized = normalize_directory_path(target_path);
        if normalized.is_empty() {
            return Err(ReadPathError::Denied("Path is required".to_string()));
        }
        let resolved = resolve_path(&normalized);
        return grants
            .resolve(
                query.get("outsideFileGrant").map(String::as_str),
                &resolved,
                scope,
            )
            .await
            .map_err(ReadPathError::from);
    }
    resolve_workspace_path_from_context(headers, query, target_path)
        .await
        .map_err(ReadPathError::Denied)
}

#[derive(Debug)]
pub enum ReadPathError {
    Denied(String),
    Io(std::io::Error),
}

impl From<GrantError> for ReadPathError {
    fn from(error: GrantError) -> Self {
        match error {
            GrantError::Denied(message) => ReadPathError::Denied(message),
            GrantError::Io(error) => ReadPathError::Io(error),
        }
    }
}

/// Inline port of opencode/project-directory-runtime.js
/// `resolveProjectDirectory`: `x-opencode-directory` header (decoded when
/// `x-opencode-directory-encoding: uri`) and the `directory` query parameter
/// win; otherwise fall back to settings (`lastDirectory`, then the active
/// project) read straight from `~/.config/ompchamber/settings.json`.
pub async fn resolve_project_directory(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> ProjectDirectory {
    let mut candidates: Vec<String> = Vec::new();

    let header_encoding = headers
        .get("x-opencode-directory-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if let Some(raw) = headers
        .get("x-opencode-directory")
        .and_then(|v| v.to_str().ok())
        && !raw.is_empty()
    {
        if header_encoding == "uri" {
            candidates
                .push(super::paths::decode_uri_component(raw).unwrap_or_else(|| raw.to_string()));
        } else {
            candidates.push(raw.to_string());
        }
    }
    if let Some(directory) = query.get("directory")
        && !directory.is_empty()
    {
        candidates.push(directory.clone());
    }

    if !candidates.is_empty() {
        let mut last_error = None;
        for candidate in &candidates {
            match validate_directory_path(candidate).await {
                Ok(validated) => {
                    return ProjectDirectory {
                        directory: Some(validated.0),
                        requested_directory: Some(validated.1),
                        error: None,
                    };
                }
                Err(error) => last_error = Some(error),
            }
        }
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: last_error,
        };
    }

    let settings = read_settings_document();
    if let Some(last_directory) = settings
        .get("lastDirectory")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        && let Ok(validated) = validate_directory_path(last_directory).await
    {
        return ProjectDirectory {
            directory: Some(validated.0),
            requested_directory: Some(validated.1),
            error: None,
        };
    }

    let active_id = settings
        .get("activeProjectId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let projects = sanitize_project_paths(&settings);
    if projects.is_empty() {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    }
    let active = projects
        .iter()
        .find(|(id, _)| !id.is_empty() && *id == active_id)
        .or_else(|| projects.first());
    let Some((_, active_path)) = active.cloned() else {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    };
    match validate_directory_path(&active_path).await {
        Ok(validated) => ProjectDirectory {
            directory: Some(validated.0),
            requested_directory: Some(validated.1),
            error: None,
        },
        Err(error) => ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some(error),
        },
    }
}

/// project-directory-runtime.js `validateDirectoryPath` — (canonical
/// directory, lexical requested directory).
async fn validate_directory_path(candidate: &str) -> Result<(PathBuf, PathBuf), String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = resolve_path(&normalize_directory_path(trimmed));
    let metadata = match tokio::fs::metadata(&resolved).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => "Directory not found".to_string(),
                std::io::ErrorKind::PermissionDenied => "Access to directory denied".to_string(),
                _ => "Failed to validate directory".to_string(),
            });
        }
    };
    if !metadata.is_dir() {
        return Err("Specified path is not a directory".to_string());
    }
    match realpath(&resolved) {
        Ok(canonical) => Ok((canonical, resolved)),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

fn read_settings_document() -> serde_json::Value {
    // The JS chain reads settings through the settings runtime (honors
    // OMPCHAMBER_DATA_DIR); only workspace *admission* uses the hardcoded
    // ~/.config/ompchamber root.
    let root = std::env::var("OMPCHAMBER_DATA_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(user_config_root);
    let path = root.join("settings.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Minimal `sanitizeProjects`: keep entries that carry a non-empty `path`.
/// (The full JS sanitizer also realpaths each project; that is a persistence
/// concern owned by the settings module port.)
fn sanitize_project_paths(settings: &serde_json::Value) -> Vec<(String, String)> {
    settings
        .get("projects")
        .and_then(|v| v.as_array())
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| {
                    let path = project
                        .get("path")
                        .and_then(|v| v.as_str())?
                        .trim()
                        .to_string();
                    if path.is_empty() {
                        return None;
                    }
                    let id = project
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((id, path))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn workspace_path_admits_project_and_config_roots_only() {
        assert!(resolve_workspace_path("/repo/file.txt", Some(Path::new("/repo"))).is_ok());
        assert_eq!(
            resolve_workspace_path("/etc/passwd", Some(Path::new("/repo"))).unwrap_err(),
            "Path is outside of active workspace"
        );
        assert_eq!(
            resolve_workspace_path("   ", Some(Path::new("/repo"))).unwrap_err(),
            "Path is required"
        );
        // The user config root is always admitted.
        let config_child = user_config_root().join("themes").join("x.css");
        assert!(
            resolve_workspace_path(&config_child.to_string_lossy(), Some(Path::new("/repo")))
                .is_ok()
        );
    }

    #[test]
    fn traversal_through_dots_is_caught_lexically() {
        // path.resolve collapses first: /repo/sub/../../etc/passwd -> /etc/passwd
        assert_eq!(
            resolve_workspace_path("/repo/sub/../../etc/passwd", Some(Path::new("/repo")))
                .unwrap_err(),
            "Path is outside of active workspace"
        );
    }
}
