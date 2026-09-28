//! Port of `server/lib/event-stream/upstream-reader.js`.
//!
//! Reusable upstream SSE reader: parses SSE blocks, tracks the latest
//! `Last-Event-ID`, reconnects after closed/stalled streams, learns the
//! upstream boot identity from `x-omp-epoch`, and reports through an mpsc
//! channel (the JS callbacks become message variants). Failure honesty is
//! load-bearing: malformed blocks never advance the cursor, over-budget
//! blocks are disavowed through a synthesized resync instead of being
//! dropped silently, and fetch failures surface as errors — never as an
//! authoritative empty success.
//!
//! 中文概要：可复用的上游 SSE 读取器。带 Last-Event-ID / x-omp-epoch 头发起
//! GET，按 "\n\n" 切块解析并推进游标；流关闭、停滞超时或出错后按退避间隔自动
//! 重连。JS 回调在此建模为 mpsc 消息（ReaderMessage）。错误语义必须如实：
//! 畸形块不推进游标、超预算块通过合成 resync 显式否认、fetch 失败以 Error
//! 上报，绝不伪装成空成功。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::event_stream::protocol::parse_sse_event_envelope;

/// 默认停滞超时：20 秒没有新字节即中断当前连接并重连。
pub const DEFAULT_UPSTREAM_STALL_TIMEOUT_MS: u64 = 20_000;
/// 默认重连退避间隔（250ms）。
pub const DEFAULT_UPSTREAM_RECONNECT_DELAY_MS: u64 = 250;
/// Parser guard (docs/plan.md §5.3.1): must sit above the largest event the
/// host's replay ring can retain so an oversized-but-retained event cannot
/// reconnect-loop this reader forever.
/// 默认 16 MiB：保护内存的同时不误伤 replay ring 可保留的事件。
pub const DEFAULT_UPSTREAM_MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;
/// Header the omp host echoes for boot-identity resume (plan §5.2.1).
/// 断点重连时随请求回显，供上游区分同次启动续传与跨启动的陈旧游标。
pub const UPSTREAM_EPOCH_HEADER: &str = "x-omp-epoch";
/// 本地合成 resync 控制帧使用的事件名（超预算块的否认路径）。
const RESYNC_EVENT_NAME: &str = "omp.stream.resync";

/// 一次连接结束的原因（JS 字符串状态的可枚举对应物）。
#[derive(Debug, Clone, PartialEq)]
pub enum DisconnectReason {
    /// 停滞超时：读取器主动中断以触发重连。
    UpstreamStalled,
    /// 调用方主动 stop。
    Stopped,
    /// 上游正常关闭了流。
    Closed,
}

/// 断开原因的 wire 字符串编码。
impl DisconnectReason {
    /// 返回与 JS 一致的字符串（"upstream_stalled" / "stopped" / "closed"）。
    pub fn as_str(&self) -> &'static str {
        match self {
            DisconnectReason::UpstreamStalled => "upstream_stalled",
            DisconnectReason::Stopped => "stopped",
            DisconnectReason::Closed => "closed",
        }
    }
}

/// One parsed (or synthesized) upstream event. `payload: None` marks a
/// data-less control frame.
/// 除 payload 外各字段均可为 None（SSE 块未提供对应行）。
#[derive(Debug, Clone)]
pub struct UpstreamEvent {
    /// `id:` 行内容；重连时作为 Last-Event-ID 回传的游标。
    pub event_id: Option<String>,
    /// `event:` 行内容；resync/boot 等控制帧靠它识别。
    pub event_name: Option<String>,
    /// 事件携带的目录；缺省时消费方回退 global。
    pub directory: Option<String>,
    /// 解析后的 data JSON；None 表示无 data 的控制帧。
    pub payload: Option<serde_json::Value>,
    /// Synthesized locally (over-budget drop disavowal), not an upstream
    /// frame — consumers must not dedupe it against connect-time restarts.
    /// hub 依据它避免与连接期重启的双重通知。
    pub synthesized: bool,
}

/// 上报给消费者的失败信息。
#[derive(Debug, Clone)]
pub enum UpstreamError {
    /// HTTP 响应非 2xx：上游（引擎）当前不可用。
    UpstreamUnavailable {
        /// 响应状态码。
        status: u16,
    },
    /// 传输/读取阶段的失败。
    StreamError {
        /// 错误文本。
        message: String,
        /// true 表示失败发生在构造 URL 时（引擎不可用），而非网络传输。
        build_url_failed: bool,
    },
}

/// 错误的展示文本构造。
impl UpstreamError {
    /// 生成人类可读的错误消息（日志与状态上报用）。
    pub fn message(&self) -> String {
        match self {
            UpstreamError::UpstreamUnavailable { status } => {
                format!("upstream unavailable ({status})")
            }
            UpstreamError::StreamError { message, .. } => message.clone(),
        }
    }
}

/// The JS callbacks (`onEvent`, `onConnect`, …) as channel messages.
/// 生命周期顺序：Connect → 若干 Event/Error/EpochChange → Disconnect。
#[derive(Debug, Clone)]
pub enum ReaderMessage {
    /// 连接建立成功；携带当次请求所用的游标。
    Connect { last_event_id: String },
    /// 一次连接结束（停滞、停止或正常关闭）。
    Disconnect { reason: DisconnectReason },
    /// 一条解析出的事件。
    Event(UpstreamEvent),
    /// 一次失败（URL 构造失败、传输错误或非 2xx）。
    Error(UpstreamError),
    /// 学到/变化的上游启动身份；changed 表示与读取器上次所知不同。
    EpochChange { epoch: String, changed: bool },
}

/// 读取器自省统计（对应 JS 的 getStats）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReaderStats {
    /// 被丢弃的块数（畸形 + 超预算）。
    pub dropped_blocks: u64,
    /// 被丢弃的累计字节数。
    pub dropped_bytes: u64,
    /// 观测到的上游重启（epoch 变化）次数。
    pub epoch_changes: u64,
}

/// 读取器配置：回调、HTTP 客户端与超时/上限参数。
pub struct ReaderConfig {
    /// `None` mirrors the JS `buildUrl()` throw ("OpenCode service
    /// unavailable"): reported as a stream error with `build_url_failed`.
    /// 每次连接尝试前调用。
    pub build_url: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// 每次连接前构造额外请求头（如 authorization）。
    pub get_headers: Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    /// 复用的 reqwest 客户端（连接池与超时配置共享）。
    pub http: reqwest::Client,
    /// 停滞超时：超过该时长没有新字节即判定停滞。
    pub stall_timeout: Duration,
    /// 两次连接尝试之间的退避间隔。
    pub reconnect_delay: Duration,
    /// 单个 SSE 块的字节上限（见 DEFAULT_UPSTREAM_MAX_BLOCK_BYTES）。
    pub max_block_bytes: usize,
    /// 初始游标（断点续传起点，可为空）。
    pub initial_last_event_id: String,
    /// 初始已知的上游启动身份（可为 None）。
    pub initial_epoch: Option<String>,
}

/// 读取任务与控制句柄共享的可变状态。
#[derive(Default)]
struct ReaderShared {
    /// 当前游标（最近已处理事件的 id，重连时回传）。
    last_event_id: Mutex<String>,
    /// 最近学到的上游启动身份。
    epoch: Mutex<Option<String>>,
    /// 丢弃与重启计数。
    stats: Mutex<ReaderStats>,
}

/// 锁保护访问器；锁中毒时直接恢复数据，读取器不因 panic 永久卡死。
impl ReaderShared {
    /// 读取游标副本。
    fn last_event_id(&self) -> String {
        self.last_event_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// 覆写游标。
    fn set_last_event_id(&self, id: String) {
        *self.last_event_id.lock().unwrap_or_else(|e| e.into_inner()) = id;
    }
    /// 读取当前上游身份。
    fn epoch(&self) -> Option<String> {
        self.epoch.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// 记一次块丢弃（块数 +1、字节累加）。
    fn record_drop(&self, bytes: usize) {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.dropped_blocks += 1;
        stats.dropped_bytes += bytes as u64;
    }
}

/// Stop handle + introspection for one spawned reader task. The accessors
/// mirror the JS reader API (`getLastEventId`/`getEpoch`/`getStats`) for the
/// bridges and tests; the hub itself drives the cursor through replay state.
/// 生命周期方法可在任务运行期间随时调用；重复 stop 幂等。
#[allow(dead_code)]
pub struct UpstreamSseReader {
    /// 与任务共享的游标、身份与统计。
    shared: Arc<ReaderShared>,
    /// 停止信号发送端（watch 值变为 false 即请求停止）。
    stop_tx: watch::Sender<bool>,
    /// 运行任务的 JoinHandle；join 时取出并等待。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Error sentinel to unwind an attempt without losing the JS semantics
/// (JS throws inside the read loop and lands in the catch → stream_error).
/// run 循环把它映射为一条 StreamError 消息。
struct AttemptAbort {
    /// 错误文本；"OpenCode service unavailable" 专门标记 URL 构造失败。
    message: String,
}

/// 读取器的生命周期与自省 API。
#[allow(dead_code)]
impl UpstreamSseReader {
    /// 启动读取任务：以配置初始化共享游标/身份/统计，建立停止通道并 spawn run 循环。
    pub fn start(
        config: ReaderConfig,
        outgoing: mpsc::UnboundedSender<ReaderMessage>,
    ) -> Arc<Self> {
        let (stop_tx, stop_rx) = watch::channel(true);
        let shared = Arc::new(ReaderShared {
            last_event_id: Mutex::new(config.initial_last_event_id.clone()),
            epoch: Mutex::new(config.initial_epoch.clone()),
            stats: Mutex::new(ReaderStats::default()),
        });
        let config = Arc::new(config);
        let reader = Arc::new(Self {
            stop_tx,
            task: Mutex::new(None),
            shared: Arc::clone(&shared),
        });
        let task = tokio::spawn(run(config, shared, stop_rx, outgoing));
        *reader.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        reader
    }

    /// Cooperative stop; the run loop exits and flushes a final Disconnect.
    /// 幂等；重复调用无副作用。
    pub fn stop(&self) {
        let _ = self.stop_tx.send(false);
    }

    /// Await task exit (tests and deterministic shutdown).
    /// 取出 JoinHandle 等待完成；已 join 过则为空操作。
    pub async fn join(&self) {
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// 当前游标（getLastEventId 对应物）。
    pub fn last_event_id(&self) -> String {
        self.shared.last_event_id()
    }

    /// Last upstream boot identity learned from `x-omp-epoch`, or `None`.
    /// 由上游 x-omp-epoch 响应头学习而来。
    pub fn epoch(&self) -> Option<String> {
        self.shared.epoch()
    }

    /// 统计快照（getStats 对应物）。
    pub fn stats(&self) -> ReaderStats {
        *self.shared.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 主循环：连接 → 流式读取 → 上报 Disconnect → 退避重连，直至收到停止信号。
async fn run(
    config: Arc<ReaderConfig>,
    shared: Arc<ReaderShared>,
    mut stop_rx: watch::Receiver<bool>,
    outgoing: mpsc::UnboundedSender<ReaderMessage>,
) {
    /// 停止信号是否已触发。
    fn stopped(stop_rx: &watch::Receiver<bool>) -> bool {
        !*stop_rx.borrow()
    }
    /// 发送一条消息；接收端已关闭时静默忽略。
    async fn send(outgoing: &mpsc::UnboundedSender<ReaderMessage>, msg: ReaderMessage) {
        let _ = outgoing.send(msg);
    }
    /// 等待重连退避；可被停止信号立即打断以尽快退出。
    async fn reconnect_delay(delay: Duration, stop_rx: &mut watch::Receiver<bool>) {
        if delay.is_zero() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop_rx.changed() => {}
        }
    }

    while !stopped(&stop_rx) {
        let mut abort_reason: Option<DisconnectReason> = None;

        // One connect attempt. Any early exit still owes a Disconnect frame
        // (the JS `finally` block always emits one).
        let attempt: Result<(), AttemptAbort> = async {
            let url = match (config.build_url)() {
                Some(url) => url,
                None => {
                    return Err(AttemptAbort {
                        message: "OpenCode service unavailable".to_string(),
                    });
                }
            };

            let mut request = config
                .http
                .get(&url)
                .header("accept", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive");
            for (name, value) in (config.get_headers)() {
                request = request.header(name, value);
            }
            let last_event_id = shared.last_event_id();
            if !last_event_id.is_empty() {
                request = request.header("Last-Event-ID", &last_event_id);
            }
            if let Some(epoch) = shared.epoch() {
                // Boot-identity echo: lets the upstream distinguish a
                // same-boot resume from a stale cross-boot cursor.
                request = request.header(UPSTREAM_EPOCH_HEADER, epoch);
            }

            let response = request.send().await.map_err(|error| AttemptAbort {
                message: error.to_string(),
            })?;

            if !response.status().is_success() {
                send(
                    &outgoing,
                    ReaderMessage::Error(UpstreamError::UpstreamUnavailable {
                        status: response.status().as_u16(),
                    }),
                )
                .await;
                return Ok(());
            }

            // Boot identity: header before Connect, and a changed epoch
            // invalidates the cursor (it belongs to the boot that issued it).
            if let Some(epoch) = response
                .headers()
                .get(UPSTREAM_EPOCH_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                // Compute under the lock, send after dropping the guard (the
                // run future must stay Send).
                let epoch_change = {
                    let mut guard = shared.epoch.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.as_deref() == Some(epoch) {
                        None
                    } else {
                        let changed = guard.is_some();
                        if changed {
                            shared
                                .stats
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .epoch_changes += 1;
                            shared.set_last_event_id(String::new());
                        }
                        *guard = Some(epoch.to_string());
                        Some(ReaderMessage::EpochChange {
                            epoch: epoch.to_string(),
                            changed,
                        })
                    }
                };
                if let Some(message) = epoch_change {
                    send(&outgoing, message).await;
                }
            }

            send(
                &outgoing,
                ReaderMessage::Connect {
                    last_event_id: shared.last_event_id(),
                },
            )
            .await;

            let mut stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            loop {
                if stopped(&stop_rx) {
                    abort_reason = Some(DisconnectReason::Stopped);
                    return Ok(());
                }
                let chunk = tokio::time::timeout(config.stall_timeout, stream.next()).await;
                let chunk = match chunk {
                    // Stall: abort the fetch; reconnect re-requests the range.
                    Err(_elapsed) => {
                        abort_reason = Some(DisconnectReason::UpstreamStalled);
                        return Ok(());
                    }
                    Ok(Some(Ok(bytes))) => bytes,
                    Ok(Some(Err(error))) => {
                        return Err(AttemptAbort {
                            message: error.to_string(),
                        });
                    }
                    Ok(None) => break,
                };

                buffer.extend_from_slice(&chunk);
                consume_buffer(&shared, &outgoing, &mut buffer, config.max_block_bytes).await?;
            }

            // Upstream closed cleanly: handle a trailing block without a
            // separator, mirroring the JS tail flush.
            let trailing = String::from_utf8_lossy(&buffer).replace("\r\n", "\n");
            let trailing = trailing.trim();
            if !trailing.is_empty() && trailing.len() <= config.max_block_bytes {
                handle_block(&shared, &outgoing, trailing).await;
            }
            Ok(())
        }
        .await;

        if let Err(abort) = attempt {
            let is_stopped = stopped(&stop_rx);
            if !is_stopped {
                // Only the buildUrl failure maps to "service unavailable";
                // everything else is a transport error.
                let build_url_failed = abort.message == "OpenCode service unavailable";
                send(
                    &outgoing,
                    ReaderMessage::Error(UpstreamError::StreamError {
                        message: abort.message,
                        build_url_failed,
                    }),
                )
                .await;
            }
        }

        let reason = abort_reason.unwrap_or_else(|| {
            if stopped(&stop_rx) {
                DisconnectReason::Stopped
            } else {
                DisconnectReason::Closed
            }
        });
        send(&outgoing, ReaderMessage::Disconnect { reason }).await;

        if !stopped(&stop_rx) {
            reconnect_delay(config.reconnect_delay, &mut stop_rx).await;
        }
    }
}

/// Extract complete blocks (`\n\n`-separated) from the raw byte buffer.
///
/// JS decodes chunks with a streaming TextDecoder and normalizes `\r\n`
/// before buffering; we decode+normalize a copy per round and re-encode the
/// remainder, which keeps cross-chunk `\r\n` pairs and split UTF-8
/// sequences correct.
/// 未完成的尾部写回 buffer 供下一轮拼接；返回 Err 表示本次尝试必须以错误收场。
async fn consume_buffer(
    shared: &Arc<ReaderShared>,
    outgoing: &mpsc::UnboundedSender<ReaderMessage>,
    buffer: &mut Vec<u8>,
    max_block_bytes: usize,
) -> Result<(), AttemptAbort> {
    let text = String::from_utf8_lossy(buffer).replace("\r\n", "\n");
    let mut remainder = text.as_str();

    while let Some(index) = remainder.find("\n\n") {
        let block = &remainder[..index];
        remainder = &remainder[index + 2..];
        if block.len() <= max_block_bytes {
            handle_block(shared, outgoing, block).await;
        } else {
            // Over-budget block: never drop silently and never
            // reconnect-loop on a retained replay entry. The `id:` line is
            // readable without a full parse — advancing the cursor past it
            // and surfacing a synthesized resync lets the consumer
            // reconcile the disavowed range explicitly (plan §5.4).
            shared.record_drop(block.len());
            let dropped_id = block
                .split('\n')
                .find(|line| line.starts_with("id:"))
                .and_then(|line| {
                    let trimmed = line["id:".len()..].trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                });
            let Some(dropped_id) = dropped_id else {
                return Err(AttemptAbort {
                    message: format!("upstream SSE block exceeded {max_block_bytes} bytes"),
                });
            };
            shared.set_last_event_id(dropped_id.clone());
            let _ = outgoing.send(ReaderMessage::Event(UpstreamEvent {
                event_id: Some(dropped_id),
                event_name: Some(RESYNC_EVENT_NAME.to_string()),
                directory: None,
                payload: None,
                synthesized: true,
            }));
        }
    }

    // An unfinished block already past the budget can only grow.
    if remainder.len() > max_block_bytes {
        shared.record_drop(remainder.len());
        return Err(AttemptAbort {
            message: format!(
                "upstream SSE block exceeded {max_block_bytes} bytes without a separator"
            ),
        });
    }

    *buffer = remainder.as_bytes().to_vec();
    Ok(())
}

/// Handle one complete block: control frames advance the cursor (their ids
/// must move `last_event_id` so reconnects do not re-request the disavowed
/// range); malformed blocks never advance it — a reconnect re-requests the
/// range instead of a silent skip.
/// 控制帧与正常事件同样推进游标；malformed 只计入丢弃统计。
async fn handle_block(
    shared: &Arc<ReaderShared>,
    outgoing: &mpsc::UnboundedSender<ReaderMessage>,
    block: &str,
) {
    let Some(envelope) = parse_sse_event_envelope(block) else {
        return;
    };
    if envelope.malformed {
        shared.record_drop(block.len());
        return;
    }
    if let Some(event_id) = &envelope.event_id {
        shared.set_last_event_id(event_id.clone());
    }
    let _ = outgoing.send(ReaderMessage::Event(UpstreamEvent {
        event_id: envelope.event_id,
        event_name: envelope.event_name,
        directory: envelope.directory,
        payload: envelope.payload,
        synthesized: false,
    }));
}

/// UpstreamSseReader 针对 CannedSseServer 的集成测试：重连、游标、超预算与 epoch 语义。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::testing::{Attempt, CannedSseServer};
    use serde_json::json;

    /// 构造指向给定 URL 的默认读取器配置（带 Basic authorization 头）。
    fn reader_config(url: String) -> ReaderConfig {
        ReaderConfig {
            build_url: Box::new(move || Some(url.clone())),
            get_headers: Box::new(|| {
                vec![("authorization".to_string(), "Basic dGVzdA==".to_string())]
            }),
            http: reqwest::Client::new(),
            stall_timeout: Duration::from_millis(DEFAULT_UPSTREAM_STALL_TIMEOUT_MS),
            reconnect_delay: Duration::from_millis(DEFAULT_UPSTREAM_RECONNECT_DELAY_MS),
            max_block_bytes: DEFAULT_UPSTREAM_MAX_BLOCK_BYTES,
            initial_last_event_id: String::new(),
            initial_epoch: None,
        }
    }

    /// 验证事件字段完整解析（id/directory/payload/synthesized）、游标推进，且鉴权与 accept 头确实送达上游。
    #[tokio::test]
    async fn emits_parsed_events_and_tracks_latest_event_id() {
        let server = CannedSseServer::start(vec![Attempt::respond(vec![
            "id: evt-1\r\ndata: {\"type\":\"server.connected\",\"properties\":{\"directory\":\"/tmp/project\"}}\r\n\r\n",
        ])]).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let config = reader_config(server.url("/event"));
        let reader = UpstreamSseReader::start(config, tx);

        // Wait for the event, then stop so join() cannot hang on the
        // hold-open fallback attempt.
        let mut events = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(ReaderMessage::Event(event))) => {
                    events += 1;
                    assert_eq!(event.event_id.as_deref(), Some("evt-1"));
                    assert_eq!(event.directory.as_deref(), Some("/tmp/project"));
                    assert_eq!(
                        event.payload,
                        Some(
                            json!({"type":"server.connected","properties":{"directory":"/tmp/project"}})
                        )
                    );
                    assert!(!event.synthesized);
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => panic!("expected evt-1 before deadline"),
            }
        }
        reader.stop();
        reader.join().await;
        // No additional events beyond the one consumed above (lifecycle
        // messages like the final Disconnect are fine).
        while let Ok(msg) = rx.try_recv() {
            if let ReaderMessage::Event(event) = msg {
                events += 1;
                assert_eq!(event.event_id.as_deref(), Some("evt-1"));
            }
        }
        assert_eq!(events, 1);
        assert_eq!(reader.last_event_id(), "evt-1");
        // Engine auth header must reach the upstream request.
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization.as_deref(), Some("Basic dGVzdA=="));
        assert_eq!(requests[0].accept.as_deref(), Some("text/event-stream"));
    }

    /// 验证停滞超时以 upstream_stalled 断开，重连携带 Last-Event-ID 续传后续事件。
    #[tokio::test]
    async fn reconnects_a_stalled_stream_with_last_event_id() {
        // Attempt 1: one event, then the stream holds open and stalls.
        // Attempt 2: resumes with Last-Event-ID and delivers evt-2.
        let server = CannedSseServer::start(vec![
            Attempt::respond_holding(vec![
                "id: evt-1\ndata: {\"type\":\"server.connected\",\"properties\":{}}\n\n",
            ]),
            Attempt::respond(vec![
                "id: evt-2\ndata: {\"type\":\"session.updated\",\"properties\":{}}\n\n",
            ]),
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = reader_config(server.url("/event"));
        config.stall_timeout = Duration::from_millis(120);
        config.reconnect_delay = Duration::from_millis(10);
        let reader = UpstreamSseReader::start(config, tx);

        let mut event_ids = Vec::new();
        let mut stalled = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while event_ids.len() < 2 && tokio::time::Instant::now() < deadline {
            match rx.recv().await.expect("message before deadline") {
                ReaderMessage::Event(event) => {
                    if let Some(id) = event.event_id.clone() {
                        event_ids.push(id);
                        if event_ids.len() == 2 {
                            reader.stop();
                        }
                    }
                }
                ReaderMessage::Disconnect {
                    reason: DisconnectReason::UpstreamStalled,
                } => {
                    stalled = true;
                }
                _ => {}
            }
        }
        reader.join().await;

        assert_eq!(event_ids, vec!["evt-1", "evt-2"]);
        assert!(stalled, "stall must abort the attempt as upstream_stalled");
        assert_eq!(reader.last_event_id(), "evt-2");
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].last_event_id, None);
        assert_eq!(requests[1].last_event_id.as_deref(), Some("evt-1"));
    }

    /// 验证非 2xx 上报 UpstreamUnavailable 错误并持续重试直到成功。
    #[tokio::test]
    async fn reports_unavailable_upstream_and_keeps_retrying() {
        let server = CannedSseServer::start(vec![
            Attempt::status(503),
            Attempt::respond(vec![
                "id: evt-1\ndata: {\"type\":\"server.connected\",\"properties\":{}}\n\n",
            ]),
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = reader_config(server.url("/event"));
        config.reconnect_delay = Duration::from_millis(5);
        let reader = UpstreamSseReader::start(config, tx);

        let mut unavailable = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match rx.recv().await.expect("message") {
                ReaderMessage::Error(UpstreamError::UpstreamUnavailable { status }) => {
                    unavailable = Some(status);
                }
                ReaderMessage::Event(event) if event.event_id.as_deref() == Some("evt-1") => {
                    reader.stop();
                    break;
                }
                _ => {}
            }
        }
        reader.join().await;
        assert_eq!(unavailable, Some(503));
        assert_eq!(server.requests().len(), 2);
    }

    /// 验证无 data 控制帧推进游标，而畸形业务块计入丢弃且不推进游标。
    #[tokio::test]
    async fn control_frames_advance_cursor_and_malformed_blocks_do_not() {
        let server = CannedSseServer::start(vec![Attempt::respond(vec![
            // Control frame: id + event, no data → cursor advances.
            "event: omp.stream.resync\nid: ctl-1\n\n",
            // Malformed business block: data present but unparseable → the
            // cursor must NOT advance past it.
            "id: bad-1\ndata: {not json}\n\n",
            // Well-formed event after the malformed one.
            "id: evt-9\ndata: {\"type\":\"x\",\"properties\":{}}\n\n",
        ])])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reader = UpstreamSseReader::start(reader_config(server.url("/event")), tx);

        let mut names = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(ReaderMessage::Event(event))) => {
                    let terminal = event.event_id.as_deref() == Some("evt-9");
                    names.push((
                        event.event_id.clone(),
                        event.event_name.clone(),
                        event.payload.is_none(),
                    ));
                    if terminal {
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => panic!("expected evt-9 before deadline"),
            }
        }
        reader.stop();
        reader.join().await;
        while let Ok(msg) = rx.try_recv() {
            if let ReaderMessage::Event(event) = msg {
                names.push((
                    event.event_id.clone(),
                    event.event_name.clone(),
                    event.payload.is_none(),
                ));
            }
        }
        assert_eq!(
            names,
            vec![
                (
                    Some("ctl-1".to_string()),
                    Some("omp.stream.resync".to_string()),
                    true
                ),
                (Some("evt-9".to_string()), None, false),
            ]
        );
        assert_eq!(reader.last_event_id(), "evt-9");
        assert_eq!(reader.stats().dropped_blocks, 1);
    }

    /// 验证超预算块通过“推进到其 id + 合成 resync”显式否认，后续事件照常处理。
    #[tokio::test]
    async fn disavows_oversized_block_through_synthesized_resync() {
        let big = "x".repeat(500);
        let server = CannedSseServer::start(vec![Attempt::respond(vec![
            "id: evt-1\ndata: {\"type\":\"a\",\"properties\":{}}\n\n",
            &format!(
                "id: evt-big\ndata: {{\"type\":\"huge\",\"properties\":{{\"pad\":\"{big}\"}}}}\n\n"
            ),
            "id: evt-3\ndata: {\"type\":\"c\",\"properties\":{}}\n\n",
        ])])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = reader_config(server.url("/event"));
        config.max_block_bytes = 64;
        let reader = UpstreamSseReader::start(config, tx);

        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(ReaderMessage::Event(event))) => {
                    let terminal = event.event_id.as_deref() == Some("evt-3");
                    seen.push((
                        event.event_id.clone(),
                        event.event_name.clone(),
                        event.synthesized,
                    ));
                    if terminal {
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => panic!("expected evt-3 before deadline"),
            }
        }
        reader.stop();
        reader.join().await;
        while let Ok(msg) = rx.try_recv() {
            if let ReaderMessage::Event(event) = msg {
                seen.push((
                    event.event_id.clone(),
                    event.event_name.clone(),
                    event.synthesized,
                ));
            }
        }
        assert_eq!(
            seen,
            vec![
                (Some("evt-1".to_string()), None, false),
                (
                    Some("evt-big".to_string()),
                    Some("omp.stream.resync".to_string()),
                    true
                ),
                (Some("evt-3".to_string()), None, false),
            ]
        );
        assert_eq!(reader.last_event_id(), "evt-3");
        assert_eq!(reader.stats().dropped_blocks, 1);
    }

    /// 验证无 id 的超预算块以 stream error 中断本次尝试，重连从最后已处理块重新请求。
    #[tokio::test]
    async fn aborts_and_reconnects_on_oversized_block_without_id() {
        let big = "x".repeat(500);
        let server = CannedSseServer::start(vec![
            Attempt::respond(vec![
                "id: evt-1\ndata: {\"type\":\"a\",\"properties\":{}}\n\n",
                &format!("data: {{\"type\":\"huge\",\"properties\":{{\"pad\":\"{big}\"}}}}\n\n"),
            ]),
            Attempt::respond(vec![
                "id: evt-3\ndata: {\"type\":\"c\",\"properties\":{}}\n\n",
            ]),
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = reader_config(server.url("/event"));
        config.max_block_bytes = 64;
        config.reconnect_delay = Duration::from_millis(5);
        let reader = UpstreamSseReader::start(config, tx);

        let mut stream_errors = 0;
        let mut got_evt_3 = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match rx.recv().await.expect("message") {
                ReaderMessage::Error(UpstreamError::StreamError { .. }) => stream_errors += 1,
                ReaderMessage::Event(event) if event.event_id.as_deref() == Some("evt-3") => {
                    got_evt_3 = true;
                    reader.stop();
                    break;
                }
                _ => {}
            }
        }
        reader.join().await;
        assert!(got_evt_3);
        assert!(
            stream_errors >= 1,
            "id-less oversize block must abort with a stream error"
        );
        let requests = server.requests();
        // The reconnect re-requests from the last handled block (evt-1).
        assert_eq!(requests[1].last_event_id.as_deref(), Some("evt-1"));
        assert!(reader.stats().dropped_blocks >= 1);
    }

    /// 验证学习上游 epoch、变化时作废游标，且重连回显已学身份。
    #[tokio::test]
    async fn learns_epoch_and_invalidates_cursor_on_change() {
        let server = CannedSseServer::start(vec![
            Attempt::respond_with_epoch(
                vec!["id: e1\ndata: {\"type\":\"a\",\"properties\":{}}\n\n"],
                "boot-1",
            ),
            Attempt::respond_with_epoch(
                vec!["id: e2\ndata: {\"type\":\"b\",\"properties\":{}}\n\n"],
                "boot-2",
            ),
        ])
        .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut config = reader_config(server.url("/event"));
        config.reconnect_delay = Duration::from_millis(5);
        let reader = UpstreamSseReader::start(config, tx);

        let mut epoch_changes = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match rx.recv().await.expect("message") {
                ReaderMessage::EpochChange { epoch, changed } => {
                    epoch_changes.push((epoch, changed))
                }
                ReaderMessage::Event(event) if event.event_id.as_deref() == Some("e2") => {
                    reader.stop();
                    break;
                }
                _ => {}
            }
        }
        reader.join().await;
        assert_eq!(
            epoch_changes,
            vec![("boot-1".to_string(), false), ("boot-2".to_string(), true)]
        );
        assert_eq!(reader.epoch().as_deref(), Some("boot-2"));
        assert_eq!(reader.stats().epoch_changes, 1);
        // Second connect echoed the learned boot identity.
        assert_eq!(server.requests()[1].epoch.as_deref(), Some("boot-1"));
    }

    /// 验证 URL 构造失败上报 build_url_failed 的 StreamError，绝不静默成功。
    #[tokio::test]
    async fn build_url_failure_reports_service_unavailable() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let config = ReaderConfig {
            build_url: Box::new(|| None),
            get_headers: Box::new(|| vec![]),
            http: reqwest::Client::new(),
            stall_timeout: Duration::from_millis(DEFAULT_UPSTREAM_STALL_TIMEOUT_MS),
            reconnect_delay: Duration::from_millis(5),
            max_block_bytes: DEFAULT_UPSTREAM_MAX_BLOCK_BYTES,
            initial_last_event_id: String::new(),
            initial_epoch: None,
        };
        let reader = UpstreamSseReader::start(config, tx.clone());
        tokio::time::sleep(Duration::from_millis(30)).await;
        reader.stop();
        reader.join().await;

        let mut saw_build_failure = false;
        while let Ok(msg) = rx.try_recv() {
            if let ReaderMessage::Error(UpstreamError::StreamError {
                message,
                build_url_failed,
            }) = msg
            {
                assert_eq!(message, "OpenCode service unavailable");
                assert!(build_url_failed);
                saw_build_failure = true;
            }
        }
        assert!(
            saw_build_failure,
            "engine-unavailable must surface as an error, never silent success"
        );
    }
}
