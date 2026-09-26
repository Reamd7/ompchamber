//! Static UI asset serving with SPA fallback.
//!
//! Ports the behavior of `opencode/static-routes-runtime.js`: serve files from
//! the dist dir; unknown paths fall back to `index.html` EXCEPT paths under
//! `/api`, `/linear`, or asset-like extensions, which 404 (so unknown API
//! routes are not answered with HTML). When the dist dir is missing the SPA
//! fallback answers a "build first" message; `--api-only` serves no assets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const SPA_FALLBACK_EXCLUDE_PREFIXES: [&str; 2] = ["/api", "/linear"];
const ASSET_EXTENSIONS: [&str; 12] = [
    "js", "css", "svg", "png", "jpg", "jpeg", "gif", "ico", "woff", "woff2", "ttf", "eot",
];

pub struct StaticState {
    dist_dir: PathBuf,
    dist_exists: bool,
    api_only: bool,
}

pub fn router(config: &crate::config::ServerConfig) -> Router {
    let state = Arc::new(StaticState {
        dist_exists: config.dist_dir.is_dir(),
        dist_dir: config.dist_dir.clone(),
        api_only: config.api_only,
    });
    Router::new().fallback(get(spa_fallback)).with_state(state)
}

fn is_spa_fallback_path(path: &str) -> bool {
    if SPA_FALLBACK_EXCLUDE_PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
    {
        return false;
    }
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.'))
        .map(|(_, ext)| ext);
    match extension {
        Some(ext) if ASSET_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => false,
        _ => true,
    }
}

fn mime_for(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "eot" => "application/vnd.ms-fontobject",
        "webp" => "image/webp",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Resolve `uri.path()` inside the dist dir, rejecting traversal.
fn resolve_dist_file(dist_dir: &Path, path: &str) -> Option<PathBuf> {
    let relative = path.trim_start_matches('/');
    if relative.is_empty() || relative.contains("..") {
        return if relative.is_empty() {
            Some(dist_dir.join("index.html"))
        } else {
            None
        };
    }
    let candidate = dist_dir.join(relative);
    let canonical_dist = dist_dir.canonicalize().ok()?;
    let canonical = candidate.canonicalize().ok()?;
    if canonical.starts_with(&canonical_dist) && canonical.is_file() {
        Some(canonical)
    } else {
        None
    }
}

async fn serve_file(path: PathBuf) -> Response {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(String::from)
        .unwrap_or_default();
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, mime_for(&extension).parse().unwrap());
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "file vanished").into_response(),
    }
}

async fn spa_fallback(State(state): State<Arc<StaticState>>, uri: Uri, _req: Request) -> Response {
    let path = uri.path().to_string();

    if state.api_only {
        // static-routes-runtime `registerApiOnlyFallbackRoutes`: API-only
        // servers serve no browser UI.
        return (
            StatusCode::NOT_FOUND,
            "API-only server; UI assets are not served.",
        )
            .into_response();
    }

    // Exact files first — assets exist or 404, never the SPA shell.
    if !state.dist_exists {
        if is_spa_fallback_path(&path) {
            return (
                StatusCode::NOT_FOUND,
                "Static files not found. Please build the application first.",
            )
                .into_response();
        }
        return (StatusCode::NOT_FOUND, "Static files not found.").into_response();
    }

    if let Some(file) = resolve_dist_file(&state.dist_dir, &path) {
        return serve_file(file).await;
    }

    if is_spa_fallback_path(&path) {
        return serve_file(state.dist_dir.join("index.html")).await;
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spa_fallback_excludes_api_linear_and_assets() {
        assert!(is_spa_fallback_path("/"));
        assert!(is_spa_fallback_path("/session/abc"));
        assert!(!is_spa_fallback_path("/api"));
        assert!(!is_spa_fallback_path("/api/config/settings"));
        assert!(!is_spa_fallback_path("/linear/callback"));
        assert!(!is_spa_fallback_path("/assets/app.js"));
        assert!(!is_spa_fallback_path("/assets/app.css"));
        assert!(is_spa_fallback_path("/docs/app.meta"));
    }

    #[test]
    fn traversal_is_rejected() {
        let dir = std::env::temp_dir();
        assert!(resolve_dist_file(&dir, "/../etc/passwd").is_none());
    }
}
