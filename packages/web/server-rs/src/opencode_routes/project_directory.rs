//! Inline port of `opencode/project-directory-runtime.js`
//! (`validateDirectoryPath`, `resolveProjectDirectory`,
//! `resolveOptionalProjectDirectory`). `fs_routes::workspace` keeps its own
//! copy for the fs surface; this one serves the opencode config routes and
//! reads the settings fallback through the shared settings store.

use std::path::PathBuf;

use axum::http::HeaderMap;

use crate::context::RouterContext;
use crate::settings;
use crate::settings::normalization::normalize_directory_path;

use super::webutil::{first_query_value, header_value, resolve_path};

#[derive(Debug, Clone)]
pub(crate) struct ProjectDirectory {
    pub directory: Option<PathBuf>,
    pub requested_directory: Option<PathBuf>,
    pub error: Option<String>,
}

/// `validateDirectoryPath` — returns (canonical, lexical requested).
pub(crate) async fn validate_directory_path(candidate: &str) -> Result<(PathBuf, PathBuf), String> {
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
    match tokio::fs::canonicalize(&resolved).await {
        Ok(canonical) => Ok((
            crate::settings::normalization::strip_verbatim_prefix(canonical),
            resolved,
        )),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

fn directory_candidates(headers: &HeaderMap, uri: &axum::http::Uri) -> Vec<String> {
    let mut candidates = Vec::new();
    let header_encoding = header_value(headers, "x-opencode-directory-encoding");
    if let Some(raw) = header_value(headers, "x-opencode-directory")
        && !raw.is_empty()
    {
        // Only marked values are decoded so literal percent sequences from
        // direct API clients are preserved (safeDecodeMarkedURIComponent).
        let value = if header_encoding.as_deref() == Some("uri") {
            percent_decode_component(&raw).unwrap_or(raw)
        } else {
            raw
        };
        candidates.push(value);
    }
    if let Some(query_directory) = first_query_value(uri, "directory")
        && !query_directory.is_empty()
    {
        candidates.push(query_directory);
    }
    candidates.into_iter().filter(|v| !v.is_empty()).collect()
}

fn percent_decode_component(input: &str) -> Option<String> {
    let raw = input.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// `resolveProjectDirectory`: explicit header/query candidates first, then
/// settings `lastDirectory`, then the active project.
pub(crate) async fn resolve_project_directory(
    ctx: &RouterContext,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> ProjectDirectory {
    let candidates = directory_candidates(headers, uri);
    if !candidates.is_empty() {
        let mut last_error = None;
        for candidate in &candidates {
            match validate_directory_path(candidate).await {
                Ok((directory, requested)) => {
                    return ProjectDirectory {
                        directory: Some(directory),
                        requested_directory: Some(requested),
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

    let settings_doc = settings::store(ctx)
        .read_migrated()
        .await
        .unwrap_or_default();

    if let Some(last_directory) = settings_doc
        .get("lastDirectory")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        && let Ok((directory, requested)) = validate_directory_path(last_directory).await
    {
        return ProjectDirectory {
            directory: Some(directory),
            requested_directory: Some(requested),
            error: None,
        };
    }

    // Minimal `sanitizeProjects`: entries carrying a non-empty path.
    let projects: Vec<(String, String)> = settings_doc
        .get("projects")
        .and_then(|v| v.as_array())
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| {
                    let path = project.get("path")?.as_str()?.trim().to_string();
                    if path.is_empty() {
                        return None;
                    }
                    Some((
                        project
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        path,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    if projects.is_empty() {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    }

    let active_id = settings_doc
        .get("activeProjectId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let active = projects
        .iter()
        .find(|(id, _)| !id.is_empty() && id == active_id)
        .or_else(|| projects.first())
        .cloned();
    let Some((_, active_path)) = active else {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    };

    match validate_directory_path(&active_path).await {
        Ok((directory, requested)) => ProjectDirectory {
            directory: Some(directory),
            requested_directory: Some(requested),
            error: None,
        },
        Err(error) => ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some(error),
        },
    }
}

/// `resolveOptionalProjectDirectory`.
pub(crate) async fn resolve_optional_project_directory(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> ProjectDirectory {
    let candidates = directory_candidates(headers, uri);
    if candidates.is_empty() {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: None,
        };
    }
    let mut last_error = None;
    for candidate in &candidates {
        match validate_directory_path(candidate).await {
            Ok((directory, requested)) => {
                return ProjectDirectory {
                    directory: Some(directory),
                    requested_directory: Some(requested),
                    error: None,
                };
            }
            Err(error) => last_error = Some(error),
        }
    }
    ProjectDirectory {
        directory: None,
        requested_directory: None,
        error: last_error,
    }
}

/// Header-only directory hint (`x-opencode-directory` or `?directory=`).
pub(crate) fn requested_directory_hint(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Option<String> {
    directory_candidates(headers, uri).into_iter().next()
}
