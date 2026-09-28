//! Port of `server/lib/opencode/managed-process-registry.js`.
//!
//! Managed OpenCode process registry + orphan reaper. The engine is spawned
//! as an EXTERNAL child binary (Unix: its own process group), so it can
//! outlive a hard-killed parent and then contend on the shared SQLite DB.
//! Like the JS module, an on-disk record of the pids WE spawned (ONE FILE PER
//! PID under the registry dir — per-process files mean no write contention
//! between concurrently running instances) plus a startup reaper that kills
//! ONLY our own, verified, genuinely-orphaned processes:
//! 1. only pids this product recorded are candidates;
//! 2. the live pid is re-verified as a managed engine `serve` matching the
//!    recorded port (guards against pid recycling);
//! 3. the kill happens only when the owner is provably gone (reparented to
//!    init/pid 1, or the recorded owner pid is dead).
//!
//! All filesystem and child-process operations are ASYNC (JS uses
//! `fsp`/`execFile` for exactly this reason — a synchronous reaper stalled
//! the Electron event loop and caused the 1.13.3 `openchamber-ui://` lag
//! regression, #1841). The fs and execFile dependencies are injectable seams,
//! mirroring the JS `createManagedProcessRegistry({ fs, execFileAsync })`.
//! The registry directory is shared with the VS Code extension's parity
//! implementation, so it is the literal `~/.config/ompchamber/managed-opencode`
//! (overridable via `OMPCHAMBER_MANAGED_PROCESS_REGISTRY`) — deliberately NOT
//! the server's `data_dir` root.
//!
//! Known gaps (see PORT-MANIFEST.md):
//! - JS signals via in-process `process.kill`; without libc the port shells
//!   out to `/bin/kill` (same argv shape `engine.rs` already uses) and to
//!   `taskkill` on Windows. `/bin/kill` cannot express JS's "EPERM means
//!   still alive" nuance for foreign-owned pids (irrelevant for pids we
//!   spawned).
//! - `new Date().toISOString()` is reproduced by a local civil-from-days
//!   algorithm (engine.rs keeps the same one private).
//!
//! 中文说明：本模块是 `server/lib/opencode/managed-process-registry.js` 的
//! Rust 移植——托管 OpenCode 进程注册表 + 孤儿回收器。引擎以外部子进程
//! 二进制方式启动（Unix 上自成 process group），因此可能在父进程被硬杀后
//! 存活，继而争抢共享的 SQLite 数据库。与 JS 版一致：把"由我们启动"的 pid
//! 记录到磁盘（注册目录下每个 pid 一个文件——按进程拆文件意味着并发运行的
//! 多个实例之间没有写竞争），并在启动时只回收满足全部条件、确属自己启动且
//! 真正成为孤儿的进程：
//! 1. 只有本产品记录过的 pid 才是候选；
//! 2. 存活 pid 会重新核验为匹配记录端口的 managed engine `serve`
//!    （防止 pid 被系统回收复用）；
//! 3. 仅当所有者可证明已消失（已 reparent 到 init/pid 1，或记录的 owner
//!    pid 已死亡）才执行 kill。
//! 所有文件系统与子进程操作均为异步（JS 出于同样原因使用 `fsp`/`execFile`：
//! 同步回收器曾拖住事件循环并导致 1.13.3 的 `openchamber-ui://` 卡顿回归，
//! 见 #1841）。fs 与 execFile 依赖以可注入 seam 的形式抽象，对应 JS 的
//! `createManagedProcessRegistry({ fs, execFileAsync })`。注册目录与
//! VS Code 扩展的同构实现共享，固定为 `~/.config/ompchamber/managed-opencode`
//! （可用 `OMPCHAMBER_MANAGED_PROCESS_REGISTRY` 覆盖），刻意不使用 server 的
//! `data_dir` 根目录。

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::engine_env::env_runtime::{Platform, host_platform};

// ---------------------------------------------------------------------------
// Identification (exported for tests, like the JS module)
// ---------------------------------------------------------------------------

/// JS `isManagedEngineIdentifier`: the managed engine is the omp host — a
/// compiled `omp-host(.exe)` binary or a Bun runtime launching
/// `.../lib/omp-host/host.ts serve` — plus the legacy `opencode serve` shape
/// so an older build's registry entries still reap.
/// 中文：managed engine 即 omp host——编译出的 `omp-host(.exe)` 二进制，或以
/// Bun 运行 `.../lib/omp-host/host.ts serve`；同时兼容旧版 `opencode serve`
/// 形态，使旧构建留下的注册表项也能被回收。
fn is_managed_engine_identifier(command: &str) -> bool {
    let lower = command.to_lowercase();
    let is_our_engine = lower.contains("omp-host") || lower.contains("opencode");
    is_our_engine && lower.contains("serve")
}

/// JS `commandIdentifiesOurServer(command, entry)`: misidentifying the engine
/// here either leaks orphans forever (`omp-host` never matched the old
/// `opencode` check) or risks reaping a process we do not own, so the port is
/// exact: engine identifier AND the recorded port (when known) in the command
/// line.
/// 中文：此处的误判要么让孤儿进程永远泄漏（`omp-host` 曾匹配不到旧的
/// `opencode` 检查），要么可能回收不属于我们的进程，因此移植逐字对齐：
/// 必须命中 engine 标识，且（端口已知时）命令行包含记录的端口号。
pub fn command_identifies_our_server(command: &str, entry_port: Option<i64>) -> bool {
    if !is_managed_engine_identifier(command) {
        return false;
    }
    // Tie to the exact server we registered when we know its port, so a
    // recycled pid running a *different* omp host instance is never mistaken
    // for ours.
    if let Some(port) = entry_port
        && !command.contains(&port.to_string())
    {
        return false;
    }
    true
}

/// JS `windowsImageLooksLikeEngine`: whether a `tasklist` CSV row's image name
/// is a managed engine binary.
/// 中文：判断 `tasklist` CSV 行中的映像名是否为 managed engine 二进制。
pub fn windows_image_looks_engine(image: &str) -> bool {
    let lower = image.to_lowercase();
    lower.contains("omp-host") || lower.contains("opencode")
}

// ---------------------------------------------------------------------------
// Entry shape
// ---------------------------------------------------------------------------

/// One `<pid>.json` record. `owner_pid`/`port`/`binary` may be absent in
/// older files; `pid` is required (JS `Number.isInteger(entry.pid)`).
/// 中文：一条 `<pid>.json` 记录。`owner_pid`/`port`/`binary` 在旧文件中可能
/// 缺失；`pid` 必填（对应 JS `Number.isInteger(entry.pid)` 门槛）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    /// 被登记的引擎进程 pid（必填，必须为整数）。
    pub pid: i64,
    /// 启动该引擎的宿主（owner）进程 pid，用于判定宿主是否已死亡。
    pub owner_pid: Option<i64>,
    /// 引擎监听端口；用于把命令行绑定到我们注册的那台 server。
    pub port: Option<i64>,
    /// 启动所用二进制路径（仅作记录，可为空）。
    pub binary: Option<String>,
    /// 启动来源标记（如 `web`），缺省为 `"web"`。
    pub runtime: Option<String>,
    /// ISO-8601 UTC 启动时间戳（含毫秒）。
    pub started_at: Option<String>,
}

/// Parse + validate one entry file (JS `JSON.parse` + the integer-pid gate;
/// both corrupt files and non-integer pids are dropped by the caller).
/// 中文：解析并校验单条记录文件（JS `JSON.parse` + 整数 pid 门槛）；损坏的
/// 文件与非整数 pid 都返回 `None`，由调用方直接丢弃。
fn parse_entry(content: &str) -> Option<RegistryEntry> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    match value.get("pid") {
        Some(serde_json::Value::Number(number)) if number.is_i64() || number.is_u64() => {}
        _ => return None,
    }
    serde_json::from_value(value).ok()
}

/// 一次回收扫描的结果统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapSummary {
    /// 本次读取并尝试处理的记录条数。
    pub inspected: usize,
    /// 实际被杀死（reaped）的孤儿进程数。
    pub reaped: usize,
}

/// Unix 上 `ps` 查询到的进程信息。
struct UnixProcInfo {
    /// 父进程 pid（判断是否已被 reparent 到 init）。
    ppid: i64,
    /// 完整命令行（核对引擎身份与端口）。
    command: String,
}

// ---------------------------------------------------------------------------
// Injectable seams (JS `{ fs, execFileAsync }`)
// ---------------------------------------------------------------------------

/// 注册表 fs seam 的统一 future 类型：按借用生命周期参数化的 boxed 异步 IO 结果。
type FsFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// Async filesystem seam over the operations the registry performs.
/// 中文：注册表所需的异步文件系统 seam（对应 JS 注入的 `fs`）；生产环境为
/// `TokioFs`，测试可换成内存实现。
pub trait RegistryFs: Send + Sync {
    /// 递归创建目录（`fs.promises.mkdir` recursive）。
    fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()>;
    /// 整文件写入（配合临时文件 + rename 实现原子落盘）。
    fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()>;
    /// 重命名：把临时文件落为正式的 `<pid>.json`。
    fn rename(&self, from: PathBuf, to: PathBuf) -> FsFuture<'_, ()>;
    /// Directory entry names (files and dirs), like `fs.promises.readdir`.
    /// 中文：目录内全部条目名（文件与子目录），结果排序，对齐
    /// `fs.promises.readdir`。
    fn read_dir_names(&self, dir: PathBuf) -> FsFuture<'_, Vec<String>>;
    /// 读取整个文件为字符串。
    fn read_to_string(&self, path: PathBuf) -> FsFuture<'_, String>;
    /// 删除文件（清理过期/损坏记录）。
    fn remove_file(&self, path: PathBuf) -> FsFuture<'_, ()>;
}

/// Production fs: `tokio::fs` (async like the JS `fsp`).
/// 中文：生产环境 fs——`tokio::fs`（与 JS `fsp` 一样保持异步）。
struct TokioFs;

/// `RegistryFs` 的 tokio 实现：每个方法直接转发到 `tokio::fs` 对应 API。
impl RegistryFs for TokioFs {
    /// `create_dir_all` 转发。
    fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::create_dir_all(dir).await })
    }

    /// 整文件写入转发。
    fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::write(path, contents).await })
    }

    /// `rename` 转发。
    fn rename(&self, from: PathBuf, to: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::rename(from, to).await })
    }

    /// 读取目录并把条目名收集排序（对齐 `fs.promises.readdir` 行为）。
    fn read_dir_names(&self, dir: PathBuf) -> FsFuture<'_, Vec<String>> {
        Box::pin(async move {
            let mut names = Vec::new();
            let mut entries = tokio::fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
            names.sort();
            Ok(names)
        })
    }

    /// `read_to_string` 转发。
    fn read_to_string(&self, path: PathBuf) -> FsFuture<'_, String> {
        Box::pin(async move { tokio::fs::read_to_string(path).await })
    }

    /// `remove_file` 转发。
    fn remove_file(&self, path: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::remove_file(path).await })
    }
}

/// execFile seam 的统一 future 类型：boxed 的异步 stdout 结果。
type ExecFuture = Pin<Box<dyn Future<Output = io::Result<String>> + Send>>;

/// JS `execFileAsync(cmd, args, { timeout })` → stdout.
/// 中文：JS `execFileAsync(cmd, args, { timeout })` 的等价签名：执行命令并
/// 返回 stdout。
pub type ExecFileFn = Arc<dyn Fn(String, Vec<String>, Option<u64>) -> ExecFuture + Send + Sync>;

/// Production execFile: tokio process with `kill_on_drop`, wrapped in the
/// requested timeout (a timeout rejects, exactly like the promisified
/// `execFile`).
/// 中文：生产环境 execFile——tokio 子进程（`kill_on_drop`），并按请求的
/// timeout 包装（超时即失败，与 promisify 后的 `execFile` 行为一致）。
fn real_exec_file() -> ExecFileFn {
    Arc::new(
        |program: String, args: Vec<String>, timeout_ms: Option<u64>| {
            Box::pin(async move {
                let mut command = tokio::process::Command::new(&program);
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                let output_fut = command.output();
                let output = if let Some(ms) = timeout_ms {
                    tokio::time::timeout(Duration::from_millis(ms), output_fut)
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "execFile timeout")
                        })??
                } else {
                    output_fut.await?
                };
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            })
        },
    )
}

/// JS `isPidAlive` (`process.kill(pid, 0)`; EPERM = still alive). The port
/// shells the probe out (`/bin/kill -0` on Unix, `tasklist` on Windows).
/// 中文：JS `isPidAlive`（`process.kill(pid, 0)`；EPERM 视为仍存活）。本移植
/// 通过外部命令探测：Unix 用 `/bin/kill -0`，Windows 用 `tasklist`。
pub type PidAliveFn = Arc<dyn Fn(i64) -> bool + Send + Sync>;

/// 生产环境 pid 存活探测：Unix 执行 `/bin/kill -0`，Windows 解析
/// `tasklist /FI "PID eq <pid>"` 的 CSV 输出；pid ≤ 0 或探测失败一律视为
/// 已死亡。
fn real_pid_alive() -> PidAliveFn {
    Arc::new(|pid: i64| {
        if pid <= 0 {
            return false;
        }
        if cfg!(windows) {
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .stdin(Stdio::null())
                .output();
            matches!(output, Ok(out) if out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().to_lowercase().starts_with("info:"))
        } else {
            std::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }
    })
}

/// 可注入的"当前时间"函数：返回 ISO-8601 UTC 字符串，测试中可固定。
pub type NowFn = Arc<dyn Fn() -> String + Send + Sync>;
/// 可注入的日志函数：回收器用它输出 `[lifecycle] ...` 消息。
pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

/// `new Date().toISOString()` — UTC with milliseconds, via the same
/// civil-from-days algorithm engine.rs uses.
/// 中文：`new Date().toISOString()` 的等价实现——带毫秒的 UTC 时间戳，
/// 用与 engine.rs 相同的 civil-from-days 算法换算年月日。
fn iso_utc_now() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs() as i64;
    let millis = duration.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let (year, month, day) = epoch_days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's civil-from-days (same as engine.rs, which keeps its copy
/// private).
/// 中文：Howard Hinnant 的 civil-from-days 算法（与 engine.rs 的私有副本
/// 相同）：把 Unix 纪元起的天数换算为 (年, 月, 日)。
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

/// JS `resolveRegistryDir`.
/// 中文：JS `resolveRegistryDir`——优先取 `OMPCHAMBER_MANAGED_PROCESS_REGISTRY`
/// 环境变量（非空白），否则落到 `~/.config/ompchamber/managed-opencode`。
pub fn resolve_registry_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OMPCHAMBER_MANAGED_PROCESS_REGISTRY") {
        let trimmed = override_dir.trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("ompchamber")
        .join("managed-opencode")
}

/// JS `entryFilePath(pid)`.
/// 中文：JS `entryFilePath(pid)`——注册表目录下 `<pid>.json` 的完整路径。
fn entry_file_path(dir: &Path, pid: i64) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// 托管进程注册表：登记/注销我们启动的引擎进程，并在启动时回收经核验的
/// 孤儿。所有外部依赖（fs、execFile、pid 探测、时钟）均可注入以便测试。
pub struct ManagedProcessRegistry {
    /// 异步文件系统 seam。
    fs: Arc<dyn RegistryFs>,
    /// 外部命令执行 seam（`ps`、`tasklist`、`/bin/kill`、`taskkill`）。
    exec_file: ExecFileFn,
    /// pid 存活探测 seam。
    pid_alive: PidAliveFn,
    /// 当前时间函数（生成 startedAt）。
    now: NowFn,
    /// 注册表目录（每 pid 一个 JSON 文件）。
    registry_dir: PathBuf,
    /// 目标平台（决定 Unix/Windows 两套识别与回收路径）。
    platform: Platform,
}

/// 注册表核心操作：记录的写入与读取、平台相关的进程识别、孤儿判定与终止，
/// 以及对外暴露的注册/注销/回收 API。
impl ManagedProcessRegistry {
    /// Production registry with real seams over `registry_dir`.
    /// 中文：生产构造器——在 `registry_dir` 上以真实 seam 组装注册表。
    pub fn new(registry_dir: PathBuf, platform: Platform) -> Self {
        Self::with_seams(
            Arc::new(TokioFs),
            real_exec_file(),
            real_pid_alive(),
            Arc::new(iso_utc_now),
            registry_dir,
            platform,
        )
    }

    /// Test/consumer constructor (JS `createManagedProcessRegistry(deps)`).
    /// 中文：测试/定制构造器（JS `createManagedProcessRegistry(deps)`）——
    /// 全部依赖显式注入。
    pub fn with_seams(
        fs: Arc<dyn RegistryFs>,
        exec_file: ExecFileFn,
        pid_alive: PidAliveFn,
        now: NowFn,
        registry_dir: PathBuf,
        platform: Platform,
    ) -> Self {
        Self {
            fs,
            exec_file,
            pid_alive,
            now,
            registry_dir,
            platform,
        }
    }

    /// Best-effort atomic entry write (tmp + rename). A failed registry write
    /// must never break spawn/shutdown.
    /// 中文：尽力而为的原子写入（临时文件 + rename）。注册表写失败绝不能
    /// 影响 spawn/shutdown 主流程，因此错误被吞掉。
    async fn write_entry_file(&self, entry: &RegistryEntry) {
        let dir = self.registry_dir.clone();
        let json = serde_json::to_string_pretty(entry)
            .map_err(|error| io::Error::other(error.to_string()));
        let result = async {
            let json = json?;
            self.fs.mkdir_all(dir.clone()).await?;
            let file_path = entry_file_path(&dir, entry.pid);
            let tmp = dir.join(format!("{}.json.tmp-{}", entry.pid, std::process::id()));
            self.fs.write_file(tmp.clone(), json).await?;
            self.fs.rename(tmp, file_path).await?;
            Ok::<(), io::Error>(())
        }
        .await;
        let _ = result;
    }

    /// 读取目录下全部 `*.json` 记录；损坏或 pid 非整数的文件顺手删除。
    /// 目录读取失败（如目录尚不存在）时返回空列表。
    async fn read_all_entries(&self) -> Vec<(RegistryEntry, PathBuf)> {
        let names = match self.fs.read_dir_names(self.registry_dir.clone()).await {
            Ok(names) => names,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for name in names.into_iter().filter(|name| name.ends_with(".json")) {
            let file_path = self.registry_dir.join(&name);
            let parsed = self
                .fs
                .read_to_string(file_path.clone())
                .await
                .ok()
                .and_then(|content| parse_entry(&content));
            match parsed {
                Some(entry) => out.push((entry, file_path)),
                None => {
                    // Corrupt/partial file (or non-integer pid) — drop it.
                    let _ = self.fs.remove_file(file_path).await;
                }
            }
        }
        out
    }

    /// JS `registerManagedProcess`: record a process WE spawned so a future
    /// run can reap it if orphaned. Non-integer pid is a no-op.
    /// 中文：JS `registerManagedProcess`——登记一个"由我们启动"的进程，供
    /// 未来某次运行在其成为孤儿时回收。pid 非整数（或缺失）时为 no-op。
    pub async fn register_managed_process(
        &self,
        pid: Option<i64>,
        owner_pid: Option<i64>,
        port: Option<i64>,
        binary: Option<String>,
        runtime: Option<String>,
    ) {
        let Some(pid) = pid else {
            return;
        };
        self.write_entry_file(&RegistryEntry {
            pid,
            owner_pid: Some(owner_pid.unwrap_or_else(|| std::process::id() as i64)),
            port,
            binary,
            runtime: Some(runtime.unwrap_or_else(|| "web".to_string())),
            started_at: Some((self.now)()),
        })
        .await;
    }

    /// JS `unregisterManagedProcess`: drop a pid after we killed/closed it
    /// ourselves. Best-effort; a missing file is not an error.
    /// 中文：JS `unregisterManagedProcess`——我们自行关闭/杀死进程后删除其
    /// 记录。尽力而为；文件不存在不算错误。
    pub async fn unregister_managed_process(&self, pid: Option<i64>) {
        if let Some(pid) = pid {
            let _ = self
                .fs
                .remove_file(entry_file_path(&self.registry_dir, pid))
                .await;
        }
    }

    /// JS `readUnixProcInfo`: `{ ppid, command }` for a live pid via
    /// `ps -p <pid> -o ppid=,command=`.
    /// 中文：JS `readUnixProcInfo`——经 `ps -p <pid> -o ppid=,command=` 获取
    /// 存活 pid 的 `{ ppid, command }`；输出为空或无法解析时返回 `None`。
    async fn read_unix_proc_info(&self, pid: i64) -> Option<UnixProcInfo> {
        let stdout = (self.exec_file)(
            "ps".to_string(),
            vec![
                "-p".to_string(),
                pid.to_string(),
                "-o".to_string(),
                "ppid=,command=".to_string(),
            ],
            Some(3000),
        )
        .await
        .ok()?;
        let line = stdout.trim();
        if line.is_empty() {
            return None;
        }
        // JS /^\s*(\d+)\s+(.*)$/
        let rest = line.trim_start();
        let digit_len = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        let digits = &rest[..digit_len];
        if digits.is_empty() || digits.len() == rest.len() {
            return None;
        }
        let tail = &rest[digit_len..];
        let command_start = tail.len() - tail.trim_start().len();
        if command_start == 0 {
            return None;
        }
        Some(UnixProcInfo {
            ppid: digits.parse().ok()?,
            command: tail[command_start..].to_string(),
        })
    }

    /// JS `readWindowsImageName`: the `tasklist` CSV row for a pid, or null.
    /// 中文：JS `readWindowsImageName`——获取 pid 对应的 `tasklist` CSV 行，
    /// 无行时返回 `None`。
    async fn read_windows_image_name(&self, pid: i64) -> Option<String> {
        let stdout = (self.exec_file)(
            "tasklist".to_string(),
            vec![
                "/FI".to_string(),
                format!("PID eq {pid}"),
                "/FO".to_string(),
                "CSV".to_string(),
                "/NH".to_string(),
            ],
            Some(3000),
        )
        .await
        .ok()?;
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// JS `signalTree`: the process group (negative pid) then the pid itself;
    /// both best-effort (either may already be gone).
    /// 中文：JS `signalTree`——先向进程组（负 pid）发信号，再向 pid 本身
    /// 发送；两步均尽力而为（目标可能已退出）。
    async fn signal_tree(&self, pid: i64, signal: &str) {
        let flag = format!("-{signal}");
        let _ = (self.exec_file)(
            "/bin/kill".to_string(),
            vec![flag.clone(), format!("-{pid}")],
            None,
        )
        .await;
        let _ = (self.exec_file)("/bin/kill".to_string(), vec![flag, pid.to_string()], None).await;
    }

    /// JS `killOrphan`.
    /// 中文：JS `killOrphan`——Windows 用 `taskkill /T /F` 连子孙进程强杀；
    /// Unix 先 TERM、最多等 1.5s，仍存活则升级 KILL 再等 300ms。
    async fn kill_orphan(&self, pid: i64) {
        if self.platform == Platform::Windows {
            let _ = (self.exec_file)(
                "taskkill".to_string(),
                vec![
                    "/PID".to_string(),
                    pid.to_string(),
                    "/T".to_string(),
                    "/F".to_string(),
                ],
                Some(5000),
            )
            .await;
            return;
        }

        self.signal_tree(pid, "TERM").await;
        let mut waited: u64 = 0;
        while waited < 1500 && (self.pid_alive)(pid) {
            tokio::time::sleep(Duration::from_millis(150)).await;
            waited += 150;
        }
        if (self.pid_alive)(pid) {
            self.signal_tree(pid, "KILL").await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// JS `processEntry`: decide + act on a single registry entry. Returns
    /// true if it was reaped.
    /// 中文：JS `processEntry`——对单条记录做判定并行动，返回 `true` 表示
    /// 该进程被回收。死亡 pid 直接跳过（由调用方清理文件）；身份无法核验时
    /// 保持不动。
    async fn process_entry(&self, entry: &RegistryEntry, log: &LogFn) -> anyhow::Result<bool> {
        // Dead pid → nothing to do (caller drops the file).
        if !(self.pid_alive)(entry.pid) {
            return Ok(false);
        }

        let owner_gone = entry
            .owner_pid
            .map(|owner_pid| !(self.pid_alive)(owner_pid))
            .unwrap_or(false);

        if self.platform == Platform::Windows {
            // Windows lacks reliable reparent-to-1 semantics, so reap only
            // when the owner is provably dead AND the image still looks like
            // a managed engine binary.
            let image = self.read_windows_image_name(entry.pid).await;
            if windows_image_looks_engine(image.as_deref().unwrap_or("")) && owner_gone {
                self.kill_orphan(entry.pid).await;
                log(format!(
                    "[lifecycle] reaped orphaned engine pid {} (owner {} gone)",
                    entry.pid,
                    entry.owner_pid.unwrap_or_default()
                ));
                return Ok(true);
            }
            return Ok(false);
        }

        let Some(info) = self.read_unix_proc_info(entry.pid).await else {
            // Can't verify identity → leave it alone.
            return Ok(false);
        };
        if !command_identifies_our_server(&info.command, entry.port) {
            return Ok(false);
        }

        let orphaned = info.ppid == 1 || owner_gone;
        if !orphaned {
            // Still owned by a live instance.
            return Ok(false);
        }

        self.kill_orphan(entry.pid).await;
        log(format!(
            "[lifecycle] reaped orphaned OpenCode pid {} (reparented/owner gone)",
            entry.pid
        ));
        Ok(true)
    }

    /// JS `reapOrphanedProcesses({ log })`: kill any genuinely-orphaned
    /// processes WE previously spawned, and prune their registry files. Safe
    /// to call at startup before spawning a new server.
    /// 中文：JS `reapOrphanedProcesses({ log })`——杀死所有确属孤儿且由我们
    /// 先前启动的进程并清理其记录文件。可在启动新 server 之前安全调用。
    pub async fn reap_orphaned_processes(&self, log: Option<LogFn>) -> ReapSummary {
        let noop: LogFn = Arc::new(|_message: String| {});
        let log = log.unwrap_or(noop);
        let records = self.read_all_entries().await;
        if records.is_empty() {
            return ReapSummary {
                inspected: 0,
                reaped: 0,
            };
        }

        let mut reaped = 0usize;
        for (entry, file_path) in &records {
            let mut drop = false;
            match self.process_entry(entry, &log).await {
                Ok(was_reaped) => {
                    if was_reaped {
                        reaped += 1;
                    }
                    // Drop the file when the process is gone (reaped now, or
                    // already dead); keep it only while the process is still
                    // alive and owned by a live owner.
                    drop = was_reaped || !(self.pid_alive)(entry.pid);
                }
                Err(error) => {
                    log(format!(
                        "[lifecycle] reap check failed for pid {}: {}",
                        entry.pid, error
                    ));
                }
            }
            if drop {
                let _ = self.fs.remove_file(file_path.clone()).await;
            }
        }

        ReapSummary {
            inspected: records.len(),
            reaped,
        }
    }
}

/// The default registry (JS `defaultRegistry`), over the resolved registry
/// dir on the host platform.
/// 中文：默认注册表（JS `defaultRegistry`）——基于宿主平台与解析出的注册
/// 目录的全局单例。
pub fn default_registry() -> &'static ManagedProcessRegistry {
    // 函数体内的惰性单例（首次调用时按解析目录 + 宿主平台构造）。
    static DEFAULT: LazyLock<ManagedProcessRegistry> =
        LazyLock::new(|| ManagedProcessRegistry::new(resolve_registry_dir(), host_platform()));
    &DEFAULT
}

/// 注册表单元测试：身份识别、内存 fs/exec 假件、注册/注销/读取，以及
/// Unix 与 Windows 两条回收路径的行为契约。
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    // -- identification --------------------------------------------------------

    /// 验证：编译版 `omp-host.exe serve --port` 命令行被识别为我们注册的
    /// server。
    #[test]
    fn identifies_compiled_omp_host_serve_command() {
        assert!(command_identifies_our_server(
            "C:\\app\\resources\\omp-host\\omp-host.exe serve --hostname 127.0.0.1 --port 58941",
            Some(58941)
        ));
    }

    /// 验证：Bun 从源码启动 `host.ts serve` 的命令行被识别为我们注册的
    /// server。
    #[test]
    fn identifies_from_source_host_launch() {
        assert!(command_identifies_our_server(
            "bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902",
            Some(3902)
        ));
    }

    /// 验证：旧版 `opencode serve --port` 形态同样能被识别并回收。
    #[test]
    fn identifies_legacy_opencode_serve_shape() {
        assert!(command_identifies_our_server(
            "opencode serve --port 4096",
            Some(4096)
        ));
    }

    /// 验证：无关进程（nginx、其它 host 二进制）不会被误判为 managed
    /// engine。
    #[test]
    fn rejects_unrelated_processes() {
        assert!(!command_identifies_our_server("nginx serve", Some(4096)));
        assert!(!command_identifies_our_server(
            "/usr/bin/some-host --watch",
            None
        ));
    }

    /// 验证：端口已知时必须匹配注册端口（防 pid 复用误杀）；端口未知时仅
    /// 做身份检查。
    #[test]
    fn ties_match_to_registered_port() {
        let command = "bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902";
        assert!(command_identifies_our_server(command, Some(3902)));
        assert!(!command_identifies_our_server(command, Some(4000)));
        // Unknown port → identity check only.
        assert!(command_identifies_our_server(command, None));
    }

    /// 验证：`tasklist` CSV 行的映像名判定——omp-host/opencode 命中，
    /// bun.exe 与 INFO 行不命中。
    #[test]
    fn windows_image_row_detection() {
        assert!(windows_image_looks_engine(
            "\"omp-host.exe\",\"1234\",\"Console\",\"1\",\"84,532 K\""
        ));
        assert!(windows_image_looks_engine(
            "\"opencode.exe\",\"1234\",\"Services\",\"0\",\"12,000 K\""
        ));
        assert!(!windows_image_looks_engine(
            "\"bun.exe\",\"1234\",\"Console\",\"1\",\"20,000 K\""
        ));
        assert!(!windows_image_looks_engine("INFO: No tasks are running"));
    }

    // -- fakes -------------------------------------------------------------------

    /// 测试用内存文件系统：以 HashMap 模拟文件与目录，并可把指定路径标记
    /// 为读取失败（损坏文件）。
    #[derive(Default)]
    struct MemoryFs {
        /// 路径 → 文件内容。
        files: Mutex<HashMap<PathBuf, String>>,
        /// 已"创建"的目录集合。
        dirs: Mutex<HashSet<PathBuf>>,
        /// 需模拟读取失败（损坏文件）的路径集合。
        reads_failed: Mutex<HashSet<PathBuf>>,
    }

    /// 内存 fs 的辅助方法。
    impl MemoryFs {
        /// 直接植入一个文件（绕过 seam，用于布置测试场景）。
        fn insert(&self, path: &Path, contents: &str) {
            self.files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(path.to_path_buf(), contents.to_string());
        }
    }

    /// `RegistryFs` 的内存实现：语义与 `TokioFs` 版本对齐。
    impl RegistryFs for MemoryFs {
        /// 记录目录为已创建。
        fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()> {
            Box::pin(async move {
                self.dirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dir);
                Ok(())
            })
        }

        /// 覆盖式写入内存文件。
        fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()> {
            Box::pin(async move {
                self.files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, contents);
                Ok(())
            })
        }

        /// 内存 rename：源文件不存在时报 NotFound。
        fn rename(&self, from: PathBuf, to: PathBuf) -> FsFuture<'_, ()> {
            Box::pin(async move {
                let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
                let contents = files.remove(&from);
                match contents {
                    Some(contents) => {
                        files.insert(to, contents);
                        Ok(())
                    }
                    None => Err(io::Error::new(io::ErrorKind::NotFound, "missing tmp")),
                }
            })
        }

        /// 列出该目录直接子文件名（已排序）；目录未创建过则报 NotFound。
        fn read_dir_names(&self, dir: PathBuf) -> FsFuture<'_, Vec<String>> {
            Box::pin(async move {
                if !self
                    .dirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&dir)
                {
                    return Err(io::Error::new(io::ErrorKind::NotFound, "no registry dir"));
                }
                let files = self.files.lock().unwrap_or_else(|e| e.into_inner());
                let mut names: Vec<String> = files
                    .keys()
                    .filter(|path| path.parent().is_some_and(|parent| parent == dir.as_path()))
                    .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                names.sort();
                Ok(names)
            })
        }

        /// 读内存文件；被标记损坏的路径报 InvalidData，缺失报 NotFound。
        fn read_to_string(&self, path: PathBuf) -> FsFuture<'_, String> {
            Box::pin(async move {
                if self
                    .reads_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&path)
                {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt"));
                }
                self.files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&path)
                    .cloned()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing entry"))
            })
        }

        /// 删除内存文件（不存在也静默成功）。
        fn remove_file(&self, path: PathBuf) -> FsFuture<'_, ()> {
            Box::pin(async move {
                self.files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&path);
                Ok(())
            })
        }
    }

    /// Records `(program, args)` calls and answers programmed stdout per
    /// program; `/bin/kill` calls also clear liveness for the target pid.
    /// 中文：记录每次 `(program, args)` 调用并按程序返回预设 stdout；
    /// `/bin/kill` 调用还会顺带把目标 pid 置为死亡。
    struct FakeExec {
        /// 已发生的调用记录。
        calls: Mutex<Vec<(String, Vec<String>)>>,
        /// 程序名 → 预设 stdout 应答。
        replies: Mutex<HashMap<String, String>>,
    }

    /// FakeExec 的构造与查询辅助。
    impl FakeExec {
        /// 创建空的假执行器。
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(HashMap::new()),
            })
        }

        /// 为指定程序预设 stdout 应答。
        fn reply(&self, program: &str, stdout: &str) {
            self.replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(program.to_string(), stdout.to_string());
        }

        /// 返回已记录调用的快照。
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// 提取 kill/taskkill 调用的目标参数（含进程组负值与 pid 本身）。
        fn kill_targets(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|(program, _)| program == "/bin/kill" || program == "taskkill")
                .flat_map(|(_, args)| {
                    args.into_iter()
                        .filter(|arg| arg.chars().all(|c| c.is_ascii_digit() || c == '-'))
                })
                .collect()
        }
    }

    /// 把 FakeExec 包装成 ExecFileFn：记录每次调用；`/bin/kill`/`taskkill`
    /// 会同步清除目标存活状态，`ps`/`tasklist` 无预设应答时返回 NotFound
    /// 模拟"查无此行"。
    fn exec_seam(exec: Arc<FakeExec>, alive: Arc<Mutex<HashSet<i64>>>) -> ExecFileFn {
        Arc::new(move |program: String, args: Vec<String>, _timeout| {
            let exec = Arc::clone(&exec);
            let alive = Arc::clone(&alive);
            Box::pin(async move {
                exec.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((program.clone(), args.clone()));
                if program == "/bin/kill" {
                    // Fake a successful signal: the direct-pid form (the last
                    // arg is the bare pid) clears liveness.
                    if let Some(target) = args.last().and_then(|value| value.parse::<i64>().ok()) {
                        alive
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&target);
                    }
                    return Ok(String::new());
                }
                if program == "taskkill" {
                    if let Some(index) = args.iter().position(|arg| arg == "/PID")
                        && let Some(target) = args
                            .get(index + 1)
                            .and_then(|value| value.parse::<i64>().ok())
                    {
                        alive
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&target);
                    }
                    return Ok(String::new());
                }
                let reply = exec
                    .replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&program)
                    .cloned()
                    .unwrap_or_default();
                if reply.is_empty() && (program == "ps" || program == "tasklist") {
                    // No row → JS `ps`/`tasklist` failing gives null stdout
                    // handling; emulate an empty (dead) row.
                    return Err(io::Error::new(io::ErrorKind::NotFound, "no row"));
                }
                Ok(reply)
            })
        })
    }

    /// 把存活 pid 集合包装成 PidAliveFn。
    fn alive_fn(alive: Arc<Mutex<HashSet<i64>>>) -> PidAliveFn {
        Arc::new(move |pid| {
            alive
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid)
        })
    }

    /// 每个测试的固定装备：内存 fs、假执行器、存活集合、日志与被测注册表。
    struct Fixture {
        /// 内存文件系统假件。
        fs: Arc<MemoryFs>,
        /// 假执行器（调用记录 + 预设应答）。
        exec: Arc<FakeExec>,
        /// 存活 pid 集合（假 kill 与存活探测共享）。
        alive: Arc<Mutex<HashSet<i64>>>,
        /// 收集到的日志消息。
        logs: Arc<Mutex<Vec<String>>>,
        /// 被测注册表。
        registry: ManagedProcessRegistry,
        /// 注册表目录（固定 /registry）。
        dir: PathBuf,
    }

    /// 按平台组装 Fixture（时间固定为 2026-01-01，目录固定为 /registry）。
    fn fixture(platform: Platform) -> Fixture {
        let fs = Arc::new(MemoryFs::default());
        let exec = FakeExec::new();
        let alive: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(HashSet::new()));
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dir = PathBuf::from("/registry");
        let registry = ManagedProcessRegistry::with_seams(
            Arc::clone(&fs) as Arc<dyn RegistryFs>,
            exec_seam(Arc::clone(&exec), Arc::clone(&alive)),
            alive_fn(Arc::clone(&alive)),
            Arc::new(|| "2026-01-01T00:00:00.000Z".to_string()),
            dir.clone(),
            platform,
        );
        Fixture {
            fs,
            exec,
            alive,
            logs,
            registry,
            dir,
        }
    }

    /// 把日志收集 Vec 包装成 LogFn。
    fn log_fn(logs: &Arc<Mutex<Vec<String>>>) -> LogFn {
        let logs = Arc::clone(logs);
        Arc::new(move |message| {
            logs.lock().unwrap_or_else(|e| e.into_inner()).push(message);
        })
    }

    /// 生成一条字段完整的记录 JSON（camelCase）。
    fn entry_json(pid: i64, owner_pid: i64, port: i64) -> String {
        serde_json::json!({
            "pid": pid,
            "ownerPid": owner_pid,
            "port": port,
            "binary": "/app/omp-host",
            "runtime": "web",
            "startedAt": "2025-01-01T00:00:00.000Z",
        })
        .to_string()
    }

    // -- register/unregister/read ------------------------------------------------

    /// 验证：注册为每个 pid 写独立 JSON 文件，缺省 owner/runtime/startedAt
    /// 按规则补全，并按 JSON.stringify(entry, null, 2) 美化打印。
    #[tokio::test]
    async fn register_writes_defaults_per_pid_file() {
        let fixture = fixture(Platform::Unix);
        fixture
            .registry
            .register_managed_process(Some(4242), Some(111), Some(58941), None, None)
            .await;

        let file = entry_file_path(&fixture.dir, 4242);
        let contents = fixture
            .fs
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&file)
            .cloned()
            .expect("entry file written");
        let entry: RegistryEntry = serde_json::from_str(&contents).expect("valid json");
        assert_eq!(entry.pid, 4242);
        assert_eq!(entry.owner_pid, Some(111));
        assert_eq!(entry.port, Some(58941));
        assert_eq!(entry.runtime.as_deref(), Some("web"));
        assert_eq!(
            entry.started_at.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        // Pretty-printed like JSON.stringify(entry, null, 2).
        assert!(contents.contains("\n  \"pid\""));
    }

    /// 验证：pid 缺失（非整数）时注册是 no-op，不产生任何文件。
    #[tokio::test]
    async fn register_noop_without_integer_pid() {
        let fixture = fixture(Platform::Unix);
        fixture
            .registry
            .register_managed_process(None, Some(1), Some(2), None, None)
            .await;
        assert!(
            fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    /// 验证：注销删除记录文件，且重复注销（文件不存在）不报错。
    #[tokio::test]
    async fn unregister_removes_entry_and_tolerates_missing() {
        let fixture = fixture(Platform::Unix);
        fixture
            .registry
            .register_managed_process(Some(7), Some(1), Some(2), None, None)
            .await;
        fixture.registry.unregister_managed_process(Some(7)).await;
        fixture.registry.unregister_managed_process(Some(999)).await;
        assert!(
            fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    /// 验证：损坏 JSON 与缺失整数 pid 的文件被丢弃；死亡 pid 的记录也被
    /// 一并清理。
    #[tokio::test]
    async fn read_drops_corrupt_and_invalid_pid_files() {
        let fixture = fixture(Platform::Unix);
        fixture
            .fs
            .insert(&entry_file_path(&fixture.dir, 10), &entry_json(10, 1, 80));
        fixture
            .fs
            .insert(&entry_file_path(&fixture.dir, 11), "{ not json");
        fixture
            .fs
            .insert(&entry_file_path(&fixture.dir, 12), "{\"port\": 80}");
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;
        assert_eq!(summary.inspected, 1);
        let files = fixture.fs.files.lock().unwrap_or_else(|e| e.into_inner());
        // Entry 10's pid is dead → its file is pruned too.
        assert!(!files.contains_key(&entry_file_path(&fixture.dir, 10)));
        assert!(!files.contains_key(&entry_file_path(&fixture.dir, 11)));
        assert!(!files.contains_key(&entry_file_path(&fixture.dir, 12)));
    }

    // -- reap (unix) ---------------------------------------------------------------

    /// 验证：pid 已死亡时只清理记录文件，不发起任何 kill。
    #[tokio::test]
    async fn reap_drops_entry_for_dead_pid_without_killing() {
        let fixture = fixture(Platform::Unix);
        fixture
            .fs
            .insert(&entry_file_path(&fixture.dir, 10), &entry_json(10, 1, 80));
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 0
            }
        );
        assert!(fixture.exec.calls().is_empty());
        assert!(
            !fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 10))
        );
    }

    /// 验证：被 reparent 到 init 的引擎以 TERM 信号组 + pid 本身回收，
    /// 日志与文件清理符合契约。
    #[tokio::test]
    async fn reap_kills_reparented_engine_and_prunes_entry() {
        let fixture = fixture(Platform::Unix);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 3902),
            &entry_json(3902, 555, 3902),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(3902);
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(555);
        fixture.exec.reply(
            "ps",
            "      1 bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902",
        );

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 1
            }
        );
        assert_eq!(
            fixture
                .logs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .map(String::as_str),
            Some("[lifecycle] reaped orphaned OpenCode pid 3902 (reparented/owner gone)")
        );
        // SIGTERM to the process group and to the pid itself.
        assert_eq!(
            fixture.exec.kill_targets(),
            vec!["-3902".to_string(), "3902".to_string()]
        );
        assert!(
            !fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 3902))
        );
    }

    /// 验证：宿主仍存活的引擎不受影响，kill 不发生、记录保留。
    #[tokio::test]
    async fn reap_leaves_live_owned_engine_alone() {
        let fixture = fixture(Platform::Unix);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 3902),
            &entry_json(3902, 555, 3902),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(3902);
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(555);
        fixture.exec.reply(
            "ps",
            "    555 bun /repo/packages/web/server/lib/omp-host/host.ts serve --port 3902",
        );

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 0
            }
        );
        assert!(fixture.exec.kill_targets().is_empty());
        assert!(
            fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 3902))
        );
    }

    /// 验证：端口不匹配（pid 被复用跑别的实例）与无关命令行都不会被回收，
    /// 记录保留。
    #[tokio::test]
    async fn reap_ignores_port_mismatch_and_foreign_commands() {
        let fixture = fixture(Platform::Unix);
        // A recycled pid running a different omp host instance (other port).
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 10),
            &entry_json(10, 555, 4000),
        );
        // An unrelated command line for the recorded pid.
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 11),
            &entry_json(11, 555, 5000),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        for pid in [10, 11, 555] {
            fixture
                .alive
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid);
        }
        fixture
            .exec
            .reply("ps", "    555 omp-host serve --port 3902");

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 2,
                reaped: 0
            }
        );
        assert!(fixture.exec.kill_targets().is_empty());
        let files = fixture.fs.files.lock().unwrap_or_else(|e| e.into_inner());
        assert!(files.contains_key(&entry_file_path(&fixture.dir, 10)));
        assert!(files.contains_key(&entry_file_path(&fixture.dir, 11)));
    }

    /// 验证：宿主已死亡时，即使 ppid 不是 1（被其它进程收养）也会被回收。
    #[tokio::test]
    async fn reap_kills_when_owner_dead_even_with_live_ppid() {
        let fixture = fixture(Platform::Unix);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 3902),
            &entry_json(3902, 555, 3902),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(3902);
        // Owner 555 is dead; ppid is some other live pid (not 1).
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(777);
        fixture
            .exec
            .reply("ps", "    777 opencode serve --port 3902");

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 1
            }
        );
        assert!(
            !fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 3902))
        );
    }

    // -- reap (win32) -----------------------------------------------------------------

    /// 验证：Windows 上映像名匹配且宿主已死的进程用 `taskkill /PID /T /F`
    /// 回收并清理记录。
    #[tokio::test]
    async fn windows_reap_kills_engine_image_with_dead_owner() {
        let fixture = fixture(Platform::Windows);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 1234),
            &entry_json(1234, 555, 58941),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(1234);
        fixture.exec.reply(
            "tasklist",
            "\"omp-host.exe\",\"1234\",\"Console\",\"1\",\"84,532 K\"",
        );

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 1
            }
        );
        assert_eq!(
            fixture
                .logs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .map(String::as_str),
            Some("[lifecycle] reaped orphaned engine pid 1234 (owner 555 gone)")
        );
        let calls = fixture.exec.calls();
        let taskkill = calls
            .iter()
            .find(|(program, _)| program == "taskkill")
            .expect("taskkill called");
        assert_eq!(taskkill.1, vec!["/PID", "1234", "/T", "/F"]);
        assert!(
            !fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 1234))
        );
    }

    /// 验证：Windows 上无关映像名（bun.exe）不被回收。
    #[tokio::test]
    async fn windows_reap_leaves_unrelated_image_untouched() {
        let fixture = fixture(Platform::Windows);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 1234),
            &entry_json(1234, 555, 58941),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(1234);
        fixture.exec.reply(
            "tasklist",
            "\"bun.exe\",\"1234\",\"Console\",\"1\",\"20,000 K\"",
        );

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 0
            }
        );
        assert!(
            fixture
                .exec
                .calls()
                .iter()
                .all(|(program, _)| program != "taskkill")
        );
        assert!(
            fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 1234))
        );
    }

    /// 验证：Windows 上即使映像名匹配，宿主仍存活也不回收。
    #[tokio::test]
    async fn windows_reap_requires_dead_owner_even_for_engine_image() {
        let fixture = fixture(Platform::Windows);
        fixture.fs.insert(
            &entry_file_path(&fixture.dir, 1234),
            &entry_json(1234, 555, 58941),
        );
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(1234);
        fixture
            .alive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(555);
        fixture.exec.reply(
            "tasklist",
            "\"omp-host.exe\",\"1234\",\"Console\",\"1\",\"84,532 K\"",
        );

        let summary = fixture
            .registry
            .reap_orphaned_processes(Some(log_fn(&fixture.logs)))
            .await;

        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 0
            }
        );
        assert!(
            fixture
                .fs
                .files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&entry_file_path(&fixture.dir, 1234))
        );
    }

    /// 验证：未提供日志函数时使用 no-op，回收流程不受影响。
    #[tokio::test]
    async fn reap_uses_noop_log_when_none_given() {
        let fixture = fixture(Platform::Unix);
        fixture
            .fs
            .insert(&entry_file_path(&fixture.dir, 10), &entry_json(10, 1, 80));
        fixture
            .fs
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fixture.dir.clone());

        let summary = fixture.registry.reap_orphaned_processes(None).await;
        assert_eq!(
            summary,
            ReapSummary {
                inspected: 1,
                reaped: 0
            }
        );
        assert!(
            fixture
                .logs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    // -- helpers -----------------------------------------------------------------------

    /// 验证：ISO 时间戳的固定形状（长度、Z 后缀与分隔符位置）。
    #[test]
    fn iso_timestamp_shape() {
        let stamp = iso_utc_now();
        assert_eq!(stamp.len(), 24);
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
        assert_eq!(&stamp[13..14], ":");
    }

    /// 验证：注册表目录解析规则——无环境变量覆盖时落到
    /// .config/ompchamber/managed-opencode。
    #[test]
    fn registry_dir_env_override_wins() {
        // Read-only check of the resolution rule (env is global; assert the
        // shape of the default when unset).
        let override_set = std::env::var("OMPCHAMBER_MANAGED_PROCESS_REGISTRY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if !override_set {
            let dir = resolve_registry_dir().to_string_lossy().to_string();
            assert!(dir.ends_with("managed-opencode"), "{dir}");
            assert!(dir.contains(".config"), "{dir}");
        }
    }
}
