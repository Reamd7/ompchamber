/**
 * DictationStreamManager
 *
 * Server-authoritative streaming dictation state machine. One manager owns
 * all dictation streams for a single WebSocket connection.
 *
 * Responsibilities:
 * - Reorders inbound chunks by `seq` and acks the highest contiguous seq.
 * - Resamples client PCM (16 kHz by default) to the provider's required rate.
 * - Segments long dictations at natural pauses: past `segmentMinSeconds` of
 *   audio it commits on the first silent chunk, and `segmentMaxSeconds` is a
 *   hard cap for speech with no pause in it. Silence-only segments are
 *   cleared instead of committed.
 * - Concatenates per-segment transcripts into live partials and emits the
 *   final text once every committed segment has a final transcript. The
 *   manager counts the commits it issued rather than trusting the session's
 *   echoed events, so a commit still in flight when the client finishes
 *   cannot be silently dropped from the transcript.
 * - Applies an adaptive finalization timeout budget based on pending work.
 */

/**
 * 流式听写的服务端权威状态机（中文说明）：一个管理器负责一条 WebSocket
 * 连接上的全部听写流——按 seq 重排并确认音频块、重采样 PCM、在自然停顿
 * 处分段提交、拼接分段转写为实时 partial 与最终文本，并按待处理工作量
 * 自适应计算收尾超时。
 */
import { Pcm16MonoResampler, parsePcmRateFromFormat, pcm16lePeakAbs } from './audio.js';

/** 基础收尾超时；实际值会按待处理分段/音频/缺失 seq 动态加码（见 estimateFinalizationTimeout）。 */
const DEFAULT_FINAL_TIMEOUT_MS = 10000;
// Parakeet is a full-attention conformer: decode cost and peak memory grow
// quadratically with segment length (measured: 60s -> 2.1s/+90MB,
// 300s -> 21.3s/+1.5GB). Segmenting keeps a long dictation off that curve and
// lets committed segments decode while the user is still speaking, so only the
// tail is left to transcribe on stop. Typical dictations are shorter than the
// minimum and are decoded as a single segment.
/** 分段下限：达到该时长后，遇到静音块即提交当前分段。 */
const DEFAULT_SEGMENT_MIN_SECONDS = 60;
/** 分段硬上限：不停顿的语音到点强制提交，避免解码成本随段长二次方增长。 */
const DEFAULT_SEGMENT_MAX_SECONDS = 90;
/** 收尾超时的绝对上限（5 分钟）。 */
const FINAL_TIMEOUT_MAX_MS = 5 * 60 * 1000;
/** 每个待出最终转写的已提交分段追加的超时。 */
const FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS = 15 * 1000;
/** 每秒待转写音频追加的超时。 */
const FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS = 1500;
/** 每个尚未收到的 seq 追加的超时。 */
const FINAL_TIMEOUT_PER_MISSING_SEQ_MS = 250;
/** PCM16 峰值绝对值低于该值视为静音（用于分段与静音段丢弃判定）。 */
const SILENCE_PEAK_THRESHOLD = 300;

/** 秒数换算为 PCM16 单声道字节数（每样本 2 字节），非正数返回 0。 */
const secondsToPcm16Bytes = (seconds, sampleRate) =>
  seconds > 0 ? Math.max(1, Math.round(seconds * sampleRate * 2)) : 0;

/**
 * Split the current segment once it is long enough to be worth decoding on its
 * own and the speaker has just gone quiet, or unconditionally at the hard cap.
 * Client chunks are ~1s, so a quiet chunk is roughly a second of silence — long
 * enough to be a sentence boundary rather than a gap between words.
 */
/** 是否应切分当前分段：达到硬上限无条件切；达到下限且刚出现静音块时切。 */
function shouldSplitSegment(state) {
  if (state.segmentMaxBytes > 0 && state.bytesSinceCommit >= state.segmentMaxBytes) {
    return true;
  }
  if (state.segmentMinBytes <= 0 || state.bytesSinceCommit < state.segmentMinBytes) {
    return false;
  }
  return state.lastChunkPeak < SILENCE_PEAK_THRESHOLD;
}

/**
 * 服务端权威的流式听写状态机：管理单条 WebSocket 连接上的全部听写流。
 * 以自己发出的 commit 计数为准（而非信任会话回显的事件），确保客户端
 * 结束时仍在途的提交不会被静默丢出最终转写。
 */
export class DictationStreamManager {
  /**
   * @param {object} params
   * @param {(msg: { type: string, payload: object }) => void} params.emit
   * @param {(startOptions: object) => Promise<{ session: object } | { error: string, retryable: boolean, reasonCode?: string }>} params.createSttSession
   *   Resolves a connected streaming transcription session for one dictation.
   *   The streaming transcription session contract:
   *   { requiredSampleRate, appendPcm16(buf), commit(), clear(), close(), on(event, handler) }
   * @param {number} [params.finalTimeoutMs]
   * @param {number} [params.segmentMinSeconds] audio before a pause may split a segment
   * @param {number} [params.segmentMaxSeconds] hard segment cap for pauseless speech
   */
  /** 中文说明：保存 emit/建会话回调与分段参数，初始化 dictationId → 状态 的 streams 表。 */
  constructor({ emit, createSttSession, finalTimeoutMs, segmentMinSeconds, segmentMaxSeconds }) {
    this.emit = emit;
    this.createSttSession = createSttSession;
    this.finalTimeoutMs = finalTimeoutMs ?? DEFAULT_FINAL_TIMEOUT_MS;
    this.segmentMinSeconds = segmentMinSeconds ?? DEFAULT_SEGMENT_MIN_SECONDS;
    this.segmentMaxSeconds = segmentMaxSeconds ?? DEFAULT_SEGMENT_MAX_SECONDS;
    this.streams = new Map();
  }

  /** 清理全部听写流（连接关闭时调用）。 */
  cleanupAll() {
    for (const dictationId of Array.from(this.streams.keys())) {
      this.cleanupStream(dictationId);
    }
  }

  /**
   * @param {string} dictationId
   * @param {string} format e.g. "audio/pcm;rate=16000;bits=16"
   * @param {object} startOptions provider/config options forwarded to createSttSession
   */
  /** 中文说明：校验采样率、经 createSttSession 建立转写会话并挂接 committed/transcript/error 事件，初始化流状态并回初始 ack。 */
  async handleStart(dictationId, format, startOptions = {}) {
    this.cleanupStream(dictationId);

    const inputRate = parsePcmRateFromFormat(format, 16000) ?? 16000;
    if (!Number.isFinite(inputRate) || inputRate <= 0) {
      this.failStream(dictationId, `Invalid dictation input rate in format: ${format}`, false);
      return;
    }

    let resolved;
    try {
      resolved = await this.createSttSession(startOptions);
    } catch (error) {
      this.failStream(dictationId, error?.message || String(error), true);
      return;
    }
    if (!resolved || resolved.error) {
      this.failStream(
        dictationId,
        resolved?.error || 'Dictation STT not configured',
        Boolean(resolved?.retryable),
        resolved?.reasonCode,
      );
      return;
    }

    const stt = resolved.session;

    stt.on('committed', ({ segmentId }) => {
      const state = this.streams.get(dictationId);
      if (!state) {
        return;
      }
      // Segment accounting is reset where the commit is issued, not here: this
      // event arrives after an async hop, and zeroing the counters on arrival
      // would discard audio that came in meanwhile — up to and including
      // mistaking the tail of the dictation for silence and clearing it.
      state.committedSegmentIds.push(segmentId);
      state.pendingCommits = Math.max(0, state.pendingCommits - 1);

      this.maybeFinalizeStream(dictationId);
    });

    stt.on('transcript', ({ segmentId, transcript, isFinal }) => {
      const state = this.streams.get(dictationId);
      if (!state) {
        return;
      }
      state.transcriptsBySegmentId.set(segmentId, transcript);
      if (isFinal) {
        state.finalTranscriptSegmentIds.add(segmentId);
      }

      const orderedIds = state.committedSegmentIds.includes(segmentId)
        ? state.committedSegmentIds
        : [...state.committedSegmentIds, segmentId];
      const partialText = orderedIds
        .map((id) => state.transcriptsBySegmentId.get(id) ?? '')
        .join(' ')
        .trim();
      this.emit({ type: 'partial', payload: { dictationId, text: partialText } });

      this.maybeSealStreamFinish(dictationId);
      this.maybeFinalizeStream(dictationId);
    });

    stt.on('error', (err) => {
      const message = err?.message || String(err);
      this.failAndCleanupStream(dictationId, message, true);
    });

    this.streams.set(dictationId, {
      dictationId,
      inputFormat: format,
      stt,
      inputRate,
      outputRate: stt.requiredSampleRate,
      resampler:
        inputRate === stt.requiredSampleRate
          ? null
          : new Pcm16MonoResampler({ inputRate, outputRate: stt.requiredSampleRate }),
      receivedChunks: new Map(),
      nextSeqToForward: 0,
      ackSeq: -1,
      segmentMinBytes: secondsToPcm16Bytes(this.segmentMinSeconds, stt.requiredSampleRate),
      segmentMaxBytes: secondsToPcm16Bytes(this.segmentMaxSeconds, stt.requiredSampleRate),
      bytesSinceCommit: 0,
      peakSinceCommit: 0,
      lastChunkPeak: 0,
      committedSegmentIds: [],
      transcriptsBySegmentId: new Map(),
      finalTranscriptSegmentIds: new Set(),
      pendingCommits: 0,
      finishRequested: false,
      finishSealed: false,
      finalSeq: null,
      finalTimeout: null,
    });

    this.emitAck(dictationId, -1);
  }

  /**
   * @param {{ dictationId: string, seq: number, audioBase64: string }} params
   */
  /** 中文说明：按 seq 去重重排音频块，重采样后送入转写会话；乱序/重复块只重发当前 ack，越界缓冲后驱动分段与收尾检查。 */
  handleChunk({ dictationId, seq, audioBase64 }) {
    const state = this.streams.get(dictationId);
    if (!state) {
      this.failStream(dictationId, 'Dictation stream not started', true);
      return;
    }

    if (!Number.isInteger(seq) || seq < 0) {
      return;
    }

    if (seq < state.nextSeqToForward) {
      this.emitAck(dictationId, state.ackSeq);
      return;
    }

    if (!state.receivedChunks.has(seq)) {
      let chunk;
      try {
        chunk = Buffer.from(audioBase64, 'base64');
      } catch {
        return;
      }
      if (chunk.length % 2 !== 0) {
        chunk = chunk.subarray(0, chunk.length - 1);
      }
      state.receivedChunks.set(seq, chunk);
    }

    while (state.receivedChunks.has(state.nextSeqToForward)) {
      const nextSeq = state.nextSeqToForward;
      const pcm16 = state.receivedChunks.get(nextSeq);
      state.receivedChunks.delete(nextSeq);

      const resampled = state.resampler ? state.resampler.processChunk(pcm16) : pcm16;
      if (resampled.length > 0) {
        state.stt.appendPcm16(resampled);
        state.bytesSinceCommit += resampled.length;
        state.lastChunkPeak = pcm16lePeakAbs(resampled);
        state.peakSinceCommit = Math.max(state.peakSinceCommit, state.lastChunkPeak);
        try {
          this.maybeAutoCommitSegment(state);
        } catch (error) {
          this.failAndCleanupStream(dictationId, error?.message || String(error), true);
          return;
        }
      }

      state.nextSeqToForward += 1;
      state.ackSeq = state.nextSeqToForward - 1;
    }

    this.emitAck(dictationId, state.ackSeq);
    this.maybeSealStreamFinish(dictationId);
    this.maybeFinalizeStream(dictationId);
  }

  /**
   * @param {string} dictationId
   * @param {number} finalSeq highest seq the client sent (or -1 if none)
   */
  /** 中文说明：登记客户端声明的最高 seq；一个音频块都没有则直接报错；否则做封段检查、收尾检查，并按待处理工作量启动收尾超时。 */
  handleFinish(dictationId, finalSeq) {
    const state = this.streams.get(dictationId);
    if (!state) {
      this.failStream(dictationId, 'Dictation stream not started', true);
      return;
    }

    state.finishRequested = true;
    state.finalSeq = finalSeq;

    if (
      finalSeq >= 0 &&
      state.ackSeq < 0 &&
      state.nextSeqToForward === 0 &&
      state.receivedChunks.size === 0
    ) {
      this.failStream(
        dictationId,
        'Dictation finished but no audio chunks were received',
        true,
      );
      this.cleanupStream(dictationId);
      return;
    }

    this.maybeSealStreamFinish(dictationId);
    this.maybeFinalizeStream(dictationId);

    const updatedState = this.streams.get(dictationId);
    if (!updatedState) {
      return;
    }

    const timeoutMs = this.estimateFinalizationTimeout(updatedState);
    if (updatedState.finalTimeout) {
      clearTimeout(updatedState.finalTimeout);
    }
    updatedState.finalTimeout = setTimeout(() => {
      this.failAndCleanupStream(dictationId, 'Timed out waiting for final transcription', true);
    }, timeoutMs);

    this.emit({ type: 'finish_accepted', payload: { dictationId, timeoutMs } });
  }

  /** 客户端主动取消：立即清理该听写流（不产出 final）。 */
  handleCancel(dictationId) {
    this.cleanupStream(dictationId);
  }

  /** 向客户端回当前最高连续 seq 的确认。 */
  emitAck(dictationId, ackSeq) {
    this.emit({ type: 'ack', payload: { dictationId, ackSeq } });
  }

  /** 向客户端上报错误（retryable 标记可重试，reasonCode 可选），不清理状态。 */
  failStream(dictationId, error, retryable, reasonCode) {
    this.emit({
      type: 'error',
      payload: {
        dictationId,
        error,
        retryable,
        ...(reasonCode ? { reasonCode } : {}),
      },
    });
  }

  /** 上报错误并清理该听写流。 */
  failAndCleanupStream(dictationId, error, retryable) {
    this.failStream(dictationId, error, retryable);
    this.cleanupStream(dictationId);
  }

  /** 清理听写流：取消收尾定时器、关闭转写会话（忽略关闭异常）并从 streams 移除。 */
  cleanupStream(dictationId) {
    const state = this.streams.get(dictationId);
    if (!state) {
      return;
    }
    if (state.finalTimeout) {
      clearTimeout(state.finalTimeout);
    }
    try {
      state.stt.close();
    } catch {
      // no-op
    }
    this.streams.delete(dictationId);
  }

  /** 估算收尾超时：基础值之上按待出转写的分段数、待转写音频秒数与缺失 seq 数线性加码，封顶 5 分钟。 */
  estimateFinalizationTimeout(state) {
    const bytesPerSecond = Math.max(1, state.outputRate * 2);
    const pendingCommittedSegments = state.committedSegmentIds.reduce((count, segmentId) => {
      return state.finalTranscriptSegmentIds.has(segmentId) ? count : count + 1;
    }, 0);
    const committedSet = new Set(state.committedSegmentIds);
    const pendingUncommittedTranscriptSegments = Array.from(
      state.transcriptsBySegmentId.keys(),
    ).reduce((count, segmentId) => {
      if (committedSet.has(segmentId)) {
        return count;
      }
      return state.finalTranscriptSegmentIds.has(segmentId) ? count : count + 1;
    }, 0);
    const pendingSegments =
      pendingCommittedSegments + pendingUncommittedTranscriptSegments + state.pendingCommits;
    const pendingAudioSeconds = Math.ceil(Math.max(0, state.bytesSinceCommit) / bytesPerSecond);
    const missingSeqCount =
      state.finalSeq === null ? 0 : Math.max(0, state.finalSeq - state.ackSeq);

    const extraMs =
      pendingSegments * FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS +
      pendingAudioSeconds * FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS +
      missingSeqCount * FINAL_TIMEOUT_PER_MISSING_SEQ_MS;

    return Math.max(
      this.finalTimeoutMs,
      Math.min(FINAL_TIMEOUT_MAX_MS, this.finalTimeoutMs + extraMs),
    );
  }

  /** 自动分段：达到切分条件时，整段皆静音则丢弃，否则提交当前分段（finish 后不再自动分段）。 */
  maybeAutoCommitSegment(state) {
    if (state.finishRequested) {
      return;
    }
    if (!shouldSplitSegment(state)) {
      return;
    }
    if (state.peakSinceCommit < SILENCE_PEAK_THRESHOLD) {
      state.stt.clear();
      state.bytesSinceCommit = 0;
      state.peakSinceCommit = 0;
      state.lastChunkPeak = 0;
      return;
    }

    state.bytesSinceCommit = 0;
    state.peakSinceCommit = 0;
    state.lastChunkPeak = 0;
    this.commitSegment(state);
  }

  /**
   * Issue a commit and record it as in flight. The session acknowledges with a
   * `committed` event; until then the manager must not finalize, or the
   * segment's transcript would be missing from the final text.
   */
  /** 中文说明：发出一次 commit 并记为在途；在收到 committed 事件前不允许收尾，否则该段转写会缺失。 */
  commitSegment(state) {
    state.pendingCommits += 1;
    try {
      state.stt.commit();
    } catch (error) {
      state.pendingCommits -= 1;
      throw error;
    }
  }

  /** 封段：客户端已 finish 且音频收齐后，对残余音频做静音丢弃或提交，此后不再接收音频。 */
  maybeSealStreamFinish(dictationId) {
    const state = this.streams.get(dictationId);
    if (!state) {
      return;
    }
    if (!state.finishRequested || state.finalSeq === null) {
      return;
    }
    if (state.ackSeq < state.finalSeq) {
      return;
    }
    if (state.finishSealed) {
      return;
    }

    if (state.bytesSinceCommit > 0) {
      if (state.peakSinceCommit < SILENCE_PEAK_THRESHOLD) {
        state.stt.clear();
        state.bytesSinceCommit = 0;
        state.peakSinceCommit = 0;
        state.lastChunkPeak = 0;
        this.dropUncommittedNonFinalTranscripts(state);
      } else {
        state.bytesSinceCommit = 0;
        state.peakSinceCommit = 0;
        state.lastChunkPeak = 0;
        try {
          this.commitSegment(state);
        } catch (error) {
          this.failAndCleanupStream(dictationId, error?.message || String(error), true);
          return;
        }
      }
    }

    state.finishSealed = true;
  }

  /** 丢弃未提交且无最终转写的分段草稿（配合静音丢弃，避免脏 partial 留进最终文本）。 */
  dropUncommittedNonFinalTranscripts(state) {
    const committedSet = new Set(state.committedSegmentIds);
    for (const segmentId of Array.from(state.transcriptsBySegmentId.keys())) {
      if (committedSet.has(segmentId)) {
        continue;
      }
      if (state.finalTranscriptSegmentIds.has(segmentId)) {
        continue;
      }
      state.transcriptsBySegmentId.delete(segmentId);
    }
  }

  /** 收尾判定：音频收齐、无在途 commit 且所有分段都有最终转写时，按提交顺序拼接并发出 final，然后清理。 */
  maybeFinalizeStream(dictationId) {
    const state = this.streams.get(dictationId);
    if (!state) {
      return;
    }

    if (!state.finishRequested || state.finalSeq === null) {
      return;
    }
    if (state.ackSeq < state.finalSeq) {
      return;
    }
    if (state.pendingCommits > 0) {
      return;
    }

    const committedSet = new Set(state.committedSegmentIds);
    const orderedSegmentIds = [...state.committedSegmentIds];
    for (const segmentId of state.transcriptsBySegmentId.keys()) {
      if (!committedSet.has(segmentId)) {
        orderedSegmentIds.push(segmentId);
      }
    }

    if (orderedSegmentIds.length === 0) {
      this.emit({ type: 'final', payload: { dictationId, text: '' } });
      this.cleanupStream(dictationId);
      return;
    }

    const allTranscriptsReady = orderedSegmentIds.every((segmentId) =>
      state.finalTranscriptSegmentIds.has(segmentId),
    );
    if (!allTranscriptsReady) {
      return;
    }

    const orderedText = orderedSegmentIds
      .map((segmentId) => state.transcriptsBySegmentId.get(segmentId) ?? '')
      .join(' ')
      .trim();

    this.emit({ type: 'final', payload: { dictationId, text: orderedText } });
    this.cleanupStream(dictationId);
  }
}
