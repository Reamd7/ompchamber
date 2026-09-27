//! Minimal git operations the GitHub module needs from `server/lib/git/service.js`
//! (`getRemoteUrl`, `getRemotes` names, `getStatus().tracking`).
//!
//! GAP NOTE: the full git service port lives in the sibling `git_service`
//! module; until it exposes shared helpers these argv-shaped subprocess calls
//! mirror the JS behavior directly (`git remote get-url <name>`,
//! `git remote -v` name list, and the branch's upstream short name). They are
//! only used for GitHub repo resolution, not for the git routes themselves.
//!
//! 中文说明：本模块为 GitHub 集成提供所需的最小 git 子进程封装：读取
//! remote URL、枚举 remote 名称、查询当前分支的 upstream 跟踪引用。
//! 所有命令都在目标目录下异步执行；进程启动失败或退出码非零统一折叠
//! 为 `None` 或空列表，由上层调用方决定回退策略。

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// 在指定目录下异步执行一次 `git <args...>` 子进程，返回 stdout 原文。
///
/// stdin 显式关闭、stdout/stderr 走管道；进程创建失败或退出码非零时
/// 返回 `None`（错误细节不透出，调用方按"无结果"处理）。
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
///
/// 对应 JS 版 `getRemoteUrl(directory, remoteName)`：执行
/// `git remote get-url <name>` 并返回 trim 后的 URL；目录不是仓库、
/// remote 不存在或输出为空时返回 `None`。
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
///
/// 对应 JS 版 `getRemotes()` 返回的 remote 名称列表：保持 `git remote`
/// 的输出顺序、跳过空行；目录不是 git 仓库时返回空数组（而非 `None`）。
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
///
/// 对应 JS 版 `getStatus().tracking`：通过
/// `git rev-parse --abbrev-ref --symbolic-full-name @{upstream}` 取当前
/// 分支 upstream 的短名（如 `origin/main`）；分支没有设置 upstream 或
/// 命令失败时返回 `None`。
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
