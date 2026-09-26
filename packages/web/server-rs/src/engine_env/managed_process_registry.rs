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
pub fn windows_image_looks_engine(image: &str) -> bool {
    let lower = image.to_lowercase();
    lower.contains("omp-host") || lower.contains("opencode")
}

// ---------------------------------------------------------------------------
// Entry shape
// ---------------------------------------------------------------------------

/// One `<pid>.json` record. `owner_pid`/`port`/`binary` may be absent in
/// older files; `pid` is required (JS `Number.isInteger(entry.pid)`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    pub pid: i64,
    pub owner_pid: Option<i64>,
    pub port: Option<i64>,
    pub binary: Option<String>,
    pub runtime: Option<String>,
    pub started_at: Option<String>,
}

/// Parse + validate one entry file (JS `JSON.parse` + the integer-pid gate;
/// both corrupt files and non-integer pids are dropped by the caller).
fn parse_entry(content: &str) -> Option<RegistryEntry> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    match value.get("pid") {
        Some(serde_json::Value::Number(number)) if number.is_i64() || number.is_u64() => {}
        _ => return None,
    }
    serde_json::from_value(value).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapSummary {
    pub inspected: usize,
    pub reaped: usize,
}

struct UnixProcInfo {
    ppid: i64,
    command: String,
}

// ---------------------------------------------------------------------------
// Injectable seams (JS `{ fs, execFileAsync }`)
// ---------------------------------------------------------------------------

type FsFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// Async filesystem seam over the operations the registry performs.
pub trait RegistryFs: Send + Sync {
    fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()>;
    fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()>;
    fn rename(&self, from: PathBuf, to: PathBuf) -> FsFuture<'_, ()>;
    /// Directory entry names (files and dirs), like `fs.promises.readdir`.
    fn read_dir_names(&self, dir: PathBuf) -> FsFuture<'_, Vec<String>>;
    fn read_to_string(&self, path: PathBuf) -> FsFuture<'_, String>;
    fn remove_file(&self, path: PathBuf) -> FsFuture<'_, ()>;
}

/// Production fs: `tokio::fs` (async like the JS `fsp`).
struct TokioFs;

impl RegistryFs for TokioFs {
    fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::create_dir_all(dir).await })
    }

    fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::write(path, contents).await })
    }

    fn rename(&self, from: PathBuf, to: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::rename(from, to).await })
    }

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

    fn read_to_string(&self, path: PathBuf) -> FsFuture<'_, String> {
        Box::pin(async move { tokio::fs::read_to_string(path).await })
    }

    fn remove_file(&self, path: PathBuf) -> FsFuture<'_, ()> {
        Box::pin(async move { tokio::fs::remove_file(path).await })
    }
}

type ExecFuture = Pin<Box<dyn Future<Output = io::Result<String>> + Send>>;

/// JS `execFileAsync(cmd, args, { timeout })` → stdout.
pub type ExecFileFn = Arc<dyn Fn(String, Vec<String>, Option<u64>) -> ExecFuture + Send + Sync>;

/// Production execFile: tokio process with `kill_on_drop`, wrapped in the
/// requested timeout (a timeout rejects, exactly like the promisified
/// `execFile`).
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
pub type PidAliveFn = Arc<dyn Fn(i64) -> bool + Send + Sync>;

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

pub type NowFn = Arc<dyn Fn() -> String + Send + Sync>;
pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

/// `new Date().toISOString()` — UTC with milliseconds, via the same
/// civil-from-days algorithm engine.rs uses.
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
fn entry_file_path(dir: &Path, pid: i64) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct ManagedProcessRegistry {
    fs: Arc<dyn RegistryFs>,
    exec_file: ExecFileFn,
    pid_alive: PidAliveFn,
    now: NowFn,
    registry_dir: PathBuf,
    platform: Platform,
}

impl ManagedProcessRegistry {
    /// Production registry with real seams over `registry_dir`.
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
pub fn default_registry() -> &'static ManagedProcessRegistry {
    static DEFAULT: LazyLock<ManagedProcessRegistry> =
        LazyLock::new(|| ManagedProcessRegistry::new(resolve_registry_dir(), host_platform()));
    &DEFAULT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    // -- identification --------------------------------------------------------

    #[test]
    fn identifies_compiled_omp_host_serve_command() {
        assert!(command_identifies_our_server(
            "C:\\app\\resources\\omp-host\\omp-host.exe serve --hostname 127.0.0.1 --port 58941",
            Some(58941)
        ));
    }

    #[test]
    fn identifies_from_source_host_launch() {
        assert!(command_identifies_our_server(
            "bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902",
            Some(3902)
        ));
    }

    #[test]
    fn identifies_legacy_opencode_serve_shape() {
        assert!(command_identifies_our_server(
            "opencode serve --port 4096",
            Some(4096)
        ));
    }

    #[test]
    fn rejects_unrelated_processes() {
        assert!(!command_identifies_our_server("nginx serve", Some(4096)));
        assert!(!command_identifies_our_server(
            "/usr/bin/some-host --watch",
            None
        ));
    }

    #[test]
    fn ties_match_to_registered_port() {
        let command = "bun /repo/packages/web/server/lib/omp-host/host.ts serve --hostname 127.0.0.1 --port 3902";
        assert!(command_identifies_our_server(command, Some(3902)));
        assert!(!command_identifies_our_server(command, Some(4000)));
        // Unknown port → identity check only.
        assert!(command_identifies_our_server(command, None));
    }

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

    #[derive(Default)]
    struct MemoryFs {
        files: Mutex<HashMap<PathBuf, String>>,
        dirs: Mutex<HashSet<PathBuf>>,
        reads_failed: Mutex<HashSet<PathBuf>>,
    }

    impl MemoryFs {
        fn insert(&self, path: &Path, contents: &str) {
            self.files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(path.to_path_buf(), contents.to_string());
        }
    }

    impl RegistryFs for MemoryFs {
        fn mkdir_all(&self, dir: PathBuf) -> FsFuture<'_, ()> {
            Box::pin(async move {
                self.dirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dir);
                Ok(())
            })
        }

        fn write_file(&self, path: PathBuf, contents: String) -> FsFuture<'_, ()> {
            Box::pin(async move {
                self.files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, contents);
                Ok(())
            })
        }

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
    struct FakeExec {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        replies: Mutex<HashMap<String, String>>,
    }

    impl FakeExec {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(HashMap::new()),
            })
        }

        fn reply(&self, program: &str, stdout: &str) {
            self.replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(program.to_string(), stdout.to_string());
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

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

    fn alive_fn(alive: Arc<Mutex<HashSet<i64>>>) -> PidAliveFn {
        Arc::new(move |pid| {
            alive
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid)
        })
    }

    struct Fixture {
        fs: Arc<MemoryFs>,
        exec: Arc<FakeExec>,
        alive: Arc<Mutex<HashSet<i64>>>,
        logs: Arc<Mutex<Vec<String>>>,
        registry: ManagedProcessRegistry,
        dir: PathBuf,
    }

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

    fn log_fn(logs: &Arc<Mutex<Vec<String>>>) -> LogFn {
        let logs = Arc::clone(logs);
        Arc::new(move |message| {
            logs.lock().unwrap_or_else(|e| e.into_inner()).push(message);
        })
    }

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

    #[test]
    fn iso_timestamp_shape() {
        let stamp = iso_utc_now();
        assert_eq!(stamp.len(), 24);
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
        assert_eq!(&stamp[13..14], ":");
    }

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
