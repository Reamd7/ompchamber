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
//!
//! 中文说明：worktree 根目录的路径归一化工具。find_worktree_root 沿目录
//! 逐级向上查找 .git（文件或目录都算，linked worktree 只携带 .git 文件）；
//! resolve_primary_worktree_root 通过 git rev-parse 推导主检出目录。git
//! 调用被抽象为可注入的 GitRunner，测试与其它模块可以替换实现。

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::config::home_dir;

/// 中文：目录路径归一化——先 trim，再把 ~ / ~/ / ~\ 展开为用户主目录；
/// 无法确定主目录时保留原值。
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
/// 中文：在目录归一化的基础上把反斜杠统一为正斜杠（Windows 路径兼容）。
fn normalize_path(value: &str) -> String {
    normalize_directory_path(value).replace('\\', "/")
}

/// `findWorktreeRoot(startDir)`: nearest ancestor (inclusive) holding `.git`.
/// 中文：从 start_dir（含自身）逐级向上查找最近的 .git；空串或纯空白
/// 输入直接返回 None，走到根仍未命中也返回 None。
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
/// 中文：一次注入式 git 调用的结果，对应 JS runGitCommand 的返回形状。
#[derive(Debug, Clone)]
pub struct GitCommandResult {
    /// git 进程是否以成功状态码退出。
    pub success: bool,
    /// 标准输出全文（stderr 丢弃，与 JS 口径一致）。
    pub stdout: String,
}

/// Injectable git runner: runs `git <args>` with `cwd = directory`.
/// 中文：可注入的 git 执行器——以 directory 为 cwd 运行 git 及给定参数，
/// 返回装箱 future 使同步闭包也能构造异步结果。
pub type GitRunner = Arc<dyn Fn(&Path, &[&str]) -> BoxFuture<GitCommandResult> + Send + Sync>;

/// Minimal boxed future alias (keeps the runner signature callable from sync context).
/// 中文：Send 装箱 future 的别名，仅为压缩 GitRunner 签名的长度。
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Default runner spawning the `git` binary, argv-identical to the JS.
/// 中文：默认执行器——真实 spawn git 二进制，参数与 JS 完全一致；
/// spawn 失败按「失败 + 空 stdout」处理。
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
/// 中文：从 git dir 字符串反推主检出目录：<repo>/.git 与
/// <repo>/.git/worktrees/<name> 都归约到 <repo>，其余形态返回 None。
pub fn derive_primary_worktree_root_from_git_dir(git_dir: &str) -> Option<String> {
    let normalized = normalize_path(git_dir);
    if normalized.is_empty() {
        return None;
    }
    if let Some(root) = normalized.strip_suffix("/.git") {
        return (!root.is_empty()).then(|| root.to_string());
    }
    // linked worktree 的 git dir 标记：<repo>/.git/worktrees/<name>。
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
/// 中文：运行 git rev-parse --absolute-git-dir --git-common-dir 推导主
/// 检出目录；git 失败或两行输出都无法归约时，原样返回传入目录
/// （对齐 JS 的 { root: directory } 行为）。
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

/// resolve_primary_worktree_root 的结果：推导出的主 worktree 根目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryWorktreeRoot {
    /// 主检出目录路径（推导失败时等于传入的原始 directory）。
    pub root: String,
}

/// find / derive / resolve 三条路径的单元测试，git 调用全部用 fake 注入。
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造固定输出的假 git 执行器（忽略目录与参数）。
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

    /// 按标签 + 进程号 + 纳秒时间戳创建互不冲突的临时目录。
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

    /// 验证：从嵌套子目录向上找到含 .git 的祖先目录，仓库之外返回 None。
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

    /// 验证：空串与纯空白输入不进行查找，直接返回 None。
    #[test]
    fn find_worktree_root_requires_nonempty_input() {
        assert_eq!(find_worktree_root(""), None);
        assert_eq!(find_worktree_root("   "), None);
    }

    /// 验证：无法归约出仓库前缀的 git dir（根级标记、裸仓库、空 root）都返回 None。
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

    /// 验证：linked worktree 的 absolute git dir 归约出主检出目录。
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

    /// 验证：普通仓库的 .git git dir 直接归约出仓库根。
    #[tokio::test]
    async fn primary_root_derives_from_plain_git_dir() {
        let git = fake_git("/Users/x/proj/.git\n/Users/x/proj/.git\n", true);
        let result = resolve_primary_worktree_root("/Users/x/proj", &git).await;
        assert_eq!(result.root, "/Users/x/proj");
    }

    /// 验证：--git-common-dir 为相对路径时，以传入目录为基准拼接后再归约。
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

    /// 验证：git 调用失败时原样返回传入目录（JS 的 { root: directory } 行为）。
    #[tokio::test]
    async fn git_failure_returns_directory_as_given() {
        let git = fake_git("", false);
        let result = resolve_primary_worktree_root("/some/dir/", &git).await;
        assert_eq!(result.root, "/some/dir/");
    }

    /// 验证：git 成功但两行输出都不含 worktree 标记时，同样回退为传入目录。
    #[tokio::test]
    async fn non_repository_output_returns_directory() {
        // Success but neither git dir carries a worktree marker.
        let git = fake_git("\n\n", true);
        let result = resolve_primary_worktree_root("/tmp/loose", &git).await;
        assert_eq!(result.root, "/tmp/loose");
    }
}
