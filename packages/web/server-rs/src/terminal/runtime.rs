//! Port of `server/lib/terminal/runtime.js` — terminal identity, PTY
//! processes, status, ordered output, bounded scrollback, WebSocket
//! attachments, flow control, viewport negotiation, and lifecycle routes.
//!
//! Structural mapping from the JS (single-threaded event loop) to tokio:
//! - `connections` + per-connection `attachments` maps → the same shapes
//!   behind `std::sync::Mutex`; publishes fan out through per-socket
//!   unbounded channels (the `socket.send` seam).
//! - The per-session FIFO event queue + drain slices → one pump task per
//!   session consuming an mpsc channel; stale events from replaced processes
//!   are dropped by process id (the JS `event.process` pointer check).
//! - Restart's drain-token invalidation → the same token guards the deferred
//!   grid drain task.
//! - Shared concurrent creates → a watch channel per pending id (JS stores
//!   the in-flight promise).
//!
//! 中文概述：终端运行时核心。维护会话（Session）、连接（Connection）
//! 与每连接 attachment 三层状态；驱动 PTY 进程的创建/重启/终止、输出
//! 事件泵与有界滚动回放；实现发送侧流控（lag 抑制 + ack 恢复 + 快照
//! 重同步）、视口尺寸协商（DRIVEN 强制所有权 / IDLE 最窄隐式所有者）、
//! 空闲会话回收，以及 v3 二进制 WebSocket 控制帧协议
//!（attach/write/ack/claimViewport 等）。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::inherited_env::{resolve_linux_pty_launch, strip_app_image_argv0_leak};
use crate::terminal::grid::GridCore;
use crate::terminal::history::{HistoryBuf, sanitize_terminal_history_chunk};
use crate::terminal::pty::{
    PtyEvent, PtyEventKind, PtyProcess, PtyProvider, PtySpawnRequest, SpawnedPty,
};
use crate::terminal::shell_integration::{
    Osc133Event, Osc133Scanner, build_bash_osc133_rc, build_zsh_osc133_wrapper,
};
use crate::terminal::shells::{Platform, ShellDeps, ShellResolver, get_terminal_shell_login_args};
use crate::terminal::theme::{Appearance, consume_terminal_theme_queries};
use crate::terminal::{MAX_INPUT_CHARS, protocol};

/// 并发终端会话上限；超出时报 Maximum terminal sessions reached（HTTP 429）。
pub const MAX_SESSIONS: usize = 20;
/// 发送侧流控：未确认字节积压超过该值（4 MiB）进入抑制，暂停直发。
pub const SEND_LAG_ENTER_BYTES: u64 = 4 * 1024 * 1024;
/// 发送侧流控：积压降回该值（1 MiB）以下才解除抑制并补发快照。
pub const SEND_LAG_EXIT_BYTES: u64 = 1024 * 1024;
/// 客户端从未 ack 过时的兜底抑制阈值（8 MiB），防只收不读的连接。
pub const SEND_LAG_FALLBACK_BYTES: u64 = 8 * 1024 * 1024;
/// 无 attachment 且无活动（写输入/claim）的会话空闲回收时限：30 分钟。
pub const IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// 默认终止宽限：SIGTERM 后 1 秒未退出升级为 SIGKILL。
pub const TERMINATION_GRACE_MS: u64 = 1000;
/// 空闲清扫任务的轮询间隔：5 分钟。
const IDLE_SWEEP_INTERVAL_MS: u64 = 5 * 60 * 1000;
/// shell 集成注入的临时目录延迟 60 秒清理，留出 shell 读取时间。
const SHELL_INTEGRATION_CLEANUP_MS: u64 = 60_000;
/// JS snapshot `runtime` field reports the hosting process (`'node'` |
/// `'bun'`); the Rust server keeps the node-classic value so the UI's
/// declared union (`'node' | 'bun'`) stays satisfied.
/// 中文：Rust 实现固定上报 node。
pub const RUNTIME_NAME: &str = "node";

/// 当前 Unix 时间戳（毫秒）；时钟早于 epoch 等异常情况返回 0。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 生成 v4 UUID 字符串：16 个随机字节手工置 version/variant 位后，
/// 格式化为 8-4-4-4-12 十六进制段。
pub fn random_uuid() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Lexical `path.resolve`: make absolute against the cwd and normalize `.`
/// and `..` without touching symlinks (the JS path module semantics the
/// runtime compares working directories with).
/// 中文：相对 std::fs::canonicalize 不解析符号链接；current_dir 获取
/// 失败时按输入原样处理。
pub fn resolve_path(input: &str) -> String {
    use std::path::{Component, Path};
    let path = Path::new(input);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let mut resolved = String::from("/");
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            resolved.push('/');
        }
        resolved.push_str(&part.to_string_lossy());
    }
    resolved
}

/// PTY 进程生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// 进程运行中，可接受 write 输入。
    Running,
    /// 进程已退出，保留 exitCode/signal 供快照查询。
    Exited,
}

/// SessionStatus 与协议字符串的编码映射。
impl SessionStatus {
    /// 输出快照/事件使用的 running/exited 字符串。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::Exited => "exited",
        }
    }
}

/// 终端主题模式：决定子进程 COLORFGBG 取值与主题变更上报内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    /// 浅色模式。
    Light,
    /// 深色模式（默认）。
    Dark,
}

/// ThemeMode 的 JS 字符串编解码。
impl ThemeMode {
    /// 按请求字符串解析：仅 light 命中浅色，其余（含缺失）一律按深色
    /// 处理，与 JS 三元写法一致。
    fn from_js(value: Option<&str>) -> ThemeMode {
        if value == Some("light") {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        }
    }

    /// 序列化为协议字符串 light/dark。
    fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

/// attachment 的输出订阅模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// 字节流：history 回放 + output 增量事件。
    Bytes,
    /// 网格：整屏 grid frame + 增量 grid 事件。
    Grid,
}

/// DRIVEN 模式下的视口驱动者：网格尺寸锁定为该连接上报的有效视口。
#[derive(Debug, Clone)]
pub struct ViewportDriver {
    /// 驱动者连接 id。
    pub connection_id: String,
    /// 驱动者声明的列数。
    pub cols: u16,
    /// 驱动者声明的行数。
    pub rows: u16,
}

/// 单个连接对单个会话的订阅状态与发送侧流控记账。
pub struct Attachment {
    /// attach 握手进行中：事件先入 pending 缓冲，快照发出后转实时投递。
    pub initializing: bool,
    /// 握手期间缓冲的事件，快照发出后按序补发。
    pub pending: Vec<Value>,
    /// 客户端上报的视口列数，0 表示尚未上报。
    pub cols: u16,
    /// 客户端上报的视口行数，0 表示尚未上报。
    pub rows: u16,
    /// 已发送未确认的（事件序号, 字节数）队列，lag 的来源。
    pub pending_acks: Vec<(u64, u64)>,
    /// 是否收到过至少一次 ack。
    pub acked_once: bool,
    /// 慢客户端抑制中：暂停直发，待 ack 降档后用快照重同步。
    pub suppressed: bool,
    /// 累计已发送字节（含快照），未 ack 连接的兜底抑制依据。
    pub sent_bytes: u64,
    /// 订阅模式：字节流或网格。
    pub feed: Feed,
}

/// Attachment 的构造与流控指标计算。
impl Attachment {
    /// 以指定 feed 构造：初始处于 initializing、零尺寸、零积压。
    fn new(feed: Feed) -> Self {
        Self {
            initializing: true,
            pending: Vec::new(),
            cols: 0,
            rows: 0,
            pending_acks: Vec::new(),
            acked_once: false,
            suppressed: false,
            sent_bytes: 0,
            feed,
        }
    }

    /// 当前未确认字节积压：pending_acks 中字节数之和。
    fn lag(&self) -> u64 {
        self.pending_acks.iter().map(|(_, bytes)| *bytes).sum()
    }
}

/// 一条终端 WebSocket 连接：出站消息通道与其全部 attachment。
pub struct Connection {
    /// 连接唯一标识，随 hello 帧下发给客户端。
    pub connection_id: String,
    /// 出站消息通道，由 socket writer 任务消费后写回 WebSocket。
    pub tx: mpsc::UnboundedSender<Message>,
    /// 本连接的 attachment 表：sessionId → Attachment。
    pub attachments: Mutex<HashMap<String, Attachment>>,
}

/// 会话全部可变状态，由 Session::inner 互斥锁保护。
pub struct SessionInner {
    /// 每会话事件序号：publish 时自增并写入帧的 q 字段。
    pub sequence: u64,
    /// 会话工作目录（spawn/restart 时设置）。
    pub cwd: String,
    /// 当前列数，随视口协商或 restart 变化。
    pub cols: u16,
    /// 当前行数，随视口协商或 restart 变化。
    pub rows: u16,
    /// 进程运行状态。
    pub status: SessionStatus,
    /// 退出码，进程退出后写入。
    pub exit_code: Option<i32>,
    /// 致死信号编号，进程被信号杀死时写入。
    pub signal: Option<i32>,
    /// PTY 后端标识，随快照上报。
    pub backend: String,
    /// 归一化后的 shell id。
    pub shell: String,
    /// 是否以 login shell 模式启动。
    pub login_shell: bool,
    /// 当前主题模式。
    pub theme_mode: ThemeMode,
    /// 客户端上报的终端背景色，用于应答 OSC 11 查询。
    pub terminal_background: Option<String>,
    /// 客户端上报的终端前景色，用于应答 OSC 10 查询。
    pub terminal_foreground: Option<String>,
    /// shell 是否已通过 CSI ? 2031 h 订阅主题模式；仅订阅且外观变化时
    /// 才向 PTY 写模式上报。
    pub theme_mode_enabled: bool,
    /// 是否应答 primary DA 探测；使用自带 conpty.dll 时关闭，避免应答
    /// 写入 shell 输入行。
    pub respond_primary_da: bool,
    /// 历史清洗器的跨 chunk 携带缓冲（转义序列截断时续拼）。
    pub pending_history: Vec<u8>,
    /// 主题查询扫描器的跨 chunk 携带缓冲（转义序列截断时续拼）。
    pub pending_theme: Vec<u8>,
    /// UTF-8 assembly carry so split codepoints never reach `output.d` as
    /// replacement characters (node-pty's StringDecoder behavior).
    /// 中文：等价 node-pty 的 StringDecoder 行为。
    pub pending_utf8: Vec<u8>,
    /// 有界滚动回放缓冲，attach 快照 history 字段的数据源。
    pub history: HistoryBuf,
    /// 最近一次客户端活动时间戳（写输入/创建/重启），空闲回收依据。
    pub last_activity: u64,
    /// 最近一次输出时间戳；输出不计入空闲判定，仅供观测。
    pub last_output_at: u64,
    /// 会话创建时间戳；同 id 复用会话对象时保留。
    pub created_at: u64,
    /// 客户端 claim 保活记录（claimant → 时间戳）；存在活跃 claim 的
    /// 会话在 DELETE 时存活，过期 claim 在释放时清理。
    pub claims: HashMap<String, u64>,
    /// 当前视口驱动者；Some 表示 DRIVEN 模式。
    pub viewport_driver: Option<ViewportDriver>,
    /// IDLE 模式下当前隐式所有者连接 id（最窄视口）。
    pub implicit_owner_id: Option<String>,
    /// 服务端网格状态机，grid feed 的数据源。
    pub grid: GridCore,
    /// 网格代际令牌：restart/重置时自增，使旧的延迟 drain 任务失效。
    pub grid_token: u64,
    /// 是否已有挂起的延迟 grid drain 任务（每会话至多一个）。
    pub grid_drain_pending: bool,
    /// 当前 PTY 进程句柄；退出后置 None，重启时被替换。
    pub process: Option<Arc<dyn PtyProcess>>,
    /// OSC 133 命令边界扫描器，产出 command-finished 事件。
    pub osc133: Osc133Scanner,
}

/// 一个终端会话：稳定 id、PTY 事件入口与受锁保护的可变状态。
pub struct Session {
    /// 会话唯一 id（客户端指定或随机生成）。
    pub id: String,
    /// PTY 事件发送端：进程输出/退出事件由此进入会话事件泵。
    pub event_tx: mpsc::UnboundedSender<PtyEvent>,
    /// 会话可变状态。
    pub inner: Mutex<SessionInner>,
}

/// Session 的构造与只读视图辅助。
impl Session {
    /// 创建会话对象与其事件 channel，返回（会话, 事件接收端）；初始
    /// 尺寸同时用于 GridCore。
    fn new(id: String, cols: u16, rows: u16) -> (Arc<Self>, mpsc::UnboundedReceiver<PtyEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let inner = SessionInner {
            sequence: 0,
            cwd: String::new(),
            cols,
            rows,
            status: SessionStatus::Running,
            exit_code: None,
            signal: None,
            backend: String::new(),
            shell: String::new(),
            login_shell: false,
            theme_mode: ThemeMode::Dark,
            terminal_background: None,
            terminal_foreground: None,
            theme_mode_enabled: false,
            respond_primary_da: true,
            pending_history: Vec::new(),
            pending_theme: Vec::new(),
            pending_utf8: Vec::new(),
            history: HistoryBuf::default(),
            last_activity: now_ms(),
            last_output_at: 0,
            created_at: now_ms(),
            claims: HashMap::new(),
            viewport_driver: None,
            implicit_owner_id: None,
            grid: GridCore::new(cols, rows),
            grid_token: 0,
            grid_drain_pending: false,
            process: None,
            osc133: Osc133Scanner::new(),
        };
        (
            Arc::new(Self {
                id,
                event_tx,
                inner: Mutex::new(inner),
            }),
            event_rx,
        )
    }

    /// 用 inner 当前值组装主题查询应答所需的 Appearance 快照。
    fn appearance(&self, inner: &SessionInner) -> Appearance {
        Appearance {
            theme_mode: inner.theme_mode.as_str().to_string(),
            terminal_background: inner.terminal_background.clone(),
            terminal_foreground: inner.terminal_foreground.clone(),
            mode_enabled: inner.theme_mode_enabled,
        }
    }
}

/// 终端运行时可调选项。
pub struct TerminalOptions {
    /// WebSocket 心跳 ping 间隔（毫秒）。
    pub heartbeat_interval_ms: u64,
    /// SIGTERM 后等待退出、再升级 SIGKILL 的宽限（毫秒）。
    pub termination_grace_ms: u64,
    /// JS skips shell-integration injection when the injected fs lacks sync
    /// writers (its test seam); Rust tests flip this flag instead.
    /// 中文：Rust 测试将该开关置 false 以跳过注入。
    pub shell_integration: bool,
}

/// 默认选项：心跳取 protocol 常量，宽限取 TERMINATION_GRACE_MS，默认
/// 启用 shell 集成。
impl Default for TerminalOptions {
    /// 构造默认值。
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: protocol::TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS,
            termination_grace_ms: TERMINATION_GRACE_MS,
            shell_integration: true,
        }
    }
}

/// 一个进行中的会话创建：记录创建参数，并持有 watch Sender 让并发
/// 同 id 请求等待并复用首个创建者的结果。
pub struct PendingCreate {
    /// 已解析的绝对工作目录；并发请求 cwd 不一致则拒绝。
    cwd: String,
    /// shell 选择；并发请求 shell 不一致则拒绝。
    shell: String,
    /// login shell 模式；并发请求不一致则拒绝。
    login_shell: bool,
    /// 结果通道：首个创建者发送 Some(Ok/Err) 唤醒所有等待者。
    tx: watch::Sender<Option<Result<Arc<Session>, String>>>,
}

/// 终端运行时全局状态：选项、PTY 工厂、shell 解析器，以及会话、并发
/// 创建、重启锁、连接四张表与停机标志。
pub struct TerminalState {
    /// 运行时选项（心跳、终止宽限、shell 集成开关）。
    pub opts: TerminalOptions,
    /// 平台 PTY 工厂（真实实现或测试桩）。
    pub provider: Arc<dyn PtyProvider>,
    /// shell 选择解析与增强 PATH 计算。
    pub shell_resolver: ShellResolver,
    /// 会话表：sessionId → Session。
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// 进行中的并发创建表：id → PendingCreate（跨 await 持有，用 tokio Mutex）。
    pub pending_creates: tokio::sync::Mutex<HashMap<String, PendingCreate>>,
    /// 每会话的 restart 串行锁表。
    pub restart_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 活跃连接表：connectionId → Connection。
    pub connections: Mutex<HashMap<String, Arc<Connection>>>,
    /// 停机标志：置位后空闲清扫任务退出。
    pub shutting_down: AtomicBool,
}

/// TerminalState 的构造、生命周期管理与查询辅助。
impl TerminalState {
    /// 构造全局状态并立即启动空闲清扫后台任务。
    pub fn new(
        opts: TerminalOptions,
        provider: Arc<dyn PtyProvider>,
        deps: ShellDeps,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            opts,
            provider,
            shell_resolver: ShellResolver::new(deps),
            sessions: Mutex::new(HashMap::new()),
            pending_creates: tokio::sync::Mutex::new(HashMap::new()),
            restart_locks: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        });
        state.spawn_idle_sweep();
        state
    }

    /// 启动周期空闲清扫后台任务；shutting_down 置位后退出循环。
    fn spawn_idle_sweep(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(IDLE_SWEEP_INTERVAL_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if state.shutting_down.load(Ordering::Acquire) {
                    return;
                }
                state.idle_sweep();
            }
        });
    }

    /// 找出无 attachment 且空闲超过 IDLE_TIMEOUT_MS 的会话，以
    /// IDLE_TIMEOUT 终止码在后台强制回收。
    fn idle_sweep(&self) {
        let now = now_ms();
        let reapable: Vec<String> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .iter()
                .filter(|(id, session)| {
                    let attached = self.is_attached(id);
                    let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                    !attached && now.saturating_sub(inner.last_activity) > IDLE_TIMEOUT_MS
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in reapable {
            if let Some(termination) = self.remove_session(
                &id,
                "IDLE_TIMEOUT",
                "Terminal expired after being idle",
                true,
            ) {
                tokio::spawn(termination);
            }
        }
    }

    /// 是否仍有任一连接 attach 着该会话。
    fn is_attached(&self, session_id: &str) -> bool {
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        connections.values().any(|connection| {
            connection
                .attachments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(session_id)
        })
    }

    /// Delete/force-kill/idle-reap path: evict the session, dispose the grid,
    /// send the fatal scoped closure, and hand the termination future to the
    /// caller — DELETE awaits it; idle sweep and force-kill run it in the
    /// background, exactly like the JS.
    /// 中文：返回的 Future 由调用方决定 await（DELETE）或后台执行（空闲
    /// 回收/强杀）；返回 None 表示会话本就不存在。
    pub fn remove_session(
        &self,
        session_id: &str,
        code: &str,
        message: &str,
        force: bool,
    ) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)?;
        {
            let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.grid = GridCore::new(inner.cols, inner.rows);
            inner.grid_token += 1;
            inner.grid_drain_pending = false;
        }
        close_attachments(self, session_id, code, message);
        session
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .process
            .clone()
            .map(|process| {
                let grace = self.opts.termination_grace_ms;
                Box::pin(terminate_process(process, force, grace))
                    as Pin<Box<dyn Future<Output = ()> + Send>>
            })
    }
    /// 取出或创建该会话的 restart 串行锁，避免同会话重启交错。
    fn restart_lock(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.restart_locks.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(locks.entry(session_id.to_string()).or_default())
    }

    /// `shutdown`: stop the idle sweep, force-terminate every session, drop
    /// every socket channel (writers end → sockets close), and mark the
    /// runtime down. Callers own wiring this into the process lifecycle.
    /// 中文：进程终止任务在后台执行，不阻塞调用方。
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let sessions: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.drain().map(|(_, session)| session).collect()
        };
        for session in sessions {
            if let Some(process) = session
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .process
                .clone()
            {
                let grace = self.opts.termination_grace_ms;
                tokio::spawn(async move {
                    terminate_process(process, true, grace).await;
                });
            }
        }
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

// ---------------------------------------------------------------------------
// Socket send seam
// ---------------------------------------------------------------------------

/// 打包并投递一帧控制消息到 socket writer 的 unbounded channel；
/// channel 已关闭（连接拆除中）返回 false。
fn send_value(tx: &mpsc::UnboundedSender<Message>, payload: &Value) -> bool {
    let frame = protocol::create_terminal_ws_control_frame(payload);
    tx.send(Message::Binary(frame.into())).is_ok()
}

/// send_value 的连接封装：写入该连接的出站通道。
fn send_to(connection: &Connection, payload: &Value) -> bool {
    send_value(&connection.tx, payload)
}

/// 对所有连接移除该会话的 attachment，并向受影响连接发送带 code/
/// message 的 fatal error 帧（如 IDLE_TIMEOUT、SESSION_NOT_FOUND）。
fn close_attachments(state: &TerminalState, session_id: &str, code: &str, message: &str) {
    let connections = state.connections.lock().unwrap_or_else(|e| e.into_inner());
    for connection in connections.values() {
        let removed = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
            .is_some();
        if removed {
            send_to(
                connection,
                &json!({"t": "error", "v": 3, "s": session_id, "code": code, "message": message, "fatal": true}),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Publish / snapshot / flow control
// ---------------------------------------------------------------------------

/// 是否存在任一以 grid feed 订阅该会话的 attachment（决定是否发布
/// grid 帧）。
fn has_grid_attachment(state: &TerminalState, session_id: &str) -> bool {
    let connections = state.connections.lock().unwrap_or_else(|e| e.into_inner());
    connections.values().any(|connection| {
        connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .is_some_and(|attachment| attachment.feed == Feed::Grid)
    })
}

/// 收集已上报有效视口尺寸的 attachment，返回（connectionId, cols, rows）
/// 列表，供 IDLE 模式的隐式所有者协商使用。
fn collect_viewport_sizes(state: &TerminalState, session_id: &str) -> Vec<(String, u16, u16)> {
    let connections = state.connections.lock().unwrap_or_else(|e| e.into_inner());
    let mut sizes = Vec::new();
    for connection in connections.values() {
        let attachments = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(attachment) = attachments.get(session_id)
            && attachment.cols > 0
            && attachment.rows > 0
        {
            sizes.push((
                connection.connection_id.clone(),
                attachment.cols,
                attachment.rows,
            ));
        }
    }
    sizes
}

/// 构造 attach 时的初始 snapshot 帧：字节 feed 携带 history 文本，
/// grid feed 携带整屏 frame；返回（帧, 计入流控的估算字节数）。
fn build_snapshot(session: &Session, inner: &mut SessionInner, grid: bool) -> (Value, u64) {
    let mut snapshot = json!({
        "t": "snapshot", "v": 3, "s": session.id, "q": inner.sequence,
        "history": if grid { String::new() } else { inner.history.text() },
        "status": inner.status.as_str(),
        "exitCode": inner.exit_code,
        "signal": inner.signal,
        "runtime": RUNTIME_NAME,
        "ptyBackend": inner.backend,
    });
    let mut bytes = 256u64;
    if grid {
        let frame = inner.grid.full_frame();
        bytes += serde_json::to_string(&frame)
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        snapshot["grid"] = frame;
    } else {
        bytes += snapshot["history"].as_str().map(str::len).unwrap_or(0) as u64;
    }
    (snapshot, bytes)
}

/// `publish`: advance the per-terminal sequence and fan the event out with
/// send-side flow control (output/grid frames only).
/// 中文：initializing 中的 attachment 先入 pending 队列等快照；
/// output/grid 帧计入 pending_acks 并按 SEND_LAG 阈值抑制慢端；
/// 其余事件（exit/resized/driverChanged 等）直发且不计流控。
pub(crate) fn publish(
    state: &TerminalState,
    session: &Session,
    inner: &mut SessionInner,
    event: Value,
) {
    inner.sequence += 1;
    let sequence = inner.sequence;
    let is_output = event.get("t").and_then(Value::as_str) == Some("output");
    let is_grid = event.get("t").and_then(Value::as_str) == Some("grid");
    let data_bytes = event
        .get("d")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0) as u64;
    let grid_bytes = if is_grid {
        serde_json::to_string(event.get("g").unwrap_or(&Value::Null))
            .map(|text| text.len() as u64)
            .unwrap_or(0)
    } else {
        0
    };
    let mut message = event;
    if let Some(object) = message.as_object_mut() {
        object.insert("v".into(), json!(3));
        object.insert("s".into(), json!(session.id));
        object.insert("q".into(), json!(sequence));
    }

    let connections = state.connections.lock().unwrap_or_else(|e| e.into_inner());
    for connection in connections.values() {
        let mut attachments = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(attachment) = attachments.get_mut(&session.id) else {
            continue;
        };
        if attachment.initializing {
            // Grid-fed attachments keep no byte replay, so buffered snapshots
            // and their first grid frame are all they need from the queue.
            if is_output && attachment.feed == Feed::Grid {
                continue;
            }
            attachment.pending.push(message.clone());
            continue;
        }
        if is_output && attachment.feed == Feed::Grid {
            continue;
        }
        if is_output || is_grid {
            if is_grid && attachment.feed != Feed::Grid {
                continue;
            }
            let suppress = attachment.suppressed
                || attachment.lag() > SEND_LAG_ENTER_BYTES
                || (!attachment.acked_once && attachment.sent_bytes > SEND_LAG_FALLBACK_BYTES);
            if suppress {
                // Sequence numbers keep advancing, so the next live frame this
                // attachment receives gap-triggers its own resync.
                attachment.suppressed = true;
                continue;
            }
            if !send_to(connection, &message) {
                continue;
            }
            let bytes = if is_grid { grid_bytes } else { data_bytes };
            attachment.sent_bytes += bytes;
            attachment.pending_acks.push((sequence, bytes));
        } else {
            send_to(connection, &message);
        }
    }
}

/// An attachment recovers (lag drained) by acknowledgment: prune the
/// acknowledged prefix, and if it was suppressed, resynchronize it with a
/// snapshot before live output resumes.
/// 中文：q 及之前的事件全部出队；仍被抑制但积压已降到
/// SEND_LAG_EXIT_BYTES 以下时解除抑制并补发快照。
fn apply_ack(state: &TerminalState, connection: &Arc<Connection>, session_id: &str, q: f64) {
    let needs_snapshot = {
        let mut attachments = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(attachment) = attachments.get_mut(session_id) else {
            return;
        };
        attachment.acked_once = true;
        let index = attachment
            .pending_acks
            .iter()
            .take_while(|(ack_q, _)| (*ack_q as f64) <= q)
            .count();
        attachment.pending_acks.drain(..index);
        if attachment.suppressed && attachment.lag() <= SEND_LAG_EXIT_BYTES {
            attachment.suppressed = false;
            true
        } else {
            false
        }
    };
    if needs_snapshot
        && let Some(session) = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    {
        send_snapshot_to(connection, &session);
    }
}

/// Send a snapshot to one attachment and account for its bytes like any other
/// frame, so a client that attaches and never reads is bounded too.
/// 中文：已 detach 或仍处于握手中的 attachment 直接跳过；快照字节数
/// 计入 sent_bytes 与 pending_acks，同样受流控约束。
fn send_snapshot_to(connection: &Arc<Connection>, session: &Arc<Session>) {
    let feed = {
        let attachments = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match attachments.get(&session.id) {
            None => return,
            Some(attachment) if attachment.initializing => return,
            Some(attachment) => attachment.feed,
        }
    };
    let (snapshot, bytes) = {
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        build_snapshot(session, &mut inner, feed == Feed::Grid)
    };
    if !send_to(connection, &snapshot) {
        return;
    }
    let mut attachments = connection
        .attachments
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(attachment) = attachments.get_mut(&session.id) {
        attachment.sent_bytes += bytes;
        attachment
            .pending_acks
            .push((snapshot["q"].as_u64().unwrap_or(0), bytes));
    }
}

// ---------------------------------------------------------------------------
// Viewport negotiation
// ---------------------------------------------------------------------------

/// 重算会话网格尺寸并按需 resize PTY 与 GridCore。DRIVEN 模式（有
/// viewport_driver）：网格锁定驱动者尺寸并跟随其变化；IDLE 模式：取
/// 所有已上报视口中最窄者为隐式所有者。尺寸变化分别发布
/// driverChanged / resized 事件。
pub(crate) fn recompute_grid(state: &Arc<TerminalState>, session: &Arc<Session>) {
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    // DRIVEN (forced ownership): the grid is locked to the claimer's effective
    // width and follows their container/zoom changes. No floor, no negotiation.
    if let Some(driver) = inner.viewport_driver.clone() {
        if driver.cols == inner.cols && driver.rows == inner.rows {
            return;
        }
        inner.cols = driver.cols;
        inner.rows = driver.rows;
        if inner.status == SessionStatus::Running {
            if let Some(process) = inner.process.clone() {
                process.resize(driver.cols, driver.rows);
            }
            inner.grid.resize(driver.cols, driver.rows);
        }
        publish(
            state,
            session,
            &mut inner,
            json!({"t": "driverChanged", "driverId": driver.connection_id, "cols": driver.cols, "rows": driver.rows}),
        );
        schedule_grid_drain(state, session, &mut inner);
        return;
    }
    // IDLE (implicit ownership): the grid is the pure minimum effective width
    // across attachments — no floor. The narrowest device IS the owner.
    let sizes = collect_viewport_sizes(state, &session.id);
    if sizes.is_empty() {
        return;
    }
    let mut owner = &sizes[0];
    for size in &sizes[1..] {
        if size.1 < owner.1 || (size.1 == owner.1 && size.2 < owner.2) {
            owner = size;
        }
    }
    let cols = owner.1;
    let rows = sizes.iter().map(|size| size.2).min().unwrap_or(owner.2);
    if cols == inner.cols
        && rows == inner.rows
        && inner.implicit_owner_id.as_deref() == Some(owner.0.as_str())
    {
        return;
    }
    inner.cols = cols;
    inner.rows = rows;
    inner.implicit_owner_id = Some(owner.0.clone());
    if inner.status == SessionStatus::Running
        && let Some(process) = inner.process.clone()
    {
        process.resize(cols, rows);
    }
    publish(
        state,
        session,
        &mut inner,
        json!({"t": "resized", "ownerId": owner.0, "cols": cols, "rows": rows}),
    );
}

/// 广播当前驱动状态：有驱动者时携带 driverId/cols/rows；无驱动者时
/// driverId 为 null 并回退到会话当前尺寸。
pub(crate) fn broadcast_driver_changed(state: &Arc<TerminalState>, session: &Arc<Session>) {
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    let event = match &inner.viewport_driver {
        Some(driver) => json!({
            "t": "driverChanged",
            "driverId": driver.connection_id,
            "cols": driver.cols,
            "rows": driver.rows,
        }),
        None => {
            json!({"t": "driverChanged", "driverId": null, "cols": inner.cols, "rows": inner.rows})
        }
    };
    publish(state, session, &mut inner, event);
}

// ---------------------------------------------------------------------------
// Session event pump
// ---------------------------------------------------------------------------

/// 启动会话事件泵任务：以 Weak 引用消费 PTY 事件，Output 交
/// process_output 处理，Exit 校验 process id 后更新状态并发布 exit
/// 事件；会话对象销毁时泵随之退出，被替换进程的陈旧事件按 id 丢弃。
fn spawn_session_pump(
    state: &Arc<TerminalState>,
    session: &Arc<Session>,
    mut events: mpsc::UnboundedReceiver<PtyEvent>,
) {
    let state = Arc::clone(state);
    let session: Weak<Session> = Arc::downgrade(session);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let Some(session) = session.upgrade() else {
                break;
            };
            match event.kind {
                PtyEventKind::Output(data) => {
                    process_output(&state, &session, event.process_id, &data);
                }
                PtyEventKind::Exit { exit_code, signal } => {
                    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if inner.process.as_ref().map(|p| p.id()) != Some(event.process_id) {
                        continue;
                    }
                    inner.status = SessionStatus::Exited;
                    inner.exit_code = exit_code;
                    inner.signal = signal;
                    inner.process = None;
                    publish(
                        &state,
                        &session,
                        &mut inner,
                        json!({"t": "exit", "exitCode": exit_code, "signal": signal}),
                    );
                }
            }
        }
    });
}

/// Split the longest valid UTF-8 prefix; the incomplete tail carries over.
/// Malformed bytes mid-stream are skipped (never buffered forever).
/// 中文：返回（有效前缀长度, 需携带到下个 chunk 的尾部字节数）。
fn split_utf8_prefix(input: &[u8]) -> (usize, usize) {
    match std::str::from_utf8(input) {
        Ok(_) => (input.len(), 0),
        Err(error) => {
            let valid = error.valid_up_to();
            match error.error_len() {
                // Hard error: drop the malformed byte(s), keep flowing.
                Some(bad_len) => (valid + bad_len, 0),
                // Truncated sequence at the end: carry into the next chunk.
                None => (valid, input.len() - valid),
            }
        }
    }
}

/// 消费一段 PTY 原始输出（仅当 process id 匹配当前进程）：应答主题/
/// 能力查询、清洗并追加滚动历史、组装跨 chunk 的 UTF-8 后发布 output
/// 事件（清洗改变内容时附 r 字段）、写入 GridCore 并调度 grid drain、
/// 扫描 OSC 133 命令边界发布 command-finished。
fn process_output(
    state: &Arc<TerminalState>,
    session: &Arc<Session>,
    process_id: u64,
    data: &[u8],
) {
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    if inner.process.as_ref().map(|p| p.id()) != Some(process_id) {
        return;
    }
    // Theme/capability queries are answered immediately, including queries
    // emitted before any WebSocket attachment exists.
    let theme = consume_terminal_theme_queries(
        &inner.pending_theme,
        data,
        &session.appearance(&inner),
        inner.respond_primary_da,
    );
    inner.pending_theme = theme.pending;
    inner.theme_mode_enabled = theme.mode_enabled;
    for response in &theme.responses {
        if let Some(process) = inner.process.clone() {
            let _ = process.write(response);
        }
    }
    let sanitized = sanitize_terminal_history_chunk(&inner.pending_history, data);
    inner.pending_history = sanitized.pending;
    inner.history.append(&sanitized.visible);
    // Output is not "activity" for lifetime purposes: an orphaned chatty
    // process must still become idle-reapable.
    inner.last_output_at = now_ms();

    // Assemble UTF-8 across chunks for the live `d` field.
    let mut assembled = std::mem::take(&mut inner.pending_utf8);
    assembled.extend_from_slice(data);
    let (valid_len, carry_len) = split_utf8_prefix(&assembled);
    let live = String::from_utf8_lossy(&assembled[..valid_len]).into_owned();
    inner.pending_utf8 = assembled[assembled.len() - carry_len..].to_vec();

    let differs = sanitized.visible != data;
    let mut event = json!({"t": "output", "d": live});
    if differs {
        event["r"] = Value::String(String::from_utf8_lossy(&sanitized.visible).into_owned());
    }
    publish(state, session, &mut inner, event);

    inner.grid.write(data);
    schedule_grid_drain(state, session, &mut inner);

    for osc_event in inner.osc133.scan(data) {
        if let Osc133Event::CommandFinished { exit_code } = osc_event {
            publish(
                state,
                session,
                &mut inner,
                json!({"t": "command-finished", "exitCode": exit_code}),
            );
        }
    }
}

/// Grid frames publish only while a grid attachment is watching — publish
/// itself advances the session sequence, and byte-feed clients must not see
/// sequence numbers drift for events they cannot observe. Like the JS
/// `setTimeout(0)` drain, the frame materializes after the current burst so
/// consecutive outputs coalesce into one frame.
/// 中文：同一会话同时只挂起一个 drain 任务；grid_token 不匹配说明网格
/// 已被 restart/重置换代，任务直接放弃，由新网格接管标志位。
pub(crate) fn schedule_grid_drain(
    state: &Arc<TerminalState>,
    session: &Arc<Session>,
    inner: &mut SessionInner,
) {
    if inner.grid_drain_pending {
        return;
    }
    inner.grid_drain_pending = true;
    let token = inner.grid_token;
    let state = Arc::clone(state);
    let session = Arc::clone(session);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(0)).await;
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.grid_token != token {
            // Restart/reset disposed this grid; its replacement owns the flag.
            return;
        }
        inner.grid_drain_pending = false;
        if inner.status == SessionStatus::Running && has_grid_attachment(&state, &session.id) {
            let frame = inner.grid.drain();
            publish(
                &state,
                &session,
                &mut inner,
                json!({"t": "grid", "g": frame}),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Process spawning
// ---------------------------------------------------------------------------

/// 一次成功 PTY spawn 的结果与元信息。
pub struct SpawnOutcome {
    /// 新启动的 PTY 进程句柄。
    pub process: Arc<dyn PtyProcess>,
    /// PTY 后端标识。
    pub backend: &'static str,
    /// 实际使用的归一化 shell id。
    pub shell_id: String,
    /// 是否以 login shell 模式启动。
    pub login_shell: bool,
    /// 是否使用随应用分发的 conpty.dll（决定 primary DA 应答开关）。
    pub conpty_dll: bool,
}

/// Full replacement environment for PTY children (`spawnPty` env assembly).
/// `NODE_CHANNEL_FD` is cleared (daemon IPC descriptors are host-private) and
/// AppImage `ARGV0` plus host-private shell vars are stripped.
/// 中文：同时覆写 PATH/TERM/COLORTERM，按主题设置 COLORFGBG，并清除
/// 宿主私有的 BASH_XTRACEFD/BASH_ENV/ENV/ELECTRON_RUN_AS_NODE。
pub fn build_child_env(
    parent: &HashMap<String, String>,
    augmented_path: &str,
    theme_mode: ThemeMode,
) -> HashMap<String, String> {
    let mut env = parent.clone();
    env.insert("PATH".to_string(), augmented_path.to_string());
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    env.insert(
        "COLORFGBG".to_string(),
        if theme_mode == ThemeMode::Light {
            "0;15"
        } else {
            "15;0"
        }
        .to_string(),
    );
    env.insert("NODE_CHANNEL_FD".to_string(), String::new());
    for key in ["BASH_XTRACEFD", "BASH_ENV", "ENV", "ELECTRON_RUN_AS_NODE"] {
        env.remove(key);
    }
    strip_app_image_argv0_leak(&mut env);
    env
}

/// 拷贝宿主进程全部环境变量（OsString 经 lossy 转 String），作为 PTY
/// 子进程环境的基底。
fn parent_process_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in std::env::vars_os() {
        env.insert(
            key.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        );
    }
    env
}

/// 在系统临时目录创建 前缀+随机 uuid 目录；名字碰撞最多重试 8 次，
/// 其他 IO 错误直接返回 None。
fn mktemp_dir(prefix: &str) -> Option<std::path::PathBuf> {
    for _ in 0..8 {
        let candidate = std::env::temp_dir().join(format!("{prefix}{}", random_uuid()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Some(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// 延迟 SHELL_INTEGRATION_CLEANUP_MS 后删除临时目录，给 shell 读取注入
/// 文件留出时间；删除失败静默忽略。
fn schedule_cleanup(dir: std::path::PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(SHELL_INTEGRATION_CLEANUP_MS)).await;
        let _ = std::fs::remove_dir_all(dir);
    });
}

/// OSC 133 shell-integration injection: emit command-boundary markers so the
/// runtime can publish command-finished events. zsh gets a temporary ZDOTDIR
/// whose `.zshenv` hands control back to the user's config immediately; bash
/// gets an `--rcfile` that chains into the user's bashrc. Injection is
/// best-effort: skipped on Windows, for login shells, when the runtime
/// disabled the seam, or when the user opts out.
/// Returns the (possibly rewritten) argv.
/// 中文：zsh 借临时 ZDOTDIR 的 .zshenv 注入，bash 改写为 --rcfile 启动，
/// 其余 shell 原样返回 argv；任一环节失败都降级为不注入，绝不阻断启动。
fn inject_shell_integration(
    opts: &TerminalOptions,
    shell_id: &str,
    args: &[String],
    env: &mut HashMap<String, String>,
    login_shell: bool,
) -> Vec<String> {
    if cfg!(windows) || login_shell || !opts.shell_integration {
        return args.to_vec();
    }
    if std::env::var("OMPCHAMBER_NO_SHELL_INTEGRATION")
        .ok()
        .as_deref()
        == Some("1")
    {
        return args.to_vec();
    }
    let home = env
        .get("HOME")
        .cloned()
        .unwrap_or_else(|| "/root".to_string());
    if shell_id == "zsh" && !env.contains_key("ZDOTDIR") {
        let Some(zdotdir) = mktemp_dir("oc-zdot-") else {
            return args.to_vec();
        };
        let zshenv = zdotdir.join(".zshenv");
        if std::fs::write(&zshenv, build_zsh_osc133_wrapper(&home)).is_err() {
            let _ = std::fs::remove_dir_all(&zdotdir);
            return args.to_vec();
        }
        env.insert(
            "ZDOTDIR".to_string(),
            zdotdir.to_string_lossy().into_owned(),
        );
        schedule_cleanup(zdotdir);
        return args.to_vec();
    }
    if shell_id == "bash" {
        let Some(dir) = mktemp_dir("oc-bash-") else {
            return args.to_vec();
        };
        let rcfile = dir.join("oc-bashrc");
        let user_bashrc = std::path::Path::new(&home).join(".bashrc");
        if std::fs::write(
            &rcfile,
            build_bash_osc133_rc(&user_bashrc.to_string_lossy()),
        )
        .is_err()
        {
            let _ = std::fs::remove_dir_all(&dir);
            return args.to_vec();
        }
        schedule_cleanup(dir);
        let mut rewritten = vec![
            "--rcfile".to_string(),
            rcfile.to_string_lossy().into_owned(),
            "-i".to_string(),
        ];
        rewritten.extend(args.iter().cloned());
        return rewritten;
    }
    args.to_vec()
}

/// 解析 shell 并逐个候选可执行文件尝试启动 PTY：组装 login 参数与子
/// 进程环境、注入 shell 集成、处理 Linux PTY 启动包装；任一候选成功
/// 即返回 SpawnOutcome，全部失败时返回最后一个错误文案。
fn spawn_pty(
    state: &TerminalState,
    event_tx: &mpsc::UnboundedSender<PtyEvent>,
    cwd: &str,
    cols: u16,
    rows: u16,
    theme_mode: ThemeMode,
    shell: Option<&str>,
    login_shell: bool,
) -> Result<SpawnOutcome, String> {
    let resolved = state
        .shell_resolver
        .resolve(shell)
        .map_err(|error| error.0)?;
    let platform = Platform::current();
    let augmented_path = state.shell_resolver.augmented_path();
    let parent = parent_process_env();
    let mut last_error: Option<String> = None;
    for executable in &resolved.executables {
        let args = if login_shell {
            match get_terminal_shell_login_args(executable, platform) {
                Some(args) => args,
                None => {
                    return Err(format!(
                        "Terminal shell \"{}\" does not support login mode",
                        resolved.id
                    ));
                }
            }
        } else {
            Vec::new()
        };
        let mut env = build_child_env(&parent, &augmented_path, theme_mode);
        let final_args =
            inject_shell_integration(&state.opts, &resolved.id, &args, &mut env, login_shell);
        let (launch_executable, launch_args) = resolve_linux_pty_launch(executable, &final_args);
        let request = PtySpawnRequest {
            cwd: cwd.to_string(),
            cols,
            rows,
            executable: launch_executable,
            args: launch_args,
            env,
            events: event_tx.clone(),
        };
        match state.provider.spawn(request) {
            Ok(SpawnedPty {
                process,
                backend,
                conpty_dll,
            }) => {
                return Ok(SpawnOutcome {
                    process,
                    backend,
                    shell_id: resolved.id.clone(),
                    login_shell,
                    conpty_dll,
                });
            }
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(last_error.unwrap_or_else(|| "No executable shell found".to_string()))
}

// ---------------------------------------------------------------------------
// Termination
// ---------------------------------------------------------------------------

/// `terminateProcess`: SIGTERM first, bounded SIGKILL escalation on grace
/// timeout, immediate SIGKILL when `force`.
/// 中文：force 为 true 直接强杀；否则先优雅终止，宽限（至少 1ms）内
/// 未退出再升级为强杀。
pub async fn terminate_process(process: Arc<dyn PtyProcess>, force: bool, grace_ms: u64) {
    if force {
        process.kill(true);
        return;
    }
    process.kill(false);
    tokio::select! {
        _ = process.wait_exit() => {}
        _ = tokio::time::sleep(Duration::from_millis(grace_ms.max(1))) => {
            process.kill(true);
        }
    }
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// 校验 cwd：trim 后非空且必须是已存在的目录；错误文案与 JS 实现一致。
async fn validate_cwd(cwd: &str) -> Result<(), String> {
    if cwd.trim().is_empty() {
        return Err("cwd is required".to_string());
    }
    match tokio::fs::metadata(cwd).await {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        _ => Err("Invalid working directory".to_string()),
    }
}

/// Apply an appearance update; write the mode report to the PTY when the
/// subscribed mode is enabled and something actually changed.
/// 中文：仅接受合法取值（themeMode 只认 light/dark）；无实际变化或
/// shell 未订阅主题模式时不写 PTY，避免污染输入行。
pub(crate) fn apply_appearance(inner: &mut SessionInner, body: &Value) {
    let previous = (
        inner.theme_mode,
        inner.terminal_background.clone(),
        inner.terminal_foreground.clone(),
    );
    if let Some(mode) = body.get("themeMode").and_then(Value::as_str)
        && (mode == "light" || mode == "dark")
    {
        inner.theme_mode = if mode == "light" {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
    }
    if let Some(background) = body.get("terminalBackground").and_then(Value::as_str) {
        inner.terminal_background = Some(background.to_string());
    }
    if let Some(foreground) = body.get("terminalForeground").and_then(Value::as_str) {
        inner.terminal_foreground = Some(foreground.to_string());
    }
    let changed = previous
        != (
            inner.theme_mode,
            inner.terminal_background.clone(),
            inner.terminal_foreground.clone(),
        );
    if changed
        && inner.theme_mode_enabled
        && let Some(process) = inner.process.clone()
    {
        let _ = process.write(&crate::terminal::theme::terminal_theme_mode_report(
            inner.theme_mode.as_str(),
        ));
    }
}

/// `startSession`: spawn the PTY and (re)initialize the session record.
/// `clear` resets replay/pending state (create + restart both clear).
/// 中文：成功时重置会话字段、重建 GridCore 并自增 grid_token（使旧
/// drain 失效）；使用自带 conpty.dll 时关闭 primary DA 应答。
async fn start_session(
    state: &Arc<TerminalState>,
    session: &Arc<Session>,
    cwd: &str,
    cols: u16,
    rows: u16,
    theme_mode: ThemeMode,
    terminal_background: Option<String>,
    terminal_foreground: Option<String>,
    shell: &str,
    login_shell: bool,
    clear: bool,
) -> Result<(), String> {
    validate_cwd(cwd).await?;
    let spawned = spawn_pty(
        state,
        &session.event_tx,
        cwd,
        cols,
        rows,
        theme_mode,
        Some(shell),
        login_shell,
    )?;
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    if clear {
        inner.history.reset();
        inner.pending_history.clear();
        inner.pending_theme.clear();
        inner.pending_utf8.clear();
        inner.theme_mode_enabled = false;
    }
    inner.cwd = cwd.to_string();
    inner.cols = cols;
    inner.rows = rows;
    inner.process = Some(Arc::clone(&spawned.process));
    inner.backend = spawned.backend.to_string();
    inner.shell = spawned.shell_id;
    inner.login_shell = spawned.login_shell;
    inner.status = SessionStatus::Running;
    inner.exit_code = None;
    inner.signal = None;
    inner.theme_mode = theme_mode;
    inner.terminal_background = terminal_background;
    inner.terminal_foreground = terminal_foreground;
    inner.last_activity = now_ms();
    inner.grid = GridCore::new(cols, rows);
    inner.grid_token += 1;
    inner.grid_drain_pending = false;
    inner.osc133.reset();
    // The bundled conpty.dll (and portable-pty's ConPTY) probes primary device
    // attributes during its own startup handshake; answering that probe writes
    // the response into the shell's input line.
    inner.respond_primary_da = !spawned.conpty_dll;
    Ok(())
}

/// JS field presence semantics: parameter defaults apply only to `undefined`;
/// `null` and other mismatched types keep their (invalid) value so the type
/// validators reject them with the exact JS messages.
/// 中文：Rust 侧用三态枚举区分字段缺失、合法、非法，精确复刻 JS
/// 默认参数只对 undefined 生效的行为。
pub enum JsField<T> {
    /// 字段缺失（JS undefined），参数默认值生效。
    Absent,
    /// 字段存在且类型匹配。
    Present(T),
    /// 字段存在但类型不匹配；保留原值交给校验器按 JS 文案拒绝。
    Invalid,
}

/// JsField 的取值辅助。
impl<T> JsField<T> {
    /// Absent 或 Invalid 时取 fallback，Present 取内部值。
    fn value_or(self, fallback: T) -> T {
        match self {
            JsField::Absent | JsField::Invalid => fallback,
            JsField::Present(value) => value,
        }
    }
}

/// createSession 请求体的解码结果；字段三态语义见 JsField。
pub struct CreateSessionRequest {
    /// 客户端指定的会话 id；缺省随机生成，超长（>128）拒绝。
    pub session_id: Option<String>,
    /// 工作目录，必填（空值报 cwd is required）。
    pub cwd: Option<String>,
    /// 列数；Absent 默认 80，上限 1000。
    pub cols: JsField<f64>,
    /// 行数；Absent 默认 24，上限 500。
    pub rows: JsField<f64>,
    /// 主题模式字符串；非 "light" 一律按 dark 处理。
    pub theme_mode: Option<String>,
    /// 客户端上报的终端背景色。
    pub terminal_background: Option<String>,
    /// 客户端上报的终端前景色。
    pub terminal_foreground: Option<String>,
    /// shell 选择；Absent 视为 auto，Present 值须能归一化。
    pub shell: JsField<String>,
    /// 是否以 login shell 启动。
    pub login_shell: JsField<bool>,
}

/// JS `validateSize`: `Number.isInteger(value) && value >= 1 && value <= max`.
/// `Invalid` (present non-number) and in-range failures both reject.
/// 中文：Absent 视为通过（默认 80x24）。
fn validate_size(value: &JsField<f64>, max: u16) -> bool {
    match value {
        JsField::Absent => true, // the defaulted size (80x24) always passes
        JsField::Invalid => false,
        JsField::Present(value) => value.fract() == 0.0 && *value >= 1.0 && *value <= max as f64,
    }
}

/// `createSession`. Errors carry the JS message text; the caller maps
/// `Maximum terminal sessions reached` to 429, everything else to 400.
/// 中文：同 id 运行中会话校验 cwd 一致后直接复用；并发同 id 创建经
/// watch channel 等待并复用首个创建者的结果；总量按 已建+在建 判定上限。
pub async fn create_session(
    state: &Arc<TerminalState>,
    request: CreateSessionRequest,
) -> Result<Arc<Session>, String> {
    if !validate_size(&request.cols, 1000) || !validate_size(&request.rows, 500) {
        return Err("Invalid terminal dimensions".to_string());
    }
    let appearance = appearance_body(&request);
    let terminal_background = request.terminal_background.clone();
    let terminal_foreground = request.terminal_foreground.clone();
    let cols = request.cols.value_or(80.0) as u16;
    let rows = request.rows.value_or(24.0) as u16;
    if matches!(request.login_shell, JsField::Invalid) {
        return Err("Invalid terminal login mode".to_string());
    }
    let login_shell = request.login_shell.value_or(false);
    let shell = match &request.shell {
        JsField::Absent => "auto".to_string(),
        JsField::Present(shell) => {
            match crate::terminal::shells::normalize_terminal_shell(Some(shell)) {
                Some(normalized) => normalized,
                None => return Err("Invalid terminal shell".to_string()),
            }
        }
        JsField::Invalid => return Err("Invalid terminal shell".to_string()),
    };
    let id = match request.session_id.as_deref() {
        Some(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => random_uuid(),
    };
    if id.len() > 128 {
        return Err("Invalid terminal session id".to_string());
    }

    let existing = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    let Some(cwd) = request.cwd.clone().filter(|cwd| !cwd.is_empty()) else {
        return Err("cwd is required".to_string());
    };
    let resolved_cwd = resolve_path(&cwd);

    if let Some(existing) = &existing
        && existing
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            == SessionStatus::Running
    {
        let existing_cwd = existing
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cwd
            .clone();
        if resolve_path(&existing_cwd) != resolved_cwd {
            return Err("Terminal session belongs to a different working directory".to_string());
        }
        let mut inner = existing.inner.lock().unwrap_or_else(|e| e.into_inner());
        apply_appearance(&mut inner, &appearance);
        drop(inner);
        return Ok(Arc::clone(existing));
    }

    let mut pending = state.pending_creates.lock().await;
    if let Some(entry) = pending.get(&id) {
        if entry.cwd != resolved_cwd {
            return Err("Terminal session belongs to a different working directory".to_string());
        }
        if entry.shell != shell {
            return Err(
                "Terminal session is already being created with a different shell".to_string(),
            );
        }
        if entry.login_shell != login_shell {
            return Err(
                "Terminal session is already being created with a different login mode".to_string(),
            );
        }
        let mut receiver = entry.tx.subscribe();
        drop(pending);
        let _ = receiver.changed().await;
        return match receiver.borrow_and_update().clone() {
            Some(Ok(session)) => {
                let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                apply_appearance(&mut inner, &appearance);
                drop(inner);
                Ok(session)
            }
            Some(Err(message)) => Err(message),
            None => Err("Terminal session creation failed".to_string()),
        };
    }
    if existing.is_none()
        && state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            + pending.len()
            >= MAX_SESSIONS
    {
        return Err("Maximum terminal sessions reached".to_string());
    }

    let (tx, _rx) = watch::channel(None);
    pending.insert(
        id.clone(),
        PendingCreate {
            cwd: resolved_cwd.clone(),
            shell: shell.clone(),
            login_shell,
            tx: tx.clone(),
        },
    );
    drop(pending);

    let theme_mode = ThemeMode::from_js(request.theme_mode.as_deref());
    let result = async {
        // Reuse the existing (exited) session object for the same identity —
        // sequence, claims, and createdAt survive, exactly like the JS.
        let session = match existing {
            Some(existing) => existing,
            None => {
                let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
                match sessions.get(&id).cloned() {
                    Some(existing) => existing,
                    None => {
                        drop(sessions);
                        let (session, events) = Session::new(id.clone(), cols, rows);
                        spawn_session_pump(state, &session, events);
                        session
                    }
                }
            }
        };
        start_session(
            state,
            &session,
            &cwd,
            cols,
            rows,
            theme_mode,
            terminal_background.clone(),
            terminal_foreground.clone(),
            &shell,
            login_shell,
            true,
        )
        .await?;
        state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), Arc::clone(&session));
        Ok(session)
    }
    .await;

    let mut pending = state.pending_creates.lock().await;
    if pending
        .get(&id)
        .is_some_and(|entry| entry.cwd == resolved_cwd && entry.shell == shell)
    {
        pending.remove(&id);
    }
    drop(pending);
    tx.send(Some(result.clone())).ok();
    let session = result?;
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    apply_appearance(&mut inner, &appearance);
    drop(inner);
    Ok(session)
}

/// 从创建请求提取 themeMode/terminalBackground/terminalForeground，
/// 组装成 apply_appearance 可直接消费的 JSON body。
fn appearance_body(request: &CreateSessionRequest) -> Value {
    json!({
        "themeMode": request.theme_mode,
        "terminalBackground": request.terminal_background,
        "terminalForeground": request.terminal_foreground,
    })
}

/// `POST /api/terminal/:sessionId/restart`: serialized per terminal, spawning
/// and wiring the replacement before terminating the old process.
/// 中文：各字段回退值在排队前按 JS 的 nullish 语义捕获；替换进程就绪
/// 后原子切换会话状态并重置回放/网格，旧进程后台优雅终止，最后广播
/// restarted 事件。
pub async fn restart_session(
    state: &Arc<TerminalState>,
    session: Arc<Session>,
    body: &Value,
) -> Result<(), String> {
    // Read request-time fallbacks before queueing behind prior restarts (the
    // JS evaluates `req.body?.x ?? session.x` when the request arrives). JS
    // `??` falls through for undefined AND null; other mismatched types keep
    // their value so the validators below reject them in the JS order
    // (cwd, dimensions, login mode).
    // JS ?? 语义的数值版本：undefined/null 回退，数字取值，其余记 NaN 交校验拒绝。
    let coalesce_number = |key: &str, fallback: f64| -> f64 {
        match body.get(key) {
            None | Some(Value::Null) => fallback,
            Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
            Some(_) => f64::NAN,
        }
    };
    let (cwd, cols, rows, theme_mode, background, foreground, shell, login_shell_raw) = {
        let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        let cwd = match body.get("cwd") {
            None | Some(Value::Null) => inner.cwd.clone(),
            Some(Value::String(cwd)) => cwd.clone(),
            Some(_) => String::new(), // validateCwd('cwd is required')
        };
        let cols = coalesce_number("cols", inner.cols as f64);
        let rows = coalesce_number("rows", inner.rows as f64);
        let theme_mode = match body.get("themeMode") {
            None | Some(Value::Null) => inner.theme_mode.as_str().to_string(),
            Some(Value::String(mode)) => mode.clone(),
            Some(_) => String::new(), // never 'light' -> dark, like the JS ternary
        };
        let background = body
            .get("terminalBackground")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| inner.terminal_background.clone());
        let foreground = body
            .get("terminalForeground")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| inner.terminal_foreground.clone());
        let shell = body
            .get("shell")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string();
        let login_shell_raw = body.get("loginShell").cloned();
        (
            cwd,
            cols,
            rows,
            theme_mode,
            background,
            foreground,
            shell,
            login_shell_raw,
        )
    };

    let restart_lock = state.restart_lock(&session.id);
    let _guard = restart_lock.lock().await;

    validate_cwd(&cwd).await?;
    if !validate_size(&JsField::Present(cols), 1000) || !validate_size(&JsField::Present(rows), 500)
    {
        return Err("Invalid terminal dimensions".to_string());
    }
    let cols = cols as u16;
    let rows = rows as u16;
    let login_shell = match login_shell_raw {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => value,
        Some(_) => return Err("Invalid terminal login mode".to_string()),
    };

    let spawned = spawn_pty(
        state,
        &session.event_tx,
        &cwd,
        cols,
        rows,
        ThemeMode::from_js(Some(&theme_mode)),
        Some(&shell),
        login_shell,
    )?;
    let old_process = {
        let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        let old_process = inner.process.clone();
        inner.process = Some(Arc::clone(&spawned.process));
        inner.backend = spawned.backend.to_string();
        inner.shell = spawned.shell_id.clone();
        inner.login_shell = spawned.login_shell;
        inner.cwd = cwd.clone();
        inner.cols = cols;
        inner.rows = rows;
        inner.history.reset();
        inner.pending_history.clear();
        inner.pending_theme.clear();
        inner.pending_utf8.clear();
        inner.theme_mode_enabled = false;
        inner.status = SessionStatus::Running;
        inner.exit_code = None;
        inner.signal = None;
        inner.grid = GridCore::new(cols, rows);
        inner.grid_token += 1;
        inner.grid_drain_pending = false;
        inner.osc133.reset();
        inner.respond_primary_da = !spawned.conpty_dll;
        inner.theme_mode = ThemeMode::from_js(Some(&theme_mode));
        inner.terminal_background = background;
        inner.terminal_foreground = foreground;
        old_process
    };
    if let Some(old_process) = old_process {
        let grace = state.opts.termination_grace_ms;
        tokio::spawn(async move {
            terminate_process(old_process, false, grace).await;
        });
    }
    let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
    publish(
        state,
        &session,
        &mut inner,
        json!({"t": "restarted", "history": ""}),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

/// 单条终端 WebSocket 连接的完整生命周期：注册 Connection 并下发
/// hello（含 connectionId）、按间隔发送心跳 ping，出站消息经 unbounded
/// channel 由独立 writer 任务写回 socket；文本帧回 BAD_FRAME 错误，
/// 二进制帧交 handle_ws_frame 处理，Close 或断开后中止 writer 并执行
/// cleanup_connection。
pub async fn run_socket(state: Arc<TerminalState>, socket: WebSocket) {
    let connection_id = random_uuid();
    let (tx, rx) = mpsc::unbounded_channel::<Message>();
    let connection = Arc::new(Connection {
        connection_id: connection_id.clone(),
        tx: tx.clone(),
        attachments: Mutex::new(HashMap::new()),
    });
    state
        .connections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(connection_id.clone(), Arc::clone(&connection));
    send_value(
        &tx,
        &json!({"t": "hello", "v": 3, "connectionId": connection_id}),
    );

    // Heartbeat (`socket.ping()` on an interval).
    {
        let tx = tx.clone();
        let interval = state.opts.heartbeat_interval_ms.max(1);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if tx.send(Message::Ping(Vec::new().into())).is_err() {
                    break;
                }
            }
        });
    }

    let (mut sink, mut stream) = socket.split();
    let mut writer = UnboundedReceiverStream::new(rx);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = writer.next().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(_) => {
                send_value(
                    &connection.tx,
                    &json!({"t": "error", "v": 3, "code": "BAD_FRAME", "message": "Binary control frame required", "fatal": false}),
                );
            }
            Message::Binary(bytes) => handle_ws_frame(&state, &connection, &bytes),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    writer_task.abort();
    cleanup_connection(&state, &connection);
}

/// 连接关闭后的清理：摘除该连接全部 attachment、从连接表注销，并对
/// 每个曾 attach 的会话释放驱动角色；随后兜底扫描所有会话，释放因
/// detach/claim 竞争遗留的驱动角色——从未驱动过的会话不触发重算与广播。
fn cleanup_connection(state: &Arc<TerminalState>, connection: &Arc<Connection>) {
    let attached: Vec<String> = {
        let mut attachments = connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        attachments
            .drain()
            .map(|(session_id, _)| session_id)
            .collect()
    };
    state
        .connections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&connection.connection_id);
    let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    for session_id in attached {
        if let Some(session) = sessions.get(&session_id) {
            release_driver_role(state, session, &connection.connection_id);
        }
    }
    // Safety net: release any driver role this connection still holds on
    // sessions it detached from earlier (detach already releases, but a raced
    // detach/claim ordering could leave the role behind). Sessions this
    // connection never drove are untouched — no recompute, no broadcast.
    for session in sessions.values() {
        let holds_driver = {
            let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            matches!(&inner.viewport_driver, Some(driver) if driver.connection_id == connection.connection_id)
        };
        if holds_driver {
            session
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .viewport_driver = None;
            recompute_grid(state, session);
            broadcast_driver_changed(state, session);
        }
    }
}
/// 释放指定连接在该会话上的 viewport 驱动角色：若确为当前驱动者，
/// 清空 viewport_driver、重算网格并广播 driverChanged；否则仅重算网格
///（隐式所有者集合可能已变化）。
fn release_driver_role(state: &Arc<TerminalState>, session: &Arc<Session>, connection_id: &str) {
    let was_driver = {
        let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
        matches!(&inner.viewport_driver, Some(driver) if driver.connection_id == connection_id)
    };
    if was_driver {
        session
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .viewport_driver = None;
        recompute_grid(state, session);
        broadcast_driver_changed(state, session);
    } else {
        recompute_grid(state, session);
    }
}

/// One inbound binary control frame (the `wsServer.on('message')` handler).
/// 中文：先校验帧格式与 v/t 字段（ping 回 pong、hello 忽略），再按 t
/// 分发 attach/viewport/claimViewport/releaseViewport/resync/ack/write/detach；
/// 未知会话回致命 SESSION_NOT_FOUND，非法输入回非致命错误帧。
fn handle_ws_frame(state: &Arc<TerminalState>, connection: &Arc<Connection>, raw: &[u8]) {
    let Some(message) = protocol::read_terminal_ws_control_frame(raw) else {
        send_to(
            connection,
            &json!({"t": "error", "v": 3, "code": "BAD_FRAME", "message": "Invalid terminal frame", "fatal": false}),
        );
        return;
    };
    if message.get("v") != Some(&json!(3)) || !message.get("t").and_then(Value::as_str).is_some() {
        send_to(
            connection,
            &json!({"t": "error", "v": 3, "code": "BAD_FRAME", "message": "Invalid terminal frame", "fatal": false}),
        );
        return;
    }
    let kind = message
        .get("t")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if kind == "ping" {
        send_to(connection, &json!({"t": "pong", "v": 3}));
        return;
    }
    if kind == "hello" {
        return;
    }
    let id = message
        .get("s")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        send_to(
            connection,
            &json!({"t": "error", "v": 3, "code": "BAD_FRAME", "message": "Session id required", "fatal": false}),
        );
        return;
    }
    if kind == "detach" {
        connection
            .attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let Some(session) = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
        {
            // A driver that detaches must release the role, otherwise the
            // ownership outlives the attachment and the close-time cleanup
            // (which walks attachments only) never releases it.
            release_driver_role(state, &session, &connection.connection_id);
        }
        return;
    }
    let Some(session) = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
    else {
        send_to(
            connection,
            &json!({"t": "error", "v": 3, "s": id, "code": "SESSION_NOT_FOUND", "message": "Terminal session not found", "fatal": true}),
        );
        return;
    };

    match kind.as_str() {
        "attach" => {
            let feed = if message.get("feed") == Some(&json!("grid")) {
                Feed::Grid
            } else {
                Feed::Bytes
            };
            let mut attachment = Attachment::new(feed);
            if let Some(cols) = message
                .get("cols")
                .and_then(Value::as_f64)
                .filter(|cols| *cols > 0.0)
            {
                attachment.cols = cols.min(1000.0) as u16;
            }
            if let Some(rows) = message
                .get("rows")
                .and_then(Value::as_f64)
                .filter(|rows| *rows > 0.0)
            {
                attachment.rows = rows.min(500.0) as u16;
            }
            connection
                .attachments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone(), attachment);
            // Register before capturing the snapshot, buffer concurrent
            // events, drop events represented by the snapshot sequence, then
            // enter live delivery.
            let (initial, bytes) = {
                let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                build_snapshot(&session, &mut inner, feed == Feed::Grid)
            };
            let sent_initial = send_to(connection, &initial);
            {
                let mut attachments = connection
                    .attachments
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(attachment) = attachments.get_mut(&id) {
                    for event in attachment.pending.drain(..) {
                        if event.get("q").and_then(Value::as_u64)
                            > initial.get("q").and_then(Value::as_u64)
                            && !(feed == Feed::Grid && event.get("t") == Some(&json!("output")))
                        {
                            send_to(connection, &event);
                        }
                    }
                    attachment.initializing = false;
                    if sent_initial {
                        attachment.sent_bytes += bytes;
                        attachment
                            .pending_acks
                            .push((initial["q"].as_u64().unwrap_or(0), bytes));
                    }
                }
            }
            // In DRIVEN mode, send the current driver info; don't recompute
            // (the PTY is driver-sized).
            let driver = session
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .viewport_driver
                .clone();
            match driver {
                Some(driver) => {
                    send_to(
                        connection,
                        &json!({"t": "driverChanged", "v": 3, "s": session.id, "driverId": driver.connection_id, "cols": driver.cols, "rows": driver.rows}),
                    );
                }
                None => recompute_grid(state, &session),
            }
        }
        "viewport" => {
            let mut updates: Option<(u16, u16)> = None;
            {
                let mut attachments = connection
                    .attachments
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(attachment) = attachments.get_mut(&id) {
                    if let Some(cols) = message
                        .get("cols")
                        .and_then(Value::as_f64)
                        .filter(|cols| *cols > 0.0)
                    {
                        attachment.cols = cols.min(1000.0) as u16;
                    }
                    if let Some(rows) = message
                        .get("rows")
                        .and_then(Value::as_f64)
                        .filter(|rows| *rows > 0.0)
                    {
                        attachment.rows = rows.min(500.0) as u16;
                    }
                    updates = Some((attachment.cols, attachment.rows));
                }
            }
            if let Some((cols, rows)) = updates {
                let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(driver) = inner.viewport_driver.clone()
                    && driver.connection_id == connection.connection_id
                {
                    inner.viewport_driver = Some(ViewportDriver {
                        cols,
                        rows,
                        ..driver
                    });
                }
            }
            recompute_grid(state, &session);
        }
        "claimViewport" => {
            // Only an attached client may drive the viewport; a claim from an
            // unattached (or detached) connection would orphan the driver role.
            if !connection
                .attachments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&id)
            {
                send_to(
                    connection,
                    &json!({"t": "error", "v": 3, "s": id, "code": "NOT_ATTACHED", "message": "Attach before claiming the viewport", "fatal": false}),
                );
                return;
            }
            let (session_cols, session_rows) = {
                let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                (inner.cols, inner.rows)
            };
            let clamp = |value: f64, max: u16| value.clamp(2.0, max as f64) as u16;
            let cols = message
                .get("cols")
                .and_then(Value::as_f64)
                .filter(|cols| cols.is_finite())
                .map(|cols| clamp(cols, 1000))
                .unwrap_or(session_cols);
            let rows = message
                .get("rows")
                .and_then(Value::as_f64)
                .filter(|rows| rows.is_finite())
                .map(|rows| clamp(rows, 500))
                .unwrap_or(session_rows);
            {
                let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.viewport_driver = Some(ViewportDriver {
                    connection_id: connection.connection_id.clone(),
                    cols,
                    rows,
                });
            }
            if cols == session_cols && rows == session_rows {
                broadcast_driver_changed(state, &session);
            } else {
                recompute_grid(state, &session);
            }
        }
        "releaseViewport" => {
            let was_driver = {
                let inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
                matches!(&inner.viewport_driver, Some(driver) if driver.connection_id == connection.connection_id)
            };
            if was_driver {
                session
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .viewport_driver = None;
                recompute_grid(state, &session);
                broadcast_driver_changed(state, &session);
            }
        }
        "resync" => {
            send_snapshot_to(connection, &session);
            let driver = session
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .viewport_driver
                .clone();
            if let Some(driver) = driver {
                send_to(
                    connection,
                    &json!({"t": "driverChanged", "v": 3, "s": session.id, "driverId": driver.connection_id, "cols": driver.cols, "rows": driver.rows}),
                );
            }
        }
        "ack" => {
            // Client-applied sequence for one session: {t:'ack', v:3, s, q}.
            if let Some(q) = message
                .get("q")
                .and_then(Value::as_f64)
                .filter(|q| q.is_finite())
            {
                apply_ack(state, connection, &id, q);
            }
        }
        "write" => {
            let Some(data) = message.get("d").and_then(Value::as_str) else {
                send_to(
                    connection,
                    &json!({"t": "error", "v": 3, "s": id, "code": "BAD_INPUT", "message": "Invalid terminal input", "fatal": false}),
                );
                return;
            };
            if data.is_empty() || data.chars().count() > MAX_INPUT_CHARS {
                send_to(
                    connection,
                    &json!({"t": "error", "v": 3, "s": id, "code": "BAD_INPUT", "message": "Invalid terminal input", "fatal": false}),
                );
                return;
            }
            let mut inner = session.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.status != SessionStatus::Running || inner.process.is_none() {
                send_to(
                    connection,
                    &json!({"t": "error", "v": 3, "s": id, "code": "NOT_RUNNING", "message": "Terminal is not running", "fatal": false}),
                );
                return;
            }
            let write_result = inner
                .process
                .as_ref()
                .map(|process| process.write(data))
                .unwrap_or_else(|| Err(std::io::Error::other("no process")));
            match write_result {
                Ok(()) => inner.last_activity = now_ms(),
                Err(_) => {
                    send_to(
                        connection,
                        &json!({"t": "error", "v": 3, "s": id, "code": "WRITE_FAILED", "message": "Failed to write to terminal", "fatal": false}),
                    );
                }
            }
        }
        _ => {}
    }
}
