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
//!
//! 中文说明：定义运行时唯一依赖的 PTY 抽象——PtyProvider/PtyProcess 两个
//! trait 及 PtyEvent 事件模型，对应 JS 的注入点：生产端接 RealPtyProvider
//!（portable-pty），测试端接记录写入/resize/kill 并按需注入输出与退出的
//! fake。本文件同时提供 shell 发现所需的真实文件系统/PATH 依赖
//!（env-runtime.js 子集：可执行检测、PATH 搜索、/etc/shells 读取）。

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
/// 中文补充：一次 PTY 生命周期事件（输出或退出），按 FIFO 顺序进入会话泵；
/// 携带 process_id 以丢弃被替换进程的陈旧回调。
#[derive(Debug)]
pub struct PtyEvent {
    /// 产生事件的进程 id（见结构体说明）。
    pub process_id: u64,
    /// 事件种类与载荷。
    pub kind: PtyEventKind,
}

/// PTY 事件种类：新增输出或进程退出。
#[derive(Debug)]
pub enum PtyEventKind {
    /// 新增的原始输出字节（未经任何解析）。
    Output(Vec<u8>),
    /// 进程退出；字段为 None 表示无法获取对应信息。
    Exit {
        /// 退出码；portable-pty 总能取到，取不到时为 None。
        exit_code: Option<i32>,
        /// 终止信号号；当前后端不暴露，恒为 None（见模块说明的差距备注）。
        signal: Option<i32>,
    },
}

/// Everything `spawnPty` computed before handing off to the provider.
/// 中文补充：请求对象完全由调用方构造，provider 只负责启动。
pub struct PtySpawnRequest {
    /// 工作目录；空串表示不设置。
    pub cwd: String,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
    /// shell 可执行文件路径。
    pub executable: String,
    /// 启动参数（如登录模式参数）。
    pub args: Vec<String>,
    /// Full replacement environment (the child never inherits the OS environ
    /// implicitly; portable-pty merges nothing when `env_clear` is used).
    /// 中文补充：配合 env_clear 使用，子进程不会隐式继承宿主环境。
    pub env: HashMap<String, String>,
    /// 事件通道：provider 把输出/退出事件发回会话泵。
    pub events: UnboundedSender<PtyEvent>,
}

/// spawn 的产物：进程控制句柄与后端元信息。
pub struct SpawnedPty {
    /// 进程句柄（写/resize/kill/等待退出）。
    pub process: Arc<dyn PtyProcess>,
    /// 后端标识，进快照的 ptyBackend 字段；客户端只展示、不分支。
    pub backend: &'static str,
    /// Windows conpty.dll mode (JS `conptyDll`): suppresses DA1 answers. The
    /// Rust backend always uses ConPTY on Windows, so it reports `true` there.
    /// 中文补充：Windows 上 ConPTY 不认识 DA1，用它抑制应答。
    pub conpty_dll: bool,
}

/// PTY 工厂 trait：运行时唯一的 spawn 依赖缝，生产与测试各有实现。
pub trait PtyProvider: Send + Sync {
    /// 按 request 启动 PTY 进程；失败返回 io::Error（多可执行文件回退由调用方驱动）。
    fn spawn(&self, request: PtySpawnRequest) -> io::Result<SpawnedPty>;
}

/// 单个 PTY 进程的控制面：写入、调整尺寸、终止与等待退出。
pub trait PtyProcess: Send + Sync {
    /// 进程身份 id：会话用它识别并丢弃被替换进程的陈旧事件。
    fn id(&self) -> u64;
    /// 系统 pid（进程组信号用）；不可得时为 None。
    fn pid(&self) -> Option<u32>;
    /// 向 PTY 写入 UTF-8 输入。
    fn write(&self, data: &str) -> io::Result<()>;
    /// 调整 PTY 尺寸。
    fn resize(&self, cols: u16, rows: u16);
    /// `killProcess`: `force` escalates to SIGKILL. Never errors — an already
    /// dead process is swallowed like the JS `try/catch`.
    /// 中文补充：进程已死时静默吞掉，等价 JS 的 try/catch。
    fn kill(&self, force: bool);
    /// Completes when the process exits; a caller that attaches after the exit
    /// never completes (JS `onExit` registration semantics). Boxed because the
    /// trait stays dyn-compatible (async-fn-in-trait is not).
    /// 中文补充：以 BoxFuture 提供，保持 trait 的 dyn 兼容性。
    fn wait_exit(&self) -> futures::future::BoxFuture<'static, ()>;
}

// ---------------------------------------------------------------------------
// Real provider (portable-pty)
// ---------------------------------------------------------------------------

/// 生产实现：基于 portable-pty 的 PTY provider。
pub struct RealPtyProvider {
    /// 进程 id 计数器（首个进程 id 为 1，单调递增）。
    next_id: AtomicU64,
}

/// RealPtyProvider 的固有构造方法。
impl RealPtyProvider {
    /// 新建 provider，id 计数从 0 起。
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }
}

/// 与 new 等价的默认构造。
impl Default for RealPtyProvider {
    /// 委托给 new。
    fn default() -> Self {
        Self::new()
    }
}

/// portable-pty 进程的控制面封装：写端、master（resize）、killer（SIGKILL
/// 升级）与退出通知。
struct RealPtyProcess {
    /// provider 分配的进程 id。
    id: u64,
    /// 子进程 pid，用于进程组信号。
    pid: Option<u32>,
    /// PTY 写端；取走后为 None，此时写返回 BrokenPipe。
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// master 端句柄，resize 经由它下发。
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// ChildKiller，仅 force 终止的升级路径使用。
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 退出通知；监视线程 notify_waiters 唤醒所有在册等待者。
    exited: Arc<Notify>,
}

/// 真实进程对控制面 trait 的实现。
impl PtyProcess for RealPtyProcess {
    /// 返回分配的进程 id。
    fn id(&self) -> u64 {
        self.id
    }

    /// 返回子进程 pid。
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 把 UTF-8 字节写入 PTY；写端已关闭时返回 BrokenPipe。
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

    /// 调整 PTY 尺寸；失败忽略（尺寸策略归协商层管）。
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

    /// 终止进程：先向进程组发 TERM/KILL（对齐 process.kill(-pid, sig)），
    /// force 时再经 ChildKiller 直接终止子进程，全程不报错。
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

    /// 注册退出等待；退出后才注册的等待者永远挂起，由终止宽限期的
    /// SIGKILL 升级兜底。
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

/// 真实 provider 的 spawn 实现。
impl PtyProvider for RealPtyProvider {
    /// 完整启动流程：openpty → 构造命令（cwd/args/env_clear+注入环境）→
    /// spawn（随后丢弃 slave 让退出表现为 EIO）→ 取 writer/克隆 reader →
    /// 启动读循环与退出监视线程 → 返回句柄与后端标识。
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
/// 中文补充：Windows 分支看扩展名（无扩展名也算可执行，匹配 cmd 内建行为）。
pub fn real_is_executable(path: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
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
/// 中文补充：返回首个命中目录里的完整路径；找不到返回 None。
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
/// 中文补充：登录 shell 的 PATH 增强随 env-runtime.js 移植再补。
pub fn real_build_augmented_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// `/etc/shells` contents for shell discovery.
/// 中文补充：读取失败（如 Windows）返回 None，发现流程跳过该来源。
pub fn real_read_etc_shells() -> Option<String> {
    std::fs::read_to_string("/etc/shells").ok()
}

// ---------------------------------------------------------------------------
// Test fake (JS runtime.test.js fake pty provider)
// ---------------------------------------------------------------------------

/// 测试用 fake PTY（对应 JS runtime.test.js 的 fake provider）：记录一切
/// 控制面调用，输出与退出由测试按需注入。
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    /// 假进程：记录控制面调用，事件由测试手动注入。
    pub struct FakePtyProcess {
        /// 进程 id（按 spawn 顺序从 1 递增）。
        id: u64,
        /// 假 pid（从 123 起）。
        pid: u32,
        /// 每次收到的 write 内容。
        pub writes: Mutex<Vec<String>>,
        /// 每次收到的 resize (cols, rows)。
        pub resizes: Mutex<Vec<(u16, u16)>>,
        /// 每次收到的 kill 信号名。
        pub kills: Mutex<Vec<&'static str>>,
        /// spawn 时的完整参数快照，供断言启动配置。
        pub spawned_with: Mutex<PtySpawnSnapshot>,
        /// 事件通道；emit_* 经它送回会话泵。
        events: UnboundedSender<PtyEvent>,
        /// 退出通知，配合 wait_exit 复现 onExit 语义。
        exited: Arc<Notify>,
    }

    /// spawn 参数快照（cwd/尺寸/可执行文件/参数/环境）。
    #[derive(Debug, Clone)]
    pub struct PtySpawnSnapshot {
        /// 工作目录。
        pub cwd: String,
        /// 列数。
        pub cols: u16,
        /// 行数。
        pub rows: u16,
        /// 可执行文件。
        pub executable: String,
        /// 参数列表。
        pub args: Vec<String>,
        /// 完整替换环境。
        pub env: HashMap<String, String>,
    }

    /// 测试注入与观测接口。
    impl FakePtyProcess {
        /// 注入一段输出（立即经事件通道送达会话泵）。
        pub fn emit_data(&self, data: &str) {
            let _ = self.events.send(PtyEvent {
                process_id: self.id,
                kind: PtyEventKind::Output(data.as_bytes().to_vec()),
            });
        }

        /// 注入退出事件并唤醒已注册的等待者。
        pub fn emit_exit(&self, exit_code: Option<i32>, signal: Option<i32>) {
            let _ = self.events.send(PtyEvent {
                process_id: self.id,
                kind: PtyEventKind::Exit { exit_code, signal },
            });
            self.exited.notify_waiters();
        }

        /// 读取写入记录副本。
        pub fn writes(&self) -> Vec<String> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// 读取 resize 记录副本。
        pub fn resizes(&self) -> Vec<(u16, u16)> {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// 读取 kill 信号记录副本。
        pub fn kills(&self) -> Vec<&'static str> {
            self.kills.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    /// 控制面 trait 的记录式实现。
    impl PtyProcess for FakePtyProcess {
        /// 返回进程 id。
        fn id(&self) -> u64 {
            self.id
        }

        /// 恒返回假 pid。
        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }

        /// 记录写入并返回 Ok。
        fn write(&self, data: &str) -> io::Result<()> {
            self.writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(data.to_string());
            Ok(())
        }

        /// 记录一次 resize。
        fn resize(&self, cols: u16, rows: u16) {
            self.resizes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((cols, rows));
        }

        /// 记录 SIGTERM 或 SIGKILL。
        fn kill(&self, force: bool) {
            self.kills
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(if force { "SIGKILL" } else { "SIGTERM" });
        }

        /// 复现 onExit 语义；fake 自身永不退出。
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

    /// 假 provider：按 spawn 顺序登记假进程。
    pub struct FakePtyProvider {
        /// 已 spawn 的假进程列表（下标即 spawn 顺序）。
        pub spawned: Mutex<Vec<Arc<FakePtyProcess>>>,
        /// When set, `spawn` fails for executables not listed here — used to
        /// exercise the multi-executable fallback.
        /// 中文补充：用于演练 auto 多可执行文件的逐个回退。
        pub fail_other_executables: Option<Vec<String>>,
        /// Artificial spawn latency (ms) so tests can hold a create in flight
        /// and observe the pending-create dedupe/conflict paths.
        /// 中文补充：把创建悬挂在 pending 窗口，观察去重与冲突路径。
        pub spawn_delay_ms: std::sync::atomic::AtomicU64,
    }

    /// 构造空的假 provider。
    impl FakePtyProvider {
        /// 新建 provider：记录为空、无白名单、无延迟。
        pub fn new() -> Self {
            Self {
                spawned: Mutex::new(Vec::new()),
                fail_other_executables: None,
                spawn_delay_ms: std::sync::atomic::AtomicU64::new(0),
            }
        }
    }

    /// 默认构造。
    impl Default for FakePtyProvider {
        /// 委托给 new。
        fn default() -> Self {
            Self::new()
        }
    }

    /// 记录式 spawn 实现。
    impl PtyProvider for FakePtyProvider {
        /// 按配置延迟/白名单过滤后创建假进程并登记，返回 fake-pty 后端。
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
