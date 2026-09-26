//! Port of the `shared.js` markdown-file and prompt-file subset used by
//! `agents.js` / `commands.js`: `parseMdFile`, `writeMdFile`, `ensureDirs`,
//! `isPromptFileReference`, `resolvePromptFilePath`, `writePromptFile`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::OpenCodeEnv;
use super::yaml;

pub(crate) type MdResult<T> = Result<T, String>;

pub(crate) struct MdFile {
    pub frontmatter: Map<String, Value>,
    pub body: String,
}

/// `shared.js` `ensureDirs`.
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
pub(crate) fn parse_md_file(file_path: &Path) -> MdResult<MdFile> {
    let raw = std::fs::read_to_string(file_path)
        .map_err(|error| format!("Failed to read markdown file: {error}"))?;
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    parse_md_content(content)
}

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
pub(crate) fn is_prompt_file_reference(value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    parse_file_reference(text).is_some()
}

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
