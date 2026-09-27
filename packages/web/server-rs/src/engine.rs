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
//!
//! 中文说明：本模块管理 omp-host 引擎的受控生命周期，移植自 JS 版的
//! `omp-host-launch.js`（resolveOmpHostLaunchSpec）与 `lifecycle.js`
//! （createManagedOpenCodeServerProcess）：以 `serve --hostname --port`
//! 形式拉起 Bun omp-host 子进程，等待 stdout 上的就绪行
//! （`opencode server listening on <url>`），就绪失败时绝不泄漏子进程。
//! 鉴权头与 auth-state-runtime.js 一致（Basic `opencode:<password>`）。

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

/// 等待引擎就绪的默认超时（可被 OMPCHAMBER_OMP_HOST_READY_TIMEOUT_MS 覆盖）。
const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
/// 健康监控轮询间隔。
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// stderr 尾部捕获的容量上限（16 KiB，超出丢弃最旧字节）。
const STDERR_TAIL_MAX_BYTES: usize = 16 * 1024;

/// 引擎启动规格：可执行文件 + 参数 + 来源标签（env-host/bundled/bun 路径）。
#[derive(Debug)]
pub struct LaunchSpec {
    /// 实际运行的可执行文件（host 二进制或 Bun 运行时）。
    pub binary: PathBuf,
    /// 传给可执行文件的参数（含源码入口路径，若有）。
    pub args: Vec<String>,
    /// 启动来源：env-host / bundled / env / path。
    pub source: &'static str,
}

/// 本 crate 的清单目录（编译期常量，作为相对路径的锚点）。
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `<web package>/server/lib/omp-host/host.ts` — the TS entry stays in place;
/// only Bun may run it.
/// 中文说明：路径做词法清洗（消除 `server-rs/..` 段），使启动诊断与 JS 版
/// 报告的路径一致。
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

/// 平台相关的 host 二进制文件名（Windows 带 .exe 后缀）。
fn host_binary_name() -> &'static str {
    if cfg!(windows) {
        "omp-host.exe"
    } else {
        "omp-host"
    }
}

/// Mirror of `resolveOmpHostLaunchSpec`: explicit host binary env → bundled
/// binary dir → source entry under a Bun runtime.
/// 中文说明：优先级为 OMPCHAMBER_OMP_HOST_BINARY 显式路径 →
/// OMPCHAMBER_BUNDLED_OMP_HOST_DIR 内置二进制 → Bun 运行时跑源码入口。
pub fn resolve_launch_spec(hostname: &str, port: u16) -> anyhow::Result<LaunchSpec> {
    // 所有启动形态共用的 serve 子命令参数。
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

/// 解析运行时二进制：OMPCHAMBER_OMP_HOST_RUNTIME 显式指定优先，
/// 否则在 PATH 上找 bun；找不到即报错（从源码拉起必须要有 Bun）。
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

/// 在 PATH 环境变量中查找可执行文件（按平台分隔符切分），返回第一个命中。
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
/// 中文说明：在保留父进程 PATH 的基础上，追加启动运行时所在目录、
/// `~/.bun/bin`、`/usr/local/bin`（macOS 另加 `/opt/homebrew/bin`），
/// 全程去重。
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

/// 生成 32 字节随机密码并做 URL-safe base64 编码（无填充），可直接进入头/URL。
fn generate_password() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 生成 RFC3339 风格的 UTC 时间戳（无 chrono 依赖：epoch 秒 + 手写日期换算）。
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
/// 中文说明：以儒略日算法把 epoch 天数换算为 (年, 月, 日)。
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

/// 引擎运行模式：受管（本进程拉起子进程）或外部（连接既有服务器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineModeKind {
    /// 受管模式：由本进程 spawn 并守护 omp-host 子进程。
    Managed,
    /// 外部模式：连接一个已存在的 OpenCode 兼容服务器，不 spawn。
    External,
}

/// 引擎状态的单一事实来源：HTTP 客户端、就绪 watch channel、子进程句柄、
/// 密码与诊断信息。受管/外部两种模式共用此结构。
pub struct EngineState {
    /// 访问引擎（及全局）HTTP API 的客户端。
    http: reqwest::Client,
    /// 当前运行模式。
    mode: EngineModeKind,
    /// 引擎 base URL；就绪门控未开时也可能已填充（gated 状态）。
    base_url: RwLock<Option<String>>,
    /// 引擎鉴权密码（用户环境变量或自动生成）。
    password: RwLock<Option<String>>,
    /// 就绪状态广播（send_replace 无订阅者也会存储）。
    ready: watch::Sender<bool>,
    /// 受管子进程句柄；shutdown 独占持有其生命周期。
    child: tokio::sync::Mutex<Option<Child>>,
    /// 子进程 pid（exit watcher 探活用；停机后清空）。
    child_pid: RwLock<Option<u32>>,
    /// 密码是否来自用户环境变量（决定 auth_source 报 user-env 还是 generated）。
    user_provided_password: bool,
    /// 停机标志：置位后 exit watcher 退出，避免误报"意外退出"。
    shutting_down: std::sync::atomic::AtomicBool,
    /// 引擎后台任务句柄（stderr 捕获、watcher、健康监控），停机时统一 abort。
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// 最近一次错误（/health 与快照展示）。
    pub last_error: RwLock<Option<String>>,
    /// 引擎 stderr 的有界尾部（诊断用）。
    pub stderr_tail: RwLock<String>,
    /// 最近一次启动信息 JSON（诊断面板展示）。
    pub last_launch: RwLock<Option<serde_json::Value>>,
}

/// `EngineState` 的公共接口：状态查询、受管/外部实例化与停机。
impl EngineState {
    /// 共享的 HTTP 客户端引用。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Desktop-shell process info (control channel `engineInfo`): the
    /// managed child's pid and engine port when we own the engine.
    /// 中文说明：仅在受管模式返回 pid 与端口；外部模式为 (false, None, None)。
    pub fn managed_process_info(&self) -> (bool, Option<u32>, Option<u16>) {
        let pid = *self.child_pid.read().unwrap_or_else(|e| e.into_inner());
        let port = self
            .base_url()
            .as_deref()
            .and_then(|url| url.rsplit(':').next())
            .and_then(|tail| tail.trim_end_matches('/').parse::<u16>().ok());
        (pid.is_some(), pid, port)
    }

    /// 当前运行模式。
    pub fn mode(&self) -> EngineModeKind {
        self.mode
    }

    /// auth-state-runtime `openCodeAuthSource`: "user-env" when
    /// OPENCODE_SERVER_PASSWORD came from the environment, "generated" when
    /// we minted one for the managed engine.
    /// 中文说明：外部模式返回 None（无密码来源可言）。
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

    /// 引擎是否就绪（watch channel 当前值）。
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// 引擎 base URL（未就绪/gated 时也可能已知端口，返回 Some）。
    pub fn base_url(&self) -> Option<String> {
        self.base_url.read().ok().and_then(|g| g.clone())
    }

    /// `Authorization: Basic <base64(user:password)>` per auth-state-runtime.
    /// 中文说明：用户名取 OPENCODE_SERVER_USERNAME（默认 "opencode"），
    /// 无密码时返回 None（外部模式未提供密码即不加头）。
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

    /// 等待引擎就绪，最多阻塞 timeout；超时返回错误（含秒数）。
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

    /// 记录最近一次错误（覆写式，/health 与快照消费）。
    pub fn record_error(&self, message: impl Into<String>) {
        let mut guard = self.last_error.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(message.into());
    }

    /// 标记就绪并写入 base URL（send_replace 确保无订阅者时也生效）。
    fn set_ready(&self, base_url: Option<String>) {
        if let Ok(mut guard) = self.base_url.write() {
            *guard = base_url;
        }
        self.ready.send_replace(true);
    }

    /// Store the engine URL while keeping readiness CLOSED (JS cold-boot
    /// miss: `isOpenCodeReady` stays false, but the port/URL are known).
    /// 中文说明：与 JS 冷启动行为对齐——健康窗口未过时端口已知但 /api 门控仍关。
    fn set_gated(&self, base_url: String) {
        if let Ok(mut guard) = self.base_url.write() {
            *guard = Some(base_url);
        }
    }

    /// 标记未就绪并清空 base URL（子进程意外退出/停机时调用）。
    fn set_not_ready(&self) {
        self.ready.send_replace(false);
        if let Ok(mut guard) = self.base_url.write() {
            *guard = None;
        }
    }

    /// External mode: connect to an existing OpenCode-compatible server.
    /// 中文说明：立即可用（ready=true），无子进程、无密码来源、无监控任务。
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
    /// 中文说明：依次完成端口择取、启动规格解析、natives 装配（源码启动）、
    /// 密码准备、登录 shell 环境快照、PATH 增强、子进程 spawn、stdout 就绪行
    /// 等待与启动健康窗口探测；就绪失败路径都会先 shutdown 再返回错误。
    pub async fn start_managed(config: &crate::config::ServerConfig) -> anyhow::Result<Arc<Self>> {
        let EngineConfig::Managed { hostname } = &config.engine else {
            anyhow::bail!("start_managed called with a non-managed engine config");
        };

        // 先择一个空闲端口（bind(0) 后立即释放，交给引擎使用）。
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

        // 组装子进程命令：注入环境、工作目录、null stdin 与管道 stdout/stderr，
        // kill_on_drop 兜底防止句柄泄漏导致孤儿进程。
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
    /// 中文说明：每 2 秒以信号 0 探活一次；探失即记录错误并置为未就绪。
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

    /// 启动周期性健康监控：每 15 秒探测一次 `/global/health`，状态翻转时
    /// 记日志；恢复健康时清除 last_error，并为错过启动健康窗口的受管引擎
    /// 补开就绪门控（对齐 lifecycle.js 的恢复语义）。
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
    /// 中文说明：先置停机标志，再独占 child 锁：组 TERM → 失败则直接 kill →
    /// 5 秒限时等待 → 仍存活则组 KILL 兜底；随后清 pid、abort 全部后台任务、
    /// 置未就绪。
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

    /// 探测引擎 `/global/health`（带鉴权，5 秒超时）；无 base URL 或请求
    /// 失败均视为不健康。
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

    /// 引擎状态快照 JSON：模式/就绪/base URL/最近错误/最近启动信息，
    /// 供诊断路由与 /health 消费。
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
/// 中文说明：解析出 URL 后继续排空 stdout，防止管道写满阻塞子进程；
/// 后续行只记 debug 日志（就绪后日志端口属后续工作，丢弃安全）。
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
/// 中文说明：经 /bin/kill 发送信号；"0" 信号即 POSIX 探活。
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
/// 中文说明：信号 "0" 用 tasklist 判存活（输出含裸 pid 即存活），
/// 其余信号用 taskkill /T /F 终止整棵进程树。
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
/// 中文说明：以负 pid 对整组发信号；组信号失败（无权限/无组）时回退单 pid。
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

/// Windows 的组信号等价物：`taskkill /T` 本身终止整棵进程树，
/// 直接委托给单进程版本；"0" 探活在 Windows 无组语义。
#[cfg(windows)]
async fn signal_process_group(pid: u32, signal: &str) -> bool {
    // taskkill /T terminates the whole process tree — the detached-group
    // intent on Windows; the "0" probe has no group meaning.
    signal_process(pid, signal).await
}

/// Unix：执行 `/bin/kill <args>`，退出码 0 视为送达成功（内部工具函数）。
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
/// 中文说明：滚动保留最后 16 KiB（超出丢最旧字节），写入共享的
/// `stderr_tail`；管道关闭（子进程退出）即结束。
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

/// 从就绪行中提取 `http(s)://` 开头的目标 URL token；无匹配返回 None。
fn parse_listening_url(line: &str) -> Option<String> {
    let after = line.split("listening on").nth(1)?;
    after
        .split_whitespace()
        .find(|t| t.starts_with("http://") || t.starts_with("https://"))
        .map(String::from)
}

/// hmr-state-runtime `getInitialOpenCodeWorkingDirectory`:
/// `OMPCHAMBER_OPENCODE_CWD` (trimmed) or the user's home directory.
/// 中文说明：均不可用时兜底当前目录 "."。
fn engine_working_directory() -> PathBuf {
    std::env::var("OMPCHAMBER_OPENCODE_CWD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(crate::config::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 在指定 hostname 上 bind 端口 0 让内核分配空闲端口，随即释放监听并
/// 返回该端口（交给引擎子进程使用；存在极小的竞态窗口）。
fn pick_free_port(hostname: &str) -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind((hostname, 0))
        .map_err(|e| anyhow::anyhow!("cannot bind engine hostname {hostname}: {e}"))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// 构造默认 HTTP 客户端（10 秒连接超时）；构建失败直接 panic（不可恢复）。
fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

/// 引擎模块的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：就绪行中的监听 URL 能被正确提取，噪声/缺失 URL 返回 None。
    #[test]
    fn parses_listening_url() {
        assert_eq!(
            parse_listening_url("opencode server listening on http://127.0.0.1:3902"),
            Some("http://127.0.0.1:3902".to_string())
        );
        assert_eq!(parse_listening_url("opencode server listening"), None);
        assert_eq!(parse_listening_url("noise"), None);
    }

    /// 验证：PATH 增强保留原路径并追加运行时目录与常见工具目录，且不重复。
    #[test]
    fn augments_path_without_duplicates() {
        let out = augment_path("/usr/bin", Some(Path::new("/x/bin/bun")));
        assert!(out.starts_with("/usr/bin"));
        assert!(out.contains("/x/bin"));
        assert!(out.contains("/usr/local/bin"));
        let count = out.split(':').filter(|p| *p == "/usr/local/bin").count();
        assert_eq!(count, 1);
    }

    /// 验证：自动生成的密码是 URL-safe base64（无 + / = 且足够长）。
    #[test]
    fn generated_password_is_url_safe() {
        let pw = generate_password();
        assert!(!pw.contains('+') && !pw.contains('/') && !pw.contains('='));
        assert!(pw.len() >= 40);
    }

    /// 验证：epoch 天数到 (年,月,日) 的换算与已知日期一致。
    #[test]
    fn epoch_days_render_utc_date() {
        // Day 20722 since 1970-01-01 is 2026-09-26.
        let (y, m, d) = epoch_days_to_ymd(20_722);
        assert_eq!((y, m, d), (2026, 9, 26));
    }

    /// 验证：鉴权头是 Basic base64("opencode:<password>") 形状。
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

/// watch channel 的回归测试：就绪翻转必须不依赖订阅者存在。
#[cfg(test)]
mod watch_regression_tests {
    use super::*;

    /// 回归验证：watch::send 在零接收者时是空操作（曾致冷启动永不就绪），
    /// send_replace 必须无条件存储。
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
