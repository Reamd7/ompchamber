/**
 * omp role/mode composer surfaces — capability-gated contract tests
 * (spec 01 §5.3(1)/§5.5, 02 §5.4, 08 §5.1-5.2; master D6-R2 three-matrix
 * degradation).
 *
 * Covers the acceptance matrix:
 *  - capability ON + healthy snapshot → role slots carry real roles
 *    (cycle order first, hidden roles filtered, thinking carried);
 *  - capability OFF / probe absent / transport failure → legacy behavior
 *    (gate reads false, no roles surface) — picker unchanged;
 *  - mode transitions POST the session-mode endpoint with the exact
 *    path/body/query and surface 409 mode-conflict distinctly;
 *  - the outgoing agent field is suppressed under model roles, and the
 *    composer/config-store wiring follows the gate (source contracts).
 */

import { describe, expect, mock, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------------------
// Module mocks — installed before importing the modules under test.
// `mock.module` replaces a whole module, so unrelated exports are stubbed.
// ---------------------------------------------------------------------------

let fakeCapabilities: Record<string, boolean> | null = { 'modelRoles.v1': true, 'modes.v1': true };
let modelsResult: unknown = { ok: true, data: null };

mock.module('@/contexts/runtimeAPIRegistry', () => ({
  getRegisteredRuntimeAPIs: () => ({
    ompCapabilities: {
      getCapabilities: async () =>
        fakeCapabilities === null ? null : { version: 1, eventSchema: '1.0', features: fakeCapabilities, minUiVersion: '0.0.0' },
    },
    ompModels: {
      getModels: async () => modelsResult,
    },
  }),
}));

mock.module('@/lib/runtime-switch', () => ({
  getRuntimeApiBaseUrl: () => 'http://localhost/',
  getRuntimeKey: () => 'rt-test',
  initializeRuntimeEndpoint: () => undefined,
  switchRuntimeEndpoint: () => undefined,
  subscribeRuntimeEndpointWillChange: () => () => undefined,
  subscribeRuntimeEndpointChanged: () => () => undefined,
}));

const { __resetOmpCapabilityGateForTests, primeOmpCapabilityGate, isOmpModelRolesEnabled, isOmpModesEnabled } =
  await import('@/lib/omp/capabilityGate');
const { buildRoleSlots, resolveSendAgent } = await import('@/hooks/useOmpModelRoles');
const { createOmpModelsAPI, createOmpModesAPI } = await import('@/lib/api/omp');

const HEALTHY_SNAPSHOT = {
  schemaVersion: '1.0',
  directory: '/repo',
  roles: {
    default: { configured: 'prov/main:high', provider: 'prov', id: 'main', thinkingLevel: 'high', source: 'global' },
    smol: { configured: 'prov/fast', provider: 'prov', id: 'fast', source: 'global' },
    vision: null,
  },
  roleMeta: {
    default: { tag: 'DEFAULT', name: 'Default' },
    smol: { tag: 'SMOL', name: 'Fast' },
    vision: { name: 'Vision', hidden: true },
  },
  cycleOrder: ['smol', 'default', 'slow'],
  enabledModels: [],
  fallbackChains: {},
  modelRoleStorage: 'global',
  defaultThinkingLevel: 'high',
  legacyDefaults: null,
};

// ---------------------------------------------------------------------------
// API layer — /api/omp/models and /api/omp/sessions/{id}/mode
// ---------------------------------------------------------------------------

describe('createOmpModelsAPI.getModels', () => {
  test('parses the roles snapshot and passes the directory query', async () => {
    const calls: Array<{ path: string; method: string; query?: Record<string, string> }> = [];
    const api = createOmpModelsAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Record<string, string> }) => {
        calls.push({ path, method: init?.method ?? 'GET', query: init?.query });
        return new Response(JSON.stringify(HEALTHY_SNAPSHOT), { status: 200 });
      }) as unknown as typeof fetch,
    });
    const result = await api.getModels({ directory: '/repo' });
    expect(calls).toEqual([{ path: '/api/omp/models', method: 'GET', query: { directory: '/repo' } }]);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.data.roles.default?.id).toBe('main');
      expect(result.data.roles.default?.thinkingLevel).toBe('high');
      expect(result.data.roles.vision).toBeNull();
      expect(result.data.cycleOrder).toEqual(['smol', 'default', 'slow']);
      expect(result.data.defaultThinkingLevel).toBe('high');
    }
  });

  test('501 (feature off) → unavailable; malformed payload → failure, never empty success', async () => {
    const unavailable = createOmpModelsAPI({
      fetchImpl: (async () => new Response(JSON.stringify({ error: 'feature-off' }), { status: 501 })) as unknown as typeof fetch,
    });
    expect(await unavailable.getModels({ directory: '/repo' })).toEqual({ ok: false, unavailable: true });

    const malformed = createOmpModelsAPI({
      fetchImpl: (async () => new Response(JSON.stringify({ roles: 42 }), { status: 200 })) as unknown as typeof fetch,
    });
    expect(await malformed.getModels({ directory: '/repo' })).toEqual({ ok: false, unavailable: false });
  });

  test('POSTs an explicit session-only model switch with directory scope', async () => {
    const calls: Array<{ path: string; method: string; query?: Record<string, string>; body?: string }> = [];
    const api = createOmpModelsAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Record<string, string> }) => {
        calls.push({ path, method: init?.method ?? 'GET', query: init?.query, body: init?.body as string | undefined });
        return new Response(JSON.stringify({ ok: true, model: 'prov/next' }), { status: 200 });
      }) as unknown as typeof fetch,
    });
    expect(await api.setSessionModel('ses_1', { providerID: 'prov', modelID: 'next' }, { directory: '/repo' }))
      .toEqual({ ok: true, model: 'prov/next' });
    expect(calls).toEqual([{
      path: '/api/omp/sessions/ses_1/model',
      method: 'POST',
      query: { directory: '/repo' },
      body: JSON.stringify({ model: { providerID: 'prov', modelID: 'next' } }),
    }]);
  });
});

describe('createOmpModesAPI.setMode (POST /api/omp/sessions/{id}/mode)', () => {
  test('posts the mode body with the directory query and returns the snapshot', async () => {
    const calls: Array<{ path: string; method: string; body: unknown; query?: Record<string, string> }> = [];
    const api = createOmpModesAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Record<string, string> }) => {
        calls.push({ path, method: init?.method ?? '', body: init?.body, query: init?.query });
        return new Response(JSON.stringify({ mode: 'plan', plan: { planFilePath: 'local://PLAN.md' } }), { status: 200 });
      }) as unknown as typeof fetch,
    });
    const result = await api.setMode('ses_1', 'plan', { directory: '/repo' });
    expect(calls).toEqual([{
      path: '/api/omp/sessions/ses_1/mode',
      method: 'POST',
      body: JSON.stringify({ mode: 'plan' }),
      query: { directory: '/repo' },
    }]);
    expect(result).toEqual({ ok: true, snapshot: { mode: 'plan', plan: { planFilePath: 'local://PLAN.md' } } });
  });

  test('exiting to standard posts mode "none"; 409 surfaces the conflicting mode; 501 degrades as unavailable', async () => {
    const calls: unknown[] = [];
    const api = createOmpModesAPI({
      fetchImpl: (async (_path: string, init?: RequestInit) => {
        calls.push(init?.body);
        return new Response(JSON.stringify({ mode: 'none' }), { status: 200 });
      }) as unknown as typeof fetch,
    });
    expect(await api.setMode('ses_1', 'none', { directory: '/repo' })).toEqual({ ok: true, snapshot: { mode: 'none' } });
    expect(calls).toEqual([JSON.stringify({ mode: 'none' })]);

    const conflicting = createOmpModesAPI({
      fetchImpl: (async () => new Response(JSON.stringify({ error: 'mode-conflict', conflict: 'goal' }), { status: 409 })) as unknown as typeof fetch,
    });
    expect(await conflicting.setMode('ses_1', 'plan', { directory: '/repo' })).toEqual({
      ok: false,
      unavailable: false,
      conflict: 'goal',
    });

    const off = createOmpModesAPI({
      fetchImpl: (async () => new Response(JSON.stringify({ error: 'modes.v1-unavailable' }), { status: 501 })) as unknown as typeof fetch,
    });
    expect(await off.setMode('ses_1', 'plan', { directory: '/repo' })).toEqual({ ok: false, unavailable: true });
  });
});

// ---------------------------------------------------------------------------
// Role slot projection (picker rows) — pure projection over the snapshot
// ---------------------------------------------------------------------------

describe('buildRoleSlots', () => {
  test('cycle order first, hidden roles filtered, model + thinking carried', () => {
    const slots = buildRoleSlots(HEALTHY_SNAPSHOT as never);
    expect(slots.map((slot) => slot.id)).toEqual(['smol', 'default', 'slow']);
    // `source` (global/project) now rides along for the settings roles
    // editor's source badges (batch 3, spec 06 §5.7).
    expect(slots[0]).toEqual({
      id: 'smol',
      name: 'Fast',
      tag: 'SMOL',
      configured: true,
      model: { provider: 'prov', id: 'fast' },
      source: 'global',
    });
    expect(slots[1]?.model).toEqual({ provider: 'prov', id: 'main', thinkingLevel: 'high' });
    // 'slow' appears in cycleOrder but has no assignment → unconfigured slot.
    expect(slots[2]).toEqual({ id: 'slow', name: 'slow', configured: false, model: null });
  });

  test('an empty snapshot yields no slots (picker renders the legacy list)', () => {
    expect(buildRoleSlots({ ...HEALTHY_SNAPSHOT, roles: {}, roleMeta: {}, cycleOrder: [] } as never)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Capability gate — the three-matrix degradation source of truth
// ---------------------------------------------------------------------------

describe('capability gate reads', () => {
  test('unresolved probe reads false; settled features read through; failure degrades to false', async () => {
    __resetOmpCapabilityGateForTests();
    expect(isOmpModelRolesEnabled()).toBe(false);
    expect(isOmpModesEnabled()).toBe(false);

    fakeCapabilities = { 'modelRoles.v1': true, 'modes.v1': true };
    await primeOmpCapabilityGate();
    expect(isOmpModelRolesEnabled()).toBe(true);
    expect(isOmpModesEnabled()).toBe(true);

    __resetOmpCapabilityGateForTests();
    fakeCapabilities = null;
    await primeOmpCapabilityGate();
    expect(isOmpModelRolesEnabled()).toBe(false);
    expect(isOmpModesEnabled()).toBe(false);
  });

  test('a transport failure degrades to false and is cached for the runtime', async () => {
    __resetOmpCapabilityGateForTests();
    const probe = primeOmpCapabilityGate();
    // Simulate the probe losing the registry (offline/relay old bundle).
    fakeCapabilities = { 'modelRoles.v1': true };
    await probe;
    // The first primed probe ran with the pre-set registry answer; reset and
    // drop the registry entirely to exercise the failure branch.
    __resetOmpCapabilityGateForTests();
    const saved = fakeCapabilities;
    fakeCapabilities = undefined as unknown as null;
    mock.module('@/contexts/runtimeAPIRegistry', () => ({ getRegisteredRuntimeAPIs: () => null }));
    await primeOmpCapabilityGate();
    expect(isOmpModelRolesEnabled()).toBe(false);
    fakeCapabilities = saved;
  });
});

// ---------------------------------------------------------------------------
// Outgoing agent field under model roles
// ---------------------------------------------------------------------------

describe('resolveSendAgent (wire agent field under model roles)', () => {
  test('model roles on → no agent field; off → legacy value passes through', () => {
    expect(resolveSendAgent('build', true)).toBe(undefined);
    expect(resolveSendAgent('build', false)).toBe('build');
    expect(resolveSendAgent('', false)).toBe(undefined);
    expect(resolveSendAgent(undefined, false)).toBe(undefined);
  });
});

// ---------------------------------------------------------------------------
// Wiring source contracts (repo precedent: issue-2903 source assertions)
// ---------------------------------------------------------------------------

describe('composer wiring follows the gate', () => {
  test('ChatInput gates every send path and disables the agent cycle under modes', () => {
    const source = readFileSync(join(__dirname, '..', 'ChatInput.tsx'), 'utf8');
    expect(source).toContain('resolveSendAgent(');
    expect(source).toContain('ompFeatureFlags.modelRoles');
    expect(source).toContain('if (ompFeatureFlags.modes) return;');
  });

  test('useConfigStore cascade drops the build fallback under the gate; ModelControls swaps surfaces', () => {
    // The cascade moved to its own module (useConfigStore.cascade.ts) when it
    // gained the omp roles.default input (06 F2 / 01 GAP-01).
    const cascade = readFileSync(join(__dirname, '..', '..', '..', 'stores', 'useConfigStore.cascade.ts'), 'utf8');
    expect(cascade).toContain('const ompModelRoles = isOmpModelRolesEnabled();');
    expect(cascade.includes('let resolvedAgent: Agent | undefined;')).toBe(true);
    expect(cascade.includes('if (!ompModelRoles) {')).toBe(true);

    const controls = readFileSync(join(__dirname, '..', 'ModelControls.tsx'), 'utf8');
    // Capability on → mode selector replaces the agent chip; off → unchanged.
    expect(controls).toContain('ompModelRoles.personasEnabled ? renderPersonaSelector() : renderAgentSelector()');
    // Role slots render only from an authoritative snapshot.
    expect(controls).toContain('{ompModelRoles.modelRolesEnabled ? (');
    // The agent restore path does not resurrect the server-stamped 'build'.
    expect(controls).toContain('!ompModelRoles.modesEnabled && latestLoadedUserChoice.agent');
  });

  test('GAP-06 thinking slot replaces variants; GAP-05 row role-assign; GAP-10 enabledModels filter', () => {
    const controls = readFileSync(join(__dirname, '..', 'ModelControls.tsx'), 'utf8');
    // GAP-06: under roles the variant trigger becomes a thinking-level slot
    // fed by the session model badge + models snapshot.
    expect(controls).toContain("if (ompModelRoles.modelRolesEnabled) {");
    expect(controls).toContain("['inherit', 'off', 'auto', ...entry.thinking.supported]");
    expect(controls).toContain('handleOmpThinkingSelect(level)');
    // GAP-05 tail: per-row role assignment commits through the settings face.
    expect(controls).toContain('handleAssignRole(entry, slot)');
    expect(controls).toContain("value: `${entry.providerID}/${entry.modelID}`");
    // GAP-10: enabledModels patterns restrict both pickers; the excluded
    // current model renders a warning row.
    expect(controls).toContain('ompFilteredProviders as ModelPickerProvider[]');
    expect(controls).toContain('ompCurrentModelExcluded ? (');

    const matcher = readFileSync(join(__dirname, '..', '..', '..', 'lib', 'omp', 'enabledModels.ts'), 'utf8');
    expect(matcher).toContain('export const createEnabledModelsMatcher');
  });
});
