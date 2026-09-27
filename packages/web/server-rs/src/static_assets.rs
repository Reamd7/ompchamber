//! Static UI asset serving with SPA fallback.
//!
//! Ports the behavior of `opencode/static-routes-runtime.js`: serve files from
//! the dist dir; unknown paths fall back to `index.html` EXCEPT paths under
//! `/api`, `/linear`, or asset-like extensions, which 404 (so unknown API
//! routes are not answered with HTML). When the dist dir is missing the SPA
//! fallback answers a "build first" message; `--api-only` serves no assets.
//!
//! 中文说明：本模块负责静态 UI 资源的服务与 SPA 回退。从 dist 目录提供
//! 文件；未知路径回退到 `index.html`，但 `/api`、`/linear` 前缀及带静态资源
//! 扩展名的路径除外（它们直接 404，避免未知 API 路由被 HTML 应答）。
//! dist 目录缺失时 SPA 回退返回"请先构建"提示；`--api-only` 模式不提供任何资源。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// 不做 SPA 回退的路径前缀：这些前缀下的未知路径直接 404（API 与 Linear 回调）。
const SPA_FALLBACK_EXCLUDE_PREFIXES: [&str; 2] = ["/api", "/linear"];
/// 视为静态资源的文件扩展名：带这些扩展名的未知路径直接 404 而非回退到 SPA shell。
const ASSET_EXTENSIONS: [&str; 12] = [
    "js", "css", "svg", "png", "jpg", "jpeg", "gif", "ico", "woff", "woff2", "ttf", "eot",
];

/// 静态资源路由的共享状态（一次性在启动时捕获，请求期间只读）。
pub struct StaticState {
    /// UI 构建产物目录（`OMPCHAMBER_DIST_DIR` 或 `<web package>/dist`）。
    dist_dir: PathBuf,
    /// dist 目录在启动时是否存在；缺失时返回"请先构建"提示而不是panic。
    dist_exists: bool,
    /// `--api-only` / OMPCHAMBER_API_ONLY：不服务任何浏览器 UI 资源。
    api_only: bool,
}

/// 构建静态资源路由：以 SPA 回退处理器作为兜底路由挂载到 axum Router。
pub fn router(config: &crate::config::ServerConfig) -> Router {
    let state = Arc::new(StaticState {
        dist_exists: config.dist_dir.is_dir(),
        dist_dir: config.dist_dir.clone(),
        api_only: config.api_only,
    });
    Router::new().fallback(get(spa_fallback)).with_state(state)
}

/// 判断路径是否允许 SPA 回退：排除 `/api`、`/linear` 前缀（含恰好等于前缀
/// 本身的路径）以及带静态资源扩展名的路径，其余（含无扩展名路由）均回退。
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

/// 按扩展名返回 Content-Type；未识别的扩展名统一 `application/octet-stream`。
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
/// 中文说明：通过 canonicalize 双重校验候选路径仍在 dist 目录内，杜绝
/// `..` 路径穿越；根路径 `/` 映射到 `index.html`。
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

/// 读取并返回一个磁盘文件：按扩展名设置 Content-Type；文件在解析后消失
/// （竞态）时返回 404。
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

/// 兜底处理器，复刻 static-routes-runtime 的分发顺序：
/// api-only → 404；dist 缺失 → "请先构建"；精确文件命中 → 直接服务；
/// SPA 回退路径 → 返回 index.html；其余（API 前缀/资源扩展名）→ 404。
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

/// 静态资源服务的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：SPA 回退排除 /api、/linear 前缀和静态资源扩展名，其余路径放行。
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

    /// 验证：含 `..` 的路径穿越请求被拒绝，不会解析到 dist 目录之外。
    #[test]
    fn traversal_is_rejected() {
        let dir = std::env::temp_dir();
        assert!(resolve_dist_file(&dir, "/../etc/passwd").is_none());
    }
}
