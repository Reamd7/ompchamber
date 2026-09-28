//! Git process execution layer — port of the `runGitCommand` / `buildGitEnv` /
//! `createGit` machinery in `server/lib/git/service.js`, plus the parts of
//! simple-git's task runner this module needs (`git.raw` semantics: fail only
//! when git exited non-zero *with* output on stderr, with the error message
//! carrying stdout+stderr concatenated).
//!
//! The runner is an injected closure (`GitRunner`) so tests can assert argv
//! construction and script git responses without a real repository.
//!
//! 中文说明：git 子进程执行层。移植 JS 版 `server/lib/git/service.js` 中的
//! `runGitCommand` / `buildGitEnv` / `createGit` 机制，以及 simple-git 任务
//! 运行器中本模块所需的部分（`git.raw` 语义：仅当 git 以非零退出码退出且
//! stderr 有输出时才判定失败，错误 message 为 stdout+stderr 拼接）。
//! 执行器以闭包（GitRunner）注入，测试因此可以在不依赖真实仓库的情况下
//! 断言 argv 构造与脚本化的 git 响应。

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

/// Result of one git invocation, mirroring the object `runGitCommand` returns
/// in JS (`{ success, exitCode, stdout, stderr, message }`).
/// 单次 git 调用的结果，对应 JS `runGitCommand` 返回的对象
/// `{ success, exitCode, stdout, stderr, message }`。
#[derive(Debug, Clone, Default)]
pub struct GitCommandResult {
    /// git 是否以退出码 0 成功结束。
    pub success: bool,
    /// git 进程退出码；spawn 失败时固定为 1。
    pub exit_code: i32,
    /// stdout 的 UTF-8 宽松解码文本（无效字节被替换字符替代）。
    pub stdout: String,
    /// stderr 的 UTF-8 宽松解码文本。
    pub stderr: String,
    /// `parseGitErrorText` of the underlying exec failure (empty on success).
    /// 底层执行失败经 `parseGitErrorText` 拼接的错误文本；成功时为空。
    pub message: String,
    /// Raw stdout bytes — `String::from_utf8_lossy` of [`Self::stdout`] is not
    /// enough for `git show <blob>` of binary content (image data URLs).
    /// 原始 stdout 字节：`git show <blob>` 读取二进制内容（图片 data URL）
    /// 时不能只依赖字符串解码。
    pub stdout_bytes: Vec<u8>,
}

/// 结果构造与判定辅助：成功/失败样板以及 stdout 修剪。
impl GitCommandResult {
    /// 构造成功结果：退出码 0、stdout 取入参、其余字段为空。
    pub fn ok(stdout: impl Into<String>) -> Self {
        let stdout = stdout.into();
        Self {
            success: true,
            exit_code: 0,
            stdout_bytes: stdout.clone().into_bytes(),
            stdout,
            stderr: String::new(),
            message: String::new(),
        }
    }

    /// 构造失败结果：退出码 1、message 留空（由调用方按需拼接）。
    pub fn fail(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        let stdout = stdout.into();
        let stderr = stderr.into();
        Self {
            success: false,
            exit_code: 1,
            stdout_bytes: stdout.clone().into_bytes(),
            stdout,
            stderr,
            message: String::new(),
        }
    }

    /// 返回去除首尾空白后的 stdout。
    pub fn stdout_trim(&self) -> &str {
        self.stdout.trim()
    }
}

/// Failure shape of a simple-git `git.raw` call that git answered with a
/// non-zero exit *and* stderr output. `message` is stdout+stderr concatenated
/// (simple-git's `getErrorMessage`), which is why `git diff --no-index`'s
/// patch-on-stdout is recoverable from `message` in the JS source.
/// simple-git `git.raw` 的失败形态：git 以非零退出码退出且 stderr 有输出。
/// `message` 为 stdout+stderr 拼接（simple-git 的 `getErrorMessage`），因此
/// JS 源码能从 `message` 恢复 `git diff --no-index` 输出到 stdout 的补丁。
#[derive(Debug, Clone)]
pub struct GitFailure {
    /// git 进程的非零退出码。
    pub exit_code: i32,
    /// 失败时捕获的 stdout 文本。
    pub stdout: String,
    /// 失败时捕获的 stderr 文本。
    pub stderr: String,
    /// stdout+stderr 拼接后的错误消息。
    pub message: String,
}

/// JS `parseGitErrorText`: stderr, stdout, message, then `String(error)` —
/// each trimmed, non-empty chunks joined with `\n`.
/// 对应 JS `parseGitErrorText`：按 stderr、stdout、message 顺序取 trim 后
/// 非空的块，用换行拼接成单条错误文本。
pub fn parse_git_error_text(stderr: &str, stdout: &str, message: &str) -> String {
    [stderr.trim(), stdout.trim(), message.trim()]
        .iter()
        .filter(|chunk| !chunk.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断错误文本是否为 "not a git repository"（大小写不敏感），用于统一
/// 识别非 git 仓库错误。
pub fn is_not_git_repository_text(text: &str) -> bool {
    text.to_lowercase().contains("not a git repository")
}

/// JS `isIndexLockError`: matches on the combined message/stderr/stdout.
/// 对应 JS `isIndexLockError`：把 message/stderr/stdout 拼接后匹配
/// index.lock 文件冲突或"另一个 git 进程似乎正在运行"的文本形态。
pub fn is_index_lock_error(result: &GitCommandResult) -> bool {
    let message = [
        result.message.as_str(),
        result.stderr.as_str(),
        result.stdout.as_str(),
    ]
    .join("\n");
    message.contains("index.lock\"") || {
        let lowered = message.to_lowercase();
        lowered.contains("index.lock': file exists")
            || lowered.contains("index.lock: file exists")
            || lowered.contains("another git process seems to be running")
    }
}

/// JS `isMissingDirectoryError`: ENOENT/ENOTDIR codes plus the text shapes
/// simple-git/Bun surface for a directory that vanished.
/// 对应 JS `isMissingDirectoryError`：识别目录不存在/已被删除的文本
/// （ENOENT/ENOTDIR 及 simple-git/Bun 暴露的等价文案）。
pub fn is_missing_directory_text(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("directory that does not exist")
        || lowered.contains("does not exist")
        || lowered.contains("no such file or directory")
}

// ---------------------------------------------------------------------------
// Git binary resolution (`resolveGitBinary`)
// ---------------------------------------------------------------------------

/// 进程级缓存的 git 可执行文件名/路径（LazyLock 惰性解析一次）。
static GIT_BINARY: LazyLock<String> = LazyLock::new(resolve_git_binary);

/// Non-Windows: always plain `git` (resolved through PATH by the OS).
/// Windows: honour `GIT_BINARY` / `OMPCHAMBER_GIT_BINARY` when the candidate
/// is an executable file, otherwise fall back to `git` (the Node-side
/// PATH/Program-Files sweep is launcher-specific and unnecessary when the
/// binary is already resolvable).
/// 非 Windows 平台固定返回 `git`（由操作系统经 PATH 解析）；Windows 优先
/// 采用 `GIT_BINARY` / `OMPCHAMBER_GIT_BINARY` 指向的可执行文件，否则回退
/// `git`（Node 侧的 PATH/Program-Files 扫描是启动器专属逻辑，此处不需要）。
fn resolve_git_binary() -> String {
    if cfg!(windows) {
        for name in ["GIT_BINARY", "OMPCHAMBER_GIT_BINARY"] {
            if let Ok(value) = std::env::var(name) {
                let trimmed = value.trim().to_string();
                if !trimmed.is_empty() && is_executable_file(std::path::Path::new(&trimmed)) {
                    return trimmed;
                }
            }
        }
    }
    "git".to_string()
}

/// 判断路径是否为可执行普通文件：Unix 检查任一执行位，非 Unix 仅要求是文件。
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 返回解析后的 git 可执行文件名（带进程级缓存）。
pub fn git_binary() -> &'static str {
    &GIT_BINARY
}

// ---------------------------------------------------------------------------
// SSH agent socket resolution (`resolveSshAuthSock` / `buildGitEnv`)
// ---------------------------------------------------------------------------

/// 判断路径是否为 Unix domain socket；非 Unix 平台恒为 false。
fn is_socket(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        match std::fs::metadata(path) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                meta.file_type().is_socket()
            }
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// 依次尝试若干 gpgconf 候选路径执行给定参数，成功时返回其 stdout 文本，
/// 全部失败返回 `None`。
fn run_gpgconf(args: &[&str]) -> Option<String> {
    let candidates = [
        "gpgconf",
        "/opt/homebrew/bin/gpgconf",
        "/usr/local/bin/gpgconf",
    ];
    for candidate in candidates {
        let output = std::process::Command::new(candidate)
            .args(args)
            .stdin(Stdio::null())
            .output();
        if let Ok(output) = output
            && output.status.success()
        {
            return Some(String::from_utf8_lossy(&output.stdout).to_string());
        }
    }
    None
}

/// 当前用户 home 目录；无法解析时回退为当前目录 `.`。
fn home() -> PathBuf {
    crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// JS `resolveSshAuthSock`: existing env wins, then `~/.gnupg/S.gpg-agent.ssh`,
/// then `gpgconf --list-dirs agent-ssh-socket` (launching the agent once and
/// retrying). Windows resolves nothing.
/// 对应 JS `resolveSshAuthSock`：优先已存在的 `SSH_AUTH_SOCK` 环境变量，
/// 其次 `~/.gnupg/S.gpg-agent.ssh`，最后 `gpgconf --list-dirs agent-ssh-socket`
/// （必要时先 `--launch gpg-agent` 再重试一次）；Windows 直接返回 `None`。
pub fn resolve_ssh_auth_sock() -> Option<String> {
    if let Ok(existing) = std::env::var("SSH_AUTH_SOCK") {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if cfg!(windows) {
        return None;
    }

    let gpg_sock = home().join(".gnupg").join("S.gpg-agent.ssh");
    if is_socket(&gpg_sock) {
        return gpg_sock.to_str().map(str::to_string);
    }

    let candidate = run_gpgconf(&["--list-dirs", "agent-ssh-socket"])
        .unwrap_or_default()
        .trim()
        .to_string();
    if !candidate.is_empty() && is_socket(std::path::Path::new(&candidate)) {
        return Some(candidate);
    }

    if !candidate.is_empty() {
        let _ = run_gpgconf(&["--launch", "gpg-agent"]);
        let retried = run_gpgconf(&["--list-dirs", "agent-ssh-socket"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if !retried.is_empty() && is_socket(std::path::Path::new(&retried)) {
            return Some(retried);
        }
    }

    None
}

/// Environment for one git invocation: the server process environment plus a
/// resolved `SSH_AUTH_SOCK` when unset, plus per-command overrides
/// (`GIT_EDITOR=true` for `rebase --continue`, etc.).
/// 构造单次 git 调用的环境变量：继承当前进程环境，`SSH_AUTH_SOCK` 缺失或
/// 为空白时补上解析结果，最后叠加每条命令的覆盖项（如 `rebase --continue`
/// 使用的 `GIT_EDITOR=true`）。
pub fn build_git_env(overrides: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let sock_blank = env
        .get("SSH_AUTH_SOCK")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true);
    if sock_blank && let Some(resolved) = resolve_ssh_auth_sock() {
        env.insert("SSH_AUTH_SOCK".to_string(), resolved);
    }
    for (key, value) in overrides {
        env.insert(key.clone(), value.clone());
    }
    env
}

// ---------------------------------------------------------------------------
// Runner seam
// ---------------------------------------------------------------------------

/// 单次 git 调用的 boxed future 输出类型，供注入式 runner 异步返回结果。
pub type GitSpawnFuture = Pin<Box<dyn Future<Output = GitCommandResult> + Send>>;

/// Injectable git runner: `(cwd, argv, env-overrides) -> result`.
/// 可注入的 git 执行器闭包：签名为 `(cwd, argv, 环境变量覆盖) -> 结果`；
/// 测试用脚本化假实现替换以断言 argv。
pub type GitRunner =
    Arc<dyn Fn(PathBuf, Vec<String>, HashMap<String, String>) -> GitSpawnFuture + Send + Sync>;

/// The production runner — `execFile(git, args, { cwd, env, maxBuffer: 20MB })`
/// semantics: non-zero exit is a failure whose `message` is the trimmed join
/// of stderr/stdout; a missing cwd or binary is a spawn failure.
/// 生产执行器：等价 JS 的 `execFile(git, args, { cwd, env, maxBuffer: 20MB })`。
/// 非零退出即失败，`message` 取 stderr/stdout trim 后的拼接；cwd 缺失或
/// 二进制不存在视为 spawn 失败（message 携带 ENOENT/ENOTDIR 风格错误码）。
pub fn real_runner() -> GitRunner {
    Arc::new(
        |cwd: PathBuf, args: Vec<String>, env_overrides: HashMap<String, String>| {
            Box::pin(async move {
                let env = build_git_env(&env_overrides);
                let mut command = tokio::process::Command::new(git_binary());
                command
                    .args(&args)
                    .env_clear()
                    .envs(&env)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                command.current_dir(&cwd);
                match command.output().await {
                    Ok(output) => {
                        let stdout_bytes = output.stdout;
                        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
                        let code = output.status.code().unwrap_or(1);
                        if code == 0 {
                            GitCommandResult {
                                success: true,
                                exit_code: 0,
                                stdout,
                                stderr,
                                message: String::new(),
                                stdout_bytes,
                            }
                        } else {
                            let message = parse_git_error_text(
                                &stderr,
                                &stdout,
                                &format!(
                                    "Command failed: {} {}",
                                    git_binary(),
                                    args.first().map(String::as_str).unwrap_or("")
                                ),
                            );
                            GitCommandResult {
                                success: false,
                                exit_code: code,
                                stdout,
                                stderr,
                                message,
                                stdout_bytes,
                            }
                        }
                    }
                    Err(error) => GitCommandResult {
                        success: false,
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: String::new(),
                        message: format!("spawn {} {}", git_binary(), error_kind_text(&error)),
                        stdout_bytes: Vec::new(),
                    },
                }
            })
        },
    )
}

/// 把 `std::io::ErrorKind` 映射为 JS/Bun 风格的错误码文本
/// （ENOENT/ENOTDIR/EIO），用于 spawn 失败消息。
fn error_kind_text(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::NotADirectory => "ENOTDIR",
        _ => "EIO",
    }
}

/// 执行层纯函数与真实 runner 行为的回归测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 `parse_git_error_text` 只拼接 trim 后非空的块并用换行连接。
    #[test]
    fn parse_git_error_text_joins_trimmed_chunks() {
        assert_eq!(
            parse_git_error_text(" fatal: bad ", "", "Command failed"),
            "fatal: bad\nCommand failed"
        );
        assert_eq!(parse_git_error_text("", "", ""), "");
    }

    /// 验证 "not a git repository" 识别对大小写不敏感。
    #[test]
    fn not_git_repository_matches_case_insensitive() {
        assert!(is_not_git_repository_text(
            "Fatal: Not a Git repository (or any of the parent directories)"
        ));
        assert!(!is_not_git_repository_text("boom"));
    }

    /// 验证 index.lock 冲突与并发 git 进程的 stderr 能被识别为 index lock 错误。
    #[test]
    fn index_lock_detection() {
        let mut result = GitCommandResult::fail(
            "",
            "fatal: Unable to create '/x/.git/index.lock': File exists.",
        );
        assert!(is_index_lock_error(&result));
        result = GitCommandResult::fail("", "error: another git process seems to be running");
        assert!(is_index_lock_error(&result));
        result = GitCommandResult::fail("", "unrelated");
        assert!(!is_index_lock_error(&result));
    }

    /// 验证真实 runner 在非仓库目录执行 rev-parse 时返回失败，
    /// 且 stderr 含 "not a git repository"。
    #[tokio::test]
    async fn real_runner_reports_non_repo_failure() {
        let runner = real_runner();
        let result = runner(
            std::env::temp_dir(),
            vec!["rev-parse".to_string(), "--git-dir".to_string()],
            HashMap::new(),
        )
        .await;
        assert!(!result.success);
        assert!(
            result
                .stderr
                .to_lowercase()
                .contains("not a git repository")
        );
    }
}
