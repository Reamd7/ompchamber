//! Port of `server/lib/opencode/pwa-manifest-routes.js`.
//!
//! Serves `/manifest.webmanifest`: app name/orientation from settings with
//! per-query overrides, plus up to three "recent session" shortcuts from the
//! engine's session list (5s TTL cache per directory, scoped list with a
//! global fallback — exactly the JS's preference chain).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::settings::helpers::{normalize_pwa_app_name, normalize_pwa_orientation};

const DEFAULT_PWA_APP_NAME: &str = "OMPChamber";
const CACHE_TTL: Duration = Duration::from_millis(5_000);

fn map_pwa_orientation_to_manifest(value: &str) -> Option<&'static str> {
    match value {
        "portrait" => Some("portrait-primary"),
        "landscape" => Some("landscape-primary"),
        _ => None,
    }
}

fn normalize_directory(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let normalized = trimmed.replace('\\', "/");
    if normalized == "/" {
        return "/".to_string();
    }
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

fn session_updated_at(session: &Value) -> f64 {
    let time = session.get("time").filter(|t| t.is_object());
    let pick = |key: &str| -> Option<f64> {
        time.and_then(|t| t.get(key))
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
    };
    pick("updated").or_else(|| pick("created")).unwrap_or(0.0)
}

fn filter_sessions_by_directory(sessions: &[Value], directory: &str) -> Vec<Value> {
    let normalized_directory = normalize_directory(directory);
    if normalized_directory.is_empty() {
        return sessions.to_vec();
    }
    let prefix = if normalized_directory == "/" {
        "/".to_string()
    } else {
        format!("{normalized_directory}/")
    };
    sessions
        .iter()
        .filter(|session| {
            let session_directory = session
                .get("directory")
                .and_then(Value::as_str)
                .map(normalize_directory)
                .unwrap_or_default();
            if session_directory.is_empty() {
                return false;
            }
            session_directory == normalized_directory || session_directory.starts_with(&prefix)
        })
        .cloned()
        .collect()
}

fn normalize_shortcut_title(value: &str, fallback: &str) -> String {
    let normalized = normalize_pwa_app_name(&Value::String(value.to_string()), fallback);
    if normalized.chars().count() > 48 {
        normalized.chars().take(48).collect()
    } else {
        normalized
    }
}

fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Clone)]
struct ModuleState {
    ctx: RouterContext,
    cache: std::sync::Arc<Mutex<HashMap<String, (Instant, Vec<Value>)>>>,
}

impl ModuleState {
    async fn list_sessions(&self, directory: Option<&str>) -> Vec<Value> {
        let Some(base) = self.ctx.engine.base_url() else {
            return Vec::new();
        };
        let mut url = format!("{base}/session");
        if let Some(directory) = directory.filter(|d| !d.is_empty()) {
            let prepared = if cfg!(windows) {
                directory.replace('/', "\\\\")
            } else {
                directory.to_string()
            };
            url = format!("{url}?directory={}", encode_uri_component(&prepared));
        }
        let mut request = self
            .ctx
            .engine
            .http()
            .get(&url)
            .timeout(Duration::from_millis(2_500))
            .header("accept", "application/json");
        if let Some(auth) = self.ctx.engine.auth_header() {
            request = request.header("authorization", auth);
        }
        let Ok(response) = request.send().await else {
            return Vec::new();
        };
        if !response.status().is_success() {
            return Vec::new();
        }
        let Ok(text) = response.text().await else {
            return Vec::new();
        };
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
    }

    async fn recent_session_shortcuts(&self, directory: Option<&str>) -> Vec<Value> {
        let cache_key = match directory {
            Some(directory) if !directory.is_empty() => {
                format!("dir:{}", normalize_directory(directory))
            }
            _ => "global".to_string(),
        };
        if let Ok(cache) = self.cache.lock()
            && let Some((at, data)) = cache.get(&cache_key)
            && at.elapsed() < CACHE_TTL
        {
            return data.clone();
        }

        let payload = if let Some(preferred) = directory.filter(|d| !d.is_empty()) {
            let scoped = self.list_sessions(Some(preferred)).await;
            let filtered = filter_sessions_by_directory(&scoped, preferred);
            if !filtered.is_empty() {
                filtered
            } else {
                let global = self.list_sessions(None).await;
                filter_sessions_by_directory(&global, preferred)
            }
        } else {
            self.list_sessions(None).await
        };

        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<(String, String, f64)> = Vec::new();
        for item in &payload {
            let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .map(|id| id.trim().chars().take(160).collect::<String>())
            else {
                continue;
            };
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let title = normalize_shortcut_title(
                item.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                &format!("Session {}", rows.len() + 1),
            );
            rows.push((id, title, session_updated_at(item)));
        }
        rows.sort_by(|a, b| b.2.total_cmp(&a.2));

        let shortcuts: Vec<Value> = rows
            .into_iter()
            .take(3)
            .map(|(id, title, _updated_at)| {
                let short_name = if title.chars().count() > 32 {
                    title.chars().take(32).collect::<String>()
                } else {
                    title.clone()
                };
                json!({
                    "name": title,
                    "short_name": short_name,
                    "description": "Open recent session",
                    "url": format!("/?session={}", encode_uri_component(&id)),
                    "icons": [{ "src": "/pwa-192.png", "sizes": "192x192", "type": "image/png" }],
                })
            })
            .collect();

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(cache_key, (Instant::now(), shortcuts.clone()));
        }
        shortcuts
    }
}

fn query_lookup(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if percent_decode_loose(name) == key {
            return Some(percent_decode_loose(value));
        }
    }
    None
}

fn percent_decode_loose(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'+' {
            out.push(b' ');
        } else if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
            out.push(bytes[index]);
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn manifest(State(state): State<ModuleState>, request: Request) -> Response {
    let query = request.uri().query().map(str::to_string);

    let query_name = ["pwa_name", "app_name", "appName"]
        .iter()
        .find_map(|key| query_lookup(query.as_deref(), key));
    let has_query_override = query_name.is_some();
    let query_orientation = query_lookup(query.as_deref(), "orientation");
    let has_orientation_override = query_orientation.is_some();

    let mut stored_name = String::new();
    let mut stored_orientation = "system".to_string();
    if let Ok(settings) = crate::settings::store(&state.ctx).read_migrated().await {
        stored_name =
            normalize_pwa_app_name(settings.get("pwaAppName").unwrap_or(&Value::Null), "");
        stored_orientation = normalize_pwa_orientation(
            settings.get("pwaOrientation").unwrap_or(&Value::Null),
            "system",
        );
    }

    // JS precedence: a query override wins (falling back to the default name
    // when it normalizes empty); otherwise the stored name or the default.
    let app_name = if has_query_override {
        let normalized = normalize_pwa_app_name(&Value::String(query_name.unwrap_or_default()), "");
        if normalized.is_empty() {
            DEFAULT_PWA_APP_NAME.to_string()
        } else {
            normalized
        }
    } else if stored_name.is_empty() {
        DEFAULT_PWA_APP_NAME.to_string()
    } else {
        stored_name
    };
    let orientation_value = if has_orientation_override {
        normalize_pwa_orientation(
            &Value::String(query_orientation.unwrap_or_default()),
            "system",
        )
    } else {
        stored_orientation
    };
    let manifest_orientation = map_pwa_orientation_to_manifest(&orientation_value);

    let short_name = if app_name.chars().count() > 30 {
        app_name.chars().take(30).collect::<String>()
    } else {
        app_name.clone()
    };

    // Preferred directory: the request's directory query (the JS resolves
    // through resolveProjectDirectory; the query param is the shape PWA
    // clients carry).
    let preferred_directory = query_lookup(query.as_deref(), "directory");
    let recent_session_shortcuts = state
        .recent_session_shortcuts(preferred_directory.as_deref())
        .await;

    let mut manifest = json!({
        "name": app_name,
        "short_name": short_name,
        "description": "Web interface for the OMPChamber AI coding assistant",
        "id": "/",
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "display_override": ["window-controls-overlay"],
        "background_color": "#151313",
        "theme_color": "#edb449",
        "icons": [
            { "src": "/pwa-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any" },
            { "src": "/pwa-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any" },
            { "src": "/pwa-maskable-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any maskable" },
            { "src": "/pwa-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any maskable" },
            { "src": "/apple-touch-icon-180x180.png", "sizes": "180x180", "type": "image/png", "purpose": "any" },
            { "src": "/apple-touch-icon-152x152.png", "sizes": "152x152", "type": "image/png", "purpose": "any" },
            { "src": "/favicon-32.png", "sizes": "32x32", "type": "image/png" },
            { "src": "/favicon-16.png", "sizes": "16x16", "type": "image/png" }
        ],
        "shortcuts": [
            {
                "name": "Appearance Settings",
                "short_name": "Settings",
                "description": "Open appearance settings",
                "url": "/?settings=appearance",
                "icons": [{ "src": "/pwa-192.png", "sizes": "192x192", "type": "image/png" }]
            }
        ],
        "categories": ["developer", "tools", "productivity"],
        "lang": "en",
    });
    if let Some(orientation) = manifest_orientation {
        manifest["orientation"] = json!(orientation);
    }
    if let Some(shortcuts) = manifest.get_mut("shortcuts").and_then(Value::as_array_mut) {
        shortcuts.extend(recent_session_shortcuts);
    }

    (
        StatusCode::OK,
        [
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, must-revalidate"),
            ),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/manifest+json"),
            ),
        ],
        Json(manifest),
    )
        .into_response()
}

pub fn router(ctx: RouterContext) -> Router {
    Router::new()
        .route("/manifest.webmanifest", get(manifest))
        .with_state(ModuleState {
            ctx,
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_mapping_matches_js() {
        assert_eq!(
            map_pwa_orientation_to_manifest("portrait"),
            Some("portrait-primary")
        );
        assert_eq!(
            map_pwa_orientation_to_manifest("landscape"),
            Some("landscape-primary")
        );
        assert_eq!(map_pwa_orientation_to_manifest("system"), None);
    }

    #[test]
    fn directory_filter_matches_subdirectories() {
        let sessions = vec![
            json!({ "id": "a", "directory": "/repo" }),
            json!({ "id": "b", "directory": "/repo/sub" }),
            json!({ "id": "c", "directory": "/other" }),
            json!({ "id": "d" }),
        ];
        let filtered = filter_sessions_by_directory(&sessions, "/repo");
        let ids: Vec<&str> = filtered.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(filter_sessions_by_directory(&sessions, "").len(), 4);
    }

    #[test]
    fn updated_at_prefers_updated_over_created() {
        assert_eq!(
            session_updated_at(&json!({ "time": { "updated": 5.0, "created": 9.0 } })),
            5.0
        );
        assert_eq!(
            session_updated_at(&json!({ "time": { "created": 9.0 } })),
            9.0
        );
        assert_eq!(session_updated_at(&json!({})), 0.0);
    }

    #[test]
    fn shortcut_titles_clamp_at_forty_eight_chars() {
        let long = "x".repeat(80);
        assert_eq!(normalize_shortcut_title(&long, "f").chars().count(), 48);
        assert_eq!(normalize_shortcut_title("", "fallback"), "fallback");
    }

    #[test]
    fn query_lookup_decodes_values() {
        assert_eq!(
            query_lookup(Some("pwa_name=My%20App"), "pwa_name").as_deref(),
            Some("My App")
        );
        assert_eq!(query_lookup(Some("other=1"), "pwa_name"), None);
    }
}
