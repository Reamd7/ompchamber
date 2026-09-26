//! Background command execution — port of routes.js `runCommandInDirectory`,
//! the git-read cache (`runCommandWithGitReadCache`), and the exec job store
//! (`execJobs` + `pruneExecJobs` + `runExecJob`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::paths::random_uuid;

pub const EXEC_JOB_TTL_MS: u64 = 30 * 60 * 1000;
const GIT_READ_CACHE_MAX_ENTRIES: usize = 500;
const GIT_READ_CACHE_MAX_BYTES: usize = 1024 * 1024;
const GIT_READ_FLAGS: [&str; 3] = ["--absolute-git-dir", "--git-common-dir", "--show-toplevel"];

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `Number(raw)` with the JS `Number.isFinite && raw > 0` gate.
fn env_ms_positive(raw: Option<&str>, default: u64) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value > 0.0 => value as u64,
        _ => default,
    }
}

/// `Number(raw)` with the JS `Number.isFinite && raw >= 0` gate.
fn env_ms_non_negative(raw: Option<&str>, default: u64) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value >= 0.0 => value as u64,
        _ => default,
    }
}

pub fn command_timeout_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_positive(raw, 5 * 60 * 1000)
}

pub fn git_read_cache_ttl_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_non_negative(raw, 30 * 1000)
}

pub fn git_check_ignore_timeout_ms_from_raw(raw: Option<&str>) -> u64 {
    env_ms_non_negative(raw, 2500)
}

pub fn upload_max_bytes_from_raw(raw: Option<&str>) -> u64 {
    match raw.and_then(|value| value.trim().parse::<f64>().ok()) {
        Some(value) if value.is_finite() && value > 0.0 => value.floor() as u64,
        _ => 100 * 1024 * 1024,
    }
}

/// routes.js `createCommandTimeoutMs` (OMPCHAMBER_FS_EXEC_TIMEOUT_MS, default
/// 5 min). Read once per server process, like the JS.
pub fn command_timeout_ms() -> u64 {
    command_timeout_ms_from_raw(
        std::env::var("OMPCHAMBER_FS_EXEC_TIMEOUT_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createGitReadCacheTtlMs` (OMPCHAMBER_GIT_READ_CACHE_TTL_MS,
/// default 30 s; 0 disables caching).
pub fn git_read_cache_ttl_ms() -> u64 {
    git_read_cache_ttl_ms_from_raw(
        std::env::var("OMPCHAMBER_GIT_READ_CACHE_TTL_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createGitCheckIgnoreTimeoutMs` (default 2500 ms).
pub fn git_check_ignore_timeout_ms() -> u64 {
    git_check_ignore_timeout_ms_from_raw(
        std::env::var("OMPCHAMBER_GIT_CHECK_IGNORE_TIMEOUT_MS")
            .ok()
            .as_deref(),
    )
}

/// routes.js `createUploadMaxBytes` (OMPCHAMBER_FS_UPLOAD_MAX_BYTES, default
/// 100 MiB). Read per request, like the JS.
pub fn upload_max_bytes() -> u64 {
    upload_max_bytes_from_raw(
        std::env::var("OMPCHAMBER_FS_UPLOAD_MAX_BYTES")
            .ok()
            .as_deref(),
    )
}

/// routes.js `normalizeCommand`.
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// routes.js `isCacheableGitReadCommand`: only deterministic
/// `git rev-parse` plumbing queries with 1–3 allowlisted flags.
pub fn is_cacheable_git_read_command(command: &str) -> bool {
    let normalized = normalize_command(command);
    let tokens: Vec<&str> = normalized.split(' ').collect();
    if tokens.len() < 3 || tokens.len() > 5 {
        return false;
    }
    if tokens[0] != "git" || tokens[1] != "rev-parse" {
        return false;
    }
    tokens[2..].iter().all(|flag| GIT_READ_FLAGS.contains(flag))
}

/// One executed command. `to_json` applies the exact JS field presence rules
/// (`exitCode`/`error` omitted when unknown).
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

impl CommandOutcome {
    /// JS pushes `{ command, success: false, error: 'Invalid command' }` for
    /// non-string/blank entries — no stdout/stderr/exitCode fields.
    pub fn invalid_command_json(command: &Value) -> Value {
        json!({
            "command": command,
            "success": false,
            "error": "Invalid command",
        })
    }

    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("command".into(), json!(self.command));
        object.insert("success".into(), json!(self.success));
        if let Some(exit_code) = self.exit_code {
            object.insert("exitCode".into(), json!(exit_code));
        }
        object.insert("stdout".into(), json!(self.stdout));
        object.insert("stderr".into(), json!(self.stderr));
        if let Some(error) = &self.error {
            object.insert("error".into(), json!(error));
        }
        Value::Object(object)
    }
}

/// routes.js `runCommandInDirectory`: `shell -c command` in `resolved_cwd`
/// with a hard SIGKILL deadline. Always resolves — failures travel inside the
/// outcome (`success: false` + `error`), never as exceptions.
pub async fn run_command_in_directory(
    shell: &str,
    shell_flag: &str,
    command: &str,
    resolved_cwd: &Path,
    timeout_ms: u64,
) -> CommandOutcome {
    let spawn_result = tokio::process::Command::new(shell)
        .arg(shell_flag)
        .arg(command)
        .current_dir(resolved_cwd)
        // The JS swaps PATH for buildAugmentedPath() (login-shell PATH
        // merge); the Rust port inherits the parent PATH as-is.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawn_result {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(error.to_string()),
            };
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Deadline loop keeps the child handle alive so the timeout arm can
    // SIGKILL it and still reap the exit status (JS: kill then 'close').
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let wait_result = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(status) => status,
        Err(_elapsed) => {
            timed_out = true;
            let _ = child.start_kill();
            child.wait().await
        }
    };

    async fn drain(pipe: &mut Option<impl tokio::io::AsyncRead + Unpin>) -> String {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut bytes).await;
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let (stdout, stderr) = futures::join!(drain(&mut stdout_pipe), drain(&mut stderr_pipe));

    let exit_code = wait_result.as_ref().ok().and_then(|status| status.code());
    let base = CommandOutcome {
        command: command.to_string(),
        success: exit_code == Some(0) && !timed_out,
        exit_code,
        stdout: stdout.trim().to_string(),
        stderr: stderr.trim().to_string(),
        error: None,
    };
    if timed_out {
        let signal = wait_result.ok().as_ref().and_then(unix_signal_name);
        return CommandOutcome {
            error: Some(format!(
                "Command timed out after {timeout_ms}ms{}",
                signal.map(|s| format!(" ({s})")).unwrap_or_default()
            )),
            ..base
        };
    }
    base
}

#[cfg(unix)]
fn unix_signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    let signal = status.signal()?;
    Some(match signal {
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("SIG{other}"),
    })
}

#[cfg(not(unix))]
fn unix_signal_name(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

struct GitReadEntry {
    at: u64,
    result: CommandOutcome,
}

pub struct GitReadCache {
    ttl_ms: u64,
    entries: Mutex<Vec<(String, GitReadEntry)>>,
    in_flight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl GitReadCache {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            entries: Mutex::new(Vec::new()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    fn entry_bytes(key: &str, result: &CommandOutcome) -> usize {
        key.len() + result.stdout.len() + result.stderr.len()
    }

    /// Insert with LRU (oldest-first) eviction enforcing the JS dual
    /// count+bytes caps.
    fn set_entry(&self, key: String, result: CommandOutcome) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(existing, _)| existing != &key);
        entries.push((
            key,
            GitReadEntry {
                at: now_ms(),
                result,
            },
        ));
        let mut total_bytes: usize = entries
            .iter()
            .map(|(k, e)| Self::entry_bytes(k, &e.result))
            .sum();
        while entries.len() > GIT_READ_CACHE_MAX_ENTRIES
            || (total_bytes > GIT_READ_CACHE_MAX_BYTES && entries.len() > 1)
        {
            let Some((oldest_key, oldest)) = entries.first() else {
                break;
            };
            let oldest_bytes = Self::entry_bytes(oldest_key, &oldest.result);
            entries.remove(0);
            total_bytes = total_bytes.saturating_sub(oldest_bytes);
        }
    }

    /// Fresh hit, refreshed to most-recently-used without altering its age.
    fn fresh_entry(&self, key: &str) -> Option<CommandOutcome> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let index = entries.iter().position(|(existing, _)| existing == key)?;
        if now_ms().saturating_sub(entries[index].1.at) >= self.ttl_ms {
            entries.remove(index);
            return None;
        }
        let (key, entry) = entries.remove(index);
        let result = entry.result;
        entries.push((
            key,
            GitReadEntry {
                at: entry.at,
                result: result.clone(),
            },
        ));
        Some(result)
    }

    /// routes.js `pruneGitReadCache`.
    pub fn prune(&self) {
        if self.ttl_ms == 0 {
            return;
        }
        let now = now_ms();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, entry)| now.saturating_sub(entry.at) < self.ttl_ms);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// routes.js `runCommandWithGitReadCache`: serve/store allowlisted
    /// git-read results and dedupe concurrent identical runs.
    pub async fn run(
        self: &Arc<Self>,
        shell: &str,
        shell_flag: &str,
        command: &str,
        resolved_cwd: &Path,
        timeout_ms: u64,
    ) -> CommandOutcome {
        let cacheable = self.ttl_ms > 0 && is_cacheable_git_read_command(command);
        let cache_key = cacheable.then(|| {
            format!(
                "{}{}",
                resolved_cwd.to_string_lossy(),
                normalize_command(command)
            )
        });

        let Some(cache_key) = cache_key else {
            return run_command_in_directory(shell, shell_flag, command, resolved_cwd, timeout_ms)
                .await;
        };

        if let Some(cached) = self.fresh_entry(&cache_key) {
            return CommandOutcome {
                command: command.to_string(),
                ..cached
            };
        }

        // In-flight dedupe: the first caller runs, the rest wait on the same
        // key lock and then read the successful result from the cache.
        let lock = {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            if in_flight.len() > 1024 {
                in_flight.clear();
            }
            Arc::clone(in_flight.entry(cache_key.clone()).or_default())
        };
        let _guard = lock.lock().await;

        if let Some(cached) = self.fresh_entry(&cache_key) {
            return CommandOutcome {
                command: command.to_string(),
                ..cached
            };
        }

        let result =
            run_command_in_directory(shell, shell_flag, command, resolved_cwd, timeout_ms).await;
        // Only cache successful results — failures may be transient.
        if result.success {
            self.set_entry(cache_key, result.clone());
        }
        result
    }
}

#[derive(Debug, Clone)]
pub struct ExecJob {
    pub job_id: String,
    pub status: &'static str,
    pub success: Option<bool>,
    pub resolved_cwd: std::path::PathBuf,
    pub results: Vec<Value>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub updated_at: u64,
}

#[derive(Default)]
pub struct ExecJobStore {
    jobs: Mutex<HashMap<String, Arc<Mutex<ExecJob>>>>,
}

impl ExecJobStore {
    /// routes.js `pruneExecJobs`.
    pub fn prune(&self) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        jobs.retain(|_, job| {
            let updated_at = job.lock().unwrap_or_else(|e| e.into_inner()).updated_at;
            updated_at == 0 || now.saturating_sub(updated_at) <= EXEC_JOB_TTL_MS
        });
    }

    pub fn insert(&self, job: ExecJob) -> Arc<Mutex<ExecJob>> {
        let shared = Arc::new(Mutex::new(job));
        let job_id = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job_id
            .clone();
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id, Arc::clone(&shared));
        shared
    }

    pub fn get(&self, job_id: &str) -> Option<Arc<Mutex<ExecJob>>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(job_id)
            .cloned()
    }

    /// routes.js `runExecJob` for the inline (non-background) path. Updates
    /// the stored job after every command so a concurrent
    /// `GET /api/fs/exec/:jobId` observes progress.
    pub async fn run_inline(
        &self,
        job: Arc<Mutex<ExecJob>>,
        commands: &[Value],
        shell: &str,
        shell_flag: &str,
        cache: &Arc<GitReadCache>,
        timeout_ms: u64,
    ) -> (bool, Vec<Value>) {
        {
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.status = "running";
            guard.updated_at = now_ms();
        }

        let resolved_cwd = job
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resolved_cwd
            .clone();
        let mut results: Vec<Value> = Vec::new();
        for command in commands {
            let invalid = match command {
                Value::String(text) => text.trim().is_empty(),
                _ => true,
            };
            if invalid {
                results.push(CommandOutcome::invalid_command_json(command));
            } else {
                let command_text = command.as_str().unwrap_or_default().to_string();
                let outcome = cache
                    .run(shell, shell_flag, &command_text, &resolved_cwd, timeout_ms)
                    .await;
                results.push(outcome.to_json());
            }
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.results = results.clone();
            guard.updated_at = now_ms();
        }

        let success = results.iter().all(|result| {
            result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        {
            let mut guard = job.lock().unwrap_or_else(|e| e.into_inner());
            guard.results = results.clone();
            guard.success = Some(success);
            guard.status = "done";
            guard.finished_at = Some(now_ms());
            guard.updated_at = now_ms();
        }
        (success, results)
    }
}

/// Shell resolution: `process.env.SHELL` (win fallback `cmd.exe`), flag `-c`
/// (`/c` on Windows).
pub fn resolve_shell() -> (String, &'static str) {
    if cfg!(windows) {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "cmd.exe".to_string()),
            "/c",
        )
    } else {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            "-c",
        )
    }
}

pub fn new_job_id() -> String {
    random_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsers_mirror_js_gates() {
        assert_eq!(command_timeout_ms_from_raw(Some("1500")), 1500);
        assert_eq!(command_timeout_ms_from_raw(Some("0")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(Some("-5")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(Some("abc")), 300_000);
        assert_eq!(command_timeout_ms_from_raw(None), 300_000);

        assert_eq!(git_read_cache_ttl_ms_from_raw(Some("0")), 0);
        assert_eq!(git_read_cache_ttl_ms_from_raw(Some("x")), 30_000);

        assert_eq!(upload_max_bytes_from_raw(Some("5")), 5);
        assert_eq!(upload_max_bytes_from_raw(Some("5.9")), 5);
        assert_eq!(upload_max_bytes_from_raw(Some("0")), 100 * 1024 * 1024);
        assert_eq!(upload_max_bytes_from_raw(None), 100 * 1024 * 1024);
    }

    #[test]
    fn cacheable_git_read_allowlist_matches_js_regex() {
        assert!(is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir"
        ));
        assert!(is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir --git-common-dir --show-toplevel"
        ));
        assert!(is_cacheable_git_read_command(
            "git   rev-parse   --show-toplevel"
        ));
        assert!(!is_cacheable_git_read_command("git rev-parse"));
        assert!(!is_cacheable_git_read_command("git rev-parse --git-dir"));
        assert!(!is_cacheable_git_read_command("git status"));
        assert!(!is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir && rm -rf /"
        ));
        assert!(!is_cacheable_git_read_command(
            "git rev-parse --absolute-git-dir --git-common-dir --show-toplevel --absolute-git-dir"
        ));
    }

    #[test]
    fn outcome_json_omits_unknown_fields() {
        let full = CommandOutcome {
            command: "id".into(),
            success: true,
            exit_code: Some(0),
            stdout: "0".into(),
            stderr: String::new(),
            error: None,
        };
        assert_eq!(
            full.to_json(),
            json!({"command": "id", "success": true, "exitCode": 0, "stdout": "0", "stderr": ""})
        );

        let spawn_error = CommandOutcome {
            command: "id".into(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("spawn failed".into()),
        };
        assert_eq!(
            spawn_error.to_json(),
            json!({"command": "id", "success": false, "stdout": "", "stderr": "", "error": "spawn failed"})
        );

        assert_eq!(
            CommandOutcome::invalid_command_json(&json!(42)),
            json!({"command": 42, "success": false, "error": "Invalid command"})
        );
    }

    #[tokio::test]
    async fn runs_a_command_and_captures_trimmed_output() {
        let outcome = run_command_in_directory(
            "/bin/sh",
            "-c",
            "printf '  out  ' ; printf ' err ' >&2 ; exit 0",
            Path::new("/tmp"),
            10_000,
        )
        .await;
        assert!(outcome.success);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, "out");
        assert_eq!(outcome.stderr, "err");
        assert!(outcome.error.is_none());
    }

    #[tokio::test]
    async fn failed_spawn_resolves_with_error() {
        let outcome = run_command_in_directory(
            "/nonexistent-shell-fsport",
            "-c",
            "true",
            Path::new("/tmp"),
            1_000,
        )
        .await;
        assert!(!outcome.success);
        assert!(outcome.error.is_some());
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let outcome =
            run_command_in_directory("/bin/sh", "-c", "sleep 5", Path::new("/tmp"), 150).await;
        assert!(!outcome.success);
        let error = outcome.error.expect("timeout error");
        assert!(
            error.starts_with("Command timed out after 150ms"),
            "{error}"
        );
    }

    fn sample_outcome() -> CommandOutcome {
        CommandOutcome {
            command: "git rev-parse --absolute-git-dir".into(),
            success: true,
            exit_code: Some(0),
            stdout: "/repo/.git".into(),
            stderr: String::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn git_read_cache_serves_and_misses_by_key() {
        let cache = GitReadCache::new(60_000);
        cache.set_entry("/repo".into(), sample_outcome());
        let cached = cache.fresh_entry("/repo").expect("fresh entry");
        assert_eq!(cached.stdout, "/repo/.git");
        assert!(cache.fresh_entry("/other").is_none());
    }

    #[tokio::test]
    async fn git_read_cache_evicts_over_the_entry_cap() {
        let cache = GitReadCache::new(60_000);
        for index in 0..GIT_READ_CACHE_MAX_ENTRIES {
            cache.set_entry(format!("/repo/{index}"), sample_outcome());
        }
        assert_eq!(cache.len(), GIT_READ_CACHE_MAX_ENTRIES);
        cache.set_entry("/repo/overflow".into(), sample_outcome());
        assert_eq!(cache.len(), GIT_READ_CACHE_MAX_ENTRIES);
        assert!(
            cache.fresh_entry("/repo/0").is_none(),
            "oldest entry evicted"
        );
        assert!(cache.fresh_entry("/repo/overflow").is_some());
    }

    #[tokio::test]
    async fn exec_jobs_prune_by_ttl() {
        let store = ExecJobStore::default();
        store.insert(ExecJob {
            job_id: "job-old".into(),
            status: "done",
            success: Some(true),
            resolved_cwd: Path::new("/repo").to_path_buf(),
            results: vec![],
            started_at: 0,
            finished_at: None,
            updated_at: now_ms() - EXEC_JOB_TTL_MS - 5_000,
        });
        store.insert(ExecJob {
            job_id: "job-fresh".into(),
            status: "done",
            success: Some(true),
            resolved_cwd: Path::new("/repo").to_path_buf(),
            results: vec![],
            started_at: 0,
            finished_at: None,
            updated_at: now_ms(),
        });
        store.prune();
        assert!(store.get("job-old").is_none());
        assert!(store.get("job-fresh").is_some());
    }
}
