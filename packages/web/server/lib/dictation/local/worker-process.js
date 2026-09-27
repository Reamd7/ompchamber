/**
 * Dictation local-speech worker process.
 *
 * Hosts the sherpa-onnx native inference (Parakeet STT) in a separate process
 * so ONNX decoding never blocks the main OMPChamber server. Communicates
 * with the parent over child_process IPC (advanced serialization, so Buffers
 * survive the trip as Uint8Array).
 *
 * Request/response protocol (parent -> worker):
 *   { type: 'session.create', requestId, sessionId, modelsDir, modelId }
 *   { type: 'session.append', requestId, sessionId, audio }
 *   { type: 'session.commit' | 'session.clear' | 'session.close', requestId, sessionId }
 * Worker -> parent:
 *   { type: 'response', requestId, ok, result?, error? }
 *   { type: 'session.committed' | 'session.transcript' | 'session.error', sessionId, ... }
 */
/**
 * 听写本地语音 worker 进程入口。
 *
 * 承载 sherpa-onnx 原生推理（Parakeet STT / Kokoro、Piper TTS），经
 * child_process IPC（advanced 序列化，Buffer 会以 Uint8Array 形式抵达，
 * 由 toBuffer 统一还原）按文件头所列协议与父进程通信；按 requestId
 * 结算请求、按 sessionId 管理会话，并缓存引擎实例避免重复加载原生
 * 模型；IPC 断开时清理全部资源后退出。
 */

import {
  SherpaOfflineRecognizerEngine,
  SherpaSegmentTranscriptionSession,
} from './sherpa-recognizer.js';
import { SherpaTtsEngine } from './sherpa-tts.js';
import { getLocalSttModelDir, getLocalSttModelSpec } from './model-catalog.js';
import { pcm16ToWav } from '../audio.js';
import path from 'path';

// 设置进程标题，便于在进程列表中识别听写 worker。
process.title = 'OMPChamber Dictation';

/** STT 识别引擎缓存：键为 "modelsDir:modelId"，避免重复加载原生模型。 */
const engines = new Map();
/** TTS 引擎缓存：键为 "modelsDir:modelId"。 */
const ttsEngines = new Map();
/** 活动的 STT 会话：键为 sessionId，值为 SherpaSegmentTranscriptionSession。 */
const sessions = new Map();
/** IPC 通道失效标志：置 true 后不再向父进程发送任何消息。 */
let ipcClosing = false;

/**
 * 向父进程发送消息；IPC 已断开、发送抛错或回调报错时置 ipcClosing
 * 并静默放弃（父进程关闭通道属于正常生命周期）。
 */
function sendToParent(message) {
  if (ipcClosing || !process.connected || !process.send) {
    return;
  }
  try {
    process.send(message, (error) => {
      if (error) {
        ipcClosing = true;
      }
    });
  } catch {
    ipcClosing = true;
  }
}

/** 以成功响应结算一个请求（result 为 undefined 时不携带 result 字段）。 */
function sendOk(requestId, result) {
  sendToParent({ type: 'response', requestId, ok: true, ...(result !== undefined ? { result } : {}) });
}

/**
 * 取或建 STT 识别引擎：按 "modelsDir:modelId" 缓存；创建时从目录
 * 规格拼接 encoder/decoder/tokens 路径，仅 transducer 类模型带 joiner。
 */
function getEngine(modelsDir, modelId) {
  const key = `${modelsDir}:${modelId}`;
  const existing = engines.get(key);
  if (existing) {
    return existing;
  }
  const modelDir = getLocalSttModelDir(modelsDir, modelId);
  const spec = getLocalSttModelSpec(modelId);
  const created = new SherpaOfflineRecognizerEngine({
    type: spec.type,
    encoder: path.join(modelDir, spec.files.encoder),
    decoder: path.join(modelDir, spec.files.decoder),
    ...(spec.files.joiner ? { joiner: path.join(modelDir, spec.files.joiner) } : {}),
    tokens: path.join(modelDir, spec.files.tokens),
    numThreads: 2,
  });
  engines.set(key, created);
  return created;
}

/** 移除并关闭一个会话；close 抛错被忽略（尽力清理）。 */
function cleanupSession(sessionId) {
  const session = sessions.get(sessionId);
  sessions.delete(sessionId);
  try {
    session?.close();
  } catch {
    // ignore
  }
}

/**
 * 把 IPC 传来的音频载荷统一还原为 Buffer：兼容 Buffer、Uint8Array
 * （advanced 序列化的产物）与旧式 {type:'Buffer'} 对象；其它类型抛错。
 */
function toBuffer(audio) {
  if (Buffer.isBuffer(audio)) {
    return audio;
  }
  if (audio instanceof Uint8Array) {
    return Buffer.from(audio.buffer, audio.byteOffset, audio.byteLength);
  }
  if (audio && typeof audio === 'object' && audio.type === 'Buffer' && Array.isArray(audio.data)) {
    return Buffer.from(audio.data);
  }
  throw new Error('Unsupported audio payload in dictation worker');
}

/**
 * 取或建 TTS 引擎：按 "modelsDir:modelId" 缓存，配置来自目录规格
 * （模型目录、类型、文件名映射、词典与线程数）。
 */
function getTtsEngine(modelsDir, modelId) {
  const key = `${modelsDir}:${modelId}`;
  const existing = ttsEngines.get(key);
  if (existing) {
    return existing;
  }
  const spec = getLocalSttModelSpec(modelId);
  const created = new SherpaTtsEngine({
    modelDir: getLocalSttModelDir(modelsDir, modelId),
    type: spec.type,
    files: spec.files,
    lexicon: spec.lexicon,
    numThreads: 2,
  });
  ttsEngines.set(key, created);
  return created;
}

/**
 * 请求分发：tts.synthesize 合成并回 WAV；session.create 先清理同 id
 * 旧会话再新建（committed/transcript/error 事件转发给父进程）；
 * session.append/commit/clear/close 操作对应会话；未知类型抛错，
 * 由外层统一转为失败响应。
 */
async function handleRequest(message) {
  switch (message.type) {
    case 'tts.synthesize': {
      const engine = getTtsEngine(message.modelsDir, message.modelId);
      const { pcm16, sampleRate } = engine.synthesize(message.text, {
        speakerId: message.speakerId,
        speed: message.speed,
      });
      sendOk(message.requestId, {
        audio: pcm16ToWav(pcm16, sampleRate),
        format: 'audio/wav',
      });
      return;
    }
    case 'session.create': {
      cleanupSession(message.sessionId);
      const engine = getEngine(message.modelsDir, message.modelId);
      const session = new SherpaSegmentTranscriptionSession({ engine });
      session.on('committed', (payload) => {
        sendToParent({ type: 'session.committed', sessionId: message.sessionId, payload });
      });
      session.on('transcript', (payload) => {
        sendToParent({ type: 'session.transcript', sessionId: message.sessionId, payload });
      });
      session.on('error', (err) => {
        sendToParent({
          type: 'session.error',
          sessionId: message.sessionId,
          error: err instanceof Error ? err.message : String(err),
        });
      });
      await session.connect();
      sessions.set(message.sessionId, session);
      sendOk(message.requestId, { requiredSampleRate: session.requiredSampleRate });
      return;
    }
    case 'session.append': {
      sessions.get(message.sessionId)?.appendPcm16(toBuffer(message.audio));
      sendOk(message.requestId);
      return;
    }
    case 'session.commit': {
      sessions.get(message.sessionId)?.commit();
      sendOk(message.requestId);
      return;
    }
    case 'session.clear': {
      sessions.get(message.sessionId)?.clear();
      sendOk(message.requestId);
      return;
    }
    case 'session.close': {
      cleanupSession(message.sessionId);
      sendOk(message.requestId);
      return;
    }
    default: {
      throw new Error(`Unknown dictation worker request: ${message?.type}`);
    }
  }
}

// 消息入口：处理请求；任何异常都转为对相应 requestId 的失败响应。
process.on('message', (message) => {
  void handleRequest(message).catch((error) => {
    sendToParent({
      type: 'response',
      requestId: message?.requestId,
      ok: false,
      error: error instanceof Error ? error.message : 'Dictation worker request failed',
    });
  });
});

// 父进程断开：清理全部会话、释放 STT/TTS 引擎的原生资源后退出。
process.once('disconnect', () => {
  ipcClosing = true;
  for (const sessionId of Array.from(sessions.keys())) {
    cleanupSession(sessionId);
  }
  for (const engine of engines.values()) {
    engine.free();
  }
  for (const tts of ttsEngines.values()) {
    tts.free();
  }
  process.exit(0);
});
