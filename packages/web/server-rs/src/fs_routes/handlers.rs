//! Route handlers — direct port of the handlers registered by
//! `server/lib/fs/routes.js` `registerFsRoutes`. Status codes, JSON shapes,
//! and error strings mirror the JS exactly.
//! 路由处理器 —— 对 `server/lib/fs/routes.js` 中 `registerFsRoutes`
//! 所注册处理器的直接移植：状态码、JSON 结构与错误文案均与 JS 版逐字对齐。

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::FsState;
use super::exec::{ExecJob, upload_max_bytes};
use super::git_dirs::find_git_directories;
use super::paths::{
    basename, dirname, encode_uri_component, extname_lower, is_path_within_root, js_truthy,
    normalize_directory_path, random_token, random_uuid, realpath, resolve_path,
};
use super::workspace::{
    ReadPathError, git_binary, resolve_read_path_from_context, resolve_workspace_path_from_context,
};

/// `/api/fs/serve` 允许读入内存并返回的最大文件大小（100 MiB），
/// 超限直接返回 413，避免超大文件拖垮服务进程。
const MAX_SERVE_BYTES: u64 = 100 * 1024 * 1024;
/// registerCommonRequestMiddleware mounts `express.json({ limit: '50mb' })`
/// for `/api/fs` paths.
/// `/api/fs` 路径 JSON 请求体的 50 MiB 上限，对齐 JS 端
/// `express.json({ limit: '50mb' })` 中间件的配置。
pub const JSON_BODY_LIMIT: usize = 50 * 1024 * 1024;

/// routes.js `FILE_MIME_MAP` (used by `/api/fs/serve`).
/// 按小写扩展名（不含点）返回 `/api/fs/serve` 的 Content-Type；
/// 未识别的扩展名回退为 `application/octet-stream`。
fn serve_mime(extension: &str) -> &'static str {
    match extension {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "eot" => "application/vnd.ms-fontobject",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

/// The smaller inline mime map on `/api/fs/raw`.
/// `/api/fs/raw` 使用的精简内联 MIME 表，仅覆盖图片与 PDF 等常用类型，
/// 其余扩展名回退为 `application/octet-stream`。
fn raw_mime(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Response helpers (JS `res.status(...).json({ error, ... reason? })`)
// ---------------------------------------------------------------------------

/// 以指定状态码返回 JSON 响应，等价 JS 端 `res.status(...).json(...)`。
fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

/// 构造 `{"error": message}` 形状的 JSON 错误响应。
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(status, json!({ "error": message.into() }))
}

/// 构造 `{"error": message, "reason": reason}` 形状的错误响应，
/// reason 供客户端按机器可读的类别区分失败原因。
fn error_response_with_reason(
    status: StatusCode,
    message: impl Into<String>,
    reason: &str,
) -> Response {
    json_response(status, json!({ "error": message.into(), "reason": reason }))
}

/// routes.js `sendOsPermissionDenied` — `{ error, reason: 'os-permission' }`.
/// Covers both JS EACCES and EPERM (io::ErrorKind::PermissionDenied).
/// 统一封装 OS 层权限拒绝：403 且 `reason: "os-permission"`，
/// 同时覆盖 JS 端的 EACCES 与 EPERM 两种错误码。
fn os_permission_denied(message: &str) -> Response {
    error_response_with_reason(StatusCode::FORBIDDEN, message, "os-permission")
}

/// 判断 IO 错误是否为 NotFound，对应 JS 端 `error.code === 'ENOENT'`。
fn is_not_found(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::NotFound
}

/// 判断 IO 错误是否为 PermissionDenied，对应 JS 端的 EACCES/EPERM。
fn is_permission(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::PermissionDenied
}

/// Body parsing: `express.json` + the global `express.urlencoded` from
/// registerCommonRequestMiddleware. Other content types read as `null`
/// (Express leaves `req.body` undefined and the handlers see `req.body ?? {}`).
/// 按请求头 Content-Type 解析请求体：JSON（含 `+json` 后缀类型）与
/// urlencoded 转为 `Value`，其它类型返回 `Value::Null`，对应 JS 端
/// `req.body ?? {}` 的缺省语义；JSON 解析失败返回 400。
pub async fn parse_body(headers: &HeaderMap, body: Bytes) -> Result<Value, Response> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    if content_type.starts_with("application/json") || content_type.ends_with("+json") {
        return serde_json::from_slice::<Value>(&body)
            .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()));
    }
    if content_type.starts_with("application/x-www-form-urlencoded") {
        // Flat pairs only — `extended: true` bracket nesting is not
        // meaningful for any fs route payload.
        let mut map = serde_json::Map::new();
        for (key, value) in url::form_urlencoded::parse(&body) {
            map.insert(key.into_owned(), Value::String(value.into_owned()));
        }
        return Ok(Value::Object(map));
    }
    Ok(Value::Null)
}

/// 读取查询参数布尔标志：仅当值恰为字符串 "true" 时为真，
/// 与 JS 端 `params.x === 'true'` 的严格比较一致。
fn query_flag(params: &HashMap<String, String>, key: &str) -> bool {
    params.get(key).map(String::as_str) == Some("true")
}

/// 将路径无损转换为 `String`（无效 UTF-8 以 U+FFFD 替换），供 JSON 输出使用。
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 将文件修改时间换算为自 Unix 纪元起的毫秒数（f64，对应 Node 的 `mtimeMs`）；
/// 无法取得时间时返回 0.0。
fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// 构造 200 + 指定 Content-Type 的二进制响应；extra 中的附加头若含
/// 非法头值会被静默跳过而不是报错。
fn bytes_response(mime: &str, extra: &[(header::HeaderName, &str)], bytes: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    if let Ok(value) = header::HeaderValue::from_str(mime) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    for (name, value) in extra {
        if let Ok(value) = header::HeaderValue::from_str(value) {
            response.headers_mut().insert(name.clone(), value);
        }
    }
    response
}

// ---------------------------------------------------------------------------
// GET /api/fs/home
// ---------------------------------------------------------------------------

/// `GET /api/fs/home`：返回当前用户主目录；无法解析时返回 500。
pub async fn home() -> Response {
    match crate::config::home_dir() {
        Some(home) if !home.as_os_str().is_empty() => {
            json_response(StatusCode::OK, json!({ "home": path_string(&home) }))
        }
        _ => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve home directory",
        ),
    }
}

// ---------------------------------------------------------------------------
// POST /api/fs/mkdir
// ---------------------------------------------------------------------------

/// `POST /api/fs/mkdir`：在工作区内递归创建目录（`create_dir_all`）。
/// 请求体需携带 `path`；`allowOutsideWorkspace` 一律拒绝（403，需 grant），
/// 路径越界返回 400，权限拒绝返回 403 os-permission，其余 IO 失败返回 500。
pub async fn mkdir(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let dir_path = parsed
        .get("path")
        .and_then(Value::as_str)
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    if dir_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }
    if js_truthy(parsed.get("allowOutsideWorkspace").unwrap_or(&Value::Null)) {
        tracing::warn!("Rejected outside-workspace mkdir without trusted directory grant");
        return error_response(
            StatusCode::FORBIDDEN,
            "Outside workspace directory creation requires a grant",
        );
    }

    let resolved = match resolve_workspace_path_from_context(&headers, &params, &dir_path).await {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    match tokio::fs::create_dir_all(&resolved.resolved).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({ "success": true, "path": path_string(&resolved.resolved) }),
        ),
        Err(error) => {
            if is_permission(&error) {
                return os_permission_denied("Access denied");
            }
            tracing::error!("Failed to create directory: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/fs/stat
// ---------------------------------------------------------------------------

/// `GET /api/fs/stat`：返回文件元信息（规范路径、isFile、size、mtimeMs）。
/// `optional=true` 时缺失文件以 200 + `{"exists": false}` 应答而非 404；
/// 目标不是普通文件时返回 400。
pub async fn stat(
    State(state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let file_path = params
        .get("path")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    let optional = query_flag(&params, "optional");
    if file_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }

    let resolved =
        match resolve_read_path_from_context(state.grants(), &headers, &params, &file_path, "stat")
            .await
        {
            Ok(resolved) => resolved,
            Err(ReadPathError::Denied(error)) => {
                if query_flag(&params, "allowOutsideWorkspace") {
                    tracing::warn!("Rejected outside-workspace stat: {error}");
                }
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            Err(ReadPathError::Io(error)) => return stat_io_error(&error, &file_path, optional),
        };

    let canonical_path = match realpath(&resolved.resolved) {
        Ok(canonical) => canonical,
        Err(error) => return stat_io_error(&error, &file_path, optional),
    };
    let metadata = match tokio::fs::metadata(&canonical_path).await {
        Ok(metadata) => metadata,
        Err(error) => return stat_io_error(&error, &file_path, optional),
    };
    if !metadata.is_file() {
        return error_response(StatusCode::BAD_REQUEST, "Specified path is not a file");
    }
    json_response(
        StatusCode::OK,
        json!({
            "path": path_string(&canonical_path),
            "isFile": true,
            "size": metadata.len(),
            "mtimeMs": mtime_ms(&metadata),
        }),
    )
}

/// 统一处理 stat 路径的 IO 错误：NotFound 按 optional 语义降级为 200/404，
/// 权限拒绝映射为 403 os-permission，其余记录日志并返回 500。
fn stat_io_error(error: &std::io::Error, file_path: &str, optional: bool) -> Response {
    if is_not_found(error) {
        if optional {
            return json_response(
                StatusCode::OK,
                json!({ "path": file_path, "exists": false }),
            );
        }
        return error_response(StatusCode::NOT_FOUND, "File not found");
    }
    if is_permission(error) {
        return os_permission_denied("Access to file denied");
    }
    tracing::error!("Failed to stat file: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// GET /api/fs/read
// ---------------------------------------------------------------------------

/// `GET /api/fs/read`：以 UTF-8（无效字节替换为 U+FFFD）读取文本文件，
/// 以 `text/plain; charset=utf-8` 返回。stat 与 read 之间若被并发写者截断
/// 导致读到空内容，会按递增退避重试至多 3 次；`optional=true` 时缺失文件
/// 返回 200 空文本而非 404。
pub async fn read(
    State(state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let file_path = params
        .get("path")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    let optional = query_flag(&params, "optional");
    if file_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }

    let resolved =
        match resolve_read_path_from_context(state.grants(), &headers, &params, &file_path, "read")
            .await
        {
            Ok(resolved) => resolved,
            Err(ReadPathError::Denied(error)) => {
                if query_flag(&params, "allowOutsideWorkspace") {
                    tracing::warn!("Rejected outside-workspace read: {error}");
                }
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            Err(ReadPathError::Io(error)) => return read_io_error(&error, optional),
        };

    let canonical_path = match realpath(&resolved.resolved) {
        Ok(canonical) => canonical,
        Err(error) => return read_io_error(&error, optional),
    };
    let metadata = match tokio::fs::metadata(&canonical_path).await {
        Ok(metadata) => metadata,
        Err(error) => return read_io_error(&error, optional),
    };
    if !metadata.is_file() {
        return error_response(StatusCode::BAD_REQUEST, "Specified path is not a file");
    }

    let mut content = match read_utf8_lossy(&canonical_path).await {
        Ok(content) => content,
        Err(error) => return read_io_error(&error, optional),
    };
    // Retry empty reads — a concurrent writer may have truncated the file
    // between the stat and the read (O_TRUNC window).
    if content.is_empty() && metadata.len() > 0 {
        for attempt in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
            content = match read_utf8_lossy(&canonical_path).await {
                Ok(content) => content,
                Err(error) => return read_io_error(&error, optional),
            };
            if !content.is_empty() {
                break;
            }
        }
        if content.is_empty() {
            tracing::warn!(
                "Read retry exhausted for {}: stat reported {} bytes but content is empty",
                canonical_path.display(),
                metadata.len()
            );
        }
    }
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        content,
    )
        .into_response()
}

/// 读取整个文件并以 `from_utf8_lossy` 解码：无效 UTF-8 字节被替换而不是报错。
async fn read_utf8_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 统一处理 read 路径的 IO 错误：NotFound 按 optional 语义返回 404 或空 200，
/// 权限拒绝映射为 403 os-permission，其余记录日志并返回 500。
fn read_io_error(error: &std::io::Error, optional: bool) -> Response {
    if is_not_found(error) {
        if optional {
            return (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                String::new(),
            )
                .into_response();
        }
        return error_response(StatusCode::NOT_FOUND, "File not found");
    }
    if is_permission(error) {
        return os_permission_denied("Access to file denied");
    }
    tracing::error!("Failed to read file: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// GET /api/fs/raw
// ---------------------------------------------------------------------------

/// `GET /api/fs/raw`：读取原始字节并按扩展名设置 MIME。
/// `download=true` 时附加 RFC 5987 编码的 Content-Disposition（含纯 ASCII
/// 回退文件名）；响应恒带 `Cache-Control: no-store`，经 outside grant 授权
/// 读取时额外附加 `Referrer-Policy: no-referrer`。
pub async fn raw(
    State(state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let file_path = params
        .get("path")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    if file_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }

    let resolved =
        match resolve_read_path_from_context(state.grants(), &headers, &params, &file_path, "raw")
            .await
        {
            Ok(resolved) => resolved,
            Err(ReadPathError::Denied(error)) => {
                if query_flag(&params, "allowOutsideWorkspace") {
                    tracing::warn!("Rejected outside-workspace raw read: {error}");
                }
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            Err(ReadPathError::Io(error)) => return raw_io_error(&error),
        };

    let canonical_path = match realpath(&resolved.resolved) {
        Ok(canonical) => canonical,
        Err(error) => return raw_io_error(&error),
    };
    let metadata = match tokio::fs::metadata(&canonical_path).await {
        Ok(metadata) => metadata,
        Err(error) => return raw_io_error(&error),
    };
    if !metadata.is_file() {
        return error_response(StatusCode::BAD_REQUEST, "Specified path is not a file");
    }

    let mime = raw_mime(&extname_lower(&canonical_path));
    let mut extra: Vec<(header::HeaderName, String)> = Vec::new();
    if query_flag(&params, "download") {
        let file_name = basename(&canonical_path);
        // RFC 5987: filename*= for non-ASCII, ASCII-only filename= fallback.
        let ascii_only: String = file_name.chars().filter(char::is_ascii).collect();
        let fallback = if ascii_only.is_empty() {
            "file".to_string()
        } else {
            ascii_only
        };
        let encoded = encode_uri_component(&file_name);
        extra.push((
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"),
        ));
    }
    extra.push((header::CACHE_CONTROL, "no-store".to_string()));
    if resolved.granted {
        extra.push((header::REFERRER_POLICY, "no-referrer".to_string()));
    }

    let bytes = match tokio::fs::read(&canonical_path).await {
        Ok(bytes) => bytes,
        Err(error) => return raw_io_error(&error),
    };
    let extra: Vec<(header::HeaderName, &str)> = extra
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str()))
        .collect();
    bytes_response(mime, &extra, bytes)
}

/// 统一处理 raw 路径的 IO 错误：NotFound → 404，权限拒绝 → 403 os-permission，
/// 其余记录日志并返回 500。
fn raw_io_error(error: &std::io::Error) -> Response {
    if is_not_found(error) {
        return error_response(StatusCode::NOT_FOUND, "File not found");
    }
    if is_permission(error) {
        return os_permission_denied("Access to file denied");
    }
    tracing::error!("Failed to read raw file: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// GET /api/fs/serve/{*path}
// ---------------------------------------------------------------------------

/// `GET /api/fs/serve/{*path}`：静态文件服务。路径先锚定到文件系统根，
/// `..` 片段无法逃逸；`allowOutsideWorkspace` 被该端点显式禁止（403）；
/// 超过 MAX_SERVE_BYTES 的文件返回 413。响应恒带 no-store 与 nosniff 头。
pub async fn serve(
    State(state): State<FsState>,
    AxumPath(raw_path): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if raw_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }
    if query_flag(&params, "allowOutsideWorkspace") {
        return error_response(
            StatusCode::FORBIDDEN,
            "allowOutsideWorkspace is not permitted for this endpoint",
        );
    }

    // JS: `path.resolve('/', rawPath)` — anchor at the filesystem root so
    // `..` segments can never escape it.
    let file_path = resolve_path(&raw_path);
    let resolved = match resolve_read_path_from_context(
        state.grants(),
        &headers,
        &params,
        &path_string(&file_path),
        "read",
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(ReadPathError::Denied(error)) => return error_response(StatusCode::BAD_REQUEST, error),
        Err(ReadPathError::Io(error)) => return serve_io_error(&error),
    };

    let canonical_path = match realpath(&resolved.resolved) {
        Ok(canonical) => canonical,
        Err(error) => return serve_io_error(&error),
    };
    let metadata = match tokio::fs::metadata(&canonical_path).await {
        Ok(metadata) => metadata,
        Err(error) => return serve_io_error(&error),
    };
    if !metadata.is_file() {
        return error_response(StatusCode::BAD_REQUEST, "Specified path is not a file");
    }
    if metadata.len() > MAX_SERVE_BYTES {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "File too large to serve");
    }

    let mime = serve_mime(&extname_lower(&canonical_path));
    let bytes = match tokio::fs::read(&canonical_path).await {
        Ok(bytes) => bytes,
        Err(error) => return serve_io_error(&error),
    };
    bytes_response(
        mime,
        &[
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
}

/// 统一处理 serve 路径的 IO 错误：NotFound → 404，权限拒绝 → 403 os-permission，
/// 其余记录日志并返回 500。
fn serve_io_error(error: &std::io::Error) -> Response {
    if is_not_found(error) {
        return error_response(StatusCode::NOT_FOUND, "File not found");
    }
    if is_permission(error) {
        return os_permission_denied("Access to file denied");
    }
    tracing::error!("Failed to serve file: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/fs/write
// ---------------------------------------------------------------------------

/// `POST /api/fs/write`：原子写入文本文件 —— 先写同目录 `*.tmp-*` 临时文件
/// 再 rename 落位，读者不会观察到直接覆盖写的截断窗口。目标内容与现有
/// 文件完全相同时跳过重写；写前自动补齐父目录；规范路径逃逸工作区根时
/// 返回 403。
pub async fn write(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let file_path = match parsed.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(StatusCode::BAD_REQUEST, "Path is required"),
    };
    let Some(content) = parsed.get("content").and_then(Value::as_str) else {
        return error_response(StatusCode::BAD_REQUEST, "Content is required");
    };

    let resolved = match resolve_workspace_path_from_context(&headers, &params, file_path).await {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    let write_path = match realpath(&resolved.resolved) {
        Ok(canonical) => canonical,
        Err(error) if is_not_found(&error) => resolved.resolved.clone(),
        Err(error) => return write_io_error(&error),
    };
    let canonical_base =
        realpath(&resolved.base).unwrap_or_else(|_| resolve_path(&path_string(&resolved.base)));
    if !is_path_within_root(&write_path, &canonical_base) {
        return error_response(StatusCode::FORBIDDEN, "Access denied");
    }

    // Skip the rewrite when the file already holds exactly this content.
    if let Ok(existing) = tokio::fs::read_to_string(&write_path).await
        && existing == content
    {
        return json_response(
            StatusCode::OK,
            json!({ "success": true, "path": path_string(&resolved.resolved) }),
        );
    }

    if let Err(error) = tokio::fs::create_dir_all(dirname(&write_path)).await {
        return write_io_error(&error);
    }

    // Atomic write: temp file + rename so concurrent readers never observe
    // the O_TRUNC window of a direct write.
    let tmp = PathBuf::from(format!(
        "{}.tmp-{}-{}-{}",
        path_string(&write_path),
        std::process::id(),
        super::exec::now_ms(),
        random_token(6),
    ));
    let write_result: Result<(), std::io::Error> = async {
        tokio::fs::write(&tmp, content.as_bytes()).await?;
        tokio::fs::rename(&tmp, &write_path).await?;
        Ok(())
    }
    .await;
    if let Err(error) = write_result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return write_io_error(&error);
    }
    json_response(
        StatusCode::OK,
        json!({ "success": true, "path": path_string(&resolved.resolved) }),
    )
}

/// 统一处理 write 路径的 IO 错误：权限拒绝映射为 403 os-permission，
/// 其余记录日志并返回 500。
fn write_io_error(error: &std::io::Error) -> Response {
    if is_permission(error) {
        return os_permission_denied("Access denied");
    }
    tracing::error!("Failed to write file: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/fs/upload
// ---------------------------------------------------------------------------

/// upload 流程内部的错误分类，最终经 upload_error_response 映射为 HTTP 响应。
enum UploadError {
    /// 请求体超过上传大小上限。
    TooLarge,
    /// 临时文件写入失败（IO 错误，如磁盘已满）。
    Write,
    /// 请求体流本身出错，携带底层错误消息。
    Body(String),
    /// 目标文件已存在且未带 overwrite=true。
    Exists,
    /// 目标父目录不存在。
    Missing,
    /// OS 层权限拒绝。
    Permission,
    /// 目标路径是目录。
    Directory,
    /// 其它 IO 错误，携带原始错误消息。
    Other(String),
}

/// 将 `std::io::ErrorKind` 归类为 UploadError，让上传各阶段共享统一映射。
fn upload_error_kind(error: &std::io::Error) -> UploadError {
    match error.kind() {
        ErrorKind::AlreadyExists => UploadError::Exists,
        ErrorKind::NotFound => UploadError::Missing,
        ErrorKind::PermissionDenied => UploadError::Permission,
        ErrorKind::IsADirectory | ErrorKind::NotADirectory => UploadError::Directory,
        _ => UploadError::Other(error.to_string()),
    }
}

/// 将 UploadError 映射为与 JS 一致的 HTTP 响应：413 超限、409 already-exists、
/// 404 not-found、403 os-permission、400 目录目标、500 其余（含流/写失败）。
fn upload_error_response(error: UploadError, max_upload_bytes: u64) -> Response {
    match error {
        UploadError::TooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("File exceeds maximum size of {max_upload_bytes} bytes"),
        ),
        UploadError::Write => {
            tracing::error!("Failed to upload file: Failed to write upload");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to write upload")
        }
        UploadError::Exists => error_response_with_reason(
            StatusCode::CONFLICT,
            "File already exists",
            "already-exists",
        ),
        UploadError::Missing => error_response_with_reason(
            StatusCode::NOT_FOUND,
            "Destination directory not found",
            "not-found",
        ),
        UploadError::Permission => os_permission_denied("Access denied"),
        UploadError::Directory => {
            error_response(StatusCode::BAD_REQUEST, "Specified path is a directory")
        }
        UploadError::Body(message) | UploadError::Other(message) => {
            tracing::error!("Failed to upload file: {message}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

/// upload_error_kind 与 upload_error_response 的组合：把 IO 错误直接转为响应。
fn upload_io_error(error: &std::io::Error, max_upload_bytes: u64) -> Response {
    upload_error_response(upload_error_kind(error), max_upload_bytes)
}

/// `POST /api/fs/upload`：流式接收 `application/octet-stream` 请求体并落盘。
/// 先校验 Content-Type 与声明的 Content-Length 上限；父目录与目标路径
/// （含已存在目标的规范路径）都必须位于工作区根内，否则 400/403；
/// 已存在目标未带 overwrite=true 时返回 409 already-exists，是目录时
/// 返回 400。实际写入交由 stream_upload_to 原子提交。
pub async fn upload(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let file_path = params
        .get("path")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    let overwrite = query_flag(&params, "overwrite");
    if file_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    if !content_type.starts_with("application/octet-stream") {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/octet-stream",
        );
    }

    let max_upload_bytes = upload_max_bytes();
    if let Some(declared) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && declared > max_upload_bytes
    {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("File exceeds maximum size of {max_upload_bytes} bytes"),
        );
    }

    let resolved = match resolve_workspace_path_from_context(&headers, &params, &file_path).await {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    let canonical_base =
        realpath(&resolved.base).unwrap_or_else(|_| resolve_path(&path_string(&resolved.base)));
    let requested_parent = dirname(&resolved.resolved);
    let canonical_parent = match realpath(&requested_parent) {
        Ok(parent) => parent,
        Err(error) => return upload_io_error(&error, max_upload_bytes),
    };
    if !is_path_within_root(&canonical_parent, &canonical_base) {
        return error_response(StatusCode::FORBIDDEN, "Access denied");
    }

    let existing_path = match realpath(&resolved.resolved) {
        Ok(existing) => Some(existing),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return upload_io_error(&error, max_upload_bytes),
    };
    let write_path = existing_path
        .clone()
        .unwrap_or_else(|| canonical_parent.join(basename(&resolved.resolved)));
    if !is_path_within_root(&write_path, &canonical_base) {
        return error_response(StatusCode::FORBIDDEN, "Access denied");
    }

    if let Some(existing) = &existing_path {
        match tokio::fs::metadata(existing).await {
            Ok(metadata) => {
                if metadata.is_dir() {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "Specified path is a directory",
                    );
                }
                if !overwrite {
                    return error_response_with_reason(
                        StatusCode::CONFLICT,
                        "File already exists",
                        "already-exists",
                    );
                }
            }
            Err(error) => return upload_io_error(&error, max_upload_bytes),
        }
    }

    let tmp = PathBuf::from(format!(
        "{}.upload-{}",
        path_string(&write_path),
        random_uuid()
    ));
    match stream_upload_to(&tmp, &write_path, body, max_upload_bytes, overwrite).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({ "success": true, "path": path_string(&resolved.resolved) }),
        ),
        Err(error) => upload_error_response(error, max_upload_bytes),
    }
}

/// routes.js `streamUploadBody` + the atomic commit (`rename` on overwrite,
/// no-replace `link` otherwise). The temp file is always cleaned up on
/// failure.
/// 请求体边流式写入临时文件边累计字节数，超限立即中止；全部写完后按
/// overwrite 选择 rename（可覆盖）或同目录 hard_link（绝不覆盖检查后才
/// 出现的目标）提交。任何失败路径都会清理临时文件。
async fn stream_upload_to(
    tmp: &Path,
    write_path: &Path,
    body: Body,
    max_bytes: u64,
    overwrite: bool,
) -> Result<(), UploadError> {
    let mut file = tokio::fs::File::create_new(tmp)
        .await
        .map_err(|error| upload_error_kind(&error))?;
    let mut received: u64 = 0;
    let mut stream_error: Option<UploadError> = None;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                stream_error = Some(UploadError::Body(error.to_string()));
                break;
            }
        };
        received += chunk.len() as u64;
        if received > max_bytes {
            stream_error = Some(UploadError::TooLarge);
            break;
        }
        if file.write_all(&chunk).await.is_err() {
            stream_error = Some(UploadError::Write);
            break;
        }
    }
    if stream_error.is_none()
        && let Err(error) = file.flush().await
    {
        stream_error = Some(upload_error_kind(&error));
    }
    drop(file);

    if let Some(error) = stream_error {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(error);
    }

    let commit = if overwrite {
        tokio::fs::rename(tmp, write_path).await
    } else {
        // A same-directory hard link commits without replacing a target that
        // appeared after the existence check.
        match tokio::fs::hard_link(tmp, write_path).await {
            Ok(()) => tokio::fs::remove_file(tmp).await,
            Err(error) => Err(error),
        }
    };
    if let Err(error) = commit {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(upload_error_kind(&error));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /api/fs/delete
// ---------------------------------------------------------------------------

/// `POST /api/fs/delete`：删除文件或目录（递归），语义等价
/// `rm(recursive: true, force: true)` —— 目标缺失也算成功。
/// 路径越界返回 400；执行失败按类别映射 404/403 os-permission/500。
pub async fn delete(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let target_path = match parsed.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(StatusCode::BAD_REQUEST, "Path is required"),
    };

    let resolved = match resolve_workspace_path_from_context(&headers, &params, target_path).await {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    // `rm(recursive: true, force: true)`: missing targets are not an error.
    match remove_force(&resolved.resolved).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({ "success": true, "path": path_string(&resolved.resolved) }),
        ),
        Err(error) => {
            if is_not_found(&error) {
                return error_response(StatusCode::NOT_FOUND, "File or directory not found");
            }
            if is_permission(&error) {
                return os_permission_denied("Access denied");
            }
            tracing::error!("Failed to delete path: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}

/// 强制删除：目标不存在时直接返回 Ok（force 语义）；目录用 remove_dir_all
/// 递归删除，其余（含符号链接本身）用 remove_file 删除。
async fn remove_force(path: &Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.is_dir() {
                tokio::fs::remove_dir_all(path).await
            } else {
                tokio::fs::remove_file(path).await
            }
        }
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// POST /api/fs/rename
// ---------------------------------------------------------------------------

/// `POST /api/fs/rename`：在工作区内重命名/移动路径。oldPath 与 newPath
/// 必须解析到同一个工作区根，否则 400；源缺失 404；权限拒绝 403。
pub async fn rename(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let old_path = match parsed.get("oldPath").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(StatusCode::BAD_REQUEST, "oldPath is required"),
    };
    let new_path = match parsed.get("newPath").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(StatusCode::BAD_REQUEST, "newPath is required"),
    };

    let resolved_old = match resolve_workspace_path_from_context(&headers, &params, old_path).await
    {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let resolved_new = match resolve_workspace_path_from_context(&headers, &params, new_path).await
    {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    if resolved_old.base != resolved_new.base {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Source and destination must share the same workspace root",
        );
    }

    match tokio::fs::rename(&resolved_old.resolved, &resolved_new.resolved).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({ "success": true, "path": path_string(&resolved_new.resolved) }),
        ),
        Err(error) => {
            if is_not_found(&error) {
                return error_response(StatusCode::NOT_FOUND, "Source path not found");
            }
            if is_permission(&error) {
                return os_permission_denied("Access denied");
            }
            tracing::error!("Failed to rename path: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/fs/reveal
// ---------------------------------------------------------------------------

/// Pure decision extracted from the JS handler so it is testable without
/// spawning a desktop launcher. Returns (program, args, wait_for_exit).
/// 按平台构造文件管理器调用：macOS 用 open（文件加 -R 在 Finder 中选中），
/// Windows 经 powershell 启动 explorer 并等待退出，其余平台用 xdg-open
/// 打开目标（或其所在目录）。返回 (程序, 参数列表, 是否等待退出)。
pub fn reveal_invocation(
    platform: &str,
    resolved: &Path,
    is_dir: bool,
) -> (String, Vec<String>, bool) {
    match platform {
        "macos" => {
            if is_dir {
                ("open".to_string(), vec![path_string(resolved)], false)
            } else {
                (
                    "open".to_string(),
                    vec!["-R".to_string(), path_string(resolved)],
                    false,
                )
            }
        }
        "windows" => {
            let escaped = path_string(resolved).replace('\'', "''");
            let explorer_arg = if is_dir {
                escaped.clone()
            } else {
                format!("/select,{escaped}")
            };
            let command =
                format!("Start-Process -FilePath explorer.exe -ArgumentList '{explorer_arg}'");
            (
                "powershell.exe".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    command,
                ],
                true,
            )
        }
        _ => {
            let dir = if is_dir {
                resolved.to_path_buf()
            } else {
                dirname(resolved)
            };
            ("xdg-open".to_string(), vec![path_string(&dir)], false)
        }
    }
}

/// `POST /api/fs/reveal`：在系统文件管理器中定位给定路径。
/// 与 JS 版一致不做工作区校验；路径缺失 404；Windows 分支需等待子进程
/// 退出以捕获 explorer 启动失败（非零退出码 → 500）。
pub async fn reveal(
    State(_state): State<FsState>,
    Query(_params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let target_path = match parsed.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        _ => return error_response(StatusCode::BAD_REQUEST, "Path is required"),
    };

    let resolved = resolve_path(target_path.trim());
    if let Err(error) = tokio::fs::symlink_metadata(&resolved).await {
        if is_not_found(&error) {
            return error_response(StatusCode::NOT_FOUND, "Path not found");
        }
        if is_permission(&error) {
            return os_permission_denied("Access to path denied");
        }
        tracing::error!("Failed to reveal path: {error}");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let is_dir = tokio::fs::metadata(&resolved)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    let (program, args, wait_for_exit) = reveal_invocation(std::env::consts::OS, &resolved, is_dir);
    let spawn_result = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(error) => {
            tracing::error!("Failed to reveal path: {error}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to launch file browser",
            );
        }
    };
    if wait_for_exit {
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let message = format!(
                    "Explorer launch failed with code {}",
                    status.code().unwrap_or_default()
                );
                tracing::error!("Failed to reveal path: {message}");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, message);
            }
            Err(error) => {
                tracing::error!("Failed to reveal path: {error}");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to launch file browser",
                );
            }
        }
    }
    json_response(
        StatusCode::OK,
        json!({ "success": true, "path": path_string(&resolved) }),
    )
}

// ---------------------------------------------------------------------------
// POST /api/fs/exec + GET /api/fs/exec/{jobId}
// ---------------------------------------------------------------------------

/// `POST /api/fs/exec`：在位于工作区内的 cwd 中同步执行一批 shell 命令。
/// 校验 commands 非空、cwd 存在且为目录、`background: true` 一律拒绝；
/// 执行前先清理过期 exec job 与 git 读缓存。命令以内联 job 跑完，
/// 返回 jobId 与逐条结果（success/stdout/exitCode）。
pub async fn exec(
    State(state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let commands = match parsed.get("commands").and_then(Value::as_array) {
        Some(commands) if !commands.is_empty() => commands.clone(),
        _ => return error_response(StatusCode::BAD_REQUEST, "Commands array is required"),
    };
    let cwd = match parsed.get("cwd").and_then(Value::as_str) {
        Some(cwd) if !cwd.is_empty() => cwd,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Working directory (cwd) is required",
            );
        }
    };

    state.exec_jobs.prune();
    state.git_read_cache.prune();

    if parsed.get("background") == Some(&Value::Bool(true)) {
        tracing::warn!("Rejected background /api/fs/exec request");
        return error_response(
            StatusCode::BAD_REQUEST,
            "Background command execution is not allowed",
        );
    }

    let resolved_cwd_candidate = resolve_path(&normalize_directory_path(cwd));
    let resolved_for_workspace = match resolve_workspace_path_from_context(
        &headers,
        &params,
        &path_string(&resolved_cwd_candidate),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!("Rejected /api/fs/exec outside workspace: {error}");
            return error_response(StatusCode::FORBIDDEN, error);
        }
    };
    let resolved_cwd = resolved_for_workspace.resolved;
    match tokio::fs::metadata(&resolved_cwd).await {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return error_response(StatusCode::BAD_REQUEST, "Specified cwd is not a directory");
        }
        Err(error) => {
            tracing::error!("Failed to execute commands: {error}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to execute commands",
            );
        }
    }

    let (shell, shell_flag) = super::exec::resolve_shell();
    let job_id = super::exec::new_job_id();
    let now = super::exec::now_ms();
    let job = state.exec_jobs.insert(ExecJob {
        job_id: job_id.clone(),
        status: "queued",
        success: None,
        resolved_cwd,
        results: Vec::new(),
        started_at: now,
        finished_at: None,
        updated_at: now,
    });

    let (success, results) = state
        .exec_jobs
        .run_inline(
            job,
            &commands,
            &shell,
            shell_flag,
            &state.git_read_cache,
            state.command_timeout_ms,
        )
        .await;
    json_response(
        StatusCode::OK,
        json!({
            "jobId": job_id,
            "status": "done",
            "success": success,
            "results": results,
        }),
    )
}

/// `GET /api/fs/exec/{jobId}`：查询执行 job 的状态与结果，未知 jobId 返回 404；
/// 每次访问刷新 updated_at 以推迟过期淘汰。
pub async fn exec_job(
    State(state): State<FsState>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    state.exec_jobs.prune();
    let Some(job) = state.exec_jobs.get(&job_id) else {
        return error_response(StatusCode::NOT_FOUND, "Job not found");
    };
    let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
    guard.updated_at = super::exec::now_ms();
    json_response(
        StatusCode::OK,
        json!({
            "jobId": guard.job_id,
            "status": guard.status,
            "success": guard.success.unwrap_or(false),
            "results": guard.results,
        }),
    )
}

// ---------------------------------------------------------------------------
// GET /api/fs/list
// ---------------------------------------------------------------------------

/// 判断路径是否指向 `.opencode/plans` 计划目录（兼容反斜杠与尾随斜杠）；
/// 该目录缺失时 list 端点以空列表 200 应答而非 404。
fn is_plans_directory(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let normalized = value.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    normalized.ends_with("/.opencode/plans") || normalized.ends_with(".opencode/plans")
}

/// `GET /api/fs/list`：列出目录内容（缺省为主目录）。经 realpath 缓存解析
/// 实际目录，但响应中的 path 与条目 path 保持在调用方请求的路径空间
/// （不展开符号链接，issue 2627）；条目按名称排序以匹配 Node readdir；
/// `respectGitignore=true` 时用 git check-ignore 过滤被忽略条目。
pub async fn list(
    State(state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    _headers: HeaderMap,
) -> Response {
    let raw_path = params
        .get("path")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path_string(&crate::config::home_dir().unwrap_or_default()));
    let respect_gitignore = query_flag(&params, "respectGitignore");

    let requested_path = resolve_path(&normalize_directory_path(&raw_path));
    let resolved_path = match state.realpath_cache.resolve(&requested_path) {
        Ok(resolved) => resolved,
        Err(error) => return list_io_error(&error, &requested_path, &raw_path),
    };

    let metadata = match tokio::fs::metadata(&resolved_path).await {
        Ok(metadata) => metadata,
        Err(error) => return list_io_error(&error, &requested_path, &raw_path),
    };
    if !metadata.is_dir() {
        return error_response_with_reason(
            StatusCode::BAD_REQUEST,
            "Specified path is not a directory",
            "not-directory",
        );
    }

    let mut dirents = match tokio::fs::read_dir(&resolved_path).await {
        Ok(dirents) => dirents,
        Err(error) => return list_io_error(&error, &requested_path, &raw_path),
    };
    let mut entries: Vec<(String, bool, bool, bool)> = Vec::new();
    while let Ok(Some(entry)) = dirents.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry
            .file_type()
            .await
            .or_else(|_| std::fs::symlink_metadata(entry.path()).map(|m| m.file_type()))
        else {
            continue;
        };
        let is_symbolic_link = file_type.is_symlink();
        let mut is_directory = file_type.is_dir();
        let is_file = file_type.is_file();
        if !is_directory && is_symbolic_link {
            is_directory = tokio::fs::metadata(entry.path())
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
        }
        entries.push((name, is_directory, is_file, is_symbolic_link));
    }
    // Node's readdir returns name-sorted dirents on macOS; the JS list route
    // relies on that order. Sort to match the observable sequence.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let ignored_paths = if respect_gitignore {
        run_check_ignore(&resolved_path, &entries, state.git_check_ignore_timeout_ms).await
    } else {
        HashMap::new()
    };

    let mut response_entries: Vec<Value> = Vec::new();
    for (name, is_directory, is_file, is_symbolic_link) in entries {
        let physical_entry_path = resolved_path.join(&name);
        if respect_gitignore && ignored_paths.contains_key(&physical_entry_path) {
            continue;
        }
        response_entries.push(json!({
            "name": name,
            "path": path_string(&requested_path.join(&name)),
            "isDirectory": is_directory,
            "isFile": is_file,
            "isSymbolicLink": is_symbolic_link,
        }));
    }

    json_response(
        StatusCode::OK,
        json!({ "path": path_string(&requested_path), "entries": response_entries }),
    )
}

/// 统一处理 list 路径的 IO 错误：NotFound 且目标是 plans 目录时返回空 200，
/// 否则 404 not-found；权限拒绝 403 os-permission；其余记录日志并返回 500。
fn list_io_error(error: &std::io::Error, requested_path: &Path, raw_path: &str) -> Response {
    if is_not_found(error) {
        let is_plans =
            is_plans_directory(&path_string(requested_path)) || is_plans_directory(raw_path);
        if is_plans {
            let path = if requested_path.as_os_str().is_empty() {
                raw_path.to_string()
            } else {
                path_string(requested_path)
            };
            return json_response(StatusCode::OK, json!({ "path": path, "entries": [] }));
        }
        return error_response_with_reason(
            StatusCode::NOT_FOUND,
            "Directory not found",
            "not-found",
        );
    }
    if is_permission(error) {
        return os_permission_denied("Access to directory denied");
    }
    tracing::error!("Failed to list directory: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

/// `git check-ignore -- <names>` with the JS kill-on-timeout behavior
/// (timeout → empty result).
/// 在 directory 下执行 `git check-ignore -- <names>`，返回被忽略条目的
/// 完整路径集合；超时、启动失败或非零退出一律返回空集合（不隐藏任何条目）。
async fn run_check_ignore(
    directory: &Path,
    entries: &[(String, bool, bool, bool)],
    timeout_ms: u64,
) -> HashMap<PathBuf, ()> {
    let names: Vec<&str> = entries.iter().map(|(name, ..)| name.as_str()).collect();
    if names.is_empty() {
        return HashMap::new();
    }
    let mut command = tokio::process::Command::new(git_binary());
    command
        .arg("check-ignore")
        .arg("--")
        .args(&names)
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = if timeout_ms > 0 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            command.output(),
        )
        .await
        {
            Ok(output) => output,
            Err(_elapsed) => return HashMap::new(),
        }
    } else {
        command.output().await
    };
    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|name| (directory.join(name.trim()), ()))
        .collect()
}

// ---------------------------------------------------------------------------
// GET /api/fs/git-dirs
// ---------------------------------------------------------------------------

/// `GET /api/fs/git-dirs`：扫描工作区目录下的 git 仓库（含指向 worktree 的
/// `.git` 文件），返回 path/name 列表；目标不是目录返回 400 not-directory。
pub async fn git_dirs(
    State(_state): State<FsState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let raw_path = params
        .get("path")
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    if raw_path.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Path is required");
    }

    let resolved = match resolve_workspace_path_from_context(&headers, &params, &raw_path).await {
        Ok(resolved) => resolved,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    match tokio::fs::metadata(&resolved.resolved).await {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return error_response_with_reason(
                    StatusCode::BAD_REQUEST,
                    "Specified path is not a directory",
                    "not-directory",
                );
            }
        }
        Err(error) => return git_dirs_io_error(&error),
    }

    match find_git_directories(&resolved.resolved) {
        Ok(repositories) => {
            let repositories: Vec<Value> = repositories
                .iter()
                .map(|repo_path| {
                    json!({
                        "path": path_string(repo_path),
                        "name": basename(repo_path),
                    })
                })
                .collect();
            json_response(
                StatusCode::OK,
                json!({
                    "path": path_string(&resolved.resolved),
                    "repositories": repositories,
                }),
            )
        }
        Err(error) => git_dirs_io_error(&error),
    }
}

/// 统一处理 git-dirs 路径的 IO 错误：NotFound → 404 not-found，
/// 权限拒绝 → 403 os-permission，其余记录日志并返回 500。
fn git_dirs_io_error(error: &std::io::Error) -> Response {
    if is_not_found(error) {
        return error_response_with_reason(
            StatusCode::NOT_FOUND,
            "Directory not found",
            "not-found",
        );
    }
    if is_permission(error) {
        return os_permission_denied("Access to directory denied");
    }
    tracing::error!("Failed to find git directories: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/fs/clone
// ---------------------------------------------------------------------------

/// routes.js `deriveCloneDirectoryName`.
/// 从远端 URL 推断克隆目录名：去掉 query/fragment，取最后一段路径或
/// scp 风格尾部，剥离 `.git` 后缀；推不出名字时返回空串。
pub fn derive_clone_directory_name(remote_url: &str) -> String {
    let remote = remote_url.trim();
    if remote.is_empty() {
        return String::new();
    }
    let without_query = remote.split(['?', '#']).next().unwrap_or(remote);
    let tail = without_query
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(without_query);
    let tail = tail.trim_end_matches('/');
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    name.trim().to_string()
}

/// 克隆时携带的 git 身份：用户名、邮箱与可选 SSH 私钥路径。
struct CloneIdentity {
    /// 提交时使用的 user.name。
    user_name: String,
    /// 提交时使用的 user.email。
    user_email: String,
    /// 可选 SSH 私钥路径，注入 core.sshCommand 用于拉取私有仓库。
    ssh_key: Option<String>,
}

/// routes.js `resolveCloneGitIdentity` — `global` reads `git config
/// --global`; named ids resolve against `~/.config/ompchamber/git-identities.json`
/// (git/identity-storage.js).
/// 解析 gitIdentityId："global" 读取 `git config --global`（user.name 与
/// user.email 任一缺失即视为无身份），其它 id 在用户级 git-identities.json
/// 的 profiles 数组中按 id 匹配；找不到返回 None，克隆退回匿名。
async fn resolve_clone_git_identity(git_identity_id: &str) -> Option<CloneIdentity> {
    let id = git_identity_id.trim();
    if id.is_empty() {
        return None;
    }
    if id == "global" {
        /// 读取一条 `git config --global` 配置：命令失败或值为空时返回 None。
        async fn git_config(key: &str) -> Option<String> {
            let output = tokio::process::Command::new(git_binary())
                .args(["config", "--global", "--get", key])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!value.is_empty()).then_some(value)
        }
        let user_name = git_config("user.name").await.unwrap_or_default();
        let user_email = git_config("user.email").await.unwrap_or_default();
        if user_name.is_empty() || user_email.is_empty() {
            return None;
        }
        let ssh_command = git_config("core.sshCommand").await.unwrap_or_default();
        let ssh_key = ssh_command.strip_prefix("ssh -i ").map(str::to_string);
        return Some(CloneIdentity {
            user_name,
            user_email,
            ssh_key,
        });
    }

    let profiles_path = super::paths::user_config_root().join("git-identities.json");
    let profiles: Value = std::fs::read_to_string(profiles_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    let profile = profiles
        .get("profiles")
        .and_then(Value::as_array)?
        .iter()
        .find(|profile| profile.get("id").and_then(Value::as_str) == Some(id))?;
    Some(CloneIdentity {
        user_name: profile.get("userName").and_then(Value::as_str)?.to_string(),
        user_email: profile
            .get("userEmail")
            .and_then(Value::as_str)?
            .to_string(),
        ssh_key: profile
            .get("sshKey")
            .and_then(Value::as_str)
            .map(|value| value.trim())
            .filter(|key| !key.is_empty())
            .map(str::to_string),
    })
}

/// routes.js `escapeCloneSshKeyPath`.
/// 为拼入 core.sshCommand 的私钥路径做单引号转义：拒绝所有 shell 元字符；
/// Windows 下先把盘符路径改写为 `/c/...` 形式再整体加引号。
/// 含非法字符时返回 Err，由调用方转为 500。
pub(crate) fn escape_clone_ssh_key_path(ssh_key_path: &str) -> Result<String, String> {
    let raw = ssh_key_path.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let normalized = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    if normalized.chars().any(|c| {
        matches!(
            c,
            '`' | '$'
                | '!'
                | '"'
                | '\''
                | ';'
                | '&'
                | '|'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '*'
                | '?'
                | '#'
                | '~'
        )
    }) {
        return Err(format!("SSH key path contains invalid characters: {raw}"));
    }
    if cfg!(windows) {
        let bytes = normalized.as_bytes();
        let unix_path = if normalized.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/'
        {
            format!(
                "/{}{}",
                normalized[..1].to_ascii_lowercase(),
                &normalized[2..]
            )
        } else {
            normalized
        };
        return Ok(format!("'{unix_path}'"));
    }
    Ok(format!("'{}'", normalized.replace('\'', "'\\''")))
}

/// `POST /api/fs/clone`：在目标父目录下执行 `git clone`。目标以分隔符结尾
/// 或已是目录时从远端 URL 推断目录名；携带 gitIdentityId 时通过
/// `-c core.sshCommand=...` 注入私钥并设 GIT_TERMINAL_PROMPT=0 禁止交互。
/// 目标已存在返回 409；克隆成功后尽力写入本地 user.name/user.email/
/// core.sshCommand 并 unset credential.helper（失败仅告警不影响结果）。
pub async fn clone(
    State(_state): State<FsState>,
    Query(_params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_body(&headers, body).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let remote = parsed
        .get("remoteUrl")
        .and_then(Value::as_str)
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    let destination = parsed
        .get("destinationPath")
        .and_then(Value::as_str)
        .map(|value| value.trim())
        .unwrap_or_default()
        .to_string();
    if remote.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Repository URL is required");
    }
    if destination.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "Destination path is required");
    }

    let mut resolved_destination = resolve_path(&normalize_directory_path(&destination));
    let mut parent_path = dirname(&resolved_destination);
    let mut directory_name = basename(&resolved_destination);

    let clone_into_destination_directory =
        destination.ends_with('/') || destination.ends_with('\\');
    let mut inferred_from_directory_target = false;
    if clone_into_destination_directory {
        inferred_from_directory_target = true;
    } else {
        match tokio::fs::metadata(&resolved_destination).await {
            Ok(metadata) if metadata.is_dir() => inferred_from_directory_target = true,
            Ok(_) => {}
            Err(error) if is_not_found(&error) => {}
            Err(error) => return clone_error(&error),
        }
    }
    if inferred_from_directory_target {
        let inferred_name = derive_clone_directory_name(&remote);
        if inferred_name.is_empty() {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Could not infer repository directory name from URL",
            );
        }
        parent_path = resolved_destination.clone();
        directory_name = inferred_name;
        resolved_destination = parent_path.join(&directory_name);
    }
    if directory_name.is_empty() || directory_name == "." || directory_name == ".." {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Destination path must include a directory name",
        );
    }

    let identity = match parsed.get("gitIdentityId").and_then(Value::as_str) {
        Some(id) => resolve_clone_git_identity(id).await,
        None => None,
    };

    let mut git_args: Vec<String> = vec![
        "clone".to_string(),
        "--".to_string(),
        remote.clone(),
        directory_name.clone(),
    ];
    if let Some(ssh_key) = identity
        .as_ref()
        .and_then(|identity| identity.ssh_key.as_deref())
        && !ssh_key.trim().is_empty()
    {
        let escaped = match escape_clone_ssh_key_path(ssh_key) {
            Ok(escaped) => escaped,
            Err(message) => {
                tracing::error!("Failed to clone repository: {message}");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, message);
            }
        };
        git_args.insert(
                0,
                format!(
                    "core.sshCommand=ssh -i {escaped} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
                ),
            );
        git_args.insert(0, "-c".to_string());
    }

    if let Err(error) = tokio::fs::create_dir_all(&parent_path).await {
        return clone_error(&error);
    }
    match tokio::fs::symlink_metadata(&resolved_destination).await {
        Ok(_) => return error_response(StatusCode::CONFLICT, "Destination path already exists"),
        Err(error) if is_not_found(&error) => {}
        Err(error) => return clone_error(&error),
    }

    let output = tokio::process::Command::new(git_binary())
        .args(&git_args)
        .current_dir(&parent_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    let (combined, success) = match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            (
                format!("{stdout}\n{stderr}").trim().to_string(),
                output.status.success(),
            )
        }
        Err(error) => return clone_error(&error),
    };
    if !success {
        let message = if combined.is_empty() {
            "git clone failed".to_string()
        } else {
            combined
        };
        tracing::error!("Failed to clone repository: {message}");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, message);
    }

    // setLocalIdentity (best-effort; failures only warn).
    if let Some(identity) = &identity
        && !identity.user_name.is_empty()
        && !identity.user_email.is_empty()
    {
        for (key, value) in [
            ("user.name", &identity.user_name),
            ("user.email", &identity.user_email),
        ] {
            let result = tokio::process::Command::new(git_binary())
                .args(["config", "--local", key, value])
                .current_dir(&resolved_destination)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await;
            if let Err(error) = result {
                tracing::warn!("Failed to apply git identity after clone: {error}");
            }
        }
        if let Some(ssh_key) = identity.ssh_key.as_deref()
            && let Ok(escaped) = escape_clone_ssh_key_path(ssh_key)
        {
            let ssh_command = format!("ssh -i {escaped} -o IdentitiesOnly=yes");
            let _ = tokio::process::Command::new(git_binary())
                .args(["config", "--local", "core.sshCommand", &ssh_command])
                .current_dir(&resolved_destination)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await;
            let _ = tokio::process::Command::new(git_binary())
                .args(["config", "--local", "--unset", "credential.helper"])
                .current_dir(&resolved_destination)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await;
        }
    }

    json_response(
        StatusCode::OK,
        json!({
            "success": true,
            "path": path_string(&resolved_destination),
            "output": combined,
        }),
    )
}

/// 统一封装 clone 路径的 IO 错误：记录日志并返回 500。
fn clone_error(error: &std::io::Error) -> Response {
    tracing::error!("Failed to clone repository: {error}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
