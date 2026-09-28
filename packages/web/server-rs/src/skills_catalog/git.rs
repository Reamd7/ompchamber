//! Port of `server/lib/skills-catalog/git.js`: non-interactive git exec
//! helpers (`runGit`, `assertGitAvailable`, `looksLikeAuthError`).
//!
//! `execFile('git', ...)` becomes a `GitRunner` trait so scan/install (and
//! tests) can drive a fake instead of the real binary. The real runner
//! mirrors Node's `execFile` semantics: inherited env plus
//! `GIT_TERMINAL_PROMPT=0`, a per-call timeout that kills the child, a
//! `maxBuffer` cap (4 MiB default) that fails the run, and result objects
//! shaped `{ ok, stdout, stderr, message, code, signal }`.
//!
//! 中文说明：非交互式 git 子进程执行封装。`execFile('git', ...)` 被抽象
//! 为 `GitRunner` trait，便于 scan/install 与测试注入假实现。真实 runner
//! 复刻 Node `execFile` 语义：继承环境变量并附加 `GIT_TERMINAL_PROMPT=0`、
//! 超时杀死子进程、maxBuffer（默认 4 MiB）超限即失败，结果形如
//! `{ ok, stdout, stderr, message, code, signal }`。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::skills_catalog::error::CatalogError;

/// 单条 git 命令的默认超时（60s）。
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// stdout/stderr 各自的默认最大缓冲（4 MiB，对齐 Node maxBuffer）。
pub const DEFAULT_MAX_BUFFER: usize = 4 * 1024 * 1024;

/// `{ sshKey }` — an optional git identity resolved from a profile.
/// 从 profile 解析出的可选 git 身份；目前仅含 SSH key。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIdentity {
    /// SSH 私钥路径；非空时克隆走 SSH URL 并注入 `core.sshCommand`。
    pub ssh_key: Option<String>,
}

/// `runGit(args, options)` options half.
/// 每次调用的可选覆盖项，缺省回落到模块默认值。
#[derive(Debug, Clone, Default)]
pub struct GitRunOptions {
    /// 子进程工作目录（None → 继承当前进程）。
    pub cwd: Option<PathBuf>,
    /// 超时毫秒数（None → 60s 默认）。
    pub timeout_ms: Option<u64>,
    /// 输出缓冲上限（None → 4 MiB 默认）。
    pub max_buffer: Option<usize>,
    /// git 身份（SSH key 等），影响 argv 前缀。
    pub identity: Option<GitIdentity>,
}

/// 选项默认值的读取辅助。
impl GitRunOptions {
    /// 生效超时：显式值或 60s 默认。
    fn timeout(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    /// 生效缓冲上限：显式值或 4 MiB 默认。
    fn max_buffer(&self) -> usize {
        self.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER)
    }
}

/// `runGit` result: `{ ok, stdout, stderr, message, code, signal }`
/// (`code` is only set for numeric exit codes; `signal` carries the POSIX
/// name when the child died by signal or was killed by the timeout).
/// 与 JS 侧结果对象逐字段对齐；失败时 message 携带
/// `Command failed: git ...` 形式的文本。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitResult {
    /// 退出码为 0 且无缓冲超限即成功。
    pub ok: bool,
    /// 子进程 stdout（UTF-8 有损转换）。
    pub stdout: String,
    /// 子进程 stderr（UTF-8 有损转换）。
    pub stderr: String,
    /// 失败摘要：spawn 错误、超时、maxBuffer 或 execFile 风格失败消息。
    pub message: String,
    /// 数字退出码；被信号杀死或超时时为 None。
    pub code: Option<i32>,
    /// 致命信号名（如 SIGTERM）；正常退出为 None。
    pub signal: Option<String>,
}

/// GitResult 的构造辅助。
impl GitResult {
    /// 构造 ok: true 的成功结果。
    pub fn success(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        GitResult {
            ok: true,
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..GitResult::default()
        }
    }
}

/// `looksLikeAuthError(message)`.
/// 大小写不敏感地匹配常见 git 认证失败标记（permission denied、
/// publickey、could not read from remote repository 等），用于把克隆失败
/// 映射为 authRequired。
pub fn looks_like_auth_error(message: &str) -> bool {
    let text = message.to_lowercase();
    text.contains("permission denied")
        || text.contains("publickey")
        || text.contains("could not read from remote repository")
        || text.contains("authentication failed")
        || text.contains("fatal: could not")
}

/// Build the final argv for the git binary: when an SSH identity is present
/// git gets `-c core.sshCommand=...` prepended (non-interactive, host keys
/// auto-accepted). Exposed for unit testing the argv shape.
/// 携带非空白 SSH key 时在 argv 前插入 `-c core.sshCommand=...`（BatchMode
/// 非交互、host key 自动接受）；否则原样返回参数。
pub fn build_git_args(args: &[String], identity: Option<&GitIdentity>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(identity) = identity
        && let Some(key) = identity.ssh_key.as_deref()
    {
        let key = key.trim();
        if !key.is_empty() {
            out.push("-c".to_string());
            out.push(format!(
                "core.sshCommand=ssh -i {key} -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
            ));
        }
    }
    out.extend(args.iter().cloned());
    out
}

/// Seam over `execFile('git', ...)`.
/// 对 `execFile('git', ...)` 的抽象缝，真实实现与测试假体都实现它。
pub trait GitRunner: Send + Sync {
    /// 以给定参数与选项执行一次 git 命令并返回结果。
    fn run(&self, args: &[String], options: &GitRunOptions) -> BoxFuture<'static, GitResult>;
}

/// The real `git` subprocess runner.
/// 走真实 `git` 二进制的零状态 runner。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealGitRunner;
/// GitRunner 实现：转发到自由函数 `run_git`。
impl GitRunner for RealGitRunner {
    /// 按参数与选项执行真实 git（见 `run_git`）。
    fn run(&self, args: &[String], options: &GitRunOptions) -> BoxFuture<'static, GitResult> {
        let args = args.to_vec();
        let options = options.clone();
        Box::pin(async move { run_git(&args, &options).await })
    }
}

/// `runGit(args, options)` against the real `git` binary.
/// 组装 argv（含 sshCommand 前缀）→ spawn（stdin 关闭、stdout/stderr 管道、
/// GIT_TERMINAL_PROMPT=0）→ 限时等待并读取双侧输出；超时杀进程记
/// SIGTERM，缓冲超限记 maxBuffer 错误，非零退出记 execFile 风格消息。
pub async fn run_git(args: &[String], options: &GitRunOptions) -> GitResult {
    let final_args = build_git_args(args, options.identity.as_ref());
    let display_args = final_args.join(" ");
    let mut command = Command::new("git");
    command
        .args(&final_args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        // Node: "spawn git ENOENT"; other spawn failures surface their text.
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                "ENOENT".to_string()
            } else {
                error.to_string()
            };
            return GitResult {
                message: format!("spawn git {code}"),
                ..GitResult::default()
            };
        }
    };

    let max_buffer = options.max_buffer();
    let stdout_task = tokio::spawn(read_capped(child.stdout.take(), max_buffer));
    let stderr_task = tokio::spawn(read_capped(child.stderr.take(), max_buffer));

    // Node kills the child when the timeout elapses; the error then carries
    // signal 'SIGTERM' and no exit code.
    let mut timed_out = false;
    let status =
        match tokio::time::timeout(Duration::from_millis(options.timeout()), child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                let (stdout, _) = drain_tasks(stdout_task, stderr_task).await;
                return GitResult {
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    message: error.to_string(),
                    ..GitResult::default()
                };
            }
            Err(_) => {
                timed_out = true;
                let _ = child.start_kill();
                child.wait().await.ok()
            }
        };

    let (stdout, stdout_exceeded) = stdout_task.await.unwrap_or((Vec::new(), false));
    let (stderr, stderr_exceeded) = stderr_task.await.unwrap_or((Vec::new(), false));
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();

    if stdout_exceeded || stderr_exceeded {
        let _ = child.start_kill();
        let stream = if stdout_exceeded { "stdout" } else { "stderr" };
        return GitResult {
            ok: false,
            stdout,
            stderr,
            message: format!("{stream} maxBuffer {max_buffer} exceeded"),
            code: None,
            signal: None,
        };
    }

    if timed_out {
        return GitResult {
            ok: false,
            stdout,
            stderr: stderr.clone(),
            message: command_failed_message(&display_args, &stderr),
            code: None,
            signal: Some("SIGTERM".to_string()),
        };
    }

    let Some(status) = status else {
        // Unreachable in practice (wait errors return above and the timeout
        // arm returned already), but never fabricate a success.
        return GitResult {
            ok: false,
            stdout,
            stderr,
            message: "git process produced no exit status".to_string(),
            code: None,
            signal: None,
        };
    };
    if status.success() {
        GitResult::success(stdout, stderr)
    } else {
        GitResult {
            ok: false,
            stdout,
            stderr: stderr.clone(),
            message: command_failed_message(&display_args, &stderr),
            code: status.code(),
            signal: signal_name(status),
        }
    }
}

/// 等待并收拢 stdout/stderr 读取任务，Join 失败时返回空缓冲。
async fn drain_tasks(
    stdout_task: tokio::task::JoinHandle<(Vec<u8>, bool)>,
    stderr_task: tokio::task::JoinHandle<(Vec<u8>, bool)>,
) -> (Vec<u8>, Vec<u8>) {
    let stdout = stdout_task.await.unwrap_or((Vec::new(), false)).0;
    let stderr = stderr_task.await.unwrap_or((Vec::new(), false)).0;
    (stdout, stderr)
}

/// Node's execFile failure message: `Command failed: git <args>` plus stderr.
/// stderr 非空时附加在 `Command failed: git <args>` 之后，复刻 Node。
fn command_failed_message(display_args: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("Command failed: git {display_args}")
    } else {
        format!("Command failed: git {display_args}\n{stderr}")
    }
}

/// 把 Unix 退出状态中的信号编号映射为惯用名称（未列出的编号记
/// `SIG<n>`）；正常退出为 None。
#[cfg(unix)]
fn signal_name(status: std::process::ExitStatus) -> Option<String> {
use crate::os_compat::ExitStatusExt;
    status.signal().map(|signal| match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        4 => "SIGILL".to_string(),
        6 => "SIGABRT".to_string(),
        8 => "SIGFPE".to_string(),
        9 => "SIGKILL".to_string(),
        11 => "SIGSEGV".to_string(),
        13 => "SIGPIPE".to_string(),
        14 => "SIGALRM".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("SIG{other}"),
    })
}

/// 非 Unix 平台没有信号语义，恒为 None。
#[cfg(not(unix))]
fn signal_name(_status: std::process::ExitStatus) -> Option<String> {
    None
}

/// Read a piped stream up to `max` bytes; the flag reports whether the cap
/// was exceeded (Node's maxBuffer behavior).
/// 分块读取管道至多 `max` 字节，读满即截断并置超限标志（Node 的
/// maxBuffer 行为）；管道缺失或读错误时返回已读数据。
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    max: usize,
) -> (Vec<u8>, bool) {
    let mut data = Vec::new();
    let Some(mut pipe) = pipe else {
        return (data, false);
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => return (data, false),
            Ok(n) => {
                data.extend_from_slice(&chunk[..n]);
                if data.len() > max {
                    data.truncate(max);
                    return (data, true);
                }
            }
            Err(_) => return (data, false),
        }
    }
}

/// `assertGitAvailable()`: `git --version` with a 5s budget.
/// 用 5s 预算跑 `git --version` 探测可用性；失败映射为 gitUnavailable。
pub async fn assert_git_available(git: &dyn GitRunner) -> Result<(), CatalogError> {
    let result = git
        .run(
            &["--version".to_string()],
            &GitRunOptions {
                timeout_ms: Some(5_000),
                ..GitRunOptions::default()
            },
        )
        .await;
    if !result.ok {
        return Err(CatalogError::git_unavailable());
    }
    Ok(())
}

/// Resolve the runner to use: an injected fake or the real binary.
/// 有注入的 runner 用之，否则落到真实二进制 runner。
pub(crate) fn resolve_runner(runner: Option<Arc<dyn GitRunner>>) -> Arc<dyn GitRunner> {
    runner.unwrap_or_else(|| Arc::new(RealGitRunner))
}

/// git 封装测试：认证错误识别、argv 组装、真实子进程的成功/失败/超时/
/// maxBuffer 行为与可用性探测映射。
#[cfg(test)]
mod tests {
    use super::*;

    /// 行为契约：各类认证失败标记都被识别。
    #[test]
    fn detects_auth_error_markers() {
        for message in [
            "fatal: Permission denied (publickey).",
            "ERROR: permission denied to repo",
            "Publickey",
            "fatal: could not read from remote repository.",
            "fatal: Could not read from remote repository.",
            "remote: Authentication failed for https://github.com/x/y",
        ] {
            assert!(looks_like_auth_error(message), "should match: {message}");
        }
    }

    /// 行为契约：普通错误与空串不会被误判为认证失败。
    #[test]
    fn non_auth_errors_are_not_flagged() {
        for message in [
            "fatal: unable to access 'https://x/': connection timed out",
            "error: pathspec 'foo' did not match",
            "",
        ] {
            assert!(
                !looks_like_auth_error(message),
                "should not match: {message}"
            );
        }
    }

    /// 行为契约：SSH key 注入 core.sshCommand 前缀且保持参数顺序。
    #[test]
    fn ssh_identity_prepends_core_ssh_command() {
        let args = vec!["clone".to_string(), "url".to_string(), "dst".to_string()];
        let identity = GitIdentity {
            ssh_key: Some("/keys/id_ed25519".to_string()),
        };
        let built = build_git_args(&args, Some(&identity));
        assert_eq!(
            built,
            vec![
                "-c".to_string(),
                "core.sshCommand=ssh -i /keys/id_ed25519 -o BatchMode=yes -o StrictHostKeyChecking=accept-new".to_string(),
                "clone".to_string(),
                "url".to_string(),
                "dst".to_string(),
            ]
        );
    }

    /// 行为契约：空白 key、无 key、无身份都不改变 argv。
    #[test]
    fn blank_ssh_keys_are_ignored() {
        let args = vec!["status".to_string()];
        let identity = GitIdentity {
            ssh_key: Some("   ".to_string()),
        };
        assert_eq!(build_git_args(&args, Some(&identity)), args);
        assert_eq!(build_git_args(&args, Some(&GitIdentity::default())), args);
        assert_eq!(build_git_args(&args, None), args);
    }

    /// 行为契约：真实 `git --version` 成功且 stdout 含版本串。
    #[tokio::test]
    async fn real_git_version_succeeds() {
        let result = run_git(
            &["--version".to_string()],
            &GitRunOptions {
                timeout_ms: Some(5_000),
                ..GitRunOptions::default()
            },
        )
        .await;
        assert!(result.ok, "git --version should succeed: {result:?}");
        assert!(result.stdout.contains("git version"));
    }

    /// 行为契约：失败的子命令返回非零 code 与 execFile 风格消息。
    #[tokio::test]
    async fn failing_git_reports_stderr_and_exit_code() {
        let result = run_git(&["nosuchsubcommand".to_string()], &GitRunOptions::default()).await;
        assert!(!result.ok);
        assert!(result.code.unwrap_or(0) != 0);
        assert!(
            result
                .message
                .starts_with("Command failed: git nosuchsubcommand")
        );
    }

    /// 行为契约：1ms 超时预算下子进程被 SIGTERM 杀死且无退出码。
    #[tokio::test]
    async fn timeout_kills_the_child_with_sigterm() {
        // A 1ms budget cannot fit git's spawn+exit; the child is killed with
        // SIGTERM exactly like Node's execFile timeout.
        let result = run_git(
            &["--version".to_string()],
            &GitRunOptions {
                timeout_ms: Some(1),
                ..GitRunOptions::default()
            },
        )
        .await;
        assert!(!result.ok);
        assert_eq!(
            result.signal.as_deref(),
            Some("SIGTERM"),
            "result: {result:?}"
        );
    }

    /// 行为契约：stdout 超过 maxBuffer 上限时运行失败并报超限消息。
    #[tokio::test]
    async fn max_buffer_overflow_fails_the_run() {
        let result = run_git(
            &["version".to_string()],
            &GitRunOptions {
                max_buffer: Some(4),
                ..GitRunOptions::default()
            },
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.message, "stdout maxBuffer 4 exceeded");
    }

    /// 行为契约：runner 失败时 assert_git_available 返回 gitUnavailable。
    #[tokio::test]
    async fn assert_git_available_maps_failure() {
        // 恒失败的假 runner，模拟 git 不在 PATH。
        struct FailingRunner;
        // 直接返回 spawn 失败语义的 GitResult。
        impl GitRunner for FailingRunner {
            // 无视参数，总是失败。
            fn run(
                &self,
                _args: &[String],
                _options: &GitRunOptions,
            ) -> BoxFuture<'static, GitResult> {
                Box::pin(async {
                    GitResult {
                        message: "spawn git ENOENT".to_string(),
                        ..GitResult::default()
                    }
                })
            }
        }
        let error = assert_git_available(&FailingRunner)
            .await
            .expect_err("fails");
        assert_eq!(error.kind, "gitUnavailable");
        assert_eq!(error.message, "Git is not available in PATH");
    }
}
