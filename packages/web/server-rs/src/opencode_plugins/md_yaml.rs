//! Markdown-with-frontmatter IO for the skills/snippets port.
//!
//! Ports the `parseMdFile`/`writeMdFile` pair from `server/lib/opencode/shared.js`
//! plus the `parseMarkdownFile`/`writeMarkdownFile` pair from `snippets.js`.
//!
//! YAML support is a hand-rolled subset (the `yaml` npm crate is not on the
//! allowed dependency list): flat and nested block mappings, block sequences,
//! inline `[a, b]` arrays, quoted and plain scalars, and `|`/`>` block
//! scalars — the shapes real SKILL.md / snippet frontmatter uses. Full YAML
//! (anchors, flow maps, multi-doc) is out of subset; parse failures degrade
//! through the same `sanitizeFrontmatter` retry the JS performs.
//!
//! Known gap vs the JS: `yaml.stringify` preserves insertion order and folds
//! long scalars at column 80; serde_json maps iterate alphabetically and this
//! writer never folds, so rewritten frontmatter can differ in key order and
//! line wrapping (parsed content is identical).

use std::path::Path;

use serde_json::{Map, Value};

/// `parseMdFile`: BOM strip, `---` frontmatter block (closing fence may sit at
/// EOF without a trailing newline), lenient YAML with the colon-value retry,
/// body trimmed.
pub(crate) fn parse_md_file(path: &Path) -> std::io::Result<(Map<String, Value>, String)> {
    let raw = std::fs::read_to_string(path)?;
    Ok(parse_md_content(&raw))
}

pub(crate) fn parse_md_content(raw: &str) -> (Map<String, Value>, String) {
    let content = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some((frontmatter_src, body)) = split_frontmatter(content) else {
        return (Map::new(), content.trim().to_string());
    };

    let frontmatter = match parse_yaml(frontmatter_src) {
        Ok(Value::Object(map)) => map,
        Ok(_) => Map::new(),
        Err(_) => match parse_yaml(&sanitize_frontmatter(frontmatter_src)) {
            Ok(Value::Object(map)) => map,
            _ => {
                tracing::warn!(
                    "Failed to parse markdown frontmatter, treating as empty: {}",
                    path_hint()
                );
                Map::new()
            }
        },
    };

    (frontmatter, body.trim().to_string())
}

fn path_hint() -> &'static str {
    "(skill/snippet markdown)"
}

/// `^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)([\s\S]*)$`
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // find the closing fence: `\n---` followed by newline or EOF
    let bytes = rest.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let next_nl = rest[cursor..]
            .find('\n')
            .map(|idx| cursor + idx)
            .unwrap_or(bytes.len());
        let line = &rest[cursor..next_nl];
        let line_trimmed = line.strip_suffix('\r').unwrap_or(line);
        if line_trimmed == "---" {
            // The regex requires at least one newline between the fences
            // (`([\s\S]*?)\r?\n---`), so a fence at position 0 is not a
            // closing fence.
            if cursor == 0 {
                return None;
            }
            let fm = &rest[..cursor];
            let after = if next_nl < bytes.len() {
                &rest[next_nl + 1..]
            } else {
                ""
            };
            return Some((fm, after));
        }
        if next_nl >= bytes.len() {
            break;
        }
        cursor = next_nl + 1;
    }
    None
}

/// Mirror of OpenCode's markdown frontmatter sanitizer: rewrite plain values
/// containing colons as block scalars so strict YAML accepts them.
fn sanitize_frontmatter(source: &str) -> String {
    let mut out = String::new();
    for line in source.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with(' ') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let Some((key, value)) = split_plain_entry(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let value = value.trim();
        if value.is_empty()
            || value == ">"
            || value == "|"
            || value.starts_with('"')
            || value.starts_with('\'')
        {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if !value.contains(':') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(&format!("{key}: |-\n  {value}\n"));
    }
    out
}

/// `^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$`
fn split_plain_entry(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = &line[..colon];
    let mut chars = key.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let rest = line[colon + 1..].trim_start();
    Some((key, rest))
}

// ---------------------------------------------------------------------------
// YAML subset
// ---------------------------------------------------------------------------

struct YamlLine<'a> {
    indent: usize,
    text: &'a str,
}

fn lex(source: &str) -> Vec<YamlLine<'_>> {
    let mut lines = Vec::new();
    for raw in source.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = raw.trim_start_matches(' ');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = raw.len() - trimmed.len();
        lines.push(YamlLine {
            indent,
            text: trimmed,
        });
    }
    lines
}

pub(crate) fn parse_yaml(source: &str) -> Result<Value, String> {
    let lines = lex(source);
    if lines.is_empty() {
        return Ok(Value::Null);
    }
    let mut cursor = 0usize;
    let value = parse_block(&lines, &mut cursor, lines[0].indent)?;
    if cursor != lines.len() {
        return Err(format!("unexpected content at line {}", cursor + 1));
    }
    Ok(value)
}

fn parse_block(lines: &[YamlLine<'_>], cursor: &mut usize, indent: usize) -> Result<Value, String> {
    if *cursor >= lines.len() {
        return Ok(Value::Null);
    }
    if lines[*cursor].text.starts_with("- ") || lines[*cursor].text == "-" {
        parse_sequence(lines, cursor, indent)
    } else {
        parse_mapping(lines, cursor, indent)
    }
}

fn parse_sequence(
    lines: &[YamlLine<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<Value, String> {
    let mut items = Vec::new();
    while *cursor < lines.len() && lines[*cursor].indent == indent {
        let entry = lines[*cursor].text.strip_prefix('-').unwrap_or("");
        let entry = entry.strip_prefix(' ').unwrap_or(entry);
        if entry.is_empty() {
            *cursor += 1;
            let nested = parse_block(lines, cursor, indent + 2)?;
            items.push(nested);
        } else if let Some((key, value)) = split_plain_entry(entry) {
            // `- key: value` inline mapping entry — gather siblings at the
            // deeper indent into one map.
            let mut map = Map::new();
            insert_scalar_or_nested(&mut map, key, value, lines, cursor, indent + 2)?;
            while *cursor < lines.len() && lines[*cursor].indent == indent + 2 {
                let line = lines[*cursor].text;
                let Some((k, v)) = split_plain_entry(line) else {
                    break;
                };
                *cursor += 1;
                insert_scalar_or_nested(&mut map, k, v, lines, cursor, indent + 2)?;
            }
            items.push(Value::Object(map));
        } else {
            *cursor += 1;
            items.push(scalar_value(entry)?);
        }
    }
    Ok(Value::Array(items))
}

fn parse_mapping(
    lines: &[YamlLine<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<Value, String> {
    let mut map = Map::new();
    while *cursor < lines.len() {
        let line = &lines[*cursor];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(format!("unexpected indentation at line {}", *cursor + 1));
        }
        if line.text.starts_with("- ") || line.text == "-" {
            break;
        }
        let Some((key, value)) = split_plain_entry(line.text) else {
            return Err(format!("unparsable mapping line: {}", line.text));
        };
        *cursor += 1;
        insert_scalar_or_nested(&mut map, key, value, lines, cursor, indent)?;
    }
    Ok(Value::Object(map))
}

/// Insert `key: value` where `value` may be empty (nested block follows),
/// a block-scalar header, an inline array, or a plain/quoted scalar.
fn insert_scalar_or_nested(
    map: &mut Map<String, Value>,
    key: &str,
    value: &str,
    lines: &[YamlLine<'_>],
    cursor: &mut usize,
    indent: usize,
) -> Result<(), String> {
    let value = strip_trailing_comment(value);
    if value.is_empty() {
        if *cursor < lines.len() && lines[*cursor].indent > indent {
            let nested = parse_block(lines, cursor, lines[*cursor].indent)?;
            map.insert(key.to_string(), nested);
        } else {
            map.insert(key.to_string(), Value::Null);
        }
        return Ok(());
    }
    if let Some(header) = block_scalar_header(value) {
        let text = read_block_scalar(lines, cursor, indent, header);
        map.insert(key.to_string(), Value::String(text));
        return Ok(());
    }
    if value.starts_with('[') && value.ends_with(']') {
        map.insert(key.to_string(), parse_inline_array(value)?);
        return Ok(());
    }
    map.insert(key.to_string(), scalar_value(value)?);
    Ok(())
}

fn block_scalar_header(value: &str) -> Option<(char, bool)> {
    // `|`, `|-`, `|+`, `>`, `>-`, `>+` (no explicit indentation digit support)
    let mut chars = value.chars();
    let style = chars.next()?;
    if style != '|' && style != '>' {
        return None;
    }
    let rest: String = chars.collect();
    let chomp = match rest.as_str() {
        "" => ' ',
        "-" => '-',
        "+" => '+',
        _ => return None,
    };
    Some((style, chomp == '-'))
}

fn read_block_scalar(
    lines: &[YamlLine<'_>],
    cursor: &mut usize,
    indent: usize,
    (style, strip): (char, bool),
) -> String {
    let mut collected: Vec<String> = Vec::new();
    while *cursor < lines.len() && lines[*cursor].indent > indent {
        let pad =
            " ".repeat(lines[*cursor].indent - indent - 2.min(lines[*cursor].indent - indent));
        collected.push(format!("{pad}{}", lines[*cursor].text));
        *cursor += 1;
    }
    let mut text = if style == '|' {
        collected.join("\n")
    } else {
        // folded: single newline between lines
        collected.join("\n")
    };
    if !strip && !text.is_empty() {
        text.push('\n');
    }
    text
}

fn parse_inline_array(value: &str) -> Result<Value, String> {
    let inner = &value[1..value.len() - 1];
    let mut items = Vec::new();
    for piece in split_inline(inner) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        items.push(scalar_value(piece)?);
    }
    Ok(Value::Array(items))
}

fn split_inline(inner: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in inner.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            ',' if !in_single && !in_double => {
                pieces.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    pieces.push(current);
    pieces
}

/// Strip a trailing ` # comment` from a plain (unquoted) scalar.
fn strip_trailing_comment(value: &str) -> &str {
    if value.starts_with('\'') || value.starts_with('"') {
        return value;
    }
    if let Some(idx) = value.find(" #") {
        return value[..idx].trim_end();
    }
    value
}

fn scalar_value(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if let Some(unquoted) = raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Ok(Value::String(unquoted.replace("''", "'")));
    }
    if let Some(unquoted) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Ok(Value::String(unescape_double(unquoted)?));
    }
    if raw.starts_with('[') {
        if raw.ends_with(']') {
            return parse_inline_array(raw);
        }
        return Err(format!("unterminated inline array: {raw}"));
    }
    Ok(coerce_plain(strip_trailing_comment(raw)))
}

fn unescape_double(source: &str) -> Result<String, String> {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Ok(out)
}

/// YAML 1.2 core schema coercion: `true/false`, `null`/`~`, integers, floats,
/// everything else a string.
fn coerce_plain(raw: &str) -> Value {
    match raw {
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        "null" | "Null" | "NULL" | "~" => return Value::Null,
        _ => {}
    }
    if let Ok(number) = raw.parse::<i64>() {
        return Value::from(number);
    }
    if (raw.contains('.') || raw.contains('e') || raw.contains('E'))
        && let Ok(number) = raw.parse::<f64>()
        && let Some(number) = serde_json::Number::from_f64(number)
    {
        return Value::Number(number);
    }
    Value::String(raw.to_string())
}

// ---------------------------------------------------------------------------
// Stringify (yaml npm default block style, 2-space indent)
// ---------------------------------------------------------------------------

pub(crate) fn stringify_yaml(frontmatter: &Map<String, Value>) -> String {
    let mut out = String::new();
    stringify_mapping(frontmatter, 0, &mut out);
    out
}

fn stringify_mapping(map: &Map<String, Value>, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for (key, value) in map {
        match value {
            Value::Object(nested) if !nested.is_empty() => {
                out.push_str(&format!("{pad}{key}:\n"));
                stringify_mapping(nested, indent + 2, out);
            }
            Value::Array(items) if !items.is_empty() => {
                out.push_str(&format!("{pad}{key}:\n"));
                for item in items {
                    out.push_str(&format!("{pad}  - {}\n", yaml_scalar(item)));
                }
            }
            Value::Null => out.push_str(&format!("{pad}{key}: null\n")),
            Value::Array(_) => out.push_str(&format!("{pad}{key}: []\n")),
            Value::Object(_) => out.push_str(&format!("{pad}{key}: {{}}\n")),
            scalar => out.push_str(&format!("{pad}{key}: {}\n", yaml_scalar(scalar))),
        }
    }
}

fn yaml_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => quote_if_needed(text),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(yaml_scalar)
                .collect::<Vec<String>>()
                .join(", ")
        ),
        Value::Object(_) => "{}".to_string(),
    }
}

fn quote_if_needed(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    if text.contains('\n') {
        // yaml npm emits block scalars for multiline strings; emit `|-` form.
        let indented = text
            .split('\n')
            .map(|line| format!("  {line}"))
            .collect::<Vec<String>>()
            .join("\n");
        return format!("|-\n{indented}");
    }
    let needs_quotes = text != text.trim()
        || text.starts_with('|')
        || text.starts_with('>')
        || text.starts_with('#')
        || text.starts_with('&')
        || text.starts_with('*')
        || text.starts_with('!')
        || text.starts_with('%')
        || text.starts_with('@')
        || text.starts_with('`')
        || text.starts_with('\'')
        || text.starts_with('"')
        || text.starts_with('-')
        || text.starts_with('?')
        || text.starts_with(':')
        || text.starts_with('[')
        || text.starts_with(']')
        || text.starts_with('{')
        || text.starts_with('}')
        || text.starts_with(',')
        || text.contains(": ")
        || text.contains(" #")
        || matches!(
            coerce_plain(text),
            Value::Bool(_) | Value::Null | Value::Number(_)
        );
    if !needs_quotes {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "''"))
}

/// `writeMdFile` (shared.js): drop null values, stringify, wrap with fences.
/// The JS swallows write errors behind a generic message; callers translate.
pub(crate) fn write_md_file(
    path: &Path,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> std::io::Result<()> {
    let cleaned: Map<String, Value> = frontmatter
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let content = format!("---\n{}---\n\n{body}", stringify_yaml(&cleaned));
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_frontmatter_and_body() {
        let (fm, body) =
            parse_md_content("---\nname: demo\ndescription: A demo skill\n---\n\nDo things.\n");
        assert_eq!(fm.get("name"), Some(&json!("demo")));
        assert_eq!(fm.get("description"), Some(&json!("A demo skill")));
        assert_eq!(body, "Do things.");
    }

    #[test]
    fn closing_fence_at_eof_without_newline() {
        let (fm, body) = parse_md_content("---\nname: demo\n---\nbody");
        assert_eq!(fm.get("name"), Some(&json!("demo")));
        assert_eq!(body, "body");
    }

    #[test]
    fn no_frontmatter_yields_trimmed_body() {
        let (fm, body) = parse_md_content("  just text  \n");
        assert!(fm.is_empty());
        assert_eq!(body, "just text");
    }

    #[test]
    fn inline_and_block_arrays() {
        let value = parse_yaml("aliases: [rev, help]\n").expect("parse");
        assert_eq!(value["aliases"], json!(["rev", "help"]));
        let value = parse_yaml("aliases:\n  - rev\n  - help\n").expect("parse");
        assert_eq!(value["aliases"], json!(["rev", "help"]));
    }

    #[test]
    fn scalar_coercion() {
        let value = parse_yaml("a: 1\nb: true\nc: ~\nd: 1.5\ne: text\n").expect("parse");
        assert_eq!(value["a"], json!(1));
        assert_eq!(value["b"], json!(true));
        assert_eq!(value["c"], Value::Null);
        assert_eq!(value["d"], json!(1.5));
        assert_eq!(value["e"], json!("text"));
    }

    #[test]
    fn colon_values_retry_as_block_scalars() {
        let value = parse_yaml("description: Build agent: creates builds\n").expect("parse");
        assert_eq!(value["description"], json!("Build agent: creates builds"));
    }

    #[test]
    fn nested_maps_round_trip() {
        let value = parse_yaml("metadata:\n  license: MIT\n  count: 2\n").expect("parse");
        assert_eq!(value["metadata"]["license"], json!("MIT"));
        assert_eq!(value["metadata"]["count"], json!(2));
    }

    #[test]
    fn stringify_matches_yaml_npm_shapes() {
        let mut fm = Map::new();
        fm.insert("aliases".into(), json!(["rev", "help"]));
        fm.insert("description".into(), json!("Review helper"));
        assert_eq!(
            stringify_yaml(&fm),
            "aliases:\n  - rev\n  - help\ndescription: Review helper\n"
        );
    }

    #[test]
    fn stringify_quotes_special_scalars() {
        let mut fm = Map::new();
        fm.insert("note".into(), json!("key: value"));
        fm.insert("flag".into(), json!("true"));
        fm.insert("empty".into(), json!(""));
        let out = stringify_yaml(&fm);
        assert!(out.contains("note: 'key: value'"));
        assert!(out.contains("flag: 'true'"));
        assert!(out.contains("empty: ''"));
    }

    #[test]
    fn write_md_file_drops_nulls() {
        let dir = std::env::temp_dir().join(format!("ompchamber-mdyaml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("SKILL.md");
        let mut fm = Map::new();
        fm.insert("name".into(), json!("x"));
        fm.insert("gone".into(), Value::Null);
        write_md_file(&path, &fm, "body").expect("write");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert_eq!(raw, "---\nname: x\n---\n\nbody");
        std::fs::remove_dir_all(&dir).ok();
    }
}
