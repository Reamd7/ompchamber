/**
 * Sherpa-onnx offline TTS (Kokoro and Piper/VITS). Runs inside the dictation worker process
 * only — never load the native addon in the main server process.
 */
/**
 * sherpa-onnx 离线 TTS（Kokoro 与 Piper/VITS）的引擎封装。
 * 仅在听写 worker 进程内运行——绝不在主服务进程加载原生 addon。
 */

import { existsSync } from 'fs';
import path from 'path';

import { loadSherpaOnnxNode } from './sherpa-loader.js';

/** 断言模型文件存在，缺失时抛出带角色标签的错误信息。 */
function assertFileExists(filePath, label) {
  if (!existsSync(filePath)) {
    throw new Error(`Missing ${label}: ${filePath}`);
  }
}

/** 把 [-1, 1] 的 Float32 采样钳位后转换为 PCM16LE Buffer。 */
function float32ToPcm16le(samples) {
  const out = new Int16Array(samples.length);
  for (let i = 0; i < samples.length; i += 1) {
    const clamped = Math.max(-1, Math.min(1, samples[i]));
    out[i] = Math.round(clamped * 32767);
  }
  return Buffer.from(out.buffer, out.byteOffset, out.byteLength);
}

/**
 * sherpa-onnx model config for one catalog entry. Kokoro carries a voices
 * bank (speaker ids) and optional lexicons; a Piper/VITS model is a single
 * voice with espeak-ng phonemization.
 * @param {{ modelDir: string, type?: string, files: Record<string, string>, lexicon?: string[] }} config
 */
/**
 * 组装对应目录条目的 sherpa-onnx 模型配置：vits 分支只需 model 与
 * tokens（espeak 数据可选，字符级模型没有）；kokoro 分支还需 voices
 * 与 espeak 数据目录，词典条目以逗号拼接；任一文件缺失立即抛错。
 */
function buildModelConfig(config) {
  const file = (key, label) => {
    const filePath = path.join(config.modelDir, config.files[key]);
    assertFileExists(filePath, label);
    return filePath;
  };
  const modelPath = file('model', 'TTS model');
  const tokensPath = file('tokens', 'TTS tokens');

  if (config.type === 'vits') {
    // Piper models phonemize through espeak-ng (`espeakData`); character
    // models (Coqui) read the text directly and carry no espeak data.
    const dataDir = config.files.espeakData ? file('espeakData', 'TTS espeak-ng dataDir') : '';
    return { vits: { model: modelPath, tokens: tokensPath, ...(dataDir ? { dataDir } : {}), lengthScale: 1.0 } };
  }

  const dataDir = file('espeakData', 'TTS espeak-ng dataDir');
  const voicesPath = file('voices', 'TTS voices');
  const lexicon = (config.lexicon ?? []).map((key) => file(key, 'TTS lexicon')).join(',');
  return {
    kokoro: {
      model: modelPath,
      voices: voicesPath,
      tokens: tokensPath,
      dataDir,
      lengthScale: 1.0,
      ...(lexicon ? { lexicon } : {}),
    },
  };
}

/**
 * sherpa-onnx OfflineTts 引擎封装：构造时校验并加载模型，
 * synthesize 输出 PCM16LE，free 释放原生资源。
 */
export class SherpaTtsEngine {
  /**
   * @param {{ modelDir: string, type?: string, files: Record<string, string>, lexicon?: string[], numThreads?: number }} config
   */
  /**
   * @param {{ modelDir: string, type?: string, files: Record<string, string>, lexicon?: string[], numThreads?: number }} config
   *   目录规格：模型目录、类型（kokoro/vits）、文件名映射与词典键列表。
   */
  constructor(config) {
    const model = buildModelConfig(config);

    const sherpa = loadSherpaOnnxNode();
    if (typeof sherpa.OfflineTts !== 'function') {
      throw new Error('sherpa-onnx-node OfflineTts is unavailable');
    }

    this.tts = new sherpa.OfflineTts({
      model,
      numThreads: config.numThreads ?? 2,
      provider: 'cpu',
      maxNumSentences: 1,
    });
  }

  /**
   * Synthesize text to PCM16LE.
   * @param {string} text
   * @param {{ speakerId?: number, speed?: number }} [options]
   * @returns {{ pcm16: Buffer, sampleRate: number }}
   */
  /**
   * 合成文本为 PCM16LE：空白文本抛错；说话人与语速缺省取 0 与 1.0；
   * 禁用 external buffer 让 sherpa 返回自行拷贝的采样（原生外部缓冲
   * 的 TypedArray 在 Electron 中会被拒绝）；采样率依次取生成结果、
   * 引擎属性，均缺失时兜底 24000Hz。
   */
  synthesize(text, options = {}) {
    const trimmed = String(text || '').trim();
    if (!trimmed) {
      throw new Error('Cannot synthesize empty text');
    }

    const audio = this.tts.generate({
      text: trimmed,
      sid: Number.isInteger(options.speakerId) ? options.speakerId : 0,
      speed: typeof options.speed === 'number' && options.speed > 0 ? options.speed : 1.0,
      // Request a copied buffer from sherpa itself: native external-backed
      // typed arrays are rejected by Electron.
      enableExternalBuffer: false,
    });

    let samples = null;
    if (audio && audio.samples instanceof Float32Array) {
      samples = Float32Array.from(audio.samples);
    } else if (audio && Array.isArray(audio.samples)) {
      samples = Float32Array.from(audio.samples);
    }
    if (!samples) {
      throw new Error('Unexpected sherpa TTS output: missing Float32 samples');
    }

    const sampleRate =
      audio && typeof audio.sampleRate === 'number' && audio.sampleRate > 0
        ? audio.sampleRate
        : typeof this.tts.sampleRate === 'number' && this.tts.sampleRate > 0
          ? this.tts.sampleRate
          : 24000;

    return { pcm16: float32ToPcm16le(samples), sampleRate };
  }

  /** 释放原生 TTS 资源；已释放或抛错时静默忽略。 */
  free() {
    try {
      this.tts?.free?.();
    } catch {
      // ignore
    }
  }
}
