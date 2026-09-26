//! Port of `server/lib/dictation/stream-manager.js` — the
//! server-authoritative streaming dictation state machine. One manager owns
//! all dictation streams for a single WebSocket connection.
//!
//! Responsibilities (from the JS): reorders inbound chunks by `seq` and
//! acks the highest contiguous seq; resamples client PCM to the provider's
//! required rate; segments long dictations at natural pauses (silence-only
//! segments are cleared, never committed); concatenates per-segment
//! transcripts into live partials and emits the final text once every
//! committed segment has a final transcript; applies an adaptive
//! finalization timeout budget based on pending work.
//!
//! Concurrency mapping: the JS single-threaded event loop becomes a
//! `Mutex<HashMap<dictationId, StreamEntry>>`. Emissions are channel sends
//! (lock-free), so every mutation happens under one lock scope; session
//! events arrive on a per-session pump task applying them under the same
//! lock (a generation guard replaces the JS `if (!state) return` staleness
//! check); the `setTimeout` finalization budget becomes an abortable tokio
//! task capturing the manager `Arc`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::audio::{Pcm16MonoResampler, parse_pcm_rate_from_format, pcm16le_peak_abs};
use super::session::{
    CreateSttSession, SessionEvent, StreamingTranscriptionSession, SttSessionOutcome,
};
use crate::dictation_tts::tts::stt::lenient_base64_decode;

const DEFAULT_FINAL_TIMEOUT_MS: u64 = 10_000;
// Parakeet is a full-attention conformer: decode cost and peak memory grow
// quadratically with segment length (measured: 60s -> 2.1s/+90MB,
// 300s -> 21.3s/+1.5GB). Segmenting keeps a long dictation off that curve
// and lets committed segments decode while the user is still speaking.
// Typical dictations are shorter than the minimum and are decoded as one
// segment.
const DEFAULT_SEGMENT_MIN_SECONDS: f64 = 60.0;
const DEFAULT_SEGMENT_MAX_SECONDS: f64 = 90.0;
const FINAL_TIMEOUT_MAX_MS: u64 = 5 * 60 * 1000;
const FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS: u64 = 15 * 1000;
const FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS: u64 = 1500;
const FINAL_TIMEOUT_PER_MISSING_SEQ_MS: u64 = 250;
const SILENCE_PEAK_THRESHOLD: i32 = 300;

fn seconds_to_pcm16_bytes(seconds: f64, sample_rate: u32) -> u64 {
    if seconds > 0.0 {
        (seconds * sample_rate as f64 * 2.0).round().max(1.0) as u64
    } else {
        0
    }
}

/// Split the current segment once it is long enough to be worth decoding on
/// its own and the speaker has just gone quiet, or unconditionally at the
/// hard cap. Client chunks are ~1s, so a quiet chunk is roughly a second of
/// silence — long enough to be a sentence boundary rather than a gap
/// between words.
fn should_split_segment(state: &StreamState) -> bool {
    if state.segment_max_bytes > 0 && state.bytes_since_commit >= state.segment_max_bytes {
        return true;
    }
    if state.segment_min_bytes == 0 || state.bytes_since_commit < state.segment_min_bytes {
        return false;
    }
    state.last_chunk_peak < SILENCE_PEAK_THRESHOLD
}

struct StreamState {
    session: Option<Box<dyn StreamingTranscriptionSession>>,
    output_rate: u32,
    resampler: Option<Pcm16MonoResampler>,
    received_chunks: HashMap<i64, Vec<u8>>,
    next_seq_to_forward: i64,
    ack_seq: i64,
    segment_min_bytes: u64,
    segment_max_bytes: u64,
    bytes_since_commit: u64,
    peak_since_commit: i32,
    last_chunk_peak: i32,
    committed_segment_ids: Vec<String>,
    /// Insertion-ordered segment transcripts (the JS `Map`).
    transcripts_by_segment_id: Vec<(String, String)>,
    final_transcript_segment_ids: HashSet<String>,
    pending_commits: u64,
    finish_requested: bool,
    finish_sealed: bool,
    /// `null` until `finish` arrives; f64 because the JS stores the raw
    /// JSON number (the transport validates `typeof === 'number'` only).
    final_seq: Option<f64>,
    final_timeout: Option<tokio::task::JoinHandle<()>>,
}

impl StreamState {
    fn transcript_for(&self, segment_id: &str) -> &str {
        self.transcripts_by_segment_id
            .iter()
            .find(|(id, _)| id == segment_id)
            .map(|(_, text)| text.as_str())
            .unwrap_or("")
    }

    /// `Map.set` semantics: replace in place, or append.
    fn set_transcript(&mut self, segment_id: String, transcript: String) {
        if let Some(slot) = self
            .transcripts_by_segment_id
            .iter_mut()
            .find(|(id, _)| *id == segment_id)
        {
            slot.1 = transcript;
        } else {
            self.transcripts_by_segment_id
                .push((segment_id, transcript));
        }
    }

    fn drop_uncommitted_non_final_transcripts(&mut self) {
        let committed: HashSet<&str> = self
            .committed_segment_ids
            .iter()
            .map(String::as_str)
            .collect();
        self.transcripts_by_segment_id.retain(|(segment_id, _)| {
            committed.contains(segment_id.as_str())
                || self.final_transcript_segment_ids.contains(segment_id)
        });
    }

    fn ordered_segment_ids(&self) -> Vec<String> {
        let committed: HashSet<&str> = self
            .committed_segment_ids
            .iter()
            .map(String::as_str)
            .collect();
        let mut ordered = self.committed_segment_ids.clone();
        for (segment_id, _) in &self.transcripts_by_segment_id {
            if !committed.contains(segment_id.as_str()) {
                ordered.push(segment_id.clone());
            }
        }
        ordered
    }
}

struct StreamEntry {
    generation: u64,
    state: StreamState,
}

fn close_entry(entry: &mut StreamEntry) {
    if let Some(handle) = entry.state.final_timeout.take() {
        handle.abort();
    }
    if let Some(mut session) = entry.state.session.take() {
        session.close();
    }
}

type Streams = Mutex<HashMap<String, StreamEntry>>;

fn lock(streams: &Streams) -> MutexGuard<'_, HashMap<String, StreamEntry>> {
    streams.lock().unwrap_or_else(|e| e.into_inner())
}

/// `DictationStreamManager`. The segment bounds and timeout budget stay
/// assignable for the same reasons the JS leaves them assignable (tests and
/// future tuning).
pub struct DictationStreamManager {
    emit_tx: mpsc::UnboundedSender<Value>,
    create_stt_session: CreateSttSession,
    pub final_timeout_ms: u64,
    pub segment_min_seconds: f64,
    pub segment_max_seconds: f64,
    streams: Streams,
    generations: AtomicU64,
}

impl DictationStreamManager {
    /// Same, with explicit tuning (the JS tests assign these fields).
    pub fn with_bounds(
        emit_tx: mpsc::UnboundedSender<Value>,
        create_stt_session: CreateSttSession,
        final_timeout_ms: u64,
        segment_min_seconds: f64,
        segment_max_seconds: f64,
    ) -> Arc<Self> {
        Arc::new(Self {
            emit_tx,
            create_stt_session,
            final_timeout_ms,
            segment_min_seconds,
            segment_max_seconds,
            streams: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(1),
        })
    }

    /// Construct the shared manager (an `Arc` because session pumps and the
    /// finalization timeout capture it, like the JS closures capture
    /// `this`).
    pub fn new(
        emit_tx: mpsc::UnboundedSender<Value>,
        create_stt_session: CreateSttSession,
    ) -> Arc<Self> {
        Arc::new(Self {
            emit_tx,
            create_stt_session,
            final_timeout_ms: DEFAULT_FINAL_TIMEOUT_MS,
            segment_min_seconds: DEFAULT_SEGMENT_MIN_SECONDS,
            segment_max_seconds: DEFAULT_SEGMENT_MAX_SECONDS,
            streams: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(1),
        })
    }

    fn emit(&self, message: Value) {
        let _ = self.emit_tx.send(message);
    }

    fn emit_ack(&self, dictation_id: &str, ack_seq: i64) {
        self.emit(json!({ "type": "ack", "dictationId": dictation_id, "ackSeq": ack_seq }));
    }

    fn fail_stream(
        &self,
        dictation_id: &str,
        error: &str,
        retryable: bool,
        reason_code: Option<&str>,
    ) {
        let mut payload = json!({
            "type": "error",
            "dictationId": dictation_id,
            "error": error,
            "retryable": retryable,
        });
        if let Some(reason_code) = reason_code {
            payload["reasonCode"] = Value::String(reason_code.to_string());
        }
        self.emit(payload);
    }

    /// Lock-free emit, then remove the stream (may be called with the map
    /// lock held by the caller — cleanup itself runs after release).
    fn cleanup_stream(&self, dictation_id: &str) {
        let mut streams = lock(&self.streams);
        if let Some(mut entry) = streams.remove(dictation_id) {
            close_entry(&mut entry);
        }
    }

    fn fail_and_cleanup_stream(&self, dictation_id: &str, error: &str, retryable: bool) {
        self.fail_stream(dictation_id, error, retryable, None);
        self.cleanup_stream(dictation_id);
    }

    pub fn cleanup_all(&self) {
        let ids: Vec<String> = lock(&self.streams).keys().cloned().collect();
        for id in ids {
            self.cleanup_stream(&id);
        }
    }

    /// `handleStart`: `format` e.g. `"audio/pcm;rate=16000;bits=16"`;
    /// `start_options` are forwarded to `createSttSession`.
    pub async fn handle_start(
        self: &Arc<Self>,
        dictation_id: String,
        format: String,
        start_options: Value,
    ) {
        self.cleanup_stream(&dictation_id);
        let generation = self.generations.fetch_add(1, Ordering::SeqCst);

        let input_rate = parse_pcm_rate_from_format(&format, Some(16000)).unwrap_or(16000);
        if input_rate == 0 {
            self.fail_stream(
                &dictation_id,
                &format!("Invalid dictation input rate in format: {format}"),
                false,
                None,
            );
            return;
        }

        let resolved = (self.create_stt_session)(start_options).await;
        let (session, mut events) = match resolved {
            SttSessionOutcome::Session { session, events } => (session, events),
            SttSessionOutcome::NotReady {
                error,
                retryable,
                reason_code,
            } => {
                self.fail_stream(
                    &dictation_id,
                    &if error.is_empty() {
                        "Dictation STT not configured".to_string()
                    } else {
                        error
                    },
                    retryable,
                    reason_code.as_deref(),
                );
                return;
            }
        };

        let output_rate = session.required_sample_rate();
        let resampler = if input_rate == output_rate {
            None
        } else {
            Some(Pcm16MonoResampler::new(input_rate, output_rate))
        };

        lock(&self.streams).insert(
            dictation_id.clone(),
            StreamEntry {
                generation,
                state: StreamState {
                    session: Some(session),
                    output_rate,
                    resampler,
                    received_chunks: HashMap::new(),
                    next_seq_to_forward: 0,
                    ack_seq: -1,
                    segment_min_bytes: seconds_to_pcm16_bytes(
                        self.segment_min_seconds,
                        output_rate,
                    ),
                    segment_max_bytes: seconds_to_pcm16_bytes(
                        self.segment_max_seconds,
                        output_rate,
                    ),
                    bytes_since_commit: 0,
                    peak_since_commit: 0,
                    last_chunk_peak: 0,
                    committed_segment_ids: Vec::new(),
                    transcripts_by_segment_id: Vec::new(),
                    final_transcript_segment_ids: HashSet::new(),
                    pending_commits: 0,
                    finish_requested: false,
                    finish_sealed: false,
                    final_seq: None,
                    final_timeout: None,
                },
            },
        );
        self.emit_ack(&dictation_id, -1);

        // The session event pump: applies committed/transcript/error events
        // to the manager state (generation-guarded against stale sessions).
        let manager = Arc::clone(self);
        let pump_id = dictation_id.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                manager.apply_session_event(&pump_id, generation, event);
            }
        });
    }

    /// One session event arriving (the JS `stt.on(...)` handlers).
    fn apply_session_event(&self, dictation_id: &str, generation: u64, event: SessionEvent) {
        match event {
            SessionEvent::Committed { segment_id, .. } => {
                {
                    let mut streams = lock(&self.streams);
                    let Some(entry) = live_entry(&mut streams, dictation_id, generation) else {
                        return;
                    };
                    let state = &mut entry.state;
                    // Segment accounting is reset where the commit is issued,
                    // not here: this event arrives after an async hop, and
                    // zeroing the counters on arrival would discard audio
                    // that came in meanwhile.
                    state.committed_segment_ids.push(segment_id);
                    state.pending_commits = state.pending_commits.saturating_sub(1);
                }
                self.maybe_finalize_stream(dictation_id);
            }
            SessionEvent::Transcript {
                segment_id,
                transcript,
                is_final,
            } => {
                {
                    let mut streams = lock(&self.streams);
                    let Some(entry) = live_entry(&mut streams, dictation_id, generation) else {
                        return;
                    };
                    let state = &mut entry.state;
                    state.set_transcript(segment_id.clone(), transcript);
                    if is_final {
                        state
                            .final_transcript_segment_ids
                            .insert(segment_id.clone());
                    }
                    let committed = state.committed_segment_ids.contains(&segment_id);
                    let ordered_ids = if committed {
                        state.committed_segment_ids.clone()
                    } else {
                        let mut ids = state.committed_segment_ids.clone();
                        ids.push(segment_id);
                        ids
                    };
                    let partial_text = ordered_ids
                        .iter()
                        .map(|id| state.transcript_for(id).to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string();
                    self.emit(json!({
                        "type": "partial",
                        "dictationId": dictation_id,
                        "text": partial_text,
                    }));
                }
                self.maybe_seal_stream_finish(dictation_id);
                self.maybe_finalize_stream(dictation_id);
            }
            SessionEvent::Error { message } => {
                self.fail_and_cleanup_stream(dictation_id, &message, true);
            }
        }
    }

    /// `handleChunk`. `seq` is validated as a non-negative integer by the
    /// transport (the JS re-checks `Number.isInteger` here).
    pub fn handle_chunk(&self, dictation_id: &str, seq: i64, audio_base64: &str) {
        let mut failed: Option<String> = None;
        let ack_seq;
        {
            let mut streams = lock(&self.streams);
            let Some(entry) = streams.get_mut(dictation_id) else {
                drop(streams);
                self.fail_stream(dictation_id, "Dictation stream not started", true, None);
                return;
            };
            let state = &mut entry.state;
            if seq < state.next_seq_to_forward {
                ack_seq = state.ack_seq;
            } else {
                state.received_chunks.entry(seq).or_insert_with(|| {
                    let mut chunk = lenient_base64_decode(audio_base64);
                    if !chunk.len().is_multiple_of(2) {
                        chunk.pop();
                    }
                    chunk
                });
                while let Some(pcm16) = state.received_chunks.remove(&state.next_seq_to_forward) {
                    let resampled = match state.resampler.as_mut() {
                        Some(resampler) => match resampler.process_chunk(&pcm16) {
                            Ok(resampled) => resampled,
                            Err(error) => {
                                failed = Some(error);
                                break;
                            }
                        },
                        None => pcm16.clone(),
                    };
                    if !resampled.is_empty() {
                        if let Some(session) = state.session.as_mut() {
                            session.append_pcm16(resampled.clone());
                        }
                        state.bytes_since_commit += resampled.len() as u64;
                        state.last_chunk_peak = match pcm16le_peak_abs(&resampled) {
                            Ok(peak) => peak,
                            Err(error) => {
                                failed = Some(error);
                                break;
                            }
                        };
                        state.peak_since_commit =
                            state.peak_since_commit.max(state.last_chunk_peak);
                        if let Some(error) = self.maybe_auto_commit_segment(state) {
                            failed = Some(error);
                            break;
                        }
                    }
                    state.next_seq_to_forward += 1;
                    state.ack_seq = state.next_seq_to_forward - 1;
                }
                ack_seq = state.ack_seq;
            }

            if let Some(error) = failed.clone() {
                close_entry(entry);
                streams.remove(dictation_id);
                drop(streams);
                self.fail_stream(dictation_id, &error, true, None);
                return;
            }
        }
        self.emit_ack(dictation_id, ack_seq);
        self.maybe_seal_stream_finish(dictation_id);
        self.maybe_finalize_stream(dictation_id);
    }

    /// `handleFinish` — `final_seq` is the highest seq the client sent (or
    /// -1 if none).
    pub fn handle_finish(self: &Arc<Self>, dictation_id: &str, final_seq: f64) {
        {
            let mut streams = lock(&self.streams);
            let Some(entry) = streams.get_mut(dictation_id) else {
                drop(streams);
                self.fail_stream(dictation_id, "Dictation stream not started", true, None);
                return;
            };
            let state = &mut entry.state;
            state.finish_requested = true;
            state.final_seq = Some(final_seq);

            if final_seq >= 0.0
                && state.ack_seq < 0
                && state.next_seq_to_forward == 0
                && state.received_chunks.is_empty()
            {
                drop(streams);
                self.fail_stream(
                    dictation_id,
                    "Dictation finished but no audio chunks were received",
                    true,
                    None,
                );
                self.cleanup_stream(dictation_id);
                return;
            }
        }

        self.maybe_seal_stream_finish(dictation_id);
        self.maybe_finalize_stream(dictation_id);

        let timeout_ms = {
            let mut streams = lock(&self.streams);
            let Some(entry) = streams.get_mut(dictation_id) else {
                return;
            };
            let timeout_ms = self.estimate_finalization_timeout(&entry.state);
            if let Some(handle) = entry.state.final_timeout.take() {
                handle.abort();
            }
            let id = dictation_id.to_string();
            let generation = entry.generation;
            let manager = Arc::clone(self);
            entry.state.final_timeout = Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                // A cleanup between schedule and fire removes the entry (or
                // a restart bumps the generation); both mean the stream is
                // no longer the one this budget was computed for.
                let stale = {
                    let streams = lock(&manager.streams);
                    !streams
                        .get(&id)
                        .is_some_and(|entry| entry.generation == generation)
                };
                if !stale {
                    manager.fail_and_cleanup_stream(
                        &id,
                        "Timed out waiting for final transcription",
                        true,
                    );
                }
            }));
            timeout_ms
        };

        self.emit(json!({
            "type": "finish_accepted",
            "dictationId": dictation_id,
            "timeoutMs": timeout_ms,
        }));
    }

    pub fn handle_cancel(&self, dictation_id: &str) {
        self.cleanup_stream(dictation_id);
    }

    fn estimate_finalization_timeout(&self, state: &StreamState) -> u64 {
        let bytes_per_second = (state.output_rate as u64 * 2).max(1);
        let pending_committed_segments = state
            .committed_segment_ids
            .iter()
            .filter(|segment_id| !state.final_transcript_segment_ids.contains(*segment_id))
            .count() as u64;
        let committed: HashSet<&str> = state
            .committed_segment_ids
            .iter()
            .map(String::as_str)
            .collect();
        let pending_uncommitted_transcript_segments = state
            .transcripts_by_segment_id
            .iter()
            .filter(|(segment_id, _)| {
                !committed.contains(segment_id.as_str())
                    && !state.final_transcript_segment_ids.contains(segment_id)
            })
            .count() as u64;
        let pending_segments = pending_committed_segments
            + pending_uncommitted_transcript_segments
            + state.pending_commits;
        let pending_audio_seconds = state.bytes_since_commit.div_ceil(bytes_per_second);
        let missing_seq_count = state
            .final_seq
            .map(|final_seq| (final_seq - state.ack_seq as f64).max(0.0) as u64)
            .unwrap_or(0);

        let extra_ms = pending_segments * FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS
            + pending_audio_seconds * FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS
            + missing_seq_count * FINAL_TIMEOUT_PER_MISSING_SEQ_MS;

        self.final_timeout_ms
            .max((self.final_timeout_ms + extra_ms).min(FINAL_TIMEOUT_MAX_MS))
    }

    fn maybe_auto_commit_segment(&self, state: &mut StreamState) -> Option<String> {
        if state.finish_requested {
            return None;
        }
        if !should_split_segment(state) {
            return None;
        }
        if state.peak_since_commit < SILENCE_PEAK_THRESHOLD {
            if let Some(session) = state.session.as_mut() {
                session.clear();
            }
            state.bytes_since_commit = 0;
            state.peak_since_commit = 0;
            state.last_chunk_peak = 0;
            return None;
        }

        state.bytes_since_commit = 0;
        state.peak_since_commit = 0;
        state.last_chunk_peak = 0;
        self.commit_segment(state)
    }

    /// Issue a commit and record it as in flight. The session acknowledges
    /// with a `committed` event; until then the manager must not finalize,
    /// or the segment's transcript would be missing from the final text.
    fn commit_segment(&self, state: &mut StreamState) -> Option<String> {
        state.pending_commits += 1;
        if let Some(session) = state.session.as_mut() {
            session.commit();
        }
        None
    }

    fn maybe_seal_stream_finish(&self, dictation_id: &str) {
        let mut failed: Option<String> = None;
        {
            let mut streams = lock(&self.streams);
            let Some(entry) = streams.get_mut(dictation_id) else {
                return;
            };
            let state = &mut entry.state;
            let Some(final_seq) = state.final_seq else {
                return;
            };
            if !state.finish_requested || (state.ack_seq as f64) < final_seq {
                return;
            }
            if state.finish_sealed {
                return;
            }

            if state.bytes_since_commit > 0 {
                if state.peak_since_commit < SILENCE_PEAK_THRESHOLD {
                    if let Some(session) = state.session.as_mut() {
                        session.clear();
                    }
                    state.bytes_since_commit = 0;
                    state.peak_since_commit = 0;
                    state.last_chunk_peak = 0;
                    state.drop_uncommitted_non_final_transcripts();
                } else {
                    state.bytes_since_commit = 0;
                    state.peak_since_commit = 0;
                    state.last_chunk_peak = 0;
                    if let Some(error) = self.commit_segment(state) {
                        failed = Some(error);
                    }
                }
            }

            if failed.is_none() {
                state.finish_sealed = true;
            }
        }
        if let Some(error) = failed {
            self.fail_and_cleanup_stream(dictation_id, &error, true);
        }
    }

    fn maybe_finalize_stream(&self, dictation_id: &str) {
        let final_text: Option<String> = {
            let mut streams = lock(&self.streams);
            let Some(entry) = streams.get_mut(dictation_id) else {
                return;
            };
            let state = &mut entry.state;
            let Some(final_seq) = state.final_seq else {
                return;
            };
            if !state.finish_requested || (state.ack_seq as f64) < final_seq {
                return;
            }
            if state.pending_commits > 0 {
                return;
            }

            let ordered_segment_ids = state.ordered_segment_ids();
            if ordered_segment_ids.is_empty() {
                Some(String::new())
            } else if ordered_segment_ids
                .iter()
                .all(|segment_id| state.final_transcript_segment_ids.contains(segment_id))
            {
                Some(
                    ordered_segment_ids
                        .iter()
                        .map(|segment_id| state.transcript_for(segment_id).to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string(),
                )
            } else {
                None
            }
        };

        if let Some(text) = final_text {
            self.emit(json!({
                "type": "final",
                "dictationId": dictation_id,
                "text": text,
            }));
            self.cleanup_stream(dictation_id);
        }
    }
}

fn live_entry<'a>(
    streams: &'a mut HashMap<String, StreamEntry>,
    dictation_id: &str,
    generation: u64,
) -> Option<&'a mut StreamEntry> {
    streams
        .get_mut(dictation_id)
        .filter(|entry| entry.generation == generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    const FORMAT: &str = "audio/pcm;rate=16000;bits=16";

    struct FakeState {
        appended: Vec<Vec<u8>>,
        commits: usize,
        clears: usize,
        closed: bool,
        segment_counter: usize,
    }

    struct FakeSttSession {
        state: Arc<Mutex<FakeState>>,
        events: mpsc::UnboundedSender<SessionEvent>,
        transcript_by_segment: Arc<dyn Fn(usize) -> String + Send + Sync>,
    }

    impl StreamingTranscriptionSession for FakeSttSession {
        fn required_sample_rate(&self) -> u32 {
            16000
        }

        fn append_pcm16(&mut self, chunk: Vec<u8>) {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .appended
                .push(chunk);
        }

        fn commit(&mut self) {
            let (segment_id, transcript) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.commits += 1;
                let segment_id = format!("seg-{}", state.segment_counter);
                state.segment_counter += 1;
                let transcript = (self.transcript_by_segment)(state.segment_counter - 1);
                (segment_id, transcript)
            };
            let _ = self.events.send(SessionEvent::Committed {
                segment_id: segment_id.clone(),
                previous_segment_id: None,
            });
            // setTimeout(0) before the transcript, like the fake in the JS
            // suite.
            let events = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                let _ = events.send(SessionEvent::Transcript {
                    segment_id,
                    transcript,
                    is_final: true,
                });
            });
        }

        fn clear(&mut self) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).clears += 1;
        }

        fn close(&mut self) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        }
    }

    fn loud_chunk_base64(samples: usize, amplitude: i16) -> String {
        let arr: Vec<i16> = (0..samples)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect();
        let bytes: Vec<u8> = arr.iter().flat_map(|s| s.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn silent_chunk_base64(samples: usize) -> String {
        let bytes = vec![0u8; samples * 2];
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    struct Fixture {
        manager: Arc<DictationStreamManager>,
        messages: Arc<Mutex<Vec<Value>>>,
        state: Arc<Mutex<FakeState>>,
    }

    #[allow(clippy::too_many_arguments)]
    fn fixture_with_bounds(
        transcript_by_segment: Arc<dyn Fn(usize) -> String + Send + Sync>,
        final_timeout_ms: u64,
        segment_min_seconds: f64,
        segment_max_seconds: f64,
    ) -> Fixture {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&messages);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            }
        });
        let state = Arc::new(Mutex::new(FakeState {
            appended: Vec::new(),
            commits: 0,
            clears: 0,
            closed: false,
            segment_counter: 0,
        }));
        let session_state = Arc::clone(&state);
        let create: CreateSttSession = Arc::new(move |_options| {
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            let session = FakeSttSession {
                state: Arc::clone(&session_state),
                events: event_tx,
                transcript_by_segment: Arc::clone(&transcript_by_segment),
            };
            Box::pin(async move {
                SttSessionOutcome::Session {
                    session: Box::new(session),
                    events: event_rx,
                }
            })
        });
        let manager = DictationStreamManager::with_bounds(
            tx,
            create,
            final_timeout_ms,
            segment_min_seconds,
            segment_max_seconds,
        );
        Fixture {
            manager,
            messages,
            state,
        }
    }

    fn fixture_with(transcript_by_segment: Arc<dyn Fn(usize) -> String + Send + Sync>) -> Fixture {
        fixture_with_bounds(transcript_by_segment, 10_000, 60.0, 90.0)
    }

    fn fixture() -> Fixture {
        fixture_with(Arc::new(|_| "hello world".to_string()))
    }

    async fn wait_for(messages: &Arc<Mutex<Vec<Value>>>, predicate: impl Fn(&Value) -> bool) {
        wait_for_within(messages, predicate, 500).await;
    }

    async fn wait_for_within(
        messages: &Arc<Mutex<Vec<Value>>>,
        predicate: impl Fn(&Value) -> bool,
        attempts: usize,
    ) {
        for _ in 0..attempts {
            if messages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(&predicate)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
        panic!("waitFor timed out");
    }

    fn messages_of(fixture: &Fixture) -> Vec<Value> {
        fixture
            .messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn of_type<'a>(fixture: &'a Fixture, kind: &str) -> Vec<Value> {
        messages_of(fixture)
            .into_iter()
            .filter(|message| message["type"] == kind)
            .collect()
    }

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_id(label: &str) -> String {
        format!("{label}-{}", SEQ.fetch_add(1, AtomicOrdering::SeqCst))
    }

    #[tokio::test]
    async fn transcribes_ordered_chunks_and_emits_final_text() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(1600, 8000));
        fixture
            .manager
            .handle_chunk(&id, 1, &loud_chunk_base64(1600, 8000));
        fixture.manager.handle_finish(&id, 1.0);

        wait_for(&fixture.messages, |m| m["type"] == "final").await;

        let final_message = of_type(&fixture, "final").remove(0);
        assert_eq!(final_message["text"], "hello world");
        let state = fixture.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.commits, 1);
        assert!(state.closed);
        drop(state);
        let acks = of_type(&fixture, "ack");
        assert_eq!(acks.last().unwrap()["ackSeq"], 1);
    }

    #[tokio::test]
    async fn reorders_out_of_order_chunks_before_appending() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 1, &loud_chunk_base64(1600, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .appended
                .len(),
            0
        );
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(1600, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .appended
                .len(),
            2
        );
        fixture.manager.handle_finish(&id, 1.0);
        wait_for(&fixture.messages, |m| m["type"] == "final").await;
    }

    #[tokio::test]
    async fn clears_silence_only_tails_instead_of_committing() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &silent_chunk_base64(1600));
        fixture.manager.handle_finish(&id, 0.0);

        wait_for(&fixture.messages, |m| m["type"] == "final").await;

        let final_message = of_type(&fixture, "final").remove(0);
        assert_eq!(final_message["text"], "");
        let state = fixture.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.commits, 0);
        assert_eq!(state.clears, 1);
    }

    #[tokio::test]
    async fn fails_fast_when_finish_arrives_with_no_chunks() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture.manager.handle_finish(&id, 3.0);
        wait_for(&fixture.messages, |m| m["type"] == "error").await;

        let error = of_type(&fixture, "error")
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                panic!("expected an error message");
            });
        assert_eq!(error["retryable"], true);
        assert!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
        );
    }

    #[tokio::test]
    async fn reports_provider_readiness_errors_from_create_stt_session() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&messages);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            }
        });
        let create: CreateSttSession = Arc::new(|_options| {
            Box::pin(async {
                SttSessionOutcome::NotReady {
                    error: "Dictation model is downloading".to_string(),
                    retryable: true,
                    reason_code: Some("model_download_in_progress".to_string()),
                }
            })
        });
        let manager = DictationStreamManager::new(tx, create);
        manager
            .clone()
            .handle_start("d1".to_string(), FORMAT.to_string(), json!({}))
            .await;
        wait_for(&messages, |m| m["type"] == "error").await;
        let error = messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|message| message["type"] == "error")
            .cloned()
            .unwrap();
        assert_eq!(error["reasonCode"], "model_download_in_progress");
        assert_eq!(error["retryable"], true);
    }

    #[tokio::test]
    async fn emits_partials_as_segment_transcripts_arrive() {
        let counter = Arc::new(AtomicUsize::new(0));
        let transcripts = {
            let counter = Arc::clone(&counter);
            Arc::new(move |_| -> String {
                let segment = counter.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                if segment == 1 {
                    "first part".to_string()
                } else {
                    "second part".to_string()
                }
            })
        };
        let fixture = fixture_with_bounds(transcripts, 10_000, 60.0, 0.05);

        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(1600, 8000));
        wait_for(&fixture.messages, |m| m["type"] == "partial").await;
        fixture
            .manager
            .handle_chunk(&id, 1, &loud_chunk_base64(1600, 8000));
        fixture.manager.handle_finish(&id, 1.0);

        wait_for(&fixture.messages, |m| m["type"] == "final").await;

        let final_message = of_type(&fixture, "final").remove(0);
        assert_eq!(final_message["text"], "first part second part");
        let partials = of_type(&fixture, "partial");
        assert!(!partials.is_empty());
    }

    #[tokio::test]
    async fn keeps_a_short_dictation_as_one_segment_across_pauses() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(16000, 8000));
        fixture
            .manager
            .handle_chunk(&id, 1, &silent_chunk_base64(16000));
        fixture
            .manager
            .handle_chunk(&id, 2, &loud_chunk_base64(16000, 8000));

        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            0
        );

        fixture.manager.handle_finish(&id, 2.0);
        // handleFinish seals synchronously: the tail commits immediately.
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            1
        );
    }

    #[tokio::test]
    async fn splits_at_a_pause_once_the_segment_passes_the_minimum() {
        let fixture =
            fixture_with_bounds(Arc::new(|_| "hello world".to_string()), 10_000, 3.0, 90.0);

        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        // 1s of audio: below the minimum, so this pause must not split.
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(16000, 8000));
        fixture
            .manager
            .handle_chunk(&id, 1, &silent_chunk_base64(16000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            0
        );

        // Past the minimum, the next quiet chunk is a segment boundary.
        fixture
            .manager
            .handle_chunk(&id, 2, &loud_chunk_base64(16000, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            0
        );
        fixture
            .manager
            .handle_chunk(&id, 3, &silent_chunk_base64(16000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            1
        );
    }

    #[tokio::test]
    async fn splits_pauseless_speech_at_the_hard_cap() {
        let fixture =
            fixture_with_bounds(Arc::new(|_| "hello world".to_string()), 10_000, 60.0, 2.0);

        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(16000, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            0
        );
        fixture
            .manager
            .handle_chunk(&id, 1, &loud_chunk_base64(16000, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commits,
            1
        );
    }

    #[tokio::test]
    async fn clears_a_silence_only_segment_at_the_hard_cap() {
        let fixture =
            fixture_with_bounds(Arc::new(|_| "hello world".to_string()), 10_000, 60.0, 1.0);

        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &silent_chunk_base64(16000));

        let state = fixture.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.commits, 0);
        assert_eq!(state.clears, 1);
    }

    #[tokio::test]
    async fn chunk_before_start_fails_retryable() {
        let fixture = fixture();
        fixture
            .manager
            .handle_chunk("missing", 0, &loud_chunk_base64(10, 100));
        wait_for(&fixture.messages, |m| m["type"] == "error").await;
        let error = of_type(&fixture, "error").remove(0);
        assert_eq!(error["error"], "Dictation stream not started");
        assert_eq!(error["retryable"], true);
    }

    #[tokio::test]
    async fn stale_seq_re_ackes_without_appending() {
        let fixture = fixture();
        let id = unique_id("d");
        fixture
            .manager
            .clone()
            .handle_start(id.clone(), FORMAT.to_string(), json!({}))
            .await;
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(1600, 8000));
        fixture
            .manager
            .handle_chunk(&id, 0, &loud_chunk_base64(1600, 8000));
        assert_eq!(
            fixture
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .appended
                .len(),
            1
        );
        wait_for(&fixture.messages, |m| {
            m["type"] == "ack" && m["ackSeq"] == 0
        })
        .await;
        let acks = of_type(&fixture, "ack");
        assert_eq!(acks.last().unwrap()["ackSeq"], 0);
    }

    #[tokio::test]
    async fn finalization_timeout_fires_when_transcripts_never_arrive() {
        struct SilentSession {
            events: mpsc::UnboundedSender<SessionEvent>,
        }
        impl StreamingTranscriptionSession for SilentSession {
            fn required_sample_rate(&self) -> u32 {
                16000
            }
            fn append_pcm16(&mut self, _chunk: Vec<u8>) {}
            fn commit(&mut self) {
                let _ = self.events.send(SessionEvent::Committed {
                    segment_id: "seg-silent".to_string(),
                    previous_segment_id: None,
                });
                // No transcript, ever.
            }
            fn clear(&mut self) {}
            fn close(&mut self) {}
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&messages);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message);
            }
        });
        let create: CreateSttSession = Arc::new(move |_options| {
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            let session = SilentSession { events: event_tx };
            Box::pin(async move {
                SttSessionOutcome::Session {
                    session: Box::new(session),
                    events: event_rx,
                }
            })
        });

        let manager = DictationStreamManager::with_bounds(tx, create, 15, 60.0, 90.0);
        manager
            .clone()
            .handle_start("d-silent".to_string(), FORMAT.to_string(), json!({}))
            .await;
        manager.handle_chunk("d-silent", 0, &loud_chunk_base64(1600, 8000));
        // finalSeq beyond the received chunks: the client is gone, nothing
        // will ever arrive, and the missing-seq terms keep the finalization
        // budget small (~2.8s) instead of the 15s per-pending-segment term.
        manager.handle_finish("d-silent", 5.0);
        wait_for_within(&messages, |m| m["type"] == "error", 1500).await;
        let error = messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|message| message["type"] == "error")
            .cloned()
            .unwrap();
        assert_eq!(error["error"], "Timed out waiting for final transcription");
        assert_eq!(error["retryable"], true);
    }
}
