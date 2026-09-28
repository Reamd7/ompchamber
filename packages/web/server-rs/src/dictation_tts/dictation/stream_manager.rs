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
//!
//! 中文说明：服务端权威的流式听写状态机，一条 WebSocket 连接对应一个
//! 管理器实例。核心职责：按 seq 重排乱序 chunk 并 ack 最高连续 seq；
//! 把客户端 PCM 重采样到提供方采样率；在自然停顿处分段（纯静音段清空、
//! 绝不提交）；把分段转写拼接成实时 partial，全部提交段拿到 final 转写后
//! 发出最终文本；并按剩余工作量自适应地计算最终化超时预算。

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

/// 最终化超时的基础预算（毫秒）。
const DEFAULT_FINAL_TIMEOUT_MS: u64 = 10_000;
/// 分段最小长度（秒）：达到后遇到停顿才切分。
// Parakeet is a full-attention conformer: decode cost and peak memory grow
// quadratically with segment length (measured: 60s -> 2.1s/+90MB,
// 300s -> 21.3s/+1.5GB). Segmenting keeps a long dictation off that curve
// and lets committed segments decode while the user is still speaking.
// Typical dictations are shorter than the minimum and are decoded as one
// segment.
const DEFAULT_SEGMENT_MIN_SECONDS: f64 = 60.0;
/// 分段硬上限（秒）：达到即无条件切分，与停顿无关。
const DEFAULT_SEGMENT_MAX_SECONDS: f64 = 90.0;
/// 自适应最终化超时的总上限（5 分钟）。
const FINAL_TIMEOUT_MAX_MS: u64 = 5 * 60 * 1000;
/// 每个待完成分段追加的预算（毫秒）。
const FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS: u64 = 15 * 1000;
/// 每秒尚未提交的音频追加的预算（毫秒）。
const FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS: u64 = 1500;
/// 每个仍缺失的 seq（客户端可能还在补发）追加的预算（毫秒）。
const FINAL_TIMEOUT_PER_MISSING_SEQ_MS: u64 = 250;
/// PCM16 峰值低于该阈值视为静音。
const SILENCE_PEAK_THRESHOLD: i32 = 300;

/// 秒数换算成 PCM16 单声道字节数（每样本 2 字节）；正数至少折算为
/// 1 字节，非正数返回 0。
fn seconds_to_pcm16_bytes(seconds: f64, sample_rate: u32) -> u64 {
    if seconds > 0.0 {
        (seconds * sample_rate as f64 * 2.0).round().max(1.0) as u64
    } else {
        0
    }
}

/// 判断当前分段是否应切分：达到硬上限时无条件切；否则要求已达到最小
/// 长度且最近一个 chunk 足够安静（约一秒的静音≈句子边界而非词间空隙）。
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

/// 单条听写流的全部服务端状态（对应 JS stream-manager 的 state 对象）。
struct StreamState {
    /// STT 会话；清理后为 None。
    session: Option<Box<dyn StreamingTranscriptionSession>>,
    /// 提供方要求的采样率（重采样目标）。
    output_rate: u32,
    /// 输入→输出采样率重采样器；两者一致时为 None。
    resampler: Option<Pcm16MonoResampler>,
    /// 乱序到达、尚未能按 seq 连续转发的 chunk 缓冲。
    received_chunks: HashMap<i64, Vec<u8>>,
    /// 下一个待转发的 seq。
    next_seq_to_forward: i64,
    /// 已确认的最高连续 seq；-1 表示尚未收到任何 chunk。
    ack_seq: i64,
    /// 分段最小字节数；达到后遇静音才切分。
    segment_min_bytes: u64,
    /// 分段硬上限字节数；达到即无条件切分。
    segment_max_bytes: u64,
    /// 当前分段自上次提交/清空以来累计的字节数。
    bytes_since_commit: u64,
    /// 当前分段内的音频峰值。
    peak_since_commit: i32,
    /// 最近一个转发 chunk 的峰值，用于静音判定。
    last_chunk_peak: i32,
    /// 已提交分段的 id（按提交顺序）。
    committed_segment_ids: Vec<String>,
    /// 插入序保存的分段转写。
    /// Insertion-ordered segment transcripts (the JS `Map`).
    transcripts_by_segment_id: Vec<(String, String)>,
    /// 已拿到 final 转写的分段 id 集合。
    final_transcript_segment_ids: HashSet<String>,
    /// 已发出 commit、尚未收到 committed 事件的次数。
    pending_commits: u64,
    /// 客户端已发送 finish。
    finish_requested: bool,
    /// finish 已封板（尾部音频已清空或提交）。
    finish_sealed: bool,
    /// `final_seq`：finish 携带的最高 seq，原样保存 JSON 数字。
    /// `null` until `finish` arrives; f64 because the JS stores the raw
    /// JSON number (the transport validates `typeof === 'number'` only).
    final_seq: Option<f64>,
    /// 最终化超时任务句柄；重新调度或清理时会被 abort。
    final_timeout: Option<tokio::task::JoinHandle<()>>,
}

/// 分段转写的存取、裁剪与排序辅助。
impl StreamState {
    /// 取指定分段的转写；无记录时返回空串。
    fn transcript_for(&self, segment_id: &str) -> &str {
        self.transcripts_by_segment_id
            .iter()
            .find(|(id, _)| id == segment_id)
            .map(|(_, text)| text.as_str())
            .unwrap_or("")
    }

    /// 写入分段转写：已存在则原位替换，否则追加到末尾（保持插入序）。
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

    /// 丢弃既未提交、也没有 final 转写的分段记录（清空静音尾部时调用）。
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

    /// 最终文本的分段顺序：先按提交序排列的已提交段，其后是尚未提交
    /// 但已有转写的段。
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

/// streams 表中的条目：状态 + 代数标记。
struct StreamEntry {
    /// 代数；同 id 重新 start 会分配新代数，旧会话的异步事件据此判定过期。
    generation: u64,
    /// 流状态。
    state: StreamState,
}

/// 释放条目持有的资源：abort 最终化超时任务并关闭会话。幂等——字段被
/// take 清空后再次调用无副作用。
fn close_entry(entry: &mut StreamEntry) {
    if let Some(handle) = entry.state.final_timeout.take() {
        handle.abort();
    }
    if let Some(mut session) = entry.state.session.take() {
        session.close();
    }
}

/// dictationId → 流条目的共享容器；互斥锁对应 JS 的单线程事件循环。
type Streams = Mutex<HashMap<String, StreamEntry>>;

/// 加锁辅助：锁中毒时恢复内层数据继续使用，不让一次 panic 毁掉整条连接。
fn lock(streams: &Streams) -> MutexGuard<'_, HashMap<String, StreamEntry>> {
    streams.lock().unwrap_or_else(|e| e.into_inner())
}

/// 听写流管理器（对应 JS 的 `DictationStreamManager`）：持有全部活跃流与
/// STT 会话工厂。分段参数与超时预算保持可赋值，便于测试与后续调优。
/// `DictationStreamManager`. The segment bounds and timeout budget stay
/// assignable for the same reasons the JS leaves them assignable (tests and
/// future tuning).
pub struct DictationStreamManager {
    /// 出站消息通道（发往 WebSocket）。
    emit_tx: mpsc::UnboundedSender<Value>,
    /// STT 会话工厂，负责选择本地/远程提供方。
    create_stt_session: CreateSttSession,
    /// 基础最终化超时（毫秒）。
    pub final_timeout_ms: u64,
    /// 分段最小长度（秒）。
    pub segment_min_seconds: f64,
    /// 分段硬上限（秒）。
    pub segment_max_seconds: f64,
    /// dictationId → 流条目。
    streams: Streams,
    /// 代数计数器，每次 start 原子递增。
    generations: AtomicU64,
}

/// 管理器主体：start/chunk/finish/cancel 的处理与封板、最终化判定。
impl DictationStreamManager {
    /// 用显式的超时与分段参数构造（对应 JS 测试直接给这些字段赋值）。
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

    /// 以默认参数构造共享管理器；返回 `Arc` 是因为会话 pump 与最终化
    /// 超时任务都要捕获它（对应 JS 闭包捕获 `this`）。
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

    /// 发送一条消息到 WebSocket 通道；接收端已关闭时静默丢弃。
    fn emit(&self, message: Value) {
        let _ = self.emit_tx.send(message);
    }

    /// 发送 ack，携带当前最高连续 seq。
    fn emit_ack(&self, dictation_id: &str, ack_seq: i64) {
        self.emit(json!({ "type": "ack", "dictationId": dictation_id, "ackSeq": ack_seq }));
    }

    /// 发送 error 消息；提供 reason_code 时附带 `reasonCode` 字段。
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

    /// 清理流：从表中移除条目，关闭会话并终止超时任务。
    /// Lock-free emit, then remove the stream (may be called with the map
    /// lock held by the caller — cleanup itself runs after release).
    fn cleanup_stream(&self, dictation_id: &str) {
        let mut streams = lock(&self.streams);
        if let Some(mut entry) = streams.remove(dictation_id) {
            close_entry(&mut entry);
        }
    }

    /// 报错后清理流（不产出最终文本）。
    fn fail_and_cleanup_stream(&self, dictation_id: &str, error: &str, retryable: bool) {
        self.fail_stream(dictation_id, error, retryable, None);
        self.cleanup_stream(dictation_id);
    }

    /// 清理全部活跃流（连接关闭时调用）。
    pub fn cleanup_all(&self) {
        let ids: Vec<String> = lock(&self.streams).keys().cloned().collect();
        for id in ids {
            self.cleanup_stream(&id);
        }
    }

    /// 处理 `start`：清掉同 id 旧流、分配新代数，解析输入采样率（非法即
    /// 失败），创建 STT 会话（NotReady 时按原样报错返回），按需构造重采样
    /// 器，注册流状态并 ack(-1)，最后 spawn 会话事件 pump。
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

    /// 应用一条会话事件（对应 JS 的 `stt.on(...)` 处理器）：Committed 记录
    /// 分段并递减 pending；Transcript 更新转写并发出 partial；Error 报错并
    /// 清理。均以代数守卫防止旧会话事件污染重启后的新流。
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

    /// 处理 `chunk`：按 seq 缓存乱序 chunk，从 next_seq_to_forward 起连续
    /// 转发——重采样后追加给会话、更新分段统计并尝试自动切分，随后推进
    /// ack。重复 seq 只重发 ack 不重复追加；重采样、峰值计算或提交失败时
    /// 报错并清理流。
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

    /// 处理 `finish`：记录 final_seq（声明的 seq>0 却一个 chunk 都没收到时
    /// 立即报错），随后尝试封板与最终化，再按剩余工作量估算并调度最终化
    /// 超时任务（到期仍未完成则以超时错误收尾），最后回 `finish_accepted`
    /// 携带预算毫秒数。
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

    /// 处理 `cancel`：立即清理流，不产出最终文本。
    pub fn handle_cancel(&self, dictation_id: &str) {
        self.cleanup_stream(dictation_id);
    }

    /// 估算最终化超时预算：在基础值之上按待完成分段数、未提交音频秒数与
    /// 缺失 seq 数线性追加，封顶 FINAL_TIMEOUT_MAX_MS，且永不低于基础值。
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

    /// 自动切分判定：finish 之后不再切；满足切分条件且当前段纯静音则清空
    /// 会话缓冲并复位统计，否则提交分段。返回 Some 表示提交侧错误。
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

    /// 发出 commit 并计入 in-flight 计数；收到 committed 事件前管理器不得
    /// 最终化，否则最终文本会缺这一段的转写。
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

    /// finish 封板（幂等，finish_sealed 防重入）：音频收齐后处理尾部——
    /// 纯静音则清空并丢弃未提交转写，否则提交尾部分段；失败则报错清理。
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

    /// 最终化判定：finish 已请求、音频收齐、无 in-flight commit 且全部分段
    /// 都有 final 转写时，按分段顺序拼接发出 `final` 文本并清理流；一个
    /// 分段都没有时发出空文本。
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

/// 取出仍属当前代数的流条目；id 缺失或代数不符（流已重启/清理）时返回
/// None——等价于 JS 的 `if (!state) return` 过期检查。
fn live_entry<'a>(
    streams: &'a mut HashMap<String, StreamEntry>,
    dictation_id: &str,
    generation: u64,
) -> Option<&'a mut StreamEntry> {
    streams
        .get_mut(dictation_id)
        .filter(|entry| entry.generation == generation)
}

/// 状态机单元测试：乱序重排与 ack、静音清空、分段切分、finish 流程、
/// 错误路径与最终化超时。
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// 测试用 PCM 格式串（16 kHz、16-bit）。
    const FORMAT: &str = "audio/pcm;rate=16000;bits=16";

    /// 假会话的调用记录与分段计数。
    struct FakeState {
        /// 收到的每段 PCM 数据。
        appended: Vec<Vec<u8>>,
        /// commit 调用次数。
        commits: usize,
        /// clear 调用次数。
        clears: usize,
        /// 会话是否已关闭。
        closed: bool,
        /// 已分配的分段序号。
        segment_counter: usize,
    }

    /// 模拟 STT 会话：commit 时发出 Committed，并在 1ms 后发出 final
    /// Transcript（模拟 JS 假件的 setTimeout(0) 时序）。
    struct FakeSttSession {
        /// 共享调用记录。
        state: Arc<Mutex<FakeState>>,
        /// 事件通道发送端。
        events: mpsc::UnboundedSender<SessionEvent>,
        /// 按分段序号生成转写文本的闭包。
        transcript_by_segment: Arc<dyn Fn(usize) -> String + Send + Sync>,
    }

    /// FakeSttSession 的会话 trait 实现。
    impl StreamingTranscriptionSession for FakeSttSession {
        /// 固定 16 kHz，与 FORMAT 一致。
        fn required_sample_rate(&self) -> u32 {
            16000
        }

        /// 记录追加的 PCM 数据。
        fn append_pcm16(&mut self, chunk: Vec<u8>) {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .appended
                .push(chunk);
        }

        /// 计数并异步回放 Committed + final Transcript 事件对。
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

        /// 计数 clear 调用。
        fn clear(&mut self) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).clears += 1;
        }

        /// 标记会话已关闭。
        fn close(&mut self) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        }
    }

    /// 生成振幅交替 ±amplitude 的响亮 chunk（base64 编码的 PCM16）。
    fn loud_chunk_base64(samples: usize, amplitude: i16) -> String {
        let arr: Vec<i16> = (0..samples)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect();
        let bytes: Vec<u8> = arr.iter().flat_map(|s| s.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// 生成全零静音 chunk（base64 编码的 PCM16）。
    fn silent_chunk_base64(samples: usize) -> String {
        let bytes = vec![0u8; samples * 2];
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// 测试夹具：管理器 + 出站消息记录 + 假会话状态。
    struct Fixture {
        /// 被测管理器。
        manager: Arc<DictationStreamManager>,
        /// 已发出消息的快照列表。
        messages: Arc<Mutex<Vec<Value>>>,
        /// 假会话调用记录。
        state: Arc<Mutex<FakeState>>,
    }

    /// 用显式的超时与分段参数构造夹具。
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

    /// 以默认参数构造夹具，可指定分段转写文本生成器。
    fn fixture_with(transcript_by_segment: Arc<dyn Fn(usize) -> String + Send + Sync>) -> Fixture {
        fixture_with_bounds(transcript_by_segment, 10_000, 60.0, 90.0)
    }

    /// 全默认夹具；转写固定为 "hello world"。
    fn fixture() -> Fixture {
        fixture_with(Arc::new(|_| "hello world".to_string()))
    }

    /// 轮询消息列表直到谓词命中（约 2 秒上限），超时 panic。
    async fn wait_for(messages: &Arc<Mutex<Vec<Value>>>, predicate: impl Fn(&Value) -> bool) {
        wait_for_within(messages, predicate, 500).await;
    }

    /// `wait_for` 的可调尝试次数版本。
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

    /// 取当前消息快照。
    fn messages_of(fixture: &Fixture) -> Vec<Value> {
        fixture
            .messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 过滤出指定 `type` 的消息。
    fn of_type<'a>(fixture: &'a Fixture, kind: &str) -> Vec<Value> {
        messages_of(fixture)
            .into_iter()
            .filter(|message| message["type"] == kind)
            .collect()
    }

    /// 全局递增计数器，保证跨测试的 dictation id 唯一。
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// 生成带唯一序号的 dictation id。
    fn unique_id(label: &str) -> String {
        format!("{label}-{}", SEQ.fetch_add(1, AtomicOrdering::SeqCst))
    }

    /// 验证：顺序 chunk → finish 的主路径产出 final 文本、恰好一次
    /// commit、会话关闭，且最终 ack 到达最后 seq。
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

    /// 验证：乱序到达的 chunk 被暂存，直到 seq 补齐后才按序追加给会话。
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

    /// 验证：纯静音的听写在 finish 时被清空而不是提交，最终文本为空串。
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

    /// 验证：finish 声明有音频却一个 chunk 都没收到时快速失败
    /// （retryable error）并关闭会话。
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

    /// 验证：会话工厂返回 NotReady 时，error 消息透传 reasonCode 与
    /// retryable 标记。
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

    /// 验证：分段转写陆续到达时逐段发出 partial，最终文本按空格拼接
    /// 全部分段。
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

    /// 验证：低于分段最小长度时中途停顿不切分，仅在 finish 时同步提交
    /// 尾部（一次 commit）。
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

    /// 验证：超过最小长度后的下一个静音 chunk 成为分段边界（触发
    /// commit），未达最小长度时同样的停顿不切。
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

    /// 验证：持续有声（无停顿）的语音在硬上限处被无条件切分。
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

    /// 验证：纯静音段即使到达硬上限也被清空，绝不提交。
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

    /// 验证：start 之前到达的 chunk 触发 "Dictation stream not started"
    /// 的 retryable 错误。
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

    /// 验证：重复/过期 seq 只重发既有 ack，不会把音频重复追加给会话。
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

    /// 验证：commit 永远等不到转写时，最终化超时以 retryable 错误收尾；
    /// 缺失 seq 项让预算保持较小，而不是整段基础超时。
    #[tokio::test]
    async fn finalization_timeout_fires_when_transcripts_never_arrive() {
        // 静默会话：commit 只回 Committed，永不发出转写。
        struct SilentSession {
            events: mpsc::UnboundedSender<SessionEvent>,
        }
        // 静默会话的 trait 实现。
        impl StreamingTranscriptionSession for SilentSession {
            // 固定 16 kHz。
            fn required_sample_rate(&self) -> u32 {
                16000
            }
            // 丢弃音频，制造"永远没有转写"的局面。
            fn append_pcm16(&mut self, _chunk: Vec<u8>) {}
            // 只回 Committed 事件。
            fn commit(&mut self) {
                let _ = self.events.send(SessionEvent::Committed {
                    segment_id: "seg-silent".to_string(),
                    previous_segment_id: None,
                });
                // No transcript, ever.
            }
            // 无需清空。
            fn clear(&mut self) {}
            // 无需清理。
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
