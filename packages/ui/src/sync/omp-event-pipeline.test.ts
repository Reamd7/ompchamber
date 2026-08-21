import { describe, expect, test } from 'bun:test';
import type { OmpCapabilities, OmpEventEnvelope, OmpStreamResyncPayload } from '@/lib/api/omp';
import { createOmpEventPipeline } from './omp-event-pipeline';
import { runOmpResync, type OmpResyncContext, type OmpResyncScope } from './omp-resync';

// ---------------------------------------------------------------------------
// Pipeline test doubles
// ---------------------------------------------------------------------------

const healthyCapabilities = (): OmpCapabilities => ({
  version: 1,
  eventSchema: '1.0',
  features: { events: true },
  minUiVersion: '0.0.0',
});

interface StreamHarness {
  capabilities: OmpCapabilities | null | 'throw';
  events: OmpEventEnvelope[];
  resyncFrames: OmpStreamResyncPayload[];
  resyncRequests: Array<{ scopes: string[] | null }>;
  wireResyncs: string[];
}

const createHarness = (overrides: Partial<StreamHarness> = {}): StreamHarness => ({
  capabilities: healthyCapabilities(),
  events: [],
  resyncFrames: [],
  resyncRequests: [],
  wireResyncs: [],
  ...overrides,
});

const mountPipeline = (harness: StreamHarness, resyncContext?: OmpResyncContext) => {
  const seen: OmpEventEnvelope[] = [];
  const pipeline = createOmpEventPipeline({
    ompCapabilities: {
      getCapabilities: async () => {
        if (harness.capabilities === 'throw') throw new Error('network down');
        return harness.capabilities;
      },
    },
    ompEvents: {
      subscribeEvents: (_directory, handlers) => {
        for (const frame of harness.resyncFrames) handlers.onResync(frame);
        for (const envelope of harness.events) handlers.onEvent(envelope);
        return { close: () => {} };
      },
    },
    directory: null,
    onEvent: (envelope) => {
      seen.push(envelope);
    },
    resync: resyncContext ?? {
      listDirectories: () => ['/repo'],
      listSessions: () => ['ses_1'],
      refetchWire: (directory) => {
        harness.wireResyncs.push(directory);
      },
    },
  });
  return { pipeline, seen };
};

// ---------------------------------------------------------------------------
// Capability gating (spec 05 §5.2.3 matrices)
// ---------------------------------------------------------------------------

describe('createOmpEventPipeline capability gate', () => {
  test('capabilities 404/missing → dormant: no subscription, no error', async () => {
    const harness = createHarness({ capabilities: null });
    let subscribed = false;
    const pipeline = createOmpEventPipeline({
      ompCapabilities: { getCapabilities: async () => null },
      ompEvents: {
        subscribeEvents: () => {
          subscribed = true;
          return { close: () => {} };
        },
      },
      directory: null,
      onEvent: () => {},
      resync: { listDirectories: () => [], listSessions: () => [], refetchWire: () => {} },
    });
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(subscribed).toBe(false);
    expect(pipeline.started()).toBe(false);
    pipeline.cleanup();
  });

  test('features.events === false → dormant (old-engine simulation)', async () => {
    let subscribed = false;
    const pipeline = createOmpEventPipeline({
      ompCapabilities: {
        getCapabilities: async () => ({ ...healthyCapabilities(), features: { events: false } }),
      },
      ompEvents: {
        subscribeEvents: () => {
          subscribed = true;
          return { close: () => {} };
        },
      },
      directory: null,
      onEvent: () => {},
      resync: { listDirectories: () => [], listSessions: () => [], refetchWire: () => {} },
    });
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(subscribed).toBe(false);
    pipeline.cleanup();
  });

  test('capabilities probe failure → dormant (relay old bundle)', async () => {
    const harness = createHarness({ capabilities: 'throw' });
    const { pipeline } = mountPipeline(harness);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(pipeline.started()).toBe(false);
    pipeline.cleanup();
  });

  test('healthy capabilities → subscribed and events dispatched', async () => {
    const harness = createHarness({
      events: [
        { id: 1, type: 'omp.mode.changed', directory: '/repo', sessionID: 'ses_1', schemaVersion: '1.0', createdAt: 1, payload: { mode: 'goal' } },
        { id: 2, type: ['omp','unknown','thing'].join('.'), directory: '/repo', schemaVersion: '1.0', createdAt: 2, payload: {} },
      ],
    });
    const { pipeline, seen } = mountPipeline(harness);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(pipeline.started()).toBe(true);
    // Unknown types flow to the consumer too — the reducer ignores them.
    expect(seen.map((envelope) => envelope.id)).toEqual([1, 2]);
    pipeline.cleanup();
  });
});

// ---------------------------------------------------------------------------
// Resync matrix (spec 05 §5.2.4)
// ---------------------------------------------------------------------------

const trackContext = (requests: string[]): OmpResyncContext => ({
  listDirectories: () => ['/repo'],
  listSessions: () => ['ses_1', 'ses_2'],
  refetchWire: (directory) => {
    requests.push(`wire:${directory}`);
  },
});

describe('runOmpResync', () => {
  test('scoped resync triggers exactly-once authoritative GETs per scope', async () => {
    const requests: string[] = [];
    const context: OmpResyncContext = {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    };
    await runOmpResync(['model', 'settings', 'model', 'dialogs'], context);
    const ompGets = requests.filter((entry) => !entry.startsWith('wire:'));
    expect(ompGets).toEqual([
      '/api/omp/models?/repo',
      '/api/omp/dialogs?/repo',
      '/api/omp/settings?/repo',
    ]);
  });

  test('untrustable scope runs the full ordered matrix (1→7 order preserved)', async () => {
    const requests: string[] = [];
    const context: OmpResyncContext = {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    };
    await runOmpResync([], context);
    // Canonical order: sessions(wire) → modes → model → dialogs → chrome → settings → agents → jobs → queue → tree → transcript(wire)
    expect(requests).toEqual([
      'wire:/repo',
      '/api/omp/sessions/ses_1/mode?/repo',
      '/api/omp/sessions/ses_2/mode?/repo',
      '/api/omp/models?/repo',
      '/api/omp/dialogs?/repo',
      '/api/omp/chrome?/repo',
      '/api/omp/settings?/repo',
      '/api/omp/agent-runs?/repo',
      '/api/omp/jobs?/repo',
      '/api/omp/sessions/ses_1/queue?/repo',
      '/api/omp/sessions/ses_2/queue?/repo',
      '/api/omp/sessions/ses_1/tree?/repo',
      '/api/omp/sessions/ses_2/tree?/repo',
      'wire:/repo',
    ]);
  });

  test('transcript scope goes through the wire refresh path', async () => {
    const requests: string[] = [];
    await runOmpResync(['transcript'], trackContext(requests));
    expect(requests).toEqual(['wire:/repo']);
  });

  test('unknown scope names are ignored (server scope list superset)', async () => {
    const requests: string[] = [];
    await runOmpResync(['not-a-scope', 'model'], {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    });
    expect(requests).toEqual(['/api/omp/models?/repo']);
  });

  test('dialogs results are delivered to the consumer per directory (ok/unavailable/failure)', async () => {
    const delivered: Array<{ directory: string; ok: boolean }> = [];
    const context: OmpResyncContext = {
      ...trackContext([]),
      fetchOmpJson: async (path) => {
        if (path !== '/api/omp/dialogs') return { ok: false, unavailable: true };
        return { ok: true, data: { dialogs: [] } };
      },
      consumeDialogs: (directory, result) => {
        delivered.push({ directory, ok: result.ok });
      },
    };
    await runOmpResync(['dialogs'], context);
    expect(delivered).toEqual([{ directory: '/repo', ok: true }]);
  });

  test('dialogs without a consumer keeps the fetch-only behavior', async () => {
    const requests: string[] = [];
    await runOmpResync(['dialogs'], {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    });
    expect(requests).toEqual(['/api/omp/dialogs?/repo']);
  });

  test('fetch failures never clear state and never abort sibling domains', async () => {
    const requests: string[] = [];
    const context: OmpResyncContext = {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(path);
        if (path === '/api/omp/settings') return { ok: false, unavailable: false };
        return { ok: false, unavailable: true };
      },
    };
    await runOmpResync(['settings', 'dialogs'], context);
    expect(requests).toContain('/api/omp/settings');
    expect(requests).toContain('/api/omp/dialogs');
  });
});

describe('pipeline resync frame handling', () => {
  test('resync frame runs the scoped matrix through the resync context', async () => {
    const requests: string[] = [];
    const harness = createHarness({
      resyncFrames: [{ scope: ['model'], lastEventId: 41 }],
    });
    const context: OmpResyncContext = {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    };
    const { pipeline } = mountPipeline(harness, context);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(requests).toEqual(['/api/omp/models?/repo']);
    pipeline.cleanup();
  });

  test('duplicate resync frames inside the rate-limit window collapse', async () => {
    const requests: string[] = [];
    const harness = createHarness({
      resyncFrames: [
        { scope: ['model'], lastEventId: 41 },
        { scope: ['dialogs'], lastEventId: 41 },
      ],
    });
    const context: OmpResyncContext = {
      ...trackContext(requests),
      fetchOmpJson: async (path, query) => {
        requests.push(`${path}?${query.directory}`);
        return { ok: false, unavailable: true };
      },
    };
    const { pipeline } = mountPipeline(harness, context);
    await new Promise((resolve) => setTimeout(resolve, 20));
    // Only the first scoped run executes inside the 2s window.
    expect(requests).toEqual(['/api/omp/models?/repo']);
    pipeline.cleanup();
  });
});
