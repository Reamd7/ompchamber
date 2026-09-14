import { describe, expect, mock, test } from 'bun:test';
import type { OmpProcessEntry, OmpProcessOutput, OmpProcessesAPI } from '@/lib/api/omp';

// Seams: the API factory and capability gate are module-level singletons, so
// the test owns them via mock.module (repo test convention).
type ListResult = Awaited<ReturnType<OmpProcessesAPI['list']>>;
let featureEnabled = true;
let listCalls = 0;
let listResult: ListResult = { ok: true, data: { revision: 1, generatedAt: 0, entries: [] } };
let outputResult: OmpProcessOutput = { key: 'k', output: 'out', truncated: false, live: true };
let outputOk = true;
const killCalls: Array<{ sessionID: string; key: string; pid?: number }> = [];
let killOk = true;

const actualCapabilityGate = await import('@/lib/omp/capabilityGate');
mock.module('@/lib/omp/capabilityGate', () => ({
  ...actualCapabilityGate,
  isOmpFeatureEnabled: () => featureEnabled,
}));
const actualRuntimeSwitch = await import('@/lib/runtime-switch');
mock.module('@/lib/runtime-switch', () => ({
  ...actualRuntimeSwitch,
  getRuntimeKey: () => 'local',
}));
// Partial mock: spread the real module so sibling test files sharing the
// process keep every other export (mock.module is process-global in bun).
const actualOmp = await import('@/lib/api/omp');
mock.module('@/lib/api/omp', () => ({
  ...actualOmp,
  createOmpProcessesAPI: () => ({
    list: async () => {
      listCalls += 1;
      return listResult;
    },
    output: async () => (outputOk ? { ok: true, data: outputResult } : { ok: false, unavailable: false }),
    kill: async (options: { sessionID: string; key: string; pid?: number }) => {
      killCalls.push(options);
      return killOk ? { ok: true, data: { ok: true, killed: 1, skipped: [] } } : { ok: false, unavailable: false };
    },
  }),
}));

const { useOmpProcessesStore } = await import('./useOmpProcessesStore');

const reset = (): void => {
  useOmpProcessesStore.setState({ byKey: {}, revisions: {} });
  featureEnabled = true;
  listCalls = 0;
  killCalls.length = 0;
  killOk = true;
  outputOk = true;
};

const entry = (overrides: Partial<OmpProcessEntry> = {}): OmpProcessEntry => ({
  key: 'ses_1 call_1',
  sessionID: 'ses_1',
  kind: 'bash',
  command: 'sleep 600 &',
  status: 'running',
  startedAt: 1,
  attribution: 'window',
  liveCount: 1,
  hasOutput: false,
  processes: [],
  ...overrides,
});

describe('useOmpProcessesStore', () => {
  test('capability off → load is a no-op', async () => {
    reset();
    featureEnabled = false;
    await useOmpProcessesStore.getState().load('/repo');
    expect(listCalls).toBe(0);
    expect(useOmpProcessesStore.getState().entries('/repo')).toBeNull();
  });

  test('load stores snapshot + revision; repeat load dedupes without force', async () => {
    reset();
    listResult = { ok: true, data: { revision: 5, generatedAt: 0, entries: [entry()] } };
    await useOmpProcessesStore.getState().load('/repo');
    expect(listCalls).toBe(1);
    expect(useOmpProcessesStore.getState().entries('/repo')).toHaveLength(1);
    await useOmpProcessesStore.getState().load('/repo');
    expect(listCalls).toBe(1);
    await useOmpProcessesStore.getState().load('/repo', { force: true });
    expect(listCalls).toBe(2);
  });

  test('failed load keeps previous rows and revision', async () => {
    reset();
    listResult = { ok: true, data: { revision: 3, generatedAt: 0, entries: [entry()] } };
    await useOmpProcessesStore.getState().load('/repo');
    listResult = { ok: false, unavailable: false };
    await useOmpProcessesStore.getState().load('/repo', { force: true });
    const rows = useOmpProcessesStore.getState().entries('/repo');
    expect(rows).toHaveLength(1);
    expect(useOmpProcessesStore.getState().revisions['local::/repo']).toBe(3);
  });

  test('kill uses the entry session; unattributed falls back to candidate sessions', async () => {
    reset();
    listResult = { ok: true, data: { revision: 1, generatedAt: 0, entries: [] } };
    const e = entry();
    await useOmpProcessesStore.getState().kill('/repo', e, 4242);
    expect(killCalls[0]).toEqual({ sessionID: 'ses_1', key: 'ses_1 call_1', pid: 4242 });

    const unattributed = entry({ key: 'proc:9', sessionID: null, candidateSessionIds: ['ses_7'] });
    await useOmpProcessesStore.getState().kill('/repo', unattributed);
    expect(killCalls[1]?.sessionID).toBe('ses_7');

    const nobody = entry({ key: 'proc:8', sessionID: null });
    expect(await useOmpProcessesStore.getState().kill('/repo', nobody)).toBeNull();
    expect(killCalls).toHaveLength(2);
  });

  test('output returns parsed payload; failure → null', async () => {
    reset();
    expect(await useOmpProcessesStore.getState().output('/repo', 'k')).toEqual(outputResult);
    outputOk = false;
    expect(await useOmpProcessesStore.getState().output('/repo', 'k')).toBeNull();
  });
});
