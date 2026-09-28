/**
 * DictationStreamManager（听写流管理器）的单元测试套件，运行于 bun:test。
 *
 * 用 FakeSttSession 模拟 StreamingTranscriptionSession 契约，覆盖：
 * 分块音频的乱序重排与提交、纯静音段的清理、三种分段策略（停顿切分、
 * 最短长度、硬上限）、partial/final 消息的产出，以及 provider 未就绪
 * 等错误路径。
 */
import { describe, it, expect } from 'bun:test';
import { EventEmitter } from 'events';

import { DictationStreamManager } from './stream-manager.js';

/** 测试用 PCM 音频格式串：16kHz / 16bit，与真实客户端上报的格式一致。 */
const FORMAT = 'audio/pcm;rate=16000;bits=16';

/**
 * 伪造的 STT 会话：实现 DictationStreamManager 依赖的
 * StreamingTranscriptionSession 接口（connect/appendPcm16/commit/clear/close），
 * commit 后异步发出 committed 与 transcript 事件，供测试断言调用次数与顺序。
 */
class FakeSttSession extends EventEmitter {
  /**
   * @param {{ transcriptBySegment?: (segmentId: string) => string }} [opts]
   *   每个分段返回的转写文本，默认恒为 'hello world'。
   */
  constructor({ transcriptBySegment = () => 'hello world' } = {}) {
    super();
    this.requiredSampleRate = 16000;
    this.appended = [];
    this.commits = 0;
    this.clears = 0;
    this.closed = false;
    this.segmentCounter = 0;
    this.transcriptBySegment = transcriptBySegment;
  }

  /** 模拟连接：无副作用。 */
  async connect() {}

  /** 记录追加的音频块，供断言乱序重排行为。 */
  appendPcm16(buf) {
    this.appended.push(buf);
  }

  /**
   * 提交当前分段：同步发出 committed 事件，并在下一个宏任务发出
   * isFinal 的 transcript 事件，模拟真实 provider 的异步转写回调。
   */
  commit() {
    this.commits += 1;
    const segmentId = `seg-${this.segmentCounter}`;
    this.segmentCounter += 1;
    this.emit('committed', { segmentId, previousSegmentId: null });
    setTimeout(() => {
      this.emit('transcript', {
        segmentId,
        transcript: this.transcriptBySegment(segmentId),
        isFinal: true,
      });
    }, 0);
  }

  /** 计数 clear 调用，对应静音段被丢弃时的清理路径。 */
  clear() {
    this.clears += 1;
  }

  /** 标记会话已关闭，供断言 finish 之后的资源回收。 */
  close() {
    this.closed = true;
  }
}

/**
 * 生成指定样本数、指定振幅的方波 PCM16 块并编码为 base64，
 * 模拟"有声"音频（峰值高于静音阈值）。
 */
function loudChunkBase64(samples = 1600, amplitude = 8000) {
  const arr = new Int16Array(samples);
  for (let i = 0; i < samples; i += 1) {
    arr[i] = i % 2 === 0 ? amplitude : -amplitude;
  }
  return Buffer.from(arr.buffer).toString('base64');
}

/** 生成全零 PCM16 块的 base64，模拟纯静音（峰值低于静音阈值）。 */
function silentChunkBase64(samples = 1600) {
  return Buffer.from(new Int16Array(samples).buffer).toString('base64');
}

/**
 * 构造绑定伪造 emit（收集消息）与固定 STT 会话的 DictationStreamManager。
 * @returns {{ manager: DictationStreamManager, messages: Array<object> }}
 */
function createManager(session) {
  const messages = [];
  const manager = new DictationStreamManager({
    emit: (msg) => messages.push(msg),
    createSttSession: async () => ({ session }),
  });
  return { manager, messages };
}

/**
 * 轮询等待断言条件成立（5ms 间隔），超时则 reject；用于等待异步转写结果。
 * @param {() => boolean} predicate 结束条件
 * @param {number} [timeoutMs] 超时毫秒数，默认 1000
 */
function waitFor(predicate, timeoutMs = 1000) {
  return new Promise((resolve, reject) => {
    const startedAt = Date.now();
    const tick = () => {
      if (predicate()) {
        resolve(undefined);
        return;
      }
      if (Date.now() - startedAt > timeoutMs) {
        reject(new Error('waitFor timed out'));
        return;
      }
      setTimeout(tick, 5);
    };
    tick();
  });
}

/** 覆盖 DictationStreamManager 的完整行为：乱序重排、静音清理、分段策略与错误路径。 */
describe('DictationStreamManager', () => {
  it('transcribes ordered chunks and emits final text', async () => {
    const session = new FakeSttSession();
    const { manager, messages } = createManager(session);

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64() });
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: loudChunkBase64() });
    manager.handleFinish('d1', 1);

    await waitFor(() => messages.some((m) => m.type === 'final'));

    const final = messages.find((m) => m.type === 'final');
    expect(final.payload.text).toBe('hello world');
    expect(session.commits).toBe(1);
    expect(session.closed).toBe(true);

    const acks = messages.filter((m) => m.type === 'ack');
    expect(acks[acks.length - 1].payload.ackSeq).toBe(1);
  });

  it('reorders out-of-order chunks before appending', async () => {
    const session = new FakeSttSession();
    const { manager, messages } = createManager(session);

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: loudChunkBase64() });
    expect(session.appended.length).toBe(0);
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64() });
    expect(session.appended.length).toBe(2);
    manager.handleFinish('d1', 1);

    await waitFor(() => messages.some((m) => m.type === 'final'));
  });

  it('clears silence-only tails instead of committing', async () => {
    const session = new FakeSttSession();
    const { manager, messages } = createManager(session);

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: silentChunkBase64() });
    manager.handleFinish('d1', 0);

    await waitFor(() => messages.some((m) => m.type === 'final'));

    const final = messages.find((m) => m.type === 'final');
    expect(final.payload.text).toBe('');
    expect(session.commits).toBe(0);
    expect(session.clears).toBe(1);
  });

  it('fails fast when finish arrives with no chunks', async () => {
    const session = new FakeSttSession();
    const { manager, messages } = createManager(session);

    await manager.handleStart('d1', FORMAT, {});
    manager.handleFinish('d1', 3);

    const error = messages.find((m) => m.type === 'error');
    expect(error).toBeDefined();
    expect(error.payload.retryable).toBe(true);
    expect(session.closed).toBe(true);
  });

  it('reports provider readiness errors from createSttSession', async () => {
    const messages = [];
    const manager = new DictationStreamManager({
      emit: (msg) => messages.push(msg),
      createSttSession: async () => ({
        error: 'Dictation model is downloading',
        retryable: true,
        reasonCode: 'model_download_in_progress',
      }),
    });

    await manager.handleStart('d1', FORMAT, {});
    const error = messages.find((m) => m.type === 'error');
    expect(error.payload.reasonCode).toBe('model_download_in_progress');
    expect(error.payload.retryable).toBe(true);
  });

  it('emits partials as segment transcripts arrive', async () => {
    let segment = 0;
    const session = new FakeSttSession({
      transcriptBySegment: () => {
        segment += 1;
        return segment === 1 ? 'first part' : 'second part';
      },
    });
    const { manager, messages } = createManager(session);
    // Force a hard-cap split after ~0.05s of audio so two segments form.
    manager.segmentMaxSeconds = 0.05;

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64(1600) });
    await waitFor(() => session.commits >= 1);
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: loudChunkBase64(1600) });
    manager.handleFinish('d1', 1);

    await waitFor(() => messages.some((m) => m.type === 'final'));

    const final = messages.find((m) => m.type === 'final');
    expect(final.payload.text).toBe('first part second part');
    const partials = messages.filter((m) => m.type === 'partial');
    expect(partials.length).toBeGreaterThan(0);
  });

  it('keeps a short dictation as one segment even across pauses', async () => {
    const session = new FakeSttSession();
    const { manager } = createManager(session);

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64(16000) });
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: silentChunkBase64(16000) });
    manager.handleChunk({ dictationId: 'd1', seq: 2, audioBase64: loudChunkBase64(16000) });

    expect(session.commits).toBe(0);

    manager.handleFinish('d1', 2);
    await waitFor(() => session.commits === 1);
  });

  it('splits at a pause once the segment passes the minimum length', async () => {
    const session = new FakeSttSession();
    const { manager } = createManager(session);
    manager.segmentMinSeconds = 3;

    await manager.handleStart('d1', FORMAT, {});
    // 2s of audio: below the minimum, so this pause must not split.
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64(16000) });
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: silentChunkBase64(16000) });
    expect(session.commits).toBe(0);

    // Past the minimum, the next quiet chunk is a segment boundary.
    manager.handleChunk({ dictationId: 'd1', seq: 2, audioBase64: loudChunkBase64(16000) });
    expect(session.commits).toBe(0);
    manager.handleChunk({ dictationId: 'd1', seq: 3, audioBase64: silentChunkBase64(16000) });
    expect(session.commits).toBe(1);
  });

  it('splits pauseless speech at the hard cap', async () => {
    const session = new FakeSttSession();
    const { manager } = createManager(session);
    manager.segmentMinSeconds = 60;
    manager.segmentMaxSeconds = 2;

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: loudChunkBase64(16000) });
    expect(session.commits).toBe(0);
    manager.handleChunk({ dictationId: 'd1', seq: 1, audioBase64: loudChunkBase64(16000) });
    expect(session.commits).toBe(1);
  });

  it('clears a silence-only segment at the hard cap instead of committing it', async () => {
    const session = new FakeSttSession();
    const { manager } = createManager(session);
    manager.segmentMaxSeconds = 1;

    await manager.handleStart('d1', FORMAT, {});
    manager.handleChunk({ dictationId: 'd1', seq: 0, audioBase64: silentChunkBase64(16000) });

    expect(session.commits).toBe(0);
    expect(session.clears).toBe(1);
  });
});
