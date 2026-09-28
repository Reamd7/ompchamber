//! Port of `server/lib/event-stream/global-hub.js`.
//!
//! Shared upstream SSE hub: one engine `/event` reader fans events out to
//! server-side subscribers (watcher, client streams) while retaining a
//! bounded replay buffer keyed by SSE `eventId`. `replay_after` never serves
//! a silent suffix — an evicted or cross-boot cursor is a `Gap` verdict that
//! forces the client to resync. A generation token drops late events from
//! stopped readers, and an upstream epoch change clears the replay and
//! notifies `Restart` so fan-out consumers emit resync controls.
//!
//! 中文概要：全局事件 hub。单个引擎 /event 读取任务把事件归一化后 fan-out 给
//! 服务端订阅者，同时维护按 eventId 索引的有界 replay 环（条数 + 估算字节双
//! 上限）。replay_after 绝不返回“静默后缀”：游标被逐出或跨启动一律 Gap，
//! 逼迫客户端 resync。generation 令牌丢弃已停读取器的迟到消息；上游 epoch
//! 变化清空 replay 并广播 Restart，由消费者下发 resync 控制帧。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::engine::EngineState;
use crate::event_stream::protocol::{GLOBAL_DIRECTORY, SseEventEnvelope};
use crate::event_stream::upstream::{
    DEFAULT_UPSTREAM_MAX_BLOCK_BYTES, DEFAULT_UPSTREAM_RECONNECT_DELAY_MS,
    DEFAULT_UPSTREAM_STALL_TIMEOUT_MS, ReaderConfig, ReaderMessage, UpstreamError,
    UpstreamSseReader,
};

/// replay 环最多保留的事件条数。
pub const MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT: usize = 2048;
/// replay 环允许占用的估算序列化字节上限。
pub const MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 估算中每个容器节点的固定开销（字节）。
const ESTIMATE_NODE_OVERHEAD: u64 = 8;
/// 估算最多遍历的节点数；超过直接返回钳制值，防止病态结构拖慢热路径。
const ESTIMATE_MAX_NODES: u64 = 2048;
/// 单事件估算字节的钳制上限。
const ESTIMATE_CLAMP_BYTES: u64 = 32 * 1024 * 1024;
/// 逻辑头指针超过该值时物理 drain 前缀，摊还内存占用。
const REPLAY_COMPACT_THRESHOLD: usize = 1024;
/// Engine upstream path for the global stream.
///
/// Deviation from JS (global-hub.js reads `/global/event`): this port reads
/// `/event`, whose events carry their directory, and applies directory
/// scoping at fan-out (see PORT notes in mod.rs).
/// 目录过滤因此移到 fan-out 侧按事件自带的 directory 执行。
pub const UPSTREAM_GLOBAL_PATH: &str = "/event";

/// 上游 boot 元数据帧事件名：纯传输信息，不进入业务 fan-out。
const BOOT_EVENT_NAME: &str = "omp.stream.boot";
/// resync 控制帧事件名（crate 内共享给消费端与测试）。
pub(crate) const RESYNC_EVENT_NAME: &str = "omp.stream.resync";

/// `normalizeEvent` from global-hub.js.
/// 字段为 fan-out 与 replay 所需的最小归一形态。
#[derive(Debug, Clone)]
pub struct NormalizedEvent {
    /// 解析出的 SSE envelope；当前恒为 Some（保留 Option 仅为结构对齐）。
    pub envelope: Option<SseEventEnvelope>,
    /// 业务载荷 JSON。
    pub payload: Value,
    /// 事件目录；上游未携带时归一为 GLOBAL_DIRECTORY。
    pub directory: String,
    /// SSE 游标 id；无 id 的事件不进入 replay 环。
    pub event_id: Option<String>,
    /// SSE 事件名（识别控制帧用）。
    pub event_name: Option<String>,
}

/// Status fan-out mirroring the JS status objects.
/// 每个变体对应 JS hub 的一次状态回调。
#[derive(Debug, Clone)]
pub enum HubStatus {
    /// 连接建立成功；was_ready 区分首次连接与断线恢复。
    Connect {
        /// true 表示此前已就绪过（本次是恢复）。
        was_ready: bool,
    },
    /// 连接断开；reason 为断开原因的字符串形式。
    Disconnect {
        /// DisconnectReason 的字符串编码。
        reason: String,
    },
    /// Upstream rebooted or resynced: every client cursor is untrustworthy.
    /// 消费者收到后应向各自客户端下发 resync。
    Restart {
        /// 新的上游启动身份（可能未知）。
        epoch: Option<String>,
        /// 触发原因（"upstream-epoch-change" / "upstream-resync"）。
        reason: &'static str,
    },
    /// `initial: true` mirrors the JS `initial-error` (never-connected).
    /// JS 的 initial-error 语义。
    Error {
        /// true 表示从未连接成功过。
        initial: bool,
        /// 原始上游错误。
        error: UpstreamError,
    },
}

/// replay_after 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// 游标命中或无游标：返回的事件序列可安全续传。
    Ok,
    /// 游标已不可用（被逐出/清空/跨启动）：调用方必须让客户端 resync。
    Gap,
}

/// hub 观测统计（stats_snapshot 的数据源）。
#[derive(Debug, Clone, Default)]
struct HubStats {
    /// 被逐出（条数或字节上限）的条目数。
    evicted_entries: u64,
    /// 被逐出条目的估算字节数。
    evicted_bytes: u64,
    /// 处理过的 resync 控制帧数。
    resyncs: u64,
    /// 上游重启（epoch 变化）次数。
    restarts: u64,
}

/// replay 环中的单个条目。
struct ReplayEntry {
    /// 归一化事件副本（replay_after 直接返回它）。
    event: NormalizedEvent,
    /// 该事件的估算序列化字节数（容量核算单位）。
    bytes: u64,
}

/// 受锁保护的 hub 核心状态。
struct HubInner {
    /// 环形存储：[..replay_head] 为已逐出的死区，等待压缩回收。
    replay: Vec<ReplayEntry>,
    /// 最旧有效条目的下标。
    replay_head: usize,
    /// 有效条目的估算字节合计。
    replay_bytes: u64,
    /// 当前是否连接上游。
    connected: bool,
    /// 生命周期内是否成功连接过（Error 的 initial 判定）。
    ever_connected: bool,
    /// Bumped on every stop()/start(): late events from a stopped reader
    /// are dropped via the generation guard.
    /// handle_message 携带读取器启动时的代际，不匹配即丢弃。
    generation: u64,
    /// 最近学到的上游启动身份（x-omp-epoch）。
    upstream_epoch: Option<String>,
    /// (generation, epoch) a restart was already broadcast for: the same
    /// connect's resync control frame must not notify clients twice.
    /// 两者配对使用。
    restart_broadcast_gen: Option<u64>,
    /// 已广播过重启的那次连接所对应的 epoch。
    restart_broadcast_epoch: Option<String>,
    /// 观测统计。
    stats: HubStats,
}

/// Bounded conservative serialized-size estimate (UTF-16 units in JS; bytes
/// here) so a few huge tool-output events cannot pin unbounded memory.
/// 迭代式遍历（显式栈），键名与标量文本都计入；越限即返回钳制值。
fn estimate_payload_bytes(value: &Value) -> u64 {
    let mut total: u64 = 0;
    let mut nodes: u64 = 0;
    let mut stack: Vec<&Value> = vec![value];
    while let Some(current) = stack.pop() {
        nodes += 1;
        if nodes > ESTIMATE_MAX_NODES {
            return ESTIMATE_CLAMP_BYTES;
        }
        match current {
            Value::Null => total += 4,
            Value::Array(items) => {
                total += ESTIMATE_NODE_OVERHEAD;
                stack.extend(items.iter());
            }
            Value::Object(map) => {
                total += ESTIMATE_NODE_OVERHEAD;
                // Keys serialize too — a value-only walk undercounts wide
                // records.
                for (key, item) in map {
                    total += key.len() as u64;
                    stack.push(item);
                }
            }
            other => {
                total += 4 + other.to_string().len() as u64;
            }
        }
        if total > ESTIMATE_CLAMP_BYTES {
            return ESTIMATE_CLAMP_BYTES;
        }
    }
    total
}

/// 全局 hub：上游读取任务 + replay 缓冲 + 事件/状态双广播通道。
pub struct GlobalHub {
    /// 受锁核心状态。
    inner: Mutex<HubInner>,
    /// 事件 fan-out 通道（watcher 与客户端流订阅）。
    event_tx: broadcast::Sender<NormalizedEvent>,
    /// 状态 fan-out 通道。
    status_tx: broadcast::Sender<HubStatus>,
    /// 当前读取器句柄与其 pump 任务；None 表示已停止。
    reader: Mutex<Option<(Arc<UpstreamSseReader>, tokio::task::JoinHandle<()>)>>,
    /// 引擎状态：base_url / 鉴权头 / HTTP 客户端来源。
    engine: Arc<EngineState>,
    /// replay 条目上限。
    replay_limit: usize,
    /// replay 字节上限。
    replay_max_bytes: u64,
    /// 读取停滞超时。
    stall_timeout: Duration,
    /// 读取重连间隔。
    reconnect_delay: Duration,
}

/// hub 生命周期、订阅、replay 查询与消息处理。
impl GlobalHub {
    /// 以默认上限（2048 条 / 8 MiB）构造 hub。
    pub fn new(engine: Arc<EngineState>) -> Arc<Self> {
        Self::with_limits(
            engine,
            MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT,
            MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES,
        )
    }

    /// 以自定义 replay 上限构造（测试注入用）。
    pub(crate) fn with_limits(
        engine: Arc<EngineState>,
        replay_limit: usize,
        replay_max_bytes: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HubInner {
                replay: Vec::new(),
                replay_head: 0,
                replay_bytes: 0,
                connected: false,
                ever_connected: false,
                generation: 0,
                upstream_epoch: None,
                restart_broadcast_gen: None,
                restart_broadcast_epoch: None,
                stats: HubStats::default(),
            }),
            event_tx: broadcast::channel(1024).0,
            status_tx: broadcast::channel(256).0,
            reader: Mutex::new(None),
            engine,
            replay_limit,
            replay_max_bytes,
            stall_timeout: Duration::from_millis(DEFAULT_UPSTREAM_STALL_TIMEOUT_MS),
            reconnect_delay: Duration::from_millis(DEFAULT_UPSTREAM_RECONNECT_DELAY_MS),
        })
    }

    /// Start (or keep) the upstream reader. Resuming a retained ring: the
    /// fresh reader's cursor is the hub's replay tail + remembered epoch, so
    /// the upstream does not replay its whole ring into an already-populated
    /// buffer.
    /// 已有读取器在运行则直接返回（幂等）。
    pub fn start(self: &Arc<Self>) {
        let mut reader_slot = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if reader_slot.is_some() {
            return;
        }

        let (generation, initial_last_event_id, initial_epoch) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.generation += 1;
            let generation = inner.generation;
            let tail = retained_tail_event_id(&inner).unwrap_or_default();
            let epoch = inner.upstream_epoch.clone();
            (generation, tail, epoch)
        };

        let engine_for_url = Arc::clone(&self.engine);
        let engine_for_auth = Arc::clone(&self.engine);
        let config = ReaderConfig {
            build_url: Box::new(move || {
                engine_for_url
                    .base_url()
                    .map(|base| format!("{}{}", base.trim_end_matches('/'), UPSTREAM_GLOBAL_PATH))
            }),
            get_headers: Box::new(move || match engine_for_auth.auth_header() {
                Some(auth) => vec![("authorization".to_string(), auth)],
                None => Vec::new(),
            }),
            http: self.engine.http().clone(),
            stall_timeout: self.stall_timeout,
            reconnect_delay: self.reconnect_delay,
            max_block_bytes: DEFAULT_UPSTREAM_MAX_BLOCK_BYTES,
            initial_last_event_id,
            initial_epoch,
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ReaderMessage>();
        let reader = UpstreamSseReader::start(config, tx);
        let hub = Arc::clone(self);
        let pump = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                hub.handle_message(generation, message);
            }
        });
        *reader_slot = Some((reader, pump));
    }

    /// Stop the upstream reader; the retained replay survives so a
    /// reconnecting client still gets its suffix. Epoch safety lives on the
    /// next start's upstream epoch check.
    /// 连接状态复位，但 replay 与 epoch 记忆保留。
    pub fn stop(&self) {
        let stopped = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((reader, pump)) = stopped {
            reader.stop();
            pump.abort();
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.connected = false;
        inner.ever_connected = false;
    }

    /// 当前是否连接上游。
    pub fn is_connected(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connected
    }

    /// 生命周期内是否连接成功过。
    pub fn has_ever_connected(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ever_connected
    }

    /// 订阅事件 fan-out（慢订阅者只丢消息，不阻塞发布方）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<NormalizedEvent> {
        self.event_tx.subscribe()
    }

    /// 订阅状态 fan-out。
    pub fn subscribe_status(&self) -> broadcast::Receiver<HubStatus> {
        self.status_tx.subscribe()
    }

    /// Newest retained event id, or `None` when nothing live remains.
    /// 也用作下次 start 的断点续传游标。
    pub fn tail_event_id(&self) -> Option<String> {
        retained_tail_event_id(&self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Replay after `event_id`. `Gap` means the requested id is no longer
    /// retained (evicted, cleared, or from another boot): the caller must
    /// resync, never serve a silent suffix. Only a missing cursor (fresh
    /// client) is `Ok` with no events.
    /// 返回的事件按上游顺序排列，且只含带 id 的业务事件。
    pub fn replay_after(&self, event_id: Option<&str>) -> (ReplayVerdict, Vec<NormalizedEvent>) {
        let Some(event_id) = event_id.filter(|id| !id.is_empty()) else {
            return (ReplayVerdict::Ok, Vec::new());
        };
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let index = inner.replay[inner.replay_head..]
            .iter()
            .position(|entry| entry.event.event_id.as_deref() == Some(event_id))
            .map(|offset| inner.replay_head + offset);
        match index {
            None => (ReplayVerdict::Gap, Vec::new()),
            Some(index) => (
                ReplayVerdict::Ok,
                inner.replay[index + 1..]
                    .iter()
                    .map(|entry| entry.event.clone())
                    .collect(),
            ),
        }
    }

    /// 最近学到的上游启动身份。
    pub fn upstream_epoch(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upstream_epoch
            .clone()
    }

    /// Observation port mirroring `getStats()`.
    /// 键名与 JS getStats 对齐。
    pub fn stats_snapshot(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({
            "retainedEntries": inner.replay.len() - inner.replay_head,
            "retainedBytes": inner.replay_bytes,
            "replayLimit": self.replay_limit,
            "replayMaxBytes": self.replay_max_bytes,
            "evictedEntries": inner.stats.evicted_entries,
            "evictedBytes": inner.stats.evicted_bytes,
            "resyncs": inner.stats.resyncs,
            "restarts": inner.stats.restarts,
            "upstreamEpoch": inner.upstream_epoch,
            "eventSubscribers": self.event_tx.receiver_count(),
            "statusSubscribers": self.status_tx.receiver_count(),
        })
    }

    /// Test hook: bump the generation like `start()` without spawning a
    /// reader, so reader messages can be driven deterministically.
    /// 返回新代际供 handle_message 使用。
    #[cfg(test)]
    pub(crate) fn begin_generation_for_test(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.generation
    }

    /// Process one reader message under the generation guard (the JS
    /// closures capture `generation` and check it on every callback).
    /// 代际不匹配直接返回，其余按消息类型更新状态或广播。
    pub(crate) fn handle_message(&self, generation: u64, message: ReaderMessage) {
        if generation
            != self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .generation
        {
            return;
        }
        match message {
            ReaderMessage::Connect { last_event_id } => {
                tracing::debug!(last_event_id, "global message stream upstream connected");
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.connected = true;
                let was_ready = inner.ever_connected;
                inner.ever_connected = true;
                let status = HubStatus::Connect { was_ready };
                drop(inner);
                let _ = self.status_tx.send(status);
            }
            ReaderMessage::Disconnect { reason } => {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.connected = false;
                let status = HubStatus::Disconnect {
                    reason: reason.as_str().to_string(),
                };
                drop(inner);
                let _ = self.status_tx.send(status);
            }
            ReaderMessage::EpochChange { epoch, changed } => {
                let status = {
                    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    // Compare against the hub's remembered epoch, not just
                    // the reader's `changed` flag: after stop()/start() a
                    // fresh reader has no prior epoch, yet a rebooted
                    // upstream still makes every retained frame and client
                    // cursor untrustworthy.
                    let rebooted = changed
                        || (inner.upstream_epoch.is_some()
                            && inner.upstream_epoch.as_deref() != Some(epoch.as_str()));
                    inner.upstream_epoch = Some(epoch.clone());
                    if !rebooted {
                        None
                    } else {
                        inner.stats.restarts += 1;
                        clear_replay(&mut inner);
                        inner.restart_broadcast_gen = Some(generation);
                        inner.restart_broadcast_epoch = Some(epoch.clone());
                        Some(HubStatus::Restart {
                            epoch: Some(epoch),
                            reason: "upstream-epoch-change",
                        })
                    }
                };
                if let Some(status) = status {
                    let _ = self.status_tx.send(status);
                }
            }
            ReaderMessage::Error(error) => {
                let initial = !self
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ever_connected;
                let _ = self.status_tx.send(HubStatus::Error { initial, error });
            }
            ReaderMessage::Event(event) => {
                // Transport identity metadata, never a business event.
                if event.event_name.as_deref() == Some(BOOT_EVENT_NAME) {
                    return;
                }
                let Some(payload) = event.payload else {
                    // Upstream control frame (data-less): the retained
                    // replay cannot bridge any client cursor anymore.
                    // Controls never replay.
                    if event.event_name.as_deref() == Some(RESYNC_EVENT_NAME) {
                        let status = {
                            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                            inner.stats.resyncs += 1;
                            clear_replay(&mut inner);
                            // A connect-time resync is the host's verdict on
                            // the stale-epoch echo — the epoch-change
                            // handler already broadcast the restart for this
                            // (generation, epoch); sending another resync to
                            // every client would double the reconcile burst.
                            // Mid-stream synthesized resyncs (over-budget
                            // drops) and later verdicts still notify.
                            let duplicate_restart = !event.synthesized
                                && inner.restart_broadcast_gen == Some(generation)
                                && inner.upstream_epoch == inner.restart_broadcast_epoch;
                            if duplicate_restart {
                                None
                            } else {
                                Some(HubStatus::Restart {
                                    epoch: inner.upstream_epoch.clone(),
                                    reason: "upstream-resync",
                                })
                            }
                        };
                        if let Some(status) = status {
                            let _ = self.status_tx.send(status);
                        }
                    }
                    return;
                };

                let envelope = SseEventEnvelope {
                    event_id: event.event_id.clone(),
                    event_name: event.event_name.clone(),
                    directory: event.directory.clone(),
                    payload: Some(payload.clone()),
                    malformed: false,
                };
                let normalized = NormalizedEvent {
                    envelope: Some(envelope),
                    directory: event
                        .directory
                        .clone()
                        .unwrap_or_else(|| GLOBAL_DIRECTORY.to_string()),
                    event_id: event.event_id,
                    event_name: event.event_name,
                    payload,
                };
                {
                    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if normalized.event_id.is_some() {
                        push_replay(
                            &mut inner,
                            &normalized,
                            self.replay_limit,
                            self.replay_max_bytes,
                        );
                    }
                }
                let _ = self.event_tx.send(normalized);
            }
        }
    }
}

/// replay 环中最新的保留事件 id；无有效条目返回 None。
fn retained_tail_event_id(inner: &HubInner) -> Option<String> {
    (inner.replay.len() > inner.replay_head)
        .then(|| {
            inner
                .replay
                .last()
                .and_then(|entry| entry.event.event_id.clone())
        })
        .flatten()
}

/// 清空 replay 并把被清条目计入逐出统计。
fn clear_replay(inner: &mut HubInner) {
    let retained = inner.replay.len() - inner.replay_head;
    if retained > 0 {
        inner.stats.evicted_entries += retained as u64;
        inner.stats.evicted_bytes += inner.replay_bytes;
    }
    inner.replay.clear();
    inner.replay_head = 0;
    inner.replay_bytes = 0;
}
/// 追加一条事件并按条数/字节双上限从头部逐出；达到压缩阈值时物理回收死区。
fn push_replay(
    inner: &mut HubInner,
    normalized: &NormalizedEvent,
    replay_limit: usize,
    replay_max_bytes: u64,
) {
    let bytes = estimate_payload_bytes(&normalized.payload);
    inner.replay.push(ReplayEntry {
        event: normalized.clone(),
        bytes,
    });
    inner.replay_bytes += bytes;
    while inner.replay.len() - inner.replay_head > replay_limit
        || inner.replay_bytes > replay_max_bytes
    {
        let Some(oldest) = inner.replay.get(inner.replay_head) else {
            break;
        };
        let oldest_bytes = oldest.bytes;
        inner.replay_head += 1;
        inner.replay_bytes = inner.replay_bytes.saturating_sub(oldest_bytes);
        inner.stats.evicted_entries += 1;
        inner.stats.evicted_bytes += oldest_bytes;
    }
    if inner.replay_head >= REPLAY_COMPACT_THRESHOLD {
        inner.replay.drain(..inner.replay_head);
        inner.replay_head = 0;
    }
}

/// GlobalHub 状态机测试：replay 判定、epoch 重启与 fan-out 行为（不经真实上游）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::upstream::{DisconnectReason, UpstreamEvent};
    use serde_json::json;

    /// 构造指向不可达地址的 hub，仅驱动内部状态机。
    fn test_hub() -> Arc<GlobalHub> {
        let engine = EngineState::external("http://127.0.0.1:9".to_string(), None);
        GlobalHub::new(engine)
    }

    /// 构造一条带 id（可选目录）的业务事件消息。
    fn business_event(id: &str, directory: Option<&str>) -> ReaderMessage {
        ReaderMessage::Event(UpstreamEvent {
            event_id: Some(id.to_string()),
            event_name: None,
            directory: directory.map(str::to_string),
            payload: Some(json!({ "type": "session.updated", "properties": {} })),
            synthesized: false,
        })
    }

    /// 验证 replay_after 对新客户端返回 Ok 空、保留游标返回后缀、未知游标返回 Gap。
    #[test]
    fn replay_after_serves_suffix_and_reports_gap_on_eviction() {
        let hub = test_hub();
        let generation = hub.begin_generation_for_test();
        for id in ["e1", "e2", "e3"] {
            hub.handle_message(generation, business_event(id, Some("/tmp/p")));
        }

        // Fresh client: ok with no events.
        let (verdict, events) = hub.replay_after(None);
        assert_eq!(verdict, ReplayVerdict::Ok);
        assert!(events.is_empty());

        // Suffix after a retained id.
        let (verdict, events) = hub.replay_after(Some("e1"));
        assert_eq!(verdict, ReplayVerdict::Ok);
        assert_eq!(
            events
                .iter()
                .map(|e| e.event_id.clone())
                .collect::<Vec<_>>(),
            vec![Some("e2".to_string()), Some("e3".to_string())]
        );

        // Unknown / evicted id: gap, never a silent empty success.
        let (verdict, events) = hub.replay_after(Some("missing"));
        assert_eq!(verdict, ReplayVerdict::Gap);
        assert!(events.is_empty());
        assert_eq!(hub.tail_event_id().as_deref(), Some("e3"));
    }

    /// 验证条数上限逐出最旧条目后其游标变为 Gap，统计正确。
    #[test]
    fn replay_eviction_reports_gap_for_oldest_entries() {
        let engine = EngineState::external("http://127.0.0.1:9".to_string(), None);
        let hub = GlobalHub::with_limits(engine, 2, u64::MAX);
        let generation = hub.begin_generation_for_test();
        for id in ["e1", "e2", "e3"] {
            hub.handle_message(generation, business_event(id, None));
        }
        // e1 was evicted by the entry cap → gap, not an empty suffix.
        assert_eq!(hub.replay_after(Some("e1")).0, ReplayVerdict::Gap);
        assert_eq!(hub.replay_after(Some("e2")).0, ReplayVerdict::Ok);
        let stats = hub.stats_snapshot();
        assert_eq!(stats["retainedEntries"], json!(2));
        assert_eq!(stats["evictedEntries"], json!(1));
    }

    /// 验证 epoch 翻转清空 replay 且只广播一次 Restart；同连接 resync 被去重、合成 resync 仍通知。
    #[tokio::test]
    async fn epoch_change_clears_replay_and_broadcasts_restart_once() {
        let hub = test_hub();
        let mut statuses = hub.subscribe_status();
        let generation = hub.begin_generation_for_test();
        hub.handle_message(generation, business_event("e1", None));
        hub.handle_message(
            generation,
            ReaderMessage::Event(UpstreamEvent {
                event_id: Some("ctl-1".to_string()),
                event_name: Some(BOOT_EVENT_NAME.to_string()),
                directory: None,
                payload: Some(json!({ "boot": true })),
                synthesized: false,
            }),
        );
        // Boot frames are transport metadata: no event fan-out.
        let mut events = hub.subscribe_events();
        assert!(events.try_recv().is_err());

        // First epoch learn: not a restart (nothing remembered before).
        hub.handle_message(
            generation,
            ReaderMessage::EpochChange {
                epoch: "boot-1".into(),
                changed: false,
            },
        );
        // Epoch flip: restart + replay cleared.
        hub.handle_message(
            generation,
            ReaderMessage::EpochChange {
                epoch: "boot-2".into(),
                changed: true,
            },
        );
        assert_eq!(hub.replay_after(Some("e1")).0, ReplayVerdict::Gap);
        hub.handle_message(generation, business_event("e2", None));

        // Same-connect upstream resync control: duplicate restart suppressed.
        hub.handle_message(
            generation,
            ReaderMessage::Event(UpstreamEvent {
                event_id: Some("ctl-2".to_string()),
                event_name: Some(RESYNC_EVENT_NAME.to_string()),
                directory: None,
                payload: None,
                synthesized: false,
            }),
        );
        // Mid-stream synthesized resync still notifies.
        hub.handle_message(
            generation,
            ReaderMessage::Event(UpstreamEvent {
                event_id: Some("ctl-3".to_string()),
                event_name: Some(RESYNC_EVENT_NAME.to_string()),
                directory: None,
                payload: None,
                synthesized: true,
            }),
        );

        let mut restarts = Vec::new();
        while let Ok(status) = statuses.try_recv() {
            if let HubStatus::Restart { reason, .. } = status {
                restarts.push(reason);
            }
        }
        assert_eq!(
            restarts,
            vec!["upstream-epoch-change", "upstream-resync"],
            "connect-time resync after epoch change must not double-notify"
        );
        // JS only counts epoch-change restarts in `stats.restarts`; resync
        // controls bump `resyncs` (getStats parity).
        assert_eq!(hub.stats_snapshot()["restarts"], json!(1));
        assert_eq!(hub.stats_snapshot()["resyncs"], json!(2));
    }

    /// 验证 Connect 携带 was_ready、事件经通道 fan-out，stop() 后迟到消息被代际丢弃。
    #[tokio::test]
    async fn connect_status_carries_was_ready_and_events_fan_out() {
        let hub = test_hub();
        let mut statuses = hub.subscribe_status();
        let mut events = hub.subscribe_events();
        let generation = hub.begin_generation_for_test();

        hub.handle_message(
            generation,
            ReaderMessage::Connect {
                last_event_id: String::new(),
            },
        );
        assert!(hub.is_connected());
        assert!(hub.has_ever_connected());
        hub.handle_message(generation, business_event("e1", Some("/tmp/p")));
        hub.handle_message(
            generation,
            ReaderMessage::Disconnect {
                reason: DisconnectReason::Closed,
            },
        );
        assert!(!hub.is_connected());
        hub.handle_message(
            generation,
            ReaderMessage::Connect {
                last_event_id: "e1".into(),
            },
        );

        let received = events.recv().await.expect("event fan-out");
        assert_eq!(received.event_id.as_deref(), Some("e1"));
        assert_eq!(received.directory, "/tmp/p");

        let mut was_ready_flags = Vec::new();
        while let Ok(status) = statuses.try_recv() {
            if let HubStatus::Connect { was_ready } = status {
                was_ready_flags.push(was_ready);
            }
        }
        assert_eq!(was_ready_flags, vec![false, true]);

        // Events after stop() (generation bump) are dropped.
        hub.stop();
        hub.handle_message(generation, business_event("late", None));
        assert!(events.try_recv().is_err());
    }

    /// 验证无目录事件归一为 global，未学到 epoch 时统计中为 null。
    #[tokio::test]
    async fn events_without_directory_normalize_to_global() {
        let hub = test_hub();
        let mut events = hub.subscribe_events();
        let generation = hub.begin_generation_for_test();
        hub.handle_message(generation, business_event("e1", None));
        let received = events.recv().await.expect("event");
        assert_eq!(received.directory, GLOBAL_DIRECTORY);
        assert!(received.envelope.is_some());
        assert_eq!(
            hub.stats_snapshot()["upstreamEpoch"],
            serde_json::Value::Null
        );
    }
}
