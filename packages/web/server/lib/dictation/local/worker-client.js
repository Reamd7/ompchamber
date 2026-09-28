/**
 * Client for the dictation local-speech worker process.
 *
 * Lazily forks the worker on first use, correlates request/response messages
 * by requestId, routes session events to per-session EventEmitters, and
 * shuts the worker down after an idle TTL so the ONNX runtime does not sit
 * in memory while dictation is unused.
 */
/**
 * 本地语音 worker 进程的客户端（主进程侧）。
 *
 * 首次使用时才 fork worker；按 requestId 关联请求/响应；把会话事件
 * 路由到各会话自己的 EventEmitter；空闲超过 TTL 后关闭 worker，
 * 避免听写闲置时 ONNX 运行时长期驻留内存。
 */

import { fork } from 'child_process';
import { randomUUID } from 'crypto';
import { EventEmitter } from 'events';
import { fileURLToPath } from 'url';

import { applySherpaLoaderEnv } from './sherpa-loader.js';

/** 单次 worker 请求的默认超时（毫秒）。 */
const DEFAULT_REQUEST_TIMEOUT_MS = 30000;
/** worker 空闲多久后自动关闭（毫秒）。 */
const DEFAULT_IDLE_TTL_MS = 5 * 60 * 1000;
/** 本地 STT 会话的默认采样率（Hz），worker 未回报时使用。 */
const DEFAULT_LOCAL_SAMPLE_RATE = 16000;
/** 保留的 worker stderr 尾部长度（字符数），用于崩溃诊断。 */
const STDERR_TAIL_MAX_CHARS = 2000;

/**
 * fork 出听写 worker 子进程：先应用 sherpa 动态库加载路径环境变量，
 * 再以 advanced 序列化（Buffer 可经 IPC 存活）fork，并捕获 stderr。
 */
function forkDictationWorker() {
  const env = { ...process.env };
  applySherpaLoaderEnv(env);
  return fork(fileURLToPath(new URL('./worker-process.js', import.meta.url)), [], {
    env,
    serialization: 'advanced',
    stdio: ['ignore', 'ignore', 'pipe', 'ipc'],
    windowsHide: true,
  });
}

/**
 * 与听写 worker 通信的客户端：懒启动子进程、请求-响应关联、
 * 会话事件分发与空闲自动关停。
 */
export class DictationWorkerClient {
  /**
   * @param {{ requestTimeoutMs?: number, idleTtlMs?: number }} [options]
   */
  /**
   * @param {{ requestTimeoutMs?: number, idleTtlMs?: number }} [options]
   *   覆盖默认请求超时与空闲关停 TTL。
   */
  constructor(options = {}) {
    this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.idleTtlMs = options.idleTtlMs ?? DEFAULT_IDLE_TTL_MS;
    this.pendingRequests = new Map();
    this.sessionEmitters = new Map();
    this.worker = null;
    this.stderrTail = '';
    this.inFlightRequests = 0;
    this.idleTimer = null;
    this.intentionalCloses = new WeakSet();
  }

  /**
   * Synthesize speech in the worker. Returns WAV bytes.
   * @param {{ modelsDir: string, modelId: string, text: string, speakerId?: number, speed?: number }} params
   * @returns {Promise<{ audio: Buffer, format: string }>}
   */
  /**
   * 在 worker 内合成语音并返回 WAV 音频；长文本合成在慢硬件上可能
   * 超过默认超时，因此单独放宽为 120 秒。
   */
  async synthesizeSpeech(params) {
    // Long texts on slow hardware can exceed the default request timeout.
    const result = await this.sendRequest(
      { type: 'tts.synthesize', ...params },
      { timeoutMs: 120000 },
    );
    return {
      audio: Buffer.isBuffer(result.audio) ? result.audio : Buffer.from(result.audio),
      format: result.format || 'audio/wav',
    };
  }

  /**
   * Create a streaming STT session in the worker.
   * @param {{ modelsDir: string, modelId: string }} params
   * @param {EventEmitter} emitter receives 'committed' | 'transcript' | 'error'
   * @returns {Promise<{ sessionId: string, requiredSampleRate: number }>}
   */
  /**
   * 在 worker 中创建流式 STT 会话：先注册事件 emitter，创建失败时
   * 清理注册并重新调度空闲关停，成功则返回会话 id 与要求的采样率。
   */
  async createSession({ modelsDir, modelId }, emitter) {
    const sessionId = randomUUID();
    this.sessionEmitters.set(sessionId, emitter);
    try {
      const result = await this.sendRequest({
        type: 'session.create',
        sessionId,
        modelsDir,
        modelId,
      });
      return { sessionId, requiredSampleRate: result?.requiredSampleRate ?? DEFAULT_LOCAL_SAMPLE_RATE };
    } catch (err) {
      this.sessionEmitters.delete(sessionId);
      this.scheduleIdleShutdownIfReady();
      throw err;
    }
  }

  /** 向会话追加 PCM16 音频；fire-and-forget，失败经会话 emitter 报 error。 */
  appendSessionAudio(sessionId, audio) {
    void this.sendRequest({ type: 'session.append', sessionId, audio }).catch((err) => {
      this.emitSessionError(sessionId, err);
    });
  }

  /** 提交会话当前分段；fire-and-forget，失败经会话 emitter 报 error。 */
  commitSession(sessionId) {
    void this.sendRequest({ type: 'session.commit', sessionId }).catch((err) => {
      this.emitSessionError(sessionId, err);
    });
  }

  /** 清空会话当前分段缓冲；fire-and-forget，失败经会话 emitter 报 error。 */
  clearSession(sessionId) {
    void this.sendRequest({ type: 'session.clear', sessionId }).catch((err) => {
      this.emitSessionError(sessionId, err);
    });
  }

  /**
   * 关闭会话：移除事件注册并以 best-effort 通知 worker（父进程已放弃
   * 该会话，失败可忽略），随后重新调度空闲关停。
   */
  closeSession(sessionId) {
    this.sessionEmitters.delete(sessionId);
    void this.sendRequest({ type: 'session.close', sessionId }).catch(() => {
      // Closing is best-effort; the parent already dropped the session.
    });
    this.scheduleIdleShutdownIfReady();
  }

  /**
   * 主动关闭 worker：先拒绝所有挂起请求并清空会话注册，再对子进程
   * disconnect + kill；kill 异常被吞掉以保证幂等。
   */
  shutdown() {
    this.clearIdleTimer();
    this.rejectAllPending(new Error('Dictation worker shut down'));
    this.sessionEmitters.clear();
    const worker = this.worker;
    this.worker = null;
    if (worker && !worker.killed) {
      this.intentionalCloses.add(worker);
      try {
        worker.disconnect();
      } catch {
        // ignore
      }
      try {
        worker.kill();
      } catch {
        // ignore
      }
    }
  }

  /**
   * 向 worker 发送带 requestId 的请求并返回 Promise：按 requestId 匹配
   * 响应，超时或 send 失败时拒绝并清理挂起状态；请求期间暂停空闲计时。
   */
  sendRequest(input, options = {}) {
    const worker = this.ensureWorker();
    const requestId = randomUUID();
    const message = { ...input, requestId };
    this.inFlightRequests += 1;
    this.clearIdleTimer();

    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pendingRequests.delete(requestId);
        this.inFlightRequests = Math.max(0, this.inFlightRequests - 1);
        this.scheduleIdleShutdownIfReady();
        reject(new Error(`Dictation worker request timed out: ${input.type}`));
      }, options.timeoutMs ?? this.requestTimeoutMs);

      this.pendingRequests.set(requestId, { resolve, reject, timeout });

      worker.send(message, (error) => {
        if (!error) {
          return;
        }
        const pending = this.pendingRequests.get(requestId);
        if (!pending) {
          return;
        }
        clearTimeout(pending.timeout);
        this.pendingRequests.delete(requestId);
        this.inFlightRequests = Math.max(0, this.inFlightRequests - 1);
        this.scheduleIdleShutdownIfReady();
        pending.reject(error);
      });
    });
  }

  /**
   * 确保 worker 存活（懒启动）：复用未杀死且 IPC 仍连接的实例，否则
   * 重新 fork、重置 stderr 尾部缓存并挂载 message/close 监听。
   */
  ensureWorker() {
    if (this.worker && !this.worker.killed && this.worker.connected) {
      return this.worker;
    }
    const worker = forkDictationWorker();
    this.worker = worker;
    this.stderrTail = '';
    worker.stderr?.on('data', (chunk) => {
      const text = Buffer.isBuffer(chunk) ? chunk.toString('utf8') : String(chunk);
      this.stderrTail = (this.stderrTail + text).slice(-STDERR_TAIL_MAX_CHARS);
    });
    worker.on('message', (message) => this.handleWorkerMessage(message));
    worker.on('close', (code, signal) => this.handleWorkerExit(worker, code, signal));
    return worker;
  }

  /**
   * 处理 worker 消息：type 为 'response' 时结算对应的挂起请求；
   * 否则按 sessionId 把会话事件（committed/transcript/error）转发给
   * 对应会话的 emitter。
   */
  handleWorkerMessage(message) {
    if (message?.type === 'response') {
      const pending = this.pendingRequests.get(message.requestId);
      if (!pending) {
        return;
      }
      clearTimeout(pending.timeout);
      this.pendingRequests.delete(message.requestId);
      this.inFlightRequests = Math.max(0, this.inFlightRequests - 1);
      this.scheduleIdleShutdownIfReady();
      if (message.ok) {
        pending.resolve(message.result);
      } else {
        pending.reject(new Error(message.error || 'Dictation worker request failed'));
      }
      return;
    }

    const emitter = this.sessionEmitters.get(message?.sessionId);
    if (!emitter) {
      return;
    }
    switch (message.type) {
      case 'session.committed':
        emitter.emit('committed', message.payload);
        return;
      case 'session.transcript':
        emitter.emit('transcript', message.payload);
        return;
      case 'session.error':
        emitter.emit('error', new Error(message.error));
        return;
      default:
    }
  }

  /**
   * worker 退出处理：主动关闭的退出只做清理；意外崩溃则携带 stderr
   * 尾部构造错误，拒绝全部挂起请求、向所有会话广播 error 并清空
   * 状态，下次请求会自动重新 fork。
   */
  handleWorkerExit(worker, code, signal) {
    const wasCurrentWorker = this.worker === worker;
    const wasIntentional = this.intentionalCloses.has(worker);
    this.intentionalCloses.delete(worker);
    if (!wasCurrentWorker || wasIntentional) {
      if (wasCurrentWorker) {
        this.worker = null;
      }
      return;
    }

    const stderr = this.stderrTail.trim();
    const error = new Error(
      `Dictation worker exited (code ${code ?? 'null'}${signal ? `, signal ${signal}` : ''}).` +
        (stderr ? ` Last stderr: ${stderr.slice(-500)}` : ''),
    );

    this.worker = null;
    this.clearIdleTimer();
    this.rejectAllPending(error);
    for (const emitter of this.sessionEmitters.values()) {
      if (emitter.listenerCount('error') > 0) {
        emitter.emit('error', error);
      }
    }
    this.sessionEmitters.clear();
    this.inFlightRequests = 0;
  }

  /** 以统一错误拒绝所有挂起请求并清除各自的超时定时器。 */
  rejectAllPending(error) {
    for (const [requestId, pending] of this.pendingRequests) {
      clearTimeout(pending.timeout);
      pending.reject(error);
      this.pendingRequests.delete(requestId);
    }
  }

  /** 向指定会话的 emitter 报 error（仅在有监听者时发出，避免抛出）。 */
  emitSessionError(sessionId, error) {
    const emitter = this.sessionEmitters.get(sessionId);
    if (emitter && emitter.listenerCount('error') > 0) {
      emitter.emit('error', error instanceof Error ? error : new Error(String(error)));
    }
  }

  /**
   * 当 worker 存活且没有在途请求与活动会话时，安排 idleTtlMs 后的
   * 自动关停；关停触发前再次确认仍处于空闲。
   */
  scheduleIdleShutdownIfReady() {
    if (!this.worker || this.inFlightRequests > 0 || this.sessionEmitters.size > 0) {
      return;
    }
    this.clearIdleTimer();
    this.idleTimer = setTimeout(() => {
      if (this.inFlightRequests === 0 && this.sessionEmitters.size === 0) {
        this.shutdown();
      }
    }, this.idleTtlMs);
  }

  /** 取消挂起的空闲关停定时器（新请求到达时调用）。 */
  clearIdleTimer() {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
  }
}

/**
 * StreamingTranscriptionSession backed by the worker process.
 * Matches the session contract consumed by DictationStreamManager.
 */
/**
 * 由 worker 进程支撑的 StreamingTranscriptionSession 实现：
 * 把契约方法转发给 DictationWorkerClient，worker 侧的会话事件由
 * client 回灌到本实例（本实例自身即会话 emitter）。
 */
export class WorkerBackedTranscriptionSession extends EventEmitter {
  /**
   * @param {DictationWorkerClient} client
   * @param {{ modelsDir: string, modelId: string }} modelConfig
   */
  /**
   * @param {DictationWorkerClient} client worker 客户端
   * @param {{ modelsDir: string, modelId: string }} modelConfig 模型定位配置
   */
  constructor(client, modelConfig) {
    super();
    this.client = client;
    this.modelConfig = modelConfig;
    this.requiredSampleRate = DEFAULT_LOCAL_SAMPLE_RATE;
    this.connectedSessionId = null;
    this.connecting = null;
  }

  /**
   * 连接：经 client 在 worker 中创建会话；并发调用共享同一次连接
   * Promise（connecting），成功后记录 sessionId 与实际要求的采样率。
   */
  async connect() {
    if (this.connectedSessionId) {
      return;
    }
    if (!this.connecting) {
      this.connecting = (async () => {
        try {
          const result = await this.client.createSession(this.modelConfig, this);
          this.connectedSessionId = result.sessionId;
          this.requiredSampleRate = result.requiredSampleRate;
        } finally {
          this.connecting = null;
        }
      })();
    }
    await this.connecting;
  }

  /** 追加 PCM16 音频；未连接时发出 'error' 事件。 */
  appendPcm16(pcm16le) {
    if (!this.connectedSessionId) {
      this.emit('error', new Error('Local STT session not connected'));
      return;
    }
    this.client.appendSessionAudio(this.connectedSessionId, pcm16le);
  }

  /** 提交当前分段；未连接时发出 'error' 事件。 */
  commit() {
    if (!this.connectedSessionId) {
      this.emit('error', new Error('Local STT session not connected'));
      return;
    }
    this.client.commitSession(this.connectedSessionId);
  }

  /** 清空当前分段（未连接时为安全空操作）。 */
  clear() {
    if (this.connectedSessionId) {
      this.client.clearSession(this.connectedSessionId);
    }
  }

  /** 关闭会话：先置空本地 sessionId 再通知 worker，保证幂等。 */
  close() {
    const sessionId = this.connectedSessionId;
    this.connectedSessionId = null;
    if (sessionId) {
      this.client.closeSession(sessionId);
    }
  }
}
