import { describe, expect, mock, test } from 'bun:test';

/**
 * GAP-F2 / 01 GAP-01: the shared default-selection cascade
 * (resolveDefaultAgentModelSelection) must read the omp model-roles face
 * (roles.default for the directory) when the capability is on, and fall back
 * to the full legacy cascade when it is off.
 */

let rolesEnabled = false;

mock.module('@/lib/omp/capabilityGate', () => ({
  isOmpModelRolesEnabled: () => rolesEnabled,
  primeOmpCapabilityGate: async () => ({ capabilities: null }),
}));

const { resolveDefaultAgentModelSelection } = await import(
  `./useConfigStore.cascade?test=${Date.now()}-${Math.random().toString(36).slice(2)}`
);

const provider = (id: string, modelIds: string[]) => ({
  id,
  models: modelIds.map((modelId) => ({ id: modelId })),
});

const agents = [
  { name: 'build', mode: 'primary', model: { providerID: 'openai', modelID: 'gpt-4.1' } },
] as never[];

const providers = [provider('openai', ['gpt-4.1']), provider('anthropic', ['claude-x'])] as never[];

describe('resolveDefaultAgentModelSelection (06 F2 / 01 GAP-01)', () => {
  test('roles on: omp default role is the only model input', () => {
    rolesEnabled = true;
    const result = resolveDefaultAgentModelSelection({
      agents,
      providers,
      settingsDefaultModel: 'openai/gpt-4.1',
      opencodeDefaultModel: 'openai/gpt-4.1',
      ompDefaultModel: { providerId: 'anthropic', modelId: 'claude-x' },
    } as never);
    expect(result.providerId).toBe('anthropic');
    expect(result.modelId).toBe('claude-x');
    expect(result.agentName).toBe(undefined);
  });

  test('roles on without a configured default role follows the engine (no pin)', () => {
    rolesEnabled = true;
    const result = resolveDefaultAgentModelSelection({
      agents,
      providers,
      settingsDefaultModel: 'openai/gpt-4.1',
      opencodeDefaultModel: 'openai/gpt-4.1',
    } as never);
    expect(result.providerId).toBe(undefined);
    expect(result.modelId).toBe(undefined);
  });

  test('roles on: legacy layers never resurrect, even with an invalid omp model', () => {
    rolesEnabled = true;
    const result = resolveDefaultAgentModelSelection({
      agents,
      providers,
      settingsDefaultModel: 'openai/gpt-4.1',
      ompDefaultModel: { providerId: 'ghost', modelId: 'missing' },
    } as never);
    expect(result.providerId).toBe(undefined);
  });

  test('roles off: legacy cascade applies in full', () => {
    rolesEnabled = false;
    const result = resolveDefaultAgentModelSelection({
      agents,
      providers,
      settingsDefaultModel: 'openai/gpt-4.1',
      opencodeDefaultModel: 'anthropic/claude-x',
    } as never);
    expect(result.providerId).toBe('openai');
    expect(result.modelId).toBe('gpt-4.1');
    expect(result.agentName).toBe('build');
  });
});
