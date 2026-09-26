//! Port of `opencode/claude-cli-auth.js`: the authoritative Claude CLI login
//! probe (`claude auth status --json`) with a login-shell PATH fallback.
//! The spawned environment is stripped of Anthropic/Claude credentials so
//! the CLI's own OAuth state is what answers.

use std::collections::BTreeMap;

use serde_json::Value;

/// Injection seam mirroring the JS `spawnSyncFn` dependency.
pub(crate) trait SyncSpawn: Send + Sync {
    /// Run `command args...` with `env` as the full child environment.
    /// Returns stdout and whether the spawn itself failed (ENOENT/timeout),
    /// mirroring Node's `spawnSync` result shape.
    fn spawn_sync(
        &self,
        command: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        timeout_ms: u64,
    ) -> SpawnOutcome;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnOutcome {
    pub stdout: String,
    pub error: bool,
}

/// Production spawn: full env replacement, bounded wait, kill on timeout.
pub(crate) struct CommandSpawnSync;

impl SyncSpawn for CommandSpawnSync {
    fn spawn_sync(
        &self,
        command: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        timeout_ms: u64,
    ) -> SpawnOutcome {
        let mut child = match std::process::Command::new(command)
            .args(args)
            .env_clear()
            .envs(env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                return SpawnOutcome {
                    stdout: String::new(),
                    error: true,
                };
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stdout = child
                        .stdout
                        .take()
                        .and_then(|mut out| {
                            use std::io::Read;
                            let mut buffer = String::new();
                            out.read_to_string(&mut buffer).ok()?;
                            Some(buffer)
                        })
                        .unwrap_or_default();
                    let error = !status.success() && stdout.trim().is_empty();
                    return SpawnOutcome { stdout, error };
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return SpawnOutcome {
                            stdout: String::new(),
                            error: true,
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return SpawnOutcome {
                        stdout: String::new(),
                        error: true,
                    };
                }
            }
        }
    }
}

fn read_status(
    spawn: &dyn SyncSpawn,
    command: &str,
    env: &BTreeMap<String, String>,
) -> SpawnOutcome {
    spawn.spawn_sync(command, &["auth", "status", "--json"], env, 6_000)
}

fn resolve_from_login_shell(
    spawn: &dyn SyncSpawn,
    env: &BTreeMap<String, String>,
    is_windows: bool,
) -> Option<String> {
    if is_windows {
        let result = spawn.spawn_sync("where", &["claude"], env, 6_000);
        return result
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);
    }
    let shell = env
        .get("SHELL")
        .cloned()
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let result = spawn.spawn_sync(&shell, &["-lic", "command -v claude"], env, 6_000);
    let trimmed = result.stdout.trim();
    let resolved = trimmed.split_whitespace().next_back()?;
    if resolved.is_empty() {
        None
    } else {
        Some(resolved.to_string())
    }
}

/// `getClaudeCliAuthStatus`.
pub(crate) fn get_claude_cli_auth_status(
    spawn: &dyn SyncSpawn,
    env: &BTreeMap<String, String>,
    is_windows: bool,
) -> ClaudeCliAuthStatus {
    let child_env: BTreeMap<String, String> = env
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    let mut result = read_status(spawn, "claude", &child_env);
    if result.stdout.trim().is_empty()
        && result.error
        && let Some(resolved) = resolve_from_login_shell(spawn, &child_env, is_windows)
    {
        result = read_status(spawn, &resolved, &child_env);
    }
    let output = result.stdout.trim().to_string();
    if output.is_empty() {
        return ClaudeCliAuthStatus {
            connected: false,
            reason: "empty-status".to_string(),
        };
    }
    match serde_json::from_str::<Value>(&output) {
        Ok(payload) => {
            let logged_in = payload.get("loggedIn") == Some(&Value::Bool(true));
            ClaudeCliAuthStatus {
                connected: logged_in,
                reason: if logged_in {
                    "logged-in".to_string()
                } else {
                    "logged-out".to_string()
                },
            }
        }
        Err(error) => ClaudeCliAuthStatus {
            connected: false,
            reason: error.to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeCliAuthStatus {
    pub connected: bool,
    pub reason: String,
}

/// Route-facing helper: run the probe off the async runtime (the production
/// spawn is blocking, bounded by the 6s spawn timeouts).
pub(crate) async fn claude_cli_auth_status() -> ClaudeCliAuthStatus {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let is_windows = cfg!(windows);
    tokio::task::spawn_blocking(move || {
        get_claude_cli_auth_status(&CommandSpawnSync, &env, is_windows)
    })
    .await
    .unwrap_or(ClaudeCliAuthStatus {
        connected: false,
        reason: "spawn task failed".to_string(),
    })
}
