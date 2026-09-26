//! Path normalization, path-safety checks, and porcelain parsing helpers —
//! port of the module-scope helpers in `server/lib/git/service.js`
//! (`normalizeDirectoryPath`, `validateRepositoryFilePaths`,
//! `cleanBranchName`, `slugWorktreeName`, `parseWorktreePorcelain`,
//! `parseRemoteBranchRef`, …).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// JS `normalizeDirectoryPath`: trim, `~` expansion. Non-string input in JS
/// passes through unchanged (then fails `.trim()` checks); here `None` in,
/// `None` out.
pub fn normalize_directory_path(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    if trimmed == "~" {
        return home_dir_str();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return home_dir_str().map(|home| join_str(&home, rest));
    }
    Some(trimmed.to_string())
}

fn home_dir_str() -> Option<String> {
    crate::config::home_dir().and_then(|p| p.to_str().map(str::to_string))
}

fn join_str(base: &str, rest: &str) -> String {
    Path::new(base).join(rest).to_string_lossy().to_string()
}

pub fn require_directory(value: Option<&str>) -> Result<String, String> {
    let normalized = normalize_directory_path(value).unwrap_or_default();
    if normalized.trim().is_empty() {
        return Err("Git directory is required".to_string());
    }
    Ok(normalized)
}

/// `path.resolve` without symlink resolution.
pub fn resolve_path(value: impl AsRef<Path>) -> PathBuf {
    absolutize(value.as_ref())
}

pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path_components(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize_path_components(&cwd.join(path))
    }
}

/// Lexical `path.normalize`: collapse `.` and `..`, drop trailing separator,
/// keep a leading `//` as-is (POSIX) — mirroring Node's behaviour closely
/// enough for the path-membership checks this module performs.
pub fn normalize_path_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut prefix_root = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => prefix_root = Some(prefix.as_os_str().to_os_string()),
            Component::RootDir => {
                result = PathBuf::from("/");
                prefix_root = None;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    if let Some(prefix) = prefix_root {
        let mut prefixed = PathBuf::from(prefix);
        prefixed.push(result.strip_prefix("/").unwrap_or(Path::new("")));
        return prefixed;
    }
    result
}

/// JS `validateRepositoryFilePaths`: every path must resolve inside (or equal)
/// the repository root — the path-safety check the JS tests pin.
pub fn validate_repository_file_paths(
    root: impl AsRef<Path>,
    file_paths: &[String],
) -> Result<(), String> {
    let repo_root = absolutize(root.as_ref());
    let root_text = repo_root.to_string_lossy().to_string();
    for file_path in file_paths {
        let absolute = absolutize(&repo_root.join(file_path));
        let text = absolute.to_string_lossy().to_string();
        if text != root_text
            && !text.starts_with(&format!("{}{}", root_text, std::path::MAIN_SEPARATOR))
        {
            return Err(format!("Path is outside repository: {}", file_path));
        }
    }
    Ok(())
}

pub fn to_git_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// JS `isInsideOrSameDirectory`.
pub fn is_inside_or_same_directory(root: &Path, target: &Path) -> bool {
    match target.strip_prefix(root) {
        Ok(relative) => relative.as_os_str().is_empty() || !relative.starts_with(".."),
        Err(_) => false,
    }
}

/// JS `canonicalPath`: realpath (falling back to the lexical path), then
/// normalize; lowercased on Windows only.
pub async fn canonical_path(input: impl AsRef<Path>) -> PathBuf {
    let absolute = absolutize(input.as_ref());
    let real = tokio::fs::canonicalize(&absolute).await.unwrap_or(absolute);
    let normalized = normalize_path_components(&real);
    #[cfg(windows)]
    {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

pub async fn check_path_exists(target: impl AsRef<Path>) -> bool {
    tokio::fs::metadata(target.as_ref()).await.is_ok()
}

/// JS `cleanBranchName`: strip `refs/heads/`, `heads/`, then a bare `refs/`.
pub fn clean_branch_name(branch: &str) -> String {
    if branch.is_empty() {
        return String::new();
    }
    for prefix in ["refs/heads/", "heads/", "refs/"] {
        if let Some(rest) = branch.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    branch.to_string()
}

const OPENCODE_ADJECTIVES: &[&str] = &[
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy",
    "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud",
    "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift", "tidy", "witty",
];

const OPENCODE_NOUNS: &[&str] = &[
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
    "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
    "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid", "star",
    "tiger", "wizard", "wolf",
];

pub const OPENCODE_WORKTREE_ATTEMPTS: usize = 26;

pub fn pick_random(values: &[&str]) -> &'static str {
    use rand::Rng;
    let mut rng = rand::rng();
    let picked: &str = values[rng.random_range(0..values.len())];
    // The two source lists are compile-time constants.
    Box::leak(picked.to_string().into_boxed_str())
}

pub fn generate_opencode_random_name() -> String {
    format!(
        "{}-{}",
        pick_random(OPENCODE_ADJECTIVES),
        pick_random(OPENCODE_NOUNS)
    )
}

/// JS `slugWorktreeName` — slugify pipeline, capped at 80 chars.
pub fn slug_worktree_name(value: &str) -> String {
    let mut out = value.trim().to_string();
    for prefix in ["refs/heads/", "heads/"] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.to_string();
            break;
        }
    }
    // `\s+` → `-`
    let mut out = split_ws_hyphenate(&out);
    out = trim_slashes(&out);
    out = out.replace('/', "-");
    out = replace_non_slug_chars(&out);
    out = collapse_hyphens(&out);
    out = out.trim_matches('-').to_string();
    out.chars().take(80).collect()
}

fn split_ws_hyphenate(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_ws = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                result.push('-');
            }
            in_ws = true;
        } else {
            in_ws = false;
            result.push(ch);
        }
    }
    result
}

fn trim_slashes(value: &str) -> String {
    value.trim_matches('/').to_string()
}

fn replace_non_slug_chars(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            result.push(ch);
        } else {
            result.push('-');
        }
    }
    result
}

fn collapse_hyphens(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut previous_hyphen = false;
    for ch in value.chars() {
        let is_hyphen = ch == '-';
        if is_hyphen && previous_hyphen {
            continue;
        }
        previous_hyphen = is_hyphen;
        result.push(ch);
    }
    result
}

/// One parsed `git worktree list --porcelain` entry (JS `parseWorktreePorcelain`).
#[derive(Debug, Clone, Default)]
pub struct WorktreePorcelainEntry {
    pub worktree: String,
    pub head: String,
    pub branch_ref: String,
    pub branch: String,
}

pub fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreePorcelainEntry> {
    let mut entries: Vec<WorktreePorcelainEntry> = Vec::new();
    let mut current: Option<WorktreePorcelainEntry> = None;

    for line in raw.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            if let Some(entry) = current.take()
                && !entry.worktree.is_empty()
            {
                entries.push(entry);
            }
            continue;
        }
        if let Some(worktree) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take()
                && !entry.worktree.is_empty()
            {
                entries.push(entry);
            }
            current = Some(WorktreePorcelainEntry {
                worktree: worktree.trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            entry.head = head.trim().to_string();
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            let branch_ref = branch_ref.trim().to_string();
            entry.branch = clean_branch_name(&branch_ref);
            entry.branch_ref = branch_ref;
        }
    }

    if let Some(entry) = current.take()
        && !entry.worktree.is_empty()
    {
        entries.push(entry);
    }
    entries
}

pub fn normalize_start_ref(value: Option<&str>) -> String {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        "HEAD".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn is_valid_commit_hash(hash: &str) -> bool {
    hash.len() >= 7 && hash.len() <= 40 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// JS `parseRemoteBranchRef` → `(remote, branch, remoteRef, fullRef)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteBranchRef {
    pub remote: String,
    pub branch: String,
    pub remote_ref: String,
    pub full_ref: String,
}

pub fn parse_remote_branch_ref(value: &str) -> Option<RemoteBranchRef> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("refs/remotes/") {
        let slash_index = rest.find('/');
        let invalid =
            |idx: Option<usize>| idx.is_none() || Some(rest.len() - 1) == idx || idx == Some(0);
        let slash_index = match slash_index {
            Some(idx) if !invalid(Some(idx)) => idx,
            _ => return None,
        };
        return Some(RemoteBranchRef {
            remote: rest[..slash_index].to_string(),
            branch: rest[slash_index + 1..].to_string(),
            remote_ref: rest.to_string(),
            full_ref: format!("refs/remotes/{}", rest),
        });
    }

    if trimmed.starts_with("remotes/") {
        return parse_remote_branch_ref(&format!("refs/{}", trimmed));
    }

    let slash_index = match trimmed.find('/') {
        Some(idx) if idx > 0 && idx != trimmed.len() - 1 => idx,
        _ => return None,
    };

    Some(RemoteBranchRef {
        remote: trimmed[..slash_index].to_string(),
        branch: trimmed[slash_index + 1..].to_string(),
        remote_ref: trimmed.to_string(),
        full_ref: format!("refs/remotes/{}", trimmed),
    })
}

/// JS `normalizeFilePathList`: trim, drop empties, dedupe preserving order.
pub fn normalize_file_path_list(paths: &[serde_json::Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in paths {
        let text = value.as_str().map(str::trim).unwrap_or("").to_string();
        if text.is_empty() || !seen.insert(text.clone()) {
            continue;
        }
        out.push(text);
    }
    out
}

/// JS `getOpenCodeDataPath`: `$XDG_DATA_HOME/opencode` or
/// `~/.local/share/opencode`.
pub fn opencode_data_path() -> PathBuf {
    let xdg = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let base = match xdg {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::home_dir()
            .unwrap_or_default()
            .join(".local")
            .join("share"),
    };
    base.join("opencode")
}

/// JS `buildSshCommand`/`escapeSshKeyPath` — SSH key path escaped for
/// `core.sshCommand`. Dangerous characters reject; Unix single-quote-escapes.
pub fn escape_ssh_key_path(ssh_key_path: &str) -> Result<String, String> {
    let is_windows = cfg!(windows);
    let mut normalized = ssh_key_path.to_string();
    if is_windows {
        normalized = normalized.replace('\\', "/");
    }
    let dangerous: &[char] = &[
        '`', '$', '!', '"', '\'', ';', '&', '|', '<', '>', '(', ')', '{', '}', '[', ']', '*', '?',
        '#', '~',
    ];
    if normalized.chars().any(|c| dangerous.contains(&c)) {
        return Err(format!(
            "SSH key path contains invalid characters: {}",
            ssh_key_path
        ));
    }
    if is_windows {
        let mut unix_path = normalized;
        let bytes = unix_path.as_bytes();
        if bytes.len() >= 3
            && bytes[1] == b':'
            && bytes[2] == b'/'
            && bytes[0].is_ascii_alphabetic()
        {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            unix_path = format!("/{}{}", drive, &unix_path[2..]);
        }
        Ok(format!("'{}'", unix_path))
    } else {
        let escaped = normalized.replace('\'', "'\\''");
        Ok(format!("'{}'", escaped))
    }
}

pub fn build_ssh_command(ssh_key_path: &str) -> Result<String, String> {
    let escaped = escape_ssh_key_path(ssh_key_path)?;
    Ok(format!("ssh -i {} -o IdentitiesOnly=yes", escaped))
}

pub fn image_mime_type(file_path: &str) -> &'static str {
    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

pub fn is_image_file(file_path: &str) -> bool {
    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" | "avif"
    )
}

pub fn trim_git_lines(value: &str) -> Vec<String> {
    value
        .split('\n')
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_directory_path_expands_tilde() {
        let home = crate::config::home_dir()
            .expect("home")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            normalize_directory_path(Some(" ~ ")).as_deref(),
            Some(home.as_str())
        );
        assert_eq!(
            normalize_directory_path(Some("~/repo")),
            Some(join_str(&home, "repo"))
        );
        assert_eq!(normalize_directory_path(Some("  ")), Some(String::new()));
        assert_eq!(
            normalize_directory_path(Some("/plain/path")),
            Some("/plain/path".to_string())
        );
    }

    #[test]
    fn validate_repository_file_paths_rejects_escape() {
        assert!(
            validate_repository_file_paths(
                "/repo",
                &["ok.txt".to_string(), "sub/a.ts".to_string()]
            )
            .is_ok()
        );
        let err =
            validate_repository_file_paths("/repo", &["../secret.txt".to_string()]).unwrap_err();
        assert_eq!(err, "Path is outside repository: ../secret.txt");
        assert!(validate_repository_file_paths("/repo", &["/etc/passwd".to_string()]).is_err());
    }

    #[test]
    fn clean_branch_name_strips_prefixes() {
        assert_eq!(clean_branch_name("refs/heads/main"), "main");
        assert_eq!(clean_branch_name("heads/main"), "main");
        assert_eq!(
            clean_branch_name("refs/remotes/origin/main"),
            "remotes/origin/main"
        );
        assert_eq!(clean_branch_name(""), "");
        assert_eq!(clean_branch_name("feature/x"), "feature/x");
    }

    #[test]
    fn slug_worktree_name_pipeline() {
        assert_eq!(slug_worktree_name("  My Cool Feature  "), "My-Cool-Feature");
        assert_eq!(slug_worktree_name("refs/heads/wt"), "wt");
        assert_eq!(slug_worktree_name("a/b/c"), "a-b-c");
        assert_eq!(slug_worktree_name("--weird--"), "weird");
        assert_eq!(slug_worktree_name("a  --  b"), "a-b");
        let long = "x".repeat(200);
        assert_eq!(slug_worktree_name(&long).len(), 80);
        assert_eq!(slug_worktree_name("/lead/trail/"), "lead-trail");
    }

    #[test]
    fn parse_worktree_porcelain_entries() {
        let raw = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\nworktree /wt\nHEAD def456\ndetached\n\n";
        let entries = parse_worktree_porcelain(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].worktree, "/repo");
        assert_eq!(entries[0].head, "abc123");
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].branch_ref, "refs/heads/main");
        assert_eq!(entries[1].branch, "");
    }

    #[test]
    fn parse_remote_branch_ref_variants() {
        let parsed = parse_remote_branch_ref("origin/main").unwrap();
        assert_eq!(parsed.remote, "origin");
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.remote_ref, "origin/main");
        assert_eq!(parsed.full_ref, "refs/remotes/origin/main");

        let parsed = parse_remote_branch_ref("remotes/origin/react").unwrap();
        assert_eq!(parsed.remote_ref, "origin/react");

        let parsed = parse_remote_branch_ref("refs/remotes/pr-owner/head").unwrap();
        assert_eq!(parsed.remote, "pr-owner");
        assert_eq!(parsed.branch, "head");

        assert!(parse_remote_branch_ref("").is_none());
        assert!(parse_remote_branch_ref("noseparator").is_none());
        assert!(parse_remote_branch_ref("origin/").is_none());
        assert!(parse_remote_branch_ref("/origin").is_none());
    }

    #[test]
    fn commit_hash_validation() {
        assert!(is_valid_commit_hash("1234567890abcdef"));
        assert!(is_valid_commit_hash(
            "1234567890abcdef1234567890abcdef12345678"
        ));
        assert!(!is_valid_commit_hash("123456"));
        assert!(!is_valid_commit_hash("HEAD"));
        assert!(!is_valid_commit_hash("--hard"));
        assert!(!is_valid_commit_hash(
            "1234567890abcdef1234567890abcdef123456789"
        ));
    }

    #[test]
    fn ssh_key_escaping() {
        assert_eq!(
            escape_ssh_key_path("/home/me/.ssh/id_ed25519").unwrap(),
            "'/home/me/.ssh/id_ed25519'"
        );
        assert_eq!(
            escape_ssh_key_path("/home/me/my key").unwrap(),
            "'/home/me/my key'"
        );
        // JS `dangerousChars` includes the single quote, so such paths are
        // rejected outright (the unix quote-escaping branch never sees them).
        assert!(escape_ssh_key_path("/home/o'neal/key").is_err());
        assert!(escape_ssh_key_path("/path;rm -rf").is_err());
        assert_eq!(
            build_ssh_command("/key").unwrap(),
            "ssh -i '/key' -o IdentitiesOnly=yes"
        );
    }

    #[test]
    fn image_helpers() {
        assert!(is_image_file("logo.PNG"));
        assert!(!is_image_file("main.rs"));
        assert_eq!(image_mime_type("a.jpg"), "image/jpeg");
        assert_eq!(image_mime_type("a.svg"), "image/svg+xml");
    }

    #[test]
    fn is_inside_or_same_directory_checks() {
        let root = Path::new("/worktrees");
        assert!(is_inside_or_same_directory(root, Path::new("/worktrees")));
        assert!(is_inside_or_same_directory(
            root,
            Path::new("/worktrees/wt/sub")
        ));
        assert!(!is_inside_or_same_directory(
            root,
            Path::new("/worktrees-other")
        ));
        assert!(!is_inside_or_same_directory(root, Path::new("/elsewhere")));
    }
}
