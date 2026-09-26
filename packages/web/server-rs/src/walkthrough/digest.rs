//! Port of `server/lib/walkthrough/digest.js`.
//!
//! The digest is what the model actually reads. Within what it covers there
//! is no truncation: a diff that does not fit the model's context is refused
//! upstream so the user can pick a roomier model, because a walkthrough
//! written against a silently clipped diff is confidently wrong in a way
//! nobody can see.
//!
//! The one thing it does not cover is tool-produced files (lockfiles, minified
//! bundles, codegen). Those are excluded by name, not by size, and they are
//! not hidden — they carry no hunk aliases, so nothing can anchor to them, and
//! they surface in the uncovered tail like any other unreviewed change.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::generated::is_generated_artifact;
use super::hunks::{HunkSource, ParsedFile, ParsedHunk, parse_diff_files};

/// A parsed file annotated with its section scope and generated-ness — the
/// `files` array every consumer outside the digest sees.
#[derive(Debug, Clone)]
pub struct DigestFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub scope: String,
    pub binary: bool,
    pub generated: bool,
    pub hunks: Vec<ParsedHunk>,
}

/// One diff section: a patch plus the scope its hunk ids live in.
#[derive(Debug, Clone)]
pub struct Section {
    pub scope: String,
    pub patch: String,
}

/// `buildDigest`'s output.
pub struct DigestBuild {
    /// The model-facing digest as compact JSON (`JSON.stringify(digest)`),
    /// byte-compatible with the JS serialization (field order and omission of
    /// absent keys included, since the prompt embeds it verbatim).
    pub digest_json: String,
    /// The digest as a value, for callers that want to inspect it.
    pub digest: Value,
    /// Every parsed file, generated ones included.
    pub files: Vec<DigestFile>,
    /// alias (`h1`, `h2`, …) → real hunk id.
    pub id_by_alias: HashMap<String, String>,
    /// Real hunk id → alias.
    pub alias_by_id: HashMap<String, String>,
    /// Reviewable hunk count: what the model is actually asked about. The
    /// excluded files still reach the client through `files`.
    pub hunk_count: usize,
    /// Reviewable file count (digest files).
    pub file_count: usize,
    pub generated_file_count: usize,
    pub generated_paths: Vec<String>,
}

fn json_string(value: &str) -> String {
    // serde_json and JS `JSON.stringify` escape the same set for our domain
    // (quotes, backslash, control chars); non-ASCII passes through verbatim.
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// `buildDigest`: parse sections into files and build the model-facing digest.
///
/// Hunks are exposed to the model as request-local aliases (`h1`, `h2`, …)
/// rather than their real ids: the aliases are far cheaper in tokens, and a
/// model cannot invent a plausible-looking id for a hunk that does not exist.
pub fn build_digest(sections: &[Section]) -> DigestBuild {
    let mut files: Vec<DigestFile> = Vec::new();
    for section in sections {
        for file in parse_diff_files(&section.patch, &section.scope) {
            files.push(to_digest_file(file, &section.scope));
        }
    }

    let mut id_by_alias: HashMap<String, String> = HashMap::new();
    let mut alias_by_id: HashMap<String, String> = HashMap::new();
    let mut counter = 0usize;

    // The digest serialization mirrors the JS spreads exactly: `oldPath`,
    // `scope`, and `binary` appear only when set (scope only for non-branch,
    // non-pr sections).
    let mut digest_json = String::from("{\"files\":[");
    let mut digest_files_meta: Vec<Value> = Vec::new();
    let mut first_file = true;

    for file in &files {
        if file.generated {
            continue;
        }

        if !first_file {
            digest_json.push(',');
        }
        first_file = false;

        let mut entry = json!({ "path": file.path });
        digest_json.push_str("{\"path\":");
        digest_json.push_str(&json_string(&file.path));

        if let Some(old_path) = &file.old_path {
            entry["oldPath"] = json!(old_path);
            digest_json.push_str(",\"oldPath\":");
            digest_json.push_str(&json_string(old_path));
        }
        digest_json.push_str(",\"status\":");
        digest_json.push_str(&json_string(&file.status));
        entry["status"] = json!(file.status);
        if file.scope != "branch" && !file.scope.starts_with("pr:") {
            digest_json.push_str(",\"scope\":");
            digest_json.push_str(&json_string(&file.scope));
            entry["scope"] = json!(file.scope);
        }
        if file.binary {
            digest_json.push_str(",\"binary\":true");
            entry["binary"] = json!(true);
        }

        digest_json.push_str(",\"hunks\":[");
        let mut hunk_entries: Vec<Value> = Vec::new();
        for (index, hunk) in file.hunks.iter().enumerate() {
            counter += 1;
            let alias = format!("h{counter}");
            id_by_alias.insert(alias.clone(), hunk.id.clone());
            alias_by_id.insert(hunk.id.clone(), alias.clone());

            if index > 0 {
                digest_json.push(',');
            }
            let old_range = format!(
                "{}-{}",
                hunk.old_start,
                hunk.old_start + hunk.old_lines.saturating_sub(1).max(0)
            );
            let new_range = format!(
                "{}-{}",
                hunk.new_start,
                hunk.new_start + hunk.new_lines.saturating_sub(1).max(0)
            );
            digest_json.push_str(&format!(
                "{{\"alias\":{},\"header\":{},\"oldLines\":{},\"newLines\":{},\"added\":{},\"deleted\":{},\"patch\":{}}}",
                json_string(&alias),
                json_string(&hunk.header),
                json_string(&old_range),
                json_string(&new_range),
                hunk.added,
                hunk.deleted,
                json_string(&hunk.body),
            ));
            hunk_entries.push(json!({
                "alias": alias,
                "header": hunk.header,
                "oldLines": old_range,
                "newLines": new_range,
                "added": hunk.added,
                "deleted": hunk.deleted,
                "patch": hunk.body,
            }));
        }
        digest_json.push_str("]}");
        entry["hunks"] = Value::Array(hunk_entries);
        digest_files_meta.push(entry);
    }
    digest_json.push_str("]}");

    let generated_paths: Vec<String> = files
        .iter()
        .filter(|f| f.generated)
        .map(|file| file.path.clone())
        .collect();
    let generated_file_count = generated_paths.len();

    DigestBuild {
        digest_json,
        digest: json!({ "files": digest_files_meta }),
        files,
        id_by_alias,
        alias_by_id,
        hunk_count: counter,
        file_count: digest_files_meta.len(),
        generated_file_count,
        generated_paths,
    }
}

impl HunkSource for DigestFile {
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

fn to_digest_file(file: ParsedFile, scope: &str) -> DigestFile {
    let generated = is_generated_artifact(&file.path);
    DigestFile {
        path: file.path,
        old_path: file.old_path,
        status: file.status,
        scope: scope.to_string(),
        binary: file.binary,
        generated,
        hunks: file.hunks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_diff(path: &str, body: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,2 @@\n{body}\n"
        )
    }

    fn sections() -> Vec<Section> {
        vec![Section {
            scope: "working".to_string(),
            patch: [
                file_diff("src/a.ts", "+const a = 1;"),
                file_diff("bun.lock", "+  \"version\": \"2\","),
                file_diff("src/b.ts", "+const b = 1;"),
            ]
            .concat(),
        }]
    }

    #[test]
    fn keeps_generated_files_out_of_what_the_model_sees() {
        let built = build_digest(&sections());

        let paths: Vec<_> = built.digest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["src/a.ts", "src/b.ts"]);
        assert!(!built.digest_json.contains("bun.lock"));
        assert_eq!(built.file_count, 2);
        assert_eq!(built.hunk_count, 2);
        assert_eq!(built.generated_file_count, 1);
        assert_eq!(built.generated_paths, vec!["bun.lock"]);
    }

    #[test]
    fn still_returns_generated_files_to_the_client_so_nothing_disappears() {
        let built = build_digest(&sections());

        let paths: Vec<_> = built.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.ts", "bun.lock", "src/b.ts"]);
        assert!(
            built
                .files
                .iter()
                .find(|f| f.path == "bun.lock")
                .unwrap()
                .generated
        );
    }

    #[test]
    fn gives_aliases_only_to_reviewable_hunks() {
        let built = build_digest(&sections());

        let mut aliases: Vec<_> = built.id_by_alias.keys().cloned().collect();
        aliases.sort();
        assert_eq!(aliases, vec!["h1", "h2"]);
        assert!(!built.id_by_alias.values().any(|id| id.contains("bun.lock")));
    }

    #[test]
    fn reports_zero_reviewable_hunks_when_only_generated_files_changed() {
        let built = build_digest(&[Section {
            scope: "working".to_string(),
            patch: file_diff("bun.lock", "+  \"version\": \"2\","),
        }]);

        assert_eq!(built.hunk_count, 0);
        assert_eq!(built.files.len(), 1);
        assert_eq!(built.generated_file_count, 1);
    }

    #[test]
    fn serializes_the_digest_like_the_js_json_stringify() {
        let built = build_digest(&sections());

        // Field order and omissions match `JSON.stringify(digest)` from the
        // JS build, including the line-range strings.
        assert!(
            built
                .digest_json
                .starts_with("{\"files\":[{\"path\":\"src/a.ts\"")
        );
        assert!(built
            .digest_json
            .contains("\"alias\":\"h1\",\"header\":\"@@ -1,1 +1,2 @@\",\"oldLines\":\"1-1\",\"newLines\":\"1-2\",\"added\":1,\"deleted\":0"));
        // 'branch'/'pr:' scopes omit the scope field.
        let branch = build_digest(&[Section {
            scope: "branch".to_string(),
            patch: file_diff("src/a.ts", "+const a = 1;"),
        }]);
        assert!(!branch.digest_json.contains("\"scope\""));
    }
}
