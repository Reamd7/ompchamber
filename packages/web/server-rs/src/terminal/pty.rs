//! PTY seam for the terminal runtime (JS `loadPtyProvider` + node-pty).
//!
//! The runtime depends only on [`PtyProvider`]/[`PtyProcess`], mirroring the
//! JS injection points: production wires [`RealPtyProvider`] (portable-pty),
//! tests wire fakes that record writes/resizes/kills and emit output/exit
//! events on demand.
//!
//! Real-provider notes vs the JS (node-pty) backend:
//! - `backend` reports `"portable-pty"` (JS: `"node-pty"` / `"bun-pty"`); the
//!   snapshot field exists so clients can display the backend, none branch on
//!   it.
//! - Signal delivery: node-pty signals the child directly; here group signals
//!   go through `/bin/kill -<SIG> -<pgid>` (the child leads its PTY session
//!   group), and `ChildKiller::kill()` provides the SIGKILL escalation. On
//!   Windows the `/bin/kill` step fails silently and only TerminateProcess
//!   runs.
//! - `ExitStatus` from portable-pty exposes no signal number, so the `signal`
//!   field of the wire `exit` event is always `null` (gap; the exit code is
//!   preserved).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

/// FIFO events from one PTY process into its session pump. `process_id`
/// replaces the JS pointer-identity check (`event.process !== session.process`)
/// used to drop stale callbacks from replaced processes.
#[derive(Debug)]
pub struct PtyEvent {
    pub process_id: u64,
    pub kind: PtyEventKind,
}

#[derive(Debug)]
pub enum PtyEventKind {
    Output(Vec<u8>),
    Exit {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Everything `spawnPty` computed before handing off to the provider.
pub struct PtySpawnRequest {
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub executable: String,
    pub args: Vec<String>,
    /// Full replacement environment (the child never inherits the OS environ
    /// implicitly; portable-pty merges nothing when `env_clear` is used).
    pub env: HashMap<String, String>,
    pub events: UnboundedSender<PtyEvent>,
}

pub struct SpawnedPty {
    pub process: Arc<dyn PtyProcess>,
    pub backend: &'static str,
    /// Windows conpty.dll mode (JS `conptyDll`): suppresses DA1 answers. The
    /// Rust backend always uses ConPTY on Windows, so it reports `true` there.
    pub conpty_dll: bool,
}

pub trait PtyProvider: Send + Sync {
    fn spawn(&self, request: PtySpawnRequest) -> io::Result<SpawnedPty>;
}

pub trait PtyProcess: Send + Sync {
    fn id(&self) -> u64;
    fn pid(&self) -> Option<u32>;
    fn write(&self, data: &str) -> io::Result<()>;
    fn resize(&self, cols: u16, rows: u16);
    /// `killProcess`: `force` escalates to SIGKILL. Never errors — an already
    /// dead process is swallowed like the JS `try/catch`.
    fn kill(&self, force: bool);
    /// Completes when the process exits; a caller that attaches after the exit
    /// never completes (JS `onExit` registration semantics). Boxed because the
    /// trait stays dyn-compatible (async-fn-in-trait is not).
    fn wait_exit(&self) -> futures::future::BoxFuture<'static, ()>;
}

// ---------------------------------------------------------------------------
// Real provider (portable-pty)
// ---------------------------------------------------------------------------

pub struct RealPtyProvider {
    next_id: AtomicU64,
}

impl RealPtyProvider {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }
}

impl Default for RealPtyProvider {
    fn default() -> Self {
        Self::new()
    }
}

struct RealPtyProcess {
    id: u64,
    pid: Option<u32>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    exited: Arc<Notify>,
}

impl PtyProcess for RealPtyProcess {
    fn id(&self) -> u64 {
        self.id
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn write(&self, data: &str) -> io::Result<()> {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(writer) => writer.write_all(data.as_bytes()),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty writer closed",
            )),
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let Ok(master) = self.master.lock() else {
            return;
        };
        let _ = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn kill(&self, force: bool) {
        let signal = if force { "KILL" } else { "TERM" };
        // Process-group signal first, exactly like `process.kill(-pid, sig)`.
        if let Some(pid) = self.pid.filter(|pid| *pid > 0) {
            let _ = Command::new("kill")
                .args([format!("-{signal}"), format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if force {
            // Escalation path: portable-pty's ChildKiller terminates the child.
            if let Ok(mut killer) = self.killer.lock() {
                let _ = killer.kill();
            }
        }
    }

    fn wait_exit(&self) -> futures::future::BoxFuture<'static, ()> {
        let exited = Arc::clone(&self.exited);
        Box::pin(async move {
            // JS onExit semantics: only waiters registered before the exit
            // notification fire; late waiters park forever (the termination
            // grace timeout escalates to SIGKILL instead).
            exited.notified().await;
        })
    }
}

impl PtyProvider for RealPtyProvider {
    fn spawn(&self, request: PtySpawnRequest) -> io::Result<SpawnedPty> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(error.to_string()))?;

        let mut command = CommandBuilder::new(&request.executable);
        command.args(request.args.iter().map(String::as_str));
        if !request.cwd.is_empty() {
            command.cwd(&request.cwd);
        }
        command.env_clear();
        for (key, value) in &request.env {
            command.env(key, value);
        }

        let child: Box<dyn Child + Send + Sync> = pair
            .slave
            .spawn_command(command)
            .map_err(|error| io::Error::other(error.to_string()))?;
        // Drop the slave so EIO surfaces when the child exits.
        drop(pair.slave);

        let pid = child.process_id();
        let killer = child.clone_killer();
        let child = Arc::new(Mutex::new(child));
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let master = Arc::new(Mutex::new(pair.master));
        let exited = Arc::new(Notify::new());

        // PTY read loop → Output events (spawn_blocking: blocking IO).
        {
            let events = request.events.clone();
            let exited = Arc::clone(&exited);
            let _ = std::thread::spawn(move || {
                let mut reader = reader;
                let mut buffer = [0u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if events
                                .send(PtyEvent {
                                    process_id: id,
                                    kind: PtyEventKind::Output(buffer[..read].to_vec()),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                // Reader EOF means the child's side closed; the exit watcher
                // fires the notify, so nothing to do here.
                drop(exited);
            });
        }

        // Exit watcher → Exit event + waiters wake.
        {
            let events = request.events.clone();
            let exited = Arc::clone(&exited);
            let child = Arc::clone(&child);
            let _ = std::thread::spawn(move || {
                let status = {
                    let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
                    guard.wait()
                };
                let exit_code = status.ok().map(|status| status.exit_code() as i32);
                let _ = events.send(PtyEvent {
                    process_id: id,
                    kind: PtyEventKind::Exit {
                        exit_code,
                        signal: None,
                    },
                });
                exited.notify_waiters();
            });
        }

        Ok(SpawnedPty {
            process: Arc::new(RealPtyProcess {
                id,
                pid,
                writer: Mutex::new(Some(writer)),
                master,
                killer: Mutex::new(killer),
                exited,
            }),
            backend: "portable-pty",
            conpty_dll: cfg!(windows),
        })
    }
}

// ---------------------------------------------------------------------------
// Real filesystem/PATH deps for the shell resolver (env-runtime.js subset)
// ---------------------------------------------------------------------------

/// `isExecutable` (env-runtime.js): a regular file that is executable (Posix
/// mode bits) or a Windows executable extension.
pub fn real_is_executable(path: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let lower = path.to_ascii_lowercase();
        [".exe", ".cmd", ".bat", ".com"]
            .iter()
            .any(|ext| lower.ends_with(ext))
            || !lower.contains('.')
    }
}

/// `searchPathFor` (env-runtime.js): split the search path by the platform
/// delimiter, try PATHEXT variants then the bare name on Windows.
pub fn real_search_path_for(name: &str, search_path: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let delimiter = if cfg!(windows) { ';' } else { ':' };
    let mut candidate_names: Vec<String> = Vec::new();
    if cfg!(windows) && !name.contains('.') {
        let path_ext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for ext in path_ext.split(';') {
            let ext = ext.trim();
            if ext.is_empty() {
                continue;
            }
            let suffix = if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            };
            let candidate = format!("{name}{suffix}");
            if !candidate_names
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&candidate))
            {
                candidate_names.push(candidate);
            }
        }
    }
    candidate_names.push(name.to_string());

    for dir in search_path.split(delimiter).filter(|dir| !dir.is_empty()) {
        for candidate in &candidate_names {
            let path = std::path::Path::new(dir).join(candidate);
            if real_is_executable(&path.to_string_lossy()) {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// `buildAugmentedPath` conservative core: keep the current PATH. The
/// login-shell PATH augmentation lands with the `env-runtime.js` port.
pub fn real_build_augmented_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// `/etc/shells` contents for shell discovery.
pub fn real_read_etc_shells() -> Option<String> {
    std::fs::read_to_string("/etc/shells").ok()
}

// ---------------------------------------------------------------------------
// Test fake (JS runtime.test.js fake pty provider)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    pub struct FakePtyProcess {
        id: u64,
        pid: u32,
        pub writes: Mutex<Vec<String>>,
        pub resizes: Mutex<Vec<(u16, u16)>>,
        pub kills: Mutex<Vec<&'static str>>,
        pub spawned_with: Mutex<PtySpawnSnapshot>,
        events: UnboundedSender<PtyEvent>,
        exited: Arc<Notify>,
    }

    #[derive(Debug, Clone)]
    pub struct PtySpawnSnapshot {
        pub cwd: String,
        pub cols: u16,
        pub rows: u16,
        pub executable: String,
        pub args: Vec<String>,
        pub env: HashMap<String, String>,
    }

    impl FakePtyProcess {
        pub fn emit_data(&self, data: &str) {
            let _ = self.events.send(PtyEvent {
                process_id: self.id,
                kind: PtyEventKind::Output(data.as_bytes().to_vec()),
            });
        }

        pub fn emit_exit(&self, exit_code: Option<i32>, signal: Option<i32>) {
            let _ = self.events.send(PtyEvent {
                process_id: self.id,
                kind: PtyEventKind::Exit { exit_code, signal },
            });
            self.exited.notify_waiters();
        }

        pub fn writes(&self) -> Vec<String> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn resizes(&self) -> Vec<(u16, u16)> {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn kills(&self) -> Vec<&'static str> {
            self.kills.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl PtyProcess for FakePtyProcess {
        fn id(&self) -> u64 {
            self.id
        }

        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }

        fn write(&self, data: &str) -> io::Result<()> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(data.to_string());
            Ok(())
        }

        fn resize(&self, cols: u16, rows: u16) {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((cols, rows));
        }

        fn kill(&self, force: bool) {
            self.kills
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(if force { "SIGKILL" } else { "SIGTERM" });
        }

        fn wait_exit(&self) -> futures::future::BoxFuture<'static, ()> {
            let exited = Arc::clone(&self.exited);
            Box::pin(async move {
                // Mirror JS onExit: only waiters registered before emit_exit
                // fire; the fake never exits on its own (SIGTERM escalation).
                let notify = exited;
                notify.notified().await;
            })
        }
    }

    pub struct FakePtyProvider {
        pub spawned: Mutex<Vec<Arc<FakePtyProcess>>>,
        /// When set, `spawn` fails for executables not listed here — used to
        /// exercise the multi-executable fallback.
        pub fail_other_executables: Option<Vec<String>>,
        /// Artificial spawn latency (ms) so tests can hold a create in flight
        /// and observe the pending-create dedupe/conflict paths.
        pub spawn_delay_ms: std::sync::atomic::AtomicU64,
    }

    impl FakePtyProvider {
        pub fn new() -> Self {
            Self {
                spawned: Mutex::new(Vec::new()),
                fail_other_executables: None,
                spawn_delay_ms: std::sync::atomic::AtomicU64::new(0),
            }
        }
    }

    impl Default for FakePtyProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PtyProvider for FakePtyProvider {
        fn spawn(&self, request: PtySpawnRequest) -> io::Result<SpawnedPty> {
            let delay = self
                .spawn_delay_ms
                .load(std::sync::atomic::Ordering::Acquire);
            if delay > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            if let Some(allowed) = &self.fail_other_executables {
                if !allowed.iter().any(|exe| *exe == request.executable) {
                    return Err(io::Error::other("spawn failed"));
                }
            }
            let count = self.spawned.lock().unwrap_or_else(|e| e.into_inner()).len();
            let process = Arc::new(FakePtyProcess {
                id: 1 + count as u64,
                pid: 123 + count as u32,
                writes: Mutex::new(Vec::new()),
                resizes: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                spawned_with: Mutex::new(PtySpawnSnapshot {
                    cwd: request.cwd.clone(),
                    cols: request.cols,
                    rows: request.rows,
                    executable: request.executable.clone(),
                    args: request.args.clone(),
                    env: request.env.clone(),
                }),
                events: request.events,
                exited: Arc::new(Notify::new()),
            });
            self.spawned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::clone(&process));
            Ok(SpawnedPty {
                process,
                backend: "fake-pty",
                conpty_dll: false,
            })
        }
    }
}
