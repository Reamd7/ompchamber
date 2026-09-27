//! JSONC config-layer IO for the plugins port.
//!
//! Ports the `shared.js` subset `plugins.js` needs: `readConfigFile` /
//! `readConfigLayer` (INVALID_JSONC surfaces as a coded error instead of a
//! partially-parsed object), `writeConfig` (defense-in-depth re-parse + backup
//! + pretty JSON), and the plugins.js copies of the user/project/custom config
//! path resolution (env re-read on every call).
//!
//! Gap vs the JS: the INVALID_JSONC message location suffix
//! (` (InvalidSymbol at offset 12)`) is omitted — the message is never
//! observable through the plugin routes (write failures answer the route
//! fallback message) and layer errors are collected but unused.
//!
//! 中文说明：插件移植所需的 JSONC 配置层 IO——readConfigFile /
//! readConfigLayer（INVALID_JSONC 以带码错误暴露而非半解析对象）、
//! writeConfig（写前整体重解析校验 + 旁路备份 + pretty JSON 落盘），
//! 以及 custom / user / project 三类配置路径解析（每次调用现读环境变量）。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::http_util::resolve_path;

/// 错误码：配置文件包含无法安全解析的 JSONC（INVALID_JSONC）。
pub(crate) const CODE_INVALID_JSONC: &str = "INVALID_JSONC";

/// `codedError(message, code)` from plugins.js.
/// 中文注解：带稳定错误码的错误（message 供展示，code 供 HTTP 层映射状态码）。
#[derive(Debug)]
pub(crate) struct CodedError {
    /// 人类可读的错误文案（路由直接返回给客户端）。
    pub message: String,
    /// 稳定错误码（如 INVALID_JSONC / IO / NOT_FOUND）。
    pub code: &'static str,
}

/// CodedError 的构造方法。
impl CodedError {
    /// 以消息与错误码构造实例。
    pub fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

/// Display 实现：直接输出 message，便于日志与错误串联。
impl std::fmt::Display for CodedError {
    /// 写出 message 本身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 用户级 OpenCode 配置目录 ~/.config/opencode（home 不可得时兜底为 /）。
pub(crate) fn opencode_home_config_dir() -> PathBuf {
    crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".config")
        .join("opencode")
}

/// plugins.js `getActiveCustomConfigPath` — resolved per call.
/// 中文注解：每次调用现读 OPENCODE_CONFIG 并 resolve；未设置时返回 None。
pub(crate) fn active_custom_config_path() -> Option<PathBuf> {
    std::env::var("OPENCODE_CONFIG")
        .ok()
        .map(|v| resolve_path(&v))
}

/// plugins.js `getActiveOpencodeConfigDir` — OPENCODE_CONFIG's directory wins.
/// 中文注解：设置了 OPENCODE_CONFIG 时取其父目录，否则用 ~/.config/opencode。
pub(crate) fn active_opencode_config_dir() -> PathBuf {
    if let Some(custom) = active_custom_config_path() {
        return custom
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
    }
    opencode_home_config_dir()
}

/// plugins.js `getActiveUserConfigPaths`.
/// 中文注解：活跃配置目录下的 config.json / opencode.json / opencode.jsonc 三个候选。
fn active_user_config_paths() -> Vec<PathBuf> {
    let dir = active_opencode_config_dir();
    vec![
        dir.join("config.json"),
        dir.join("opencode.json"),
        dir.join("opencode.jsonc"),
    ]
}

/// plugins.js `getPrimaryUserConfigPath` — first existing else the default.
/// 中文注解：返回首个实际存在的用户配置候选；都不存在时回退第一个。
pub(crate) fn primary_user_config_path() -> PathBuf {
    let paths = active_user_config_paths();
    paths
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| paths[0].clone())
}

/// plugins.js `getProjectConfigPath` — first existing candidate else the first.
/// 中文注解：无工作目录返回 None；否则在根下与 .opencode/ 下的
/// opencode.json / opencode.jsonc 四个候选中取首个存在的，都不存在回退第一个。
pub(crate) fn project_config_path(working_directory: Option<&Path>) -> Option<PathBuf> {
    let working_directory = working_directory?;
    let candidates = [
        working_directory.join("opencode.json"),
        working_directory.join("opencode.jsonc"),
        working_directory.join(".opencode").join("opencode.json"),
        working_directory.join(".opencode").join("opencode.jsonc"),
    ];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .or_else(|| candidates.first().cloned())
}

/// `parseConfigObject`: comment-only/whitespace-only files read as `{}`; any
/// other parse problem is INVALID_JSONC.
/// 中文注解：以允许注释与尾逗号的选项解析 JSONC；空文件或纯注释读作 {}，
/// 顶层非对象或解析失败报 INVALID_JSONC（附带文件路径）。
pub(crate) fn parse_config_object(
    content: &str,
    file_path: &Path,
) -> Result<Map<String, Value>, CodedError> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
    };
    let parsed = jsonc_parser::parse_to_serde_value(content, &options).map_err(|_| {
        CodedError::new(
            format!(
                "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely",
                file_path.display()
            ),
            CODE_INVALID_JSONC,
        )
    })?;
    match parsed {
        None => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map),
        Some(_) => Err(CodedError::new(
            format!(
                "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely",
                file_path.display()
            ),
            CODE_INVALID_JSONC,
        )),
    }
}

/// `readConfigFile`: missing/blank → `{}`; unparseable → INVALID_JSONC.
/// 中文注解：文件缺失或纯空白返回 {}；读取失败记日志并报 IO；
/// 其余交给 parse_config_object。
pub(crate) fn read_config_file(file_path: &Path) -> Result<Map<String, Value>, CodedError> {
    if !file_path.exists() {
        return Ok(Map::new());
    }
    let content = std::fs::read_to_string(file_path).map_err(|error| {
        tracing::error!(
            "Failed to read config file: {}: {error}",
            file_path.display()
        );
        CodedError::new("Failed to read OpenCode configuration", "IO")
    })?;
    let normalized = content.trim();
    if normalized.is_empty() {
        return Ok(Map::new());
    }
    parse_config_object(normalized, file_path)
}

/// 单层配置的读取结果：内容对象与可选的降级错误（INVALID_JSONC 时内容为空、错误保留）。
pub(crate) struct ConfigLayer {
    /// 解析出的配置对象（降级时为空 map）。
    pub config: Map<String, Value>,
    /// 读取期间发生的 INVALID_JSONC 错误（正常为 None）。
    pub error: Option<CodedError>,
}

/// `readConfigLayer`: INVALID_JSONC degrades to `{}` + the error; everything
/// else propagates.
/// 中文注解：无路径返回空层；INVALID_JSONC 记日志并降级为空配置 + 携带错误；
/// 其它错误原样上抛。
pub(crate) fn read_config_layer(file_path: Option<&Path>) -> Result<ConfigLayer, CodedError> {
    let Some(file_path) = file_path else {
        return Ok(ConfigLayer {
            config: Map::new(),
            error: None,
        });
    };
    match read_config_file(file_path) {
        Ok(config) => Ok(ConfigLayer {
            config,
            error: None,
        }),
        Err(error) if error.code == CODE_INVALID_JSONC => {
            tracing::error!("{}", error.message);
            Ok(ConfigLayer {
                config: Map::new(),
                error: Some(error),
            })
        }
        Err(error) => Err(error),
    }
}

/// `writeConfig`: never overwrite a file we cannot fully parse; back the
/// existing file up next to itself before writing pretty JSON.
/// 中文注解：目标已存在时先整体重解析（拒绝覆盖无法解析的文件）并复制
/// .ompchamber.backup 备份，再以 pretty JSON 写入（必要时建父目录）；
/// IO 类错误收敛为固定文案，INVALID_JSONC 原样上抛。
pub(crate) fn write_config(
    config: &Map<String, Value>,
    file_path: &Path,
) -> Result<(), CodedError> {
    let write_result = (|| -> Result<(), CodedError> {
        if file_path.exists() {
            let existing = std::fs::read_to_string(file_path)
                .map_err(|_| CodedError::new("Failed to write OpenCode configuration", "IO"))?;
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                parse_config_object(trimmed, file_path)?;
            }
            let backup = PathBuf::from(format!("{}.ompchamber.backup", file_path.display()));
            std::fs::copy(file_path, &backup)
                .map_err(|_| CodedError::new("Failed to write OpenCode configuration", "IO"))?;
            tracing::info!("Created config backup: {}", backup.display());
        }
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| CodedError::new("Failed to write OpenCode configuration", "IO"))?;
        }
        let serialized = serde_json::to_string_pretty(&Value::Object(config.clone()))
            .map_err(|_| CodedError::new("Failed to write OpenCode configuration", "IO"))?;
        std::fs::write(file_path, serialized)
            .map_err(|_| CodedError::new("Failed to write OpenCode configuration", "IO"))?;
        Ok(())
    })();
    match write_result {
        Err(error) if error.code == CODE_INVALID_JSONC => Err(error),
        Err(error) => {
            tracing::error!(
                "Failed to write config file: {}: {}",
                file_path.display(),
                error
            );
            Err(CodedError::new(
                "Failed to write OpenCode configuration",
                "IO",
            ))
        }
        ok => ok,
    }
}

/// config_layers.rs 的单元测试：覆盖缺失/空白/注释文件的读取、JSONC 容错、
/// INVALID_JSONC 报码，以及写入的备份与拒写行为。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 为当前测试创建唯一的临时目录。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-plugin-config-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 进程内原子递增计数，用于临时目录去重。
    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        // 局部静态计数器：每次调用递增，保证并发测试目录唯一。
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    /// 验证缺失/空白/纯注释文件读作 {}，含注释与尾逗号的 JSONC 正常解析。
    #[test]
    fn read_config_file_handles_missing_blank_and_jsonc() {
        let dir = temp_dir("read");
        let missing = dir.join("missing.json");
        assert!(read_config_file(&missing).expect("read").is_empty());

        let blank = dir.join("blank.json");
        std::fs::write(&blank, "   \n").expect("write");
        assert!(read_config_file(&blank).expect("read").is_empty());

        let commented = dir.join("comments.jsonc");
        std::fs::write(&commented, "// only a comment\n").expect("write");
        assert!(read_config_file(&commented).expect("read").is_empty());

        let jsonc = dir.join("trailing.jsonc");
        std::fs::write(&jsonc, "{\n  // c\n  \"a\": 1,\n}").expect("write");
        let config = read_config_file(&jsonc).expect("read");
        assert_eq!(config.get("a"), Some(&json!(1)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 验证损坏的 JSONC 以 INVALID_JSONC 错误码与固定文案报错。
    #[test]
    fn invalid_jsonc_is_a_coded_error() {
        let dir = temp_dir("invalid");
        let file = dir.join("broken.jsonc");
        std::fs::write(&file, "{ broken").expect("write");
        let error = read_config_file(&file).expect_err("must fail");
        assert_eq!(error.code, CODE_INVALID_JSONC);
        assert!(error.message.contains("contains invalid JSONC"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 验证写入前生成 .ompchamber.backup 备份，且落盘内容为 pretty JSON。
    #[test]
    fn write_config_backs_up_and_pretty_prints() {
        let dir = temp_dir("write");
        let file = dir.join("opencode.json");
        std::fs::write(&file, r#"{"plugin":["a"]}"#).expect("write");
        let mut config = Map::new();
        config.insert("plugin".into(), json!(["a", "b"]));
        write_config(&config, &file).expect("write");

        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "{\n  \"plugin\": [\n    \"a\",\n    \"b\"\n  ]\n}"
        );
        assert!(
            file.with_file_name("opencode.json.ompchamber.backup")
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(file.with_file_name("opencode.json.ompchamber.backup"))
                .expect("read"),
            r#"{"plugin":["a"]}"#
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 验证既有文件无法解析时拒绝写入：原文件与备份文件均不被触碰。
    #[test]
    fn write_config_refuses_unparseable_existing_file() {
        let dir = temp_dir("refuse");
        let file = dir.join("opencode.json");
        std::fs::write(&file, "{ nope").expect("write");
        let error = write_config(&Map::new(), &file).expect_err("must refuse");
        assert_eq!(error.code, CODE_INVALID_JSONC);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "{ nope");
        assert!(
            !file
                .with_file_name("opencode.json.ompchamber.backup")
                .exists()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
