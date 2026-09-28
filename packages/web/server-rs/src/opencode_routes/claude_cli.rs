//! Port of `opencode/claude-cli-auth.js`: the authoritative Claude CLI login
//! probe (`claude auth status --json`) with a login-shell PATH fallback.
//! The spawned environment is stripped of Anthropic/Claude credentials so
//! the CLI's own OAuth state is what answers.
//!
//! 中文说明：移植 `opencode/claude-cli-auth.js`：以 `claude auth status
//! --json` 作为 Claude CLI 登录态的权威探测，直接执行失败时回退到
//! 登录 shell（zsh/bash -lic）里 `command -v claude` 解析出的路径再试。
//! 子进程环境会剔除 Anthropic/Claude 相关凭据变量，确保应答来自 CLI
//! 自身的 OAuth 状态而非环境变量。

use std::collections::BTreeMap;

use serde_json::Value;

/// Injection seam mirroring the JS `spawnSyncFn` dependency.
/// 中文：同步子进程执行抽象（对应 JS 注入的 `spawnSyncFn`），
/// 测试用假实现替换以避免真实 spawn。
pub(crate) trait SyncSpawn: Send + Sync {
    /// Run `command args...` with `env` as the full child environment.
    /// Returns stdout and whether the spawn itself failed (ENOENT/timeout),
    /// mirroring Node's `spawnSync` result shape.
    /// 中文：以 `env` 为完整子进程环境执行 `command args...`，最多等待
    /// `timeout_ms` 毫秒；返回 stdout 及 spawn 本身是否失败
    /// （找不到命令/超时），对齐 Node `spawnSync` 的返回形态。
    fn spawn_sync(
        &self,
        command: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        timeout_ms: u64,
    ) -> SpawnOutcome;
}

/// 中文：spawn 结果：stdout 文本 + spawn 是否失败（ENOENT/超时），
/// 对应 Node `spawnSync` 返回对象的子集。
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnOutcome {
    /// 子进程 stdout（失败时为空串）。
    pub stdout: String,
    /// spawn 本身是否失败（启动失败/超时/等待出错）。
    pub error: bool,
}

/// Production spawn: full env replacement, bounded wait, kill on timeout.
/// 中文：生产实现：清空并替换子进程环境、管道捕获 stdout、按超时
/// 上限轮询等待，超时或出错即 kill。
pub(crate) struct CommandSpawnSync;

/// 中文：`CommandSpawnSync` 对 [`SyncSpawn`] 的生产实现。
impl SyncSpawn for CommandSpawnSync {
    /// 中文：spawn `claude` 子进程并限时收集 stdout：启动失败、超时、
    /// wait 出错都折叠为 `error: true` + 空 stdout；非零退出码仅在
    /// stdout 为空时视为失败（CLI 会把 JSON 打到 stdout）。
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

/// 中文：执行 `command auth status --json`（6 秒上限）并返回结果。
fn read_status(
    spawn: &dyn SyncSpawn,
    command: &str,
    env: &BTreeMap<String, String>,
) -> SpawnOutcome {
    spawn.spawn_sync(command, &["auth", "status", "--json"], env, 6_000)
}

/// 中文：登录 shell 回退解析：Windows 用 `where claude` 取首个命中；
/// 类 Unix 用 `$SHELL -lic 'command -v claude'`（缺省 /bin/zsh）并取
/// 输出最后一个词作为可执行路径。
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
/// 中文：探测 Claude CLI 登录态：过滤掉 ANTHROPIC_API_KEY /
/// ANTHROPIC_AUTH_TOKEN / CLAUDE_CODE_OAUTH_TOKEN 后执行探测；
/// 输出为空且失败时走登录 shell 回退；stdout 非空则按 JSON 解析
/// `loggedIn`，解析失败把错误文案放入 `reason`。
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

/// 中文：Claude CLI 登录态结论：是否已连接 + 机器可读的原因标签
/// （logged-in / logged-out / empty-status / 解析错误文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeCliAuthStatus {
    /// 是否已登录（CLI 报告 `loggedIn: true`）。
    pub connected: bool,
    /// 结论原因：成功为 logged-in/logged-out，失败为 empty-status 或错误文案。
    pub reason: String,
}

/// Route-facing helper: run the probe off the async runtime (the production
/// spawn is blocking, bounded by the 6s spawn timeouts).
/// 中文：路由入口：把阻塞的探测丢到 `spawn_blocking` 线程执行
/// （生产 spawn 受 6 秒超时约束），避免卡住 async runtime；
/// 线程池失败时返回 disconnected + "spawn task failed"。
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
