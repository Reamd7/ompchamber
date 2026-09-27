//! Inline port of `opencode/project-directory-runtime.js`
//! (`validateDirectoryPath`, `resolveProjectDirectory`,
//! `resolveOptionalProjectDirectory`). `fs_routes::workspace` keeps its own
//! copy for the fs surface; this one serves the opencode config routes and
//! reads the settings fallback through the shared settings store.
//!
//! 中文说明：内联移植 `opencode/project-directory-runtime.js` 的三个
//! 入口（路径校验、必需/可选项目目录解析）。候选目录来自
//! `x-opencode-directory` 头（带 `uri` 编码标记时才做百分号解码）与
//! `?directory=` query；无显式候选时回退到设置里的 `lastDirectory`，
//! 再回退到活动项目。`fs_routes::workspace` 另有一份独立拷贝。

use std::path::PathBuf;

use axum::http::HeaderMap;

use crate::context::RouterContext;
use crate::settings;

use super::webutil::{first_query_value, header_value, resolve_path};

/// 中文：项目目录解析结果：规范化目录、词法化请求目录与错误文案；
/// 成功时 `error` 为 `None`，失败时两个目录字段为 `None`。
#[derive(Debug, Clone)]
pub(crate) struct ProjectDirectory {
    /// 校验通过并 canonicalize 后的目录（失败时为 None）。
    pub directory: Option<PathBuf>,
    /// 词法解析后的原始请求目录（用于回显/设置存储）。
    pub requested_directory: Option<PathBuf>,
    /// 解析失败原因（无显式请求且无可用回退时也为 None）。
    pub error: Option<String>,
}

/// `settings-normalization-runtime.js` `normalizeDirectoryPath`.
/// 中文：规范化目录路径字符串：去首尾引号、展开 `~`/`~/`/`~\` 为
/// home 目录，其余原样返回（不做存在性检查）。
fn normalize_directory_path(value: &str) -> String {
    let mut trimmed = value.trim().to_string();
    if trimmed.len() >= 2 {
        let quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
        if quoted {
            trimmed = trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    if trimmed.is_empty() {
        return trimmed;
    }
    let home = crate::config::home_dir();
    if trimmed == "~" {
        if let Some(home) = home {
            return home.to_string_lossy().into_owned();
        }
        return trimmed;
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && let Some(home) = home
        {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    trimmed
}

/// `validateDirectoryPath` — returns (canonical, lexical requested).
/// 中文：校验目录参数并返回 (canonical 目录, 词法请求目录) 二元组：
/// 空值报错；经规范化与词法解析后用 `tokio::fs::metadata` 确认存在
/// 且是目录，错误按 NotFound/PermissionDenied/其他映射为固定文案，
/// 最后 canonicalize 失败也报统一错误。
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
        Ok(canonical) => Ok((canonical, resolved)),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

/// 中文：收集显式目录候选：`x-opencode-directory` 头优先（仅当
/// `x-opencode-directory-encoding: uri` 时做百分号解码，保留直连
/// API 客户端送来的字面 `%` 序列），其次是 `?directory=` query。
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

/// 中文：单个组件的百分号解码（对应 `decodeURIComponent`）：
/// `%XX` 序列逐字节解码，序列不完整或结果非 UTF-8 时返回 `None`。
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
/// 中文：解析必需的项目目录，优先级：显式头/query 候选（逐个校验，
/// 全失败则返回最后一个错误）→ 设置中的 `lastDirectory` → 设置中
/// 的活动项目（`activeProjectId` 匹配不到则取首个，仅保留路径非空
/// 的项目）。全部不可用时返回错误，要求提供目录参数或活动项目。
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
/// 中文：解析可选的项目目录：仅当存在显式头/query 候选时才校验；
/// 没有候选返回全 `None` 的成功结果，不回退到设置。
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
/// 中文：只看请求头的目录提示（`x-opencode-directory` 或
/// `?directory=`），返回首个候选原文，不做校验。
pub(crate) fn requested_directory_hint(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Option<String> {
    directory_candidates(headers, uri).into_iter().next()
}
