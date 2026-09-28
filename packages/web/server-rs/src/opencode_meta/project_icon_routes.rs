//! Port of `server/lib/opencode/project-icon-routes.js` — project icon
//! upload/read/discovery with the JS's storage layout
//! (`<dataDir>/project-icons/project-<sha1(id)>.<ext>`), MIME allowlist,
//! 5 MB limit, SVG theme injection, and favicon discovery through the
//! fuzzy filesystem search runtime (`fs_routes::search`).
//!
//! JS route map:
//! - `GET    /api/projects/:projectId/icon` — serve (theme-aware for SVG)
//! - `PUT    /api/projects/:projectId/icon` — upload from `{ dataUrl }`
//! - `DELETE /api/projects/:projectId/icon` — remove
//! - `POST   /api/projects/:projectId/icon/discover` — favicon adoption
//!
//! 中文说明：本模块是 `server/lib/opencode/project-icon-routes.js` 的 Rust
//! 移植，实现项目图标的读取（GET）、上传（PUT）、删除（DELETE）与 favicon
//! 自动发现（POST discover）。存储布局、MIME 白名单、5 MB 上限、SVG 主题
//! 注入与“最短 favicon 路径优先”等行为均与 JS 版逐点对齐；文件搜索复用
//! `fs_routes::search` 的模糊搜索运行时。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value, json};

use super::{MetaState, now_ms, parse_json_body, parse_query};
use crate::fs_routes::search::{SearchHit, SearchOptions, search_filesystem_files};
use crate::settings::normalization::sanitize_projects;

/// MIME → 扩展名白名单映射，同时定义了候选图标的遍历顺序。
const MIME_TO_EXTENSION: [(&str, &str); 5] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/svg+xml", "svg"),
    ("image/webp", "webp"),
    ("image/x-icon", "ico"),
];

/// 图标字节大小上限：5 MB，与 JS 版一致。
const PROJECT_ICON_MAX_BYTES: usize = 5 * 1024 * 1024;
/// 主题 → 默认图标颜色：light 为深灰 #111111，dark 为浅灰 #f5f5f5。
const PROJECT_ICON_THEME_COLORS: [(&str, &str); 2] = [("light", "#111111"), ("dark", "#f5f5f5")];

/// 挂载图标四条路由：GET/PUT/DELETE `/icon` 与 POST `/icon/discover`。
pub(crate) fn routes(state: Arc<MetaState>) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/projects/{projectId}/icon",
            get(get_icon).put(put_icon).delete(delete_icon),
        )
        .route(
            "/api/projects/{projectId}/icon/discover",
            post(discover_icon),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Helpers (JS module-scope closures)
// ---------------------------------------------------------------------------

/// 图标存储目录：`<dataDir>/project-icons`。
fn icons_dir(state: &MetaState) -> PathBuf {
    state.data_dir.join("project-icons")
}

/// 归一化 MIME（对应 JS `normalizeProjectIconMime`）：`image/jpg` 折叠为 `image/jpeg`，仅接受白名单五种图片类型。
/// JS `normalizeProjectIconMime` — `image/jpg` folds to `image/jpeg`.
fn normalize_project_icon_mime(value: &str) -> Option<&'static str> {
    let normalized = value.trim().to_lowercase();
    if normalized == "image/jpg" {
        return Some("image/jpeg");
    }
    match normalized.as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/svg+xml" => Some("image/svg+xml"),
        "image/webp" => Some("image/webp"),
        "image/x-icon" => Some("image/x-icon"),
        _ => None,
    }
}

/// 反向查表：扩展名 → MIME；未知扩展名返回 None。
fn extension_to_mime(extension: &str) -> Option<&'static str> {
    MIME_TO_EXTENSION
        .iter()
        .find(|(_, ext)| *ext == extension)
        .map(|(mime, _)| *mime)
}

/// 由归一化 MIME 推出图标文件具体路径（对应 JS `projectIconPathForMime`）；MIME 不在白名单返回 None。
/// JS `projectIconPathForMime` — normalized MIME → concrete path.
fn project_icon_path_for_mime(state: &MetaState, project_id: &str, mime: &str) -> Option<PathBuf> {
    let normalized = normalize_project_icon_mime(mime)?;
    let extension = MIME_TO_EXTENSION
        .iter()
        .find(|(known, _)| *known == normalized)
        .map(|(_, ext)| *ext)?;
    Some(icons_dir(state).join(format!(
        "{}.{}",
        project_icon_base_name(project_id),
        extension
    )))
}

/// 图标文件基础名：`project-<sha1(projectId)>`（SHA-1 由 `walkthrough::sha1` 提供与 Node 一致的 hex 摘要）。
/// JS `projectIconBaseName`: `project-<sha1(projectId)>` (`walkthrough::sha1`
/// provides the Node-compatible SHA-1 hex digest — the `sha1` crate is not a
/// direct dependency of this crate).
fn project_icon_base_name(project_id: &str) -> String {
    format!(
        "project-{}",
        crate::walkthrough::sha1::sha1_hex(project_id.as_bytes())
    )
}

/// 枚举该项目所有可能的图标路径（对应 JS `projectIconPathCandidates`），按白名单扩展名顺序排列。
/// JS `projectIconPathCandidates` — every supported extension, in order.
fn project_icon_path_candidates(state: &MetaState, project_id: &str) -> Vec<PathBuf> {
    let base = project_icon_base_name(project_id);
    MIME_TO_EXTENSION
        .iter()
        .map(|(_, ext)| icons_dir(state).join(format!("{base}.{ext}")))
        .collect()
}

/// 删除除 `keep` 外的全部候选图标文件（对应 JS `removeProjectIconFiles`）；文件不存在视为成功，其它错误向上传播。
/// JS `removeProjectIconFiles` — unlink every candidate except `keep`;
/// missing files (ENOENT) are fine, other failures propagate.
async fn remove_project_icon_files(
    state: &MetaState,
    project_id: &str,
    keep: Option<&Path>,
) -> std::io::Result<()> {
    for candidate in project_icon_path_candidates(state, project_id) {
        if keep == Some(candidate.as_path()) {
            continue;
        }
        if let Err(error) = tokio::fs::remove_file(candidate).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error);
        }
    }
    Ok(())
}

/// 解析 `{ dataUrl }`（对应 JS `parseProjectIconDataUrl`）：校验
/// `data:<mime>;base64,<payload>` 格式与字符集、MIME 限 PNG/JPEG/SVG、
/// 解码后非空且 ≤ 5 MB；各失败分支返回与 JS 逐字一致的错误文案。
/// JS `parseProjectIconDataUrl`: `/^data:([^;,]+);base64,([A-Za-z0-9+/=\s]+)$/i`.
fn parse_project_icon_data_url(value: Option<&Value>) -> Result<(String, Vec<u8>), &'static str> {
    use base64::engine::Engine as _;

    let Some(raw) = value.and_then(Value::as_str) else {
        return Err("dataUrl is required");
    };
    let trimmed = raw.trim();
    if trimmed.len() < 5 || !trimmed[..5].eq_ignore_ascii_case("data:") {
        return Err("Invalid dataUrl format");
    }
    let rest = &trimmed[5..];
    let Some(mime_end) = rest.find(|c| c == ';' || c == ',') else {
        return Err("Invalid dataUrl format");
    };
    let mime_raw = &rest[..mime_end];
    if mime_raw.is_empty() {
        return Err("Invalid dataUrl format");
    }
    let after = &rest[mime_end..];
    if after.len() < 8 || !after[..8].eq_ignore_ascii_case(";base64,") {
        return Err("Invalid dataUrl format");
    }
    let payload = &after[8..];
    let is_whitespace = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}');
    let valid_payload_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=') || is_whitespace(c);
    if payload.is_empty() || !payload.chars().all(valid_payload_char) {
        return Err("Invalid dataUrl format");
    }

    let mime = normalize_project_icon_mime(mime_raw)
        .filter(|mime| *mime == "image/png" || *mime == "image/jpeg" || *mime == "image/svg+xml");
    let Some(mime) = mime else {
        return Err("Icon must be PNG, JPEG, or SVG");
    };

    let base64_text: String = payload.chars().filter(|c| !is_whitespace(*c)).collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    // Node's `Buffer.from(s, 'base64')` is lenient about degenerate tails:
    // dangling padding and a lone trailing character decode to zero bytes
    // (their bits are dropped) instead of failing. Mirror that so padding-
    // only payloads reach the "Icon content is empty" branch like the JS.
    let mut node_lenient = base64_text.trim_end_matches('=').to_string();
    if node_lenient.len() % 4 == 1 {
        node_lenient.pop();
    }
    let bytes = engine
        .decode(node_lenient.as_bytes())
        .map_err(|_| "Failed to decode icon data")?;
    if bytes.is_empty() {
        return Err("Icon content is empty");
    }
    if bytes.len() > PROJECT_ICON_MAX_BYTES {
        return Err("Icon exceeds size limit (5 MB)");
    }
    Ok((mime.to_string(), bytes))
}

/// 归一化主题变体（对应 JS `normalizeProjectIconThemeVariant`）：仅接受 light/dark（忽略大小写与空白）。
/// JS `normalizeProjectIconThemeVariant`.
fn normalize_project_icon_theme_variant(value: Option<&str>) -> Option<&'static str> {
    let normalized = value?.trim().to_lowercase();
    match normalized.as_str() {
        "light" => Some("light"),
        "dark" => Some("dark"),
        _ => None,
    }
}

/// 校验十六进制颜色（对应 JS `projectIconHexColorPattern`）：# 开头的 3/4/6/8 位 hex。
/// JS `projectIconHexColorPattern`:
/// `/^#(?:[\da-fA-F]{3}|[\da-fA-F]{4}|[\da-fA-F]{6}|[\da-fA-F]{8})$/`.
fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'#') {
        return false;
    }
    let rest = &bytes[1..];
    (rest.len() == 3 || rest.len() == 4 || rest.len() == 6 || rest.len() == 8)
        && rest.iter().all(|b| b.is_ascii_hexdigit())
}

/// 归一化显式颜色参数（对应 JS `normalizeProjectIconColor`）：去空白后必须是合法 hex 颜色，否则视为未提供。
/// JS `normalizeProjectIconColor`.
fn normalize_project_icon_color(value: Option<&str>) -> Option<String> {
    let normalized = value?.trim();
    if !is_hex_color(normalized) {
        return None;
    }
    Some(normalized.to_string())
}

/// 判断字节是否为单词字符（字母/数字/下划线），用于 `<svg` 标签名的边界匹配（等价 JS 正则的 \b）。
fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// SVG 主题注入（对应 JS `applyProjectIconSvgTheme`）：在首个 `<svg>`
/// 开标签后插入覆盖 `:root` 颜色的 `<style>`；显式 color 优先于主题默认
/// 色，两者皆无则原样返回。
/// JS `applyProjectIconSvgTheme` — inject `<style>` right after the opening
/// `<svg>` tag with the theme's (or explicit) color.
pub(crate) fn apply_project_icon_svg_theme(
    svg_markup: &str,
    theme_variant: Option<&str>,
    icon_color: Option<&str>,
) -> String {
    let color = icon_color.map(str::to_string).or_else(|| {
        theme_variant.and_then(|variant| {
            PROJECT_ICON_THEME_COLORS
                .iter()
                .find(|(name, _)| *name == variant)
                .map(|(_, color)| color.to_string())
        })
    });
    let Some(color) = color else {
        return svg_markup.to_string();
    };

    let bytes = svg_markup.as_bytes();
    let mut svg_tag_index = None;
    let mut index = 0;
    while index + 4 <= bytes.len() {
        if bytes[index] == b'<'
            && svg_markup[index + 1..].len() >= 3
            && svg_markup[index + 1..index + 4].eq_ignore_ascii_case("svg")
        {
            let after = index + 4;
            if after >= bytes.len() || !is_word_char(bytes[after]) {
                svg_tag_index = Some(index);
                break;
            }
        }
        index += 1;
    }
    let Some(svg_tag_index) = svg_tag_index else {
        return svg_markup.to_string();
    };
    let Some(offset) = svg_markup[svg_tag_index..].find('>') else {
        return svg_markup.to_string();
    };
    let svg_open_tag_end = svg_tag_index + offset;

    let override_style = format!(
        "<style data-ompchamber-theme-icon=\"1\">:root{{color:{color}!important;}}</style>"
    );
    format!(
        "{}{}{}",
        &svg_markup[..=svg_open_tag_end],
        override_style,
        &svg_markup[svg_open_tag_end + 1..]
    )
}

/// 读取并净化 settings 中的项目列表（JS `findProjectById` 的数据来源）；缺失或非法时返回空列表。
/// JS `findProjectById` over `sanitizeProjects(settings.projects) || []`.
fn sanitized_projects(settings: &Map<String, Value>) -> Vec<Value> {
    sanitize_projects(settings.get("projects")).unwrap_or_default()
}

/// 在项目列表中按 id 查找项目；找不到返回 None。
fn find_project<'a>(projects: &'a [Value], project_id: &str) -> Option<&'a Value> {
    projects
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(project_id))
}

/// 取路径最后一段的扩展名并转小写（对应 JS `path.extname(p).slice(1).toLowerCase()`）。
/// `path.extname(p).slice(1).toLowerCase()` for icon file names.
fn lowercase_extension(path: &str) -> String {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match file_name.rsplit_once('.') {
        Some((_, extension)) => extension.to_lowercase(),
        None => String::new(),
    }
}

/// 判断是否 favicon 文件（对应 JS 正则 `/(^|\/)favicon\.(ico|png|svg|jpg|jpeg|webp)$/i`）。
/// JS `/(^|\/)favicon\.(ico|png|svg|jpg|jpeg|webp)$/i`.
fn is_favicon_path(path: &str) -> bool {
    // favicon 允许的扩展名集合（含 jpeg：能被匹配到，但无 MIME 映射 → 415）。
    const EXTENSIONS: [&str; 6] = ["ico", "png", "svg", "jpg", "jpeg", "webp"];
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let Some((stem, extension)) = file_name.split_once('.') else {
        return false;
    };
    if !stem.eq_ignore_ascii_case("favicon") {
        return false;
    }
    EXTENSIONS.contains(&extension.to_lowercase().as_str())
}

/// 构造统一 JSON 错误响应：`{ "error": message }` + 给定状态码。
fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// 把项目的新 `iconImage` 写回持久化 settings（上传传对象、删除传 null），
/// 返回各图标路由共享的 `{ project, settings }` 响应体；持久化失败映射为
/// Err(()) 由调用方转 500。
/// Replace (or clear) the project's `iconImage` in the persisted settings
/// and return the shared `{ project, settings }` response body.
async fn persist_icon_update(
    state: &MetaState,
    projects: &[Value],
    project_id: &str,
    icon_image: Value,
) -> Result<Value, ()> {
    let next_projects: Vec<Value> = projects
        .iter()
        .map(|entry| {
            let mut updated = entry.clone();
            if updated.get("id").and_then(Value::as_str) == Some(project_id)
                && let Some(map) = updated.as_object_mut()
            {
                map.insert("iconImage".to_string(), icon_image.clone());
            }
            updated
        })
        .collect();
    let updated_settings = state
        .settings
        .persist(&json!({ "projects": next_projects }))
        .await
        .map_err(|_| ())?;
    let updated_project = updated_settings
        .get("projects")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(project_id))
                .cloned()
        })
        .unwrap_or(Value::Null);
    Ok(json!({
        "project": updated_project,
        "settings": updated_settings,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/projects/:projectId/icon
// ---------------------------------------------------------------------------

/// `GET /icon`：校验项目存在后，按“元数据 MIME 首选、其余扩展名候选”
/// 顺序读取图标；SVG 可经 `?theme=`/`?iconColor=` 注入主题色；响应携带
/// 一年 immutable 的 Cache-Control。候选文件缺失则试下一个，全缺失 404。
async fn get_icon(
    State(state): State<Arc<MetaState>>,
    AxumPath(project_id): AxumPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "projectId is required");
    }

    let settings = match state.settings.read_migrated().await {
        Ok(settings) => settings,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load project icon",
            );
        }
    };
    let projects = sanitized_projects(&settings);
    let Some(project) = find_project(&projects, project_id) else {
        return json_error(StatusCode::NOT_FOUND, "Project not found");
    };

    let metadata_mime = project
        .get("iconImage")
        .and_then(|icon| icon.get("mime"))
        .and_then(Value::as_str)
        .and_then(normalize_project_icon_mime);
    let preferred_path =
        metadata_mime.and_then(|mime| project_icon_path_for_mime(&state, project_id, mime));
    let candidates = match &preferred_path {
        Some(preferred) => {
            let mut all = vec![preferred.clone()];
            all.extend(
                project_icon_path_candidates(&state, project_id)
                    .into_iter()
                    .filter(|candidate| candidate != preferred),
            );
            all
        }
        None => project_icon_path_candidates(&state, project_id),
    };

    let query = parse_query(query.as_deref());
    let requested_theme_variant = normalize_project_icon_theme_variant(query.first("theme"));
    let requested_icon_color = normalize_project_icon_color(query.first("iconColor"));

    for icon_path in candidates {
        match tokio::fs::read(&icon_path).await {
            Ok(data) => {
                let extension = lowercase_extension(&icon_path.to_string_lossy());
                let resolved_mime = if Some(icon_path.as_path()) == preferred_path.as_deref()
                    && let Some(metadata_mime) = metadata_mime
                {
                    metadata_mime
                } else {
                    extension_to_mime(&extension).unwrap_or("application/octet-stream")
                };
                let content_type = if resolved_mime == "image/svg+xml" {
                    "image/svg+xml; charset=utf-8"
                } else {
                    resolved_mime
                };
                let cache_control = "public, max-age=31536000, immutable";

                if resolved_mime == "image/svg+xml"
                    && (requested_theme_variant.is_some() || requested_icon_color.is_some())
                {
                    let svg_markup = String::from_utf8_lossy(&data).into_owned();
                    let themed = apply_project_icon_svg_theme(
                        &svg_markup,
                        requested_theme_variant,
                        requested_icon_color.as_deref(),
                    );
                    return (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, content_type),
                            (header::CACHE_CONTROL, cache_control),
                        ],
                        themed,
                    )
                        .into_response();
                }

                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, content_type),
                        (header::CACHE_CONTROL, cache_control),
                    ],
                    Body::from(data),
                )
                    .into_response();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to read project icon",
                );
            }
        }
    }

    json_error(StatusCode::NOT_FOUND, "Project icon not found")
}

// ---------------------------------------------------------------------------
// PUT /api/projects/:projectId/icon
// ---------------------------------------------------------------------------

/// `PUT /icon`：解析 body 的 `dataUrl` 并校验，通过后落盘、清理旧扩展名并
/// 持久化 iconImage（source=custom）；校验失败 400、项目不存在 404、IO 失败 500。
async fn put_icon(
    State(state): State<Arc<MetaState>>,
    AxumPath(project_id): AxumPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "projectId is required");
    }

    let body = match parse_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let parsed = match parse_project_icon_data_url(body.as_ref().and_then(|b| b.get("dataUrl"))) {
        Ok(parsed) => parsed,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };
    let (mime, bytes) = parsed;

    let (projects, icon_image) = match put_icon_flow(&state, project_id, &mime, &bytes).await {
        Ok(Ok(inner)) => inner,
        Ok(Err(response)) => return response,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to upload project icon",
            );
        }
    };

    match persist_icon_update(&state, &projects, project_id, icon_image).await {
        Ok(payload) => Json(payload).into_response(),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to upload project icon",
        ),
    }
}

/// PUT 主体流程：确认项目存在后把字节写入 MIME 对应路径并删除其它扩展名
/// 文件，返回 (项目列表, 新 iconImage 元数据) 供持久化。
async fn put_icon_flow(
    state: &MetaState,
    project_id: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<Result<(Vec<Value>, Value), Response>, std::io::Error> {
    let settings = state.settings.read_migrated().await.map_err(io_error)?;
    let projects = sanitized_projects(&settings);
    if find_project(&projects, project_id).is_none() {
        return Ok(Err(json_error(StatusCode::NOT_FOUND, "Project not found")));
    }
    let Some(icon_path) = project_icon_path_for_mime(state, project_id, mime) else {
        return Ok(Err(json_error(
            StatusCode::BAD_REQUEST,
            "Unsupported icon format",
        )));
    };
    tokio::fs::create_dir_all(icons_dir(state)).await?;
    tokio::fs::write(&icon_path, bytes).await?;
    remove_project_icon_files(state, project_id, Some(&icon_path)).await?;
    Ok(Ok((
        projects,
        json!({ "mime": mime, "updatedAt": now_ms(), "source": "custom" }),
    )))
}

/// 把 settings 读取错误转换为 io::Error，让 JS 的兜底 500 分支保持单一出口。
/// Settings-read failures surface as io errors so the JS catch-all 500
/// mapping stays one branch.
fn io_error(error: crate::error::AppError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

// ---------------------------------------------------------------------------
// DELETE /api/projects/:projectId/icon
// ---------------------------------------------------------------------------

/// `DELETE /icon`：删除全部图标文件并把 iconImage 置为 null 持久化，返回 `{ project, settings }`。
async fn delete_icon(
    State(state): State<Arc<MetaState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Response {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "projectId is required");
    }

    let projects = match delete_icon_flow(&state, project_id).await {
        Ok(Ok(projects)) => projects,
        Ok(Err(response)) => return response,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to remove project icon",
            );
        }
    };

    match persist_icon_update(&state, &projects, project_id, Value::Null).await {
        Ok(payload) => Json(payload).into_response(),
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to remove project icon",
        ),
    }
}

/// DELETE 主体流程：确认项目存在后删除所有候选图标文件，返回项目列表供持久化。
async fn delete_icon_flow(
    state: &MetaState,
    project_id: &str,
) -> Result<Result<Vec<Value>, Response>, std::io::Error> {
    let settings = state.settings.read_migrated().await.map_err(io_error)?;
    let projects = sanitized_projects(&settings);
    if find_project(&projects, project_id).is_none() {
        return Ok(Err(json_error(StatusCode::NOT_FOUND, "Project not found")));
    }
    remove_project_icon_files(state, project_id, None).await?;
    Ok(Ok(projects))
}

// ---------------------------------------------------------------------------
// POST /api/projects/:projectId/icon/discover
// ---------------------------------------------------------------------------

/// `POST /icon/discover`：在项目目录模糊搜索 favicon 并收养路径最短者
/// （source=auto）；已有 custom 图标时跳过（除非 `force: true`）；无
/// favicon 404、扩展名无 MIME 映射 415。
async fn discover_icon(
    State(state): State<Arc<MetaState>>,
    AxumPath(project_id): AxumPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "projectId is required");
    }

    let body = match parse_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let force = body.as_ref().and_then(|b| b.get("force")) == Some(&Value::Bool(true));

    let settings = match state.settings.read_migrated().await {
        Ok(settings) => settings,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to discover project icon",
            );
        }
    };
    let projects = sanitized_projects(&settings);
    let Some(project) = find_project(&projects, project_id) else {
        return json_error(StatusCode::NOT_FOUND, "Project not found");
    };

    if project
        .get("iconImage")
        .and_then(|icon| icon.get("source"))
        .and_then(Value::as_str)
        == Some("custom")
        && !force
    {
        return Json(json!({
            "project": project.clone(),
            "skipped": true,
            "reason": "custom-icon-present",
        }))
        .into_response();
    }

    let project_path = project
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let favicon_candidates = search_filesystem_files(
        Path::new(&project_path),
        &SearchOptions {
            limit: 200,
            query: "favicon".to_string(),
            include_hidden: true,
            respect_gitignore: false,
        },
    )
    .await;

    let mut filtered: Vec<&SearchHit> = favicon_candidates
        .iter()
        .filter(|entry| is_favicon_path(&entry.path))
        .collect();
    // JS `.sort((a, b) => a.path.length - b.path.length)` — stable.
    filtered.sort_by_key(|entry| entry.path.len());
    let Some(selected) = filtered.first() else {
        return json_error(StatusCode::NOT_FOUND, "No favicon found in project");
    };

    let extension = lowercase_extension(&selected.path);
    let Some(mime) = extension_to_mime(&extension) else {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported favicon format",
        );
    };

    let icon_image = match discover_icon_flow(&state, project_id, mime, &selected.path).await {
        Ok(Ok(icon_image)) => icon_image,
        Ok(Err(response)) => return response,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to discover project icon",
            );
        }
    };

    let discovered_path = selected.path.clone();
    match persist_icon_update(&state, &projects, project_id, icon_image).await {
        Ok(mut payload) => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("discoveredPath".to_string(), Value::from(discovered_path));
            }
            Json(payload).into_response()
        }
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to discover project icon",
        ),
    }
}

/// discover 主体流程：读取源 favicon、校验非空且 ≤ 5 MB，复制到项目图标
/// 路径并清理其它扩展名，返回 `source: "auto"` 元数据。
async fn discover_icon_flow(
    state: &MetaState,
    project_id: &str,
    mime: &str,
    source_path: &str,
) -> Result<Result<Value, Response>, std::io::Error> {
    let bytes = tokio::fs::read(source_path).await?;
    if bytes.is_empty() {
        return Ok(Err(json_error(
            StatusCode::BAD_REQUEST,
            "Discovered icon is empty",
        )));
    }
    if bytes.len() > PROJECT_ICON_MAX_BYTES {
        return Ok(Err(json_error(
            StatusCode::BAD_REQUEST,
            "Discovered icon exceeds size limit (5 MB)",
        )));
    }
    let Some(icon_path) = project_icon_path_for_mime(state, project_id, mime) else {
        return Ok(Err(json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported favicon format",
        )));
    };
    tokio::fs::create_dir_all(icons_dir(state)).await?;
    tokio::fs::write(&icon_path, &bytes).await?;
    remove_project_icon_files(state, project_id, Some(&icon_path)).await?;
    Ok(Ok(
        json!({ "mime": mime, "updatedAt": now_ms(), "source": "auto" }),
    ))
}
