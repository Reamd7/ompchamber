import { describe, test, expect, mock, afterAll } from 'bun:test';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
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
type ForkMutation = { op: string; leafId?: string; customType?: string; data?: unknown };
type FakeSessionContext = { messages: unknown[] };

const forkMutations: ForkMutation[] = [];
const fakeManagerEntries: unknown[] = [];
type CreatedOptions = { cwd?: string; sessionManager?: { getSessionId?: () => string }; localProtocolOptions?: { getSessionId: () => string; getArtifactsDir: () => string }; toolNames?: string[]; systemPrompt?: string; model?: unknown; agentRegistry?: unknown; settings?: unknown; hasUI?: boolean; planYolo?: boolean };
const createdOptions: CreatedOptions[] = [];
const registries: unknown[] = [];
const toolUiContextCalls: Array<{ uiContext: unknown; hasUI: boolean }> = [];
type ExtensionInitCall = { session: unknown; options: { mode?: string; uiContext?: { askDialog?: unknown } } };
const extensionUiInitCalls: ExtensionInitCall[] = [];

const makeFakeSession = (id: string) => ({
  model: { provider: 'p1', id: 'current-model' },
  isStreaming: false,
  // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
  messages: [] as unknown[],
  subscribe: () => () => {},
  sessionManager: {
    getSessionName: () => 'stub-session-name',
    // Live-session artifacts dir parity (SessionManager.getArtifactsDir:
    // sessionFile minus '.jsonl') — the per-session local:// root source.
    getArtifactsDir: () => path.join(sessionDir, id),
  },
  setModel: mock(async () => ({ switched: true })),
  setThinkingLevel: mock(() => {}),
  maybeStartTitleGeneration: () => {},
  prompt: mock(async () => true),
  getTodoPhases: (): [] => [],
  steer: mock(async () => {}),
  abort: mock(async () => {}),
  dispose: mock(async () => {}),
});

const fakeSessions = new Map();
const sessionFor = (id: string) => {
  if (!fakeSessions.has(id)) fakeSessions.set(id, makeFakeSession(id));
  return fakeSessions.get(id);
};

const realSdk = await import('@oh-my-pi/pi-coding-agent');
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');
mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async (session: { id?: string }, options: ExtensionInitCall['options']) => {
    extensionUiInitCalls.push({ session, options });
  },
}));
mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {
    constructor() {
      // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
      registries.push(this);
    }
    onChange() {
      return () => {};
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
      async open(file: string) {
        return {
          getSessionId: () => path.basename(file, '.jsonl'),
          onSessionNameChanged: () => {},
          // Idle-session reads (#infoFromManager) need the transcript reader
          // surface; an empty header/entries set is enough for wire building.
          getHeader: (): null => null,
          getEntries: () => fakeManagerEntries,
          buildSessionContext: (): FakeSessionContext => ({ messages: [] }),
          getCwd: (): undefined => undefined,
          getSessionName: (): undefined => undefined,
          getArtifactsDir: () => file.slice(0, -'.jsonl'.length),
          close: async () => {},
        };
      },
      async list(cwd?: string) {
        return sessionFiles.filter((file) => !file.cwd || !cwd || file.cwd === cwd);
      },
      getDefaultSessionDir: () => sessionDir,
      async forkFrom(filePath: string) {
        return {
          getSessionId: () => `${path.basename(filePath, '.jsonl')}_fork`,
          getEntries: () => fakeForkEntries,
          getEntry: (id: string) => fakeForkEntries.find((entry: { id: string }) => entry.id === id),
          branch: (leafId: string) => { forkMutations.push({ op: 'branch', leafId }); },
          resetLeaf: () => { forkMutations.push({ op: 'resetLeaf' }); },
          appendCustomEntry: (customType: string, data: ForkMutation['data']) => {
            forkMutations.push({ op: 'marker', customType, data });
            return 'marker_1';
          },
          close: async () => {},
        };
      },
    },
  ),
  createAgentSession: async (options: CreatedOptions) => {
    createdOptions.push(options);
    return {
      // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
      session: sessionFor((options.sessionManager as { getSessionId?: () => string } | undefined)?.getSessionId?.() ?? 's1'),
      setToolUIContext: (uiContext: { askDialog?: unknown }, hasUI: boolean) => toolUiContextCalls.push({ uiContext, hasUI }),
    };
  },
  // The SDK Settings constructor is private (a type-level gate); the harness
  // only serves the `Settings.init` seam, and the stub it returns already
  // carries every member the boot path touches — so a standalone class
  // replaces the extends with zero runtime change.
  Settings: class {
    static async init() {
      return {
        getCwd: () => 'C:/stub-boot',
        cloneForCwd: async () => ({ getCwd: () => 'C:/stub-boot' }),
      };
    }
    // Delegated so same-process suites that probe for the isolated loader
    // (omp-parity.test loadRealSettings) keep the genuine loader: bun's
    // mock.module interception also covers direct file-URL imports of the
    // package's source files, so capability probing is the only pierce.
    // This harness itself never calls loadIsolated.
    static loadIsolated: typeof realSdk.Settings.loadIsolated = realSdk.Settings.loadIsolated.bind(realSdk.Settings);
  },
  VERSION: 'test',
  discoverAuthStorage: () => ({}),
  // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
  BUILTIN_TOOLS: [] as string[],
}));

const { OmpHostEngine } = await import('./engine.ts');

afterAll(() => {
  rmSync(agentDir, { recursive: true, force: true, maxRetries: 5 });
});

// Engine methods destructure their full wire-arg record (every member is
// required in the synthesized parameter type); calls that omit optional
// fields pad them with `undefined` — the destructured values are identical.
describe('OmpHostEngine prompt dispatch', () => {
  test('submits with TUI steer semantics and does not reject while streaming', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'hello', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const session = sessionFor('s1');
    expect(session.prompt).toHaveBeenCalledWith('hello', { images: [], streamingBehavior: 'steer' });
    expect(session.steer).not.toHaveBeenCalled();

    session.isStreaming = true;
    await expect(
      engine.prompt({ sessionID: 's1', directory: '/repo', text: 'mid turn', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined }),
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
      await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
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
    const bare = await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'bare', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    expect(bare?.info.model).toEqual({ providerID: 'p1', modelID: 'current-model' });

    // An explicit session level rides model.variant — the exact send-time
    // snapshot the turn runs with.
    session.thinkingLevel = 'xhigh';
    const stamped = await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'stamped', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    expect(stamped?.info.model).toEqual({ providerID: 'p1', modelID: 'current-model', variant: 'xhigh' });
    delete session.thinkingLevel;
  });

  test('maps wire delivery "queue" to a follow-up', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'after this turn', delivery: 'queue', model: undefined, agent: undefined, images: undefined, messageID: undefined });
    const session = sessionFor('s2');
    expect(session.prompt).toHaveBeenCalledWith('after this turn', { images: [], streamingBehavior: 'followUp' });
  });

  test('getTodos projects the latest SDK TodoPhase via its `tasks` field', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'plan', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const session = sessionFor('s2');
    session.getTodoPhases = () => [
      { name: 'earlier', tasks: [{ content: 'stale', status: 'completed' }] },
      { name: 'current', tasks: [{ content: 'write code', status: 'in_progress' }, { content: 'test it', status: 'pending' }] },
    ];
    // Regression: pre-18 field names (items/todos) made this read return []
    // unconditionally against SDK 18's TodoPhase { name, tasks } shape.
    expect(await engine.getTodos({ sessionID: 's2', directory: '/repo' })).toEqual([
      { content: 'write code', status: 'in_progress', priority: 'medium' },
      { content: 'test it', status: 'pending', priority: 'medium' },
    ]);
  });

  test('gives each embedded session a private agent registry', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const before = registries.length;
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'one', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'two', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const passed = createdOptions.slice(-2).map((options) => options.agentRegistry);
    expect(passed).toHaveLength(2);
    expect(registries.length).toBeGreaterThanOrEqual(before + 2);
    expect(passed[0]).not.toBe(passed[1]);
  });

  test('leaves the model unset so the SDK resolves the settings default', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'defaults', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const options = createdOptions.at(-1);
    expect(options?.model).toBeUndefined();
  });

  test('abort forwards to the live agent session and reports unknown sessions as false', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const session = sessionFor('s1');

    await expect(engine.abort({ sessionID: 's1', directory: '/repo' })).resolves.toBe(true);
    expect(session.abort).toHaveBeenCalledWith({ reason: 'User aborted' });

    await expect(engine.abort({ sessionID: 'never-materialized', directory: '/repo' })).resolves.toBe(false);
  });

  test('updateSession writes the registry under a live session\'s own directory', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });

    const updated = await engine.updateSession({ sessionID: 's1', directory: '/elsewhere', timeArchived: 123, title: undefined, metadata: undefined });

    // The live session owns /repo: the patch must land and be reported there,
    // never stranded as a phantom /elsewhere registry entry that listings under
    // the owning directory never read.
    expect(engine.registry.get('/repo', 's1')?.timeArchived).toBe(123);
    expect(engine.registry.get('/elsewhere', 's1')).toBe(null);
    expect(updated?.time?.archived).toBe(123);
  });

  test('abort force-disposes a session whose teardown never settles and emits session.idle', async () => {
    const engine = new OmpHostEngine({ agentDir, abortTeardownTimeoutMs: 25 });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'stuck turn' });
    const session = sessionFor('s2');
    // One signal-blind tool / never-settling post-prompt task: abort's own
    // teardown promise parks forever (pi-agent-session abort awaits it bare).
    session.abort = mock(() => new Promise(() => {}));

    await expect(engine.abort({ sessionID: 's2', directory: '/repo' })).resolves.toBe(true);
    expect(engine.sessions.has('s2')).toBe(false);
    expect(session.dispose).toHaveBeenCalled();

    // Escalation must settle every client: one durable session.idle, routed
    // under the session's own directory (module invariant).
    const idle = engine.bus.replay.filter(
      (entry) => entry.envelope.type === 'session.idle' && entry.envelope.properties.sessionID === 's2'
    );
    expect(idle).toHaveLength(1);
    expect(idle[0].directory).toBe('/repo');
  });

  test('abort survives a rejecting teardown without escalating', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up' });
    const session = sessionFor('s1');
    session.abort = mock(async () => {
      throw new Error('teardown blew up');
    });

    await expect(engine.abort({ sessionID: 's1', directory: '/repo' })).resolves.toBe(true);
    expect(engine.sessions.has('s1')).toBe(true);
    expect(engine.bus.replay.some((entry) => entry.envelope.type === 'session.idle')).toBe(false);
  });

  test('abort settles an awaiting-async session that has nothing running', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'ended awaiting async' });
    const session = sessionFor('s2');
    // agent_end(isTerminal=false) put the session in the engine-level
    // awaiting-async limbo and the resume never came: pi is idle (nothing to
    // abort) while the engine keeps reporting busy — Stop looked dead and
    // only a fresh steer healed it (agent_start clears awaitingAsyncSince).
    // Fake sessions are shared across tests; restore a healthy abort (an
    // earlier test parks s2's) and clear dispose's call history (an earlier
    // test exercised it).
    session.abort = mock(async () => {});
    session.dispose = mock(async () => {});
    const live = engine.sessions.get('s2');
    if (!live) throw new Error('s2 missing');
    live.awaitingAsyncSince = Date.now();
    const before = engine.bus.replay.filter((entry) => entry.envelope.type === 'session.idle').length;

    await expect(engine.abort({ sessionID: 's2', directory: '/repo' })).resolves.toBe(true);
    expect(live.awaitingAsyncSince === null).toBe(true);
    expect(engine.sessions.has('s2')).toBe(true);
    expect(session.dispose).not.toHaveBeenCalled();
    const idle = engine.bus.replay.filter((entry) => entry.envelope.type === 'session.idle');
    expect(idle.length).toBe(before + 1);
    expect(idle.at(-1)?.directory).toBe('/repo');
  });

  test('updateSession refuses a mis-addressed update for an idle session', async () => {
    const engine = new OmpHostEngine({ agentDir });

    // s3's transcript lives under /repo. A write addressed to /elsewhere owns
    // neither the transcript nor a registry entry and must not fabricate one:
    // before the guard it "succeeded" by answering with a synthesized session
    // while no listing keyed by the transcript's cwd could ever observe it.
    const refused = await engine.updateSession({ sessionID: 's3', directory: '/elsewhere', timeArchived: 123, title: undefined, metadata: undefined });
    expect(refused).toBe(null);
    expect(engine.registry.get('/elsewhere', 's3')).toBe(null);

    // The owning directory still applies the same update.
    const updated = await engine.updateSession({ sessionID: 's3', directory: '/repo', timeArchived: 123, title: undefined, metadata: undefined });
    expect(updated?.time?.archived).toBe(123);
    expect(engine.registry.get('/repo', 's3')?.timeArchived).toBe(123);
  });

  test('updateSession keeps registry-only sessions updatable for bookkeeping', async () => {
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 'pruned', { timeCreated: 1 });

    const updated = await engine.updateSession({ sessionID: 'pruned', directory: '/repo', timeArchived: 9, title: undefined, metadata: undefined });

    expect(updated?.time?.archived).toBe(9);
    expect(engine.registry.get('/repo', 'pruned')?.timeArchived).toBe(9);
  });


  test('keeps old-UI explicit model compatibility while model-free prompts do not switch', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    const before = session.setModel.mock.calls.length;

    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'roles request', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    expect(session.setModel.mock.calls.length).toBe(before);

    await engine.prompt({
      sessionID: 's1', directory: '/repo', text: 'legacy request',
      model: { providerID: 'p1', modelID: 'zzz-first' },
      agent: undefined, images: undefined, delivery: undefined, messageID: undefined,
    });
    expect(session.setModel.mock.calls.length).toBe(before + 1);
    expect(session.setModel).toHaveBeenLastCalledWith({ provider: 'p1', id: 'zzz-first' });
    expect(engine.registry.get('/repo', 's1')?.model).toBe('p1/zzz-first');
  });

  test('setSessionModel applies thinking-only changes without a model switch (GAP-06)', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');

    // Materialize first so the session exists.
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
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
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'wiring', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const options = createdOptions.at(-1);
    // Settings injection (06 §5.1 / master R6): the boot instance is handed
    // to the SDK instead of the process singleton.
    expect(options?.settings).toBe(await engine.settingsStore?.settingsFor('/repo'));
    // R13: hasUI comes from the dialog lease snapshot — no lease → false
    // (fail-closed), never from the capability.
    expect(options?.hasUI).toBe(false);
    // R7/R8: local:// resolution is session-pinned with zero global mutation.
    expect(options?.localProtocolOptions).toBeTruthy();
    expect(typeof options?.localProtocolOptions).toBe('object');
    // Retained for the agent-runs aggregator (04 §5.5).
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((engine.sessions.get('s1') as { agentRegistry: unknown }).agentRegistry).toBe(options?.agentRegistry);
    // A lease flip drives hasUI on the next materialization.
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'c1' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'with lease', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    expect(createdOptions.at(-1)?.hasUI).toBe(true);
  });

  test('attaches both extension and tool UI contexts during first materialization', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const beforeTool = toolUiContextCalls.length;
    const beforeExtension = extensionUiInitCalls.length;
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'browser-1' });

    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'first turn', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's2', clientId: 'browser-2' });
    await Promise.resolve();

    expect(createdOptions.at(-1)?.hasUI).toBe(true);
    const attach = toolUiContextCalls.slice(beforeTool).at(-1);
    expect(attach?.hasUI).toBe(true);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(typeof (attach?.uiContext as { askDialog?: unknown } | undefined)?.askDialog).toBe('function');
    const extensionInit = extensionUiInitCalls.slice(beforeExtension);
    expect(extensionInit).toHaveLength(1);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((extensionInit[0] as { options: { mode?: string } }).options).mode).toBe('json');
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(typeof ((extensionInit[0] as { options: { uiContext: { askDialog?: unknown } } }).options.uiContext).askDialog).toBe('function');
  });

  test('legacy build/plan metas normalize to the standard session; planYolo never reaches createAgentSession', async () => {
    // 02 §5.1/§5.8: the build/plan agent pair is deleted — 'plan' meta is a
    // standard session (plan mode is driven by the mode endpoints), so no
    // planYolo, no systemPrompt overlay, and no crash shape can occur.
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { agent: 'plan', model: 'p1/zzz-first' });
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'plan it', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const planOptions = createdOptions.at(-1);
    expect(planOptions?.planYolo).toBeUndefined();
    expect(planOptions?.systemPrompt).toBeUndefined();
    expect(JSON.stringify(planOptions ?? {})).not.toContain('autoApproveOnResolve');

    // A persona meta resolves the persona store (02 §5.1 D-B2): the overlay
    // shapes systemPrompt/toolNames at construction.
    engine.personas.set('grumpy', { name: 'grumpy', systemPrompt: 'Be grumpy.', tools: ['read'] });
    engine.registry.update('/repo', 's2', { persona: 'grumpy' });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'hello', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const personaOptions = createdOptions.at(-1);
    expect(personaOptions?.systemPrompt).toBe('Be grumpy.');
    expect(personaOptions?.toolNames).toEqual(['read']);
  });
});


describe('local:// per-session root wiring (spec 04 §5.2.3, TUI parity)', () => {
  test('cold resolve pins the session-private artifacts dir, never the project session dir', async () => {
    const engine = new OmpHostEngine({ agentDir });
    // No file on disk: the resolve fails 404, but the SDK handler materializes
    // the session's root (resolveLocalTarget mkdir) — where it lands is the
    // wiring assertion: <sessionDir>/s1/local, not the shared <sessionDir>/local.
    const res = await engine.uriDomain.uri.resolve({
      body: { scheme: 'local', ref: 'scratch.md', sessionID: 's1', directory: '/repo' },
    });
    expect(res.status).toBe(404);
    expect(existsSync(path.join(sessionDir, 's1', 'local'))).toBe(true);
    expect(existsSync(path.join(sessionDir, 'local'))).toBe(false);
    expect(existsSync(path.join(sessionDir, 's2', 'local'))).toBe(false);
  });

  test('unknown session answers 404 session-not-found without creating roots', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const res = await engine.uriDomain.uri.resolve({
      body: { scheme: 'local', ref: 'x.md', sessionID: 's_missing', directory: '/repo' },
    });
    expect(res.status).toBe(404);
    expect(await res.json()).toMatchObject({ error: 'session-not-found' });
    expect(existsSync(path.join(sessionDir, 's_missing'))).toBe(false);
  });

  test('materialized session pins its own manager artifacts dir into createAgentSession', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'hi' });
    const options = createdOptions.at(-1);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((options as { localProtocolOptions: { getSessionId: () => string } }).localProtocolOptions).getSessionId()).toBe('s3');
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((options as { localProtocolOptions: { getArtifactsDir: () => string } }).localProtocolOptions).getArtifactsDir()).toBe(path.join(sessionDir, 's3'));

    // Live path: a resolve after materialization reads the same session's
    // artifacts dir from its live manager, and only that session's root
    // materializes.
    const res = await engine.uriDomain.uri.resolve({
      body: { scheme: 'local', ref: 'note.md', sessionID: 's3', directory: '/repo' },
    });
    expect(res.status).toBe(404);
    expect(existsSync(path.join(sessionDir, 's3', 'local'))).toBe(true);
    expect(existsSync(path.join(sessionDir, 's2', 'local'))).toBe(false);
  });

  test('artifacts.list walks a session local root from disk with relative refs only', async () => {
    mkdirSync(path.join(sessionDir, 's3', 'local', 'scratch'), { recursive: true });
    writeFileSync(path.join(sessionDir, 's3', 'local', 'PLAN.md'), '# plan');
    writeFileSync(path.join(sessionDir, 's3', 'local', 'scratch', 'notes.md'), 'n');
    const engine = new OmpHostEngine({ agentDir });
    const res = await engine.uriDomain.artifacts.list({ directory: '/repo', sessionID: 's3' });
    // SAFETY: the artifacts endpoint answers {files, truncated}.
    const body = (await res.json()) as { files: Array<{ ref: string; size?: number }>; truncated?: boolean };
    expect(res.status).toBe(200);
    expect(body.files.map((file) => file.ref).sort()).toEqual(['PLAN.md', 'scratch/notes.md']);
    expect(body.files.every((file) => !file.ref.includes(':') && !file.ref.includes('\\'))).toBe(true);
    expect(body.files.every((file) => typeof file.size === 'number' && file.size > 0)).toBe(true);
    expect(body.truncated).toBe(false);

    const unknown = await engine.uriDomain.artifacts.list({ directory: '/repo', sessionID: 's_missing' });
    expect(unknown.status).toBe(404);
    expect(await unknown.json()).toMatchObject({ error: 'session-not-found' });
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

    expect(forked?.id).toBe('s1_fork');
    expect(forked?.title).toBe('root work (fork)');
    expect(forked?.parentID).toBeUndefined();
    expect(forked?.forkParentID).toBe('s1');
    expect(engine.registry.get('/repo', 's1_fork')).toMatchObject({ forkParentID: 's1' });
    // The listing projection carries the same split — the session-tree
    // builder reads forkParentID, the UI subagent checks read parentID.
    const listed = (await engine.listSessions({ directory: '/repo' })).find((s) => s.id === 's1_fork');
    expect(listed?.parentID).toBeUndefined();
    expect(listed?.forkParentID).toBe('s1');
  });
});

describe('OmpHostEngine fork boundary (wire messageID)', () => {
  test('bounds the fork before the selected message (omp /branch semantics)', async () => {
    forkMutations.length = 0;
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.update('/repo', 's1', { title: 'root work' });

    const forked = await engine.fork({ sessionID: 's1', directory: '/repo', messageID: 'e3' });

    expect(forked?.forkParentID).toBe('s1');
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
