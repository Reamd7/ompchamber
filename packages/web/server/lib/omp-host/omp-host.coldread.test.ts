import { describe, expect, mock, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { OmpHostEngine } from './engine.ts';

// Cold-read contracts (docs/plan.md §7, acceptance §10.1 "冷读"): every cold
// GET path releases its temporary manager (close + releaseRetainedEntries in
// finally), the entry-tree snapshot no longer hands a raw manager to the URI
// domain, and the counters observe it all without touching content.

const realSdk = await import('@oh-my-pi/pi-coding-agent');
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');

// Harness state the top-level module mocks close over.
interface ColdMockState {
  agentDir: string;
  files: Array<{ path: string; cwd: string }>;
  released: string[];
  /** close→release ordering proof (the SDK contract is close-first). */
  order: string[];
  failClose: boolean;
}
const mockState: ColdMockState = {
  agentDir: '',
  files: [],
  released: [],
  order: [],
  failClose: false,
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
          getEntries: () => [
            { type: 'model_change', model: 'p1/m1', role: 'default', timestamp: 1_000 },
            { type: 'compaction', summary: 's', tokensBefore: 10, timestamp: 2_000 },
          ],
          buildSessionContext: () => ({
            messages: [
              { role: 'user', content: 'hi', timestamp: 1 },
              { role: 'assistant', content: [{ type: 'text', text: 'ok' }], model: 'p1/m1', timestamp: 2 },
            ],
          }),
          getCwd: () => undefined,
          getSessionName: () => undefined,
          getArtifactsDir: () => file.slice(0, -'.jsonl'.length),
          getTree: () => [],
          getLeafId: () => null,
          moveTo: async () => {},
          close: async () => {
            mockState.order.push('close');
            if (mockState.failClose) throw new Error('close exploded');
          },
          releaseRetainedEntries: () => {
            mockState.order.push('release');
            mockState.released.push(file);
          },
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
    throw new Error('cold reads must never materialize an agent');
  },
}));

const { coldReaderStats, resetColdReaderStats } = await import('./cold-reader.ts');
const { OmpHostEngine: Engine } = await import('./engine.ts');

const makeEngine = () => {
  const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-cold-'));
  const file = path.join(agentDir, 'sessions', 'c1.jsonl');
  mockState.agentDir = agentDir;
  mockState.files = [{ path: file, cwd: '/repo' }];
  mockState.released = [];
  mockState.order = [];
  mockState.failClose = false;
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, '');
  return { engine: new Engine({ agentDir }), file };
};

describe('cold reads release every temporary manager (plan §7)', () => {
  test('getSession cold path: one open, one release, one close', async () => {
    resetColdReaderStats();
    const { engine } = makeEngine();
    const session = await engine.getSession({ sessionID: 'c1', directory: '/repo' });
    expect(session?.id).toBe('c1');
    const stats = coldReaderStats();
    expect(stats.opens).toBe(1);
    expect(stats.releases).toBe(1);
    expect(stats.closes).toBe(1);
  });

  test('getMessages cold path releases the entries mirror', async () => {
    resetColdReaderStats();
    const { engine } = makeEngine();
    const messages = await engine.getMessages({ sessionID: 'c1', directory: '/repo' });
    expect(messages?.length).toBeGreaterThan(0);
    const stats = coldReaderStats();
    expect(stats.opens).toBe(1);
    expect(stats.releases).toBe(1);
    expect(stats.closes).toBe(1);
  });

  test('getEntries and getTelemetry release on success and error paths', async () => {
    resetColdReaderStats();
    const { engine } = makeEngine();
    await engine.getEntries({ sessionID: 'c1', directory: '/repo' });
    await engine.getTelemetry({ sessionID: 'c1', directory: '/repo' });
    const stats = coldReaderStats();
    expect(stats.opens).toBeGreaterThanOrEqual(2);
    expect(stats.releases).toBe(stats.opens);
    expect(stats.closes).toBe(stats.opens);
  });

  test('consume throws still release (finally contract)', async () => {
    resetColdReaderStats();
    const { withColdManager } = await import('./cold-reader.ts');
    await expect(
      withColdManager('<missing-but-mocked>.jsonl', () => {
        throw new Error('consumer exploded');
      }),
    ).rejects.toThrow('consumer exploded');
    const stats = coldReaderStats();
    expect(stats.releases).toBe(1);
    expect(stats.closes).toBe(1);
  });

  test('release runs only after close settles (SDK contract order)', async () => {
    resetColdReaderStats();
    const { engine } = makeEngine();
    await engine.getSession({ sessionID: 'c1', directory: '/repo' });
    expect(mockState.order).toEqual(['close', 'release']);
  });

  test('a failing close still releases the retained mirror', async () => {
    resetColdReaderStats();
    mockState.order = [];
    mockState.failClose = true;
    const { withColdManager } = await import('./cold-reader.ts');
    await withColdManager('<missing-but-mocked>.jsonl', () => 'ok');
    expect(mockState.order).toEqual(['close', 'release']);
    const stats = coldReaderStats();
    expect(stats.closes).toBe(0);
    expect(stats.failedCloses).toBe(1);
    expect(stats.releases).toBe(1);
  });

  test('entry tree snapshot carries no manager across the boundary', async () => {
    const { engine } = makeEngine();
    resetColdReaderStats();
    // The domain's tree.entryTree handle consumes the engine's snapshot;
    // the recorded releases prove the cold manager was closed inside
    // entryTreeFor, never handed across the boundary.
    const response = await engine.uriDomain.tree.entryTree({ sessionID: 'c1', directory: '/repo' });
    expect(response.status).toBe(200);
    expect(mockState.released.length).toBe(1);
    const body = await response.json();
    expect(body).toBeTruthy();
  });

  test('cold GETs never materialize an agent session', async () => {
    const { engine } = makeEngine();
    await engine.getSession({ sessionID: 'c1', directory: '/repo' });
    await engine.getMessages({ sessionID: 'c1', directory: '/repo' });
    await engine.getEntries({ sessionID: 'c1', directory: '/repo' });
    await engine.getTelemetry({ sessionID: 'c1', directory: '/repo' });
    expect(engine.liveRecord('c1')).toBeNull();
  });
});
