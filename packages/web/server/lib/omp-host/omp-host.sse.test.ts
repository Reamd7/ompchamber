/**
 * SSE 端点契约测试（docs/plan.md §5.2/§5.2.1、验收 §10.1）：gap/restart/
 * epoch 续订语义、带内控制帧、订阅者清理、慢消费者关闭与只报计数器的
 * 诊断口。
 */
import { describe, expect, test } from 'vitest';
import { OmpEventBus, WireEventBus } from './events.ts';
import { registerEndpoints } from './endpoints.ts';
import type { OmpHostEngine } from './engine.ts';
import type { RouteHandler } from './endpoints.ts';

// SSE endpoint contracts (docs/plan.md §5.2/§5.2.1, acceptance §10.1):
// gap/restart/epoch resume semantics, in-band controls, subscriber cleanup,
// slow-consumer close, and the counters-only diagnostics port.

// SAFETY: endpoint tests install only the members the mounted routes call;
// the double is a duck-typed partial, not a full engine.
/** SAFETY：端点测试只装配被挂载路由会调用的成员；替身是鸭子类型的部分
 * 面，不是完整引擎。 */
const asEngineDouble = <T,>(double: T): OmpHostEngine => double as OmpHostEngine;

/** mountSse 的返回面：两条总线与三个可直调的 handler。 */
interface MountedSse {
  // wire 事件总线。
  wireBus: WireEventBus;
  // omp 原生事件总线。
  ompBus: OmpEventBus;
  // wire /event 的 SSE handler（带 global 开关）。
  sseHandler: (request: Request, options: { global: boolean }) => Response;
  // /omp/events 路由 handler。
  ompEventsHandler: RouteHandler;
  // /omp/diagnostics 路由 handler。
  diagnosticsHandler: RouteHandler;
}

/** 装配小容量双总线与桩引擎，挂载全部端点并挑出三个 SSE 相关 handler。 */
const mountSse = (): MountedSse => {
  const wireBus = new WireEventBus({ capacity: 8, maxBytes: 64 * 1024, maxEventBytes: 512 * 1024 });
  const ompBus = new OmpEventBus({ capacity: 8, maxBytes: 64 * 1024, maxEventBytes: 512 * 1024 });
  const routes: Array<{ method: string; pattern: string; handler: RouteHandler }> = [];
  const route = (method: string, pattern: string, handler: RouteHandler) => routes.push({ method, pattern, handler });
  const getStreamDiagnostics = () => ({
    wireBus: wireBus.stats(),
    ompBus: ompBus.stats(),
    dataProportional: { liveSessions: 0, wireIdEchoes: 0, personas: 0 },
    process: { heapUsedBytes: 0, externalBytes: 0, arrayBufferBytes: 0, rssBytes: 0 },
  });
  const { sseHandler } = registerEndpoints(
    route,
    asEngineDouble({
      bus: wireBus,
      ompBus,
      getStreamDiagnostics,
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      processDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
    }),
    { version: 'test' },
  );
  const find = (pattern: string): RouteHandler => {
    const entry = routes.find((r) => r.pattern === pattern);
    if (!entry) throw new Error(`route not mounted: ${pattern}`);
    return entry.handler;
  };
  return {
    wireBus,
    ompBus,
    sseHandler,
    ompEventsHandler: find('/omp/events'),
    diagnosticsHandler: find('/omp/diagnostics'),
  };
};

/** 构造带 AbortController 的 SSE 请求（测试可随时掐断连接）。 */
const sseRequest = (path: string, headers: Record<string, string> = {}) => {
  const abort = new AbortController();
  const request = new Request(`http://host${path}`, { headers, signal: abort.signal });
  return { request, abort };
};

/** 读到 predicate 命中为止：端点起始帧（boot/控制/重放）在响应 start()
 * 内同步入队，因此连接后发出的哨兵事件能在无定时器的情况下确定终止
 * 读取（FIFO 保证哨兵之前的帧都已到达）。 */
/**
 * Read until `predicate` matches. Initial endpoint frames (boot, control,
 * replay) are enqueued synchronously during the response's start(), so a
 * sentinel event emitted after connect terminates the read deterministically
 * without timers: FIFO order guarantees every pre-sentinel frame arrived.
 */
const readUntil = async (response: Response, predicate: (text: string) => boolean, maxChunks = 64): Promise<string> => {
  if (!response.body) throw new Error('no stream body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  for (let i = 0; i < maxChunks && !predicate(text); i += 1) {
    const { value, done } = await reader.read();
    if (done) break;
    if (value) text += decoder.decode(value, { stream: true });
  }
  return text;
};

/** 把已关闭的流一路读到尽头（慢消费者关闭契约用）。 */
/** Drain a closed stream to its end (for the slow-consumer close contract). */
const drainToEnd = async (response: Response): Promise<void> => {
  const reader = response.body!.getReader();
  for (;;) {
    const { done } = await reader.read();
    if (done) return;
  }
};

/** 把 chunk 按空行切分为 SSE 帧。 */
const frames = (chunk: string): string[] => chunk.split('\n\n').filter((frame) => frame.length > 0);

/** 取帧的 event: 名（不依赖 id:/event: 行的先后顺序）。 */
/** SSE event name of a frame, regardless of `id:`/`event:` line order. */
const frameEvent = (frame: string): string | null => {
  const line = frame.split('\n').find((candidate) => candidate.startsWith('event: '));
  return line ? line.slice('event: '.length) : null;
};

/** 取帧的 data: 行内容。 */
const dataLine = (frame: string): string => frame.split('\n').find((line) => line.startsWith('data: '))!.slice('data: '.length);

/** 读出连接后的起始帧突发（读到 test.sentinel 为止）。 */
const readInitialBurst = (response: Response): Promise<string> => readUntil(response, (text) => text.includes('test.sentinel'));

/** 直调 /omp/events handler 并把结果断言为 Response（该路由恒有应答）。 */
const runOmpEvents = async (mounted: MountedSse, request: Request): Promise<Response> =>
  // SAFETY: RouteHandler resolves Response | void; the omp/events route always
  // responds. asEngineDouble stands in for the unused engine member.
  (await mounted.ompEventsHandler(request, { params: {}, url: new URL(request.url), headers: request.headers, engine: asEngineDouble({}) })) as Response;

/** wire /event SSE 续订：新连接的 epoch 头 + boot 帧 + 全量重放；gap/
 * restart/epoch 不匹配一律下发无数据的 resync 控制帧（携带真实 tail id），
 * 绝不静默只发后缀；abort/慢消费者/预中止流的订阅清理。 */
describe('wire /event SSE resume', () => {
  test('fresh connect: epoch header + boot frame + full replay, no control', async () => {
    const mounted = mountSse();
    mounted.wireBus.emit('message.updated', { i: 1 }, '/a');
    mounted.wireBus.emit('session.idle', { sessionID: 's' }, '/a');
    const { request } = sseRequest('/event');
    const response = mounted.sseHandler(request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');

    expect(response.headers.get('x-omp-epoch')).toBe(mounted.wireBus.epoch);
    const list = frames(await readInitialBurst(response));
    expect(frameEvent(list[0])).toBe('omp.stream.boot');
    expect(JSON.parse(dataLine(list[0]))).toEqual({ epoch: mounted.wireBus.epoch });
    expect(list.map(frameEvent)).toEqual(['omp.stream.boot', 'message.updated', 'session.idle', 'test.sentinel']);
  });

  test('gap resume: data-less resync control with the real tail, no silent suffix', async () => {
    const mounted = mountSse();
    for (let i = 0; i < 12; i += 1) mounted.wireBus.emit('message.updated', { i }, '/a'); // ring keeps 5..12
    const { request } = sseRequest('/event', { 'last-event-id': '1', 'x-omp-epoch': mounted.wireBus.epoch });
    const tail = mounted.wireBus.tailEventId();
    const response = mounted.sseHandler(request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');
    const list = frames(await readInitialBurst(response));

    const control = list.find((frame) => frameEvent(frame) === 'omp.stream.resync');
    expect(control).toBeDefined();
    expect(control).not.toContain('data:'); // in-band wire control carries no payload
    expect(control).toContain(`id: ${tail}`); // reconnectable tail, not a phantom id
    // Baseline switched to the tail: none of the retained suffix replays.
    expect(list.filter((frame) => frameEvent(frame) === 'message.updated')).toHaveLength(0);
  });

  test('restart resume (cursor ahead of boot) resyncs with the tail id', async () => {
    const mounted = mountSse();
    mounted.wireBus.emit('message.updated', { i: 1 }, '/a');
    const { request } = sseRequest('/event', { 'last-event-id': '99', 'x-omp-epoch': mounted.wireBus.epoch });
    const response = mounted.sseHandler(request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');
    const control = frames(await readInitialBurst(response)).find((frame) => frameEvent(frame) === 'omp.stream.resync');
    expect(control).toBeDefined();
    expect(control).toContain('id: 1');
  });

  test('empty ring resync control sends id: 0 to clear stale cursors', async () => {
    const mounted = mountSse();
    // Fresh boot, nothing emitted; a resuming cursor cannot belong here.
    const { request } = sseRequest('/event', { 'last-event-id': '5', 'x-omp-epoch': mounted.wireBus.epoch });
    const response = mounted.sseHandler(request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');
    const control = frames(await readInitialBurst(response)).find((frame) => frameEvent(frame) === 'omp.stream.resync');
    expect(control).toBeDefined();
    expect(control).toContain('id: 0');
  });

  test('epoch mismatch downgrades to resync; matching epoch replays the suffix', async () => {
    const mounted = mountSse();
    mounted.wireBus.emit('message.updated', { i: 1 }, '/a');
    mounted.wireBus.emit('message.updated', { i: 2 }, '/a');

    // Cursor 1 is provable, but the epoch header is from another boot.
    const stale = sseRequest('/event', { 'last-event-id': '1', 'x-omp-epoch': 'older-boot' });
    const staleResponse = mounted.sseHandler(stale.request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');
    const staleList = frames(await readInitialBurst(staleResponse));
    expect(staleList.some((frame) => frameEvent(frame) === 'omp.stream.resync')).toBe(true);
    expect(staleList.filter((frame) => frameEvent(frame) === 'message.updated')).toHaveLength(0);

    // Same cursor with this boot's epoch replays exactly the suffix.
    const good = sseRequest('/event', { 'last-event-id': '1', 'x-omp-epoch': mounted.wireBus.epoch });
    const goodResponse = mounted.sseHandler(good.request, { global: true });
    mounted.wireBus.emit('test.sentinel', {}, '/a');
    const goodList = frames(await readInitialBurst(goodResponse));
    const replayed = goodList.filter((frame) => frameEvent(frame) === 'message.updated');
    expect(replayed).toHaveLength(1);
    expect(JSON.parse(dataLine(replayed[0])).properties.i).toBe(2);
    expect(goodList.some((frame) => frameEvent(frame) === 'omp.stream.resync')).toBe(false);
  });

  test('abort unsubscribes: subscriber count returns to zero', async () => {
    const mounted = mountSse();
    const { request, abort } = sseRequest('/event');
    mounted.sseHandler(request, { global: true });
    expect(mounted.wireBus.stats().subscribers).toBe(1);
    abort.abort();
    expect(mounted.wireBus.stats().subscribers).toBe(0);
  });

  test('dead-slow consumer is closed and unsubscribed once queued BYTES cross the cap', async () => {
    const mounted = mountSse();
    const { request } = sseRequest('/event');
    const response = mounted.sseHandler(request, { global: true });
    // Never read the stream while the bus floods it. The bound is serialized
    // bytes (desiredSize with a byteLength size strategy): sixteen ~1.5MB
    // frames cross 16MiB of queued backlog even though the chunk count is
    // trivially small — under the old count-strategy code this stayed open.
    for (let i = 0; i < 16; i += 1) {
      mounted.wireBus.emit('message.part.updated', { part: { output: 'x'.repeat(1_500_000) } }, '/a');
    }
    expect(mounted.wireBus.stats().subscribers).toBe(0);
    // The stream terminates (done) rather than holding a growing queue.
    await drainToEnd(response);
  });

  test('a stream that is already aborted before connect never leaves a subscriber behind', async () => {
    const mounted = mountSse();
    const { request, abort } = sseRequest('/event');
    abort.abort();
    mounted.sseHandler(request, { global: true });
    // The abort listener is attached after the synchronous initial replay;
    // an already-aborted request must still reach finish() and unsubscribe.
    expect(mounted.wireBus.stats().subscribers).toBe(0);
  });

  test('a fast-draining consumer is not killed by chunk count alone', async () => {
    const mounted = mountSse();
    const { request, abort } = sseRequest('/event');
    mounted.sseHandler(request, { global: true });
    // Thousands of small frames queue well under the byte bound — the
    // subscriber survives (the count-based cap would have closed it).
    for (let i = 0; i < 2000; i += 1) mounted.wireBus.emit('message.updated', { i }, '/a');
    expect(mounted.wireBus.stats().subscribers).toBe(1);
    abort.abort();
    expect(mounted.wireBus.stats().subscribers).toBe(0);
  });
});

/** omp /omp/events SSE 续订：ok 判定重放持久后缀；gap 的 resync 信封携带
 * 真实 tail（id/resumeFrom）而非幽灵 next id。 */
describe('omp /omp/events SSE resume', () => {
  test('ok resume replays the durable suffix', async () => {
    const mounted = mountSse();
    mounted.ompBus.publish('omp.notice.raised', { i: 1 }, { directory: '/a', durable: true });
    mounted.ompBus.publish('omp.notice.raised', { i: 2 }, { directory: '/a', durable: true });
    const { request } = sseRequest('/omp/events', { 'last-event-id': '1', 'x-omp-epoch': mounted.ompBus.epoch });
    const response = await runOmpEvents(mounted, request);
    mounted.ompBus.publish('test.sentinel', {}, { directory: '/a', durable: true });
    const list = frames(await readInitialBurst(response));
    const events = list.filter((frame) => frameEvent(frame) === 'omp.notice.raised');
    expect(events).toHaveLength(1);
    expect(JSON.parse(dataLine(events[0])).payload.i).toBe(2);
    expect(list.some((frame) => frameEvent(frame) === 'omp.stream.resync')).toBe(false);
  });

  test('gap resync envelope carries the real tail as id/resumeFrom, not the phantom next id', async () => {
    const mounted = mountSse();
    for (let i = 0; i < 12; i += 1) mounted.ompBus.publish('omp.notice.raised', { i }, { directory: '/a', durable: true });
    expect(mounted.ompBus.tailEventId()).toBe(12);
    const { request } = sseRequest('/omp/events', { 'last-event-id': '1', 'x-omp-epoch': mounted.ompBus.epoch });
    const response = await runOmpEvents(mounted, request);
    mounted.ompBus.publish('test.sentinel', {}, { directory: '/a', durable: true });
    const list = frames(await readInitialBurst(response));
    const control = list.find((frame) => frameEvent(frame) === 'omp.stream.resync');
    expect(control).toBeDefined();
    const envelope = JSON.parse(dataLine(control!));
    expect(envelope.id).toBe(12);
    expect(envelope.payload.resumeFrom).toBe(12);
    expect(envelope.payload.lastEventId).toBe(1); // diagnostics: what the client asked from
    expect(envelope.payload.reason).toBe('gap');
    expect(envelope.id).not.toBe(mounted.ompBus.nextEventId); // phantom ids re-trigger resync loops
    // Baseline is the tail: no suffix replay after the control.
    expect(list.filter((frame) => frameEvent(frame) === 'omp.notice.raised')).toHaveLength(0);
  });
});

/** omp 诊断口：只返回计数器——两条总线的 stats、与数据量成比例的计数及
 * 进程内存，不暴露任何条目内容。 */
describe('omp diagnostics port', () => {
  test('returns counters only: bus stats, data-proportional counts, process memory', async () => {
    const mounted = mountSse();
    mounted.wireBus.emit('message.updated', { i: 1 }, '/a');
    const { request } = sseRequest('/omp/diagnostics');
    // SAFETY: RouteHandler resolves Response | void; diagnostics always
    // responds, and asEngineDouble stands in for the unused engine member.
    const response = (await mounted.diagnosticsHandler(request, { params: {}, url: new URL(request.url), headers: request.headers, engine: asEngineDouble({}) })) as Response;
    // SAFETY: the diagnostics payload shape is the contract under test.
    const payload = (await response.json()) as {
      wireBus: { epoch: string; retainedEntries: number; subscribers: number };
      ompBus: { capacity: number };
      dataProportional: { liveSessions: number };
      process: { heapUsedBytes: number };
    };
    expect(payload.wireBus.epoch).toBe(mounted.wireBus.epoch);
    expect(payload.wireBus.retainedEntries).toBe(1);
    expect(payload.wireBus.subscribers).toBe(0);
    expect(payload.ompBus.capacity).toBeGreaterThan(0);
    expect(payload.dataProportional.liveSessions).toBe(0);
    expect(payload.process.heapUsedBytes).toBeGreaterThanOrEqual(0);
    // No transcript/payload content: the payload is counters and declared
    // estimates only (decision D9).
    expect(JSON.stringify(payload)).not.toContain('message.updated');
  });
});
