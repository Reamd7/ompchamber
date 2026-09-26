//! Worktree-root path normalization used by project-id scoping consumers.
//!
//! Ports two JS precedents:
//! - `findWorktreeRoot` from `server/lib/opencode/shared.js` — filesystem walk
//!   looking for a `.git` entry (file or directory; linked worktrees carry a
//!   `.git` file).
//! - `resolvePrimaryWorktreeRoot` + `derivePrimaryWorktreeRootFromGitDir` from
//!   `server/lib/git/service.js` — shells out to
//!   `git rev-parse --absolute-git-dir --git-common-dir` and derives the
//!   primary checkout owning a worktree. The git invocation is injectable so
//!   tests (and future modules) can fake it, mirroring how the JS seams its
//!   git calls through `runGitCommand`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::config::home_dir;

/// `normalizeDirectoryPath`: trim, expand a leading `~` to the home directory.
fn normalize_directory_path(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    if trimmed == "~" {
        return home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| trimmed.to_string());
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    trimmed.to_string()
}

/// `normalizePath`: directory normalization + backslashes to forward slashes.
fn normalize_path(value: &str) -> String {
    normalize_directory_path(value).replace('\\', "/")
}

/// `findWorktreeRoot(startDir)`: nearest ancestor (inclusive) holding `.git`.
pub fn find_worktree_root(start_dir: &str) -> Option<PathBuf> {
    if start_dir.trim().is_empty() {
        return None;
    }
    let mut current = PathBuf::from(start_dir);
    // JS path.resolve also drops a trailing separator, so lean on the
    // canonical-ish form without touching the filesystem (no lexical cleanup
    // in std; ancestors are unaffected by trailing separators anyway).
    loop {
        if current.join(".git").try_exists().unwrap_or(false) {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Result of one injected git invocation (`runGitCommand` shape).
#[derive(Debug, Clone)]
pub struct GitCommandResult {
    pub success: bool,
    pub stdout: String,
}

/// Injectable git runner: runs `git <args>` with `cwd = directory`.
pub type GitRunner = Arc<dyn Fn(&Path, &[&str]) -> BoxFuture<GitCommandResult> + Send + Sync>;

/// Minimal boxed future alias (keeps the runner signature callable from sync context).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Default runner spawning the `git` binary, argv-identical to the JS.
pub fn default_git_runner() -> GitRunner {
    Arc::new(|directory: &Path, args: &[&str]| {
        let directory = directory.to_path_buf();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Box::pin(async move {
            let mut command = tokio::process::Command::new("git");
            command.current_dir(&directory);
            for arg in &args {
                command.arg(arg);
            }
            match command.output().await {
                Ok(output) => GitCommandResult {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                },
                Err(_) => GitCommandResult {
                    success: false,
                    stdout: String::new(),
                },
            }
        })
    })
}

/// `derivePrimaryWorktreeRootFromGitDir`: `<repo>/.git` → `<repo>`,
/// `<repo>/.git/worktrees/<name>` → `<repo>`; anything else → None.
pub fn derive_primary_worktree_root_from_git_dir(git_dir: &str) -> Option<String> {
    let normalized = normalize_path(git_dir);
    if normalized.is_empty() {
        return None;
    }
    if let Some(root) = normalized.strip_suffix("/.git") {
        return (!root.is_empty()).then(|| root.to_string());
    }
    const MARKER: &str = "/.git/worktrees/";
    if let Some(marker_index) = normalized.find(MARKER)
        && marker_index > 0
    {
        let root = &normalized[..marker_index];
        return (!root.is_empty()).then(|| root.to_string());
    }
    None
}

/// `resolvePrimaryWorktreeRoot(directory)` — git-derived primary checkout.
///
/// On git failure the directory is returned as given (JS `{ root: directory }`),
/// letting callers normalize it themselves.
pub async fn resolve_primary_worktree_root(
    directory: &str,
    git: &GitRunner,
) -> PrimaryWorktreeRoot {
    let dir_path = PathBuf::from(normalize_directory_path(directory));
    let result = git(
        &dir_path,
        &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
    )
    .await;
    if !result.success {
        return PrimaryWorktreeRoot {
            root: directory.to_string(),
        };
    }
    let lines: Vec<String> = result
        .stdout
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let absolute_git_dir = normalize_path(lines.first().map(String::as_str).unwrap_or(""));
    if let Some(root) = derive_primary_worktree_root_from_git_dir(&absolute_git_dir) {
        return PrimaryWorktreeRoot { root };
    }
    let raw_common_dir = normalize_path(lines.get(1).map(String::as_str).unwrap_or(""));
    if !raw_common_dir.is_empty() {
        let common_dir = if Path::new(&raw_common_dir).is_absolute() {
            raw_common_dir.clone()
        } else {
            Path::new(&dir_path)
                .join(&raw_common_dir)
                .to_string_lossy()
                .into_owned()
        };
        if let Some(root) = derive_primary_worktree_root_from_git_dir(&common_dir) {
            return PrimaryWorktreeRoot { root };
        }
    }
    PrimaryWorktreeRoot {
        root: directory.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryWorktreeRoot {
    pub root: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_git(stdout: &'static str, success: bool) -> GitRunner {
        Arc::new(move |_dir: &Path, _args: &[&str]| {
            Box::pin(async move {
                GitCommandResult {
                    success,
                    stdout: stdout.to_string(),
                }
            })
        })
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oc-projects-worktree-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn find_worktree_root_walks_up_to_git_dir() {
        let root = temp_root("find");
        let repo = root.join("repo");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(
            find_worktree_root(nested.to_str().unwrap()),
            Some(repo.clone())
        );
        assert_eq!(
            find_worktree_root(repo.to_str().unwrap()),
            Some(repo.clone())
        );
        // Outside any repository → None.
        assert_eq!(find_worktree_root(root.to_str().unwrap()), None);
    }

    #[test]
    fn find_worktree_root_requires_nonempty_input() {
        assert_eq!(find_worktree_root(""), None);
        assert_eq!(find_worktree_root("   "), None);
    }

    #[test]
    fn derive_root_rejects_rootless_git_dirs() {
        assert_eq!(derive_primary_worktree_root_from_git_dir(""), None);
        // Marker at index 0 (root-level .git) cannot yield a repo prefix.
        assert_eq!(
            derive_primary_worktree_root_from_git_dir("/.git/worktrees/"),
            None
        );
        // A bare repo path carries no checkout prefix.
        assert_eq!(
            derive_primary_worktree_root_from_git_dir("/x/bare-repo"),
            None
        );
        // "/.git" alone strips to an empty root.
        assert_eq!(derive_primary_worktree_root_from_git_dir("/.git"), None);
        // Trailing marker still yields the repo prefix (JS slices by index).
        assert_eq!(
            derive_primary_worktree_root_from_git_dir("/x/.git/worktrees/"),
            Some("/x".to_string())
        );
    }

    #[tokio::test]
    async fn primary_root_derives_from_worktree_git_dir() {
        // Linked worktree: absolute git dir lives under the main checkout.
        let git = fake_git(
            "/Users/x/proj/.git/worktrees/abc\n/Users/x/proj/.git\n",
            true,
        );
        let result = resolve_primary_worktree_root("/Users/x/.worktrees/abc", &git).await;
        assert_eq!(result.root, "/Users/x/proj");
    }

    #[tokio::test]
    async fn primary_root_derives_from_plain_git_dir() {
        let git = fake_git("/Users/x/proj/.git\n/Users/x/proj/.git\n", true);
        let result = resolve_primary_worktree_root("/Users/x/proj", &git).await;
        assert_eq!(result.root, "/Users/x/proj");
    }

    #[tokio::test]
    async fn primary_root_resolves_relative_common_dir() {
        // `--git-common-dir` may be relative (`.git` inside the cwd).
        let git = fake_git("/Users/x/.worktrees/abc/.git\n.git/worktrees/abc\n", true);
        // absolute git dir is worktree-local (no marker), common dir is
        // relative: resolve against the directory.
        let result = resolve_primary_worktree_root("/Users/x/.worktrees/abc", &git).await;
        let expected = Path::new("/Users/x/.worktrees/abc")
            .join(".git/worktrees/abc")
            .to_string_lossy()
            .into_owned();
        let derived = derive_primary_worktree_root_from_git_dir(&expected);
        assert_eq!(result.root, derived.expect("marker present"));
    }

    #[tokio::test]
    async fn git_failure_returns_directory_as_given() {
        let git = fake_git("", false);
        let result = resolve_primary_worktree_root("/some/dir/", &git).await;
        assert_eq!(result.root, "/some/dir/");
    }

    #[tokio::test]
    async fn non_repository_output_returns_directory() {
        // Success but neither git dir carries a worktree marker.
        let git = fake_git("\n\n", true);
        let result = resolve_primary_worktree_root("/tmp/loose", &git).await;
        assert_eq!(result.root, "/tmp/loose");
    }
}
