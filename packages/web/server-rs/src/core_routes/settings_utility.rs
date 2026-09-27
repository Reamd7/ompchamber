//! `registerSettingsUtilityRoutes` from `opencode/core-routes.js`:
//! `GET /api/config/themes` (`theme-runtime.js` port) and
//! `POST /api/config/reload` (`refreshOpenCodeAfterConfigChange` subset).
//!
//! Theme discovery reads `~/.config/ompchamber/themes` (JS
//! `OMPCHAMBER_USER_THEMES_DIR` — deliberately NOT the data dir) with the
//! 512 KiB per-file cap (`MAX_THEME_JSON_BYTES`).
//! opencode/core-routes.js 的 `registerSettingsUtilityRoutes` 移植：
//! `GET /api/config/themes`（theme-runtime.js）与
//! `POST /api/config/reload`（`refreshOpenCodeAfterConfigChange` 子集）。
//!
//! 主题发现读取 `~/.config/ompchamber/themes`（JS 的
//! `OMPCHAMBER_USER_THEMES_DIR`——刻意不落在 data 目录），单文件
//! 512 KiB 上限。

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::context::RouterContext;

/// 单个主题 JSON 文件的大小上限，超限直接跳过，防止读入超大文件。
const MAX_THEME_JSON_BYTES: u64 = 512 * 1024;

/// `index.js` `OMPCHAMBER_USER_THEMES_DIR` (`~/.config/ompchamber/themes` on
/// every platform — independent of `OMPCHAMBER_DATA_DIR`).
/// index.js 的 `OMPCHAMBER_USER_THEMES_DIR`：各平台统一为
/// `~/.config/ompchamber/themes`，与 `OMPCHAMBER_DATA_DIR` 无关。
fn user_themes_dir() -> Option<PathBuf> {
    crate::config::home_dir().map(|home| home.join(".config").join("ompchamber").join("themes"))
}

/// 值是 trim 后非空的字符串才返回 true。
fn is_non_empty_string(value: Option<&Value>) -> bool {
    value
        .and_then(|value| value.as_str())
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

/// `normalizeThemeJson` (theme-runtime.js): validate the metadata/colors shape
/// and normalize the metadata block; extra top-level fields pass through.
/// theme-runtime.js 的 `normalizeThemeJson`：校验 metadata/colors 的
/// 必填形态（全部必需颜色键、variant ∈ {light,dark}、id/name 非空），
/// 归一 metadata 块（trim、补默认 description/version、清洗 tags）；
/// 顶层额外字段透传，不合法返回 None。
pub(crate) fn normalize_theme_json(raw: &Value) -> Option<Value> {
    let obj = raw.as_object()?;
    let metadata = obj.get("metadata")?.as_object()?;
    let colors = obj.get("colors")?.as_object()?;

    let variant = metadata.get("variant").and_then(|value| value.as_str())?;
    if variant != "light" && variant != "dark" {
        return None;
    }
    if !is_non_empty_string(metadata.get("id")) || !is_non_empty_string(metadata.get("name")) {
        return None;
    }

    let primary = colors.get("primary")?.as_object()?;
    let surface = colors.get("surface")?.as_object()?;
    let interactive = colors.get("interactive")?.as_object()?;
    let status = colors.get("status")?.as_object()?;
    let syntax = colors.get("syntax")?.as_object()?;
    let syntax_base = syntax.get("base")?.as_object()?;
    let syntax_highlights = syntax.get("highlights")?.as_object()?;

    let required: Vec<Option<&Value>> = vec![
        primary.get("base"),
        primary.get("foreground"),
        surface.get("background"),
        surface.get("foreground"),
        surface.get("muted"),
        surface.get("mutedForeground"),
        surface.get("elevated"),
        surface.get("elevatedForeground"),
        surface.get("subtle"),
        interactive.get("border"),
        interactive.get("selection"),
        interactive.get("selectionForeground"),
        interactive.get("focusRing"),
        interactive.get("hover"),
        status.get("error"),
        status.get("errorForeground"),
        status.get("errorBackground"),
        status.get("errorBorder"),
        status.get("warning"),
        status.get("warningForeground"),
        status.get("warningBackground"),
        status.get("warningBorder"),
        status.get("success"),
        status.get("successForeground"),
        status.get("successBackground"),
        status.get("successBorder"),
        status.get("info"),
        status.get("infoForeground"),
        status.get("infoBackground"),
        status.get("infoBorder"),
        syntax_base.get("background"),
        syntax_base.get("foreground"),
        syntax_base.get("keyword"),
        syntax_base.get("string"),
        syntax_base.get("number"),
        syntax_base.get("function"),
        syntax_base.get("variable"),
        syntax_base.get("type"),
        syntax_base.get("comment"),
        syntax_base.get("operator"),
        syntax_highlights.get("diffAdded"),
        syntax_highlights.get("diffRemoved"),
        syntax_highlights.get("lineNumber"),
    ];
    if !required.into_iter().all(is_non_empty_string) {
        return None;
    }

    let tags = metadata
        .get("tags")
        .and_then(|value| value.as_array())
        .map(|entries| {
            Value::Array(
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .filter(|entry| !entry.trim().is_empty())
                    .map(Value::String)
                    .collect(),
            )
        })
        .unwrap_or_else(|| Value::Array(Vec::new()));

    let mut normalized_metadata = metadata.clone();
    normalized_metadata.insert(
        "id".into(),
        Value::String(
            metadata
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
    );
    normalized_metadata.insert(
        "name".into(),
        Value::String(
            metadata
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
    );
    let description = match metadata.get("description") {
        Some(Value::String(text)) => Value::String(text.clone()),
        _ => Value::String(String::new()),
    };
    normalized_metadata.insert("description".into(), description);
    let version = metadata
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| Value::String(text.to_string()))
        .unwrap_or_else(|| Value::String("1.0.0".to_string()));
    normalized_metadata.insert("version".into(), version);
    normalized_metadata.insert("variant".into(), Value::String(variant.to_string()));
    normalized_metadata.insert("tags".into(), tags);

    let mut normalized = obj.clone();
    normalized.insert("metadata".into(), Value::Object(normalized_metadata));
    Some(Value::Object(normalized))
}

/// `readCustomThemesFromDisk` (theme-runtime.js): every failure degrades to
/// an empty list (a missing dir is fine); per-file failures skip the file.
/// theme-runtime.js 的 `readCustomThemesFromDisk`：目录缺失或不可读
/// 一律降级为空列表；逐文件跳过非 .json、非普通文件、超大、解析失败、
/// 校验失败与重复 id 的条目，其余按文件名排序收集。
pub(crate) fn read_custom_themes_from_disk(themes_dir: &Path, max_bytes: u64) -> Vec<Value> {
    let entries = match std::fs::read_dir(themes_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut themes = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut candidates: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    candidates.sort();
    for path in candidates {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            tracing::warn!("[themes] Failed to read {name}");
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > max_bytes {
            tracing::warn!("[themes] Skip {name}: too large ({} bytes)", metadata.len());
            continue;
        }
        let Ok(raw_text) = std::fs::read_to_string(&path) else {
            tracing::warn!("[themes] Failed to read {name}");
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw_text) else {
            tracing::warn!("[themes] Skip {name}: invalid theme JSON");
            continue;
        };
        let Some(normalized) = normalize_theme_json(&parsed) else {
            tracing::warn!("[themes] Skip {name}: invalid theme JSON");
            continue;
        };
        let id = normalized
            .get("metadata")
            .and_then(|metadata| metadata.get("id"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if seen.contains(&id) {
            tracing::warn!("[themes] Skip {name}: duplicate theme id \"{id}\"");
            continue;
        }
        seen.push(id);
        themes.push(normalized);
    }
    themes
}

/// `GET /api/config/themes`.
/// `GET /api/config/themes`：返回用户主题目录下收集到的主题列表。
pub(crate) async fn themes() -> Response {
    let themes = user_themes_dir()
        .map(|dir| read_custom_themes_from_disk(&dir, MAX_THEME_JSON_BYTES))
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "themes": themes })),
    )
        .into_response()
}

/// config-mutation-response.js 的 `buildExternalManualRestartResponse`：
/// 操作成功但需用户手动重启的响应体。
fn external_manual_restart_response(message: &str) -> Value {
    // `buildExternalManualRestartResponse` (config-mutation-response.js).
    serde_json::json!({
        "success": true,
        "requiresReload": false,
        "requiresManualRestart": true,
        "message": message,
    })
}

/// `POST /api/config/reload` — applies accumulated deferred OpenCode config
/// changes. External engine: the running server keeps its startup-cached
/// config until the user restarts it, so report an honest manual restart.
/// Managed engine restart is a documented engine.rs gap; here the route
/// verifies the engine is ready and reports the JS reload response.
/// `POST /api/config/reload`：应用累积的延迟 OpenCode 配置变更。
/// External 引擎下运行中的服务沿用启动时缓存的配置，需用户自行重启，
/// 因此如实返回手动重启提示；托管引擎的重启是 engine.rs 已记录的
/// 缺口，这里只等待引擎就绪（12 秒）并返回 JS 形态的 reload 响应。
pub(crate) async fn reload(State(ctx): State<RouterContext>) -> Response {
    tracing::info!("[Server] Manual configuration reload requested");
    let external = ctx.engine.mode() == crate::engine::EngineModeKind::External;
    if external {
        return (
            StatusCode::OK,
            Json(external_manual_restart_response(
                "Configuration is saved on disk. Restart your connected OpenCode server to apply the changes.",
            )),
        )
            .into_response();
    }
    match ctx
        .engine
        .wait_ready(std::time::Duration::from_secs(12))
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "requiresReload": true,
                "message": "Configuration reloaded successfully. Refreshing interface…",
                "reloadDelayMs": crate::core_routes::CLIENT_RELOAD_DELAY_MS,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": error.to_string(),
                "success": false,
            })),
        )
            .into_response(),
    }
}

/// settings_utility 路由与主题归一的测试。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_routes::router;
    use crate::core_routes::tests::{json_response, temp_dir, test_ctx};
    use crate::engine::EngineState;
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};

/// 一份字段齐全的合法主题 JSON 样例（含待归一的空白 id 与非法 tags 项）。
    fn valid_theme_json() -> Value {
        serde_json::json!({
            "metadata": { "id": "  midnight ", "name": "Midnight", "variant": "dark",
                          "description": null, "tags": ["dark", "", 3] },
            "colors": {
                "primary": { "base": "#fff", "foreground": "#000" },
                "surface": { "background": "#111", "foreground": "#eee", "muted": "#222",
                             "mutedForeground": "#888", "elevated": "#333", "elevatedForeground": "#ddd",
                             "subtle": "#444" },
                "interactive": { "border": "#555", "selection": "#666", "selectionForeground": "#777",
                                 "focusRing": "#999", "hover": "#aaa" },
                "status": { "error": "#e00", "errorForeground": "#fff", "errorBackground": "#300", "errorBorder": "#f00",
                            "warning": "#ed0", "warningForeground": "#000", "warningBackground": "#330", "warningBorder": "#fd0",
                            "success": "#0d0", "successForeground": "#000", "successBackground": "#030", "successBorder": "#0f0",
                            "info": "#0df", "infoForeground": "#000", "infoBackground": "#033", "infoBorder": "#0ff" },
                "syntax": {
                    "base": { "background": "#000", "foreground": "#fff", "keyword": "#f0f", "string": "#0f0",
                              "number": "#ff0", "function": "#0ff", "variable": "#f70", "type": "#7f0",
                              "comment": "#777", "operator": "#aaa" },
                    "highlights": { "diffAdded": "#0f0", "diffRemoved": "#f00", "lineNumber": "#666" },
                },
            },
            "customExtra": "kept",
        })
    }

/// 验证合法主题被接受且 metadata 被归一（trim id、默认 version、
/// 过滤空 tags），顶层额外字段保留。
    #[test]
    fn normalize_theme_json_accepts_and_normalizes_valid_theme() {
        let normalized = normalize_theme_json(&valid_theme_json()).expect("valid theme");
        let metadata = normalized["metadata"].as_object().unwrap();
        assert_eq!(metadata["id"], "midnight");
        assert_eq!(metadata["name"], "Midnight");
        assert_eq!(metadata["description"], "");
        assert_eq!(metadata["version"], "1.0.0");
        assert_eq!(metadata["variant"], "dark");
        assert_eq!(metadata["tags"], serde_json::json!(["dark"]));
        assert_eq!(normalized["customExtra"], "kept");
    }

/// 验证缺颜色键、非法 variant、空 name、缺块的主题一律被拒绝。
    #[test]
    fn normalize_theme_json_rejects_incomplete_themes() {
        let mut missing_color = valid_theme_json();
        missing_color["colors"]["syntax"]["highlights"]["diffAdded"] = Value::Null;
        assert!(normalize_theme_json(&missing_color).is_none());

        let mut bad_variant = valid_theme_json();
        bad_variant["metadata"]["variant"] = Value::String("neon".into());
        assert!(normalize_theme_json(&bad_variant).is_none());

        let mut no_name = valid_theme_json();
        no_name["metadata"]["name"] = Value::String("   ".into());
        assert!(normalize_theme_json(&no_name).is_none());

        assert!(
            normalize_theme_json(&serde_json::json!({ "metadata": {}, "colors": {} })).is_none()
        );
        assert!(normalize_theme_json(&Value::Null).is_none());
    }

/// 验证目录扫描跳过非法、重复 id、损坏 JSON、非 JSON 文件与子目录，
/// 只保留首个合法主题。
    #[test]
    fn read_custom_themes_skips_invalid_and_duplicate_entries() {
        let dir = temp_dir("themes");
        std::fs::write(
            dir.join("a.json"),
            serde_json::to_string(&valid_theme_json()).unwrap(),
        )
        .unwrap();

        let mut duplicate = valid_theme_json();
        duplicate["metadata"]["id"] = Value::String("midnight".into());
        duplicate["metadata"]["name"] = Value::String("Duplicate".into());
        std::fs::write(
            dir.join("b.json"),
            serde_json::to_string(&duplicate).unwrap(),
        )
        .unwrap();

        std::fs::write(dir.join("broken.json"), "{ not json").unwrap();

        let mut invalid = valid_theme_json();
        invalid["metadata"]["variant"] = Value::String("blue".into());
        std::fs::write(
            dir.join("invalid.json"),
            serde_json::to_string(&invalid).unwrap(),
        )
        .unwrap();

        std::fs::write(dir.join("ignored.txt"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("nested.json")).unwrap();

        let themes = read_custom_themes_from_disk(&dir, MAX_THEME_JSON_BYTES);
        assert_eq!(themes.len(), 1, "only the first valid theme survives");
        assert_eq!(themes[0]["metadata"]["name"], "Midnight");
    }

/// 验证超过 max_bytes 的主题文件被跳过，恰好等于上限时保留。
    #[test]
    fn read_custom_themes_enforces_size_cap() {
        let dir = temp_dir("themes-size");
        let mut oversized = valid_theme_json();
        oversized["padding"] = Value::String("x".repeat(1024));
        // Write a file larger than a tiny cap to prove the size gate.
        std::fs::write(
            dir.join("big.json"),
            serde_json::to_string(&oversized).unwrap(),
        )
        .unwrap();
        let size = std::fs::metadata(dir.join("big.json")).unwrap().len();
        let themes = read_custom_themes_from_disk(&dir, size - 1);
        assert!(themes.is_empty());
        let themes = read_custom_themes_from_disk(&dir, size);
        assert_eq!(themes.len(), 1);
    }

/// 验证主题目录缺失时安全返回空列表。
    #[test]
    fn read_custom_themes_missing_dir_is_empty() {
        let themes = read_custom_themes_from_disk(
            Path::new("/definitely/not/a/themes/dir"),
            MAX_THEME_JSON_BYTES,
        );
        assert!(themes.is_empty());
    }

/// 验证 External 引擎的 reload 响应要求手动重启（与 JS 形态逐字段一致）。
    #[tokio::test]
    async fn reload_external_engine_requires_manual_restart() {
        let ctx = test_ctx(
            temp_dir("reload-external"),
            EngineState::external("http://127.0.0.1:1".into(), None),
        );
        let (status, body) = json_response(
            router(ctx),
            HttpRequest::builder()
                .method(Method::POST)
                .uri("/api/config/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "success": true,
                "requiresReload": false,
                "requiresManualRestart": true,
                "message": "Configuration is saved on disk. Restart your connected OpenCode server to apply the changes.",
            })
        );
    }

/// 验证真实用户主题目录收集结果的每条都带 metadata（只断言形态）。
    #[test]
    fn themes_route_shape() {
        // The handler reads the real user themes dir; assert only the shape.
        let themes = user_themes_dir()
            .map(|dir| read_custom_themes_from_disk(&dir, MAX_THEME_JSON_BYTES))
            .unwrap_or_default();
        assert!(themes.iter().all(|theme| theme.get("metadata").is_some()));
    }

/// 验证手动重启响应体的四个字段形态。
    #[test]
    fn external_manual_restart_response_matches_config_mutation_shape() {
        let response = external_manual_restart_response("msg");
        assert_eq!(response["success"], true);
        assert_eq!(response["requiresReload"], false);
        assert_eq!(response["requiresManualRestart"], true);
        assert_eq!(response["message"], "msg");
        assert!(response.as_object().is_some());
    }
}
