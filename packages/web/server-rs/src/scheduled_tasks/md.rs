//! Minimal markdown-with-frontmatter reader/writer shared by the
//! scheduled-tasks port (loops + snippets).
//!
//! Ports the pieces of `server/lib/opencode/shared.js` these modules use:
//! `parseMdFile` (BOM strip; `---` frontmatter block where the closing fence
//! may sit at EOF without a trailing newline; body trimmed) and `writeMdFile`
//! (drop null values, stringify frontmatter, `---\n<yaml>---\n\n<body>`).
//!
//! YAML support is a flat-scalar subset (strings, booleans, numbers, null)
//! rather than a full YAML engine — the loop/snippet frontmatter formats only
//! use flat scalar maps. Gap noted in PORT-MANIFEST.md.
//!
//! 计划任务移植（loops + snippets）共享的极简 markdown+frontmatter
//! 读写器：解析侧剥离 BOM、识别可位于 EOF 的闭合围栏；写出侧丢弃
//! null 值并按扁平标量 YAML 序列化。仅支持平铺标量子集
//!（循环/片段的 frontmatter 只用到平铺标量 map），差异记录于
//! PORT-MANIFEST.md。

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

/// Parse a markdown file into `(frontmatter, body)`. Body is trimmed.
/// 读文件并解析为 (frontmatter, 正文)；正文已去首尾空白，IO 错误原样上抛。
pub fn parse_md_document(path: &Path) -> std::io::Result<(Map<String, Value>, String)> {
    Ok(parse_md_content(&std::fs::read_to_string(path)?))
}

/// 解析字符串内容：剥 UTF-8 BOM；无 frontmatter 围栏时返回空 map 与原文。
pub fn parse_md_content(content: &str) -> (Map<String, Value>, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some((fm_raw, body)) = split_frontmatter(content) else {
        return (Map::new(), content.trim().to_string());
    };
    (parse_flat_yaml(fm_raw), body.trim().to_string())
}

/// `^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$` — the closing fence may
/// sit at end-of-file without a trailing newline.
/// 拆出 (frontmatter 原文, 正文)：文件须以 `---` 开头，闭合围栏是首个
/// 整行 `---`；允许闭合围栏位于 EOF 且不带换行。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // Find the first line that is exactly `---`.
    let mut search_from = 0usize;
    while let Some(rel) = after_open[search_from..].find("\n---") {
        let line_start = search_from + rel + 1;
        let rest = &after_open[line_start..];
        let is_fence = rest.starts_with("---\n") || rest.starts_with("---\r\n") || rest == "---";
        if is_fence {
            let fm = &after_open[..line_start];
            let mut body = &rest[3..];
            if let Some(stripped) = body.strip_prefix('\r') {
                body = stripped;
            }
            let body = body.strip_prefix('\n').unwrap_or(body);
            return Some((fm, body));
        }
        search_from = line_start + 1;
    }
    None
}

/// Flat scalar YAML subset: `key: value` lines (comments and blank lines
/// skipped). Unindented continuation and nested structures are out of subset.
/// 解析扁平标量 YAML 子集：仅接受顶层 `key: value` 行；注释、空行与
/// 缩进行（嵌套内容）一律跳过，不做续行合并。
pub fn parse_flat_yaml(source: &str) -> Map<String, Value> {
    let mut map = Map::new();
    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            continue; // nested content — out of subset
        }
        let Some((key, value)) = split_key_value(trimmed) else {
            continue;
        };
        map.insert(key, scalar_value(value));
    }
    map
}

/// 拆 `key: value`：key 非空且以字母或 `_` 开头才有效，否则返回 None。
fn split_key_value(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let key = &line[..colon];
    let value = line[colon + 1..].trim();
    if key.is_empty()
        || !key
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return None;
    }
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

/// 标量解析优先级：`[...]` 流式数组、`null`/`~`、`true`/`false`、引号
/// 字符串、整数（禁前导零）、浮点数，其余一律按字符串处理。
fn scalar_value(raw: &str) -> Value {
    let v = raw.trim();
    if v.starts_with('[') && v.ends_with(']') && v.len() >= 2 {
        let items: Vec<Value> = v[1..v.len() - 1]
            .split(',')
            .map(|item| scalar_value(item.trim()))
            .collect();
        return Value::Array(items);
    }
    if v.is_empty() || v == "null" || v == "~" {
        return Value::Null;
    }
    if v == "true" {
        return Value::Bool(true);
    }
    if v == "false" {
        return Value::Bool(false);
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    if let Ok(n) = v.parse::<i64>()
        && (!v.starts_with('0') || v == "0")
    {
        return Value::Number(n.into());
    }
    if let Ok(f) = v.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    Value::String(v.to_string())
}

/// `writeMdFile`: drop null values, stringify, `---\n<yaml>---\n\n<body>`.
/// 写出 markdown 文档：丢弃 null 值、键按 BTreeMap 排序序列化为
/// `---` 围栏 YAML + 空行 + 正文；自动创建父目录，IO 错误上抛。
pub fn write_md_document(
    path: &Path,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> std::io::Result<()> {
    let cleaned: BTreeMap<&String, &Value> =
        frontmatter.iter().filter(|(_, v)| !v.is_null()).collect();
    let mut yaml = String::new();
    for (key, value) in &cleaned {
        yaml.push_str(&format!("{}: {}\n", key, yaml_scalar(value)));
    }
    let content = format!("---\n{yaml}---\n\n{body}");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// 单个值转为 YAML 标量串：bool/数字直写，字符串经 quote_if_needed
/// 按需加引号，数组与对象转流式（flow）形式。
fn yaml_scalar(value: &Value) -> String {
    match value {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => quote_if_needed(s),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(s) => quote_if_needed(s),
                    other => yaml_scalar(other),
                })
                .collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", k, yaml_scalar(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        _ => "null".to_string(),
    }
}

/// 判断并加双引号：空串、首尾空格、含 `: `/`#`/换行、YAML 保留起始符、
/// 纯布尔/null 词、可解析为数字等会被 YAML 误读的形状都触发引号，
/// 引号内转义反斜杠与双引号。
fn quote_if_needed(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains(": ")
        || s.ends_with(':')
        || s.contains('#')
        || s.contains('\n')
        || s.starts_with([
            '&', '*', '!', '%', '@', '`', '>', '|', '{', '[', '\'', '"', '?', '-', ',',
        ])
        || ["true", "false", "null", "~"].contains(&s)
        || s.parse::<f64>().is_ok();
    if needs_quoting {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// `~/.config/opencode` (OPENCODE_CONFIG_DIR analog from snippets.js).
/// `~/.config/opencode`（对应 snippets.js 的 OPENCODE_CONFIG_DIR）；
/// 取不到 home 目录时退化为相对路径。
pub fn user_opencode_config_dir() -> std::path::PathBuf {
    if let Some(home) = crate::config::home_dir() {
        return home.join(".config").join("opencode");
    }
    std::path::PathBuf::from(".config").join("opencode")
}

/// `~/.agents/loops` (USER_LOOP_ROOT from loops.js).
/// `~/.agents/loops`（loops.js 的 USER_LOOP_ROOT）；home 缺失时得到相对路径。
pub fn user_loop_root() -> std::path::PathBuf {
    let base = crate::config::home_dir().unwrap_or_default();
    base.join(".agents").join("loops")
}

/// frontmatter 解析与写出的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
/// 验证字符串、布尔与带引号标量及正文的常规解析。
    fn parses_frontmatter_and_body() {
        let (fm, body) = parse_md_content(
            "---\nname: daily\nschedule: \"0 9 * * *\"\nenabled: true\n---\nRun daily.\n",
        );
        assert_eq!(fm.get("name"), Some(&json!("daily")));
        assert_eq!(fm.get("schedule"), Some(&json!("0 9 * * *")));
        assert_eq!(fm.get("enabled"), Some(&json!(true)));
        assert_eq!(body, "Run daily.");
    }

    #[test]
/// 验证闭合围栏落在 EOF 且无换行时仍能拆出 frontmatter 与正文。
    fn accepts_closing_fence_at_eof_without_newline() {
        let (fm, body) = parse_md_content("---\nname: x\n---\nbody at eof");
        assert_eq!(fm.get("name"), Some(&json!("x")));
        assert_eq!(body, "body at eof");
    }

    #[test]
/// 验证无围栏的纯 markdown 得到空 frontmatter 与原文正文。
    fn treats_plain_markdown_as_empty_frontmatter() {
        let (fm, body) = parse_md_content("not a frontmatter file");
        assert!(fm.is_empty());
        assert_eq!(body, "not a frontmatter file");
    }

    #[test]
/// 验证 UTF-8 BOM 被剥离后再进入围栏解析。
    fn strips_utf8_bom() {
        let (fm, _) = parse_md_content("\u{feff}---\nname: bom\n---\nbody");
        assert_eq!(fm.get("name"), Some(&json!("bom")));
    }

    #[test]
/// 验证 null 键被丢弃，且写出的文档能无损读回。
    fn writes_roundtrip_with_plain_scalars() {
        let dir = std::env::temp_dir().join(format!("oc-md-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("loop.md");
        let mut fm = Map::new();
        fm.insert("name".into(), json!("daily-digest"));
        fm.insert("enabled".into(), json!(false));
        fm.insert("dropped".into(), Value::Null);
        write_md_document(&path, &fm, "Run the digest.").expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("enabled: false"));
        assert!(text.contains("name: daily-digest"));
        assert!(!text.contains("dropped"));
        assert!(text.contains("Run the digest."));

        let (parsed, body) = parse_md_document(&path).expect("parse");
        assert_eq!(parsed.get("name"), Some(&json!("daily-digest")));
        assert_eq!(parsed.get("enabled"), Some(&json!(false)));
        assert_eq!(body, "Run the digest.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
/// 验证含冒号/特殊形状的值被引号包裹后往返不变形。
    fn quotes_values_yaml_would_misread() {
        let mut fm = Map::new();
        fm.insert("schedule".into(), json!("0 9 * * *"));
        fm.insert("tricky".into(), json!("has: colon"));
        let dir = std::env::temp_dir().join(format!("oc-md-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("q.md");
        write_md_document(&path, &fm, "b").expect("write");
        let (parsed, _) = parse_md_document(&path).expect("parse");
        assert_eq!(parsed.get("schedule"), Some(&json!("0 9 * * *")));
        assert_eq!(parsed.get("tricky"), Some(&json!("has: colon")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
