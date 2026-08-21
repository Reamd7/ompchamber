import { beforeEach, describe, expect, mock, test } from 'bun:test';

let capabilities: Record<string, boolean> | null = null;
let modelsResult: unknown = { ok: true, data: null };

mock.module('@/contexts/runtimeAPIRegistry', () => ({
  getRegisteredRuntimeAPIs: () => ({
    ompModels: {
      getModels: async () => modelsResult,
    },
  }),
}));

mock.module('@/lib/omp/capabilityGate', () => ({
  // The mock must cover every export other same-process test files import
  // (bun's mock.module is process-global once installed).
  isOmpModelRolesEnabled: () => capabilities?.['modelRoles.v1'] === true,
  isOmpPersonasEnabled: () => capabilities?.['personas.v1'] === true,
  isOmpModesEnabled: () => capabilities?.['modes.v1'] === true,
  isOmpAgentDefinitionsEnabled: () => capabilities?.['agentDefinitions.v1'] === true,
  __resetOmpCapabilityGateForTests: () => {},
  primeOmpCapabilityGate: async () => ({ capabilities: null }),
}));

const { resolveOmpDefaults } = await import('./omp-defaults');

const snapshot = (roles: unknown) => ({
  ok: true,
  data: {
    roles,
    roleMeta: {},
    cycleOrder: [],
    enabledModels: [],
    fallbackChains: {},
    modelRoleStorage: 'global',
    defaultThinkingLevel: 'high',
    legacyDefaults: null,
    schemaVersion: '1.0',
    directory: '/repo',
  },
});

beforeEach(() => {
  capabilities = { 'modelRoles.v1': true, 'personas.v1': true };
  modelsResult = { ok: true, data: null };
});

describe('resolveOmpDefaults (spec 08 §5.1 GAP-01)', () => {
  test('resolves the directory default role with its thinking level', async () => {
    modelsResult = snapshot({ default: { configured: 'prov/main:high', provider: 'prov', id: 'main', thinkingLevel: 'high', source: 'global' } });
    expect(await resolveOmpDefaults('/repo')).toEqual({
      model: { providerID: 'prov', modelID: 'main', thinkingLevel: 'high' },
      modelRolesEnabled: true,
      personasEnabled: true,
    });
  });

  test('an unconfigured default role is a legal follow-the-engine state', async () => {
    modelsResult = snapshot({ default: null });
    expect(await resolveOmpDefaults('/repo')).toEqual({ model: null, modelRolesEnabled: true, personasEnabled: true });
  });

  test('capability off, missing directory, or failed snapshot degrade to legacy', async () => {
    capabilities = { 'modelRoles.v1': false, 'personas.v1': true };
    expect(await resolveOmpDefaults('/repo')).toEqual({ model: null, modelRolesEnabled: false, personasEnabled: true });
    capabilities = { 'modelRoles.v1': true, 'personas.v1': true };
    expect(await resolveOmpDefaults(null)).toEqual({ model: null, modelRolesEnabled: false, personasEnabled: false });
    modelsResult = { ok: false, unavailable: false };
    expect(await resolveOmpDefaults('/repo')).toEqual({ model: null, modelRolesEnabled: false, personasEnabled: true });
  });

  test('GAP-02: personas.v1 stays on even when the models snapshot degrades', async () => {
    // Persona-typed surfaces key off personasEnabled alone — a missing models
    // answer must not resurrect the legacy build-first agent fallback.
    capabilities = { 'modelRoles.v1': false, 'personas.v1': true };
    modelsResult = { ok: false, unavailable: false };
    const defaults = await resolveOmpDefaults('/repo');
    expect(defaults.modelRolesEnabled).toBe(false);
    expect(defaults.personasEnabled).toBe(true);
  });
});
