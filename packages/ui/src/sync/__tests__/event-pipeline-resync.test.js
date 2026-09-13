import { afterEach, describe, expect, it, mock } from 'bun:test';

// A WebSocket upgrade authenticates via a minted URL token; stub only that
// mint so the socket assertions exercise the transport rather than the auth
// round-trip (same approach as event-pipeline.test.js).
const actualRuntimeAuth = await import('@/lib/runtime-auth');
mock.module('@/lib/runtime-auth', () => ({
  ...actualRuntimeAuth,
  refreshRuntimeUrlAuthToken: async () => 'test-url-token',
}));

const { createEventPipeline } = await import('../event-pipeline');
// Resync control handling and local retention caps (docs/plan.md §5.2,
// §5.3.1): the pipeline must adopt sentinel cursors, learn the server boot
// epoch, bound its per-directory queues, and reconcile on overflow.

const originalDocument = globalThis.document;
const originalWindow = globalThis.window;
const originalWebSocket = globalThis.WebSocket;

function installDomStubs() {
  globalThis.document = {
    visibilityState: 'visible',
    addEventListener() {},
    removeEventListener() {},
  };

  globalThis.window = {
    location: {
      href: 'http://127.0.0.1:3000/',
      origin: 'http://127.0.0.1:3000',
    },
    addEventListener() {},
    removeEventListener() {},
  };
}

class FakeWebSocket {
  static instances = [];

  constructor(url) {
    this.url = url;
    this.readyState = 0;
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;
    FakeWebSocket.instances.push(this);
  }

  close() {
    this.readyState = 3;
  }

  emitOpen() {
    this.readyState = 1;
    this.onopen?.();
  }

  emitMessage(payload) {
    this.onmessage?.({ data: JSON.stringify(payload) });
  }
}

afterEach(() => {
  globalThis.document = originalDocument;
  globalThis.window = originalWindow;
  globalThis.WebSocket = originalWebSocket;
  FakeWebSocket.instances = [];
});

/**
 * SSE sdk stub: every attempt records its call args and plays a scripted
 * list of steps — `{ sse }` drives onSseEvent (control frames), `{ data }`
 * yields into the event stream. Intermediate scripts end their stream so
 * the reconnect loop advances; the final script holds open.
 */
const makeScriptedSseSdk = (scripts) => {
  const calls = [];
  const sdk = {
    global: {
      event: async (args) => {
        const isLast = calls.length >= scripts.length - 1;
        const script = scripts[Math.min(calls.length, scripts.length - 1)];
        calls.push(args);
        const stream = (async function* () {
          for (const step of script) {
            if (step.sse) args.onSseEvent?.(step.sse);
            if (step.data) yield step.data;
          }
          if (isLast) {
            await new Promise(() => {});
          }
        })();
        return { stream };
      },
    },
  };
  return { sdk, calls };
};

describe('createEventPipeline — SSE resync controls', () => {
  it('adopts the resync tail cursor, learns the boot epoch, and echoes both on reconnect', async () => {
    installDomStubs();
    const resyncs = [];
    const { sdk, calls } = makeScriptedSseSdk([
      [
        { sse: { event: 'omp.stream.boot', data: { epoch: 'boot-1' } } },
        { sse: { event: 'omp.stream.resync', id: '40' } },
        { data: { payload: { type: 'session.updated', properties: { info: { id: 's1' } } } } },
      ],
      [{ data: { payload: { type: 'server.connected', properties: {} } } }],
    ]);

    const { cleanup } = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      onEvent: () => {},
      onResync: (reason) => resyncs.push(reason),
    });

    await new Promise((resolve) => setTimeout(resolve, 30));
    cleanup();

    expect(resyncs).toEqual(['stream-resync']);
    expect(calls.length).toBeGreaterThanOrEqual(2);
    expect(calls[1].headers['Last-Event-ID']).toBe('40');
    expect(calls[1].headers['x-omp-epoch']).toBe('boot-1');
  });

  it('resync id 0 clears the cursor: the next connect resumes fresh', async () => {
    installDomStubs();
    const { sdk, calls } = makeScriptedSseSdk([
      [
        { sse: { event: 'omp.stream.boot', data: { epoch: 'boot-2' } } },
        { sse: { event: 'omp.stream.resync', id: '0' } },
      ],
      [{ data: { payload: { type: 'server.connected', properties: {} } } }],
    ]);

    const { cleanup } = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      onEvent: () => {},
      onResync: () => {},
    });

    await new Promise((resolve) => setTimeout(resolve, 30));
    cleanup();

    expect(calls.length).toBeGreaterThanOrEqual(2);
    // Fresh resume: no Last-Event-ID header at all (the headers object is
    // absent entirely when there is no cursor to send).
    expect(calls[1].headers ?? {}).not.toHaveProperty('Last-Event-ID');
  });
});

describe('createEventPipeline — WS resync frames', () => {
  it('echoes the learned epoch (not just the cursor) on the reconnect URL', async () => {
    installDomStubs();
    globalThis.WebSocket = FakeWebSocket;

    const { cleanup } = createEventPipeline({
      sdk: { global: { event: async () => { throw new Error('SSE should not be used'); } } },
      transport: 'ws',
      reconnectDelayMs: 0,
      onEvent: () => {},
      onResync: () => {},
    });

    await new Promise((resolve) => setTimeout(resolve, 50));
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.emitMessage({ type: 'ready', scope: 'global' });
    // The bridge's resync control teaches us the live epoch + tail cursor.
    socket.emitMessage({ type: 'resync', eventId: 'evt-9', epoch: 'boot-w' });
    socket.onclose?.({ code: 1006 });

    // The first reconnect retry waits RETRY_BACKOFF_BASE_MS (250ms) — give
    // the loop real time to dial the next socket.
    await new Promise((resolve) => setTimeout(resolve, 400));
    cleanup();

    const next = FakeWebSocket.instances[1];
    expect(next).toBeDefined();
    const url = new URL(next.url, 'ws://127.0.0.1');
    expect(url.searchParams.get('lastEventId')).toBe('evt-9');
    expect(url.searchParams.get('epoch')).toBe('boot-w');
  });

  it('adopts the resync control and keeps delivering live events', async () => {
    installDomStubs();
    globalThis.WebSocket = FakeWebSocket;

    const resyncs = [];
    const delivered = [];
    const { cleanup } = createEventPipeline({
      sdk: { global: { event: async () => { throw new Error('SSE should not be used'); } } },
      transport: 'ws',
      onEvent: (directory, payload) => delivered.push({ directory, payload }),
      onResync: (reason) => resyncs.push(reason),
    });

    await new Promise((resolve) => setTimeout(resolve, 50));
    const socket = FakeWebSocket.instances[0];
    expect(socket).toBeDefined();
    socket.emitOpen();
    socket.emitMessage({ type: 'ready', scope: 'global' });
    socket.emitMessage({ type: 'resync', eventId: 'evt-9', epoch: 'boot-w' });
    socket.emitMessage({
      type: 'event',
      eventId: 'evt-10',
      directory: '/tmp/project',
      payload: { type: 'session.status', properties: { sessionID: 'session-1' } },
    });

    await new Promise((resolve) => setTimeout(resolve, 80));
    cleanup();

    expect(resyncs).toEqual(['stream-resync']);
    expect(delivered).toEqual([
      {
        directory: '/tmp/project',
        payload: { type: 'session.status', properties: { sessionID: 'session-1' } },
      },
    ]);
  });
});

describe('createEventPipeline — local retention caps', () => {
  it('drops the queued window and reconciles on per-directory queue overflow', async () => {
    installDomStubs();
    const resyncs = [];
    const delivered = [];
    const events = [
      { payload: { type: 'installation.progress', properties: { i: 1 } } },
      { payload: { type: 'installation.progress', properties: { i: 2 } } },
      // One event larger than the 8MiB per-directory byte budget: the
      // estimator now counts the WHOLE payload, so an oversized event cannot
      // slip under the cap on a clamped estimate — and the overflow verdict
      // is deterministic regardless of flush timing.
      { payload: { type: 'installation.progress', properties: { blob: 'x'.repeat(9 * 1024 * 1024) } } },
      { payload: { type: 'installation.progress', properties: { i: 3 } } },
    ];
    const sdk = {
      global: {
        event: async () => ({
          stream: (async function* () {
            for (const event of events) {
              yield event;
            }
            await new Promise(() => {});
          })(),
        }),
      },
    };

    const pipeline = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      onEvents: (_directory, payloads) => delivered.push(...payloads),
      onResync: (reason) => resyncs.push(reason),
    });

    await new Promise((resolve) => setTimeout(resolve, 60));
    const stats = pipeline.stats();
    pipeline.cleanup();

    expect(resyncs).toEqual(['queue-overflow']);
    expect(stats.overflowResyncs).toBe(1);
    // Conservation: the two pending events AND the over-budget event were
    // dropped (an event that cannot fit an empty queue is never reseeded);
    // the successor delivered normally.
    expect(delivered).toHaveLength(1);
    expect(stats.droppedEvents).toBe(3);
  });

  it('caps tracked directories against unbounded fan-out and reconciles on the first drop', async () => {
    installDomStubs();
    const resyncs = [];
    const delivered = [];
    const events = [];
    for (let i = 0; i < 100; i += 1) {
      events.push({ payload: { type: 'installation.progress', properties: { dir: `dir-${i}` } } });
    }
    const sdk = {
      global: {
        event: async () => ({
          stream: (async function* () {
            for (const event of events) {
              yield event;
            }
            await new Promise(() => {});
          })(),
        }),
      },
    };

    const pipeline = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      routeDirectory: (_directory, payload) => payload.properties.dir,
      onEvents: (directory, payloads) => delivered.push([directory, payloads.length]),
      onResync: (reason) => resyncs.push(reason),
    });

    // All 100 events land inside one flush window: 64 directories track
    // pending work, the remaining 36 drop — and the FIRST drop asks the
    // consumer to reconcile instead of losing the directory silently.
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(resyncs.filter((r) => r === 'queue-overflow').length).toBe(1);
    // Conservation: every event was either delivered or counted dropped.
    const deliveredCount = delivered.reduce((total, [, count]) => total + count, 0);
    const stats = pipeline.stats();
    expect(deliveredCount + stats.droppedEvents).toBe(100);
    expect(stats.droppedEvents).toBe(36);
    pipeline.cleanup();
  });

  it('releases a directory record once its queue drains (cap bounds pending fan-out, not history)', async () => {
    installDomStubs();
    const delivered = [];
    const sdk = {
      global: {
        event: async () => ({
          stream: (async function* () {
            for (let i = 0; i < 30; i += 1) {
              yield { payload: { type: 'installation.progress', properties: { dir: `dir-${i}` } } };
            }
            await new Promise(() => {});
          })(),
        }),
      },
    };

    const pipeline = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      routeDirectory: (_directory, payload) => payload.properties.dir,
      onEvents: (directory, payloads) => delivered.push([directory, payloads.length]),
    });

    // 30 directories enqueue, flush inside the frame window, and release —
    // the next 40 directories must be tracked too (old behavior pinned every
    // directory ever seen, so 64 was a lifetime ceiling).
    await new Promise((resolve) => setTimeout(resolve, 60));
    const stats = pipeline.stats();
    pipeline.cleanup();

    expect(delivered.length).toBe(30);
    expect(stats.trackedDirectories).toBe(0);
  });

  it('cleanup clears every directory queue and timer', async () => {
    installDomStubs();
    const sdk = {
      global: {
        event: async () => ({
          stream: (async function* () {
            yield { payload: { type: 'installation.progress', properties: { i: 1 } } };
            await new Promise(() => {});
          })(),
        }),
      },
    };

    const pipeline = createEventPipeline({
      sdk,
      transport: 'sse',
      reconnectDelayMs: 0,
      heartbeatTimeoutMs: 60_000,
      onEvent: () => {},
    });

    await new Promise((resolve) => setTimeout(resolve, 50));
    pipeline.cleanup();

    const stats = pipeline.stats();
    expect(stats.trackedDirectories).toBe(0);
    expect(stats.queuedEvents).toBe(0);
  });
});
