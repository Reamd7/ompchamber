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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::e2ee::MAX_PLAINTEXT_FRAME_BYTES;

pub const TUNNEL_FRAME_HEADER_BYTES: usize = 5;
pub const TUNNEL_FRAGMENT_FLAG: u8 = 0x80;

// Batch envelope container (mirror of protocol.ts). Only used when both peers
// negotiated `batch`. Reserve the per-frame envelope overhead from the payload
// budget so any single frame still fits one 64 KiB encrypted plaintext.
pub const BATCH_CONTAINER_TAG_SINGLE: u8 = 0x00;
pub const BATCH_CONTAINER_TAG_BATCH: u8 = 0x01;
pub const BATCH_FRAME_LENGTH_BYTES: usize = 4;
pub const BATCH_ENVELOPE_RESERVED_BYTES: usize = 1 + BATCH_FRAME_LENGTH_BYTES;
pub const MAX_TUNNEL_PAYLOAD_BYTES: usize =
    MAX_PLAINTEXT_FRAME_BYTES - TUNNEL_FRAME_HEADER_BYTES - BATCH_ENVELOPE_RESERVED_BYTES;

pub mod frame_type {
    pub const HTTP_REQUEST: u8 = 1;
    pub const HTTP_BODY: u8 = 2;
    pub const HTTP_RESPONSE: u8 = 3;
    pub const STREAM_END: u8 = 4;
    pub const STREAM_ABORT: u8 = 5;
    pub const WS_OPEN: u8 = 6;
    pub const WS_OPENED: u8 = 7;
    pub const WS_TEXT: u8 = 8;
    pub const WS_BINARY: u8 = 9;
    pub const WS_CLOSE: u8 = 10;
    pub const PING: u8 = 11;
    pub const PONG: u8 = 12;

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

    pub fn is_tunnel_frame_type(value: u8) -> bool {
        ALL.contains(&value)
    }
}

const MAX_STREAM_ID: u32 = 0xffffffff;

/// Error messages are pinned to the JS `TunnelCodecError` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelCodecError(pub String);

impl std::fmt::Display for TunnelCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TunnelCodecError {}

impl TunnelCodecError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTunnelFrame {
    pub frame_type: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
    pub has_more_fragments: bool,
}

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

pub fn encode_json_payload(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

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

struct PendingFragments {
    chunks: Vec<Vec<u8>>,
    total_bytes: usize,
}

/// Reassembles fragmented messages per `(streamId, frameType)`. Bounded to
/// protect memory (JS `createFragmentAssembler`).
pub struct FragmentAssembler {
    pending: HashMap<(u32, u8), PendingFragments>,
    max_message_bytes: usize,
}

impl Default for FragmentAssembler {
    fn default() -> Self {
        Self::new(16 * 1024 * 1024)
    }
}

impl FragmentAssembler {
    pub fn new(max_message_bytes: usize) -> Self {
        Self {
            pending: HashMap::new(),
            max_message_bytes,
        }
    }

    /// Returns the complete message payload once all fragments arrived, or
    /// `None` while more fragments are expected.
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

    pub fn drop_stream(&mut self, stream_id: u32) {
        self.pending
            .retain(|(pending_stream, _), _| *pending_stream != stream_id);
    }
}

/// Batch envelope encoder (mirror of tunnel-codec.ts `encodeFrameBatch`). Only
/// used when both peers negotiated `batch`. One encrypted WS message still
/// equals one `encrypt()` call — this only changes how many tunnel frames it
/// carries.
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
fn is_buffered_frame_type(frame_type: u8) -> bool {
    matches!(
        frame_type,
        frame_type::HTTP_BODY | frame_type::WS_TEXT | frame_type::WS_BINARY
    )
}

// See the TS mirror (tunnel-codec.ts) for the 150ms rationale: the chat render
// pipeline's 100ms input throttle + ~64ms paced-reveal smoothing make a 150ms
// batch window invisible.
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 150;
pub const DEFAULT_BATCH_MAX_BYTES: usize = 24 * 1024;
pub const DEFAULT_BATCH_MAX_FRAMES: usize = 32;

type SendBatchFn = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// Outbound batching buffer (mirror of tunnel-codec.ts
/// `createOutboundFrameBatcher`). Runs as one tokio task: `enqueue` hands
/// frames to the task; the task applies the JS flush rules (immediate flush
/// for non-buffered frame types, window/byte/frame caps otherwise).
///
/// Dropping the handle (or calling [`OutboundFrameBatcher::dispose`]) discards
/// any buffered frames without flushing, mirroring JS `dispose()`.
#[derive(Clone)]
pub struct OutboundFrameBatcher {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl OutboundFrameBatcher {
    pub fn start(window_ms: u64, send_batch: SendBatchFn) -> Self {
        Self::with_limits(
            window_ms,
            DEFAULT_BATCH_MAX_BYTES,
            DEFAULT_BATCH_MAX_FRAMES,
            send_batch,
        )
    }

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

    pub fn enqueue(&self, frame: Vec<u8>) {
        let _ = self.tx.send(frame);
    }

    /// Flush any buffered frames now (JS `flush()`). Fire-and-forget like the
    /// JS sync method; tests await the window or a follow-up send.
    pub fn flush(&self) {
        let _ = self.tx.send(FLUSH_MARKER.to_vec());
    }

    /// Discard buffered frames and stop batching (JS `dispose`).
    pub fn dispose(self) {
        // Dropping the sender ends the task; the run loop discards its buffer
        // on channel close instead of flushing.
    }
}

/// Sentinel payload signalling an explicit flush request (empty frame is
/// otherwise a valid — if unusual — tunnel frame).
static FLUSH_MARKER: &[u8] = b"\x00__relay_flush__";

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
