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

const realSdk = await import('@oh-my-pi/pi-coding-agent');
mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  BUILTIN_TOOLS: { read: class {}, bash: class {}, write: class {}, task: class {} },
}));

const {
  planOptionsFor,
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
  DEFAULT_PLAN_FILE_PATH,
} = await import('./domain-modes.js');

const tmpRoot = mkdtempSync(path.join(tmpdir(), 'omp-domain-modes-'));
afterAll(() => {
  rmSync(tmpRoot, { recursive: true, force: true, maxRetries: 5 });
});

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

const recorder = () => {
  const events = [];
  const appended = [];
  return {
    events,
    appended,
    publish: (type, payload, opts) => events.push({ type, payload, opts }),
    appendEntry: (mode, data) => {
      appended.push({ mode, data });
      return `entry-${appended.length}`;
    },
    modeEvents: () => events.filter((event) => event.type === 'omp.mode.changed'),
  };
};

const post = (url, body) =>
  new Request(url, {
    method: 'POST',
    body: JSON.stringify(body ?? {}),
    headers: { 'content-type': 'application/json' },
  });
const put = (url, body) => new Request(url, { method: 'PUT', body: JSON.stringify(body ?? {}) });
const ctxFor = (url) => ({ params: {}, url: new URL(url), headers: new Headers() });

/** Yield long enough for async bridge prepares to resolve and register pending. */
const tick = () => new Promise((resolve) => setTimeout(resolve, 5));

const STATUS_ACTIVE = { provider: 'anthropic', id: 'claude-sonnet-4' };

// ---------------------------------------------------------------------------
// 1. planOptionsFor — P0 defect fix contract
// ---------------------------------------------------------------------------

describe('planOptionsFor (spec 02 §2.1 P0 defect a)', () => {
  test('emits the verified SDK PlanYolo shape for legacy plan agent with a resolvable target', () => {
    const options = planOptionsFor({ agent: 'plan' }, { target: STATUS_ACTIVE });
    expect(options).toEqual({ planYolo: { target: STATUS_ACTIVE } });
    // The crash shape must never be produced (autoApproveOnResolve matches no
    // SDK field; prewalk.ts:300 dereferences planYolo.target).
    expect('autoApproveOnResolve' in options.planYolo).toBe(false);
  });

  test('passes thinkingLevel through as an optional sibling of target', () => {
    expect(planOptionsFor({ agent: 'plan' }, { target: STATUS_ACTIVE, thinkingLevel: 'high' })).toEqual({
      planYolo: { target: STATUS_ACTIVE, thinkingLevel: 'high' },
    });
    expect(planOptionsFor({ agent: 'plan' }, { target: STATUS_ACTIVE, thinkingLevel: 'auto' })).toEqual({
      planYolo: { target: STATUS_ACTIVE, thinkingLevel: 'auto' },
    });
  });

  test('legacy plan agent without a resolvable target degrades to a standard session', () => {
    expect(planOptionsFor({ agent: 'plan' }, {})).toEqual({});
    expect(planOptionsFor({ agent: 'plan' }, { target: undefined })).toEqual({});
    expect(planOptionsFor({ agent: 'plan' })).toEqual({});
  });

  test('mode-aware plan (modes.v1) never arms constructor planYolo', () => {
    expect(planOptionsFor({ agent: 'plan', mode: 'plan' }, { target: STATUS_ACTIVE })).toEqual({});
    expect(planOptionsFor({ mode: 'plan' }, { target: STATUS_ACTIVE })).toEqual({});
  });

  test('non-plan metas get no plan options', () => {
    expect(planOptionsFor({ agent: 'build' }, { target: STATUS_ACTIVE })).toEqual({});
    expect(planOptionsFor({ agent: 'reviewer' }, { target: STATUS_ACTIVE })).toEqual({});
    expect(planOptionsFor(null, { target: STATUS_ACTIVE })).toEqual({});
    expect(planOptionsFor(undefined, {})).toEqual({});
  });

  test('rejects an invalid thinking level loudly', () => {
    expect(() => planOptionsFor({ agent: 'plan' }, { target: STATUS_ACTIVE, thinkingLevel: 'ultra' })).toThrow(
      ModeDomainError,
    );
  });
});

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
    ]) {
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

const definitionHandlers = (overrides = {}) => {
  const map = new Map();
  const persisted = [];
  const store = mapBackedStore(map, (records) => persisted.push(records));
  return {
    map,
    persisted,
    handlers: createAgentDefinitionHandlers({
      store,
      allowedTools: new Set(['read', 'bash', 'write', 'task', 'mcp__custom']),
      ...overrides,
    }),
  };
};

describe('agent-definitions CRUD (spec 02 §5.2, scoped sidecar contract)', () => {
  test('create → 201 with the scoped record shape; list returns it; delete → 204', async () => {
    const { handlers, map } = definitionHandlers();
    const created = await handlers.create(
      post('http://x/omp/agent-definitions', {
        scope: 'global',
        definition: { name: 'reviewer', prompt: 'Review code.', tools: ['read', 'mcp__custom'], mode: 'subagent' },
      }),
      ctxFor('http://x/omp/agent-definitions'),
    );
    expect(created.status).toBe(201);
    expect(await created.json()).toEqual({
      name: 'reviewer',
      prompt: 'Review code.',
      tools: ['read', 'mcp__custom'],
      mode: 'subagent',
      scope: 'global',
    });
    expect(map.get('reviewer')?.scope).toBe('global');

    const listed = await handlers.list();
    expect(await listed.json()).toEqual({ agents: [map.get('reviewer')] });

    const removed = await handlers.remove(null, { params: { name: 'reviewer' } });
    expect(removed.status).toBe(204);
    expect(map.size).toBe(0);
  });


  test('duplicate create throws 409 agent-definition-exists (route maps it)', async () => {
    const { handlers } = definitionHandlers();
    const body = { definition: { name: 'scout', prompt: 'p' } };
    await handlers.create(post('http://x', body), ctxFor('http://x'));
    let caught = null;
    try {
      await handlers.create(post('http://x', body), ctxFor('http://x'));
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(ModeDomainError);
    expect(caught.status).toBe(409);
    expect(caught.body.error).toBe('agent-definition-exists');

    let missing = null;
    try {
      await handlers.remove(null, { params: { name: 'nope' } });
    } catch (error) {
      missing = error;
    }
    expect(missing?.status).toBe(404);
  });

  test('tools are validated against BUILTIN_TOOLS + registered tools', async () => {
    const { handlers } = definitionHandlers();
    let caught = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a1', prompt: 'p', tools: ['read', 'nonsense'] } }),
        ctxFor('http://x'),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(400);
    expect(caught.body).toEqual({ error: 'unknown-tools', tools: ['nonsense'] });
  });

  test('mode:"primary" is rejected — the deleted build/plan concept (02 §5.3)', async () => {
    const { handlers } = definitionHandlers();
    let caught = null;
    try {
      await handlers.create(
        post('http://x', { definition: { name: 'a2', prompt: 'p', mode: 'primary' } }),
        ctxFor('http://x'),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(400);
    expect(caught.body.error).toBe('invalid-mode');
  });

  test('project scope is gated behind settingsProjectScopes (409 until lit, R2-M4)', async () => {
    const gatedOff = definitionHandlers();
    let caught = null;
    try {
      await gatedOff.handlers.create(
        post('http://x', { scope: 'project', definition: { name: 'a3', prompt: 'p' } }),
        ctxFor('http://x'),
      );
    } catch (error) {
      caught = error;
    }
    expect(caught?.status).toBe(409);
    expect(caught.body.error).toBe('project-scope-unavailable');

    const gatedOn = definitionHandlers({ settingsProjectScopes: true });
    const created = await gatedOn.handlers.create(
      post('http://x', { scope: 'project', definition: { name: 'a3', prompt: 'p' } }),
      ctxFor('http://x'),
    );
    expect(created.status).toBe(201);
    expect((await created.json()).scope).toBe('project');
  });

  test('update patches fields, renames via renameTo, and preserves unknown sidecar fields', async () => {
    const { handlers, map } = definitionHandlers();
    await handlers.create(
      post('http://x', { definition: { name: 'old', prompt: 'p', tools: ['read'], description: 'keep me' } }),
      ctxFor('http://x'),
    );
    const updated = await handlers.update(
      put('http://x/omp/agent-definitions/old', { definition: { prompt: 'new prompt' }, renameTo: 'fresh' }),
      { params: { name: 'old' }, url: new URL('http://x/omp/agent-definitions/old'), headers: new Headers() },
    );
    expect(await updated.json()).toEqual({
      name: 'fresh',
      prompt: 'new prompt',
      tools: ['read'],
      description: 'keep me',
      scope: 'global',
    });
    expect(map.has('old')).toBe(false);
    expect(map.has('fresh')).toBe(true);
  });

  test('jsonFileStore round-trips the engine sidecar shape and tolerates missing/corrupt files', () => {
    const file = path.join(tmpRoot, 'nested', 'openchamber-agents.json');
    const store = jsonFileStore(file, 'agents');
    expect(store.load()).toEqual([]);
    store.save([{ name: 'a', prompt: 'p', tools: [] }]);
    expect(existsSync(file)).toBe(true);
    expect(JSON.parse(readFileSync(file, 'utf8'))).toEqual({ agents: [{ name: 'a', prompt: 'p', tools: [] }] });
    expect(store.load()).toEqual([{ name: 'a', prompt: 'p', tools: [] }]);
    writeFileSync(file, '{corrupt');
    expect(store.load()).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 4. Personas
// ---------------------------------------------------------------------------

const personaHandlers = () => {
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
  const personas = [
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
    const map = new Map([['m', { name: 'm', systemPrompt: 's' }]]);
    expect(personaFor({ persona: 'm' }, map)).toMatchObject({ status: 'active' });
  });
});

// ---------------------------------------------------------------------------
// 5. Plan review bridge
// ---------------------------------------------------------------------------

const PREPARE_RESULT = {
  content: [{ type: 'text', text: 'Plan ready for review.' }],
  details: { planFilePath: 'local://auth-plan.md', title: 'auth', planExists: true },
};

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
    const toolResult = await pending;
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
    expect(result.dispatched).toBe(true);
    const toolResult = await pending;
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
    const session = {
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

const mountedDomain = ({ features, ...domainOptions } = {}) => {
  const routes = new Map();
  const route = (method, pattern, handler) => routes.set(`${method} ${pattern}`, handler);
  const publishFor = (sessionId, directory) => (type, payload, opts) => {
    domainPublishes.push({ sessionId, directory, type, payload, opts });
  };
  const appendFor = (sessionId) => (mode, data) => {
    domainAppends.push({ sessionId, mode, data });
    return 'entry';
  };
  const domainPublishes = [];
  const domainAppends = [];
  const domain = createModesDomain({
    publishFor,
    appendFor,
    agentDefinitionStore: mapBackedStore(new Map()),
    personasStore: jsonFileStore(path.join(tmpRoot, `personas-${Date.now()}-${Math.random()}.json`), 'personas'),
    allowedTools: new Set(['read', 'bash']),
    ...domainOptions,
  });
  registerModesDomainRoutes(route, domain, { features: features ?? { 'modes.v1': true, 'agentDefinitions.v1': true, 'personas.v1': true } });
  const call = async (method, pattern, { body, params = {}, url = 'http://x' } = {}) => {
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
    const contexts = new Map([
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
      body: { definition: { name: 'via-route', prompt: 'p', tools: ['read'] } },
    });
    expect(created.status).toBe(201);
    const listed = await call('GET', '/omp/agent-definitions');
    expect(await listed.json()).toMatchObject({ agents: [{ name: 'via-route' }] });

    const gated = await call('POST', '/omp/agent-definitions', {
      body: { scope: 'project', definition: { name: 'proj', prompt: 'p' } },
    });
    expect(gated.status).toBe(409);
    expect(await gated.json()).toEqual({
      error: 'project-scope-unavailable',
      message: 'Project-scoped agent definitions require settings.projectScopes.v1; use scope "global".',
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
