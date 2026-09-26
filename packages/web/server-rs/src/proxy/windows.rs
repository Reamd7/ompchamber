//! Port of the Windows-only `/api/session` merge from `server/lib/opencode/
//! proxy.js` (`process.platform === 'win32'` branch): bare session listings
//! merge the global list with per-project-directory lists because Windows
//! directory scoping hides cross-directory sessions from the engine.

use std::cmp::Ordering;
use std::collections::HashSet;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::proxy::headers::percent_encode_component;
use crate::proxy::sanitize::sanitize_session_list_payload;
use crate::proxy::{ProxyState, fetch_session_list};
use serde_json::Value;

const WINDOWS_SESSION_FETCH_TIMEOUT_MS: u64 = 10_000;

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

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}
