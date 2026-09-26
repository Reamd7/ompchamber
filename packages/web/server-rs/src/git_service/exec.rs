//! Git process execution layer — port of the `runGitCommand` / `buildGitEnv` /
//! `createGit` machinery in `server/lib/git/service.js`, plus the parts of
//! simple-git's task runner this module needs (`git.raw` semantics: fail only
//! when git exited non-zero *with* output on stderr, with the error message
//! carrying stdout+stderr concatenated).
//!
//! The runner is an injected closure (`GitRunner`) so tests can assert argv
//! construction and script git responses without a real repository.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

/// Result of one git invocation, mirroring the object `runGitCommand` returns
/// in JS (`{ success, exitCode, stdout, stderr, message }`).
#[derive(Debug, Clone, Default)]
pub struct GitCommandResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// `parseGitErrorText` of the underlying exec failure (empty on success).
    pub message: String,
    /// Raw stdout bytes — `String::from_utf8_lossy` of [`Self::stdout`] is not
    /// enough for `git show <blob>` of binary content (image data URLs).
    pub stdout_bytes: Vec<u8>,
}

impl GitCommandResult {
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

    pub fn stdout_trim(&self) -> &str {
        self.stdout.trim()
    }
}

/// Failure shape of a simple-git `git.raw` call that git answered with a
/// non-zero exit *and* stderr output. `message` is stdout+stderr concatenated
/// (simple-git's `getErrorMessage`), which is why `git diff --no-index`'s
/// patch-on-stdout is recoverable from `message` in the JS source.
#[derive(Debug, Clone)]
pub struct GitFailure {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub message: String,
}

/// JS `parseGitErrorText`: stderr, stdout, message, then `String(error)` —
/// each trimmed, non-empty chunks joined with `\n`.
pub fn parse_git_error_text(stderr: &str, stdout: &str, message: &str) -> String {
    [stderr.trim(), stdout.trim(), message.trim()]
        .iter()
        .filter(|chunk| !chunk.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_not_git_repository_text(text: &str) -> bool {
    text.to_lowercase().contains("not a git repository")
}

/// JS `isIndexLockError`: matches on the combined message/stderr/stdout.
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
pub fn is_missing_directory_text(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("directory that does not exist")
        || lowered.contains("does not exist")
        || lowered.contains("no such file or directory")
}

// ---------------------------------------------------------------------------
// Git binary resolution (`resolveGitBinary`)
// ---------------------------------------------------------------------------

static GIT_BINARY: LazyLock<String> = LazyLock::new(resolve_git_binary);

/// Non-Windows: always plain `git` (resolved through PATH by the OS).
/// Windows: honour `GIT_BINARY` / `OMPCHAMBER_GIT_BINARY` when the candidate
/// is an executable file, otherwise fall back to `git` (the Node-side
/// PATH/Program-Files sweep is launcher-specific and unnecessary when the
/// binary is already resolvable).
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

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn git_binary() -> &'static str {
    &GIT_BINARY
}

// ---------------------------------------------------------------------------
// SSH agent socket resolution (`resolveSshAuthSock` / `buildGitEnv`)
// ---------------------------------------------------------------------------

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

fn home() -> PathBuf {
    crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// JS `resolveSshAuthSock`: existing env wins, then `~/.gnupg/S.gpg-agent.ssh`,
/// then `gpgconf --list-dirs agent-ssh-socket` (launching the agent once and
/// retrying). Windows resolves nothing.
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

pub type GitSpawnFuture = Pin<Box<dyn Future<Output = GitCommandResult> + Send>>;

/// Injectable git runner: `(cwd, argv, env-overrides) -> result`.
pub type GitRunner =
    Arc<dyn Fn(PathBuf, Vec<String>, HashMap<String, String>) -> GitSpawnFuture + Send + Sync>;

/// The production runner — `execFile(git, args, { cwd, env, maxBuffer: 20MB })`
/// semantics: non-zero exit is a failure whose `message` is the trimmed join
/// of stderr/stdout; a missing cwd or binary is a spawn failure.
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

fn error_kind_text(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::NotADirectory => "ENOTDIR",
        _ => "EIO",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_error_text_joins_trimmed_chunks() {
        assert_eq!(
            parse_git_error_text(" fatal: bad ", "", "Command failed"),
            "fatal: bad\nCommand failed"
        );
        assert_eq!(parse_git_error_text("", "", ""), "");
    }

    #[test]
    fn not_git_repository_matches_case_insensitive() {
        assert!(is_not_git_repository_text(
            "Fatal: Not a Git repository (or any of the parent directories)"
        ));
        assert!(!is_not_git_repository_text("boom"));
    }

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
