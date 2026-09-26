//! Frontmatter YAML subset — stands in for the JS `yaml` package
//! (`yaml.parse`/`yaml.stringify`) used by `shared.js` for agent/command
//! markdown frontmatter. No YAML crate is on the dependency allow-list, so
//! this implements the subset that frontmatter actually contains:
//! nested block mappings, block sequences, flow collections, quoted and
//! plain scalars (YAML 1.2 core-schema typing), block scalars (`|`, `|-`,
//! `>`, `>-`) and comments. Anchors, tags, complex keys, and multi-document
//! streams are rejected — none appear in OpenCode frontmatter.

use serde_json::{Map, Value};

#[derive(Debug)]
pub(crate) struct YamlError(pub String);

/// `yaml.parse(text) || {}` — an empty document reads as an empty object.
pub(crate) fn parse_yaml_object(text: &str) -> Result<Map<String, Value>, YamlError> {
    let lines = split_lines(text);
    let mut parser = Parser { lines, pos: 0 };
    if parser.peek().is_none() {
        return Ok(Map::new());
    }
    let value = parser.parse_block(0)?;
    if let Some((indent, raw)) = parser.peek() {
        return Err(YamlError(format!(
            "unexpected content at indent {indent}: {raw}"
        )));
    }
    match value {
        Value::Object(map) => Ok(map),
        other => Err(YamlError(format!(
            "expected a mapping document, found {other:?}"
        ))),
    }
}

/// `yaml.stringify(map)` — two-space indent, block style, trailing newline.
pub(crate) fn stringify_yaml(map: &Map<String, Value>) -> String {
    if map.is_empty() {
        return "{}\n".to_string();
    }
    let mut out = String::new();
    emit_map(map, 0, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct RawLine {
    indent: usize,
    text: String,
}

fn split_lines(text: &str) -> Vec<RawLine> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        lines.push(RawLine {
            indent,
            text: raw.to_string(),
        });
    }
    lines
}

struct Parser {
    lines: Vec<RawLine>,
    pos: usize,
}

impl Parser {
    /// Next meaningful line (blank/comment-only lines are skipped and
    /// consumed); the returned line itself is not consumed.
    fn peek(&mut self) -> Option<(usize, String)> {
        while self.pos < self.lines.len() {
            let line = &self.lines[self.pos];
            let trimmed = line.text.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                self.pos += 1;
                continue;
            }
            let content = line.text[indent_of(&line.text)..].to_string();
            return Some((line.indent, content));
        }
        None
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    /// Raw line at the current position (blank/comment lines included) for
    /// block-scalar consumption.
    fn raw_current(&self) -> Option<&RawLine> {
        self.lines.get(self.pos)
    }

    fn parse_block(&mut self, min_indent: usize) -> Result<Value, YamlError> {
        let Some((indent, content)) = self.peek() else {
            return Ok(Value::Null);
        };
        if indent < min_indent {
            return Ok(Value::Null);
        }
        if content == "-" || content.starts_with("- ") {
            self.parse_sequence(indent)
        } else {
            Ok(Value::Object(self.parse_mapping(indent)?))
        }
    }

    fn parse_sequence(&mut self, indent: usize) -> Result<Value, YamlError> {
        let mut items = Vec::new();
        loop {
            let Some((line_indent, content)) = self.peek() else {
                break;
            };
            if line_indent != indent || !(content == "-" || content.starts_with("- ")) {
                break;
            }
            let rest = content[1..].to_string();
            let rest_trimmed = rest.trim_start_matches(' ');
            if rest_trimmed.is_empty() {
                self.advance();
                items.push(self.parse_block(indent + 1)?);
            } else {
                let inner_indent = indent + (rest.len() - rest_trimmed.len()) + 1;
                // Re-anchor the current line at the item content so the item
                // parses as a nested block (mapping/scalar/flow).
                self.lines[self.pos] = RawLine {
                    indent: inner_indent,
                    text: format!("{}{}", " ".repeat(inner_indent), rest_trimmed),
                };
                let next = self.peek();
                match next {
                    Some((_, item_content)) if is_mapping_start(&item_content) => {
                        items.push(Value::Object(self.parse_mapping(inner_indent)?));
                    }
                    Some((_, item_content)) => {
                        self.advance();
                        items.push(parse_scalar_value(&item_content)?);
                    }
                    None => items.push(Value::Null),
                }
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_mapping(&mut self, indent: usize) -> Result<Map<String, Value>, YamlError> {
        let mut map = Map::new();
        loop {
            let Some((line_indent, content)) = self.peek() else {
                break;
            };
            if line_indent < indent {
                break;
            }
            if line_indent > indent {
                return Err(YamlError(format!(
                    "unexpected indentation at line: {content}"
                )));
            }
            if content == "-" || content.starts_with("- ") {
                break;
            }
            let (key, rest) = split_key(&content)?;
            self.advance();
            let value = if rest.is_empty() {
                // `key:` — nested block, same-indent sequence, or null.
                match self.peek() {
                    Some((next_indent, _)) if next_indent > indent => {
                        self.parse_block(indent + 1)?
                    }
                    Some((next_indent, next_content))
                        if next_indent == indent
                            && (next_content == "-" || next_content.starts_with("- ")) =>
                    {
                        self.parse_sequence(indent)?
                    }
                    _ => Value::Null,
                }
            } else if rest.starts_with('|') || rest.starts_with('>') {
                self.parse_block_scalar(&rest, indent)?
            } else {
                parse_scalar_value(&rest)?
            };
            map.insert(key, value);
        }
        Ok(map)
    }

    /// Block scalar (`|`/`>` with optional `-`/`+` chomping): consume the
    /// following lines indented deeper than the key.
    fn parse_block_scalar(&mut self, header: &str, key_indent: usize) -> Result<Value, YamlError> {
        let folded = header.starts_with('>');
        let chomp_strip = header.contains('-');
        let chomp_keep = header.contains('+');
        let mut raw_lines: Vec<String> = Vec::new();
        // YAML block-scalar indentation is the indent of the first
        // non-empty content line, not the parent key's indent + 1.
        let mut block_indent: Option<usize> = None;
        while let Some(line) = self.raw_current() {
            let is_blank = line.text.trim().is_empty();
            let deeper = line.indent > key_indent;
            if !is_blank && !deeper {
                break;
            }
            if is_blank && !deeper {
                let mut lookahead = self.pos + 1;
                let mut trailing_blank = true;
                while lookahead < self.lines.len() {
                    let next = &self.lines[lookahead];
                    if next.text.trim().is_empty() {
                        lookahead += 1;
                        continue;
                    }
                    trailing_blank = next.indent > key_indent;
                    break;
                }
                if trailing_blank {
                    raw_lines.push(String::new());
                    self.pos += 1;
                    continue;
                }
                break;
            }
            if block_indent.is_none() && !is_blank {
                block_indent = Some(line.indent);
            }
            let content_indent = block_indent.unwrap_or(key_indent + 1);
            let text = &line.text;
            let stripped = if text.len() >= content_indent {
                text[content_indent..].to_string()
            } else {
                text.trim_start_matches(' ').to_string()
            };
            raw_lines.push(stripped);
            self.pos += 1;
        }
        let mut body = if folded {
            raw_lines.join(" ")
        } else {
            raw_lines.join("\n")
        };
        if !chomp_strip && !body.is_empty() {
            body.push('\n');
        }
        if chomp_keep {
            body.push('\n');
        }
        Ok(Value::String(body))
    }
}

fn indent_of(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

fn is_mapping_start(content: &str) -> bool {
    split_key(content).is_ok()
}

/// Split `key: rest` respecting quoted keys; the colon must be followed by a
/// space or end of line (YAML plain-key rule).
fn split_key(content: &str) -> Result<(String, String), YamlError> {
    let bytes = content.as_bytes();
    let mut i = 0;
    let quote = if bytes.first() == Some(&b'"') || bytes.first() == Some(&b'\'') {
        Some(bytes[0])
    } else {
        None
    };
    if let Some(q) = quote {
        i = 1;
        while i < bytes.len() {
            if bytes[i] == q {
                if q == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                if q == b'"' && bytes.get(i.wrapping_sub(1)) == Some(&b'\\') {
                    i += 1;
                    continue;
                }
                break;
            }
            i += 1;
        }
        if i >= bytes.len() {
            return Err(YamlError(format!("unterminated quoted key: {content}")));
        }
        let key = unquote_scalar(&content[..=i])?;
        let rest = content[i + 1..].trim().to_string();
        if !rest.starts_with(':') {
            return Err(YamlError(format!("missing colon after key: {content}")));
        }
        let value = rest[1..].trim().to_string();
        return Ok((key, value));
    }
    let mut colon: Option<usize> = None;
    while i < bytes.len() {
        if bytes[i] == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            colon = Some(i);
            break;
        }
        i += 1;
    }
    let Some(colon) = colon else {
        return Err(YamlError(format!("missing colon: {content}")));
    };
    let key = content[..colon].trim().to_string();
    if key.is_empty() {
        return Err(YamlError(format!("empty key: {content}")));
    }
    Ok((key, content[colon + 1..].trim().to_string()))
}

/// Parse a scalar/flow value from the text after `key: `.
fn parse_scalar_value(text: &str) -> Result<Value, YamlError> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return parse_flow(trimmed);
    }
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        return Ok(Value::String(unquote_scalar(trimmed)?));
    }
    // Strip a trailing comment (` #...`).
    let scalar = strip_trailing_comment(trimmed);
    if scalar.is_empty() {
        return Ok(Value::Null);
    }
    if scalar == "~" || scalar.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if scalar.eq_ignore_ascii_case("true") {
        return Ok(Value::Bool(true));
    }
    if scalar.eq_ignore_ascii_case("false") {
        return Ok(Value::Bool(false));
    }
    if looks_like_integer(scalar)
        && let Ok(parsed) = scalar.parse::<i64>()
    {
        return Ok(Value::Number(parsed.into()));
    }
    if looks_like_float(scalar)
        && let Ok(parsed) = scalar.parse::<f64>()
        && let Some(number) = serde_json::Number::from_f64(parsed)
    {
        return Ok(Value::Number(number));
    }
    // A plain scalar cannot contain `: ` (that is YAML's mapping cue) —
    // reject so the caller can retry with `sanitizeFrontmatter`.
    if scalar.contains(": ") || scalar.ends_with(':') {
        return Err(YamlError(format!(
            "plain scalar contains a colon: {scalar}"
        )));
    }
    Ok(Value::String(scalar.to_string()))
}

fn strip_trailing_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut in_double = false;
    let mut in_single = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' if !in_single => in_double = !in_double,
            b'\'' if !in_double => in_single = !in_single,
            b'#' if !in_double
                && !in_single
                && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') =>
            {
                return text[..i].trim_end();
            }
            _ => {}
        }
        i += 1;
    }
    text
}

fn looks_like_integer(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

fn looks_like_float(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    if body.is_empty() {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    let mut saw_exp = false;
    for (index, ch) in body.char_indices() {
        match ch {
            '0'..='9' => saw_digit = true,
            '.' if !saw_dot && !saw_exp => saw_dot = true,
            'e' | 'E' if saw_digit && !saw_exp => saw_exp = true,
            '-' | '+'
                if saw_exp
                    && matches!(
                        body.as_bytes().get(index.wrapping_sub(1)),
                        Some(b'e') | Some(b'E')
                    ) => {}
            _ => return false,
        }
    }
    saw_digit && (saw_dot || saw_exp)
}

fn unquote_scalar(text: &str) -> Result<String, YamlError> {
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'\'') {
        let inner = text
            .strip_prefix('\'')
            .and_then(|t| t.strip_suffix('\''))
            .ok_or_else(|| YamlError(format!("unterminated single quote: {text}")))?;
        return Ok(inner.replace("''", "'"));
    }
    let inner = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .ok_or_else(|| YamlError(format!("unterminated double quote: {text}")))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let code = u32::from_str_radix(&hex, 16)
                    .map_err(|_| YamlError(format!("bad \\u escape: {text}")))?;
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            Some(other) => {
                return Err(YamlError(format!("unsupported escape \\{other}")));
            }
            None => return Err(YamlError("trailing backslash".to_string())),
        }
    }
    Ok(out)
}

/// Flow collections (`[a, b]`, `{k: v}`) with JSON-ish scalars inside.
fn parse_flow(text: &str) -> Result<Value, YamlError> {
    let mut parser = FlowParser {
        chars: text.chars().collect(),
        pos: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(YamlError(format!("trailing flow content: {text}")));
    }
    Ok(value)
}

struct FlowParser {
    chars: Vec<char>,
    pos: usize,
}

impl FlowParser {
    fn skip_ws(&mut self) {
        while matches!(self.chars.get(self.pos), Some(' ') | Some('\t')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Value, YamlError> {
        self.skip_ws();
        match self.chars.get(self.pos) {
            Some('[') => self.parse_array(),
            Some('{') => self.parse_map(),
            _ => self.parse_scalar(),
        }
    }

    fn parse_array(&mut self) -> Result<Value, YamlError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.chars.get(self.pos) {
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(YamlError("unterminated flow array".to_string())),
                _ => {}
            }
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.chars.get(self.pos) {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(YamlError("expected , or ] in flow array".to_string())),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_map(&mut self) -> Result<Value, YamlError> {
        self.pos += 1;
        let mut map = Map::new();
        loop {
            self.skip_ws();
            match self.chars.get(self.pos) {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(YamlError("unterminated flow map".to_string())),
                _ => {}
            }
            let key = self.parse_scalar_string()?;
            self.skip_ws();
            if self.chars.get(self.pos) != Some(&':') {
                return Err(YamlError("expected : in flow map".to_string()));
            }
            self.pos += 1;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.chars.get(self.pos) {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(YamlError("expected , or } in flow map".to_string())),
            }
        }
        Ok(Value::Object(map))
    }

    fn parse_scalar(&mut self) -> Result<Value, YamlError> {
        let text = self.parse_scalar_string()?;
        parse_scalar_value(&text)
    }

    fn parse_scalar_string(&mut self) -> Result<String, YamlError> {
        self.skip_ws();
        let start = self.pos;
        match self.chars.get(self.pos) {
            Some('"') | Some('\'') => {
                let quote = self.chars[self.pos];
                self.pos += 1;
                while let Some(&ch) = self.chars.get(self.pos) {
                    if ch == quote {
                        if quote == '\'' && self.chars.get(self.pos + 1) == Some(&'\'') {
                            self.pos += 2;
                            continue;
                        }
                        self.pos += 1;
                        let text: String = self.chars[start..self.pos].iter().collect();
                        return Ok(text);
                    }
                    self.pos += 1;
                }
                Err(YamlError("unterminated quoted flow scalar".to_string()))
            }
            _ => {
                while let Some(&ch) = self.chars.get(self.pos) {
                    if ch == ',' || ch == '}' || ch == ']' || ch == ':' {
                        break;
                    }
                    self.pos += 1;
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                Ok(text.trim().to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

fn emit_map(map: &Map<String, Value>, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for (key, value) in map {
        let key_text = emit_scalar_string(key);
        match value {
            Value::Object(nested) if !nested.is_empty() => {
                out.push_str(&format!("{pad}{key_text}:\n"));
                emit_map(nested, indent + 2, out);
            }
            Value::Object(_) => out.push_str(&format!("{pad}{key_text}: {{}}\n")),
            Value::Array(items) if !items.is_empty() => {
                out.push_str(&format!("{pad}{key_text}:\n"));
                for item in items {
                    emit_sequence_item(item, indent + 2, out);
                }
            }
            Value::Array(_) => out.push_str(&format!("{pad}{key_text}: []\n")),
            scalar => out.push_str(&format!("{pad}{key_text}: {}\n", emit_inline(scalar))),
        }
    }
}

fn emit_sequence_item(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(nested) if !nested.is_empty() => {
            let mut nested_out = String::new();
            emit_map(nested, indent + 2, &mut nested_out);
            let mut lines = nested_out.lines();
            if let Some(first) = lines.next() {
                out.push_str(&format!("{pad}- {}\n", first.trim_start()));
            }
            for line in lines {
                out.push_str(&format!("{pad}  {}\n", line.trim_start()));
            }
        }
        Value::Object(_) => out.push_str(&format!("{pad}- {{}}\n")),
        Value::Array(items) if !items.is_empty() => {
            out.push_str(&format!("{pad}-\n"));
            for item in items {
                emit_sequence_item(item, indent + 2, out);
            }
        }
        Value::Array(_) => out.push_str(&format!("{pad}- []\n")),
        scalar => out.push_str(&format!("{pad}- {}\n", emit_inline(scalar))),
    }
}

fn emit_inline(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => emit_scalar_string(text),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(emit_inline).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", emit_scalar_string(k), emit_inline(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn emit_scalar_string(text: &str) -> String {
    if is_safe_plain_scalar(text) {
        text.to_string()
    } else {
        double_quote(text)
    }
}

fn is_safe_plain_scalar(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let first = text.chars().next().unwrap_or(' ');
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if text.trim() != text {
        return false;
    }
    if text.contains(": ") || text.ends_with(':') || text.contains(" #") {
        return false;
    }
    if text.contains('\n') || text.contains('\r') || text.contains('\t') {
        return false;
    }
    if text.chars().any(|c| c.is_control()) {
        return false;
    }
    if text == "~" || text.eq_ignore_ascii_case("null") {
        return false;
    }
    if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("false") {
        return false;
    }
    if looks_like_integer(text) || looks_like_float(text) {
        return false;
    }
    if text.starts_with("---") || text.starts_with("...") {
        return false;
    }
    true
}

fn double_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_map(text: &str) -> Map<String, Value> {
        parse_yaml_object(text).expect("parse")
    }

    #[test]
    fn parses_standard_frontmatter() {
        let map = parse_map(
            "description: My build agent\nmodel: anthropic/claude-sonnet-4\nmode: primary\n",
        );
        assert_eq!(map.get("description"), Some(&json!("My build agent")));
        assert_eq!(map.get("model"), Some(&json!("anthropic/claude-sonnet-4")));
        assert_eq!(map.get("mode"), Some(&json!("primary")));
    }

    #[test]
    fn parses_nested_maps_numbers_and_bools() {
        let map =
            parse_map("temperature: 0.7\npermission:\n  edit:\n    commit: true\n  bash: deny\n");
        assert_eq!(map.get("temperature"), Some(&json!(0.7)));
        assert_eq!(
            map.get("permission"),
            Some(&json!({"edit": {"commit": true}, "bash": "deny"}))
        );
    }

    #[test]
    fn parses_sequences_same_indent_and_nested() {
        let map = parse_map("tools:\n- write\n- edit\nother:\n  - a\n  - b\n");
        assert_eq!(map.get("tools"), Some(&json!(["write", "edit"])));
        assert_eq!(map.get("other"), Some(&json!(["a", "b"])));
    }

    #[test]
    fn parses_flow_collections_and_quoted_scalars() {
        let map = parse_map("a: {x: 1, y: \"two\"}\nb: [1, 'q']\nc: \"a: b\"\n");
        assert_eq!(map.get("a"), Some(&json!({"x": 1, "y": "two"})));
        assert_eq!(map.get("b"), Some(&json!([1, "q"])));
        assert_eq!(map.get("c"), Some(&json!("a: b")));
    }

    #[test]
    fn parses_block_scalars() {
        let map = parse_map("description: |-\n  Build agent: creates builds\nmodel: x\n");
        assert_eq!(
            map.get("description"),
            Some(&json!("Build agent: creates builds"))
        );
    }

    #[test]
    fn rejects_unquoted_colons_in_plain_scalars() {
        assert!(parse_yaml_object("description: Build agent: creates builds\n").is_err());
    }

    #[test]
    fn empty_document_is_empty_object() {
        assert!(parse_map("").is_empty());
        assert!(parse_map("\n  \n# only a comment\n").is_empty());
    }

    #[test]
    fn stringify_round_trips() {
        let mut map = Map::new();
        map.insert("description".into(), json!("My build agent"));
        map.insert("model".into(), json!("openai/gpt-5"));
        map.insert("temperature".into(), json!(0.7));
        map.insert("flag".into(), json!(true));
        map.insert("nothing".into(), Value::Null);
        map.insert(
            "permission".into(),
            json!({"edit": {"commit": true}, "webfetch": "deny"}),
        );
        map.insert("tools".into(), json!(["write", "edit"]));
        let text = stringify_yaml(&map);
        // serde_json maps are BTreeMaps, so emission order is alphabetical
        // (JS yaml.stringify preserves insertion order; the round-trip value
        // equality is what the frontmatter contract needs).
        assert_eq!(
            text,
            "description: My build agent\nflag: true\nmodel: openai/gpt-5\nnothing: null\npermission:\n  edit:\n    commit: true\n  webfetch: deny\ntemperature: 0.7\ntools:\n  - write\n  - edit\n"
        );
        let reparsed = parse_map(&text);
        assert_eq!(Value::Object(reparsed), Value::Object(map));
    }

    #[test]
    fn stringify_empty_map_matches_js_yaml() {
        assert_eq!(stringify_yaml(&Map::new()), "{}\n");
    }

    #[test]
    fn stringify_quotes_ambiguous_scalars() {
        let mut map = Map::new();
        map.insert("a".into(), json!("true"));
        map.insert("b".into(), json!("123"));
        map.insert("c".into(), json!(""));
        map.insert("d".into(), json!("has: colon"));
        let text = stringify_yaml(&map);
        assert!(text.contains("a: \"true\""));
        assert!(text.contains("b: \"123\""));
        assert!(text.contains("c: \"\""));
        assert!(text.contains("d: \"has: colon\""));
        let reparsed = parse_map(&text);
        assert_eq!(reparsed.get("a"), Some(&json!("true")));
        assert_eq!(reparsed.get("b"), Some(&json!("123")));
        assert_eq!(reparsed.get("c"), Some(&json!("")));
    }
}
