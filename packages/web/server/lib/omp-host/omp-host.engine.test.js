import { describe, test, expect, mock, afterAll } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const agentDir = mkdtempSync(path.join(tmpdir(), 'omp-engine-test-'));
const sessionDir = path.join(agentDir, 'sessions');

const sessionFiles = [{ id: 's1', path: path.join(sessionDir, 's1.jsonl') }, { id: 's2', path: path.join(sessionDir, 's2.jsonl') }];
const createdOptions = [];
const registries = [];
const toolUiContextCalls = [];
const extensionUiInitCalls = [];

const makeFakeSession = () => ({
  model: { provider: 'p1', id: 'current-model' },
  isStreaming: false,
  messages: [],
  subscribe: () => () => {},
  setModel: mock(async () => ({ switched: true })),
  setThinkingLevel: mock(async () => {}),
  maybeStartTitleGeneration: () => {},
  prompt: mock(async () => true),
  steer: mock(async () => {}),
});

const fakeSessions = new Map();
const sessionFor = (id) => {
  if (!fakeSessions.has(id)) fakeSessions.set(id, makeFakeSession());
  return fakeSessions.get(id);
};

const realSdk = await import('@oh-my-pi/pi-coding-agent');
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');
mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async (session, options) => {
    extensionUiInitCalls.push({ session, options });
  },
}));
mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {
    constructor() {
      registries.push(this);
    }
  },
  ModelRegistry: class {
    constructor() {}
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'zzz-first' }, { provider: 'p1', id: 'current-model' }];
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
    return {
      session: sessionFor(options.sessionManager?.getSessionId?.() ?? 's1'),
      setToolUIContext: (uiContext, hasUI) => toolUiContextCalls.push({ uiContext, hasUI }),
    };
  },
  Settings: class extends realSdk.Settings {
    static async init() {
      return {
        getCwd: () => 'C:/stub-boot',
        cloneForCwd: async () => ({ getCwd: () => 'C:/stub-boot' }),
      };
    }
  },
  VERSION: 'test',
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


  test('keeps old-UI explicit model compatibility while model-free prompts do not switch', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    const before = session.setModel.mock.calls.length;

    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'roles request' });
    expect(session.setModel.mock.calls.length).toBe(before);

    await engine.prompt({
      sessionID: 's1', directory: '/repo', text: 'legacy request',
      model: { providerID: 'p1', modelID: 'zzz-first' },
    });
    expect(session.setModel.mock.calls.length).toBe(before + 1);
    expect(session.setModel).toHaveBeenLastCalledWith({ provider: 'p1', id: 'zzz-first' });
    expect(engine.registry.get('/repo', 's1')?.model).toBe('p1/zzz-first');
  });

  test('setSessionModel applies thinking-only changes without a model switch (GAP-06)', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');

    // Materialize first so the session exists.
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up' });
    const modelCallsBefore = session.setModel.mock.calls.length;

    // Same model as the session's current model (p1/current-model) → the
    // engine must not call setModel, only setThinkingLevel.
    const result = await engine.setSessionModel({
      sessionID: 's1',
      directory: '/repo',
      model: { providerID: 'p1', modelID: 'current-model' },
      thinkingLevel: 'high',
    });
    expect(result.ok).toBe(true);
    expect(session.setModel.mock.calls.length).toBe(modelCallsBefore);
    expect(session.setThinkingLevel).toHaveBeenLastCalledWith('high');

    // A different model switches the model AND applies the thinking level.
    await engine.setSessionModel({
      sessionID: 's1',
      directory: '/repo',
      model: { providerID: 'p1', modelID: 'zzz-first' },
      thinkingLevel: 'off',
    });
    expect(session.setModel.mock.calls.length).toBe(modelCallsBefore + 1);
    expect(session.setThinkingLevel).toHaveBeenLastCalledWith('off');
  });
  test('materialize injects the keyed Settings instance, lease-driven hasUI, session-pinned local options', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'wiring' });
    const options = createdOptions.at(-1);
    // Settings injection (06 §5.1 / master R6): the boot instance is handed
    // to the SDK instead of the process singleton.
    expect(options.settings).toBe(await engine.settingsStore.settingsFor('/repo'));
    // R13: hasUI comes from the dialog lease snapshot — no lease → false
    // (fail-closed), never from the capability.
    expect(options.hasUI).toBe(false);
    // R7/R8: local:// resolution is session-pinned with zero global mutation.
    expect(options.localProtocolOptions).toBeTruthy();
    expect(typeof options.localProtocolOptions).toBe('object');
    // Retained for the agent-runs aggregator (04 §5.5).
    expect(engine.sessions.get('s1').agentRegistry).toBe(options.agentRegistry);
    // A lease flip drives hasUI on the next materialization.
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'c1' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'with lease' });
    expect(createdOptions.at(-1).hasUI).toBe(true);
  });

  test('attaches both extension and tool UI contexts during first materialization', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const beforeTool = toolUiContextCalls.length;
    const beforeExtension = extensionUiInitCalls.length;
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'browser-1' });

    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'first turn' });
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'browser-2' });
    await Promise.resolve();

    expect(createdOptions.at(-1).hasUI).toBe(true);
    const attach = toolUiContextCalls.slice(beforeTool).at(-1);
    expect(attach?.hasUI).toBe(true);
    expect(typeof attach?.uiContext?.askDialog).toBe('function');
    const extensionInit = extensionUiInitCalls.slice(beforeExtension);
    expect(extensionInit).toHaveLength(1);
    expect(extensionInit[0].options.mode).toBe('json');
    expect(typeof extensionInit[0].options.uiContext.askDialog).toBe('function');
  });

  test('plan sessions receive the SDK PlanYolo shape {target, thinkingLevel?}', async () => {
    // P0 defect a regression guard: the old `{autoApproveOnResolve}` literal
    // was an unknown field — silent read-only plan mode + xd://propose
    // TypeError (spec 02 §4). With a persisted model the correct shape is
    // passed; without one planYolo is omitted entirely (never the bad shape).
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { agent: 'plan', model: 'p1/zzz-first' });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'plan it' });
    const withModel = createdOptions.at(-1);
    expect(withModel.planYolo).toEqual({ target: { provider: 'p1', id: 'zzz-first' } });

    engine.registry.update('/repo', 's2', { agent: 'plan' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'plan without model' });
    const withoutModel = createdOptions.at(-1);
    expect(withoutModel.planYolo).toBeUndefined();
    expect(JSON.stringify(withoutModel)).not.toContain('autoApproveOnResolve');
  });
});
