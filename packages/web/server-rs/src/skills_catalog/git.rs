//! Port of `server/lib/skills-catalog/git.js`: non-interactive git exec
//! helpers (`runGit`, `assertGitAvailable`, `looksLikeAuthError`).
//!
//! `execFile('git', ...)` becomes a `GitRunner` trait so scan/install (and
//! tests) can drive a fake instead of the real binary. The real runner
//! mirrors Node's `execFile` semantics: inherited env plus
//! `GIT_TERMINAL_PROMPT=0`, a per-call timeout that kills the child, a
//! `maxBuffer` cap (4 MiB default) that fails the run, and result objects
//! shaped `{ ok, stdout, stderr, message, code, signal }`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::skills_catalog::error::CatalogError;

pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_MAX_BUFFER: usize = 4 * 1024 * 1024;

/// `{ sshKey }` — an optional git identity resolved from a profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIdentity {
    pub ssh_key: Option<String>,
}

/// `runGit(args, options)` options half.
#[derive(Debug, Clone, Default)]
pub struct GitRunOptions {
    pub cwd: Option<PathBuf>,
    pub timeout_ms: Option<u64>,
    pub max_buffer: Option<usize>,
    pub identity: Option<GitIdentity>,
}

impl GitRunOptions {
    fn timeout(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    fn max_buffer(&self) -> usize {
        self.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER)
    }
}

/// `runGit` result: `{ ok, stdout, stderr, message, code, signal }`
/// (`code` is only set for numeric exit codes; `signal` carries the POSIX
/// name when the child died by signal or was killed by the timeout).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub message: String,
    pub code: Option<i32>,
    pub signal: Option<String>,
}

impl GitResult {
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
pub trait GitRunner: Send + Sync {
    fn run(&self, args: &[String], options: &GitRunOptions) -> BoxFuture<'static, GitResult>;
}

/// The real `git` subprocess runner.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealGitRunner;
impl GitRunner for RealGitRunner {
    fn run(&self, args: &[String], options: &GitRunOptions) -> BoxFuture<'static, GitResult> {
        let args = args.to_vec();
        let options = options.clone();
        Box::pin(async move { run_git(&args, &options).await })
    }
}

/// `runGit(args, options)` against the real `git` binary.
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

async fn drain_tasks(
    stdout_task: tokio::task::JoinHandle<(Vec<u8>, bool)>,
    stderr_task: tokio::task::JoinHandle<(Vec<u8>, bool)>,
) -> (Vec<u8>, Vec<u8>) {
    let stdout = stdout_task.await.unwrap_or((Vec::new(), false)).0;
    let stderr = stderr_task.await.unwrap_or((Vec::new(), false)).0;
    (stdout, stderr)
}

/// Node's execFile failure message: `Command failed: git <args>` plus stderr.
fn command_failed_message(display_args: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("Command failed: git {display_args}")
    } else {
        format!("Command failed: git {display_args}\n{stderr}")
    }
}

#[cfg(unix)]
fn signal_name(status: std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
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

#[cfg(not(unix))]
fn signal_name(_status: std::process::ExitStatus) -> Option<String> {
    None
}

/// Read a piped stream up to `max` bytes; the flag reports whether the cap
/// was exceeded (Node's maxBuffer behavior).
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
pub(crate) fn resolve_runner(runner: Option<Arc<dyn GitRunner>>) -> Arc<dyn GitRunner> {
    runner.unwrap_or_else(|| Arc::new(RealGitRunner))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn assert_git_available_maps_failure() {
        struct FailingRunner;
        impl GitRunner for FailingRunner {
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
