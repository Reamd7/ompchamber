//! Port of the `shared.js` markdown-file and prompt-file subset used by
//! `agents.js` / `commands.js`: `parseMdFile`, `writeMdFile`, `ensureDirs`,
//! `isPromptFileReference`, `resolvePromptFilePath`, `writePromptFile`.
//!
//! 中文说明：移植 `shared.js` 中被 `agents.js`/`commands.js` 用到的
//! markdown 文件与 prompt 文件子集——frontmatter 解析/清洗/写回、
//! 目录确保，以及 `{file:...}` 引用的识别与落盘。YAML 由内部的
//! `yaml` 模块（allow-list 友好的子集实现）处理。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::OpenCodeEnv;
use super::yaml;

/// 中文：markdown 文件操作的统一错误类型（字符串错误，镜像 JS 抛错文案）。
pub(crate) type MdResult<T> = Result<T, String>;

/// 中文：解析后的 markdown 文件：frontmatter 键值映射 + trim 过的正文。
pub(crate) struct MdFile {
    /// frontmatter 解析结果（无 frontmatter 时为空映射）。
    pub frontmatter: Map<String, Value>,
    /// 正文（已 trim）。
    pub body: String,
}

/// `shared.js` `ensureDirs`.
/// 中文：确保配置目录存在：config 根目录及 agents/commands/skills
/// 三个子目录，缺则递归创建（失败静默，后续读写自然会报错）。
pub(crate) fn ensure_dirs(env: &OpenCodeEnv) {
    let dirs = [
        env.config_dir.clone(),
        env.agent_dir(),
        env.command_dir(),
        env.skill_dir(),
    ];
    for dir in dirs {
        if !dir.exists() {
            let _ = std::fs::create_dir_all(&dir);
        }
    }
}

/// Mirror of OpenCode's markdown frontmatter sanitizer: lines whose unquoted
/// scalar value contains a colon (`description: Build agent: creates builds`)
/// are rewritten as block scalars so a second parse pass accepts them.
/// 中文：清洗 frontmatter 文本：跳过注释/空行/续行/已引号行，仅把
/// "未加引号且值中含冒号" 的行改写为 `key: |-` 块标量，让第二次
/// YAML 解析能够接受；其余行原样保留。
fn sanitize_frontmatter(frontmatter: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in frontmatter.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with(' ') {
            out.push(line.to_string());
            continue;
        }
        let Some((key, value)) = split_key_line(line) else {
            out.push(line.to_string());
            continue;
        };
        let value = value.trim();
        if value.is_empty()
            || value == ">"
            || value == "|"
            || value.starts_with('"')
            || value.starts_with('\'')
        {
            out.push(line.to_string());
            continue;
        }
        if !value.contains(':') {
            out.push(line.to_string());
            continue;
        }
        out.push(format!("{key}: |-"));
        out.push(format!("  {value}"));
    }
    out.join("\n")
}

/// `^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$` per line.
/// 中文：按 JS 正则 `^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$` 拆分单行
/// 键值；不匹配（首字符非法、缺冒号）返回 `None`。
fn split_key_line(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    if bytes.is_empty() || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return None;
    }
    let mut index = 1;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
        index += 1;
    }
    let key_end = index;
    let mut cursor = index;
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if cursor >= bytes.len() || bytes[cursor] != b':' {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    Some((&line[..key_end], &line[cursor..]))
}

/// `shared.js` `parseMdFile`: BOM-stripped, frontmatter closed by an exact
/// `---` line (newline or EOF after it), lenient YAML retry, trimmed body.
/// 中文：读取并解析 markdown 文件：先剥掉 UTF-8 BOM 再交给
/// [`parse_md_content`]；读取失败返回带路径上下文的错误。
pub(crate) fn parse_md_file(file_path: &Path) -> MdResult<MdFile> {
    let raw = std::fs::read_to_string(file_path)
        .map_err(|error| format!("Failed to read markdown file: {error}"))?;
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    parse_md_content(content)
}

/// 中文：解析 markdown 文本：无 frontmatter 时返回空映射 + trim 后
/// 正文；frontmatter 先按原样解析，失败则用清洗版重试，再失败按
/// 空映射降级（仅记录 warn 日志）。
pub(crate) fn parse_md_content(content: &str) -> MdResult<MdFile> {
    let Some((frontmatter_text, body)) = split_frontmatter(content) else {
        return Ok(MdFile {
            frontmatter: Map::new(),
            body: content.trim().to_string(),
        });
    };
    let frontmatter = match yaml::parse_yaml_object(&frontmatter_text) {
        Ok(map) => map,
        Err(first_error) => {
            match yaml::parse_yaml_object(&sanitize_frontmatter(&frontmatter_text)) {
                Ok(map) => map,
                Err(_) => {
                    tracing::warn!(
                        "Failed to parse markdown frontmatter, treating as empty: {first_error:?}"
                    );
                    Map::new()
                }
            }
        }
    };
    Ok(MdFile {
        frontmatter,
        body: body.trim().to_string(),
    })
}

/// JS regex `^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$`: the closing
/// `---` must be its own line (lazy first match).
/// 中文：对应 JS 正则 `^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$`：
/// 开头必须是独立的 `---` 行，闭合 `---` 也必须独占一行（其后是换行
/// 或 EOF）；惰性匹配取第一个闭合。返回 (frontmatter 文本, 正文)。
fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let opener_len = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let mut cursor = &content[opener_len..];
    let mut frontmatter_lines: Vec<&str> = Vec::new();
    loop {
        match cursor.find('\n') {
            Some(newline) => {
                let raw_line = &cursor[..newline];
                let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
                if line == "---" {
                    return Some((
                        frontmatter_lines.join("\n"),
                        cursor[newline + 1..].to_string(),
                    ));
                }
                frontmatter_lines.push(line);
                cursor = &cursor[newline + 1..];
            }
            None => {
                // Final line without a trailing newline: a lone `---` closes
                // at EOF (gray-matter accepts this shape).
                let line = cursor.strip_suffix('\r').unwrap_or(cursor);
                if line == "---" {
                    return Some((frontmatter_lines.join("\n"), String::new()));
                }
                return None;
            }
        }
    }
}

/// `shared.js` `writeMdFile`.
/// 中文：写回 markdown 文件：过滤掉值为 null 的 frontmatter 键，
/// YAML 序列化后以 `---` 围栏 + 空行 + 正文重组并覆盖写。写失败时
/// 记录 error 日志并返回统一错误文案。
pub(crate) fn write_md_file(
    file_path: &Path,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> MdResult<()> {
    let cleaned: Map<String, Value> = frontmatter
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let yaml_text = yaml::stringify_yaml(&cleaned);
    let content = format!("---\n{yaml_text}---\n\n{body}");
    std::fs::write(file_path, content).map_err(|error| {
        tracing::error!("Failed to write markdown file {file_path:?}: {error}");
        "Failed to write agent markdown file".to_string()
    })
}

// ---------------------------------------------------------------------------
// Prompt file helpers
// ---------------------------------------------------------------------------

/// `/^\{file:(.+)\}$/i`.
/// 中文：判断 JSON 值是否为 `{file:路径}` 形式的 prompt 文件引用
/// （大小写不敏感的 JS 正则 `/^\{file:(.+)\}$/i` 对应实现）。
pub(crate) fn is_prompt_file_reference(value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    parse_file_reference(text).is_some()
}

/// 中文：提取 `{file:...}` 内部非空路径文本；不匹配返回 `None`。
fn parse_file_reference(trimmed: &str) -> Option<String> {
    let inner = trimmed.strip_prefix("{file:")?.strip_suffix('}')?;
    // `.+` requires a non-empty capture.
    if inner.is_empty() {
        return None;
    }
    Some(inner.to_string())
}

/// `shared.js` `resolvePromptFilePath` — `{file:...}` target relative to the
/// OpenCode config dir (`./`-prefixed and bare names included).
/// 中文：把 `{file:...}` 引用解析为实际路径：`./` 前缀与裸名相对
/// OpenCode 配置目录，绝对路径原样返回；非字符串/格式不符返回 `None`。
pub(crate) fn resolve_prompt_file_path(env: &OpenCodeEnv, reference: &Value) -> Option<PathBuf> {
    let text = reference.as_str()?;
    let target = parse_file_reference(text.trim())?.trim().to_string();
    if target.is_empty() {
        return None;
    }
    if let Some(relative) = target.strip_prefix("./") {
        return Some(env.config_dir.join(relative));
    }
    if Path::new(&target).is_absolute() {
        return Some(PathBuf::from(target));
    }
    Some(env.config_dir.join(target))
}

/// `shared.js` `writePromptFile`.
/// 中文：写 prompt 文件：先确保父目录存在再覆盖写；失败仅记录
/// error 日志（不向调用方报错，镜像 JS 的静默行为）。
pub(crate) fn write_prompt_file(file_path: &Path, content: &str) {
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(file_path, content) {
        tracing::error!("Failed to write prompt file {file_path:?}: {error}");
        return;
    }
    tracing::info!("Updated prompt file: {file_path:?}");
}
