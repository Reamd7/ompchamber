//! Minimal git operations the GitHub module needs from `server/lib/git/service.js`
//! (`getRemoteUrl`, `getRemotes` names, `getStatus().tracking`).
//!
//! GAP NOTE: the full git service port lives in the sibling `git_service`
//! module; until it exposes shared helpers these argv-shaped subprocess calls
//! mirror the JS behavior directly (`git remote get-url <name>`,
//! `git remote -v` name list, and the branch's upstream short name). They are
//! only used for GitHub repo resolution, not for the git routes themselves.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

async fn run_git(directory: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(Path::new(directory))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// `getRemoteUrl(directory, remoteName)`: `git remote get-url <name>`,
/// trimmed, null on failure.
pub async fn get_remote_url(directory: &str, remote_name: &str) -> Option<String> {
    let url = run_git(directory, &["remote", "get-url", remote_name]).await?;
    let trimmed = url.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Remote names in listing order (`getRemotes()` name list; non-repo → []).
pub async fn get_remote_names(directory: &str) -> Vec<String> {
    let output = run_git(directory, &["remote"]).await.unwrap_or_default();
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `getStatus().tracking`: the branch's upstream short name
/// (e.g. `origin/main`), null when the branch has no upstream.
pub async fn get_tracking_branch(directory: &str) -> Option<String> {
    let output = run_git(
        directory,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .await?;
    let trimmed = output.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
