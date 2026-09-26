//! The streaming-transcription-session contract shared by the dictation
//! stream manager and its providers (`openai-compatible-session.js`,
//! `local/worker-client.js` in the JS).
//!
//! A session buffers appended PCM16 audio, transcribes each committed
//! segment exactly once, and reports outcomes through an unbounded event
//! channel (the JS `EventEmitter` `committed` / `transcript` / `error`
//! events). Delivery is asynchronous: events arrive after an async hop, so
//! the manager re-checks stream state on arrival.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

/// Events a session emits (the JS `session.emit(...)` payloads).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// `committed { segmentId, previousSegmentId }`.
    Committed {
        segment_id: String,
        previous_segment_id: Option<String>,
    },
    /// `transcript { segmentId, transcript, isFinal }`.
    Transcript {
        segment_id: String,
        transcript: String,
        is_final: bool,
    },
    /// `error (Error)` — message only crosses the wire.
    Error { message: String },
}

/// The session contract consumed by [`super::stream_manager`].
pub trait StreamingTranscriptionSession: Send {
    fn required_sample_rate(&self) -> u32;
    fn append_pcm16(&mut self, chunk: Vec<u8>);
    /// Requests a commit; the segment acknowledgment arrives as a
    /// [`SessionEvent::Committed`].
    fn commit(&mut self);
    fn clear(&mut self);
    fn close(&mut self);
}

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

/// Outcome of `createSttSession`: a connected session plus its event stream,
/// or the readiness error shape the WS protocol reports.
pub enum SttSessionOutcome {
    Session {
        session: Box<dyn StreamingTranscriptionSession>,
        events: mpsc::UnboundedReceiver<SessionEvent>,
    },
    NotReady {
        error: String,
        retryable: bool,
        reason_code: Option<String>,
    },
}

pub type CreateSttSession =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, SttSessionOutcome> + Send + Sync>;
