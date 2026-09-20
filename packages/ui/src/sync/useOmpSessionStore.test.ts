import { afterEach, describe, expect, test } from 'bun:test';
import type { OmpEventEnvelope } from '@/lib/api/omp';
import { OMP_COMPACTION_LOADER_TTL_MS, OMP_RETRY_LOADER_TTL_MS, sweepOmpVolatile, useOmpSessionStore } from './useOmpSessionStore';

const envelope = (id: number, type: string, createdAt: number, sessionID: string | undefined, payload: unknown): OmpEventEnvelope => ({
  id,
  type,
  directory: '/repo',
  ...(sessionID ? { sessionID } : {}),
  schemaVersion: '1.0',
  createdAt,
  payload,
});

const reset = (runtimeKey = 'rt-test'): void => {
  useOmpSessionStore.getState().clearAll(runtimeKey);
};

afterEach(() => {
  reset();
});

describe('useOmpSessionStore', () => {
  test('applyEvent creates the directory slice lazily and routes effects', () => {
    reset();
    const effects = useOmpSessionStore.getState().applyEvent(
      'rt-test',
      '/repo',
      envelope(1, 'omp.notice.raised', 100, undefined, { level: 'warning', message: 'careful' }),
    );
    expect(effects).toEqual([{ kind: 'notice', level: 'warning', message: 'careful' }]);
    expect(useOmpSessionStore.getState().directories['/repo']).toBeDefined();
  });

  test('frames from a different runtime are ignored', () => {
    reset('rt-a');
    useOmpSessionStore.getState().applyEvent('rt-b', '/repo', envelope(1, 'omp.usage.turn', 1, 'ses_1', { messageID: 'm1' }));
    expect(useOmpSessionStore.getState().directories['/repo']).toBe(undefined);
  });

  test('settleSession clears volatile loaders and awaiting-async on wire idle', () => {
    reset();
    const store = useOmpSessionStore.getState();
    store.applyEvent('rt-test', '/repo', envelope(1, 'omp.compaction.started', 100, 'ses_1', { reason: 'r' }));
    store.applyEvent('rt-test', '/repo', envelope(2, 'omp.session.settled', 100, 'ses_1', { isTerminal: false }));
    store.applyEvent('rt-test', '/repo', envelope(3, 'omp.mode.changed', 100, 'ses_1', { mode: 'goal' }));
    useOmpSessionStore.getState().settleSession('rt-test', '/repo', 'ses_1');
    const slice = useOmpSessionStore.getState().directories['/repo'];
    expect(slice?.loaders.ses_1).toBe(undefined);
    expect(slice?.awaitingAsync.ses_1).toBe(undefined);
    // Durable state survives the settle.
    expect(slice?.mode.ses_1?.mode).toBe('goal');
  });

  test('clearSession removes every trace of the session', () => {
    reset();
    const store = useOmpSessionStore.getState();
    store.applyEvent('rt-test', '/repo', envelope(1, 'omp.retry.started', 100, 'ses_1', { attempt: 1, supersededMessageID: 'msg_A' }));
    store.applyEvent('rt-test', '/repo', envelope(2, 'omp.usage.turn', 100, 'ses_1', { messageID: 'm1' }));
    useOmpSessionStore.getState().clearSession('rt-test', '/repo', 'ses_1');
    const slice = useOmpSessionStore.getState().directories['/repo'];
    expect(slice?.loaders.ses_1).toBe(undefined);
    expect(slice?.telemetry.ses_1).toBe(undefined);
    // Message-keyed overlay state belongs to the message, not the session —
    // it stays until the directory slice goes away.
    expect(slice?.superseded.msg_A).toBeDefined();
  });

  test('clearDirectory drops the whole slice', () => {
    reset();
    useOmpSessionStore.getState().applyEvent('rt-test', '/repo', envelope(1, 'omp.usage.turn', 100, 'ses_1', { messageID: 'm1' }));
    useOmpSessionStore.getState().clearDirectory('rt-test', '/repo');
    expect(useOmpSessionStore.getState().directories['/repo']).toBe(undefined);
  });

  test('volatile loaders expire via the TTL sweep (deterministic)', () => {
    reset();
    const store = useOmpSessionStore.getState();
    store.applyEvent('rt-test', '/repo', envelope(1, 'omp.retry.started', 1000, 'ses_1', { attempt: 1 }));
    store.applyEvent('rt-test', '/repo', envelope(2, 'omp.compaction.started', 1000, 'ses_2', { reason: 'r' }));
    store.applyEvent('rt-test', '/repo', envelope(3, 'omp.mode.changed', 1000, 'ses_1', { mode: 'goal' }));
    const slice = useOmpSessionStore.getState().directories['/repo']!;
    // Within both TTLs: nothing swept.
    expect(sweepOmpVolatile(slice, 1000 + 60_000)).toBe(false);
    // Retry TTL (5min) passed, compaction TTL (10min) not: retry drops, compaction stays.
    expect(sweepOmpVolatile(slice, 1000 + OMP_RETRY_LOADER_TTL_MS + 1)).toBe(true);
    expect(slice.loaders.ses_1).toBe(undefined);
    expect(slice.loaders.ses_2?.compaction).toBeDefined();
    // Beyond all TTLs: everything volatile clears, durable state survives.
    expect(sweepOmpVolatile(slice, 1000 + OMP_COMPACTION_LOADER_TTL_MS + 1)).toBe(true);
    expect(slice.loaders.ses_2).toBe(undefined);
    expect(slice.mode.ses_1?.mode).toBe('goal');
  });

  test('a consumed no-op frame keeps every map identity', () => {
    reset();
    const store = useOmpSessionStore.getState();
    store.applyEvent('rt-test', '/repo', envelope(1, 'omp.mode.changed', 100, 'ses_1', { mode: 'goal' }));
    const before = useOmpSessionStore.getState().directories['/repo']!;
    // A duplicate/replayed mode frame changes nothing but is still consumed
    // (its id is above a fresh gate, so the mark must advance).
    store.applyEvent('rt-test', '/repo', envelope(2, 'omp.mode.changed', 100, 'ses_1', { mode: 'goal' }));
    const after = useOmpSessionStore.getState().directories['/repo']!;
    expect(after.lastAppliedEventId).toBe(2);
    // Identity, not just equality: subscribers selecting `mode`/`goal`
    // leaves must not re-render for a frame that stored nothing.
    expect(after.mode).toBe(before.mode);
    expect(after.goal).toBe(before.goal);
    expect(after.thinking).toBe(before.thinking);
    expect(after.telemetry).toBe(before.telemetry);
    expect(after.chrome).toBe(before.chrome);
  });

  test('planReview state is isolated per frame, not shared across directories', () => {
    reset();
    const store = useOmpSessionStore.getState();
    const review = { planFilePath: 'local://p.md', title: 'Plan', planExists: true };
    store.applyEvent('rt-test', '/repo-a', envelope(1, 'omp.plan.review_requested', 100, 'ses_1', { details: review }));
    const sliceA = useOmpSessionStore.getState().directories['/repo-a']!;
    expect(sliceA.planReview.ses_1?.details).toEqual(review);
    // A frame for another directory must not leak into the first slice.
    store.applyEvent('rt-test', '/repo-b', envelope(2, 'omp.plan.review_requested', 100, 'ses_2', { details: null }));
    const sliceAAfter = useOmpSessionStore.getState().directories['/repo-a']!;
    expect(sliceAAfter.planReview.ses_1?.details).toEqual(review);
  });
});
