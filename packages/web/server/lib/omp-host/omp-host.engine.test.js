import { describe, test, expect, mock, afterAll } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const agentDir = mkdtempSync(path.join(tmpdir(), 'omp-engine-test-'));
const sessionDir = path.join(agentDir, 'sessions');

const sessionFiles = [
  { id: 's1', path: path.join(sessionDir, 's1.jsonl') },
  { id: 's2', path: path.join(sessionDir, 's2.jsonl') },
  // Owned by /repo: lets tests address an idle session through a directory
  // that does not own it (cwd-less files match every directory, preserving
  // the pre-existing mock behavior for the other ids).
  { id: 's3', path: path.join(sessionDir, 's3.jsonl'), cwd: '/repo' },
];
const fakeForkEntries = [
  { type: 'message', id: 'e1', parentId: null, message: { role: 'user', timestamp: 1, content: 'first' } },
  { type: 'message', id: 'e2', parentId: 'e1', message: { role: 'assistant', timestamp: 2, content: 'reply' } },
  { type: 'message', id: 'e3', parentId: 'e2', message: { role: 'user', timestamp: 3, content: 'second' } },
  { type: 'message', id: 'e4', parentId: 'e3', message: { role: 'assistant', timestamp: 4, content: 'reply 2' } },
];
const forkMutations = [];
const fakeManagerEntries = [];
const createdOptions = [];
const registries = [];
const toolUiContextCalls = [];
const extensionUiInitCalls = [];

const makeFakeSession = () => ({
  model: { provider: 'p1', id: 'current-model' },
  isStreaming: false,
  messages: [],
  subscribe: () => () => {},
  sessionManager: { getSessionName: () => 'stub-session-name' },
  setModel: mock(async () => ({ switched: true })),
  setThinkingLevel: mock(() => {}),
  maybeStartTitleGeneration: () => {},
  prompt: mock(async () => true),
  steer: mock(async () => {}),
  abort: mock(async () => {}),
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
        return {
          getSessionId: () => path.basename(file, '.jsonl'),
          onSessionNameChanged: () => {},
          // Idle-session reads (#infoFromManager) need the transcript reader
          // surface; an empty header/entries set is enough for wire building.
          getHeader: () => null,
          getEntries: () => fakeManagerEntries,
          buildSessionContext: () => ({ messages: [] }),
          getCwd: () => undefined,
          getSessionName: () => undefined,
          close: async () => {},
        };
      },
      async list(cwd) {
        return sessionFiles.filter((file) => !file.cwd || !cwd || file.cwd === cwd);
      },
      getDefaultSessionDir: () => sessionDir,
      async forkFrom(filePath) {
        return {
          getSessionId: () => `${path.basename(filePath, '.jsonl')}_fork`,
          getEntries: () => fakeForkEntries,
          getEntry: (id) => fakeForkEntries.find((entry) => entry.id === id),
          branch: (leafId) => { forkMutations.push({ op: 'branch', leafId }); },
          resetLeaf: () => { forkMutations.push({ op: 'resetLeaf' }); },
          appendCustomEntry: (customType, data) => {
            forkMutations.push({ op: 'marker', customType, data });
            return 'marker_1';
          },
          close: async () => {},
        };
      },
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
  test('projects transcript model/mode switches as timeline dividers', async () => {
    const stamp = 1_787_811_000_000;
    const session = sessionFor('s1');
    fakeManagerEntries.push(
      { type: 'model_change', model: 'p1/old', role: 'default', timestamp: stamp - 10_000 },
      { type: 'model_change', model: 'p1/new', role: 'temporary', timestamp: stamp },
      { type: 'mode_change', mode: 'plan', timestamp: stamp + 5_000 },
    );
    try {
      const engine = new OmpHostEngine({ agentDir });
      // Materialize the live session so the projection reads its messages.
      await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      session.messages = [{ role: 'user', content: 'before', timestamp: stamp - 5_000 }];
      const projected = await engine.getMessages({ sessionID: 's1', directory: '/repo' });
      const rows = (projected ?? []).map((item) => ({
        role: item.info.role,
        text: item.parts[0]?.text ?? '',
        ompRole: item.info.metadata?.ompRole,
      }));
      expect(rows).toEqual([
        { role: 'assistant', text: '[omp:modelChange] p1/old', ompRole: 'modelChange' },
        { role: 'user', text: 'before', ompRole: undefined },
        { role: 'assistant', text: '[omp:modelChange] p1/new', ompRole: 'modelChange' },
        { role: 'assistant', text: '[omp:modeChange] plan', ompRole: 'modeChange' },
      ]);
    } finally {
      fakeManagerEntries.length = 0;
      session.messages = [];
    }
  });

  test('stamps the effective thinking level on the user message snapshot', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s3');
    // No explicit level and no model default in the registry → no variant.
    const bare = await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'bare' });
    expect(bare.info.model).toEqual({ providerID: 'p1', modelID: 'current-model' });

    // An explicit session level rides model.variant — the exact send-time
    // snapshot the turn runs with.
    session.thinkingLevel = 'xhigh';
    const stamped = await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'stamped' });
    expect(stamped.info.model).toEqual({ providerID: 'p1', modelID: 'current-model', variant: 'xhigh' });
    delete session.thinkingLevel;
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

  test('abort forwards to the live agent session and reports unknown sessions as false', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up' });
    const session = sessionFor('s1');

    await expect(engine.abort({ sessionID: 's1', directory: '/repo' })).resolves.toBe(true);
    expect(session.abort).toHaveBeenCalledWith({ reason: 'User aborted' });

    await expect(engine.abort({ sessionID: 'never-materialized', directory: '/repo' })).resolves.toBe(false);
  });

  test('updateSession writes the registry under a live session\'s own directory', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up' });

    const updated = await engine.updateSession({ sessionID: 's1', directory: '/elsewhere', timeArchived: 123 });

    // The live session owns /repo: the patch must land and be reported there,
    // never stranded as a phantom /elsewhere registry entry that listings under
    // the owning directory never read.
    expect(engine.registry.get('/repo', 's1')?.timeArchived).toBe(123);
    expect(engine.registry.get('/elsewhere', 's1')).toBe(null);
    expect(updated?.time?.archived).toBe(123);
  });

  test('updateSession refuses a mis-addressed update for an idle session', async () => {
    const engine = new OmpHostEngine({ agentDir });

    // s3's transcript lives under /repo. A write addressed to /elsewhere owns
    // neither the transcript nor a registry entry and must not fabricate one:
    // before the guard it "succeeded" by answering with a synthesized session
    // while no listing keyed by the transcript's cwd could ever observe it.
    const refused = await engine.updateSession({ sessionID: 's3', directory: '/elsewhere', timeArchived: 123 });
    expect(refused).toBe(null);
    expect(engine.registry.get('/elsewhere', 's3')).toBe(null);

    // The owning directory still applies the same update.
    const updated = await engine.updateSession({ sessionID: 's3', directory: '/repo', timeArchived: 123 });
    expect(updated?.time?.archived).toBe(123);
    expect(engine.registry.get('/repo', 's3')?.timeArchived).toBe(123);
  });

  test('updateSession keeps registry-only sessions updatable for bookkeeping', async () => {
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 'pruned', { timeCreated: 1 });

    const updated = await engine.updateSession({ sessionID: 'pruned', directory: '/repo', timeArchived: 9 });

    expect(updated?.time?.archived).toBe(9);
    expect(engine.registry.get('/repo', 'pruned')?.timeArchived).toBe(9);
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

    // Regression (real SDK contract): setThinkingLevel returns void
    // (agent-session.d.ts:736). Treating it as a thenable threw
    // "undefined is not an object (evaluating '.catch')" and answered the
    // model endpoint with a 500 — every thinking-level change failed.
    // 'inherit' is the wire sentinel that clears the explicit level.
    const inherit = await engine.setSessionModel({
      sessionID: 's1',
      directory: '/repo',
      model: { providerID: 'p1', modelID: 'current-model' },
      thinkingLevel: 'inherit',
    });
    expect(inherit.ok).toBe(true);
    expect(session.setThinkingLevel).toHaveBeenLastCalledWith(undefined);

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

  test('legacy build/plan metas normalize to the standard session; planYolo never reaches createAgentSession', async () => {
    // 02 §5.1/§5.8: the build/plan agent pair is deleted — 'plan' meta is a
    // standard session (plan mode is driven by the mode endpoints), so no
    // planYolo, no systemPrompt overlay, and no crash shape can occur.
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { agent: 'plan', model: 'p1/zzz-first' });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'plan it' });
    const planOptions = createdOptions.at(-1);
    expect(planOptions.planYolo).toBeUndefined();
    expect(planOptions.systemPrompt).toBeUndefined();
    expect(JSON.stringify(planOptions)).not.toContain('autoApproveOnResolve');

    // A persona meta resolves the persona store (02 §5.1 D-B2): the overlay
    // shapes systemPrompt/toolNames at construction.
    engine.personas.set('grumpy', { name: 'grumpy', systemPrompt: 'Be grumpy.', tools: ['read'] });
    engine.registry.update('/repo', 's2', { persona: 'grumpy' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'hello' });
    const personaOptions = createdOptions.at(-1);
    expect(personaOptions.systemPrompt).toBe('Be grumpy.');
    expect(personaOptions.toolNames).toEqual(['read']);
  });
});

describe('OmpHostEngine fork lineage', () => {
  test('records forkParentID lineage and never emits subagent parentID', async () => {
    // engine.fork must NOT write wire `parentID`: the shared UI treats a
    // parentID session as a read-only subagent session ("subagent sessions
    // cannot be prompted"), and a user fork is a normal promptable session.
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { title: 'root work' });

    const forked = await engine.fork({ sessionID: 's1', directory: '/repo' });

    expect(forked.id).toBe('s1_fork');
    expect(forked.title).toBe('root work (fork)');
    expect(forked.parentID).toBeUndefined();
    expect(forked.forkParentID).toBe('s1');
    expect(engine.registry.get('/repo', 's1_fork')).toMatchObject({ forkParentID: 's1' });
    // The listing projection carries the same split — the session-tree
    // builder reads forkParentID, the UI subagent checks read parentID.
    const listed = (await engine.listSessions({ directory: '/repo' })).find((s) => s.id === 's1_fork');
    expect(listed.parentID).toBeUndefined();
    expect(listed.forkParentID).toBe('s1');
  });
});

describe('OmpHostEngine fork boundary (wire messageID)', () => {
  test('bounds the fork before the selected message (omp /branch semantics)', async () => {
    forkMutations.length = 0;
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { title: 'root work' });

    const forked = await engine.fork({ sessionID: 's1', directory: '/repo', messageID: 'e3' });

    expect(forked.forkParentID).toBe('s1');
    // The leaf moves to the boundary entry's parent (e3 and its tail leave
    // the active path), and an appended marker entry makes the rewind durable.
    expect(forkMutations).toEqual([
      { op: 'branch', leafId: 'e2' },
      { op: 'marker', customType: 'ompchamber.forkBoundary', data: { from: 's1', at: 'e3' } },
    ]);
  });

  test('a root boundary rewinds to a fresh leaf', async () => {
    forkMutations.length = 0;
    const engine = new OmpHostEngine({ agentDir });

    await engine.fork({ sessionID: 's1', directory: '/repo', messageID: 'e1' });

    expect(forkMutations).toEqual([
      { op: 'resetLeaf' },
      { op: 'marker', customType: 'ompchamber.forkBoundary', data: { from: 's1', at: 'e1' } },
    ]);
  });

  test('an unknown boundary falls back to the whole-transcript fork', async () => {
    forkMutations.length = 0;
    const engine = new OmpHostEngine({ agentDir });

    await engine.fork({ sessionID: 's1', directory: '/repo', messageID: 'msg_nope' });

    expect(forkMutations).toEqual([]);
  });

  test('no messageID keeps the whole transcript (omp /fork semantics)', async () => {
    forkMutations.length = 0;
    const engine = new OmpHostEngine({ agentDir });

    await engine.fork({ sessionID: 's1', directory: '/repo' });

    expect(forkMutations).toEqual([]);
  });
});
