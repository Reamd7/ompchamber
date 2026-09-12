import { describe, expect, it, vi } from 'vitest';
import { EventEmitter } from 'node:events';

import { createGlobalMessageStreamHub } from './global-hub.js';
import { createGlobalMessageStreamWsBridge } from './global-ws-bridge.js';
import { acceptDirectoryMessageStreamWsConnection } from './directory-ws-bridge.js';

// Cross-layer replay/resync contracts (docs/plan.md §5.4, phase-1 acceptance):
// byte-capped hub replay, distinguishable gap vs ok, upstream epoch change
// isolation, control-frame fan-out through both WS bridges.

function createSseResponse({ blocks = [], holdOpen = false, epoch = null } = {}) {
  const encoder = new TextEncoder();
  let index = 0;

  return {
    ok: true,
    headers: new Headers(epoch ? { 'x-omp-epoch': epoch } : {}),
    body: {
      getReader() {
        return {
          async read() {
            if (index < blocks.length) {
              return { value: encoder.encode(blocks[index++]), done: false };
            }
            if (holdOpen) {
              return new Promise(() => {});
            }
            return { value: undefined, done: true };
          },
        };
      },
    },
  };
}

async function waitForAssertion(assertion) {
  const deadline = Date.now() + 1000;
  let lastError;

  while (Date.now() < deadline) {
    try {
      assertion();
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  }

  throw lastError;
}

const eventBlock = (id, type, extra = '{}') =>
  `id: ${id}\ndata: {"type":"${type}","properties":${extra}}\n\n`;

describe('global hub replay caps and gap semantics', () => {
  it('bounds retained replay by bytes and entries simultaneously', async () => {
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      replayLimit: 4,
      replayMaxBytes: 900,
      fetchImpl: async () => createSseResponse({
        holdOpen: true,
        blocks: [
          eventBlock('evt-1', 'session.updated'),
          eventBlock('evt-2', 'session.updated', `{"directory":"/p","pad":"${'x'.repeat(400)}"}`),
          eventBlock('evt-3', 'session.updated', `{"directory":"/p","pad":"${'x'.repeat(400)}"}`),
          eventBlock('evt-4', 'session.updated'),
          eventBlock('evt-5', 'session.updated'),
        ],
      }),
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        const stats = hub.getStats();
        expect(stats.retainedEntries).toBeGreaterThan(0);
        expect(stats.retainedEntries).toBeLessThanOrEqual(4);
        expect(stats.retainedBytes).toBeLessThanOrEqual(900);
        expect(stats.evictedEntries).toBeGreaterThan(0);
      });
    } finally {
      hub.stop();
    }
  });

  it('replayAfter distinguishes ok suffixes from evicted gaps', async () => {
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      replayLimit: 2,
      fetchImpl: async () => createSseResponse({
        holdOpen: true,
        blocks: [eventBlock('evt-1', 'a'), eventBlock('evt-2', 'b'), eventBlock('evt-3', 'c')],
      }),
    });

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(hub.tailEventId()).toBe('evt-3');
      });
      expect(hub.replayAfter('evt-2')).toEqual({
        status: 'ok',
        events: [expect.objectContaining({ eventId: 'evt-3' })],
      });
      // evt-1 was evicted by the entry cap: gap, never a silent suffix.
      expect(hub.replayAfter('evt-1').status).toBe('gap');
      // Fresh cursor: ok with nothing to replay.
      expect(hub.replayAfter('')).toEqual({ status: 'ok', events: [] });
    } finally {
      hub.stop();
    }
  });
});

describe('global hub upstream controls and epoch isolation', () => {
  it('clears replay and notifies restart on an upstream resync control; controls never fan out', async () => {
    const events = [];
    const statuses = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      fetchImpl: async () => createSseResponse({
        holdOpen: true,
        blocks: [
          eventBlock('evt-1', 'session.updated'),
          'event: omp.stream.resync\nid: evt-9\n\n',
          eventBlock('evt-10', 'session.updated'),
        ],
      }),
    });
    hub.subscribeEvent((event) => events.push(event.eventId));
    hub.subscribeStatus((status) => statuses.push(status.type));

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(events).toEqual(['evt-1', 'evt-10']);
      });
      expect(statuses).toContain('restart');
      // The control itself is not a business event, and the replay only
      // holds post-control history.
      expect(hub.replayAfter('evt-1').status).toBe('gap');
      expect(hub.replayAfter('evt-9').status).toBe('gap');
      expect(hub.replayAfter('evt-10')).toEqual({ status: 'ok', events: [] });
      expect(hub.getStats().resyncs).toBe(1);
    } finally {
      hub.stop();
    }
  });

  it('does not fan out upstream boot frames', async () => {
    const events = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      fetchImpl: async () => createSseResponse({
        holdOpen: true,
        blocks: [
          'event: omp.stream.boot\ndata: {"epoch":"abc"}\n\n',
          eventBlock('evt-1', 'session.updated'),
        ],
      }),
    });
    hub.subscribeEvent((event) => events.push(event.eventId));

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(events).toEqual(['evt-1']);
      });
    } finally {
      hub.stop();
    }
  });

  it('clears replay and notifies restart when the upstream epoch changes', async () => {
    let attempt = 0;
    const statuses = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      fetchImpl: async () => {
        attempt += 1;
        return createSseResponse({
          epoch: attempt === 1 ? 'boot-a' : 'boot-b',
          holdOpen: attempt !== 1,
          blocks: attempt === 1 ? [eventBlock('evt-1', 'a'), eventBlock('evt-2', 'b')] : [eventBlock('evt-3', 'c')],
        });
      },
    });
    hub.subscribeStatus((status) => statuses.push(status));

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(hub.tailEventId()).toBe('evt-3');
      });
      // First connect learned boot-a without a restart; the reconnect saw a
      // different epoch → restart + replay isolation.
      expect(hub.getStats().upstreamEpoch).toBe('boot-b');
      expect(hub.getStats().restarts).toBe(1);
      expect(hub.replayAfter('evt-2').status).toBe('gap');
      expect(hub.replayAfter('evt-3')).toEqual({ status: 'ok', events: [] });
      const restart = statuses.find((status) => status.type === 'restart');
      expect(restart).toEqual(expect.objectContaining({ epoch: 'boot-b' }));
    } finally {
      hub.stop();
    }
  });

  it('treats an epoch change across stop/start as a restart — retained replay never survives it', async () => {
    // Regression: a fresh reader reports changed:false on its first connect,
    // so the hub must compare the NEW epoch against its own remembered one —
    // otherwise boot-a replay frames survive into the boot-b stream and
    // every stale client cursor is a false `ok` (plan §5.4).
    let epoch = 'boot-a';
    const statuses = [];
    const hub = createGlobalMessageStreamHub({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      upstreamReconnectDelayMs: 0,
      fetchImpl: async () => createSseResponse({
        epoch,
        holdOpen: true,
        blocks: epoch === 'boot-a' ? [eventBlock('evt-1', 'a')] : [eventBlock('evt-2', 'b')],
      }),
    });
    hub.subscribeStatus((status) => statuses.push(status));

    try {
      hub.start();
      await waitForAssertion(() => {
        expect(hub.tailEventId()).toBe('evt-1');
      });
      hub.stop(); // the stop the last WS client leaving performs
      epoch = 'boot-b';
      hub.start();
      await waitForAssertion(() => {
        expect(hub.tailEventId()).toBe('evt-2');
      });
      expect(hub.getStats().upstreamEpoch).toBe('boot-b');
      expect(hub.getStats().restarts).toBe(1);
      expect(hub.replayAfter('evt-1').status).toBe('gap'); // cross-boot cursor
      expect(statuses.some((status) => status.type === 'restart' && status.epoch === 'boot-b')).toBe(true);
    } finally {
      hub.stop();
    }
  });
});

const makeSocket = () => {
  const socket = new EventEmitter();
  socket.readyState = 1;
  socket.bufferedAmount = 0;
  socket.sent = [];
  socket.send = (payload) => {
    socket.sent.push(JSON.parse(payload));
  };
  socket.ping = () => {};
  socket.close = () => {
    socket.emit('close');
  };
  return socket;
};

describe('global WS bridge resync fan-out', () => {
  const makeHub = (replayResult, tail, epoch) => {
    let statusSubscriber = null;
    return {
      hub: {
        start: () => {},
        stop: () => {},
        isConnected: () => true,
        tailEventId: () => tail,
        getStats: () => ({ upstreamEpoch: epoch }),
        replayAfter: () => replayResult,
        subscribeEvent: () => () => {},
        subscribeStatus: (subscriber) => {
          statusSubscriber = subscriber;
          return () => {};
        },
      },
      notifyStatus: (status) => statusSubscriber?.(status),
    };
  };

  it('sends a resync control (not a suffix) when the requested id is a gap', () => {
    const { hub } = makeHub({ status: 'gap', events: [] }, 'evt-7', 'boot-x');
    const bridge = createGlobalMessageStreamWsBridge({
      globalHub: hub,
      ownsGlobalHub: false,
      wsClients: new Set(),
      processForwardedEventPayload: () => {},
      heartbeatIntervalMs: 60_000,
    });
    const socket = makeSocket();
    bridge.accept(socket, { requestedLastEventId: 'evt-2' });

    expect(socket.sent).toContainEqual({ type: 'ready', scope: 'global' });
    expect(socket.sent).toContainEqual({ type: 'resync', eventId: 'evt-7', epoch: 'boot-x' });
    // No event frames after a gap verdict — the client must reconcile.
    expect(socket.sent.filter((frame) => frame.type === 'event')).toHaveLength(0);
    bridge.close();
  });

  it('forces a resync when the client epoch predates the upstream epoch — even on a numerically-matching cursor', () => {
    // The cursor 'evt-2' WOULD replay ok under this boot's hub, but the
    // client learned it under 'old-boot': numeric ids restart per boot, so
    // only the epoch comparison disproves the resume (plan §5.2.1).
    const { hub } = makeHub(
      { status: 'ok', events: [{ eventId: 'evt-3', directory: '/p', payload: { type: 'session.updated' } }] },
      'evt-9',
      'boot-x',
    );
    const bridge = createGlobalMessageStreamWsBridge({
      globalHub: hub,
      ownsGlobalHub: false,
      wsClients: new Set(),
      processForwardedEventPayload: () => {},
      heartbeatIntervalMs: 60_000,
    });
    const socket = makeSocket();
    bridge.accept(socket, { requestedLastEventId: 'evt-2', requestedEpoch: 'old-boot' });

    expect(socket.sent).toContainEqual({ type: 'resync', eventId: 'evt-9', epoch: 'boot-x' });
    expect(socket.sent.filter((frame) => frame.type === 'event')).toHaveLength(0);
    bridge.close();
  });

  it('accepts a same-epoch resume and replays the suffix', () => {
    const { hub } = makeHub(
      { status: 'ok', events: [{ eventId: 'evt-3', directory: '/p', payload: { type: 'session.updated' } }] },
      'evt-3',
      'boot-x',
    );
    const bridge = createGlobalMessageStreamWsBridge({
      globalHub: hub,
      ownsGlobalHub: false,
      wsClients: new Set(),
      processForwardedEventPayload: () => {},
      heartbeatIntervalMs: 60_000,
    });
    const socket = makeSocket();
    bridge.accept(socket, { requestedLastEventId: 'evt-2', requestedEpoch: 'boot-x' });

    expect(socket.sent).toContainEqual({ type: 'ready', scope: 'global' });
    expect(socket.sent).toContainEqual({
      type: 'event',
      eventId: 'evt-3',
      directory: '/p',
      payload: { type: 'session.updated' },
    });
    expect(socket.sent.filter((frame) => frame.type === 'resync')).toHaveLength(0);
    bridge.close();
  });

  it('fans a restart status out to ready clients as resync controls', () => {
    const { hub, notifyStatus } = makeHub({ status: 'ok', events: [] }, 'evt-9', 'boot-y');
    const bridge = createGlobalMessageStreamWsBridge({
      globalHub: hub,
      ownsGlobalHub: false,
      wsClients: new Set(),
      processForwardedEventPayload: () => {},
      heartbeatIntervalMs: 60_000,
    });
    const socket = makeSocket();
    bridge.accept(socket, { requestedLastEventId: '' });
    notifyStatus({ type: 'restart', epoch: 'boot-z', reason: 'upstream-epoch-change' });

    expect(socket.sent).toContainEqual({ type: 'resync', eventId: 'evt-9', epoch: 'boot-z' });
    bridge.close();
  });
});

describe('directory WS bridge control forwarding', () => {
  it('relays a data-less omp.stream.resync as a resync control frame', async () => {
    const socket = makeSocket();
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    acceptDirectoryMessageStreamWsConnection({
      socket,
      requestedLastEventId: 'evt-5',
      requestedDirectory: '/tmp/project',
      buildOpenCodeUrl: (path) => `http://127.0.0.1:4096${path}`,
      getOpenCodeAuthHeaders: () => ({}),
      processForwardedEventPayload: () => {},
      wsClients: new Set(),
      heartbeatIntervalMs: 60_000,
      upstreamReconnectDelayMs: 0,
      fetchImpl: async () => createSseResponse({
        holdOpen: true,
        blocks: [
          'event: omp.stream.resync\nid: evt-9\n\n',
          'event: omp.stream.boot\ndata: {"epoch":"abc"}\n\n',
        ],
      }),
    });

    try {
      await waitForAssertion(() => {
        expect(socket.sent).toContainEqual({ type: 'ready', scope: 'directory' });
        expect(socket.sent).toContainEqual({ type: 'resync', eventId: 'evt-9' });
      });
      // The boot frame is transport metadata: never a business event frame.
      expect(socket.sent.filter((frame) => frame.type === 'event')).toHaveLength(0);
    } finally {
      socket.close();
      warnSpy.mockRestore();
    }
  });
});
