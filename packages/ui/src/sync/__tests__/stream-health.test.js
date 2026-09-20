import { afterEach, beforeEach, describe, expect, it } from 'bun:test';

const { createEventPipeline } = await import('../event-pipeline');
const { getStreamHealth, resetStreamHealthForTest } = await import('../stream-health');

// Stream liveness telemetry: the wire pipeline must publish its lifecycle
// transitions, wire-frame receipts, delivered-event counts, and resync
// controls so the engine status report can distinguish a healthy stream
// (fresh frames + fresh deliveries) from a live-but-deaf one (frames such as
// heartbeats arriving while no events are delivered) and from a wedged
// transport (both stale).

const originalDocument = globalThis.document;
const originalWindow = globalThis.window;

function installDomStubs() {
  globalThis.document = {
    visibilityState: 'visible',
    addEventListener() {},
    removeEventListener() {},
  };
  globalThis.window = {
    location: { href: 'http://127.0.0.1:3000/', origin: 'http://127.0.0.1:3000' },
    addEventListener() {},
    removeEventListener() {},
  };
}

/**
 * SSE sdk stub. `scripts` entries: `{ reject: Error }` fails the attempt,
 * an array of `{ sse }`/`{ data }` steps drives one connected stream.
 * Non-final scripts end their stream so the reconnect loop advances; the
 * final script holds open.
 */
const makeScriptedSseSdk = (scripts) => {
  const calls = [];
  const sdk = {
    global: {
      event: async (args) => {
        const index = calls.length;
        calls.push(args);
        const script = scripts[Math.min(index, scripts.length - 1)];
        if (script.reject) {
          throw script.reject;
        }
        const isLast = index >= scripts.length - 1;
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

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const eventOf = (type) => ({ type, properties: {} });

beforeEach(() => {
  installDomStubs();
  resetStreamHealthForTest();
});

afterEach(() => {
  globalThis.document = originalDocument;
  globalThis.window = originalWindow;
});

describe('stream-health telemetry from the wire pipeline', () => {
  it('records connecting at mount and connected with transport after first connect', async () => {
    const { sdk } = makeScriptedSseSdk([[{ data: eventOf('session.updated') }]]);
    const pipeline = createEventPipeline({ sdk, transport: 'sse', onEvent: () => {} });
    try {
      expect(getStreamHealth().status).toBe('connecting');
      await wait(80);
      const health = getStreamHealth();
      expect(health.status).toBe('connected');
      expect(health.transport).toBe('sse');
      expect(health.connectedAt).toBeGreaterThan(0);
      expect(health.lastWireFrameAt).toBeGreaterThan(0);
      expect(health.lastDeliveredEventsAt).toBeGreaterThan(0);
      expect(health.deliveredEvents).toBe(1);
    } finally {
      pipeline.cleanup();
    }
    expect(getStreamHealth().status).toBe('idle');
  });

  it('records reconnecting with a reason when an attempt fails, then recovers', async () => {
    const { sdk } = makeScriptedSseSdk([
      { reject: new Error('fetch failed') },
      [{ data: eventOf('session.updated') }],
    ]);
    const pipeline = createEventPipeline({ sdk, transport: 'sse', onEvent: () => {}, reconnectDelayMs: 10 });
    try {
      await wait(450);
      const health = getStreamHealth();
      expect(health.status).toBe('connected');
      expect(health.lastDisconnectAt).toBeGreaterThan(0);
      expect(health.lastDisconnectReason).toContain('sse_error');
    } finally {
      pipeline.cleanup();
    }
  });

  it('records stream-resync controls and forwards them to the consumer', async () => {
    const resyncs = [];
    const { sdk } = makeScriptedSseSdk([
      [
        { sse: { event: 'omp.stream.resync', id: '40' } },
        { data: eventOf('session.updated') },
      ],
    ]);
    const pipeline = createEventPipeline({
      sdk,
      transport: 'sse',
      onEvent: () => {},
      onResync: (reason) => resyncs.push(reason),
    });
    try {
      await wait(80);
      const health = getStreamHealth();
      expect(resyncs).toEqual(['stream-resync']);
      expect(health.resyncs).toBe(1);
      expect(health.lastResyncReason).toBe('stream-resync');
      expect(health.lastResyncAt).toBeGreaterThan(0);
    } finally {
      pipeline.cleanup();
    }
  });
});
