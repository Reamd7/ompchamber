import { describe, test, expect, mock, afterAll } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const agentDir = mkdtempSync(path.join(tmpdir(), 'omp-engine-test-'));
const sessionDir = path.join(agentDir, 'sessions');

const sessionFiles = [{ id: 's1', path: path.join(sessionDir, 's1.jsonl') }, { id: 's2', path: path.join(sessionDir, 's2.jsonl') }];
const createdOptions = [];
const registries = [];

const makeFakeSession = () => ({
  model: { provider: 'p1', id: 'current-model' },
  isStreaming: false,
  messages: [],
  subscribe: () => () => {},
  sessionManager: { onSessionNameChanged: () => {}, getSessionName: () => undefined },
  setModel: async () => ({ switched: true }),
  maybeStartTitleGeneration: () => {},
  prompt: mock(async () => true),
  steer: mock(async () => {}),
});

const fakeSessions = new Map();
const sessionFor = (id) => {
  if (!fakeSessions.has(id)) fakeSessions.set(id, makeFakeSession());
  return fakeSessions.get(id);
};

mock.module('@oh-my-pi/pi-coding-agent', () => ({
  AgentRegistry: class {
    constructor() {
      registries.push(this);
    }
  },
  ModelRegistry: class {
    constructor() {}
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'zzz-first' }];
    }
  },
  SessionManager: Object.assign(
    class {},
    {
      async open(file) {
        return { getSessionId: () => path.basename(file, '.jsonl'), onSessionNameChanged: () => {} };
      },
      async list() {
        return sessionFiles;
      },
      getDefaultSessionDir: () => sessionDir,
    },
  ),
  createAgentSession: async (options) => {
    createdOptions.push(options);
    return { session: sessionFor(options.sessionManager?.getSessionId?.() ?? 's1') };
  },
  discoverAuthStorage: () => ({}),
  BUILTIN_TOOLS: [],
}));

const { OmpHostEngine } = await import('./engine.js');

afterAll(() => {
  rmSync(agentDir, { recursive: true, force: true, maxRetries: 5 });
});

describe('OmpHostEngine prompt dispatch', () => {
  test('submits with TUI steer semantics and does not reject while streaming', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'hello' });
    const session = sessionFor('s1');
    expect(session.prompt).toHaveBeenCalledWith('hello', { images: [], streamingBehavior: 'steer' });
    expect(session.steer).not.toHaveBeenCalled();

    session.isStreaming = true;
    await expect(
      engine.prompt({ sessionID: 's1', directory: '/repo', text: 'mid turn' }),
    ).resolves.toBeTruthy();
    expect(session.prompt).toHaveBeenCalledTimes(2);
    expect(session.prompt).toHaveBeenLastCalledWith('mid turn', { images: [], streamingBehavior: 'steer' });
  });

  test('maps wire delivery "queue" to a follow-up', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'after this turn', delivery: 'queue' });
    const session = sessionFor('s2');
    expect(session.prompt).toHaveBeenCalledWith('after this turn', { images: [], streamingBehavior: 'followUp' });
  });

  test('gives each embedded session a private agent registry', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const before = registries.length;
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'one' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'two' });
    const passed = createdOptions.slice(-2).map((options) => options.agentRegistry);
    expect(passed).toHaveLength(2);
    expect(registries.length).toBeGreaterThanOrEqual(before + 2);
    expect(passed[0]).not.toBe(passed[1]);
  });

  test('leaves the model unset so the SDK resolves the settings default', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'defaults' });
    const options = createdOptions.at(-1);
    expect(options.model).toBeUndefined();
  });
});
