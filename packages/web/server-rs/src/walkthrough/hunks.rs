//! unified diff → 可寻址 hunk 的唯一权威实现：模型叙事锚定 hunk id，
//! 客户端用同一 id 找回代码，staleness 即“当前 diff 不再存在的 id”。
//! id 形如 <scope>:<path>:<sha1(header+"\n"+body)[:8]>，同一文件内字节级
//! 重复的 hunk 追加 -2、-3… 后缀；摘要实现见 super::sha1（与 Node crypto
//! 逐字节一致）。
//! Port of `server/lib/walkthrough/hunks.js`.
//!
//! Parsing a unified diff into addressable hunks lives here and only here. The
//! model anchors its narrative to hunk ids, the client resolves those ids back
//! to rendered code, and staleness is "an id the current diff no longer has" —
//! all three break the moment two implementations disagree about what an id
//! is, so the client is never given the algorithm, only the results.
//!
//! An id is `<scope>:<path>:<sha1(header + "\n" + body)[:8]>` with a `-2`,
//! `-3`, … suffix for byte-identical hunks repeated inside one file; see
//! [`super::sha1`] for why the digest is implemented locally.

use std::collections::HashMap;

use super::sha1::sha1_hex;

/// git 文件头行的识别前缀（"diff --git "）。
const FILE_HEADER_PREFIX: &str = "diff --git ";

/// 取 SHA-1 十六进制摘要的前 8 个字符作为 hunk 指纹。
fn short_hash(value: &str) -> String {
    sha1_hex(value.as_bytes())[..8].to_string()
}

/// 解析 "diff --git a/old b/new" 行的两侧路径（含带引号的含空格路径）。
/// `parsePathsFromFileHeader`: `diff --git a/old b/new`, with either side
/// quoted when it contains spaces. Mirrors the JS regex
/// `/^diff --git (?:"?a\/(.+?)"?) (?:"?b\/(.+?)"?)$/` including its lazy
/// backtracking: the first space at which the remainder parses as the `b/`
/// side wins, so quoted paths containing spaces resolve correctly.
fn parse_paths_from_file_header(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(FILE_HEADER_PREFIX)?;
    if rest.is_empty() {
        return None;
    }

    // Optional leading quote, required `a/`-style prefix, optional trailing
    // quote (each quote optional independently, as in the regex).
    let unquote = |part: &str, prefix: &str| -> Option<String> {
        let body = part.strip_prefix('"').unwrap_or(part);
        let body = body.strip_prefix(prefix)?;
        let body = body.strip_suffix('"').unwrap_or(body);
        if body.is_empty() {
            return None;
        }
        Some(body.to_string())
    };

    let mut split = 0usize;
    loop {
        let next = rest[split..].find(' ')?;
        let at = split + next;
        let (old_part, new_part) = (&rest[..at], &rest[at + 1..]);
        if let (Some(old_path), Some(new_path)) = (unquote(old_part, "a/"), unquote(new_part, "b/"))
        {
            return Some((old_path, new_path));
        }
        split = at + 1;
    }
}

/// 由文件头附属行推断状态：new file / deleted file / rename from 分别
/// 映射 added / deleted / renamed，其余为 modified。
/// `statusFromHeaderLines`.
fn status_from_header_lines(lines: &[String]) -> &'static str {
    if lines.iter().any(|line| line.starts_with("new file mode")) {
        "added"
    } else if lines
        .iter()
        .any(|line| line.starts_with("deleted file mode"))
    {
        "deleted"
    } else if lines.iter().any(|line| line.starts_with("rename from")) {
        "renamed"
    } else {
        "modified"
    }
}

/// 文件头是否声明二进制（"Binary files " 或 "GIT binary patch"）。
/// `isBinaryHeader`.
fn is_binary_header(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.starts_with("Binary files ") || line.starts_with("GIT binary patch"))
}

/// 一个文件中的一个 hunk：头行、行区间与增删统计、可独立应用的 patch、
/// 指纹 id 与正文。
/// One parsed hunk of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHunk {
/// 全局唯一 hunk id（scope:path:sha1[:8]，重复时加出现序号后缀）。
    pub id: String,
/// hunk 头行原文（@@ … @@，含尾部上下文）。
    pub header: String,
/// 旧侧起始行号。
    pub old_start: i64,
/// 旧侧行数。
    pub old_lines: i64,
/// 新侧起始行号。
    pub new_start: i64,
/// 新侧行数。
    pub new_lines: i64,
/// 新增行数（+ 开头的行）。
    pub added: i64,
/// 删除行数（- 开头的行）。
    pub deleted: i64,
/// 可独立应用的补丁：文件头 + 仅此 hunk。
    /// Standalone applicable patch: file header + this hunk only.
    pub patch: String,
/// hunk 正文（不含头行）。
    /// Hunk body without the header line.
    pub body: String,
}

/// diff 中的一个文件：路径、状态、头块文本与全部 hunk。
/// One parsed file of a diff.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
/// 新路径（优先取 b 侧，取不到再退 a 侧）。
    pub path: String,
/// 旧路径，仅在与新路径不同（改名等）时存在。
    pub old_path: Option<String>,
/// 文件状态：added / deleted / renamed / modified。
    pub status: String,
/// 是否二进制文件。
    pub binary: bool,
/// 首个 hunk 之前的文件头块（headerLines 以换行连接）。
    /// `headerLines.join('\n')` — the file header block up to the first hunk.
    pub header_text: String,
/// 按出现顺序排列的 hunk 列表。
    pub hunks: Vec<ParsedHunk>,
}

/// 手写解析 hunk 头行，返回 (oldStart, oldLines, newStart, newLines)；
/// 省略的计数按 1 解析，允许 @@ 之后带任意尾部上下文。
/// `HUNK_HEADER` = `/^@@\s+-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s+@@(.*)$/`,
/// hand-parsed (no regex crate). Returns
/// `(oldStart, oldLines, newStart, newLines)`; a missing count defaults to 1.
fn parse_hunk_header(line: &str) -> Option<(i64, i64, i64, i64)> {
    let rest = line.strip_prefix("@@")?;
    let rest = strip_js_whitespace_run(rest)?;
    let rest = rest.strip_prefix('-')?;
    let (old_start, rest) = split_digits(rest)?;
    let (old_lines, rest) = match rest.strip_prefix(',') {
        Some(after) => split_digits(after)?,
        None => (1, rest),
    };
    let rest = strip_js_whitespace_run(rest)?;
    let rest = rest.strip_prefix('+')?;
    let (new_start, rest) = split_digits(rest)?;
    let (new_lines, rest) = match rest.strip_prefix(',') {
        Some(after) => split_digits(after)?,
        None => (1, rest),
    };
    let rest = strip_js_whitespace_run(rest)?;
    if !rest.starts_with("@@") {
        return None;
    }
    // `(.*)$` accepts any trailing context after the closing `@@`.
    Some((old_start, old_lines, new_start, new_lines))
}

/// 按 JS 正则 \s+ 的语义消费一段空白（至少一个空白字符）并返回余下文本；
/// 开头无空白则返回 None。
/// A JS `\s+` run (at least one whitespace char) and the remainder after it.
fn strip_js_whitespace_run(text: &str) -> Option<&str> {
    let mut end = 0;
    for (offset, character) in text.char_indices() {
        if matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r') {
            end = offset + character.len_utf8();
        } else {
            break;
        }
    }
    (end > 0).then_some(&text[end..])
}

/// 从文本头部取一段 ASCII 数字解析为 i64，返回 (值, 余下文本)；
/// 开头无数字则返回 None。
fn split_digits(text: &str) -> Option<(i64, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    let value: i64 = text[..end].parse().ok()?;
    Some((value, &text[end..]))
}

/// 正在累积、尚未闭合落盘的 hunk。
struct OpenHunk {
/// 头行原文。
    header: String,
/// 旧侧起始行。
    old_start: i64,
/// 旧侧行数。
    old_lines: i64,
/// 新侧起始行。
    new_start: i64,
/// 新侧行数。
    new_lines: i64,
/// 累计新增行数。
    added: i64,
/// 累计删除行数。
    deleted: i64,
/// 正文行缓冲。
    lines: Vec<String>,
}

/// 单文件的解析状态机：当前文件、头行缓冲、打开的 hunk 与指纹计数，
/// 闭合转移以方法形式提供。
/// Per-file parse state (`current`, `headerLines`, `hunk`, `hunkDigests` in
/// the JS loop), with the close-hunk/close-file transitions as methods.
struct FileAccumulator {
/// 正在解析的文件（遇到下一个 diff --git 行之前有效）。
    current: Option<ParsedFile>,
/// 首个 hunk 之前累积的文件头行。
    header_lines: Vec<String>,
/// 当前打开的 hunk（None 表示处于文件头区域）。
    hunk: Option<OpenHunk>,
/// 指纹 → 已出现次数（字节级重复 hunk 的去重依据）。
    hunk_digests: HashMap<String, u32>,
/// hunk id 的命名空间。
    scope: String,
}

/// 闭合转移逻辑：把打开的 hunk 与文件安全落盘。
impl FileAccumulator {
/// 以 scope 创建空状态。
    fn new(scope: &str) -> Self {
        Self {
            current: None,
            header_lines: Vec::new(),
            hunk: None,
            hunk_digests: HashMap::new(),
            scope: scope.to_string(),
        }
    }

/// 闭合当前 hunk：计算指纹 id（字节级重复时加出现序号后缀）并推入文件。
    fn close_hunk(&mut self) {
        let Some(open) = self.hunk.take() else {
            return;
        };
        let Some(file) = self.current.as_mut() else {
            return;
        };
        let body = open.lines.join("\n");
        // The id covers the header and the body, so any edit to the hunk —
        // even one that keeps its line numbers — produces a different id.
        // That is what makes "this stop is stale" detectable without diffing
        // narratives.
        let digest = short_hash(&format!("{}\n{}", open.header, body));
        let seen = self.hunk_digests.get(&digest).copied().unwrap_or(0);
        self.hunk_digests.insert(digest.clone(), seen + 1);
        // A file can legitimately contain byte-identical hunks (repeated
        // boilerplate edits). Disambiguate by occurrence so ids stay unique
        // without becoming positional for the common case.
        let suffix = if seen == 0 {
            String::new()
        } else {
            format!("-{}", seen + 1)
        };
        let header = open.header;
        file.hunks.push(ParsedHunk {
            header: header.clone(),
            id: format!("{}:{}:{}{}", self.scope, file.path, digest, suffix),
            old_start: open.old_start,
            old_lines: open.old_lines,
            new_start: open.new_start,
            new_lines: open.new_lines,
            added: open.added,
            deleted: open.deleted,
            patch: format!("{}\n{}\n{}\n", file.header_text, header, body),
            body,
        });
    }

/// 闭合当前文件（先闭合 hunk），补二进制标记后收入结果列表。
    fn close_file(&mut self, files: &mut Vec<ParsedFile>) {
        self.close_hunk();
        if let Some(mut file) = self.current.take() {
            file.binary = file.binary || is_binary_header(&self.header_lines);
            files.push(file);
        }
        self.header_lines.clear();
        self.hunk_digests.clear();
    }
}

/// 把覆盖任意数量文件的 unified diff 拆成文件与 hunk。scope 是 id 的
/// 命名空间：不同 scope 中字节相同的 hunk 保持不同 id，使针对 staged
/// 变更写的导览不会静默命中 unstaged 的 hunk。解析不出路径的文件被丢弃。
/// `parseDiffFiles`: split a unified diff covering any number of files into
/// files and hunks.
///
/// `scope` is the opaque namespace for the ids (e.g. 'staged', 'branch'). Two
/// scopes of the same repository can contain byte-identical hunks; the scope
/// keeps their ids distinct so a walkthrough written against staged changes
/// never silently resolves against unstaged ones.
pub fn parse_diff_files(patch: &str, scope: &str) -> Vec<ParsedFile> {
    if patch.trim().is_empty() {
        return Vec::new();
    }

    // JS `text.split(/\r?\n/)`.
    let lines: Vec<String> = patch
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();

    let mut files: Vec<ParsedFile> = Vec::new();
    let mut state = FileAccumulator::new(scope);

    for line in lines {
        if line.starts_with(FILE_HEADER_PREFIX) {
            state.close_file(&mut files);
            state.header_lines = vec![line.clone()];
            let paths = parse_paths_from_file_header(&line);
            state.current = Some(ParsedFile {
                path: paths
                    .as_ref()
                    .map(|(_, new)| new.clone())
                    .or_else(|| paths.as_ref().map(|(old, _)| old.clone()))
                    .unwrap_or_default(),
                old_path: match &paths {
                    Some((old, new)) if old != new => Some(old.clone()),
                    _ => None,
                },
                status: "modified".to_string(),
                binary: false,
                header_text: line,
                hunks: Vec::new(),
            });
            continue;
        }

        if state.current.is_none() {
            continue;
        }

        if let Some((old_start, old_lines, new_start, new_lines)) = parse_hunk_header(&line) {
            state.close_hunk();
            let file = state.current.as_mut().expect("checked above");
            file.status = status_from_header_lines(&state.header_lines).to_string();
            file.header_text = state.header_lines.join("\n");
            state.hunk = Some(OpenHunk {
                header: line,
                old_start,
                old_lines,
                new_start,
                new_lines,
                added: 0,
                deleted: 0,
                lines: Vec::new(),
            });
            continue;
        }

        if state.hunk.is_none() {
            state.header_lines.push(line);
            continue;
        }

        let open = state.hunk.as_mut().expect("checked above");
        open.lines.push(line.clone());
        if line.starts_with('+') {
            open.added += 1;
        } else if line.starts_with('-') {
            open.deleted += 1;
        }
    }

    state.close_file(&mut files);
    files.retain(|file| !file.path.is_empty());
    files
}

/// 拥有可寻址 hunk 的最小接口（ParsedFile 与 DigestFile 都实现）。
/// Anything that owns addressable hunks plus the path and status the client
/// index needs — implemented by both [`ParsedFile`] and the digest's
/// `DigestFile`.
pub trait HunkSource {
/// 所属文件路径。
    fn path(&self) -> &str;
/// 所属文件状态。
    fn status(&self) -> &str;
/// hunk 列表。
    fn hunks(&self) -> &[ParsedHunk];
}

/// 解析结果的原生实现。
impl HunkSource for ParsedFile {
/// 新路径。
    fn path(&self) -> &str {
        &self.path
    }
/// 状态字符串。
    fn status(&self) -> &str {
        &self.status
    }
/// hunk 列表。
    fn hunks(&self) -> &[ParsedHunk] {
        &self.hunks
    }
}

/// id 键控的 hunk 索引：别名解析与 staleness 检查用，保持文件先后的
/// 位置顺序。
/// `indexHunks`: flatten parsed files into an id-keyed index for resolution
/// and staleness checks, preserving file-then-position order.
pub struct HunkIndex {
/// 按文件与位置顺序排列的条目。
    entries: Vec<IndexedHunk>,
/// id → entries 下标。
    by_id: HashMap<String, usize>,
}

/// 索引条目：hunk id 加上所属文件的路径与状态。
/// The index value shape: the hunk with its owning file's path and status.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedHunk {
/// hunk id。
    pub id: String,
/// 所属文件路径。
    pub path: String,
/// 所属文件状态。
    pub status: String,
}

/// 索引查询接口。
impl HunkIndex {
/// id 是否存在于当前 diff。
    pub fn has(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

/// 索引条目总数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

/// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

/// 按文件先后顺序产出全部 id。
    /// Ids in file-then-position order (`[...hunkIndex.keys()]`).
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.id.as_str())
    }

/// 按 id 取条目。
    pub fn get(&self, id: &str) -> Option<&IndexedHunk> {
        self.by_id.get(id).map(|&index| &self.entries[index])
    }
}

/// 由任意 HunkSource 集合构建索引（indexHunks）。
pub fn index_hunks<'a, F: HunkSource + 'a>(files: impl IntoIterator<Item = &'a F>) -> HunkIndex {
    let mut entries = Vec::new();
    let mut by_id = HashMap::new();
    for file in files {
        for hunk in file.hunks() {
            by_id.insert(hunk.id.clone(), entries.len());
            entries.push(IndexedHunk {
                id: hunk.id.clone(),
                path: file.path().to_string(),
                status: file.status().to_string(),
            });
        }
    }
    HunkIndex { entries, by_id }
}

/// 按文件与位置顺序列出全部 hunk id，用于计算未被任何 stop 覆盖的尾部。
/// `listHunkIds`: every hunk id in the diff, in file-then-position order.
/// Used to compute the "not covered by any stop" tail.
pub fn list_hunk_ids(files: &[ParsedFile]) -> Vec<String> {
    files
        .iter()
        .flat_map(|file| file.hunks.iter().map(|hunk| hunk.id.clone()))
        .collect()
}

/// 解析与索引测试（见 tests 子模块文件）。
#[cfg(test)]
mod tests;
