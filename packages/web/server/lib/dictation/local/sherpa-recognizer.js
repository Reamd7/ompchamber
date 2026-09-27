/**
 * Sherpa-onnx offline recognizer engine (NeMo transducer / Parakeet) plus a
 * segment transcription session that decodes each segment exactly once, when
 * the segment is committed.
 *
 * Parakeet is an offline model: it is trained to see a whole utterance at
 * once. Decoding the accumulated audio repeatedly to animate a live transcript
 * costs O(n^2) work for a result the final decode throws away, so this session
 * only decodes on commit.
 *
 * Runs inside the dictation worker process only — never load the native
 * addon in the main server process.
 */
/**
 * sherpa-onnx 离线识别引擎与"每段仅解码一次"的分段转写会话。
 *
 * Parakeet 是离线模型，需要一次看完整段语音；对累积音频反复解码来
 * 刷新实时字幕是 O(n^2) 的浪费且最终解码会丢弃中间结果，因此本
 * 会话只在 commit 时解码一次。本模块只在听写 worker 进程内运行，
 * 绝不在主服务进程加载原生 addon。
 */

import { EventEmitter } from 'events';
import { existsSync } from 'fs';
import { randomUUID } from 'crypto';

import { loadSherpaOnnxNode } from './sherpa-loader.js';
import { pcm16lePeakAbs, pcm16leToFloat32 } from '../audio.js';

/** 断言模型文件存在，缺失时抛出带角色标签的错误信息。 */
function assertFileExists(filePath, label) {
  if (!existsSync(filePath)) {
    throw new Error(`Missing ${label}: ${filePath}`);
  }
}

/**
 * sherpa-onnx OfflineRecognizer 的封装：构造时按模型类型
 * （nemo_transducer / whisper）组装原生配置，提供创建流、喂数据、
 * 整段解码与释放原生资源的能力。
 */
export class SherpaOfflineRecognizerEngine {
  /**
   * @param {{ type: 'nemo_transducer' | 'whisper',
   *           encoder: string, decoder: string, joiner?: string, tokens: string,
   *           numThreads?: number }} config
   */
    /**
     * @param {{ type: 'nemo_transducer' | 'whisper',
     *           encoder: string, decoder: string, joiner?: string, tokens: string,
     *           numThreads?: number }} config
     *   模型类型与各文件路径；whisper 留空语言以自动检测，transducer 需 joiner。
     */
  constructor(config) {
    assertFileExists(config.encoder, 'offline encoder');
    assertFileExists(config.decoder, 'offline decoder');
    if (config.type === 'nemo_transducer') {
      assertFileExists(config.joiner, 'offline joiner');
    }
    assertFileExists(config.tokens, 'tokens');

    const sherpa = loadSherpaOnnxNode();

    const modelConfig =
      config.type === 'whisper'
        ? {
            whisper: {
              encoder: config.encoder,
              decoder: config.decoder,
              // Empty language auto-detects for multilingual Whisper exports.
              language: '',
              task: 'transcribe',
              tailPaddings: -1,
            },
            tokens: config.tokens,
            modelType: 'whisper',
            numThreads: config.numThreads ?? 2,
            provider: 'cpu',
            debug: 0,
          }
        : {
            transducer: {
              encoder: config.encoder,
              decoder: config.decoder,
              joiner: config.joiner,
            },
            tokens: config.tokens,
            modelType: 'nemo_transducer',
            numThreads: config.numThreads ?? 2,
            provider: 'cpu',
            debug: 0,
          };

    const recognizerConfig = {
      featConfig: {
        sampleRate: 16000,
        featureDim: 80,
      },
      modelConfig,
      decodingMethod: 'greedy_search',
      maxActivePaths: 4,
    };

    this.recognizer = new sherpa.OfflineRecognizer(recognizerConfig);
    const sr = this.recognizer?.config?.featConfig?.sampleRate;
    this.sampleRate =
      typeof sr === 'number' && Number.isFinite(sr) && sr > 0
        ? sr
        : recognizerConfig.featConfig.sampleRate;
  }

  /** 创建一个新的离线解码流（对应原生资源，用完需释放）。 */
  createStream() {
    return this.recognizer.createStream();
  }

  /**
   * 向流写入 Float32 采样；兼容 node addon（对象参数）与 WASM
   * （位置参数）两种 acceptWaveform 签名。
   */
  acceptWaveform(stream, sampleRate, samples) {
    if (!stream || typeof stream.acceptWaveform !== 'function') {
      throw new Error('Unexpected sherpa offline stream: missing acceptWaveform()');
    }
    // sherpa-onnx-node expects acceptWaveform({ samples, sampleRate });
    // the WASM build expects acceptWaveform(sampleRate, samples).
    if (stream.acceptWaveform.length <= 1) {
      stream.acceptWaveform({ samples, sampleRate });
    } else {
      stream.acceptWaveform(sampleRate, samples);
    }
  }

  /**
   * Decode a full PCM16 segment and return its text.
   * Applies auto-gain when the peak is low so quiet microphones still decode.
   * @param {Buffer} pcm16
   * @returns {string}
   */
    /**
     * 解码完整 PCM16 分段并返回文本；峰值过低时按目标峰值 0.6 自动
     * 增益（上限 50 倍），让安静麦克风也能识别；解码流在 finally 中释放。
     */
  decodePcm16(pcm16) {
    if (pcm16.length === 0) {
      return '';
    }

    const peak = pcm16lePeakAbs(pcm16);
    const peakFloat = peak / 32768.0;
    const targetPeak = 0.6;
    const maxGain = 50;
    const gain =
      peakFloat > 0 && peakFloat < targetPeak ? Math.min(maxGain, targetPeak / peakFloat) : 1;

    const stream = this.createStream();
    try {
      const floatSamples = pcm16leToFloat32(pcm16, gain);
      this.acceptWaveform(stream, this.sampleRate, floatSamples);
      this.recognizer.decode(stream);
      const result = this.recognizer.getResult(stream);
      const text =
        typeof result === 'object' && result && 'text' in result ? result.text : result;
      return String(text ?? '').trim();
    } finally {
      try {
        stream.free?.();
      } catch {
        // ignore
      }
    }
  }

  /** 释放原生识别器资源；已释放或抛错时静默忽略。 */
  free() {
    try {
      this.recognizer?.free?.();
    } catch {
      // ignore
    }
  }
}

/**
 * Segment transcription session backed by the offline recognizer.
 * Accumulates the current segment's PCM and decodes it once in `commit()`,
 * which emits the segment's final transcript and starts a new segment.
 *
 * Implements the StreamingTranscriptionSession contract used by
 * DictationStreamManager. It never emits non-final transcripts: the manager's
 * live `partial` messages are the concatenation of already-committed segments.
 */
/**
 * 基于离线识别引擎的分段转写会话：累积当前分段的 PCM，在 commit()
 * 时一次性解码并发出该段最终转写，随后开启新分段。
 *
 * 实现 DictationStreamManager 消费的 StreamingTranscriptionSession
 * 契约；不产生非最终转写——管理器的实时 partial 消息由已提交分段
 * 拼接而来。
 */
export class SherpaSegmentTranscriptionSession extends EventEmitter {
  /**
   * @param {{ engine: SherpaOfflineRecognizerEngine }} params
   */
    /**
     * @param {{ engine: SherpaOfflineRecognizerEngine }} params 底层识别引擎
     */
  constructor({ engine }) {
    super();
    this.engine = engine;
    this.requiredSampleRate = engine.sampleRate;
    this.connected = false;
    this.currentSegmentId = null;
    this.previousSegmentId = null;
    this.pcm16 = Buffer.alloc(0);
  }

  /** 初始化首个分段 id 并置为已连接（幂等）。 */
  async connect() {
    if (this.connected) {
      return;
    }
    this.currentSegmentId = randomUUID();
    this.connected = true;
  }

  /** 追加 PCM16 音频到当前分段缓冲；未连接时发出 'error' 事件。 */
  appendPcm16(chunk) {
    if (!this.connected || !this.currentSegmentId) {
      this.emit('error', new Error('Sherpa transcription session not connected'));
      return;
    }
    this.pcm16 = this.pcm16.length === 0 ? chunk : Buffer.concat([this.pcm16, chunk]);
  }

  /**
   * 提交当前分段：先切换到新分段并发出 'committed'（解码长段会阻塞
   * worker 数秒，期间下一分段的音频仍会到达），再同步解码旧段；
   * 解码失败发出 'error'，成功发出 isFinal 的 'transcript'。
   */
  commit() {
    if (!this.connected || !this.currentSegmentId) {
      this.emit('error', new Error('Sherpa transcription session not connected'));
      return;
    }

    const segmentId = this.currentSegmentId;
    const previousSegmentId = this.previousSegmentId;
    const pcm16 = this.pcm16;

    // Start the next segment before decoding: decoding blocks the worker for
    // seconds on long segments, and audio for the next one keeps arriving.
    this.previousSegmentId = segmentId;
    this.currentSegmentId = randomUUID();
    this.pcm16 = Buffer.alloc(0);

    this.emit('committed', { segmentId, previousSegmentId });

    let transcript;
    try {
      transcript = this.engine.decodePcm16(pcm16);
    } catch (err) {
      this.emit('error', err instanceof Error ? err : new Error(String(err)));
      return;
    }
    this.emit('transcript', { segmentId, transcript, isFinal: true });
  }

  /** 丢弃当前分段缓冲（静音清理路径）并换新分段 id。 */
  clear() {
    if (!this.connected) {
      return;
    }
    this.pcm16 = Buffer.alloc(0);
    this.currentSegmentId = randomUUID();
  }

  /** 关闭会话并清空全部状态（原生引擎资源由 worker 统一管理）。 */
  close() {
    this.connected = false;
    this.currentSegmentId = null;
    this.pcm16 = Buffer.alloc(0);
  }
}
