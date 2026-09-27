/**
 * Pseudo-streaming transcription session for OpenAI-compatible Whisper
 * endpoints (faster-whisper, whisper.cpp, OpenAI, ...).
 *
 * The Whisper HTTP API cannot stream, so audio is buffered per segment and
 * transcribed on commit(). This matches how the local session behaves: the
 * DictationStreamManager splits long dictations at pauses, and everything
 * shorter is one request on stop.
 *
 * Implements the StreamingTranscriptionSession contract used by
 * DictationStreamManager.
 */
/**
 * 面向 OpenAI 兼容 Whisper 端点的"伪流式"转写会话。
 *
 * Whisper HTTP API 不支持流式，因此音频按分段缓冲、在 commit() 时
 * 一次性转写；DictationStreamManager 会在停顿处切分长听写，更短的
 * 听写则整体作为一次请求在 stop 时发出，行为与本地会话一致。
 */

import { EventEmitter } from 'events';
import { randomUUID } from 'crypto';

import { transcribeAudio } from '../tts/stt.js';
import { pcm16ToWav } from './audio.js';

/** 会话固定要求的采样率（Hz）：Whisper 端点只接受 16kHz 音频。 */
const OPENAI_COMPATIBLE_SAMPLE_RATE = 16000;

/**
 * 按 StreamingTranscriptionSession 契约实现的分段缓冲会话：
 * appendPcm16 累积音频，commit() 把缓冲打包成 WAV 调用 transcribeAudio，
 * 并以事件（committed / transcript / error）回报结果。
 */
export class OpenAICompatibleTranscriptionSession extends EventEmitter {
  /**
   * @param {{ baseURL: string, model: string, apiKey?: string, language?: string, prompt?: string }} config
   */
  /**
   * @param {{ baseURL: string, model: string, apiKey?: string, language?: string, prompt?: string }} config
   *   自定义 STT 服务器地址、模型名及可选鉴权/语言/提示参数。
   */
  constructor(config) {
    super();
    this.config = config;
    this.requiredSampleRate = OPENAI_COMPATIBLE_SAMPLE_RATE;
    this.connected = false;
    this.segmentId = randomUUID();
    this.previousSegmentId = null;
    this.pcm16 = Buffer.alloc(0);
  }

  /**
   * 校验 baseURL 与 model 已配置（缺失即抛错），成功后置 connected=true；
   * 真正的 HTTP 连接推迟到首次转写请求。
   */
  async connect() {
    if (!this.config.baseURL) {
      throw new Error('Custom STT server URL is not configured');
    }
    if (!this.config.model) {
      throw new Error('STT model is not configured');
    }
    this.connected = true;
  }

  /** 追加一段 PCM16LE 音频到当前分段缓冲；未连接时发出 'error' 事件。 */
  appendPcm16(chunk) {
    if (!this.connected) {
      this.emit('error', new Error('STT session not connected'));
      return;
    }
    this.pcm16 = this.pcm16.length === 0 ? chunk : Buffer.concat([this.pcm16, chunk]);
  }

  /**
   * 提交当前分段：切换到新分段 id、发出 'committed'，随后异步把缓冲的
   * PCM 封装成 WAV 调用 transcribeAudio；成功发出 isFinal 的
   * 'transcript'，失败发出 'error'（不抛出，避免打断调用方）。
   */
  commit() {
    if (!this.connected) {
      this.emit('error', new Error('STT session not connected'));
      return;
    }

    const committedId = this.segmentId;
    const previousSegmentId = this.previousSegmentId;
    const committedPcm16 = this.pcm16;
    this.previousSegmentId = committedId;
    this.segmentId = randomUUID();
    this.pcm16 = Buffer.alloc(0);
    this.emit('committed', { segmentId: committedId, previousSegmentId });

    void (async () => {
      try {
        const wav = pcm16ToWav(committedPcm16, OPENAI_COMPATIBLE_SAMPLE_RATE);
        const text = await transcribeAudio({
          audioBuffer: wav,
          mimeType: 'audio/wav',
          model: this.config.model,
          baseURL: this.config.baseURL,
          apiKey: this.config.apiKey,
          language: this.config.language,
        });
        this.emit('transcript', {
          segmentId: committedId,
          transcript: (text ?? '').trim(),
          isFinal: true,
        });
      } catch (err) {
        this.emit('error', err instanceof Error ? err : new Error(String(err)));
      }
    })();
  }

  /** 丢弃当前分段缓冲（静音清理路径）并换新分段 id。 */
  clear() {
    this.pcm16 = Buffer.alloc(0);
    this.segmentId = randomUUID();
  }

  /** 关闭会话：清除连接状态与音频缓冲。 */
  close() {
    this.connected = false;
    this.pcm16 = Buffer.alloc(0);
  }
}
