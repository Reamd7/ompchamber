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

const FILE_HEADER_PREFIX: &str = "diff --git ";

fn short_hash(value: &str) -> String {
    sha1_hex(value.as_bytes())[..8].to_string()
}

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

/// `isBinaryHeader`.
fn is_binary_header(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.starts_with("Binary files ") || line.starts_with("GIT binary patch"))
}

/// One parsed hunk of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHunk {
    pub id: String,
    pub header: String,
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub added: i64,
    pub deleted: i64,
    /// Standalone applicable patch: file header + this hunk only.
    pub patch: String,
    /// Hunk body without the header line.
    pub body: String,
}

/// One parsed file of a diff.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub binary: bool,
    /// `headerLines.join('\n')` — the file header block up to the first hunk.
    pub header_text: String,
    pub hunks: Vec<ParsedHunk>,
}

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

struct OpenHunk {
    header: String,
    old_start: i64,
    old_lines: i64,
    new_start: i64,
    new_lines: i64,
    added: i64,
    deleted: i64,
    lines: Vec<String>,
}

/// Per-file parse state (`current`, `headerLines`, `hunk`, `hunkDigests` in
/// the JS loop), with the close-hunk/close-file transitions as methods.
struct FileAccumulator {
    current: Option<ParsedFile>,
    header_lines: Vec<String>,
    hunk: Option<OpenHunk>,
    hunk_digests: HashMap<String, u32>,
    scope: String,
}

impl FileAccumulator {
    fn new(scope: &str) -> Self {
        Self {
            current: None,
            header_lines: Vec::new(),
            hunk: None,
            hunk_digests: HashMap::new(),
            scope: scope.to_string(),
        }
    }

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

/// Anything that owns addressable hunks plus the path and status the client
/// index needs — implemented by both [`ParsedFile`] and the digest's
/// `DigestFile`.
pub trait HunkSource {
    fn path(&self) -> &str;
    fn status(&self) -> &str;
    fn hunks(&self) -> &[ParsedHunk];
}

impl HunkSource for ParsedFile {
    fn path(&self) -> &str {
        &self.path
    }
    fn status(&self) -> &str {
        &self.status
    }
    fn hunks(&self) -> &[ParsedHunk] {
        &self.hunks
    }
}

/// `indexHunks`: flatten parsed files into an id-keyed index for resolution
/// and staleness checks, preserving file-then-position order.
pub struct HunkIndex {
    entries: Vec<IndexedHunk>,
    by_id: HashMap<String, usize>,
}

/// The index value shape: the hunk with its owning file's path and status.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedHunk {
    pub id: String,
    pub path: String,
    pub status: String,
}

impl HunkIndex {
    pub fn has(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Ids in file-then-position order (`[...hunkIndex.keys()]`).
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.id.as_str())
    }

    pub fn get(&self, id: &str) -> Option<&IndexedHunk> {
        self.by_id.get(id).map(|&index| &self.entries[index])
    }
}

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

/// `listHunkIds`: every hunk id in the diff, in file-then-position order.
/// Used to compute the "not covered by any stop" tail.
pub fn list_hunk_ids(files: &[ParsedFile]) -> Vec<String> {
    files
        .iter()
        .flat_map(|file| file.hunks.iter().map(|hunk| hunk.id.clone()))
        .collect()
}

#[cfg(test)]
mod tests;
