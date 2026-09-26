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

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

/// Parse a markdown file into `(frontmatter, body)`. Body is trimmed.
pub fn parse_md_document(path: &Path) -> std::io::Result<(Map<String, Value>, String)> {
    Ok(parse_md_content(&std::fs::read_to_string(path)?))
}

pub fn parse_md_content(content: &str) -> (Map<String, Value>, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some((fm_raw, body)) = split_frontmatter(content) else {
        return (Map::new(), content.trim().to_string());
    };
    (parse_flat_yaml(fm_raw), body.trim().to_string())
}

/// `^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$` — the closing fence may
/// sit at end-of-file without a trailing newline.
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
pub fn user_opencode_config_dir() -> std::path::PathBuf {
    if let Some(home) = crate::config::home_dir() {
        return home.join(".config").join("opencode");
    }
    std::path::PathBuf::from(".config").join("opencode")
}

/// `~/.agents/loops` (USER_LOOP_ROOT from loops.js).
pub fn user_loop_root() -> std::path::PathBuf {
    let base = crate::config::home_dir().unwrap_or_default();
    base.join(".agents").join("loops")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
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
    fn accepts_closing_fence_at_eof_without_newline() {
        let (fm, body) = parse_md_content("---\nname: x\n---\nbody at eof");
        assert_eq!(fm.get("name"), Some(&json!("x")));
        assert_eq!(body, "body at eof");
    }

    #[test]
    fn treats_plain_markdown_as_empty_frontmatter() {
        let (fm, body) = parse_md_content("not a frontmatter file");
        assert!(fm.is_empty());
        assert_eq!(body, "not a frontmatter file");
    }

    #[test]
    fn strips_utf8_bom() {
        let (fm, _) = parse_md_content("\u{feff}---\nname: bom\n---\nbody");
        assert_eq!(fm.get("name"), Some(&json!("bom")));
    }

    #[test]
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
