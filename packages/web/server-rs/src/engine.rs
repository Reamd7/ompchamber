//! Managed omp-host engine lifecycle.
//!
//! Ports the core of `server/lib/opencode/omp-host-launch.js`
//! (`resolveOmpHostLaunchSpec`) and `server/lib/opencode/lifecycle.js`
//! (`createManagedOpenCodeServerProcess`): spawn the Bun omp-host child with
//! the `serve --hostname --port` shape, wait for the readiness stdout line
//! (`opencode server listening on <url>`), and never leak a child when
//! readiness fails. Auth headers mirror `auth-state-runtime.js`
//! (Basic `opencode:<password>`).
//!
//! Turn-1 known gaps (tracked in PORT-MANIFEST.md): pi-natives staging
//! (`omp-host-natives.js`), the managed-process orphan registry, login-shell
//! env snapshotting, and health-failure auto-restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

use crate::config::EngineConfig;

const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const STDERR_TAIL_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct LaunchSpec {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub source: &'static str,
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `<web package>/server/lib/omp-host/host.ts` — the TS entry stays in place;
/// only Bun may run it.
pub fn omp_host_entry() -> PathBuf {
    // Lexically cleaned so launch diagnostics carry the same path the JS
    // server reports (no `server-rs/..` segment).
    let raw = crate_dir()
        .join("..")
        .join("server")
        .join("lib")
        .join("omp-host")
        .join("host.ts");
    let mut clean = PathBuf::new();
    for component in raw.components() {
        match component {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    clean
}

fn host_binary_name() -> &'static str {
    if cfg!(windows) {
        "omp-host.exe"
    } else {
        "omp-host"
    }
}

/// Mirror of `resolveOmpHostLaunchSpec`: explicit host binary env → bundled
/// binary dir → source entry under a Bun runtime.
pub fn resolve_launch_spec(hostname: &str, port: u16) -> anyhow::Result<LaunchSpec> {
    let serve_args = vec![
        "serve".to_string(),
        "--hostname".to_string(),
        hostname.to_string(),
        "--port".to_string(),
        port.to_string(),
    ];

    if let Ok(explicit) = std::env::var("OMPCHAMBER_OMP_HOST_BINARY") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            let path = PathBuf::from(explicit);
            if !path.exists() {
                anyhow::bail!("OMPCHAMBER_OMP_HOST_BINARY does not exist: {explicit}");
            }
            return Ok(LaunchSpec {
                binary: path,
                args: serve_args,
                source: "env-host",
            });
        }
    }

    let bundled_dirs: Vec<PathBuf> = [std::env::var("OMPCHAMBER_BUNDLED_OMP_HOST_DIR").ok()]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect();
    for dir in bundled_dirs {
        let candidate = dir.join(host_binary_name());
        if candidate.exists() {
            return Ok(LaunchSpec {
                binary: candidate,
                args: serve_args,
                source: "bundled",
            });
        }
    }

    let entry = omp_host_entry();
    if !entry.exists() {
        anyhow::bail!("omp host entry missing: {}", entry.display());
    }
    let (runtime, source) = resolve_runtime_binary()?;
    Ok(LaunchSpec {
        binary: runtime,
        args: [entry.to_string_lossy().to_string()]
            .into_iter()
            .chain(serve_args)
            .collect(),
        source,
    })
}

fn resolve_runtime_binary() -> anyhow::Result<(PathBuf, &'static str)> {
    if let Ok(explicit) = std::env::var("OMPCHAMBER_OMP_HOST_RUNTIME") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            let path = PathBuf::from(explicit);
            if !path.exists() {
                anyhow::bail!("OMPCHAMBER_OMP_HOST_RUNTIME does not exist: {explicit}");
            }
            return Ok((path, "env"));
        }
    }
    if search_path_for("bun").is_some() {
        Ok((PathBuf::from("bun"), "path"))
    } else {
        Err(anyhow::anyhow!(
            "bun runtime not found on PATH (required to launch the omp host from source)"
        ))
    }
}

fn search_path_for(binary: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    path.split(if cfg!(windows) { ';' } else { ':' })
        .filter(|s| !s.is_empty())
        .map(|dir| Path::new(dir).join(binary))
        .find(|candidate| candidate.is_file())
}

/// Login-shell PATH augmentation is ported later (`env-runtime.js`); this is
/// the conservative core: keep the parent PATH and make sure the directory of
/// the launch runtime and common tool locations are reachable.
fn augment_path(existing: &str, extra: Option<&Path>) -> String {
    let sep: &str = if cfg!(windows) { ";" } else { ":" };
    let mut entries: Vec<String> = existing
        .split(sep)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let mut push_dir = |dir: String| {
        if !dir.is_empty() && !entries.iter().any(|e| e == &dir) {
            entries.push(dir);
        }
    };
    if let Some(dir) = extra
        .and_then(Path::parent)
        .map(|p| p.to_string_lossy().to_string())
    {
        push_dir(dir);
    }
    if let Some(home) = crate::config::home_dir() {
        push_dir(home.join(".bun").join("bin").to_string_lossy().to_string());
    }
    push_dir("/usr/local/bin".to_string());
    if cfg!(target_os = "macos") {
        push_dir("/opt/homebrew/bin".to_string());
    }
    entries.join(sep)
}

fn generate_password() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn utc_timestamp() -> String {
    // RFC3339-ish UTC stamp without a chrono dependency: fixed epoch date
    // plus seconds. Replaced by a proper clock port when scheduled tasks
    // (which need real dates) land.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let (year, month, day) = epoch_days_to_ymd(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Civil-from-days algorithm (Howard Hinnant) for UTC date rendering.
fn epoch_days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineModeKind {
    Managed,
    External,
}

pub struct EngineState {
    http: reqwest::Client,
    mode: EngineModeKind,
    base_url: RwLock<Option<String>>,
    password: RwLock<Option<String>>,
    ready: watch::Sender<bool>,
    child: tokio::sync::Mutex<Option<Child>>,
    child_pid: RwLock<Option<u32>>,
    user_provided_password: bool,
    shutting_down: std::sync::atomic::AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub last_error: RwLock<Option<String>>,
    pub stderr_tail: RwLock<String>,
    pub last_launch: RwLock<Option<serde_json::Value>>,
}

impl EngineState {
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Desktop-shell process info (control channel `engineInfo`): the
    /// managed child's pid and engine port when we own the engine.
    pub fn managed_process_info(&self) -> (bool, Option<u32>, Option<u16>) {
        let pid = *self.child_pid.read().unwrap_or_else(|e| e.into_inner());
        let port = self
            .base_url()
            .as_deref()
            .and_then(|url| url.rsplit(':').next())
            .and_then(|tail| tail.trim_end_matches('/').parse::<u16>().ok());
        (pid.is_some(), pid, port)
    }

    pub fn mode(&self) -> EngineModeKind {
        self.mode
    }

    /// auth-state-runtime `openCodeAuthSource`: "user-env" when
    /// OPENCODE_SERVER_PASSWORD came from the environment, "generated" when
    /// we minted one for the managed engine.
    pub fn auth_source(&self) -> Option<&'static str> {
        match self.mode {
            EngineModeKind::Managed => Some(if self.user_provided_password {
                "user-env"
            } else {
                "generated"
            }),
            EngineModeKind::External => None,
        }
    }

    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    pub fn base_url(&self) -> Option<String> {
        self.base_url.read().ok().and_then(|g| g.clone())
    }

    /// `Authorization: Basic <base64(user:password)>` per auth-state-runtime.
    pub fn auth_header(&self) -> Option<String> {
        let password = self.password.read().ok()?.clone()?;
        let username = std::env::var("OPENCODE_SERVER_USERNAME")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "opencode".to_string());
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Some(format!("Basic {credentials}"))
    }

    pub async fn wait_ready(&self, timeout: Duration) -> anyhow::Result<()> {
        let mut rx = self.ready.subscribe();
        if *rx.borrow() {
            return Ok(());
        }
        tokio::time::timeout(timeout, async {
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("engine not ready within {}s", timeout.as_secs()))
    }

    pub fn record_error(&self, message: impl Into<String>) {
        let mut guard = self.last_error.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(message.into());
    }

    fn set_ready(&self, base_url: Option<String>) {
        if let Ok(mut guard) = self.base_url.write() {
            *guard = base_url;
        }
        self.ready.send_replace(true);
    }

    /// Store the engine URL while keeping readiness CLOSED (JS cold-boot
    /// miss: `isOpenCodeReady` stays false, but the port/URL are known).
    fn set_gated(&self, base_url: String) {
        if let Ok(mut guard) = self.base_url.write() {
            *guard = Some(base_url);
        }
    }

    fn set_not_ready(&self) {
        self.ready.send_replace(false);
        if let Ok(mut guard) = self.base_url.write() {
            *guard = None;
        }
    }

    /// External mode: connect to an existing OpenCode-compatible server.
    pub fn external(base_url: String, password: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            http: default_http_client(),
            mode: EngineModeKind::External,
            base_url: RwLock::new(Some(base_url)),
            password: RwLock::new(password),
            ready: watch::Sender::new(true),
            child: tokio::sync::Mutex::new(None),
            child_pid: RwLock::new(None),
            user_provided_password: false,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            last_error: RwLock::new(None),
            stderr_tail: RwLock::new(String::new()),
            last_launch: RwLock::new(None),
        })
    }

    /// Spawn the managed omp-host child and wait for its readiness line.
    ///
    /// On readiness failure the child is terminated before the error surfaces
    /// (lifecycle.js: a spawn that never prints the listening line must not
    /// survive as an untracked engine process).
    pub async fn start_managed(config: &crate::config::ServerConfig) -> anyhow::Result<Arc<Self>> {
        let EngineConfig::Managed { hostname } = &config.engine else {
            anyhow::bail!("start_managed called with a non-managed engine config");
        };

        let port = pick_free_port(hostname)?;
        let spec = resolve_launch_spec(hostname, port)?;
        // lifecycle.js: source launches need the pi_natives addon in the
        // per-user cache; compiled host binaries ship it beside the exe.
        if spec.source != "env-host" && spec.source != "bundled" {
            crate::omp_host_natives::ensure_omp_host_natives().await;
        }
        // Auth password: user-provided or generated (auth-state-runtime
        // `ensureLocalOpenCodeServerPassword`).
        let user_provided_password = std::env::var("OPENCODE_SERVER_PASSWORD")
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let password = std::env::var("OPENCODE_SERVER_PASSWORD")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(generate_password);

        let env_runtime = crate::engine_env::EnvRuntime::shared();
        env_runtime.apply_login_shell_env_snapshot().await;
        // lifecycle.js: `shellEnvKeysCount: Object.keys(shellEnv).length` —
        // the snapshot's total size, not the merged-override count.
        let shell_env_keys_count = env_runtime
            .get_login_shell_env_snapshot()
            .await
            .map(|snapshot| snapshot.len())
            .unwrap_or(0);
        let mut envs: HashMap<String, String> = env_runtime.effective_env();
        let existing_path = envs.get("PATH").cloned().unwrap_or_default();
        envs.insert("OPENCODE_SERVER_PASSWORD".to_string(), password.clone());
        let mut envs = crate::provider_env_aliases::apply_provider_env_aliases(&envs);
        envs.insert(
            "PATH".to_string(),
            augment_path(&existing_path, Some(&spec.binary)),
        );

        let launch_info = serde_json::json!({
            "launchedAt": utc_timestamp(),
            "binary": spec.binary.to_string_lossy(),
            "args": spec.args,
            "cwd": engine_working_directory(),
            "hostname": hostname,
            "port": port,
            "runtimeSource": spec.source,
            "sourceBinary": spec.binary.to_string_lossy(),
            "wrapperType": serde_json::Value::Null,
            "hasShellEnv": shell_env_keys_count > 0,
            "shellEnvKeysCount": shell_env_keys_count,
            "pathEntryCount": envs
                .get("PATH")
                .map(|path| {
                    path.split(if cfg!(windows) { ';' } else { ':' })
                        .filter(|entry| !entry.is_empty())
                        .count()
                })
                .unwrap_or(0),
        });
        tracing::info!("[omp-host] Launching managed engine {}", launch_info);

        let mut command = Command::new(&spec.binary);
        command
            .args(&spec.args)
            .envs(&envs)
            .current_dir(engine_working_directory())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // lifecycle.js spawns detached on non-Windows so the engine
            // survives independently of the web server's process group.
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to spawn omp host ({}): {error}",
                spec.binary.display()
            )
        })?;

        let pid_for_state = child.id();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let ready_timeout = Duration::from_millis(
            std::env::var("OMPCHAMBER_OMP_HOST_READY_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_READY_TIMEOUT_MS),
        );

        let state = Arc::new(Self {
            http: default_http_client(),
            mode: EngineModeKind::Managed,
            base_url: RwLock::new(None),
            password: RwLock::new(Some(password)),
            ready: watch::Sender::new(false),
            child: tokio::sync::Mutex::new(Some(child)),
            child_pid: RwLock::new(pid_for_state),
            user_provided_password: user_provided_password,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            last_error: RwLock::new(None),
            stderr_tail: RwLock::new(String::new()),
            last_launch: RwLock::new(Some(launch_info)),
        });

        // Readiness: parse `opencode server listening on <url>` from stdout.
        let (url_tx, url_rx) = oneshot::channel::<anyhow::Result<String>>();
        let stdout_task = tokio::spawn(readiness_from_stdout(stdout, url_tx));
        let stderr_task = tokio::spawn(capture_stderr_tail(stderr, Arc::clone(&state)));

        let readiness = tokio::time::timeout(ready_timeout, url_rx).await;
        let url = match readiness {
            Ok(Ok(Ok(url))) => url,
            Ok(Ok(Err(error))) => {
                state.shutdown().await;
                anyhow::bail!("omp host failed to become ready: {error}");
            }
            Ok(Err(_)) => {
                state.shutdown().await;
                anyhow::bail!("omp host readiness watcher dropped unexpectedly");
            }
            Err(_) => {
                state.shutdown().await;
                anyhow::bail!(
                    "timeout waiting for omp host to start after {}ms",
                    ready_timeout.as_millis()
                );
            }
        };
        stdout_task.abort();
        // stderr capture runs until the child exits (its pipe closes); it is
        // reaped with the other engine tasks at shutdown — awaiting it here
        // would block forever while the engine lives.
        state
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(stderr_task);

        // lifecycle.js flips `isOpenCodeReady` only when `/global/health`
        // answers healthy within the startup window (waitForReady, 10s) —
        // NOT at the listening line. The /api/* proxy gate keys on that
        // flag, so a slow cold engine stays gated on both servers.
        let mut request = state.http.get(format!("{url}/global/health"));
        if let Some(auth) = state.auth_header() {
            request = request.header("authorization", auth);
        }
        let health_ok = 'health: {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                match request
                    .try_clone()
                    .unwrap_or_else(|| request.try_clone().expect("cloneable request"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        let body = response.text().await.unwrap_or_default();
                        if serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|body| body.get("healthy").and_then(|v| v.as_bool()))
                            == Some(true)
                        {
                            break 'health true;
                        }
                    }
                    _ => {}
                }
                if tokio::time::Instant::now() >= deadline {
                    break 'health false;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        };
        if health_ok {
            state.set_ready(Some(url.clone()));
            tracing::info!("[omp-host] Managed engine ready at {}", url);
        } else {
            // Mirror the JS: the child stays up and serves, readiness stays
            // closed, and the /api gate 503s with the restarting body.
            state.set_gated(url.clone());
            tracing::warn!("[omp-host] engine missed the startup health window; /api gated");
        }

        state.spawn_exit_watcher();
        Ok(state)
    }
    /// Crash watcher: polls the child's liveness (`kill -0`) WITHOUT holding
    /// the child lock — `shutdown()` exclusively owns the child lifecycle.
    /// An unexpected exit is recorded honestly for /health; auto-restart
    /// remains a documented gap.
    fn spawn_exit_watcher(self: &Arc<Self>) {
        let state = Arc::clone(self);
        let pid = state.child_pid.read().ok().and_then(|g| *g);
        let Some(pid) = pid else { return };
        let task = tokio::spawn(async move {
            loop {
                if state
                    .shutting_down
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return;
                }
                if !signal_process(pid, "0").await {
                    let detail = "engine process exited unexpectedly".to_string();
                    tracing::error!("[omp-host] {detail}");
                    state.record_error(detail);
                    state.set_not_ready();
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
    }

    pub fn spawn_health_monitor(self: &Arc<Self>) {
        let state = Arc::clone(self);
        let task = tokio::spawn(async move {
            let mut was_healthy: Option<bool> = None;
            loop {
                tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
                let healthy = state.probe_health().await;
                if was_healthy != Some(healthy) {
                    if healthy {
                        tracing::info!("[omp-host] engine health recovered");
                        if let Ok(mut guard) = state.last_error.write() {
                            *guard = None;
                        }
                    } else {
                        tracing::warn!("[omp-host] engine health check failed");
                    }
                    was_healthy = Some(healthy);
                }
                // Recovery parity with lifecycle.js: a healthy managed
                // engine clears a missed startup health window — otherwise
                // readiness stays false forever and the /api gate 503s a
                // serving engine.
                if healthy
                    && state.mode() == EngineModeKind::Managed
                    && !state.is_ready()
                    && state.base_url().is_some()
                {
                    let base_url = state.base_url();
                    state.set_ready(base_url);
                    tracing::info!(
                        "[omp-host] engine healthy, marking ready (startup window recovery)"
                    );
                }
            }
        });
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
    }

    /// Terminate the managed engine: SIGTERM to the whole process group (the
    /// host spawns worker children — a lone-pid TERM leaves them holding the
    /// pipes open), bounded wait, then SIGKILL — the intent of
    /// `terminateChildProcess`, adapted to the detached group spawn.
    pub async fn shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut guard = self.child.lock().await;
            if let Some(mut child) = guard.take() {
                if let Some(pid) = child.id() {
                    let terminated = signal_process_group(pid, "TERM").await;
                    if !terminated {
                        let _ = child.start_kill();
                    }
                    if tokio::time::timeout(Duration::from_secs(5), child.wait())
                        .await
                        .is_err()
                    {
                        let _ = signal_process_group(pid, "KILL").await;
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                } else {
                    // Already gone.
                    let _ = child.wait().await;
                }
            }
        }
        if let Ok(mut guard) = self.child_pid.write() {
            *guard = None;
        }
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for task in tasks.drain(..) {
            task.abort();
        }
        self.set_not_ready();
        tracing::info!("[omp-host] managed engine stopped");
    }

    pub async fn probe_health(&self) -> bool {
        let Some(base) = self.base_url() else {
            return false;
        };
        let mut request = self.http.get(format!("{base}/global/health"));
        if let Some(auth) = self.auth_header() {
            request = request.header("authorization", auth);
        }
        matches!(
            request.timeout(Duration::from_secs(5)).send().await,
            Ok(resp) if resp.status().is_success()
        )
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": match self.mode { EngineModeKind::Managed => "managed", EngineModeKind::External => "external" },
            "ready": self.is_ready(),
            "baseUrl": self.base_url(),
            "lastError": self.last_error.read().ok().and_then(|g| g.clone()),
            "lastLaunch": self.last_launch.read().ok().and_then(|g| g.clone()),
        })
    }
}

/// Read stdout until the readiness line, then keep draining so the pipe never
/// fills (post-readiness logging is a later port; dropping lines is safe).
async fn readiness_from_stdout(
    stdout: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    url_tx: oneshot::Sender<anyhow::Result<String>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut result: anyhow::Result<String> =
        Err(anyhow::anyhow!("omp host stdout closed before readiness"));
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("[omp-host:stdout] {}", line);
        if line.starts_with("opencode server listening") {
            result = parse_listening_url(&line)
                .ok_or_else(|| anyhow::anyhow!("failed to parse server url from output: {line}"));
            break;
        }
    }
    let _ = url_tx.send(result);
    // Drain the remainder so the child never blocks on a full pipe.
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!("[omp-host:stdout] {}", line);
    }
}

/// Signal a single pid; `signal` "0" is the liveness probe.
/// Returns false when the signal could not be delivered (no such process).
#[cfg(unix)]
async fn signal_process(pid: u32, signal: &str) -> bool {
    run_kill(&["-".to_string() + signal, pid.to_string()]).await
}

/// Windows has no /bin/kill — the exit watcher's `kill -0` probe always
/// failed, so a perfectly healthy engine was recorded as "exited
/// unexpectedly" right after startup and the readiness gate answered 503
/// forever (alpha.7 desktop build). Probe liveness with
/// `tasklist /FI "PID eq n"`; other signals terminate the process tree via
/// `taskkill /T /F`.
#[cfg(windows)]
async fn signal_process(pid: u32, signal: &str) -> bool {
    if signal == "0" {
        let output = tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await;
        return match output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                // A match lists one process row containing the bare pid;
                // the no-match case prints an "INFO: No tasks" header.
                text.split_whitespace().any(|token| token == pid.to_string())
            }
            _ => false,
        };
    }
    tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Signal the child's whole process group (negative pid). The host is spawned
/// with `process_group(0)`, so the group id equals the child pid; its worker
/// children die with it instead of surviving with open pipes.
#[cfg(unix)]
async fn signal_process_group(pid: u32, signal: &str) -> bool {
    let group_arg = format!("-{pid}");
    let delivered = run_kill(&["-".to_string() + signal, group_arg]).await;
    if delivered {
        true
    } else {
        // No permission or no group: fall back to the single pid.
        signal_process(pid, signal).await
    }
}

#[cfg(windows)]
async fn signal_process_group(pid: u32, signal: &str) -> bool {
    // taskkill /T terminates the whole process tree — the detached-group
    // intent on Windows; the "0" probe has no group meaning.
    signal_process(pid, signal).await
}

#[cfg(unix)]
async fn run_kill(args: &[String]) -> bool {
    match tokio::process::Command::new("/bin/kill")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Bounded stderr tail capture (lifecycle.js `runtimeStderrTail`).
async fn capture_stderr_tail(
    stderr: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    state: Arc<EngineState>,
) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::with_capacity(STDERR_TAIL_MAX_BYTES);
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > STDERR_TAIL_MAX_BYTES {
                    let excess = buffer.len() - STDERR_TAIL_MAX_BYTES;
                    buffer.drain(..excess);
                }
                if let Ok(mut guard) = state.stderr_tail.write() {
                    *guard = String::from_utf8_lossy(&buffer).to_string();
                }
            }
        }
    }
}

fn parse_listening_url(line: &str) -> Option<String> {
    let after = line.split("listening on").nth(1)?;
    after
        .split_whitespace()
        .find(|t| t.starts_with("http://") || t.starts_with("https://"))
        .map(String::from)
}

/// hmr-state-runtime `getInitialOpenCodeWorkingDirectory`:
/// `OMPCHAMBER_OPENCODE_CWD` (trimmed) or the user's home directory.
fn engine_working_directory() -> PathBuf {
    std::env::var("OMPCHAMBER_OPENCODE_CWD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(crate::config::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn pick_free_port(hostname: &str) -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind((hostname, 0))
        .map_err(|e| anyhow::anyhow!("cannot bind engine hostname {hostname}: {e}"))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listening_url() {
        assert_eq!(
            parse_listening_url("opencode server listening on http://127.0.0.1:3902"),
            Some("http://127.0.0.1:3902".to_string())
        );
        assert_eq!(parse_listening_url("opencode server listening"), None);
        assert_eq!(parse_listening_url("noise"), None);
    }

    #[test]
    fn augments_path_without_duplicates() {
        let out = augment_path("/usr/bin", Some(Path::new("/x/bin/bun")));
        assert!(out.starts_with("/usr/bin"));
        assert!(out.contains("/x/bin"));
        assert!(out.contains("/usr/local/bin"));
        let count = out.split(':').filter(|p| *p == "/usr/local/bin").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn generated_password_is_url_safe() {
        let pw = generate_password();
        assert!(!pw.contains('+') && !pw.contains('/') && !pw.contains('='));
        assert!(pw.len() >= 40);
    }

    #[test]
    fn epoch_days_render_utc_date() {
        // Day 20722 since 1970-01-01 is 2026-09-26.
        let (y, m, d) = epoch_days_to_ymd(20_722);
        assert_eq!((y, m, d), (2026, 9, 26));
    }

    #[test]
    fn auth_header_is_basic_opencode() {
        let state =
            EngineState::external("http://127.0.0.1:1".to_string(), Some("secret".to_string()));
        let header = state.auth_header().expect("auth header");
        let encoded = header.strip_prefix("Basic ").expect("basic prefix");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        assert_eq!(decoded, b"opencode:secret".to_vec());
    }
}

#[cfg(test)]
mod watch_regression_tests {
    use super::*;

    #[test]
    fn readiness_flips_without_any_subscriber() {
        // Regression: watch::send is a no-op with zero receivers, so a cold
        // start never became ready. send_replace must store unconditionally.
        let state = EngineState::external("http://127.0.0.1:9".into(), None);
        assert!(state.is_ready());
        state.set_not_ready();
        assert!(!state.is_ready());
        assert_eq!(state.base_url(), None);
        state.set_ready(Some("http://127.0.0.1:10".into()));
        assert!(state.is_ready());
        assert_eq!(state.base_url().as_deref(), Some("http://127.0.0.1:10"));
    }
}
