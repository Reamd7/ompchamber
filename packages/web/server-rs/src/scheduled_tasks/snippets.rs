//! Port of `server/lib/opencode/snippets.js` (the `expandSnippets` path used
//! by scheduled-task prompt dispatch).
//!
//! Snippets are `#name` hashtags expanding from markdown snippet files in
//! `~/.config/opencode/snippets[.alt]` (global) and
//! `<project>/.opencode/snippets[.alt]` (project), with `<prepend>`,
//! `<append>` and `<inject>` blocks, alias frontmatter, and a per-run
//! expansion-count guard (15) against self-referential loops.
//!
//! Not ported (unused by scheduled dispatch): create/update/delete snippet
//! APIs and snippet listing — those belong to the `opencode/` remainder port.
//!
//! `#name` 片段（snippet）展开：从全局与项目目录的 markdown 片段文件
//! 构建注册表，支持 `<prepend>`/`<append>`/`<inject>` 块、frontmatter
//! 别名，以及防自引用循环的单片段展开次数上限。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::md::parse_md_document;

/// 片段名最大长度（字节）。
const SNIPPET_NAME_MAX: usize = 80;

/// 片段名校验：非空、不超上限、以字母数字开头，且仅含字母数字/`_`/`-`。
fn is_valid_snippet_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > SNIPPET_NAME_MAX {
        return false;
    }
    bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// 一个已加载片段的正文内容。
#[derive(Debug, Clone)]
struct Snippet {
    /// 片段文件 frontmatter 之后的正文。
    content: String,
}

/// 全局片段目录：`~/.config/opencode` 下的 snippets 与 snippet 两种命名。
fn global_snippet_dirs() -> Vec<PathBuf> {
    let config_root = super::md::user_opencode_config_dir();
    vec![config_root.join("snippets"), config_root.join("snippet")]
}

/// 项目片段目录：working_directory/.opencode 下的 snippets 与 snippet 两种命名。
fn project_snippet_dirs(working_directory: &Path) -> Vec<PathBuf> {
    vec![
        working_directory.join(".opencode").join("snippets"),
        working_directory.join(".opencode").join("snippet"),
    ]
}

/// 归一化 frontmatter 的 alias/aliases 字段：单值或数组均可，
/// 剔除空串与非字符串项。
fn normalize_aliases(frontmatter: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let raw = frontmatter
        .get("aliases")
        .or_else(|| frontmatter.get("alias"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let list: Vec<serde_json::Value> = match raw {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Null => Vec::new(),
        single => vec![single],
    };
    list.into_iter()
        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// `loadSnippetRegistry`: later same-name snippets replace earlier
/// registrations (and their aliases); aliases that collide with a canonical
/// name are ignored (`registerSnippet`).
/// 扫描全局（及可选的项目）片段目录构建注册表：目录内按文件名排序，
/// 同名片段后者覆盖前者（连同别名重挂）；与规范名冲突的别名被忽略。
fn load_registry(working_directory: Option<&Path>) -> HashMap<String, Snippet> {
    let mut dirs = global_snippet_dirs();
    if let Some(wd) = working_directory {
        dirs.extend(project_snippet_dirs(wd));
    }

    let mut registry: HashMap<String, Snippet> = HashMap::new();
    let mut canonical: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for path in files {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_valid_snippet_name(name) {
                continue;
            }
            let Ok((frontmatter, body)) = parse_md_document(&path) else {
                continue;
            };
            let key = name.to_ascii_lowercase();
            canonical.insert(key.clone());
            let snippet = Snippet { content: body };
            let aliases = normalize_aliases(&frontmatter);
            registry.insert(key, snippet.clone());
            for alias in aliases {
                let alias_key = alias.to_ascii_lowercase();
                if is_valid_snippet_name(&alias) && !canonical.contains(&alias_key) {
                    registry.insert(alias_key, snippet.clone());
                }
            }
        }
    }
    registry
}

/// 从片段中抽出的 prepend/append 块收集器。
#[derive(Debug, Default)]
struct Blocks {
    /// 按出现顺序收集的 `<prepend>` 块。
    prepend: Vec<String>,
    /// 按出现顺序收集的 `<append>` 块。
    append: Vec<String>,
}

/// `parseSnippetBlocks`: extract `<prepend>`/`<append>` blocks (in order),
/// drop `<inject>` blocks, trim the inline remainder.
/// 解析片段正文：抽出 `<prepend>`/`<append>` 块（保序）、丢弃
/// `<inject>` 块，剩余内联文本去首尾空白。
fn parse_snippet_blocks(content: &str) -> (String, Blocks) {
    let mut blocks = Blocks::default();
    let mut inline = content.to_string();

    for tag in ["prepend", "append"] {
        inline = replace_tag_blocks(&inline, tag, &mut blocks);
    }
    inline = strip_tag_blocks(&inline, "inject");
    (inline.trim().to_string(), blocks)
}

/// 把 input 中所有 `<tag>...</tag>` 块（大小写不敏感）移出并收进
/// blocks，其余文本保留；未闭合的块一直取到串尾。
fn replace_tag_blocks(input: &str, tag: &str, blocks: &mut Blocks) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = find_case_insensitive(rest, &open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let end = find_case_insensitive(after, &close).unwrap_or(after.len());
        let value = after[..end].trim();
        if !value.is_empty() {
            if tag == "prepend" {
                blocks.prepend.push(value.to_string());
            } else {
                blocks.append.push(value.to_string());
            }
        }
        let mut next = &after[end..];
        if next.starts_with(&close) {
            next = &next[close.len()..];
        }
        rest = next;
    }
    out.push_str(rest);
    out
}

/// 与 replace_tag_blocks 相同的扫描，但直接丢弃块内容、不收集。
fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = find_case_insensitive(rest, &open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let end = find_case_insensitive(after, &close).unwrap_or(after.len());
        let mut next = &after[end..];
        if next.starts_with(&close) {
            next = &next[close.len()..];
        }
        rest = next;
    }
    out.push_str(rest);
    out
}

/// ASCII 大小写不敏感地查找 needle 首次出现的字节偏移；
/// needle 为空时返回 None。
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let n_lower: Vec<u8> = n.iter().map(|b| b.to_ascii_lowercase()).collect();
    (0..h.len().saturating_sub(n.len() - 1)).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(&n_lower)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

/// 单个片段在一次展开中的最大展开次数；超过即判为循环并停止展开。
const MAX_EXPANSION_COUNT: usize = 15;

/// `expandSnippets(text, workingDirectory)`: expand `#hashtag` snippet
/// references, collecting prepend/append blocks, and join with blank lines.
/// 对外入口：加载注册表后展开 text 中的 `#hashtag` 引用；
/// prepend 块、内联正文、append 块以空行依次拼接。
pub fn expand_snippets(text: &str, working_directory: Option<&Path>) -> String {
    let registry = load_registry(working_directory);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut collector = Blocks::default();
    let expanded = expand_text(text, &registry, &mut counts, &mut collector);
    let expanded = expanded.trim().to_string();

    let mut parts: Vec<String> = Vec::new();
    for block in collector.prepend {
        if !block.is_empty() {
            parts.push(block);
        }
    }
    if !expanded.is_empty() {
        parts.push(expanded);
    }
    for block in collector.append {
        if !block.is_empty() {
            parts.push(block);
        }
    }
    parts.join("\n\n")
}

/// `expandText`: repeat hashtag replacement until a fixed point (or loop
/// detection); prepend/append blocks recurse with the shared collector.
/// 反复做 hashtag 替换直到不动点或检出循环；
/// prepend/append 块递归展开并入共享 collector。
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

/// 单趟扫描替换：识别 `#name`（`#skill(` 视为技能调用跳过），命中注册表
/// 则递归展开片段内容，同一片段超限时置 loop_detected 并原样保留。
fn replace_hashtags(
    text: &str,
    registry: &HashMap<String, Snippet>,
    counts: &mut HashMap<String, usize>,
    loop_detected: &mut bool,
    collector: &mut Blocks,
) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '#' && i + 1 < chars.len() && is_tag_char(chars[i + 1]) {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && is_tag_char(chars[end]) {
                end += 1;
            }
            let name: String = chars[start..end].iter().collect();
            let after = chars.get(end).copied();
            let lowered = name.to_ascii_lowercase();
            // `#skill(` is a skill invocation, not a snippet.
            let is_skill_call = lowered == "skill" && after == Some('(');
            if !is_skill_call && let Some(snippet) = registry.get(&lowered) {
                let count = counts.get(&lowered).copied().unwrap_or(0) + 1;
                if count > MAX_EXPANSION_COUNT {
                    *loop_detected = true;
                } else {
                    counts.insert(lowered.clone(), count);
                    let (inline, blocks) = parse_snippet_blocks(&snippet.content);
                    for block in &blocks.prepend {
                        let expanded = expand_text(block, registry, counts, &mut Blocks::default());
                        collector.prepend.push(expanded);
                    }
                    for block in &blocks.append {
                        let expanded = expand_text(block, registry, counts, &mut Blocks::default());
                        collector.append.push(expanded);
                    }
                    let inner = expand_text(&inline, registry, counts, collector);
                    out.push_str(&inner);
                    i = end;
                    continue;
                }
            }
            out.push('#');
            out.push_str(&name);
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// hashtag 名的合法字符：字母数字/`_`/`-`。
fn is_tag_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 片段展开的端到端单元测试（使用真实临时目录）。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：建父目录并写文件，失败即 panic。
    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    /// 测试辅助：生成以 tag 命名的唯一临时目录（先清残留）。
    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oc-snip-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 验证项目 snippet 目录中的片段被 #hashtag 内联展开。
    #[test]
    fn expands_hashtag_from_project_snippet_dir() {
        let root = temp("basic");
        let project = root.join("repo");
        write(
            &project.join(".opencode").join("snippet").join("greet.md"),
            "Hello from the snippet",
        );
        assert_eq!(
            expand_snippets("Do stuff #greet now", Some(&project)),
            "Do stuff Hello from the snippet now"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证 prepend 块、内联正文、append 块以空行顺序拼接。
    #[test]
    fn collects_prepend_and_append_blocks() {
        let root = temp("blocks");
        let project = root.join("repo");
        write(
            &project.join(".opencode").join("snippet").join("wrap.md"),
            "<prepend>BEFORE</prepend>inline<append>AFTER</append>",
        );
        assert_eq!(
            expand_snippets("#wrap", Some(&project)),
            "BEFORE\n\ninline\n\nAFTER"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证未知 hashtag、#skill( 调用与孤立 # 均原样保留。
    #[test]
    fn unknown_hashtags_and_skill_calls_stay_verbatim() {
        let project = Path::new("/definitely/not/a/real/project");
        assert_eq!(
            expand_snippets("plain #nope text", Some(project)),
            "plain #nope text"
        );
        assert_eq!(
            expand_snippets("call #skill(x)", Some(project)),
            "call #skill(x)"
        );
        assert_eq!(
            expand_snippets("bare hash # alone", Some(project)),
            "bare hash # alone"
        );
    }

    /// 验证 frontmatter 别名与规范名同样能触发展开。
    #[test]
    fn aliases_resolve_like_canonical_names() {
        let root = temp("alias");
        let project = root.join("repo");
        write(
            &project.join(".opencode").join("snippet").join("full.md"),
            "---\naliases: [short]\n---\nexpanded body",
        );
        assert_eq!(expand_snippets("#short", Some(&project)), "expanded body");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证自引用片段被展开次数上限截停而非无限递归。
    #[test]
    fn self_referential_snippets_stop_instead_of_looping() {
        let root = temp("loop");
        let project = root.join("repo");
        write(
            &project.join(".opencode").join("snippet").join("loop.md"),
            "a #loop b",
        );
        // Terminates; the expansion count guard caps the recursion.
        let expanded = expand_snippets("#loop", Some(&project));
        assert!(expanded.contains("a ") && expanded.contains(" b"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证纯空白输入展开为空串。
    #[test]
    fn empty_text_expands_to_empty() {
        assert_eq!(expand_snippets("   ", None), "");
    }
}
