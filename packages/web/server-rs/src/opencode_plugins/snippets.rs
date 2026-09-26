//! Full port of `server/lib/opencode/snippets.js`.
//!
//! NOTE: `scheduled_tasks::snippets` already ports the `expandSnippets` path
//! this file shares (dispatch-only subset). This module is the complete
//! library — listing, CRUD, and expansion — so the opencode surface owns the
//! canonical copy; consolidate on this one when scheduled-tasks next changes
//! (tracked in PORT-MANIFEST.md).
//!
//! Snippets are `#name`-addressable markdown files in
//! `~/.config/opencode/snippets` (preferred) / `snippet` and
//! `<project>/.opencode/snippets` / `snippet`, with `aliases` frontmatter and
//! `<prepend>`/`<append>`/`<inject>` blocks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::md_yaml::{parse_yaml, stringify_yaml};

const SNIPPET_NAME_MAX: usize = 80;
const MAX_EXPANSION_COUNT: usize = 15;

fn opencode_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("opencode")
}

fn global_snippet_dirs(home: &Path) -> Vec<PathBuf> {
    let config_dir = opencode_config_dir(home);
    // ALT (`snippets`) is preferred for writes/reads; singular is the legacy.
    vec![config_dir.join("snippets"), config_dir.join("snippet")]
}

fn project_snippet_dirs(working_directory: &Path) -> Vec<PathBuf> {
    vec![
        working_directory.join(".opencode").join("snippets"),
        working_directory.join(".opencode").join("snippet"),
    ]
}

fn load_dirs(working_directory: Option<&Path>, home: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut dirs: Vec<(PathBuf, &'static str)> = global_snippet_dirs(home)
        .into_iter()
        .map(|dir| (dir, "global"))
        .collect();
    if let Some(working_directory) = working_directory {
        dirs.extend(
            project_snippet_dirs(working_directory)
                .into_iter()
                .map(|dir| (dir, "project")),
        );
    }
    dirs
}

/// `/^[a-z0-9][a-z0-9_-]{0,79}$/i`.
fn snippet_name_ok(name: &str) -> bool {
    let count = name.chars().count();
    if count == 0 || count > SNIPPET_NAME_MAX {
        return false;
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

fn assert_valid_snippet_name(name: &str) -> Result<(), String> {
    if !snippet_name_ok(name) {
        return Err("Snippet name must use letters, numbers, dashes, or underscores".to_string());
    }
    Ok(())
}

/// snippets.js `parseMarkdownFile` — the closing fence must be followed by a
/// (optional) newline: `/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/`.
fn parse_markdown_content(content: &str) -> (Map<String, Value>, String) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (Map::new(), content.trim().to_string());
    };
    let bytes = rest.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let next_nl = rest[cursor..]
            .find('\n')
            .map(|idx| cursor + idx)
            .unwrap_or(bytes.len());
        let line = rest[cursor..next_nl]
            .strip_suffix('\r')
            .unwrap_or(&rest[cursor..next_nl]);
        if line == "---" && cursor > 0 {
            let frontmatter_src = &rest[..cursor];
            let mut after = if next_nl < bytes.len() {
                &rest[next_nl + 1..]
            } else {
                ""
            };
            // `\r?\n?` — the optional trailing newline after the fence is
            // already consumed above; strip one leading `\r` remnant.
            if after.starts_with('\r') {
                after = &after[1..];
            }
            let frontmatter = parse_yaml(frontmatter_src)
                .ok()
                .and_then(|value| match value {
                    Value::Object(map) => Some(map),
                    _ => None,
                })
                .unwrap_or_default();
            return (frontmatter, after.trim().to_string());
        }
        if next_nl >= bytes.len() {
            break;
        }
        cursor = next_nl + 1;
    }
    (Map::new(), content.trim().to_string())
}

fn normalize_aliases(frontmatter: &Map<String, Value>) -> Vec<String> {
    let raw = frontmatter
        .get("aliases")
        .or_else(|| frontmatter.get("alias"));
    let Some(raw) = raw else {
        return Vec::new();
    };
    let items: Vec<&Value> = match raw {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(text) => text.trim().to_string(),
            other => other.to_string(),
        })
        .filter(|alias| !alias.is_empty())
        .collect()
}

/// `writeMarkdownFile` — aliases then description, fenced.
fn write_markdown_file(
    file_path: &Path,
    content: &Value,
    aliases: &[String],
    description: Option<&Value>,
) -> std::io::Result<()> {
    let mut frontmatter = Map::new();
    let normalized_aliases: Vec<String> = aliases
        .iter()
        .map(|alias| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
        .collect();
    if !normalized_aliases.is_empty() {
        frontmatter.insert(
            "aliases".into(),
            Value::Array(
                normalized_aliases
                    .iter()
                    .map(|alias| Value::from(alias.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(description) = description.and_then(Value::as_str)
        && !description.trim().is_empty()
    {
        frontmatter.insert("description".into(), Value::from(description.trim()));
    }

    let body = content.as_str().unwrap_or("");
    let output = if !frontmatter.is_empty() {
        format!(
            "---\n{}---\n{}",
            stringify_yaml(&frontmatter),
            if body.is_empty() {
                String::new()
            } else {
                format!("\n{body}")
            }
        )
    } else {
        body.to_string()
    };
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file_path, output)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Snippet {
    pub name: String,
    pub content: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub file_path: PathBuf,
    pub source: &'static str,
}

impl Snippet {
    pub(crate) fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), Value::from(self.name.clone()));
        map.insert("content".into(), Value::from(self.content.clone()));
        map.insert(
            "aliases".into(),
            Value::Array(
                self.aliases
                    .iter()
                    .map(|a| Value::from(a.clone()))
                    .collect(),
            ),
        );
        if let Some(description) = &self.description {
            map.insert("description".into(), Value::from(description.clone()));
        }
        map.insert(
            "filePath".into(),
            Value::from(self.file_path.to_string_lossy().as_ref()),
        );
        map.insert("source".into(), Value::from(self.source));
        Value::Object(map)
    }
}

#[derive(Default)]
struct Registry {
    by_key: HashMap<String, Snippet>,
    canonical_names: HashSet<String>,
}

impl Registry {
    fn remove_snippet_aliases(&mut self, snippet: &Snippet) {
        for alias in &snippet.aliases {
            let alias_key = alias.to_lowercase();
            if self.canonical_names.contains(&alias_key) {
                continue;
            }
            if let Some(existing) = self.by_key.get(&alias_key)
                && existing.file_path == snippet.file_path
                && existing.name == snippet.name
            {
                self.by_key.remove(&alias_key);
            }
        }
    }

    fn register(&mut self, snippet: Snippet) {
        let key = snippet.name.to_lowercase();
        if let Some(existing) = self.by_key.get(&key).cloned()
            && existing.name.to_lowercase() == key
        {
            self.remove_snippet_aliases(&existing);
        }
        let aliases = snippet.aliases.clone();
        self.by_key.insert(key.clone(), snippet);
        self.canonical_names.insert(key.clone());
        for alias in aliases {
            let alias_key = alias.to_lowercase();
            if snippet_name_ok(&alias) && !self.canonical_names.contains(&alias_key) {
                self.by_key
                    .insert(alias_key, self.by_key.get(&key).cloned().unwrap());
            }
        }
    }
}

fn load_snippet_file(dir: &Path, filename: &str, source: &'static str) -> Option<Snippet> {
    let name = filename.strip_suffix(".md")?;
    if !snippet_name_ok(name) {
        return None;
    }
    let file_path = dir.join(filename);
    let raw = std::fs::read_to_string(&file_path).ok()?;
    let (frontmatter, body) = parse_markdown_content(&raw);
    let description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Snippet {
        name: name.to_string(),
        content: body,
        aliases: normalize_aliases(&frontmatter),
        description,
        file_path,
        source,
    })
}

fn load_snippet_registry(working_directory: Option<&Path>, home: &Path) -> Registry {
    let mut registry = Registry::default();
    for (dir, source) in load_dirs(working_directory, home) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for filename in names {
            if !filename.ends_with(".md") {
                continue;
            }
            match load_snippet_file(&dir, &filename, source) {
                Some(snippet) => registry.register(snippet),
                None => tracing::warn!(
                    "[Snippets] Failed to load {}",
                    dir.join(&filename).display()
                ),
            }
        }
    }
    registry
}

/// `listSnippets` — unique by `(source, filePath)`, name-sorted.
pub(crate) fn list_snippets(working_directory: Option<&Path>, home: &Path) -> Vec<Snippet> {
    let registry = load_snippet_registry(working_directory, home);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut snippets = Vec::new();
    for snippet in registry.by_key.into_values() {
        let key = (
            snippet.source.to_string(),
            snippet.file_path.to_string_lossy().into_owned(),
        );
        if !seen.insert(key) {
            continue;
        }
        snippets.push(snippet);
    }
    snippets.sort_by(|a, b| js_locale_compare(&a.name, &b.name));
    snippets
}

/// `String.prototype.localeCompare` approximation: case-insensitive primary,
/// lowercase tie-break.
fn js_locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let la = a.to_lowercase();
    let lb = b.to_lowercase();
    la.cmp(&lb).then_with(|| a.cmp(b))
}

pub(crate) fn get_snippet(
    name: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<Option<Snippet>, String> {
    assert_valid_snippet_name(name)?;
    let registry = load_snippet_registry(working_directory, home);
    Ok(registry.by_key.get(&name.to_lowercase()).cloned())
}

fn writable_snippet_dir(
    scope: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<PathBuf, String> {
    if scope == "project" {
        let Some(working_directory) = working_directory else {
            return Err("Project directory is required for project snippets".to_string());
        };
        let preferred = working_directory.join(".opencode").join("snippet");
        let alternate = working_directory.join(".opencode").join("snippets");
        return Ok(if alternate.exists() && !preferred.exists() {
            alternate
        } else {
            preferred
        });
    }
    let config_dir = opencode_config_dir(home);
    let alt = config_dir.join("snippets");
    let standard = config_dir.join("snippet");
    Ok(if alt.exists() && !standard.exists() {
        alt
    } else {
        standard
    })
}

pub(crate) fn create_snippet(
    name: &str,
    config: &Value,
    working_directory: Option<&Path>,
    scope: &str,
    home: &Path,
) -> Result<Snippet, String> {
    assert_valid_snippet_name(name)?;
    let dir = writable_snippet_dir(scope, working_directory, home)?;
    let file_path = dir.join(format!("{name}.md"));
    if file_path.exists() {
        return Err(format!("Snippet \"{name}\" already exists"));
    }
    write_snippet_config(&file_path, config).map_err(|error| error.to_string())?;
    get_snippet(name, working_directory, home)?
        .ok_or_else(|| format!("Snippet \"{name}\" not found"))
}

fn write_snippet_config(file_path: &Path, config: &Value) -> std::io::Result<()> {
    let aliases: Vec<String> = config
        .get("aliases")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.trim().to_string(),
                    other => other.to_string(),
                })
                .filter(|alias| !alias.is_empty())
                .collect()
        })
        .unwrap_or_default();
    write_markdown_file(
        file_path,
        config.get("content").unwrap_or(&Value::Null),
        &aliases,
        config.get("description"),
    )
}

pub(crate) fn update_snippet(
    name: &str,
    updates: &Value,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<Snippet, String> {
    let existing = get_snippet(name, working_directory, home)?
        .ok_or_else(|| format!("Snippet \"{name}\" not found"))?;
    // `{ ...existing, ...(updates || {}) }` — updates win key-wise.
    let mut merged = existing.to_value();
    if let Some(updates) = updates.as_object() {
        for (key, value) in updates {
            merged
                .as_object_mut()
                .expect("existing serializes to object")
                .insert(key.clone(), value.clone());
        }
    }
    write_snippet_config(&existing.file_path, &merged).map_err(|error| error.to_string())?;
    get_snippet(name, working_directory, home)?
        .ok_or_else(|| format!("Snippet \"{name}\" not found"))
}

pub(crate) fn delete_snippet(
    name: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<(), String> {
    let existing = get_snippet(name, working_directory, home)?
        .ok_or_else(|| format!("Snippet \"{name}\" not found"))?;
    std::fs::remove_file(&existing.file_path).map_err(|error| error.to_string())
}

#[derive(Debug, Default)]
struct Blocks {
    prepend: Vec<String>,
    append: Vec<String>,
}

/// `expandSnippets(text, workingDirectory)` — expand `#hashtag` references,
/// collect prepend/append blocks, join with blank lines.
pub(crate) fn expand_snippets(text: &str, working_directory: Option<&Path>, home: &Path) -> String {
    let registry = load_snippet_registry(working_directory, home);
    let snippets: HashMap<String, Snippet> = registry.by_key;
    let mut collector = Blocks::default();
    let expanded = expand_text(text, &snippets, &mut HashMap::new(), &mut collector)
        .trim()
        .to_string();
    collector.prepend.push(expanded);
    collector
        .prepend
        .into_iter()
        .chain(collector.append)
        .filter(|part| !part.is_empty())
        .collect::<Vec<String>>()
        .join("\n\n")
}

/// `expandText` — fixed-point hashtag replacement with a loop guard.
fn expand_text(
    text: &str,
    registry: &HashMap<String, Snippet>,
    counts: &mut HashMap<String, usize>,
    collector: &mut Blocks,
) -> String {
    let mut expanded = text.to_string();
    loop {
        let previous = expanded.clone();
        let mut loop_detected = false;
        expanded = replace_hashtags(&expanded, registry, counts, &mut loop_detected, collector);
        if expanded == previous || loop_detected {
            break;
        }
    }
    expanded
}

fn replace_hashtags(
    text: &str,
    registry: &HashMap<String, Snippet>,
    counts: &mut HashMap<String, usize>,
    loop_detected: &mut bool,
    collector: &mut Blocks,
) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch != '#' {
            out.push(ch);
            index += 1;
            continue;
        }
        // Hashtag name: [a-zA-Z0-9_-]+ (the JS regex is case-insensitive).
        let mut end = index + 1;
        while end < chars.len()
            && (chars[end].is_ascii_alphanumeric() || chars[end] == '_' || chars[end] == '-')
        {
            end += 1;
        }
        if end == index + 1 {
            out.push(ch);
            index += 1;
            continue;
        }
        let name: String = chars[index + 1..end].iter().collect();
        let input = &chars[end..];
        if name.to_lowercase() == "skill" && input.first() == Some(&'(') {
            out.push('#');
            out.push_str(&name);
            index = end;
            continue;
        }
        let Some(snippet) = registry.get(&name.to_lowercase()) else {
            out.push('#');
            out.push_str(&name);
            index = end;
            continue;
        };
        let key = snippet.name.to_lowercase();
        let count = counts.get(&key).copied().unwrap_or(0) + 1;
        if count > MAX_EXPANSION_COUNT {
            *loop_detected = true;
            out.push('#');
            out.push_str(&name);
            index = end;
            continue;
        }
        counts.insert(key, count);

        let (inline, mut blocks) = parse_snippet_blocks(&snippet.content);
        for block in std::mem::take(&mut blocks.prepend) {
            let expanded = expand_text(&block, registry, counts, collector);
            collector.prepend.push(expanded);
        }
        for block in std::mem::take(&mut blocks.append) {
            let expanded = expand_text(&block, registry, counts, collector);
            collector.append.push(expanded);
        }
        out.push_str(&expand_text(&inline, registry, counts, collector));
        index = end;
    }
    out
}

/// `parseSnippetBlocks`: extract `<prepend>`/`<append>` blocks in order
/// (unclosed tags run to end-of-string), drop `<inject>` blocks, trim.
fn parse_snippet_blocks(content: &str) -> (String, Blocks) {
    let mut blocks = Blocks::default();
    let after_prepend = replace_tag_blocks(content, "prepend", &mut blocks);
    let after_append = replace_tag_blocks(&after_prepend, "append", &mut blocks);
    let stripped = strip_tag_blocks(&after_append, "inject");
    (stripped.trim().to_string(), blocks)
}

fn replace_tag_blocks(input: &str, tag: &str, blocks: &mut Blocks) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = find_case_insensitive(rest, &open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        if let Some(end) = find_case_insensitive(after_open, &close) {
            let value = after_open[..end].trim();
            if !value.is_empty() {
                push_block(blocks, tag, value);
            }
            rest = &after_open[end + close.len()..];
        } else {
            let value = after_open.trim();
            if !value.is_empty() {
                push_block(blocks, tag, value);
            }
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn push_block(blocks: &mut Blocks, tag: &str, value: &str) {
    if tag == "prepend" {
        blocks.prepend.push(value.to_string());
    } else {
        blocks.append.push(value.to_string());
    }
}

fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = find_case_insensitive(rest, &open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        if let Some(end) = find_case_insensitive(after_open, &close) {
            rest = &after_open[end + close.len()..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n_lower: Vec<u8> = needle
        .as_bytes()
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    if n_lower.is_empty() {
        return Some(0);
    }
    (0..h.len().saturating_sub(n_lower.len() - 1)).find(|&index| {
        h[index..index + n_lower.len()]
            .iter()
            .zip(&n_lower)
            .all(|(byte, lower)| byte.to_ascii_lowercase() == *lower)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-snippets-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    fn write_snippet(project: &Path, relative: &str, content: &str) {
        let path = project.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }
    #[test]
    fn loads_project_snippets_with_aliases_and_description() {
        let root = temp_root("aliases");
        let project = root.join("project");
        write_snippet(
            &project,
            ".opencode/snippet/review.md",
            "---\naliases: [rev]\ndescription: Review helper\n---\nReview carefully.",
        );

        let listed = list_snippets(Some(&project), &root);
        let review = listed.iter().find(|s| s.name == "review").expect("listed");
        assert_eq!(review.aliases, ["rev"]);
        assert_eq!(review.description.as_deref(), Some("Review helper"));
        assert_eq!(review.source, "project");

        let alias = get_snippet("rev", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(alias.name, "review");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn snippet_dir_wins_over_snippets_dir() {
        let root = temp_root("precedence");
        let project = root.join("project");
        write_snippet(&project, ".opencode/snippets/same.md", "Old");
        write_snippet(&project, ".opencode/snippet/same.md", "New");

        let same = get_snippet("same", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(same.content, "New");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn canonical_names_beat_colliding_aliases() {
        let root = temp_root("canonical");
        let project = root.join("project");
        write_snippet(&project, ".opencode/snippets/review.md", "Review by name");
        write_snippet(
            &project,
            ".opencode/snippet/helper.md",
            "---\naliases: [review, help]\n---\nReview by alias",
        );

        let review = get_snippet("review", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(
            (review.name.as_str(), review.content.as_str()),
            ("review", "Review by name")
        );
        let help = get_snippet("help", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(
            (help.name.as_str(), help.content.as_str()),
            ("helper", "Review by alias")
        );
        assert_eq!(
            expand_snippets("Use #review and #help", Some(&project), &root),
            "Use Review by name and Review by alias"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn replacement_drops_earlier_alias_registrations() {
        let root = temp_root("replace");
        let project = root.join("project");
        write_snippet(
            &project,
            ".opencode/snippet/alias-first.md",
            "---\naliases: [review, assist]\n---\nReview by alias",
        );
        write_snippet(&project, ".opencode/snippet/review.md", "Review by name");

        let review = get_snippet("review", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(review.content, "Review by name");
        let assist = get_snippet("assist", Some(&project), &root)
            .expect("lookup")
            .expect("found");
        assert_eq!(
            (assist.name.as_str(), assist.content.as_str()),
            ("alias-first", "Review by alias")
        );
        assert_eq!(
            expand_snippets("Use #review and #assist", Some(&project), &root),
            "Use Review by name and Review by alias"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn create_update_delete_round_trip() {
        let root = temp_root("crud");
        let project = root.join("project");
        let created = create_snippet(
            "custom-one",
            &json!({ "content": "Body", "aliases": ["co"] }),
            Some(&project),
            "project",
            &root,
        )
        .expect("create");
        assert_eq!(created.name, "custom-one");
        assert_eq!(created.content, "Body");
        assert_eq!(created.aliases, ["co"]);

        let updated = update_snippet(
            "custom-one",
            &json!({ "content": "Updated" }),
            Some(&project),
            &root,
        )
        .expect("update");
        assert_eq!(updated.content, "Updated");
        assert_eq!(updated.aliases, ["co"]);

        delete_snippet("custom-one", Some(&project), &root).expect("delete");
        assert!(
            get_snippet("custom-one", Some(&project), &root)
                .expect("lookup")
                .is_none()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_invalid_names() {
        let root = temp_root("invalid");
        let project = root.join("project");
        let error = create_snippet(
            "../bad",
            &json!({ "content": "" }),
            Some(&project),
            "project",
            &root,
        )
        .expect_err("must throw");
        assert!(error.contains("Snippet name"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn expands_recursively_with_prepend_and_append() {
        let root = temp_root("expand");
        let project = root.join("project");
        write_snippet(&project, ".opencode/snippet/base.md", "Base text");
        write_snippet(
            &project,
            ".opencode/snippet/review.md",
            "<prepend>Before</prepend>Review #base<append>After</append>",
        );

        assert_eq!(
            expand_snippets("Please #review", Some(&project), &root),
            "Before\n\nPlease Review Base text\n\nAfter"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn loops_are_guarded_and_skill_refs_are_untouched() {
        let root = temp_root("loops");
        let project = root.join("project");
        write_snippet(&project, ".opencode/snippet/self.md", "#self again");

        let expanded = expand_snippets("start #self", Some(&project), &root);
        assert!(
            expanded.contains("#self"),
            "guard leaves the reference: {expanded}"
        );

        write_snippet(&project, ".opencode/snippet/skill.md", "should not fire");
        assert_eq!(
            expand_snippets("call #skill(foo)", Some(&project), &root),
            "call #skill(foo)"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
