import { describe, test, expect, mock, afterAll } from 'bun:test';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type { WireEventEnvelope } from './events.ts';

const agentDir = mkdtempSync(path.join(tmpdir(), 'omp-engine-test-'));
/** Failure guard only: resolves late so a missing signal fails the test
 * instead of hanging it. The pass path awaits the real event above. */
const guardAfter = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));
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
/** Registry-event frame the stateful mock hands to engine subscribers. */
type MockRegistryEvent = { type: string; ref: { id: string; sessionFile?: string } };
/** Module-scope handles onto the mocked global registry instance, captured at
 * construction — no type casts needed at the call sites. */
let emitGlobalRegistryEvent: (event: MockRegistryEvent) => void = () => {};
let clearGlobalRegistryRefs: () => void = () => {};
/** Mock registry shapes: what the engine's projections and this file's
 * assertions read off a rehydrated ref (SDK AgentRef subset). */
type MockRegistryHistory = { metrics?: { tokens?: number }; outputPath?: string };
type MockRegistryRef = {
  id: string;
  displayName?: string;
  kind?: string;
  parentId?: string;
  status: string;
  session: unknown;
  sessionFile?: string;
  activity?: string;
  createdAt?: number;
  lastActivity?: number;
  history?: MockRegistryHistory;
};
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
  executeBash: mock(async () => ({
    output: '',
    exitCode: 0,
    cancelled: false,
    truncated: false,
    totalLines: 0,
    totalBytes: 0,
    outputLines: 0,
    outputBytes: 0,
  })),
  isBashRunning: false,
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
    static #instance: InstanceType<typeof this> | undefined;
    static global() {
      this.#instance ??= new this();
      // Bind the module-scope handles to the singleton on every lookup: the
      // engine also constructs per-session registries whose constructors must
      // not steal them.
      emitGlobalRegistryEvent = (event) => this.#instance!.#emit(event);
      clearGlobalRegistryRefs = () => this.#instance!.#refs.clear();
      return this.#instance;
    }
    // Stateful registry: the real registerPersistedSubagents drives it in the
    // rehydration tests, so register/setHistory/emit must behave like the SDK's.
    #refs = new Map<string, MockRegistryRef>();
    #listeners = new Set<(event: { type: string; ref: unknown }) => void>();
    constructor() {
      // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
      registries.push(this);
    }
    list() {
      return [...this.#refs.values()];
    }
    get(id: string) {
      return this.#refs.get(id);
    }
    register(input: Omit<MockRegistryRef, 'status' | 'session'> & Partial<Pick<MockRegistryRef, 'status' | 'session'>>) {
      const ref: MockRegistryRef = { status: 'running', session: null, ...input };
      this.#refs.set(ref.id, ref);
      this.#emit({ type: 'registered', ref });
      return ref;
    }
    setHistory(id: string, history: MockRegistryHistory, expectedSessionFile?: string) {
      const ref = this.#refs.get(id);
      if (!ref || (expectedSessionFile !== undefined && ref.sessionFile !== expectedSessionFile)) return false;
      ref.history = { ...(ref.history ?? {}), ...history };
      this.#emit({ type: 'metadata_changed', ref });
      return true;
    }
    unregister(id: string) {
      const ref = this.#refs.get(id);
      if (!ref) return false;
      this.#refs.delete(id);
      this.#emit({ type: 'removed', ref });
      return true;
    }
    onChange(listener: (event: { type: string; ref: unknown }) => void) {
      this.#listeners.add(listener);
      return () => this.#listeners.delete(listener);
    }
    #emit(event: MockRegistryEvent) {
      for (const listener of this.#listeners) {
        try {
          listener(event);
        } catch {
          // Stale engines from earlier tests must not break the emit loop.
        }
      }
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

describe('agent-runs aggregator: process-global registry mapping (registry split-brain fix)', () => {

  test('global-registry subagent refs map to their owning live session via sessionFile', async () => {
    const engine = new OmpHostEngine({ agentDir });
    // Materialize a live session s_glob in /repo.
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    // A subagent ref registered by the task executor into the GLOBAL registry
    // (never the per-session one): sessionFile carries <ts>_<sessionID>/.
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const refs = [{ id: 'ScoutRun', displayName: 'Scout', kind: 'sub', status: 'running', session: null, sessionFile: path.join(sessionDir, '2026-09-04T00-00-00-000Z_s2', 'ScoutRun.jsonl'), createdAt: 1, lastActivity: 9 }] as unknown as ReturnType<typeof globalRegistry.list>;
    const originalList = globalRegistry.list.bind(globalRegistry);
    globalRegistry.list = () => refs;
    try {
      const snapshot = engine.uriDomain?.aggregator.refresh();
      const row = snapshot?.agentRuns.find((r) => r.agentId === 'ScoutRun');
      expect(row).toBeTruthy();
      expect(row?.sessionID).toBe('s2');
      expect(row?.directory).toBe('/repo');
      expect(row?.status).toBe('running');
      expect(row?.hasTranscript).toBe(true);
    } finally {
      globalRegistry.list = originalList;
    }
  });

  test('global refs without a live owning session stay out of the snapshot', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const refs = [{ id: 'GhostRun', displayName: 'Ghost', kind: 'sub', status: 'parked', session: null, sessionFile: path.join(sessionDir, '2026-09-04T00-00-00-000Z_s_unknown', 'GhostRun.jsonl'), createdAt: 1, lastActivity: 2 }] as unknown as ReturnType<typeof globalRegistry.list>;
    const originalList = globalRegistry.list.bind(globalRegistry);
    globalRegistry.list = () => refs;
    try {
      const snapshot = engine.uriDomain?.aggregator.refresh();
      expect(snapshot?.agentRuns.some((r) => r.agentId === 'GhostRun')).toBe(false);
    } finally {
      globalRegistry.list = originalList;
    }
  });
});

describe('subagent session read resolution (read-only drill-in)', () => {
  const subFile = path.join(sessionDir, '2026-09-04T00-00-00-000Z_s2', 'ScoutRun.jsonl');

  test('getSession resolves a subagent transcript by its own sessionID with wire parentID', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const originalList = globalRegistry.list.bind(globalRegistry);
    globalRegistry.list = () => [{ id: 'ScoutRun', displayName: 'Scout', kind: 'sub', status: 'parked', session: null, sessionFile: subFile, createdAt: 1, lastActivity: 9 }] as unknown as ReturnType<typeof globalRegistry.list>;
    // The header probe reads the real file — write a session header like
    // production transcripts carry (the old SessionManager mock made this
    // unnecessary; readSessionHeaderId bypasses the mock).
    mkdirSync(path.dirname(subFile), { recursive: true });
    writeFileSync(subFile, JSON.stringify({ type: 'session', version: 3, id: 'ScoutRun', timestamp: '2026-09-04T00:00:00.000Z', cwd: '/repo' }) + '\n' + JSON.stringify({ type: 'message', message: { role: 'user', content: 'count files', timestamp: 1 } }) + '\n');
    try {
      const session = await engine.getSession({ sessionID: 'ScoutRun', directory: '/repo' });
      expect(session?.id).toBe('ScoutRun');
      expect(session?.parentID).toBe('s2');
      expect(session?.title).toBe('Scout');
      // Messages read through the same resolution: a non-null page, empty here
      // (the mocked buildSessionContext yields no messages).
      const page = await engine.getMessagesPage({ sessionID: 'ScoutRun', directory: '/repo' });
      expect(Array.isArray(page?.messages)).toBe(true);
    } finally {
      globalRegistry.list = originalList;
    }
  });

  test('a transcript outside the directory sessions root is refused', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const originalList = globalRegistry.list.bind(globalRegistry);
    globalRegistry.list = () => [{ id: 'Foreign', displayName: 'F', kind: 'sub', status: 'parked', session: null, sessionFile: path.join(tmpdir(), 'elsewhere', 'Foreign.jsonl'), createdAt: 1, lastActivity: 2 }] as unknown as ReturnType<typeof globalRegistry.list>;
    try {
      expect(await engine.getSession({ sessionID: 'Foreign', directory: '/repo' })).toBeNull();
    } finally {
      globalRegistry.list = originalList;
    }
  });

  test('agent-runs rows carry childSessionID from the live session accessor', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const originalList = globalRegistry.list.bind(globalRegistry);
    globalRegistry.list = () => [{ id: 'ScoutRun', displayName: 'Scout', kind: 'sub', status: 'running', session: { sessionId: 'ScoutRun' }, sessionFile: subFile, createdAt: 1, lastActivity: 9 }] as unknown as ReturnType<typeof globalRegistry.list>;
    try {
      const snapshot = engine.uriDomain?.aggregator.refresh();
      const row = snapshot?.agentRuns.find((r) => r.agentId === 'ScoutRun');
      expect(row?.childSessionID).toBe('ScoutRun');
    } finally {
      globalRegistry.list = originalList;
    }
  });
});

describe('disk scan: historical run rows (restart persistence)', () => {
  test('ensureDirectory surfaces nested transcripts as historical rows with childSessionID', async () => {
    const engine = new OmpHostEngine({ agentDir });
    // Restart-persistence layout: <root>/<ts>_<hostID>/<Task>.jsonl. The
    // mocked SessionManager derives sessionId from the file basename, so
    // childSessionID === the task name here; the real engine reads the header.
    const subDir = path.join(sessionDir, '2026-09-05T00-00-00-000Z_s2');
    mkdirSync(subDir, { recursive: true });
    writeFileSync(path.join(subDir, 'BranchScout.jsonl'), JSON.stringify({ type: 'session', version: 3, id: 'BranchScout', timestamp: '2026-09-04T00:00:00.000Z', cwd: '/repo' }) + '\n' + JSON.stringify({ type: 'message', message: { role: 'user', content: 'research', timestamp: 1 } }) + '\n');
    const snapshot = await engine.uriDomain?.aggregator.ensureDirectory('/repo');
    const row = snapshot?.agentRuns.find((r) => r.agentId === 'BranchScout');
    expect(row).toBeTruthy();
    expect(row?.sessionID).toBe('s2');
    expect(row?.status).toBe('historical');
    expect(row?.childSessionID).toBe('BranchScout');
    expect(row?.hasTranscript).toBe(true);
  });

  test('listSessions stays host-only after the sidebar ruling', async () => {
    const engine = new OmpHostEngine({ agentDir });
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const subDir = path.join(sessionDir, '2026-09-05T00-00-00-000Z_s2');
    mkdirSync(subDir, { recursive: true });
    writeFileSync(path.join(subDir, 'BranchScout.jsonl'), JSON.stringify({ type: 'session', version: 3, id: 'BranchScout', timestamp: '2026-09-04T00:00:00.000Z', cwd: '/repo' }) + '\n' + JSON.stringify({ type: 'message', message: { role: 'user', content: 'research', timestamp: 1 } }) + '\n');
    const sessions = await engine.listSessions({ directory: '/repo' });
    // Subagent runs do not join the session list (maintainer ruling: the
    // sidebar stays host-sessions-only); reads still resolve them.
    expect(sessions.some((s) => (s as { parentID?: string }).parentID)).toBe(false);
    expect(await engine.getSession({ sessionID: 'BranchScout', directory: '/repo' })).toBeTruthy();
  });
  test('post-restart: session reads resolve a historical run with no registry ref', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const subDir = path.join(sessionDir, '2026-09-05T00-00-00-000Z_s2');
    mkdirSync(subDir, { recursive: true });
    writeFileSync(path.join(subDir, 'BranchScout.jsonl'), JSON.stringify({ type: 'session', version: 3, id: 'BranchScout', timestamp: '2026-09-04T00:00:00.000Z', cwd: '/repo' }) + '\n' + JSON.stringify({ type: 'message', message: { role: 'user', content: 'research', timestamp: 1 } }) + '\n');
    // No prompt (no live session), no registry refs — pure disk resolution.
    const snapshot = await engine.uriDomain?.aggregator.ensureDirectory('/repo');
    const row = snapshot?.agentRuns.find((r) => r.agentId === 'BranchScout');
    expect(row?.childSessionID).toBe('BranchScout');
    // The read endpoints resolve through the same disk cache.
    const session = await engine.getSession({ sessionID: 'BranchScout', directory: '/repo' });
    expect(session?.parentID).toBe('s2');
    expect(session?.title).toBe('BranchScout');
    const page = await engine.getMessagesPage({ sessionID: 'BranchScout', directory: '/repo' });
    expect(Array.isArray(page?.messages)).toBe(true);
  });
});

describe('consumption-time subagent rehydration (materialization)', () => {
  test('materializing a session re-registers settled runs with history and child ids', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    // Production artifacts layout: <root>/<ts>_<sessionID>/<Run>.jsonl next to
    // the host transcript <root>/<ts>_<sessionID>.jsonl.
    const artifactsDir = path.join(sessionDir, '2026-09-06T00-00-00-000Z_s9');
    mkdirSync(artifactsDir, { recursive: true });
    const weaverFile = path.join(artifactsDir, 'Weaver.jsonl');
    writeFileSync(weaverFile, [
      JSON.stringify({ type: 'session', id: 'weaver-child-1', parentId: null, timestamp: '2026-09-06T00:00:00.000Z' }),
      JSON.stringify({ type: 'session_init', id: 'i2', parentId: 'weaver-child-1', task: 'Map the retry ladder and report', agent: 'scout', modelRole: 'smol', readOnly: true, timestamp: '2026-09-06T00:00:01.000Z' }),
      JSON.stringify({ type: 'message', id: 'm3', parentId: 'i2', timestamp: '2026-09-06T00:01:00.000Z', message: { role: 'assistant', timestamp: '2026-09-06T00:01:00.000Z', content: [{ type: 'text', text: 'done' }], usage: { input: 10, output: 5, totalTokens: 15, cost: { total: 0.01 } }, provider: 'p1', model: 'm1', stopReason: 'stop' } }),
    ].join('\n') + '\n');
    writeFileSync(path.join(artifactsDir, 'Weaver.md'), '# Weaver output\n');
    sessionFiles.push({ id: 's9', path: `${artifactsDir}.jsonl` });
    // Rehydration is fire-and-forget off the materialization path. Subscribe
    // before prompting, then await the real completion signal — the registry's
    // metadata_changed event once Weaver's reconstructed history lands. The 2s
    // race is a failure guard so a broken path fails instead of hanging.
    const { promise: historyLanded, resolve: onHistory } = Promise.withResolvers<void>();
    const stopListen = globalRegistry.onChange((event) => {
      if (event.type === 'metadata_changed' && event.ref.id === 'Weaver') onHistory();
    });
    // The row reaches consumers through omp.agents.updated after the
    // rehydration path warms child ids; await that publish (the UI's own
    // signal), guarded so a broken path fails instead of hanging.
    const { promise: rowPublished, resolve: onRow } = Promise.withResolvers<void>();
    const stopBus = engine.ompBus.subscribeSince(Number.MAX_SAFE_INTEGER, (entry) => {
      if (entry.envelope.type !== 'omp.agents.updated') return;
      // SAFETY: test fixture narrowing — omp.agents.updated payload rows are
      // untyped on the envelope boundary; only the two asserted fields are read.
      const rows = entry.envelope.payload as { agentRuns?: Array<{ agentId?: string; childSessionID?: string }> };
      for (const candidate of rows.agentRuns ?? []) {
        if (candidate.agentId === 'Weaver' && candidate.childSessionID === 'weaver-child-1') onRow();
      }
    }, { directory: '/repo' });
    try {
      await engine.prompt({ sessionID: 's9', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
      await Promise.race([historyLanded, guardAfter(2000)]);
      await Promise.race([rowPublished, guardAfter(3000)]);
      const row = engine.uriDomain?.aggregator.refresh().agentRuns.find((r) => r.agentId === 'Weaver');
      expect(row).toBeTruthy();
      expect(row?.sessionID).toBe('s9');
      expect(row?.directory).toBe('/repo');
      expect(row?.status).toBe('parked');
      expect(row?.childSessionID).toBe('weaver-child-1');
      // SAFETY: test fixture narrowing — AgentRunHistory.metrics is unknown
      // on the wire boundary; the fixture's reconstructed summary carries tokens.
      const metrics = row?.history?.metrics as { tokens?: number } | undefined;
      expect(metrics?.tokens).toBe(15);
      // The drill-in target resolves through the re-registered ref — the read
      // that 404'd when the SDK had reclaimed the run and the disk cache was
      // stale.
      const child = await engine.getSession({ sessionID: 'weaver-child-1', directory: '/repo' });
      expect(child?.parentID).toBe('s9');
    } finally {
      stopListen();
      stopBus();
      const index = sessionFiles.findIndex((entry) => entry.id === 's9');
      if (index >= 0) sessionFiles.splice(index, 1);
      clearGlobalRegistryRefs();
    }
  });


  test('a removed run ref is re-registered while its owning session is live', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const { AgentRegistry } = await import('@oh-my-pi/pi-coding-agent');
    const globalRegistry = AgentRegistry.global();
    const artifactsDir = path.join(sessionDir, '2026-09-08T00-00-00-000Z_s10');
    mkdirSync(artifactsDir, { recursive: true });
    const regrowFile = path.join(artifactsDir, 'Regrow.jsonl');
    writeFileSync(regrowFile, [
      JSON.stringify({ type: 'session', id: 'regrow-child-1', parentId: null, timestamp: '2026-09-08T00:00:00.000Z' }),
      JSON.stringify({ type: 'session_init', id: 'i2', parentId: 'regrow-child-1', task: 'Audit the probe ladder', agent: 'scout', readOnly: true, timestamp: '2026-09-08T00:00:01.000Z' }),
    ].join('\n') + '\n');
    sessionFiles.push({ id: 's10', path: `${artifactsDir}.jsonl` });
    let removedSeen = false;
    const { promise: firstRegistered, resolve: onFirstRegistered } = Promise.withResolvers<void>();
    const { promise: reRegistered, resolve: onReRegistered } = Promise.withResolvers<void>();
    const stopListen = globalRegistry.onChange((event) => {
      if (event.ref.id !== 'Regrow') return;
      if (event.type === 'removed') removedSeen = true;
      if (event.type === 'registered') {
        if (removedSeen) onReRegistered();
        else onFirstRegistered();
      }
    });
    try {
      await engine.prompt({ sessionID: 's10', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
      // Materialization rehydration is fire-and-forget: await its registration
      // before tearing the ref down.
      await Promise.race([firstRegistered, guardAfter(2000)]);
      expect(globalRegistry.get('Regrow')).toBeTruthy();
      // The SDK settles and reclaims the ref (idle-TTL park, one-shot
      // settle): the row would vanish mid-view without re-registration.
      globalRegistry.unregister('Regrow');
      expect(globalRegistry.get('Regrow')).toBeUndefined();
      await Promise.race([reRegistered, guardAfter(3000)]);
      const row = engine.uriDomain?.aggregator.refresh().agentRuns.find((r) => r.agentId === 'Regrow');
      expect(row?.sessionID).toBe('s10');
      expect(row?.status).toBe('parked');
      expect(row?.childSessionID).toBe('regrow-child-1');
    } finally {
      stopListen();
      const index = sessionFiles.findIndex((entry) => entry.id === 's10');
      if (index >= 0) sessionFiles.splice(index, 1);
      clearGlobalRegistryRefs();
    }
  });
});

describe('disk-row cache invalidation on new runs', () => {
  test('a registry registered event drops the one-shot cache so late transcripts surface', async () => {
    const engine = new OmpHostEngine({ agentDir });
    // /repo must be a live directory for the invalidation mapping to apply.
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const subDir = path.join(sessionDir, '2026-09-07T00-00-00-000Z_s2');
    mkdirSync(subDir, { recursive: true });
    const primed = await engine.uriDomain?.aggregator.ensureDirectory('/repo');
    expect(primed?.agentRuns.some((r) => r.agentId === 'LateRun')).toBe(false);
    const lateFile = path.join(subDir, 'LateRun.jsonl');
    writeFileSync(lateFile, JSON.stringify({ type: 'session', version: 3, id: 'LateRun', timestamp: '2026-09-07T00:00:00.000Z', cwd: '/repo' }) + '\n');
    emitGlobalRegistryEvent({
      type: 'registered',
      ref: { id: 'LateRun', sessionFile: lateFile },
    });
    const rescanned = await engine.uriDomain?.aggregator.ensureDirectory('/repo');
    const row = rescanned?.agentRuns.find((r) => r.agentId === 'LateRun');
    expect(row?.status).toBe('historical');
    expect(row?.sessionID).toBe('s2');
    expect(row?.childSessionID).toBe('LateRun');
  });
});

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

  test('reports busy for the accepted-pre-dispatch window, then idle when no turn starts', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s2');
    session.isStreaming = false;
    const original = session.prompt;
    // Park inside session.prompt — the shape of the SDK's image-describe
    // fallback: dispatch work blocks before any agent_start event exists,
    // and the UI reads "idle + unanswered user message" as a failed send.
    let release: () => void = () => {};
    let promptEntered = false;
    session.prompt = mock(() => {
      promptEntered = true;
      return new Promise<boolean>((resolve) => { release = () => resolve(true); });
    });
    try {
      const pending = engine.prompt({ sessionID: 's2', directory: '/repo', text: 'with image', model: undefined, agent: undefined, images: [{ data: 'QUJD', mimeType: 'image/png' }], delivery: undefined, messageID: undefined });
      const deadline = Date.now() + 5_000;
      while (Date.now() < deadline) {
        if (promptEntered) break;
        await new Promise((r) => setTimeout(r, 10));
      }
      const busy = engine.bus.replay.filter(
        (e) => e.envelope.type === 'session.status' && e.envelope.properties.sessionID === 's2',
      );
      // SAFETY: bus payloads are the engine's own emitted wire shapes.
      expect((busy.at(-1)?.envelope.properties as { status?: { type?: string } } | undefined)?.status?.type).toBe('busy');
      // The authoritative snapshot must agree: without record.inFlight a
      // mid-window /session/status poll reports idle and the UI's resync
      // lowers the event-set busy right back — which is what re-raised the
      // "engine did not start a reply" banner during a long describe call.
      const statuses = await engine.getSessionStatuses({ directory: '/repo' });
      expect(statuses.s2?.type).toBe('busy');
      release();
      await pending;
      const replay = engine.bus.replay;
      const busyIdx = replay.findIndex((e) => e.envelope.type === 'session.status' && e.envelope.properties.sessionID === 's2');
      const idleIdx = replay.findIndex((e) => e.envelope.type === 'session.idle' && e.envelope.properties.sessionID === 's2');
      expect(busyIdx).toBeGreaterThanOrEqual(0);
      expect(idleIdx).toBeGreaterThan(busyIdx);
    } finally {
      session.prompt = original;
    }
  });

  test('emits no compensating idle when the dispatch left a turn running', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s3');
    session.isStreaming = true;
    try {
      await engine.prompt({ sessionID: 's3', directory: '/repo', text: 'mid turn', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
      expect(engine.bus.replay.some((e) => e.envelope.type === 'session.idle' && e.envelope.properties.sessionID === 's3')).toBe(false);
      expect(engine.bus.replay.some((e) => e.envelope.type === 'session.status' && e.envelope.properties.sessionID === 's3')).toBe(true);
    } finally {
      session.isStreaming = false;
    }
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
    // The shared temp registry carries state from earlier tests (a switch's
    // selector, or the materialization backfill of the session's own model).
    // This contract pins the never-materialized path: no selector → no model
    // option, so the SDK resolves the settings default.
    engine.registry.remove('/repo', 's1');
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'defaults', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    const options = createdOptions.at(-1);
    expect(options?.model).toBeUndefined();
  });

  test('materialization backfills the registry model and publishes the warm record', async () => {
    const engine = new OmpHostEngine({ agentDir });
    engine.registry.remove('/repo', 's1');
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm', model: undefined, agent: undefined, images: undefined, delivery: undefined, messageID: undefined });
    // The session's own model seeds the registry so a later cold read (idle
    // eviction) still reports a model instead of dropping the field.
    expect(engine.registry.get('/repo', 's1')?.model).toBe('p1/current-model');
    // The warm record reaches clients without waiting for a list refresh.
    const warm = engine.bus.replay.filter(
      (e) => e.envelope.type === 'session.updated' && e.envelope.properties.sessionID === 's1',
    );
    // SAFETY: bus payloads are the engine's own emitted wire shapes.
    const models = warm.map(
      (e) => (e.envelope.properties as { info?: { model?: { id: string; providerID: string } } } | undefined)?.info?.model,
    );
    expect(models).toContainEqual({ id: 'current-model', providerID: 'p1' });
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
    const session = sessionFor('s2');
    // A stuck turn is mid-stream: the dispatch window's compensating idle
    // only fires when prompt() ended without a running turn.
    session.isStreaming = true;
    await engine.prompt({ sessionID: 's2', directory: '/repo', text: 'stuck turn' });
    // One signal-blind tool / never-settling post-prompt task: abort's own
    // teardown promise parks forever (pi-agent-session abort awaits it bare).
    session.abort = mock(() => new Promise(() => {}));

    await expect(engine.abort({ sessionID: 's2', directory: '/repo' })).resolves.toBe(true);
    expect(engine.liveRecord('s2') != null).toBe(false);
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
    const session = sessionFor('s1');
    // Abort targets a running turn; a still-streaming session emits no
    // dispatch-window idle for the warm-up prompt.
    session.isStreaming = true;
    await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm up' });
    session.abort = mock(async () => {
      throw new Error('teardown blew up');
    });

    await expect(engine.abort({ sessionID: 's1', directory: '/repo' })).resolves.toBe(true);
    expect(engine.liveRecord('s1') != null).toBe(true);
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
    // The limbo state this test settles is "pi idle, engine busy": earlier
    // tests share the fake and may leave isStreaming set.
    session.isStreaming = false;
    const live = engine.liveRecord('s2')?.payload;
    if (!live) throw new Error('s2 missing');
    live.awaitingAsyncSince = Date.now();
    const before = engine.bus.replay.filter((entry) => entry.envelope.type === 'session.idle').length;

    await expect(engine.abort({ sessionID: 's2', directory: '/repo' })).resolves.toBe(true);
    expect(live.awaitingAsyncSince === null).toBe(true);
    expect(engine.liveRecord('s2') != null).toBe(true);
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
    expect((engine.liveRecord('s1')?.payload as { agentRegistry: unknown } | undefined)?.agentRegistry).toBe(options?.agentRegistry);
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

describe('OmpHostEngine executeBash (`!` local shell)', () => {
  const bashRecord = (overrides = {}) => ({
    role: 'bashExecution',
    command: 'pwd',
    output: '/repo\n',
    exitCode: 0,
    cancelled: false,
    truncated: false,
    timestamp: 0,
    ...overrides,
  });
  const bashResult = (overrides = {}) => ({
    output: '/repo\n',
    exitCode: 0,
    cancelled: false,
    truncated: false,
    totalLines: 1,
    totalBytes: 6,
    outputLines: 1,
    outputBytes: 6,
    ...overrides,
  });

  type PartView = { messageID?: string; shellAction?: { command?: string; output?: string; status?: string } };

  test('runs the session bash runner, emits running→settled cards, and echo-bridges the persisted row', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const events: WireEventEnvelope[] = [];
    engine.bus.subscribeSince(0, (entry) => events.push(entry.envelope), { directory: '/repo' });

    const session = sessionFor('s1');
    session.isStreaming = false;
    session.messages = [];
    const record = bashRecord();
    session.executeBash = mock(async (command: string, onChunk?: (chunk: string) => void) => {
      onChunk?.('/re');
      onChunk?.('po\n');
      record.timestamp = Date.now();
      session.messages.push(record);
      return bashResult();
    });

    const outcome = await engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'pwd' });

    expect(outcome.status).toBe('ok');
    if (outcome.status !== 'ok') return;
    expect(session.executeBash).toHaveBeenCalledWith('pwd', expect.any(Function), { excludeFromContext: false, useUserShell: true });
    expect(outcome.result).toMatchObject({ output: '/repo\n', exitCode: 0, cancelled: false, truncated: false, timedOut: false });
    expect(outcome.message.info.role).toBe('user');
    expect(outcome.message.info.metadata?.ompRole).toBe('bash');
    expect(outcome.message.parts[0]?.shellAction).toMatchObject({ command: 'pwd', output: '/repo\n', status: 'completed' });

    // SAFETY: test fixture narrowing — `part` here is the emitted
    // message.part.updated payload this test's own dispatch produces.
    const shellParts = events
      .filter((event) => event.type === 'message.part.updated')
      .map((event) => event.properties.part as PartView | undefined)
      .filter((part): part is PartView => Boolean(part?.shellAction));
    expect(shellParts.length).toBeGreaterThanOrEqual(3);
    // Every emitted card — running, streamed chunks, settled — carries the
    // dispatch-time live id, so the client reconciles onto one row.
    expect(new Set(shellParts.map((part) => part.messageID))).toEqual(new Set([outcome.message.info.id]));
    expect(shellParts[0]?.shellAction?.status).toBe('running');
    expect(shellParts[0]?.shellAction?.output).toBe('');
    expect(shellParts.map((part) => part.shellAction?.output)).toContain('/re');
    expect(shellParts.at(-1)?.shellAction?.status).toBe('completed');
    expect(shellParts.at(-1)?.shellAction?.output).toBe('/repo\n');

    // Busy is claimed up front and handed back once the run settles.
    const statuses = events.filter((event) => event.type === 'session.status' || event.type === 'session.idle');
    expect(statuses[0]?.properties.status).toEqual({ type: 'busy' });
    expect(statuses.at(-1)?.type).toBe('session.idle');

    // The canonical record id echo-bridges onto the live row id, so a later
    // transcript re-projection does not mint a second card.
    const projected = await engine.getMessages({ sessionID: 's1', directory: '/repo' });
    const row = projected?.find((message) => message.info.metadata?.ompRole === 'bash');
    expect(row?.info.id).toBe(outcome.message.info.id);
  });

  test('a non-zero exit projects an error card without failing the request', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    session.isStreaming = false;
    session.messages = [];
    const record = bashRecord({ command: 'false', output: 'boom', exitCode: 3 });
    session.executeBash = mock(async () => {
      record.timestamp = Date.now();
      session.messages.push(record);
      return bashResult({ output: 'boom', exitCode: 3 });
    });

    const outcome = await engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'false' });

    expect(outcome.status).toBe('ok');
    if (outcome.status !== 'ok') return;
    expect(outcome.result.exitCode).toBe(3);
    expect(outcome.message.parts[0]?.shellAction?.status).toBe('error');
    expect(outcome.message.info.metadata?.exitCode).toBe(3);
  });

  test('an aborted run settles as a cancelled card', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    session.isStreaming = false;
    session.messages = [];
    // The abort path (session.abort → abortBash) resolves the run with
    // cancelled: true — the record and result agree.
    const record = bashRecord({ command: 'sleep 60', output: '', exitCode: undefined, cancelled: true });
    session.executeBash = mock(async () => {
      record.timestamp = Date.now();
      session.messages.push(record);
      return bashResult({ output: '', exitCode: undefined, cancelled: true });
    });

    const outcome = await engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'sleep 60' });

    expect(outcome.status).toBe('ok');
    if (outcome.status !== 'ok') return;
    expect(outcome.result.cancelled).toBe(true);
    expect(outcome.message.parts[0]?.shellAction?.status).toBe('cancelled');
    expect(outcome.message.info.metadata?.cancelled).toBe(true);
  });

  test('a thrown dispatch settles the running card as an error and rethrows', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const events: WireEventEnvelope[] = [];
    engine.bus.subscribeSince(0, (entry) => events.push(entry.envelope), { directory: '/repo' });
    const session = sessionFor('s1');
    session.isStreaming = false;
    session.messages = [];
    session.executeBash = mock(async () => {
      throw new Error('runner exploded');
    });

    await expect(engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'pwd' })).rejects.toThrow('runner exploded');

    // SAFETY: test fixture narrowing — `part` is the message.part.updated
    // payload this test's own dispatch produces.
    const shellParts = events
      .filter((event) => event.type === 'message.part.updated')
      .map((event) => event.properties.part as PartView | undefined)
      .filter((part): part is PartView => Boolean(part?.shellAction));
    expect(shellParts.at(-1)?.shellAction?.status).toBe('error');
    expect(shellParts.at(-1)?.shellAction?.output).toBe('runner exploded');
    // The failed dispatch still hands the session back to idle.
    expect(events.at(-1)?.type).toBe('session.idle');
  });

  test('a mid-turn run defers its record; the echo registers when the record lands', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    session.messages = [];
    session.isStreaming = true;
    // Deferred append: while the session streams, the SDK parks the record
    // in pendingMessages — it is NOT visible in session.messages yet.
    session.executeBash = mock(async () => bashResult());

    const outcome = await engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'pwd' });
    expect(outcome.status).toBe('ok');
    if (outcome.status !== 'ok') return;
    const liveId = outcome.message.info.id;

    // The record lands at the turn's pendingMessages flush; the first
    // projection that sees it must re-emit the live row id, not the
    // canonical execution id.
    const record = bashRecord();
    record.timestamp = Date.now();
    session.messages.push(record);
    const projected = await engine.getMessages({ sessionID: 's1', directory: '/repo' });
    const row = projected?.find((message) => message.info.metadata?.ompRole === 'bash');
    expect(row?.info.id).toBe(liveId);
    session.isStreaming = false;
  });

  test('`cd` refuses outright rather than moving the pinned session directory', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const session = sessionFor('s1');
    session.executeBash = mock(async () => bashResult());

    const outcome = await engine.executeBash({ sessionID: 's1', directory: '/repo', command: 'cd ..' });

    expect(outcome.status).toBe('refused');
    expect(session.executeBash).not.toHaveBeenCalled();
  });

  test('an unknown session answers notFound and settles back to idle', async () => {
    const engine = new OmpHostEngine({ agentDir });
    const events: WireEventEnvelope[] = [];
    engine.bus.subscribeSince(0, (entry) => events.push(entry.envelope), { directory: '/repo' });

    const outcome = await engine.executeBash({ sessionID: 'missing', directory: '/repo', command: 'pwd' });

    expect(outcome.status).toBe('notFound');
    const statuses = events.filter((event) => event.type === 'session.status' || event.type === 'session.idle');
    expect(statuses[0]?.properties.status).toEqual({ type: 'busy' });
    expect(statuses.at(-1)?.type).toBe('session.idle');
  });
});
