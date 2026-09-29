//! Shared HTTP plumbing for the plugin/skill route families: Express-style
//! query parsing (qs-like first-value/joined-array access), the
//! `project-directory-runtime.js` directory resolvers, lenient JSON body
//! reads, and the `config-mutation-response.js` deferred-restart shape.
//!
//! The Node-`path`/URI helpers are local copies (the `fs_routes::paths`
//! module is private to its port; same convention as `session_goal`'s
//! `encode_uri_component` copy). The git-identity profile reader mirrors
//! `git_service::identity`'s storage file (`git_service::identity` is
//! private).
//!
//! 中文说明：plugin/skill 路由族共享的 HTTP 基础设施——qs 风格的查询串
//! 解析、Express 风格的宽松 JSON body 读取、project-directory-runtime 的
//! 目录解析，以及 config-mutation-response 的延迟重启响应形状。
//! Node path/URI 工具与 git-identity 只读访问为本地副本（对应的 Rust
//! 模块是私有的，无法直接复用）。

use std::path::{Component, Path, PathBuf};

use axum::http::HeaderMap;
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Node path / URI primitives
// ---------------------------------------------------------------------------

/// settings-normalization-runtime.js `normalizeDirectoryPath`.
/// 中文注解：settings 管线的 `normalizeDirectoryPath` 再导出——
/// 去首尾引号、展开 ~ 与 ~/ 为 home 目录，并自愈 Windows verbatim /
/// 重复盘符前缀。
pub(crate) use crate::settings::normalization::normalize_directory_path;

/// Node `path.resolve(input)` — lexical, no symlink following.
/// 中文注解：词法化的 path.resolve——相对路径基于当前目录，消解 . / .. / 根与盘符，不访问文件系统。
pub(crate) fn resolve_path(input: &str) -> PathBuf {
    let path = Path::new(input);
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    for component in path.components() {
        match component {
            Component::RootDir => {
                let rendered = out.to_string_lossy().into_owned();
                if !rendered.is_empty() && rendered.ends_with(':') {
                    // Windows drive prefix (`C:`) followed by its root —
                    // keep the drive, add the platform separator.
                    out.push(if cfg!(windows) { "\\" } else { "/" });
                } else {
                    out = PathBuf::from("/");
                }
            }
            Component::Prefix(prefix) => {
                out = PathBuf::from(prefix.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(segment) => {
                out.push(segment);
            }
        }
    }
    out
}

/// `decodeURIComponent` — None on malformed escapes / invalid UTF-8.
/// 中文注解：decodeURIComponent 的字节级实现；非法转义或结果非 UTF-8 时返回 None。
pub(crate) fn decode_uri_component(value: &str) -> Option<String> {
    // 单个十六进制字符转数值；非法字符返回 None。
    fn hex_val(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = hex_val(*bytes.get(index + 1)?)?;
            let lo = hex_val(*bytes.get(index + 2)?)?;
            out.push(hi * 16 + lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `encodeURIComponent`.
/// 中文注解：encodeURIComponent 的字节级实现，保留字母数字与 -_.!~*'() 其余百分号编码。
pub(crate) fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Query / body parsing
// ---------------------------------------------------------------------------

/// Parsed query string: keys and all their values in order.
/// 中文注解：查询串解析结果——保持键的首次出现顺序，同键多值聚合在一起。
pub(crate) struct QueryValues(Vec<(String, Vec<String>)>);

/// 解析 query 串：按 & 与 = 切分（无 = 视为空值），键值做宽松百分号解码
/// （失败保留原文），同键的值按出现顺序追加。
pub(crate) fn parse_query(raw: Option<&str>) -> QueryValues {
    let Some(raw) = raw else {
        return QueryValues(Vec::new());
    };
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for piece in raw.split('&') {
        if piece.is_empty() {
            continue;
        }
        let (key, value) = match piece.split_once('=') {
            Some((key, value)) => (key, value),
            None => (piece, ""),
        };
        let decoded_key = decode_uri_component(key).unwrap_or_else(|| key.to_string());
        let decoded_value = decode_uri_component(value).unwrap_or_else(|| value.to_string());
        match out
            .iter_mut()
            .find(|(existing, _)| *existing == decoded_key)
        {
            Some((_, values)) => values.push(decoded_value),
            None => out.push((decoded_key, vec![decoded_value])),
        }
    }
    QueryValues(out)
}

/// QueryValues 的取值方法，语义对齐 Express/qs 的访问习惯。
impl QueryValues {
    /// Express `Array.isArray(req.query.key) ? req.query.key[0] : req.query.key`
    /// 中文注解：数组取首值、单值原样，即 Express 的 req.query.key 读取语义。
    pub(crate) fn first(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }

    /// Express `String(req.query.key)` — arrays stringify as `a,b`.
    /// 中文注解：数组值用逗号连接、单值原样，即 String(req.query.key) 语义。
    pub(crate) fn joined(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, values)| values.join(","))
    }

    /// 暴露全部键值对（键 -> 有序值列表），供需要遍历查询参数的路由使用。
    pub(crate) fn pairs(&self) -> &[(String, Vec<String>)] {
        &self.0
    }
}

/// Express `express.json()`: only `*json` content types populate `req.body`;
/// malformed JSON bodies parse to `undefined` (null) rather than failing the
/// route — the plugin/skill handlers all guard with `req.body?.x`.
/// 中文注解：仅当 content-type 含 json 时解析 body；解析失败返回 null
/// 而非让路由失败（对齐 Express 宽松语义，调用方以可选访问取字段）。
pub(crate) fn read_json_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let json_content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if !json_content_type {
        return Value::Null;
    }
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Directory resolution (project-directory-runtime.js)
// ---------------------------------------------------------------------------

/// 校验通过的项目目录。
pub(crate) struct ValidatedDirectory {
    /// 规范化（canonicalize 优先）后的绝对目录路径。
    pub directory: PathBuf,
}

/// `validateDirectoryPath` — exists and is a directory; canonical path wins.
/// 中文注解：先 normalize 再 resolve，要求路径存在且为目录；按 IO 错误
/// 种类映射文案，成功时尽量返回 canonical 路径。
pub(crate) fn validate_directory_path(candidate: &str) -> Result<ValidatedDirectory, String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = resolve_path(&normalize_directory_path(trimmed));
    let metadata = match std::fs::metadata(&resolved) {
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
    let directory = crate::settings::normalization::strip_verbatim_prefix(
        std::fs::canonicalize(&resolved).unwrap_or(resolved),
    );
    Ok(ValidatedDirectory { directory })
}

/// `resolveOptionalProjectDirectory`: `x-opencode-directory` header (decoded
/// when `x-opencode-directory-encoding: uri`) then the `directory` query
/// parameter; no candidates → `None`; every candidate failing → the last
/// error.
/// 中文注解：依次尝试 x-opencode-directory 头（encoding=uri 时先解码）与
/// directory 查询参数，任一通过校验即返回；全部失败返回最后一个错误，
/// 无候选返回 Ok(None)。
pub(crate) fn resolve_optional_project_directory(
    headers: &HeaderMap,
    query: &QueryValues,
) -> Result<Option<PathBuf>, String> {
    let mut candidates: Vec<String> = Vec::new();
    let header_encoding = headers
        .get("x-opencode-directory-encoding")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if let Some(raw) = headers
        .get("x-opencode-directory")
        .and_then(|value| value.to_str().ok())
        && !raw.is_empty()
    {
        if header_encoding == "uri" {
            candidates.push(decode_uri_component(raw).unwrap_or_else(|| raw.to_string()));
        } else {
            candidates.push(raw.to_string());
        }
    }
    if let Some(directory) = query.first("directory")
        && !directory.is_empty()
    {
        candidates.push(directory.to_string());
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut last_error = None;
    for candidate in &candidates {
        match validate_directory_path(candidate) {
            Ok(validated) => return Ok(Some(validated.directory)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "Failed to validate directory".to_string()))
}

/// OMPChamber 用户设置文件路径：~/.config/ompchamber/settings.json（home 不可得兜底 /）。
fn ompchamber_settings_path() -> PathBuf {
    crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".config")
        .join("ompchamber")
        .join("settings.json")
}

/// 读取设置文件为 JSON；缺失或损坏一律返回 null，由调用方按缺省处理。
fn read_settings_document() -> Value {
    std::fs::read_to_string(ompchamber_settings_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

/// `resolveProjectDirectory` — like the optional resolver but falls back to
/// settings (`lastDirectory`, then the active project's path).
/// 中文注解：在可选解析之后回退到设置文件——先 lastDirectory，再按
/// activeProjectId 匹配（无匹配取首个项目）的路径；仍失败则返回错误文案。
pub(crate) async fn resolve_project_directory(
    headers: &HeaderMap,
    query: &QueryValues,
) -> ProjectDirectory {
    match resolve_optional_project_directory(headers, query) {
        Ok(Some(directory)) => {
            return ProjectDirectory {
                directory: Some(directory),
                error: None,
            };
        }
        Ok(None) => {}
        Err(error) => {
            return ProjectDirectory {
                directory: None,
                error: Some(error),
            };
        }
    }
    let settings = read_settings_document();
    if let Some(last_directory) = settings
        .get("lastDirectory")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && let Ok(validated) = validate_directory_path(last_directory)
    {
        return ProjectDirectory {
            directory: Some(validated.directory),
            error: None,
        };
    }

    let active_id = settings
        .get("activeProjectId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let projects: Vec<(String, String)> = settings
        .get("projects")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.trim().to_string();
                    let path = item.get("path").and_then(Value::as_str)?.trim().to_string();
                    (!id.is_empty() && !path.is_empty()).then_some((id, path))
                })
                .collect()
        })
        .unwrap_or_default();
    let Some((_, active_path)) = projects
        .iter()
        .find(|(id, _)| !id.is_empty() && id == active_id)
        .or_else(|| projects.first())
        .cloned()
    else {
        return ProjectDirectory {
            directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    };
    match validate_directory_path(&active_path) {
        Ok(validated) => ProjectDirectory {
            directory: Some(validated.directory),
            error: None,
        },
        Err(error) => ProjectDirectory {
            directory: None,
            error: Some(error),
        },
    }
}

/// 项目目录解析结果：directory 与 error 二选一（成功给目录，失败给文案）。
pub(crate) struct ProjectDirectory {
    /// 解析成功的目录（canonical 路径）。
    pub directory: Option<PathBuf>,
    /// 解析失败的错误文案（供路由直接返回）。
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

/// `buildDeferredRestartResponse(message)`.
/// 中文注解：构造 success + requiresRestart + restartDeferred + message 的延迟重启响应体。
pub(crate) fn build_deferred_restart_response(message: &str) -> Value {
    Value::Object(Map::from_iter([
        ("success".into(), Value::Bool(true)),
        ("requiresReload".into(), Value::Bool(false)),
        ("requiresRestart".into(), Value::Bool(true)),
        ("restartDeferred".into(), Value::Bool(true)),
        ("message".into(), Value::from(message)),
    ]))
}

/// `completePluginMutation` past tense:
/// `operation.replace(/ion$/, 'ed').replace(/update$/, 'updated')`.
/// 中文注解：复刻 JS 的 /ion$/→ed 与 /update$/→updated 两步替换，
/// 得到插件变更完成消息里的过去时用词。
pub(crate) fn plugin_mutation_past_tense(operation: &str) -> String {
    let replaced = operation
        .strip_suffix("ion")
        .map(|stem| format!("{stem}ed"))
        .unwrap_or_else(|| operation.to_string());
    replaced
        .strip_suffix("update")
        .map(|stem| format!("{stem}updated"))
        .unwrap_or(replaced)
}

/// `isEnvFlagEnabled`: any value other than unset, empty, "0" or "false".
/// 中文注解：除未设置、空串、"0"、"false"（大小写不敏感）外一律视为开启。
pub(crate) fn is_env_flag_enabled(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let normalized = value.trim().to_ascii_lowercase();
    !normalized.is_empty() && normalized != "0" && normalized != "false"
}

// ---------------------------------------------------------------------------
// Git identities (git-service identity storage, read-only)
// ---------------------------------------------------------------------------

/// `getProfiles()` / `getProfile(id)` against
/// `~/.config/ompchamber/git-identities.json` (missing/invalid → empty).
/// 中文注解：只读访问 ~/.config/ompchamber/git-identities.json 的 profiles
/// 数组；文件缺失或非法返回空表。
pub(crate) fn git_identity_profiles() -> Vec<Value> {
    let root = crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("ompchamber");
    std::fs::read_to_string(root.join("git-identities.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|document| document.get("profiles").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// 按 id 在 profiles 中查找单个 git identity；未找到返回 None。
pub(crate) fn git_identity_profile(id: &str) -> Option<Value> {
    git_identity_profiles()
        .into_iter()
        .find(|profile| profile.get("id").and_then(Value::as_str) == Some(id))
}

/// One process-wide env lock for every opencode_plugins test that mutates
/// `HOME` / `OPENCODE_CONFIG` / `OPENCODE_CONFIG_DIR` (parallel `#[test]`
/// modules would otherwise race the shared environment).
/// 中文注解：全进程唯一的环境互斥锁，所有改写 HOME / OPENCODE_CONFIG /
/// OPENCODE_CONFIG_DIR 的 opencode_plugins 测试都须经它串行。
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// http_util.rs 的单元测试：查询串语义、过去时改写、环境开关、响应形状与 path/URI 工具。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证查询串首值/逗号连接取值语义，以及缺失键返回 None。
    #[test]
    fn query_first_and_joined_semantics() {
        let values = parse_query(Some("directory=%2Ftmp&specs=a&specs=b&refresh=true"));
        assert_eq!(values.first("directory"), Some("/tmp"));
        assert_eq!(values.joined("specs").as_deref(), Some("a,b"));
        assert_eq!(values.first("refresh"), Some("true"));
        assert_eq!(values.first("missing"), None);
    }

    #[cfg(windows)]
    #[test]
    fn validate_directory_heals_corrupted_drive_prefixes() {
        // A stale UI mirror can still send the `C:\C:\...` prefixes older
        // builds persisted; validation must collapse them instead of 400ing
        // the skills/plugin routes that share this resolver.
        let dir = std::env::temp_dir().join("http-util-heal-validate");
        std::fs::create_dir_all(&dir).unwrap();
        let candidate = format!("{}{}", "C:\\".repeat(40), dir.to_string_lossy());
        let validated = validate_directory_path(&candidate)
            .unwrap_or_else(|error| panic!("validation must heal: {error}"));
        assert_eq!(
            validated.directory,
            crate::settings::normalization::strip_verbatim_prefix(
                std::fs::canonicalize(&dir).unwrap()
            )
        );
    }

    /// 验证 creation/update/deletion 等操作词的过去时改写与 JS 正则结果一致。
    #[test]
    fn past_tense_matches_the_js_rewrites() {
        assert_eq!(
            plugin_mutation_past_tense("entry creation"),
            "entry created"
        );
        assert_eq!(plugin_mutation_past_tense("entry update"), "entry updated");
        assert_eq!(
            plugin_mutation_past_tense("entry deletion"),
            "entry deleted"
        );
        assert_eq!(plugin_mutation_past_tense("file creation"), "file created");
        assert_eq!(plugin_mutation_past_tense("file update"), "file updated");
        assert_eq!(plugin_mutation_past_tense("file deletion"), "file deleted");
    }

    /// 验证环境开关仅在未设置、空串、"0"、"false"（含大小写）时为假。
    #[test]
    fn env_flag_matches_opencode_reading() {
        assert!(is_env_flag_enabled(Some("1")));
        assert!(is_env_flag_enabled(Some("yes")));
        assert!(!is_env_flag_enabled(Some("")));
        assert!(!is_env_flag_enabled(Some("0")));
        assert!(!is_env_flag_enabled(Some("false")));
        assert!(!is_env_flag_enabled(Some("FALSE")));
        assert!(!is_env_flag_enabled(None));
    }

    /// 验证延迟重启响应体的固定字段形状。
    #[test]
    fn deferred_restart_shape() {
        let body =
            build_deferred_restart_response("Plugin entry created. Restart the engine to apply.");
        assert_eq!(body["success"], Value::Bool(true));
        assert_eq!(body["requiresRestart"], Value::Bool(true));
        assert_eq!(body["restartDeferred"], Value::Bool(true));
        assert_eq!(body["requiresReload"], Value::Bool(false));
    }

    /// 验证 resolve_path 对 .. 段的词法消解。
    #[test]
    fn resolve_path_collapses_segments() {
        assert_eq!(resolve_path("/p/a/../x"), PathBuf::from("/p/x"));
    }

    /// 验证百分号编解码往返一致，残缺转义返回 None。
    #[test]
    fn uri_component_round_trip() {
        assert_eq!(encode_uri_component("a b/c@1"), "a%20b%2Fc%401");
        assert_eq!(
            decode_uri_component("a%20b%2Fc%401").as_deref(),
            Some("a b/c@1")
        );
        assert_eq!(decode_uri_component("%").is_none(), true);
    }
}
