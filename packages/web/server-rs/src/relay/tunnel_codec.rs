//! Port of `server/lib/relay/tunnel-codec.js` — tunnel mux frame codec
//! (Layer 3 of the protocol spec). Pure functions, no I/O.
//!
//! JS mirror contract: byte-compatible with
//! `packages/ui/src/lib/relay/tunnel-codec.ts`; the pinned vectors in
//! [`self::tests`] come from running the JS codec directly.
//!
//! Frame layout: `[1 byte frameType (high bit = fragment-continues)][4 byte BE
//! streamId][payload]`. Client-initiated streams use odd streamIds starting at
//! 1; even ids are reserved.
//!
//! 中文概述：tunnel 多路复用帧的编解码器（协议第 3 层），从 JS 版
//! tunnel-codec.js 移植，全部为纯函数、无 I/O。覆盖：单帧编解码、
//! 大消息分片与重组、batch 信封封装，以及出站帧合并缓冲器
//! （OutboundFrameBatcher，唯一的异步部分）。与 UI 侧 TS 镜像实现
//! 保持字节级兼容，错误文案也与 JS TunnelCodecError 逐字对齐。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::e2ee::MAX_PLAINTEXT_FRAME_BYTES;

/// tunnel 帧固定头部长度：1 字节 frameType + 4 字节大端 streamId。
pub const TUNNEL_FRAME_HEADER_BYTES: usize = 5;
/// frameType 首位掩码（0x80）：置位表示「本帧之后还有分片」。
pub const TUNNEL_FRAGMENT_FLAG: u8 = 0x80;

// Batch envelope container (mirror of protocol.ts). Only used when both peers
// negotiated `batch`. Reserve the per-frame envelope overhead from the payload
// budget so any single frame still fits one 64 KiB encrypted plaintext.
/// batch 信封「单帧」容器 tag：1 字节 0x00 后直接跟原始帧，无长度前缀。
pub const BATCH_CONTAINER_TAG_SINGLE: u8 = 0x00;
/// batch 信封「多帧」容器 tag：1 字节 0x01 后跟若干「4 字节大端长度+帧」。
pub const BATCH_CONTAINER_TAG_BATCH: u8 = 0x01;
/// batch 信封中每帧的长度前缀字节数（大端 u32）。
pub const BATCH_FRAME_LENGTH_BYTES: usize = 4;
/// 每帧在 batch 信封中的最大开销：1 字节容器 tag + 4 字节长度前缀。
pub const BATCH_ENVELOPE_RESERVED_BYTES: usize = 1 + BATCH_FRAME_LENGTH_BYTES;
/// 单个 tunnel 帧允许的最大 payload：从一次加密的明文预算里扣除帧头
/// 与 batch 信封预留，保证任何单帧装进信封后仍不超限。
pub const MAX_TUNNEL_PAYLOAD_BYTES: usize =
    MAX_PLAINTEXT_FRAME_BYTES - TUNNEL_FRAME_HEADER_BYTES - BATCH_ENVELOPE_RESERVED_BYTES;

/// tunnel 帧类型常量表（12 种帧）。数值与 JS/TS 实现逐一对应；高位
/// 0x80 不属于类型本身，只作分片标志。
pub mod frame_type {
    /// HTTP 请求头（JSON 编码的请求元数据），开启一条 HTTP 流。
    pub const HTTP_REQUEST: u8 = 1;
    /// HTTP 请求/响应的 body 数据块（双向均可发送）。
    pub const HTTP_BODY: u8 = 2;
    /// HTTP 响应头（状态码 + headers），服务端回传。
    pub const HTTP_RESPONSE: u8 = 3;
    /// 流正常结束：发送端不会再有数据（HTTP 与 WS 流通用）。
    pub const STREAM_END: u8 = 4;
    /// 流异常中止（abort）：接收方应立即释放流资源。
    pub const STREAM_ABORT: u8 = 5;
    /// 客户端请求建立 WebSocket 流（携带目标 URL 等元数据）。
    pub const WS_OPEN: u8 = 6;
    /// 主机确认 WebSocket 流已建立。
    pub const WS_OPENED: u8 = 7;
    /// WebSocket 文本帧数据。
    pub const WS_TEXT: u8 = 8;
    /// WebSocket 二进制帧数据。
    pub const WS_BINARY: u8 = 9;
    /// WebSocket 流关闭（携带 close code/reason）。
    pub const WS_CLOSE: u8 = 10;
    /// keepalive Ping（链路层保活）。
    pub const PING: u8 = 11;
    /// keepalive Pong，回应 Ping。
    pub const PONG: u8 = 12;

    /// 全部合法帧类型集合，供 is_tunnel_frame_type 做成员校验。
    const ALL: [u8; 12] = [
        HTTP_REQUEST,
        HTTP_BODY,
        HTTP_RESPONSE,
        STREAM_END,
        STREAM_ABORT,
        WS_OPEN,
        WS_OPENED,
        WS_TEXT,
        WS_BINARY,
        WS_CLOSE,
        PING,
        PONG,
    ];

    /// 判断字节是否为已知的 tunnel 帧类型（不含分片标志位）。
    pub fn is_tunnel_frame_type(value: u8) -> bool {
        ALL.contains(&value)
    }
}

/// streamId 上限（u32::MAX）。JS 版显式校验它；Rust 的 u32 类型天然
/// 保证该界不可能被超越。
const MAX_STREAM_ID: u32 = 0xffffffff;

/// Error messages are pinned to the JS `TunnelCodecError` strings.
/// 编解码错误。元组字段 `.0` 即错误消息字符串，与 JS
/// `TunnelCodecError` 的文案逐字对齐（跨实现测试据此比对）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelCodecError(pub String);

/// Display 实现：直接透出 `.0` 中的错误消息。
impl std::fmt::Display for TunnelCodecError {
    /// 输出 `.0` 的原始内容，不添加任何包装前缀。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error trait 空实现：Display 已提供人类可读消息，无需附加 source。
impl std::error::Error for TunnelCodecError {}

/// 私有构造器所在实现块。
impl TunnelCodecError {
    /// 以任意可转 String 的消息构造错误（仅模块内部使用）。
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// 编码一个 tunnel 帧：`[frameType(|0x80)][streamId BE u32][payload]`。
///
/// `has_more_fragments` 置位时 frameType 首位被置 1，表示后续还有
/// 分片；payload 超过 MAX_TUNNEL_PAYLOAD_BYTES 时返回错误（消息固定，
/// 与 JS 一致）。
pub fn encode_tunnel_frame(
    frame_type: u8,
    stream_id: u32,
    payload: &[u8],
    has_more_fragments: bool,
) -> Result<Vec<u8>, TunnelCodecError> {
    // stream_id is u32 and MAX_STREAM_ID is u32::MAX: the JS bound is not
    // representable here (clippy: comparison always false).
    if payload.len() > MAX_TUNNEL_PAYLOAD_BYTES {
        return Err(TunnelCodecError::new("tunnel payload exceeds maximum size"));
    }
    let mut frame = Vec::with_capacity(TUNNEL_FRAME_HEADER_BYTES + payload.len());
    frame.push(if has_more_fragments {
        frame_type | TUNNEL_FRAGMENT_FLAG
    } else {
        frame_type
    });
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// decode_tunnel_frame 的解码结果：一帧的四个组成部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTunnelFrame {
    /// 去掉分片标志位后的帧类型（见 frame_type 模块）。
    pub frame_type: u8,
    /// 大端解析出的 streamId。
    pub stream_id: u32,
    /// 帧头之后的原始 payload 字节。
    pub payload: Vec<u8>,
    /// 是否还有后续分片（frameType 首位）。
    pub has_more_fragments: bool,
}

/// 解码一个 tunnel 帧。帧长不足 5 字节、或去掉分片位后的类型不在
/// frame_type 表中时返回错误；错误文案与 JS 版逐字一致。
pub fn decode_tunnel_frame(frame: &[u8]) -> Result<DecodedTunnelFrame, TunnelCodecError> {
    if frame.len() < TUNNEL_FRAME_HEADER_BYTES {
        return Err(TunnelCodecError::new("tunnel frame too short"));
    }
    let raw_type = frame[0];
    let has_more_fragments = (raw_type & TUNNEL_FRAGMENT_FLAG) != 0;
    let frame_type_value = raw_type & !TUNNEL_FRAGMENT_FLAG;
    if !frame_type::is_tunnel_frame_type(frame_type_value) {
        return Err(TunnelCodecError::new(format!(
            "unknown tunnel frame type {frame_type_value}"
        )));
    }
    Ok(DecodedTunnelFrame {
        frame_type: frame_type_value,
        stream_id: u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]),
        payload: frame[TUNNEL_FRAME_HEADER_BYTES..].to_vec(),
        has_more_fragments,
    })
}

/// 把 JSON 值序列化为帧 payload 字节。序列化失败时返回空 Vec，
/// 由后续 decode_json_payload 的解析/校验兜底。
pub fn encode_json_payload(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

/// 解析 JSON payload 并用 `validate` 谓词校验形状。反序列化失败或
/// 校验不过时返回错误；两条错误消息均与 JS 版对齐。
pub fn decode_json_payload(
    payload: &[u8],
    validate: impl Fn(&serde_json::Value) -> bool,
) -> Result<serde_json::Value, TunnelCodecError> {
    let parsed: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| TunnelCodecError::new("malformed JSON tunnel payload"))?;
    if !validate(&parsed) {
        return Err(TunnelCodecError::new(
            "unexpected JSON tunnel payload shape",
        ));
    }
    Ok(parsed)
}

/// Split a body/message into payload-sized chunks. Empty input yields one
/// empty chunk (JS `chunkPayload`).
///
/// 中文补充：chunk_size 必须落在 (0, MAX_TUNNEL_PAYLOAD_BYTES] 区间；
/// 空输入返回一个空块（而不是零个块），保证空消息仍会发出一帧。
pub fn chunk_payload(bytes: &[u8], chunk_size: usize) -> Result<Vec<Vec<u8>>, TunnelCodecError> {
    if chunk_size == 0 || chunk_size > MAX_TUNNEL_PAYLOAD_BYTES {
        return Err(TunnelCodecError::new("invalid chunk size"));
    }
    if bytes.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    Ok(bytes.chunks(chunk_size).map(<[u8]>::to_vec).collect())
}

/// Encode one logical message as one or more frames, setting the fragment flag
/// on all but the last (JS `encodeFragmentedMessage`).
///
/// 中文补充：按 MAX_TUNNEL_PAYLOAD_BYTES 切块后逐帧编码，除最后一帧
/// 外都置分片标志；块大小恒合法，因此内部错误路径不会触发（unwrap
/// 兜底为空帧）。
pub fn encode_fragmented_message(frame_type: u8, stream_id: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    let chunks = chunk_payload(payload, MAX_TUNNEL_PAYLOAD_BYTES).unwrap_or_default();
    let last = chunks.len().saturating_sub(1);
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            encode_tunnel_frame(frame_type, stream_id, &chunk, index < last).unwrap_or_default()
        })
        .collect()
}

/// 某条 (streamId, frameType) 消息已收到的分片累积状态。
struct PendingFragments {
    /// 已收到的分片（保持到达顺序）。
    chunks: Vec<Vec<u8>>,
    /// 已累积的字节数，用于总量上限校验。
    total_bytes: usize,
}

/// Reassembles fragmented messages per `(streamId, frameType)`. Bounded to
/// protect memory (JS `createFragmentAssembler`).
///
/// 中文补充：以 (streamId, frameType) 为键缓存分片；单条消息总字节数
/// 超过 max_message_bytes 时报错，防止对端用海量分片耗尽内存。
pub struct FragmentAssembler {
    /// 进行中的分片消息表：((streamId, frameType)) -> 累积状态。
    pending: HashMap<(u32, u8), PendingFragments>,
    /// 单条重组消息的最大字节数。
    max_message_bytes: usize,
}

/// 默认构造：消息上限 16 MiB（与 JS 版默认一致）。
impl Default for FragmentAssembler {
    /// 以 16 MiB 的消息上限构造重组器。
    fn default() -> Self {
        Self::new(16 * 1024 * 1024)
    }
}

/// 重组器的推入/丢弃操作。
impl FragmentAssembler {
    /// 以指定的单消息字节上限构造重组器。
    pub fn new(max_message_bytes: usize) -> Self {
        Self {
            pending: HashMap::new(),
            max_message_bytes,
        }
    }

    /// Returns the complete message payload once all fragments arrived, or
    /// `None` while more fragments are expected.
    ///
    /// 中文补充：未分片消息直接返回 payload；首个分片先入表，末片
    /// 到达时按顺序拼接为完整消息。超过上限返回错误，且已累积的
    /// 分片随之丢弃（entry 已被 remove）。
    pub fn push(&mut self, frame: DecodedTunnelFrame) -> Result<Option<Vec<u8>>, TunnelCodecError> {
        let key = (frame.stream_id, frame.frame_type);
        let entry = self.pending.remove(&key);
        if !frame.has_more_fragments && entry.is_none() {
            return Ok(Some(frame.payload));
        }
        let (mut chunks, carried) = match entry {
            Some(entry) => (entry.chunks, entry.total_bytes),
            None => (Vec::new(), 0),
        };
        let total_bytes = carried + frame.payload.len();
        if total_bytes > self.max_message_bytes {
            return Err(TunnelCodecError::new(
                "fragmented message exceeds maximum size",
            ));
        }
        chunks.push(frame.payload);
        if frame.has_more_fragments {
            self.pending.insert(
                key,
                PendingFragments {
                    chunks,
                    total_bytes,
                },
            );
            return Ok(None);
        }
        let mut message = vec![0u8; total_bytes];
        let mut offset = 0usize;
        for chunk in chunks {
            message[offset..offset + chunk.len()].copy_from_slice(&chunk);
            offset += chunk.len();
        }
        Ok(Some(message))
    }

    /// 丢弃指定 streamId 的全部未完成分片（流 abort/close 时调用，
    /// 防止内存滞留）。
    pub fn drop_stream(&mut self, stream_id: u32) {
        self.pending
            .retain(|(pending_stream, _), _| *pending_stream != stream_id);
    }
}

/// Batch envelope encoder (mirror of tunnel-codec.ts `encodeFrameBatch`). Only
/// used when both peers negotiated `batch`. One encrypted WS message still
/// equals one `encrypt()` call — this only changes how many tunnel frames it
/// carries.
///
/// 中文补充：空列表是调用方 bug，直接报错；单帧走 0x00 紧凑格式，
/// 多帧走 0x01 + 每帧 4 字节大端长度；总长超过一次加密的明文上限
/// 时报错。
pub fn encode_frame_batch(frames: &[Vec<u8>]) -> Result<Vec<u8>, TunnelCodecError> {
    if frames.is_empty() {
        return Err(TunnelCodecError::new("cannot encode an empty frame batch"));
    }
    if frames.len() == 1 {
        let frame = &frames[0];
        let mut out = Vec::with_capacity(1 + frame.len());
        out.push(BATCH_CONTAINER_TAG_SINGLE);
        out.extend_from_slice(frame);
        if out.len() > MAX_PLAINTEXT_FRAME_BYTES {
            return Err(TunnelCodecError::new(
                "frame batch exceeds maximum plaintext size",
            ));
        }
        return Ok(out);
    }
    let mut total = 1usize;
    for frame in frames {
        total += BATCH_FRAME_LENGTH_BYTES + frame.len();
    }
    if total > MAX_PLAINTEXT_FRAME_BYTES {
        return Err(TunnelCodecError::new(
            "frame batch exceeds maximum plaintext size",
        ));
    }
    let mut out = vec![0u8; total];
    out[0] = BATCH_CONTAINER_TAG_BATCH;
    let mut offset = 1usize;
    for frame in frames {
        out[offset..offset + 4].copy_from_slice(&(frame.len() as u32).to_be_bytes());
        offset += BATCH_FRAME_LENGTH_BYTES;
        out[offset..offset + frame.len()].copy_from_slice(frame);
        offset += frame.len();
    }
    Ok(out)
}

/// Decodes a batch-envelope plaintext into its ordered tunnel frames.
///
/// 中文补充：先读容器 tag（0x00 单帧 / 0x01 批量 / 其它报错）；批量
/// 格式逐帧读取长度前缀与帧体，长度越界（截断）或最终解析出空帧
/// 列表时报错。返回帧保持编码时的顺序。
pub fn decode_frame_batch(plaintext: &[u8]) -> Result<Vec<Vec<u8>>, TunnelCodecError> {
    if plaintext.is_empty() {
        return Err(TunnelCodecError::new("empty batch plaintext"));
    }
    let tag = plaintext[0];
    if tag == BATCH_CONTAINER_TAG_SINGLE {
        return Ok(vec![plaintext[1..].to_vec()]);
    }
    if tag != BATCH_CONTAINER_TAG_BATCH {
        return Err(TunnelCodecError::new(format!(
            "unknown batch container tag {tag}"
        )));
    }
    let mut frames = Vec::new();
    let mut offset = 1usize;
    while offset < plaintext.len() {
        if offset + BATCH_FRAME_LENGTH_BYTES > plaintext.len() {
            return Err(TunnelCodecError::new("truncated batch frame length"));
        }
        let length = u32::from_be_bytes([
            plaintext[offset],
            plaintext[offset + 1],
            plaintext[offset + 2],
            plaintext[offset + 3],
        ]) as usize;
        offset += BATCH_FRAME_LENGTH_BYTES;
        if offset + length > plaintext.len() {
            return Err(TunnelCodecError::new("truncated batch frame body"));
        }
        frames.push(plaintext[offset..offset + length].to_vec());
        offset += length;
    }
    if frames.is_empty() {
        return Err(TunnelCodecError::new("empty frame batch"));
    }
    Ok(frames)
}

// Only high-volume body/stream data is buffered; setup/teardown/keepalive
// frames flush immediately so TTFT, terminal echo, and liveness stay snappy.
/// 判断帧类型是否允许进入批量缓冲：只有高频的 body/WS 数据帧会被
/// 缓冲合并，setup/teardown/keepalive 帧立即刷出（理由见上方注释）。
fn is_buffered_frame_type(frame_type: u8) -> bool {
    matches!(
        frame_type,
        frame_type::HTTP_BODY | frame_type::WS_TEXT | frame_type::WS_BINARY
    )
}

// See the TS mirror (tunnel-codec.ts) for the 150ms rationale: the chat render
// pipeline's 100ms input throttle + ~64ms paced-reveal smoothing make a 150ms
// batch window invisible.
/// 默认批量窗口：150ms（理由见上方注释与 TS 镜像实现）。
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 150;
/// 默认单批字节上限：24 KiB，未到明文上限即提前发出以降低延迟。
pub const DEFAULT_BATCH_MAX_BYTES: usize = 24 * 1024;
/// 默认单批帧数上限：32 帧。
pub const DEFAULT_BATCH_MAX_FRAMES: usize = 32;

/// 批量结果的发送回调：接收已封装的明文（调用方负责加密与 WS 发送）。
type SendBatchFn = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// Outbound batching buffer (mirror of tunnel-codec.ts
/// `createOutboundFrameBatcher`). Runs as one tokio task: `enqueue` hands
/// frames to the task; the task applies the JS flush rules (immediate flush
/// for non-buffered frame types, window/byte/frame caps otherwise).
///
/// Dropping the handle (or calling [`OutboundFrameBatcher::dispose`]) discards
/// any buffered frames without flushing, mirroring JS `dispose()`.
///
/// 中文补充：句柄只是无界 channel 的发送端，克隆共享同一个批处理
/// 任务；最后一个句柄消失（或 dispose）时任务退出并丢弃缓冲帧。
#[derive(Clone)]
pub struct OutboundFrameBatcher {
    /// 指向批处理任务的发送端；drop 最后一个克隆即终止任务。
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// 批处理器的生命周期与入队接口。
impl OutboundFrameBatcher {
    /// 以默认字节/帧数上限启动批处理任务。
    pub fn start(window_ms: u64, send_batch: SendBatchFn) -> Self {
        Self::with_limits(
            window_ms,
            DEFAULT_BATCH_MAX_BYTES,
            DEFAULT_BATCH_MAX_FRAMES,
            send_batch,
        )
    }

    /// 以显式的窗口、字节与帧数上限启动批处理任务。
    pub fn with_limits(
        window_ms: u64,
        max_batch_bytes: usize,
        max_batch_frames: usize,
        send_batch: SendBatchFn,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_batcher(
            rx,
            Duration::from_millis(window_ms),
            max_batch_bytes,
            max_batch_frames,
            send_batch,
        ));
        Self { tx }
    }

    /// 把一个已编码帧交给批处理任务（fire-and-forget，永不阻塞）。
    pub fn enqueue(&self, frame: Vec<u8>) {
        let _ = self.tx.send(frame);
    }

    /// Flush any buffered frames now (JS `flush()`). Fire-and-forget like the
    /// JS sync method; tests await the window or a follow-up send.
    ///
    /// 中文补充：通过哨兵 payload 请求立即刷出；与 JS 一样是异步生效
    /// 的尽力而为操作，没有完成回执。
    pub fn flush(&self) {
        let _ = self.tx.send(FLUSH_MARKER.to_vec());
    }

    /// Discard buffered frames and stop batching (JS `dispose`).
    ///
    /// 中文补充：drop 发送端让任务退出并丢弃缓冲帧；幂等，可安全
    /// 重复调用。
    pub fn dispose(self) {
        // Dropping the sender ends the task; the run loop discards its buffer
        // on channel close instead of flushing.
    }
}

/// Sentinel payload signalling an explicit flush request (empty frame is
/// otherwise a valid — if unusual — tunnel frame).
///
/// 中文补充：以 \x00 开头，与任何真实帧的首字节（合法 frameType）
/// 区分开。
static FLUSH_MARKER: &[u8] = b"\x00__relay_flush__";

/// 批处理任务主体：实现 JS createOutboundFrameBatcher 的全部刷出
/// 规则——非缓冲帧类型立即 flush；缓冲类型按窗口/字节/帧数上限
/// 合并；收到 flush 哨兵或窗口计时器到期时刷出；发送端全部 drop 时
/// 直接退出（丢弃缓冲，不 flush）。
async fn run_batcher(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    window: Duration,
    max_batch_bytes: usize,
    max_batch_frames: usize,
    send_batch: SendBatchFn,
) {
    let mut buffer: Vec<Vec<u8>> = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut deadline: Option<Instant> = None;
    // JS initializes `lastFlushAt = 0`, so the first buffered frame always
    // passes the `at - lastFlushAt >= windowMs` check.
    let mut last_flush_at: Option<Instant> = None;

    // 把缓冲中的帧打包成 batch 明文并发送；空缓冲直接返回（嵌套于
    // 函数体内，用普通注释避免 unused_doc_comments）。
    fn flush(
        buffer: &mut Vec<Vec<u8>>,
        buffered_bytes: &mut usize,
        last_flush_at: &mut Option<Instant>,
        send_batch: &SendBatchFn,
    ) {
        if buffer.is_empty() {
            return;
        }
        let frames = std::mem::take(buffer);
        *buffered_bytes = 0;
        *last_flush_at = Some(Instant::now());
        if let Ok(plaintext) = encode_frame_batch(&frames) {
            send_batch(plaintext);
        }
    }

    loop {
        let timer = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    // Sender dropped: dispose — discard the buffer without flushing.
                    break;
                };
                if frame.as_slice() == FLUSH_MARKER {
                    deadline = None;
                    flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
                    continue;
                }
                let frame_type = frame.first().copied().unwrap_or(0) & !TUNNEL_FRAGMENT_FLAG;
                if !is_buffered_frame_type(frame_type) {
                    buffer.push(frame);
                    deadline = None;
                    flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
                    continue;
                }
                let at = Instant::now();
                let window_passed = match last_flush_at {
                    Some(last) => at.duration_since(last) >= window,
                    None => true,
                };
                if buffer.is_empty() && window_passed {
                    buffer.push(frame);
                    deadline = None;
                    flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
                    continue;
                }
                let frame_cost = BATCH_FRAME_LENGTH_BYTES + frame.len();
                if !buffer.is_empty() && 1 + buffered_bytes + frame_cost > MAX_PLAINTEXT_FRAME_BYTES {
                    deadline = None;
                    flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
                }
                buffer.push(frame);
                buffered_bytes += frame_cost;
                if buffered_bytes >= max_batch_bytes || buffer.len() >= max_batch_frames {
                    deadline = None;
                    flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
                    continue;
                }
                if deadline.is_none() {
                    deadline = Some(at + window);
                }
            }
            _ = timer => {
                deadline = None;
                flush(&mut buffer, &mut buffered_bytes, &mut last_flush_at, &send_batch);
            }
        }
    }
}
