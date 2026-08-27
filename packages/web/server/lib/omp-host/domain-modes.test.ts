// Tests for the session-modes & agents domain (spec 02; domain-modes.js).
//
// SDK shapes asserted here were verified against the installed source:
// - PlanYolo = { target: Model; thinkingLevel? } — sdk.ts:393-394,
//   session/agent-session-types.ts:89-92 (no `autoApproveOnResolve` field).
// - SessionManager.appendModeChange(mode, data?) — session-manager.ts:2179.
// - preparePlanForReview result — agent-session.ts:933-948:
//   { content:[{type:'text',...}], details:{planFilePath,title,planExists} }.

import { describe, test, expect, mock, afterAll } from 'bun:test';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type {
  AgentDefinitionAdapter,
  AgentDefinitionHandlersOptions,
  AgentDiscoveryResult,
  DiscoveredAgent,
  ModeAppendEntry,
  ModeSessionContext,
  ModesDomainDeps,
  ModesRouteContext,
  ModesRouteHandler,
  ModesRouteMount,
  ModesRouteRegistrationOptions,
  OmpAgentRecord,
  ModeChangeData,
  OmpPersona,
  OmpEventPublish,
  OmpEventPayload,
  PersonaHandlers,
  PersonaStore,
  PlanProposalSession,
  PlanReviewToolResult,
  PreparePlanReviewResult,
} from './domain-modes.ts';

const realSdk = await import('@oh-my-pi/pi-coding-agent');
const { parseAgent } = await import('@oh-my-pi/pi-coding-agent/task/agents');
mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  BUILTIN_TOOLS: { read: class {}, bash: class {}, write: class {}, task: class {} },
}));

const {
  createModeTracker,
  ModeDomainError,
  createAgentDefinitionHandlers,
  createPersonaHandlers,
  personaFor,
  planReviewBridge,
  createModesDomain,
  registerModesDomainRoutes,
  jsonFileStore,
  mapBackedStore,
  migrateSidecarAgents,
  serializeAgentMarkdown,
  DEFAULT_PLAN_FILE_PATH,
} = await import('./domain-modes.ts');

const tmpRoot = mkdtempSync(path.join(tmpdir(), 'omp-domain-modes-'));
afterAll(() => {
  rmSync(tmpRoot, { recursive: true, force: true, maxRetries: 5 });
});

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

/** Recorded omp.mode.changed envelope — the payload shape tracker assertions read. */
interface ModeChangedEvent {
  type: string;
  payload: { mode: string; data?: ModeChangeData };
  opts?: { durable?: boolean };
}

const recorder = () => {
  const events: Array<{ type: string; payload: OmpEventPayload; opts?: { durable?: boolean } }> = [];
  const appended: Array<{ mode: string; data: ModeChangeData | undefined }> = [];
  return {
    events,
    appended,
    publish: (type: string, payload: OmpEventPayload, opts?: { durable?: boolean }) => {
      events.push({ type, payload, opts });
    },
    appendEntry: (mode: string, data?: ModeChangeData) => {
      appended.push({ mode, data });
      return `entry-${appended.length}`;
    },
    modeEvents: (): ModeChangedEvent[] =>
      events.filter((event): event is ModeChangedEvent => event.type === 'omp.mode.changed'),
  };
};

const post = <T>(url: string, body?: T) =>
  new Request(url, {
    method: 'POST',
    body: JSON.stringify(body ?? {}),
    headers: { 'content-type': 'application/json' },
  });
const put = <T>(url: string, body?: T) => new Request(url, { method: 'PUT', body: JSON.stringify(body ?? {}) });
const ctxFor = (url: string): ModesRouteContext => ({ params: {}, url: new URL(url), headers: new Headers() });

/** Yield long enough for async bridge prepares to resolve and register pending. */
const tick = () => new Promise((resolve) => setTimeout(resolve, 5));



// ---------------------------------------------------------------------------
// 2. Mode tracker — transitions, persistence, projection
// ---------------------------------------------------------------------------

describe('createModeTracker transitions (spec 02 §5.4)', () => {
  test('plan enter persists a mode_change entry and publishes omp.mode.changed (durable)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    const snapshot = tracker.enterMode('plan', {});
    expect(snapshot.mode).toBe('plan');
    expect(snapshot.plan).toEqual({
      planFilePath: DEFAULT_PLAN_FILE_PATH,
      paused: false,
      hasDraftContent: false,
    });
    expect(rec.appended).toEqual([{ mode: 'plan', data: { planFilePath: DEFAULT_PLAN_FILE_PATH } }]);
    expect(rec.modeEvents()).toEqual([
      { type: 'omp.mode.changed', payload: { mode: 'plan', data: { planFilePath: DEFAULT_PLAN_FILE_PATH } }, opts: { durable: true } },
    ]);
  });

  test('plan three-state toggle: pause → plan_paused, resume → plan, exit → none (TUI 3386-3409 parity)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('plan', { planFilePath: 'local://auth-plan.md' });
    tracker.pauseMode('plan');
    expect(tracker.mode).toBe('plan_paused');
    expect(tracker.snapshot().plan).toMatchObject({ planFilePath: 'local://auth-plan.md', paused: true });
    tracker.resumeMode('plan');
    expect(tracker.mode).toBe('plan');
    tracker.exitMode();
    expect(tracker.mode).toBe('none');
    expect(tracker.snapshot().plan).toBeUndefined();
    expect(rec.appended.map((entry) => entry.mode)).toEqual(['plan', 'plan_paused', 'plan', 'none']);
  });

  test('goal enter publishes but does NOT append — the SDK GoalRuntime owns goal entries (agent-session.ts:1420-1426)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('goal', { objective: 'ship it' });
    expect(tracker.mode).toBe('goal');
    expect(rec.appended).toEqual([]);
    expect(rec.modeEvents().map((event) => event.payload.mode)).toEqual(['goal']);
  });

  test('goal exit appends none (TUI interactive-mode.ts:2966 parity) and persist:false skips it', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('goal', {});
    tracker.exitMode();
    expect(rec.appended).toEqual([{ mode: 'none', data: undefined }]);
    expect(tracker.mode).toBe('none');

    tracker.enterMode('goal', {});
    tracker.exitMode({ persist: false });
    expect(rec.appended).toHaveLength(1);
  });

  test('mutual exclusion matrix plan × goal × vibe answers 409 with the TUI conflict copy', () => {
    const cases = [
      // [active mode, entering mode, expectConflict]
      ['plan', 'goal', 'plan'],
      ['plan', 'vibe', 'plan'],
      ['plan_paused', 'goal', 'plan'],
      ['plan_paused', 'vibe', 'plan'],
      ['goal', 'plan', 'goal'],
      ['goal', 'vibe', 'goal'],
      ['goal_paused', 'plan', 'goal'],
      ['goal_paused', 'vibe', 'goal'],
      ['vibe', 'plan', 'vibe'],
      ['vibe', 'goal', 'vibe'],
      ['loop', 'plan', 'loop'],
    ];
    for (const [active, entering, conflict] of cases) {
      const tracker = createModeTracker({});
      if (active === 'plan_paused' || active === 'goal_paused') {
        tracker.enterMode(active.replace('_paused', ''), {});
        tracker.pauseMode(active.replace('_paused', ''));
      } else if (active === 'vibe') {
        tracker.enterMode('vibe', { previousTools: ['read'] });
      } else if (active === 'loop') {
        tracker.enterMode('loop', { count: 3 });
      } else {
        tracker.enterMode(active, {});
      }
      let caught = null;
      try {
        tracker.enterMode(entering, entering === 'vibe' ? { previousTools: [] } : {});
      } catch (error) {
        caught = error;
      }
      expect(caught, `${entering} into ${active} must conflict`).toBeInstanceOf(ModeDomainError);
      expect(caught.status).toBe(409);
      expect(caught.body).toEqual({
        error: 'mode-conflict',
        conflict,
        message: `Exit ${conflict} mode first.`,
      });
    }
  });

  test('allowed non-conflicting combinations: goal×loop coexist (TUI continuation-suppression parity)', () => {
    const tracker = createModeTracker({});
    tracker.enterMode('loop', { count: 2 });
    expect(() => tracker.enterMode('goal', { objective: 'x' })).not.toThrow();
    expect(tracker.mode).toBe('goal');
    expect(tracker.snapshot().loop).toBeUndefined(); // loop projection yields to the goal mode field
  });

  test('same-mode re-enter is an idempotent no-op (no entry, no event)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('plan', {});
    const before = rec.modeEvents().length;
    tracker.enterMode('plan', {});
    expect(rec.modeEvents().length).toBe(before);
    expect(rec.appended).toHaveLength(1);
  });

  test('vibe enter requires the captured previousTools and persists them (TUI 3524 parity)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    expect(() => tracker.enterMode('vibe', {})).toThrow(ModeDomainError);
    tracker.enterMode('vibe', { previousTools: ['read', 'bash'] });
    expect(rec.appended).toEqual([{ mode: 'vibe', data: { previousTools: ['read', 'bash'] } }]);
    tracker.exitMode();
    expect(rec.appended.map((entry) => entry.mode)).toEqual(['vibe', 'none']);
  });

  test('loop is never persisted (TUI writes no loop mode_change entries)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('loop', { count: 3, prompt: 'keep going' });
    expect(tracker.snapshot().loop).toEqual({ state: 'running', remaining: 3, limit: 3, prompt: 'keep going' });
    tracker.pauseMode('loop');
    expect(tracker.snapshot().loop).toMatchObject({ state: 'paused', remaining: 3 });
    tracker.exitMode();
    expect(tracker.mode).toBe('none');
    expect(rec.appended).toEqual([]);
    expect(rec.modeEvents().map((event) => event.payload.mode)).toEqual(['loop', 'loop', 'none']);
  });

  test('applyGoalUpdate derives goal_paused from goal.status and shapes snapshot.goal (02 §5.4 GET shape)', () => {
    const tracker = createModeTracker({});
    tracker.enterMode('goal', {});
    const goal = { id: 'g1', objective: 'o', status: 'active', tokensUsed: 10, timeUsedSeconds: 1 };
    const goalState = { enabled: true, mode: 'active', goal };
    tracker.applyGoalUpdate(goal, goalState);
    expect(tracker.snapshot().goal).toEqual({ ...goal, state: goalState });
    tracker.applyGoalUpdate({ ...goal, status: 'paused' }, goalState);
    expect(tracker.mode).toBe('goal_paused');
    // A goal update outside goal modes must not hijack the mode field.
    const other = createModeTracker({});
    other.enterMode('plan', {});
    other.applyGoalUpdate(goal, goalState);
    expect(other.mode).toBe('plan');
  });

  test('prewalk is an orthogonal status bit publishing mode:"prewalk" (02 §5.7)', () => {
    const rec = recorder();
    const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
    tracker.enterMode('plan', {});
    tracker.setPrewalk(true, { target: '@smol' });
    expect(tracker.snapshot()).toMatchObject({ mode: 'plan', prewalk: { target: '@smol' } });
    const last = rec.modeEvents().at(-1);
    expect(last.payload).toEqual({ mode: 'prewalk', data: { target: '@smol' } });
    expect(last.opts.durable).toBe(true);
    tracker.setPrewalk(false);
    expect(tracker.snapshot().prewalk).toBeUndefined();
  });

  test('cold-start recovery from buildSessionContext restores projection without appending (02 §5.4)', () => {
    for (const [context, expectedMode, expectedPlanPath] of [
      [{ mode: 'plan', modeData: { planFilePath: 'local://auth-plan.md' } }, 'plan', 'local://auth-plan.md'],
      [{ mode: 'plan_paused' }, 'plan_paused', DEFAULT_PLAN_FILE_PATH],
      [{ mode: 'goal', modeData: { goal: { id: 'g', status: 'active' } } }, 'goal', null],
      [{ mode: 'goal_paused', modeData: { goal: { id: 'g', status: 'paused' } } }, 'goal_paused', null],
      [{ mode: 'vibe', modeData: { previousTools: ['read'] } }, 'vibe', null],
      [{ mode: 'none' }, 'none', null],
      [{}, 'none', null],
    ] as const) {
      const rec = recorder();
      const tracker = createModeTracker({ publish: rec.publish, appendEntry: rec.appendEntry });
      const snapshot = tracker.recoverFromSessionContext(context);
      expect(snapshot.mode, `recover ${JSON.stringify(context)}`).toBe(expectedMode);
      expect(rec.appended).toEqual([]);
      expect(rec.modeEvents()).toHaveLength(1);
      if (expectedMode === 'plan' || expectedMode === 'plan_paused') {
        expect(snapshot.plan.planFilePath).toBe(expectedPlanPath);
      }
      if (expectedMode === 'goal' || expectedMode === 'goal_paused') {
        expect(snapshot.goal).toMatchObject({ id: 'g' });
      }
    }
  });

  test('snapshot carries persona and review when set', () => {
    const tracker = createModeTracker({});
    tracker.setPersona('reviewer-persona');
    tracker.enterMode('plan', {});
    tracker.setReview({ planFilePath: 'local://x-plan.md', title: 'x', planExists: true });
    expect(tracker.snapshot()).toEqual({
      mode: 'plan',
      persona: 'reviewer-persona',
      plan: {
        planFilePath: DEFAULT_PLAN_FILE_PATH,
        paused: false,
        hasDraftContent: false,
        review: { planFilePath: 'local://x-plan.md', title: 'x', planExists: true },
      },
    });
  });

  test('invalid transitions and payloads answer 400s', () => {
    const tracker = createModeTracker({});
    expect(() => tracker.enterMode('none')).toThrow(ModeDomainError);
    expect(() => tracker.enterMode('bogus')).toThrow(ModeDomainError);
    expect(() => tracker.enterMode('loop', { count: 0 })).toThrow(ModeDomainError);
    expect(() => tracker.pauseMode('plan')).toThrow(ModeDomainError); // not active
    tracker.enterMode('plan', {});
    expect(() => tracker.pauseMode('plan')).not.toThrow();
    expect(() => tracker.pauseMode('plan')).toThrow(ModeDomainError); // already paused
    expect(() => tracker.resumeMode('plan')).not.toThrow(); // plan_paused → plan
    expect(() => tracker.resumeMode('plan')).toThrow(ModeDomainError); // not paused anymore
  });
});

// ---------------------------------------------------------------------------
// 3. Agent definitions CRUD
// ---------------------------------------------------------------------------

// In-memory discovery + fs fake: writes land in a files Map, discovery
// re-parses them with the REAL SDK parser (round-trip fidelity), bundled
// agents stay read-only, and project/user dirs mirror the engine adapters.
const definitionHarness = (overrides: Partial<AgentDefinitionHandlersOptions> = {}) => {
  const refreshCalls: Array<string | null> = [];
  const revealCalls: string[] = [];
  const root = path.join(tmpRoot, `harness-${Math.random().toString(36).slice(2)}`);
  const userAgentsDir = path.join(root, 'user-agents');
  const projAgentsDir = path.join(root, 'proj', '.omp', 'agents');
  const files = new Map<string, string>();
  const writes: Array<{ path: string; content: string }> = [];
  const deletes: string[] = [];
  const bundled: DiscoveredAgent[] = [
    { name: 'scout', description: 'Read-only recon', systemPrompt: 'Scout.', source: 'bundled' },
    { name: 'task', description: 'General worker', systemPrompt: 'Work.', source: 'bundled', model: ['@task'], spawns: '*' },
  ];
  const discover = async (directory: string | null): Promise<AgentDiscoveryResult> => {
    const agents: DiscoveredAgent[] = [];
    const seen = new Set<string>();
    for (const [dir, source] of [
      [path.join(path.resolve(directory ?? root), '.omp', 'agents'), 'project'],
      [userAgentsDir, 'user'],
    ] as const) {
      for (const file of [...files.keys()].filter((p) => p.startsWith(dir + path.sep) && p.endsWith('.md')).sort()) {
        const agent = parseAgent(file, files.get(file), source, 'off');
        if (!seen.has(agent.name)) { seen.add(agent.name); agents.push(agent); }
      }
    }
    for (const agent of bundled) if (!seen.has(agent.name)) agents.push(agent);
    return { agents, projectAgentsDir: files.size > 0 && [...files.keys()].some((p) => p.startsWith(projAgentsDir)) ? projAgentsDir : null };
  };
  const handlers = createAgentDefinitionHandlers({
    discover,
    writeFile: async (p, content) => {
      files.set(p, content);
      writes.push({ path: p, content });
    },
    deleteFile: async (p) => {
      if (!files.has(p)) return false;
      files.delete(p);
      deletes.push(p);
      return true;
    },
    readFile: async (p) => {
      const content = files.get(p);
      if (content === undefined) throw new Error(`ENOENT: ${p}`);
      return content;
    },
    userAgentsDir,
    projectAgentsDirFor: (directory) => path.join(path.resolve(directory ?? root), '.omp', 'agents'),
    allowedTools: new Set(['read', 'bash', 'write', 'task', 'yield', 'mcp__custom']),
    onDefinitionsChanged: async (directory) => { refreshCalls.push(directory); },
    revealFile: async (filePath) => { revealCalls.push(filePath); },
    ...overrides,
  });
  return { handlers, files, writes, deletes, userAgentsDir, projAgentsDir, root, refreshCalls, revealCalls };
};

const ctxForName = (name: string | undefined, directory: string): ModesRouteContext => ({
  params: { name },
  url: new URL(`http://x/omp/agent-definitions/${name}?directory=${encodeURIComponent(directory)}`),
  headers: new Headers(),
});

describe('agent-definitions CRUD (spec 02 §5.2 — discovery chain + .md storage)', () => {
  const PROJ_DIR = '/tmp/wire-proj';

  test('create writes the .md, re-discovers the record (201); list joins sources; delete → 204', async () => {
    const { handlers, files, writes, deletes, userAgentsDir } = definitionHarness();
    const created = await handlers.create(
      post('http://x/omp/agent-definitions', {
        scope: 'user',
        definition: {
          name: 'reviewer',
          description: 'Review code.',
          systemPrompt: 'Be thorough.',
          model: ['@smol'],
          thinkingLevel: 'medium',
          tools: ['read'],
          spawns: '*',
          prewalk: true,
          advisor: '@slow:high',
        },
      }),
      ctxForName(undefined, PROJ_DIR),
    );
    expect(created.status).toBe(201);
    const record = await created.json();
    expect(record).toMatchObject({
      name: 'reviewer',
      description: 'Review code.',
      source: 'user',
      systemPrompt: 'Be thorough.',
      model: ['@smol'],
      thinkingLevel: 'medium',
      tools: ['read', 'yield'],
      spawns: '*',
      prewalk: true,
      advisor: '@slow:high',
    });
    // The written file is the discovery truth (SDK frontmatter contract).
    const filePath = path.join(userAgentsDir, 'reviewer.md');
    expect(files.get(filePath)).toContain('name: reviewer');
    expect(files.get(filePath)).toContain('description: Review code.');
    expect(writes).toHaveLength(1);

    const listed = await handlers.list(null, ctxForName(undefined, PROJ_DIR));
    // SAFETY: list() answers the read projection `{ agents: OmpAgentRecord[] }`
    // (the createAgentDefinitionHandlers contract asserted throughout this block).
    const payload = (await listed.json()) as { agents: OmpAgentRecord[] };
    expect(payload.agents.map((a) => a.name)).toEqual(['reviewer', 'scout', 'task']);
    expect(payload.agents.map((a) => a.source)).toEqual(['user', 'bundled', 'bundled']);

    const removed = await handlers.remove(null, ctxForName('reviewer', PROJ_DIR));
    expect(removed.status).toBe(204);
    expect(deletes).toEqual([filePath]);
  });
  test('update preserves hand-authored frontmatter keys the form never shows (P9)', async () => {
    const harness = definitionHarness();
    const filePath = path.join(harness.userAgentsDir, 'custom.md');
    harness.files.set(filePath, [
      '---',
      'name: custom',
      'description: Hand-authored with extra keys',
      'autoloadSkills:',
      '  - theme-system',
      'blocking: true',
      'output: json',
      'vibe: spicy   # unknown-to-SDK key with a comment',
      'tools:',
      '  - read',
      '---',
      '',
      'Original prompt.',
      '',
    ].join('\n'));
    const response = await harness.handlers.update(
      new Request('http://x/omp/agent-definitions/custom', {
        method: 'PUT',
        body: JSON.stringify({ definition: { description: 'Edited in the GUI' } }),
      }),
      ctxForName('custom', PROJ_DIR),
    );
    expect(response.status).toBe(200);
    const written = harness.writes.at(-1)?.content ?? '';
    // Unknown + SDK-extra keys survive verbatim.
    expect(written).toContain('autoloadSkills:');
    expect(written).toContain('- theme-system');
    expect(written).toContain('blocking: true');
    expect(written).toContain('output: json');
    expect(written).toContain('vibe: spicy');
    // Patched key lands; untouched keys keep their values.
    expect(written).toContain('description: Edited in the GUI');
    expect(written).toContain('- read');
    // Unpatched systemPrompt survives (merge carried it from discovery).
    expect(written).toContain('Original prompt.');
    // The write still round-trips through the SDK parser (discover → 200 record).
    const reread = await harness.handlers.get(new Request('http://x/omp/agent-definitions/custom'), ctxForName('custom', PROJ_DIR));
    expect(reread.status).toBe(200);
  });


  test('duplicate create and unknown-name delete throw the documented domain errors', async () => {
    const { handlers } = definitionHarness();
    const body = { definition: { name: 'scout', description: 'shadow', systemPrompt: 'p' } };
    let caught = null;
    try {
      await handlers.create(post('http://x', body), ctxForName(undefined, PROJ_DIR));
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(ModeDomainError);
    expect(caught.status).toBe(409);
    expect(caught.body.error).toBe('agent-definition-exists');

    let missing = null;
    try {
      await handlers.remove(null, ctxForName('nope', PROJ_DIR));
    } catch (error) {
      missing = error;
    }
    expect(missing?.status).toBe(404);
    expect(missing?.body.error).toBe('not-found');
  });

  test('bundled definitions are read-only (update and delete → 409 bundled-read-only)', async () => {
    const { handlers } = definitionHarness();
    for (const run of [
      () => handlers.update(
        put('http://x/omp/agent-definitions/scout', { definition: { description: 'x' } }),
        ctxForName('scout', PROJ_DIR),
      ),
      () => handlers.remove(null, ctxForName('scout', PROJ_DIR)),
    ]) {
      let caught = null;
      try {
        await run();
      } catch (error) {
        caught = error;
      }
      expect(caught?.status).toBe(409);
      expect(caught?.body.error).toBe('bundled-read-only');
    }
  });

  test('tools are validated against the allowlist (unknown-tools 400)', async () => {
    const { handlers } = definitionHarness();
    let caught = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a1', description: 'd', systemPrompt: 'p', tools: ['read', 'nonsense'] } }),
        ctxForName(undefined, PROJ_DIR),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(400);
    expect(caught.body).toEqual({ error: 'unknown-tools', tools: ['nonsense'] });
  });

  test('thinkingLevel is validated against the SDK selector set (invalid-thinking-level 400)', async () => {
    const { handlers } = definitionHarness();
    let caught = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a2', description: 'd', systemPrompt: 'p', thinkingLevel: 'ultra' } }),
        ctxForName(undefined, PROJ_DIR),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(400);
    expect(caught.body.error).toBe('invalid-thinking-level');
  });

  test('description + systemPrompt are required (400 invalid-description / invalid-prompt)', async () => {
    const { handlers } = definitionHarness();
    let noDescription = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a3', systemPrompt: 'p' } }),
        ctxForName(undefined, PROJ_DIR),
      );
    } catch (error) {
      noDescription = error;
    }
    expect(noDescription?.body.error).toBe('invalid-description');

    let noPrompt = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a4', description: 'd' } }),
        ctxForName(undefined, PROJ_DIR),
      );
    } catch (error) {
      noPrompt = error;
    }
    expect(noPrompt?.body.error).toBe('invalid-prompt');
  });

  test('project scope is gated behind settingsProjectScopes (409 until lit, R2-M4)', async () => {
    const gatedOff = definitionHarness();
    let caught = null;
    try {
      await gatedOff.handlers.create(
        post('http://x', { scope: 'project', definition: { name: 'a5', description: 'd', systemPrompt: 'p' } }),
        ctxForName(undefined, PROJ_DIR),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(409);
    expect(caught.body.error).toBe('project-scope-unavailable');

    const gatedOn = definitionHarness({ settingsProjectScopes: true });
    const created = await gatedOn.handlers.create(
      post('http://x', { scope: 'project', definition: { name: 'a5', description: 'd', systemPrompt: 'p' } }),
      ctxForName(undefined, PROJ_DIR),
    );
    expect(created.status).toBe(201);
    // SAFETY: create answers 201 with the re-discovered OmpAgentRecord (route contract).
    expect(((await created.json()) as OmpAgentRecord).source).toBe('project');
    expect([...gatedOn.files.keys()][0]).toBe(path.join(path.resolve(PROJ_DIR), '.omp', 'agents', 'a5.md'));
  });

  test('update patches the file, renames via renameTo (write new + delete old), and null clears a field', async () => {
    const { handlers, files, deletes } = definitionHarness();
    await handlers.create(
      post('http://x', { definition: { name: 'old', description: 'd', systemPrompt: 'p', model: ['@smol'] } }),
      ctxForName(undefined, PROJ_DIR),
    );
    const updated = await handlers.update(
      put('http://x/omp/agent-definitions/old', {
        definition: { systemPrompt: 'new prompt', model: null },
        renameTo: 'fresh',
      }),
      ctxForName('old', PROJ_DIR),
    );
    // SAFETY: update answers the re-discovered record; discovery always sets
    // filePath (DiscoveredAgent.filePath → definitionToRecord).
    const record = (await updated.json()) as OmpAgentRecord & { filePath: string };
    expect(record.name).toBe('fresh');
    expect(record.systemPrompt).toBe('new prompt');
    expect(record.model).toBeUndefined();
    expect(files.has(path.join(record.filePath))).toBe(true);
    expect(deletes).toEqual([path.join(path.dirname(record.filePath), 'old.md')]);
  });

  test('mutations fire onDefinitionsChanged (hot reload, 02 §5.2 refresh) and the refresh handler answers 204', async () => {
    const { handlers, refreshCalls } = definitionHarness();
    await handlers.create(
      post('http://x', { scope: 'user', definition: { name: 'hot-probe', description: 'd', systemPrompt: 'p' } }),
      ctxForName(undefined, PROJ_DIR),
    );
    await handlers.update(
      put('http://x/omp/agent-definitions/hot-probe', { definition: { systemPrompt: 'p2' } }),
      ctxForName('hot-probe', PROJ_DIR),
    );
    await handlers.remove(null, ctxForName('hot-probe', PROJ_DIR));
    expect(refreshCalls).toHaveLength(3);

    const refreshed = await handlers.refresh(null, ctxForName(undefined, PROJ_DIR));
    expect(refreshed.status).toBe(204);
    expect(refreshCalls).toHaveLength(4);
  });

  test('reveal opens the definition file for editable layers; bundled and unknown answer 409/404', async () => {
    const { handlers, revealCalls, userAgentsDir } = definitionHarness();
    await handlers.create(
      post('http://x', { scope: 'user', definition: { name: 'revealable', description: 'd', systemPrompt: 'p' } }),
      ctxForName(undefined, PROJ_DIR),
    );
    const revealed = await handlers.reveal(null, ctxForName('revealable', PROJ_DIR));
    expect(revealed.status).toBe(200);
    expect(revealCalls).toEqual([path.join(userAgentsDir, 'revealable.md')]);

    let bundled = null;
    try {
      await handlers.reveal(null, ctxForName('scout', PROJ_DIR));
    } catch (error) {
      bundled = error;
    }
    expect(bundled?.status).toBe(409);
    expect(bundled?.body.error).toBe('bundled-read-only');

    let missing = null;
    try {
      await handlers.reveal(null, ctxForName('ghost', PROJ_DIR));
    } catch (error) {
      missing = error;
    }
    expect(missing?.status).toBe(404);
  });

  test('a failing onDefinitionsChanged never fails the mutation (dispatch stays fresh regardless)', async () => {
    const { handlers, files } = definitionHarness({
      onDefinitionsChanged: async () => { throw new Error('refresh exploded'); },
    });
    const created = await handlers.create(
      post('http://x', { scope: 'user', definition: { name: 'resilient', description: 'd', systemPrompt: 'p' } }),
      ctxForName(undefined, PROJ_DIR),
    );
    expect(created.status).toBe(201);
    expect([...files.keys()].some((p) => p.endsWith('resilient.md'))).toBe(true);
  });

  test('serializeAgentMarkdown round-trips through the real SDK parser', () => {
    const md = serializeAgentMarkdown({
      name: 'round',
      description: 'Round trip: with colon',
      systemPrompt: 'Body line 1\nline 2.',
      tools: ['read'],
      model: ['@smol', 'anthropic/*:high'],
      thinkingLevel: 'auto',
      spawns: '*',
      prewalk: true,
      advisor: '@slow',
    });
    const parsed = parseAgent(path.join(tmpRoot, 'round.md'), md, 'user', 'off');
    expect(parsed.name).toBe('round');
    expect(parsed.description).toBe('Round trip: with colon');
    expect(parsed.systemPrompt).toBe('Body line 1\nline 2.');
    expect(parsed.model).toEqual(['@smol', 'anthropic/*:high']);
    expect(parsed.thinkingLevel).toBe('auto');
    expect(parsed.spawns).toBe('*');
    expect(parsed.prewalk).toBe(true);
    expect(parsed.advisor).toBe('@slow');
  });
});

describe('migrateSidecarAgents (spec 02 §6.2 — sidecar → .md + persona mirror)', () => {
  const record = (name) => ({ name, description: `${name} desc`, prompt: `${name} prompt.`, tools: ['read'] });

  test('writes every record, mirrors personas, and marks the sidecar done', async () => {
    const written = [];
    const personas = new Map();
    let done = false;
    const result = await migrateSidecarAgents({
      loadRecords: () => [record('one'), record('two')],
      agentExists: async () => false,
      writeAgent: async (r) => {
        written.push(r.name);
      },
      personaExists: (name) => personas.has(name),
      mirrorPersona: (r) => personas.set(r.name, { name: r.name, systemPrompt: r.prompt }),
      markDone: () => { done = true; },
    });
    expect(result).toEqual({ migrated: 2, skipped: 0 });
    expect(written).toEqual(['one', 'two']);
    expect([...personas.keys()]).toEqual(['one', 'two']);
    expect(done).toBe(true);
  });

  test('a name already present in discovery skips the write but still mirrors the persona', async () => {
    const written = [];
    const personas = new Map();
    const result = await migrateSidecarAgents({
      loadRecords: () => [record('exists')],
      agentExists: async (name) => name === 'exists',
      writeAgent: async (r) => {
        written.push(r.name);
      },
      personaExists: () => false,
      mirrorPersona: (r) => personas.set(r.name, r),
      markDone: () => {},
    });
    expect(result).toEqual({ migrated: 1, skipped: 1 });
    expect(written).toEqual([]);
    expect(personas.has('exists')).toBe(true);
  });

  test('an existing persona is never clobbered by the mirror', async () => {
    const mirrored = [];
    await migrateSidecarAgents({
      loadRecords: () => [record('kept')],
      agentExists: async () => false,
      writeAgent: async () => {},
      personaExists: () => true,
      mirrorPersona: (r) => mirrored.push(r.name),
      markDone: () => {},
    });
    expect(mirrored).toEqual([]);
  });

  test('a write failure keeps the sidecar (no markDone) for an idempotent retry', async () => {
    let done = false;
    const result = await migrateSidecarAgents({
      loadRecords: () => [record('fine'), record('boom')],
      agentExists: async () => false,
      writeAgent: async (r) => {
        if (r.name === 'boom') throw new Error('disk full');
      },
      personaExists: () => false,
      mirrorPersona: () => {},
      markDone: () => { done = true; },
      log: () => {},
    });
    expect(result).toEqual({ migrated: 1, skipped: 0, failed: 'boom' });
    expect(done).toBe(false);
  });
});


/** PersonaHandlers as invoked below: `list()` passes no request (route context is optional). */
type LoosePersonaHandlers = {
  [K in keyof PersonaHandlers]: (request?: Request, ctx?: ModesRouteContext) => Response | Promise<Response>;
};

/** personaHandlers harness product: the sidecar store plus the loose route handlers. */
interface PersonaHarness {
  store: PersonaStore;
  handlers: LoosePersonaHandlers;
}

const personaHandlers = (): PersonaHarness => {
  const file = path.join(tmpRoot, 'personas.json');
  const store = jsonFileStore(file, 'personas');
  return { store, handlers: createPersonaHandlers({ store, allowedTools: new Set(['read', 'bash']) }) };
};

describe('personas (spec 02 §5.2a)', () => {
  test('default none: empty list; CRUD round-trip', async () => {
    const { handlers } = personaHandlers();
    expect(await (await handlers.list()).json()).toEqual({ personas: [] });

    const created = await handlers.create(
      post('http://x/omp/personas', { persona: { name: 'grumpy', description: 'd', systemPrompt: 'Be grumpy.', tools: ['read'] } }),
      ctxFor('http://x'),
    );
    expect(created.status).toBe(201);
    expect(await created.json()).toEqual({
      name: 'grumpy', description: 'd', systemPrompt: 'Be grumpy.', tools: ['read'],
    });

    const one = await handlers.get(null, { params: { name: 'grumpy' } });
    expect(await one.json()).toMatchObject({ name: 'grumpy' });

    const updated = await handlers.update(
      put('http://x/omp/personas/grumpy', { persona: { systemPrompt: 'Be cheerful.' } }),
      { params: { name: 'grumpy' }, url: new URL('http://x'), headers: new Headers() },
    );
    expect(await updated.json()).toMatchObject({ systemPrompt: 'Be cheerful.' });

    let dup = null;
    try {
      await handlers.create(post('http://x', { persona: { name: 'grumpy' } }), ctxFor('http://x'));
    } catch (error) {
      dup = error;
    }
    expect(dup?.status).toBe(409);

    expect((await handlers.remove(null, { params: { name: 'grumpy' } })).status).toBe(204);
    expect(await (await handlers.list()).json()).toEqual({ personas: [] });
  });

  test('persona tools are validated against the allowlist', async () => {
    const { handlers } = personaHandlers();
    let caught = null;
    try {
      await handlers.create(post('http://x', { persona: { name: 'p1', tools: ['nope'] } }), ctxFor('http://x'));
    } catch (error) {
      caught = error;
    }
    expect(caught?.body.error).toBe('unknown-tools');
  });
});

describe('personaFor — materialize overlay contract (02 §5.1 D-B2, §6.1 migration)', () => {
  const personas: OmpPersona[] = [
    { name: 'reviewer-persona', systemPrompt: 'Only review.', tools: ['read'] },
    { name: 'bare' },
  ];

  test('unset / build / plan meta → standard session, no overlay', () => {
    expect(personaFor({}, personas)).toEqual({ status: 'standard', persona: null });
    expect(personaFor(null, personas)).toEqual({ status: 'standard', persona: null });
    expect(personaFor({ agent: 'build' }, personas)).toEqual({ status: 'standard', persona: null });
    expect(personaFor({ agent: 'plan' }, personas)).toEqual({ status: 'standard', persona: null });
  });

  test('known persona → active overlay with systemPrompt/tools for materialize', () => {
    expect(personaFor({ persona: 'reviewer-persona' }, personas)).toEqual({
      status: 'active',
      persona: { name: 'reviewer-persona', systemPrompt: 'Only review.', tools: ['read'] },
    });
    expect(personaFor({ persona: 'bare' }, personas)).toEqual({
      status: 'active',
      persona: { name: 'bare' },
    });
  });

  test('unknown name → missing (degrade to standard + notice), legacy meta.agent participates', () => {
    expect(personaFor({ persona: 'ghost' }, personas)).toEqual({ status: 'missing', name: 'ghost', persona: null });
    expect(personaFor({ agent: 'ghost' }, personas)).toEqual({ status: 'missing', name: 'ghost', persona: null });
  });

  test('accepts Map stores as well as arrays', () => {
    const map = new Map<string, OmpPersona>([['m', { name: 'm', systemPrompt: 's' }]]);
    expect(personaFor({ persona: 'm' }, map)).toMatchObject({ status: 'active' });
  });
});

// ---------------------------------------------------------------------------
// 5. Plan review bridge
// ---------------------------------------------------------------------------

const PREPARE_RESULT: PreparePlanReviewResult = {
  content: [{ type: 'text', text: 'Plan ready for review.' }],
  details: { planFilePath: 'local://auth-plan.md', title: 'auth', planExists: true },
};

/** Bridge results always settle with a leading text block (domain-modes.ts SUPERSEDED_RESULT). */
interface PlanReviewTextResult extends PlanReviewToolResult {
  content: Array<{ type: string; text: string }>;
}

describe('planReviewBridge (spec 02 §5.5)', () => {
  test('hook publishes omp.plan.review_requested (durable) with the PlanApprovalDetails and holds pending', async () => {
    const events = [];
    const bridge = planReviewBridge({
      publish: (type, payload, opts) => events.push({ type, payload, opts }),
      prepare: async () => PREPARE_RESULT,
    });
    const pending = bridge.hook('auth');
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(events).toEqual([
      { type: 'omp.plan.review_requested', payload: { details: PREPARE_RESULT.details }, opts: { durable: true } },
    ]);
    expect(bridge.snapshot()).toEqual({ planFilePath: 'local://auth-plan.md', review: PREPARE_RESULT.details });
    // Still pending — the turn does not end until a decision (spec 5.5 step 3).
    let settled = false;
    void pending.then(() => { settled = true; });
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(settled).toBe(false);
    bridge.dispose('test');
    expect(await pending).toMatchObject({ content: [{ type: 'text', text: expect.stringContaining('aborted') }] });
  });

  test('decide(refine) settles with the ACP-parity refinement guidance (acp-agent.ts:1721-1723)', async () => {
    const bridge = planReviewBridge({ prepare: async () => PREPARE_RESULT });
    const pending = bridge.hook('auth');
    await tick(); // let prepare resolve and the pending propose register
    const result = bridge.decide({ choice: 'refine', feedback: 'trim step 2' });
    expect(result.dispatched).toBe(false);
    // SAFETY: every bridge settle path emits content[0] as a text block
    // (SUPERSEDED_RESULT and decide()'s literal results in domain-modes.ts).
    const toolResult = (await pending) as PlanReviewTextResult;
    expect(toolResult.content[0].text).toBe(
      'Plan refinement requested. Update the plan file, then write auth to xd://propose again when ready.',
    );
    expect(toolResult.details).toEqual({ choice: 'refine', feedback: 'trim step 2' });
    // Review stays available for GET /plan reopen (TUI /plan-review parity).
    expect(bridge.snapshot().review).toEqual(PREPARE_RESULT.details);
  });

  test('decide(approve-execute) settles approved and reports dispatched', async () => {
    const bridge = planReviewBridge({ prepare: async () => PREPARE_RESULT });
    const pending = bridge.hook('auth');
    await tick();
    const result = bridge.decide({ choice: 'approve-execute', executionRole: 'slow' });
    // SAFETY: every bridge settle path emits content[0] as a text block
    // (decide() settles the approve path with the literal text block above).
    const toolResult = (await pending) as PlanReviewTextResult;
    expect(toolResult.content[0].text).toBe('Plan approved (approve-execute).');
    expect(toolResult.details).toEqual({
      choice: 'approve-execute',
      planFilePath: 'local://auth-plan.md',
      executionRole: 'slow',
    });
  });

  test('a second propose supersedes the first pending review', async () => {
    const bridge = planReviewBridge({ prepare: async () => PREPARE_RESULT });
    const first = bridge.hook('auth');
    const second = bridge.hook('auth-v2');
    await tick();
    expect(await first).toMatchObject({ content: [{ type: 'text', text: expect.stringContaining('superseded') }] });
    bridge.decide({ choice: 'approve-keep' });
    expect(await second).toMatchObject({ content: [{ type: 'text', text: 'Plan approved (approve-keep).' }] });
  });

  test('decide without a pending propose and invalid choices', () => {
    const bridge = planReviewBridge({ prepare: async () => PREPARE_RESULT });
    expect(bridge.decide({ choice: 'refine', feedback: 'x' })).toMatchObject({
      dispatched: false,
      reason: 'no-pending-proposal',
    });
    expect(() => bridge.decide({ choice: 'explode' })).toThrow(ModeDomainError);
  });

  test('hookFor binds to an AgentSession preparePlanForReview (agent-session.ts:933-948)', async () => {
    const session: PlanProposalSession = {
      preparePlanForReview: async (title) => ({
        content: [{ type: 'text', text: 'Plan ready for review.' }],
        details: { planFilePath: `local://${title}-plan.md`, title, planExists: true },
      }),
    };
    const bridge = planReviewBridge({});
    const hook = bridge.hookFor(session);
    const pending = hook('auth');
    await tick();
    bridge.decide({ choice: 'approve-compact' });
    expect((await pending).details.planFilePath).toBe('local://auth-plan.md');
  });
});

// ---------------------------------------------------------------------------
// 6. Domain wiring + routes
// ---------------------------------------------------------------------------

/** mountedDomain options: ModesDomainDeps plus the route registration feature flags. */
type MountedDomainOptions = ModesDomainDeps & ModesRouteRegistrationOptions;

const mountedDomain = ({ features, ...domainOptions }: MountedDomainOptions = {}) => {
  const routes = new Map<string, ModesRouteHandler>();
  const route: ModesRouteMount = (method, pattern, handler) => routes.set(`${method} ${pattern}`, handler);
  const publishFor = (sessionId: string, directory: string): OmpEventPublish => (type, payload, opts) => {
    domainPublishes.push({ sessionId, directory, type, payload, opts });
  };
  const appendFor = (sessionId: string): ModeAppendEntry => (mode, data) => {
    domainAppends.push({ sessionId, mode, data });
    return 'entry';
  };
  const domainPublishes: Array<{ sessionId: string; directory: string; type: string; payload: OmpEventPayload; opts?: { durable?: boolean } }> = [];
  const domainAppends: Array<{ sessionId: string; mode: string; data: ModeChangeData | undefined }> = [];
  const domain = createModesDomain({
    publishFor,
    appendFor,
    agentDefinitions: ((): AgentDefinitionAdapter => {
    // Minimal in-memory discovery: written definitions are re-discovered as
    // plain records (route-level tests assert routing/gating, not parsing).
    const records: DiscoveredAgent[] = [];
    return {
      discover: async () => ({ agents: [...records], projectAgentsDir: null }),
      writeFile: async (filePath, content) => {
        records.push(parseAgent(filePath, content, 'user', 'off'));
      },
      deleteFile: async (filePath) => {
        const index = records.findIndex((record) => record.filePath === filePath);
        if (index < 0) return false;
        records.splice(index, 1);
        return true;
      },
      userAgentsDir: path.join(tmpRoot, 'mounted-user-agents'),
      projectAgentsDirFor: () => path.join(tmpRoot, 'mounted-proj-agents'),
    };
  })(),
    personasStore: jsonFileStore(path.join(tmpRoot, `personas-${Date.now()}-${Math.random()}.json`), 'personas'),
    allowedTools: new Set(['read', 'bash']),
    ...domainOptions,
  });
  registerModesDomainRoutes(route, domain, { features: features ?? { 'modes.v1': true, 'agentDefinitions.v1': true, 'personas.v1': true } });
  const call = async (
    method: string,
    pattern: string,
    { body, params = {}, url = 'http://x' }: { body?: unknown; params?: Record<string, string>; url?: string } = {},
  ) => {
    const handler = routes.get(`${method} ${pattern}`);
    if (!handler) throw new Error(`route not mounted: ${method} ${pattern}`);
    const request = body !== undefined
      ? new Request(url, { method, body: JSON.stringify(body), headers: { 'content-type': 'application/json' } })
      : new Request(url, { method });
    return handler(request, { params, url: new URL(url), headers: new Headers() });
  };
  return { domain, call, domainPublishes, domainAppends };
};

describe('modes domain + route mounting', () => {
  test('gated-off capabilities answer explicit 501s (master R2)', async () => {
    const { call } = mountedDomain({ features: {} });
    const mode = await call('GET', '/omp/sessions/{id}/mode', { url: 'http://x?directory=C:/p' });
    expect(mode.status).toBe(501);
    expect(await mode.json()).toEqual({ error: 'modes.v1-unavailable' });
    const agents = await call('GET', '/omp/agent-definitions');
    expect(agents.status).toBe(501);
    expect(await agents.json()).toEqual({ error: 'agentDefinitions.v1-unavailable' });
    const personas = await call('GET', '/omp/personas');
    expect(await personas.json()).toEqual({ error: 'personas.v1-unavailable' });
  });

  test('GET/POST /omp/sessions/{id}/mode with directory scoping', async () => {
    const { call } = mountedDomain();
    const noDirectory = await call('GET', '/omp/sessions/{id}/mode');
    expect(noDirectory.status).toBe(400);

    const url = 'http://x/omp/sessions/s1/mode?directory=C:/proj';
    const entered = await call('POST', '/omp/sessions/{id}/mode', {
      url, params: { id: 's1' }, body: { mode: 'plan' },
    });
    expect(await entered.json()).toMatchObject({ mode: 'plan', plan: { planFilePath: DEFAULT_PLAN_FILE_PATH } });
    const got = await call('GET', '/omp/sessions/{id}/mode', { url, params: { id: 's1' } });
    expect(await got.json()).toMatchObject({ mode: 'plan' });

    // Separate directory → separate tracker.
    const other = await call('GET', '/omp/sessions/{id}/mode', {
      url: 'http://x/omp/sessions/s1/mode?directory=C:/other', params: { id: 's1' },
    });
    expect(await other.json()).toEqual({ mode: 'none' });
  });

  test('POST mode conflict maps to 409 with the conflict body', async () => {
    const { call } = mountedDomain();
    const url = 'http://x/omp/sessions/s2/mode?directory=C:/p';
    await call('POST', '/omp/sessions/{id}/mode', { url, params: { id: 's2' }, body: { mode: 'goal' } });
    const conflict = await call('POST', '/omp/sessions/{id}/mode', {
      url, params: { id: 's2' }, body: { mode: 'plan' },
    });
    expect(conflict.status).toBe(409);
    expect(await conflict.json()).toEqual({
      error: 'mode-conflict', conflict: 'goal', message: 'Exit goal mode first.',
    });
  });

  test('publishFor/appendFor bindings feed the omp bus and sessionManager per session', async () => {
    const { call, domainPublishes, domainAppends } = mountedDomain();
    await call('POST', '/omp/sessions/{id}/mode', {
      url: 'http://x/omp/sessions/s3/mode?directory=C:/p',
      params: { id: 's3' },
      body: { mode: 'plan' },
    });
    expect(domainPublishes.at(-1)).toMatchObject({
      sessionId: 's3', directory: 'C:/p', type: 'omp.mode.changed',
      payload: { mode: 'plan', data: { planFilePath: DEFAULT_PLAN_FILE_PATH } },
      opts: { durable: true },
    });
    expect(domainAppends.at(-1)).toEqual({ sessionId: 's3', mode: 'plan', data: { planFilePath: DEFAULT_PLAN_FILE_PATH } });
  });

  test('cold-start recovery runs once per session via sessionContextFor', async () => {
    const contexts = new Map<string, ModeSessionContext>([
      ['s4', { mode: 'plan', modeData: { planFilePath: 'local://recovered-plan.md' } }],
    ]);
    const { domain } = mountedDomain({
      sessionContextFor: (sessionId) => contexts.get(sessionId),
    });
    const tracker = domain.trackerFor('s4', 'C:/p');
    expect(tracker.snapshot()).toMatchObject({
      mode: 'plan',
      plan: { planFilePath: 'local://recovered-plan.md', paused: false },
    });
    contexts.set('s4', { mode: 'goal' });
    expect(domain.trackerFor('s4', 'C:/p').snapshot().mode).toBe('plan'); // cached, no re-recovery
  });

  test('GET /plan 404s without plan state; carries review after a propose', async () => {
    const { domain, call } = mountedDomain();
    const url = 'http://x/omp/sessions/s5/plan?directory=C:/p';
    const cold = await call('GET', '/omp/sessions/{id}/plan', { url, params: { id: 's5' } });
    expect(cold.status).toBe(404);

    await call('POST', '/omp/sessions/{id}/mode', {
      url: url.replace('/plan', '/mode'), params: { id: 's5' }, body: { mode: 'plan' },
    });
    const bridge = domain.bridgeFor('s5', 'C:/p');
    bridge.hookFor({ preparePlanForReview: async () => PREPARE_RESULT });
    void bridge.hook('auth'); // stays pending until the review decision below
    await new Promise((resolve) => setTimeout(resolve, 5));
    const plan = await call('GET', '/omp/sessions/{id}/plan', { url, params: { id: 's5' } });
    expect(await plan.json()).toEqual({
      planFilePath: 'local://auth-plan.md',
      review: PREPARE_RESULT.details,
    });
    // The tracker's plan projection sees the review through the onReview wiring.
    expect(domain.trackerFor('s5', 'C:/p').snapshot().plan.review).toEqual(PREPARE_RESULT.details);

    const decision = await call('POST', '/omp/sessions/{id}/plan/review', {
      url: 'http://x/omp/sessions/s5/plan/review?directory=C:/p',
      params: { id: 's5' },
      body: { choice: 'refine', feedback: 'shorter' },
    });
    expect(await decision.json()).toEqual({ dispatched: false, mode: 'plan' });
  });

  test('agent-definitions + personas CRUD through the mounted routes', async () => {
    const { call } = mountedDomain();
    const created = await call('POST', '/omp/agent-definitions', {
      body: { definition: { name: 'via-route', description: 'd', systemPrompt: 'p', tools: ['read'] } },
    });
    expect(created.status).toBe(201);
    const listed = await call('GET', '/omp/agent-definitions');
    expect(await listed.json()).toMatchObject({ agents: [{ name: 'via-route' }] });

    const gated = await call('POST', '/omp/agent-definitions', {
      body: { scope: 'project', definition: { name: 'proj', description: 'd', systemPrompt: 'p' } },
    });
    expect(gated.status).toBe(409);
    expect(await gated.json()).toEqual({
      error: 'project-scope-unavailable',
      message: 'Project-scoped agent definitions require settings.projectScopes.v1; use scope "user".',
    });

    const persona = await call('POST', '/omp/personas', {
      body: { persona: { name: 'route-persona', systemPrompt: 's' } },
    });
    expect(persona.status).toBe(201);
    const personas = await call('GET', '/omp/personas');
    expect(await personas.json()).toMatchObject({ personas: [{ name: 'route-persona' }] });
  });

  test('release disposes the pending review and forgets the session', async () => {
    const { domain } = mountedDomain();
    const bridge = domain.bridgeFor('s6', 'C:/p');
    bridge.hookFor({ preparePlanForReview: async () => PREPARE_RESULT });
    const pending = bridge.hook('auth');
    domain.release('s6', 'C:/p');
    expect(await pending).toMatchObject({ content: [{ type: 'text', text: expect.stringContaining('aborted') }] });
    expect(domain.trackerFor('s6', 'C:/p').snapshot()).toEqual({ mode: 'none' });
  });
});
