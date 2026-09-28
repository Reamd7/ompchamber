//! Port of the Windows-only `/api/session` merge from `server/lib/opencode/
//! proxy.js` (`process.platform === 'win32'` branch): bare session listings
//! merge the global list with per-project-directory lists because Windows
//! directory scoping hides cross-directory sessions from the engine.
//!
//! 中文说明：合并全局与各项目目录的会话列表——先拉取全局 `/session`，
//! 再读取 settings.json 中登记的每个项目目录的 `?directory=` 列表，
//! 按 id 去重后合并返回；仅 Windows 平台的路由使用本模块。

use std::cmp::Ordering;
use std::collections::HashSet;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::proxy::headers::percent_encode_component;
use crate::proxy::sanitize::sanitize_session_list_payload;
use crate::proxy::{ProxyState, fetch_session_list};
use serde_json::Value;

/// Windows 上每次会话列表请求（全局或按目录）的超时时间（毫秒）。
const WINDOWS_SESSION_FETCH_TIMEOUT_MS: u64 = 10_000;

/// Windows 专用的 `/api/session` 合并处理器。
///
/// 流程：取全局列表并净化（失败仅记录日志、按空处理）；从
/// `~/.config/ompchamber/settings.json` 的 `projects[].path` 收集项目目录；
/// 每个目录按原生/正斜杠/反斜杠三种拼写（去重保序）尝试带 `?directory=`
/// 的请求，成功响应净化后按 `id` 去重追加。全局与所有目录读取均失败时
/// 返回 504 GATEWAY_TIMEOUT；否则合并、按 `time_updated` 降序排序
/// （净化后的载荷缺失该字段视为 0，实际为稳定排序）并以 200 返回净化结果。
pub(super) async fn merge_session_list(st: &ProxyState, req_headers: &HeaderMap) -> Response {
    let global_sessions = match fetch_session_list(
        st,
        "/session",
        req_headers,
        Some(WINDOWS_SESSION_FETCH_TIMEOUT_MS),
    )
    .await
    {
        Ok(result) => match result.payload {
            Some(Value::Array(_)) if result.status.is_success() => Some(
                sanitize_session_list_payload(&result.payload.expect("checked array")),
            ),
            _ => None,
        },
        Err(error) => {
            tracing::info!("[SessionMerge] Global session list failed: {error}");
            None
        }
    };

    let mut project_dirs: Vec<String> = Vec::new();
    if let Some(home) = home_dir() {
        let settings_path = home
            .join(".config")
            .join("ompchamber")
            .join("settings.json");
        if let Ok(raw) = std::fs::read_to_string(&settings_path) {
            if let Ok(settings) = serde_json::from_str::<Value>(&raw) {
                if let Some(projects) = settings.get("projects").and_then(Value::as_array) {
                    for project in projects {
                        if let Some(path) = project.get("path").and_then(Value::as_str) {
                            let trimmed = path.trim();
                            if !trimmed.is_empty() {
                                project_dirs.push(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut seen: HashSet<String> = session_ids(global_sessions.as_ref());
    let mut extra_sessions: Vec<Value> = Vec::new();
    let mut successful_project_reads = 0u32;

    for dir in project_dirs {
        // Same three spellings the JS tries per project (native, forward
        // slashes, backslashes), de-duplicated preserving order.
        let mut candidates: Vec<String> =
            vec![dir.clone(), dir.replace('\\', "/"), dir.replace('/', "\\")];
        let mut unique: Vec<String> = Vec::with_capacity(candidates.len());
        for candidate in candidates.drain(..) {
            if !unique.contains(&candidate) {
                unique.push(candidate);
            }
        }

        for candidate in unique {
            let encoded = percent_encode_component(&candidate);
            let path = format!("/session?directory={encoded}");
            let Ok(result) = fetch_session_list(
                st,
                &path,
                req_headers,
                Some(WINDOWS_SESSION_FETCH_TIMEOUT_MS),
            )
            .await
            else {
                continue;
            };
            let list = match result.payload {
                Some(Value::Array(items)) if result.status.is_success() => {
                    successful_project_reads += 1;
                    sanitize_session_list_payload(&Value::Array(items))
                }
                _ => continue,
            };
            if let Some(items) = list.as_array() {
                for session in items {
                    if let Some(id) = session.get("id").and_then(Value::as_str) {
                        if seen.insert(id.to_string()) {
                            extra_sessions.push(session.clone());
                        }
                    }
                }
            }
        }
    }

    if global_sessions.is_none() && successful_project_reads == 0 {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "OpenCode session list timed out" })),
        )
            .into_response();
    }

    let mut merged: Vec<Value> = match global_sessions {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let global_count = merged.len();
    merged.extend(extra_sessions);
    // JS sorts by `time_updated` (absent from sanitized payloads → 0), i.e. a
    // stable no-op ordering in practice; mirrored literally.
    merged.sort_by(|a, b| {
        let a_time = a.get("time_updated").and_then(Value::as_f64).unwrap_or(0.0);
        let b_time = b.get("time_updated").and_then(Value::as_f64).unwrap_or(0.0);
        b_time.partial_cmp(&a_time).unwrap_or(Ordering::Equal)
    });
    tracing::info!(
        "[SessionMerge] {} global + {} extra = {} total",
        global_count,
        merged.len() - global_count,
        merged.len()
    );

    (
        StatusCode::OK,
        Json(sanitize_session_list_payload(&Value::Array(merged))),
    )
        .into_response()
}

/// 从会话列表 payload 中收集所有 `id` 字符串，用于跨目录去重。
fn session_ids(payload: Option<&Value>) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(items) = payload.and_then(Value::as_array) {
        for session in items {
            if let Some(id) = session.get("id").and_then(Value::as_str) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

/// 取用户主目录：优先 `USERPROFILE`，回退 `HOME`；缺失或为空返回 `None`。
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}
