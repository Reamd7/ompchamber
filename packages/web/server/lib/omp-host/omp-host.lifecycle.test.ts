import { describe, expect, mock, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { OmpHostEngine } from './engine.ts';

// Live-session lifecycle matrix (docs/plan.md §3.2-3.4/§4, acceptance §10.1
// "live 生命周期"): directory keys, eviction state machine, awaited
// disposal, quarantine tombstones, activity guards, delete/move/shutdown
// serialization. The engine gets a fake monotonic clock and an injected
// agent factory; SessionManager is module-mocked like omp-host.engine.test.
// The engine VALUE loads dynamically after mock.module so it binds the
// mocked SDK surface (same seam as omp-host.engine.test.ts).
const realSdk = await import('@oh-my-pi/pi-coding-agent');
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');

// Per-harness state the top-level module mocks close over (registered before
// any engine import so bun's mock.module applies to engine.ts's bindings).
interface LifecycleMockState {
  agentDir: string;
  files: Array<{ path: string; cwd: string }>;
}
const mockState: LifecycleMockState = {
  agentDir: '',
  files: [],
};

mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async () => {},
}));

mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {},
  ModelRegistry: class {
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'm1' }];
    }
  },
  SessionManager: Object.assign(
    class {},
    {
      async open(file: string) {
        return {
          getSessionId: () => path.basename(file, '.jsonl'),
          onSessionNameChanged: () => () => {},
          getHeader: (): null => null,
          getEntries: () => [],
          buildSessionContext: () => ({ messages: [] }),
          getCwd: () => undefined,
          getSessionName: () => undefined,
          getArtifactsDir: () => file.slice(0, -'.jsonl'.length),
          close: async () => {},
          moveTo: async () => {},
        };
      },
      async list(cwd?: string) {
        return mockState.files
          .filter((file) => !cwd || file.cwd === cwd)
          .map((file) => ({ id: path.basename(file.path, '.jsonl'), path: file.path }));
      },
      getDefaultSessionDir: () => path.join(mockState.agentDir, 'sessions'),
      async forkFrom() {
        throw new Error('not needed');
      },
      createEmptySessionFile: () => 'unused',
    },
  ),
  createAgentSession: async () => {
    throw new Error('injected factory must be used');
  },
}));
mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async () => {},
}));

interface FixtureSession {
  sessionManager: {
    getSessionName: () => string;
    getArtifactsDir: () => string;
    onSessionNameChanged: (cb: () => void) => (() => void) | undefined;
  };
  model: { provider: string; id: string };
  messages: unknown[];
  isStreaming: boolean;
  isAborting: boolean;
  isRetrying: boolean;
  isCompacting: boolean;
  isGeneratingHandoff: boolean;
  isBashRunning: boolean;
  isEvalRunning: boolean;
  hasPendingBashMessages: boolean;
  hasPendingPythonMessages: boolean;
  hasPostPromptWork: boolean;
  queuedMessageCount: number;
  subscribe: () => () => void;
  hasPendingAsyncWork: () => boolean;
  beginDispose: () => void;
  dispose: (options?: { drainTimeoutMs?: number }) => Promise<void>;
  prompt: () => Promise<boolean>;
  abort: () => Promise<void>;
  maybeStartTitleGeneration: () => void;
}

const makeFixtureSession = (id: string, dir: string): FixtureSession => ({
  sessionManager: {
    getSessionName: () => `name-${id}`,
    getArtifactsDir: () => path.join(dir, id),
    onSessionNameChanged: () => () => {},
  },
  model: { provider: 'p1', id: 'm1' },
  messages: [],
  isStreaming: false,
  isAborting: false,
  isRetrying: false,
  isCompacting: false,
  isGeneratingHandoff: false,
  isBashRunning: false,
  isEvalRunning: false,
  hasPendingBashMessages: false,
  hasPendingPythonMessages: false,
  hasPostPromptWork: false,
  queuedMessageCount: 0,
  subscribe: () => () => {},
  hasPendingAsyncWork: () => false,
  beginDispose: () => {},
  dispose: async () => {},
  prompt: async () => true,
  abort: async () => {},
  maybeStartTitleGeneration: () => {},
});

interface Harness {
  engine: OmpHostEngine;
  clock: { now: number };
  sessions: Map<string, FixtureSession>;
  agentDir: string;
  realPaths: string[];
}

const createHarness = async ({
  sessionOverrides,
  files: extraFiles,
}: {
  sessionOverrides?: Record<string, Partial<FixtureSession>>;
  files?: Array<{ id: string; cwd: string }>;
} = {}): Promise<Harness> => {
  const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-lifecycle-'));
  const clock = { now: 0 };
  const sessions = new Map<string, FixtureSession>();
  const realPaths: string[] = [];
  mockState.agentDir = agentDir;
  mockState.files = [];

  for (const file of extraFiles ?? [{ id: 's1', cwd: '/repo' }]) {
    const filePath = path.join(agentDir, 'sessions', `${file.id}.jsonl`);
    mockState.files.push({ path: filePath, cwd: file.cwd });
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(filePath, '');
    realPaths.push(filePath);
  }

  // Dynamic after the module mocks above (see header comment).
  const { OmpHostEngine: Engine } = await import('./engine.ts');
  const engine = new Engine({
    agentDir,
    now: () => clock.now,
    // Small real-clock bounds: the hung-disposal tests exercise bounded
    // waits that must complete well inside the default test timeout.
    evictDrainTimeoutMs: 150,
    shutdownDisposeDeadlineMs: 200,
    // SAFETY: the engine option expects the SDK factory signature; the
    // harness supplies a duck-typed double the engine only calls.
    createAgentSession: (async (options: { cwd: string; sessionManager: { getSessionId?: () => string } }) => {
      const id = options.sessionManager.getSessionId?.() ?? 's1';
      const fixture = { ...makeFixtureSession(id, options.cwd), ...(sessionOverrides?.[id] ?? {}) };
      sessions.set(id, fixture);
      return {
        // SAFETY: fixture is a FixtureSession double, not the SDK session.
        session: fixture as never,
        setToolUIContext: () => {},
      };
    }) as never,
  });

  return { engine, clock, sessions, agentDir, realPaths };
};

const settle = (ms = 20) => new Promise((resolve) => setTimeout(resolve, ms));

describe('LiveSessionRegistry lifecycle (plan §3.2-3.4)', () => {
  test('idle TTL evicts a single live session — no live-count gate', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.clock.now += 29 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.clock.now += 2 * 60_000; // past the 30-minute TTL
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('UI lease holders and pending dialogs veto eviction', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.engine.dialogs.leases.release({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.engine.dialogs.registry.register({
      directory: '/repo',
      sessionId: 's1',
      kind: 'approval',
      payload: { approval: { prompt: 'ok?' } },
    });
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('every SDK activity signal vetoes eviction', async () => {
    const flags: Array<keyof FixtureSession> = [
      'isStreaming',
      'isAborting',
      'isRetrying',
      'isCompacting',
      'isGeneratingHandoff',
      'isBashRunning',
      'isEvalRunning',
      'hasPendingBashMessages',
      'hasPendingPythonMessages',
      'hasPostPromptWork',
    ];
    for (const flag of flags) {
      // SAFETY: `flag` is a keyof FixtureSession whose value is boolean.
      const h = await createHarness({ sessionOverrides: { s1: { [flag]: true } as Partial<FixtureSession> } });
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      h.clock.now += 31 * 60_000;
      h.engine.sweepIdleSessionsNow();
      await settle(10);
      expect(h.engine.liveRecord('s1')?.state).toBe('live');
    }
    const asyncWork = await createHarness({ sessionOverrides: { s1: { hasPendingAsyncWork: () => true } } });
    await asyncWork.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    asyncWork.clock.now += 31 * 60_000;
    asyncWork.engine.sweepIdleSessionsNow();
    await settle(10);
    expect(asyncWork.engine.liveRecord('s1')?.state).toBe('live');

    const queued = await createHarness({ sessionOverrides: { s1: { queuedMessageCount: 2 } } });
    await queued.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    queued.clock.now += 31 * 60_000;
    queued.engine.sweepIdleSessionsNow();
    await settle(10);
    expect(queued.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('in-flight operations veto eviction', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const record = h.engine.liveRecord('s1');
    if (!record) throw new Error('missing record');
    record.inFlight = 1;
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('eviction order: beginDispose → host teardown → session.idle → dispose settled', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          beginDispose: () => order.push('beginDispose'),
          subscribe: () => {
            order.push('subscribed');
            return () => order.push('unsubscribed');
          },
          dispose: async () => {
            order.push('dispose-start');
            await new Promise((resolve) => setTimeout(resolve, 15));
            order.push('dispose-done');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(80);

    expect(order.indexOf('beginDispose')).toBeLessThan(order.indexOf('dispose-start'));
    expect(order.indexOf('beginDispose')).toBeLessThan(order.indexOf('unsubscribed'));
    const idleSeen = h.engine.bus.replay.some((entry) => entry.envelope.type === 'session.idle');
    expect(idleSeen).toBe(true);
    expect(order.indexOf('dispose-done')).toBeGreaterThan(order.indexOf('unsubscribed'));
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('failed disposal quarantines: tombstone blocks rematerialization, observable in stats', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            throw new Error('drain exploded');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);

    const record = h.engine.liveRecord('s1');
    expect(record?.state).toBe('failed');
    expect(record?.failure?.reason).toContain('drain exploded');
    const diagnostics = h.engine.getStreamDiagnostics();
    expect(diagnostics.dataProportional.liveSessions.failed).toBe(1);
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'again' })).rejects.toThrow(/quarantined/);
  });

  test('never-settling disposal keeps the record evicting; materialize rejects, bounded', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: () => new Promise(() => {}),
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');

    // A new writer is rejected with the retryable busy error after the
    // bounded wait — never a second AgentSession over the same file.
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'again' })).rejects.toThrow(/evicting/);
    expect(h.sessions.size).toBe(1);
  });

  test('materialization failure releases every installed resource', async () => {
    const unsubscribed: string[] = [];
    const disposed: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          subscribe: () => () => {
            unsubscribed.push('s1');
          },
          dispose: async () => {
            disposed.push('s1');
          },
          sessionManager: {
            getSessionName: () => 'n',
            getArtifactsDir: () => '/a',
            onSessionNameChanged: () => {
              throw new Error('name callback registration exploded');
            },
          },
        },
      },
    });
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' })).rejects.toThrow(
      /name callback registration exploded/,
    );
    await settle(20);

    expect(unsubscribed).toEqual(['s1']);
    expect(disposed).toEqual(['s1']);
    expect(h.engine.liveRecord('s1')).toBeNull();
    // Setup failure is recoverable: no quarantine tombstone remains.
    expect(h.engine.getStreamDiagnostics().dataProportional.liveSessions.total).toBe(0);
  });

  test('a throwing host unsubscribe cannot strand an evicting record', async () => {
    const disposed: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          subscribe: () => () => {
            throw new Error('unsub exploded');
          },
          dispose: async () => {
            disposed.push('s1');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(50);
    // A host-side teardown failure must not abort disposal: the SDK writer
    // still drained and the record finished evicting instead of stranding.
    expect(disposed).toEqual(['s1']);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('deleteSession refuses with a retryable busy error while disposal hangs', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: { dispose: () => new Promise(() => {}) },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    // evictDrainTimeoutMs 150 → the bounded wait times out inside the gate;
    // the transcript is never removed under a still-live writer.
    await expect(h.engine.deleteSession({ sessionID: 's1', directory: '/repo' })).rejects.toThrow(/not deletable/);
    expect(fs.existsSync(realPath)).toBe(true);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');
  });

  test('deleteSession refuses a quarantined record instead of unlinking under it', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            throw new Error('drain exploded');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('failed');

    const [realPath] = h.realPaths;
    await expect(h.engine.deleteSession({ sessionID: 's1', directory: '/repo' })).rejects.toThrow(/not deletable/);
    expect(fs.existsSync(realPath)).toBe(true);
  });

  test('moveSession refuses with a retryable busy error while disposal hangs', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: { dispose: () => new Promise(() => {}) },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    await expect(h.engine.moveSession({ sessionID: 's1', destination: '/elsewhere' })).rejects.toThrow(/not movable/);
    expect(fs.existsSync(realPath)).toBe(true);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');
  });

  test('deleteSession awaits disposal before touching the transcript file', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    // Replace the fixture's dispose AFTER materialization (overrides map to
    // the same object the engine holds).
    const session = h.sessions.get('s1');
    if (!session) throw new Error('s1 missing');
    session.dispose = async () => {
      if (!fs.existsSync(realPath)) throw new Error('file removed before disposal settled');
      await new Promise((resolve) => setTimeout(resolve, 15));
    };
    await h.engine.deleteSession({ sessionID: 's1', directory: '/repo' });
    expect(fs.existsSync(realPath)).toBe(false);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('same session id in two directories is two isolated records', async () => {
    const h = await createHarness({ files: [{ id: 'same', cwd: '/a' }, { id: 'same', cwd: '/b' }] });
    await h.engine.prompt({ sessionID: 'same', directory: '/a', text: 'in a' });
    await h.engine.prompt({ sessionID: 'same', directory: '/b', text: 'in b' });
    const recordA = h.engine.liveRecord('same', '/a');
    const recordB = h.engine.liveRecord('same', '/b');
    expect(recordA?.state).toBe('live');
    expect(recordB?.state).toBe('live');
    expect(recordA).not.toBe(recordB);
    expect(recordA?.directory).toBe('/a');
    expect(recordB?.directory).toBe('/b');
    // An id-only lookup refuses the ambiguous pair instead of guessing.
    expect(h.engine.liveRecord('same')).toBeNull();
  });

  test('moveSession evicts the live writer before relocating the file', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            order.push('disposed');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    await h.engine.moveSession({ sessionID: 's1', destination: '/elsewhere' });
    expect(order).toEqual(['disposed']);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('shutdown: beginDispose on every live record; hanging disposal quarantines within the deadline', async () => {
    const order: string[] = [];
    const h = await createHarness({
      files: [{ id: 'a1', cwd: '/repo' }, { id: 'a2', cwd: '/repo' }],
      sessionOverrides: {
        a1: {
          beginDispose: () => order.push('a1-begin'),
          dispose: async () => {},
        },
        a2: {
          dispose: () => new Promise(() => {}),
        },
      },
    });
    await h.engine.prompt({ sessionID: 'a1', directory: '/repo', text: 'warm' });
    await h.engine.prompt({ sessionID: 'a2', directory: '/repo', text: 'warm' });

    const startedAt = Date.now();
    await h.engine.shutdown();
    expect(Date.now() - startedAt).toBeLessThan(20_000);
    expect(order).toContain('a1-begin');
    // The settled record is gone; the hanging one is quarantined observably.
    expect(h.engine.liveRecord('a1')).toBeNull();
    expect(h.engine.liveRecord('a2')?.state).toBe('evicting');
    await expect(h.engine.prompt({ sessionID: 'a1', directory: '/repo', text: 'post-shutdown' })).rejects.toThrow(
      /shutting down/,
    );
  });

  test('external transcript rewrite evicts the stale live writer before the next prompt (plan §8.2)', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            order.push('disposed');
          },
        },
      },
    });
    // Non-empty transcript so materialize records a tail entry id.
    fs.writeFileSync(h.realPaths[0], '{"id":"e1"}\n{"id":"e2"}\n');
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    // External writer shrinks the transcript — classifyExternalChange: dirty.
    fs.writeFileSync(h.realPaths[0], '{"id":"e1"}\n');

    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'after-rewrite' });
    // The stale writer was disposed and a fresh one materialized through the
    // gate — never two writers over the same file.
    expect(order).toEqual(['disposed']);
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('eviction clears wireIdOverrides scoped to the record (plan D5)', async () => {
    const h = await createHarness({
      files: [{ id: 's1', cwd: '/repo' }, { id: 's2', cwd: '/repo' }],
      sessionOverrides: {
        // s2 stays active through the sweep so its overrides must survive.
        s2: { isStreaming: true },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    await h.engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm' });
    h.engine.wireIdOverrides.set('/repo s1 cold-a', 'live-a');
    h.engine.wireIdOverrides.set('/repo s2 cold-b', 'live-b');
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')).toBeNull();
    expect(h.engine.liveRecord('s2')?.state).toBe('live');
    expect([...h.engine.wireIdOverrides.keys()]).toEqual(['/repo s2 cold-b']);
  });

  test('HTTP boundary: prompt against an evicting record answers 409 SessionBusyError (plan §3.4)', async () => {
    const h = await createHarness({
      sessionOverrides: { s1: { dispose: () => new Promise(() => {}) } },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');

    const { startOmpHost } = await import('./host.ts');
    const host = await startOmpHost({ engine: h.engine, port: 0 });
    try {
      const response = await fetch(`${host.baseUrl}/session/s1/prompt_async`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ directory: '/repo', parts: [{ type: 'text', text: 'again' }] }),
      });
      expect(response.status).toBe(409);
      // SAFETY: response.json() yields unknown; the 409 wire shape is the
      // contract under test.
      const payload = (await response.json()) as { _tag?: string };
      expect(payload._tag).toBe('SessionBusyError');
    } finally {
      await host.close();
    }
  });
});
