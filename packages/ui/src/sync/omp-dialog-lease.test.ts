import { describe, expect, test } from 'bun:test';
import type { OmpLeaseInfo } from '@/lib/api/omp';
import { OmpDialogLease } from './omp-dialog-lease';

/**
 * Deterministic clock + scheduler: tests drive time exclusively through
 * `advance()` — no real timers anywhere in this file (§8.3 discipline).
 */
interface Harness {
  readonly lease: OmpDialogLease;
  readonly acquireCalls: () => number;
  readonly releaseCalls: () => number;
  advance: (ms: number) => void;
}

const createHarness = (options?: {
  acquireResults?: Array<'ok' | 'fail' | 'unavailable'>;
  heartbeatIntervalMs?: number;
  onActive?: () => void | Promise<void>;
}): Harness => {
  const acquireResults = options?.acquireResults ?? ['ok'];
  const state = {
    t: 1000,
    acquireCalls: 0,
    releaseCalls: 0,
  };
  const pending: Array<{ at: number; fn: () => void }> = [];
  const view = {
    lease: null as OmpDialogLease | null,
  };
  const harness: Harness = {
    get lease() { return view.lease as OmpDialogLease; },
    acquireCalls: () => state.acquireCalls,
    releaseCalls: () => state.releaseCalls,
    advance: (ms: number) => {
      const target = state.t + ms;
      for (;;) {
        const due = pending.filter((entry) => entry.at <= target).sort((a, b) => a.at - b.at)[0];
        if (due === undefined) break;
        pending.splice(pending.indexOf(due), 1);
        state.t = due.at;
        due.fn();
      }
      state.t = target;
    },
  };
  view.lease = new OmpDialogLease({
    api: {
      acquireLease: async () => {
        const index = Math.min(state.acquireCalls, acquireResults.length - 1);
        const outcome = acquireResults[index];
        state.acquireCalls += 1;
        if (outcome === 'ok') {
          const info: OmpLeaseInfo = {
            leaseId: `lease_${state.acquireCalls}`,
            expiresAt: state.t + 30_000,
            heartbeatIntervalMs: options?.heartbeatIntervalMs ?? 10_000,
          };
          return { ok: true, lease: info };
        }
        if (outcome === 'unavailable') return { ok: false, unavailable: true };
        return { ok: false, unavailable: false };
      },
      releaseLease: async () => {
        state.releaseCalls += 1;
        return { ok: true };
      },
    },
    directory: '/repo',
    sessionId: 'ses_1',
    clientId: 'client_1',
    clock: {
      now: () => state.t,
      schedule: (fn: () => void, delayMs: number) => {
        pending.push({ at: state.t + delayMs, fn });
        return pending.length;
      },
      cancel: () => undefined,
    },
    onActive: options?.onActive,
  });
  return harness;
};

/** Lets one started acquire settle so the next heartbeat can be armed. */
const tick = async (): Promise<void> => {
  await Promise.resolve();
  await Promise.resolve();
};

describe('OmpDialogLease — acquire + heartbeat (advance-driven)', () => {
  test('start acquires once; each heartbeat interval re-acquires idempotently', async () => {
    const h = createHarness();
    h.lease.start();
    await tick();
    expect(h.acquireCalls()).toBe(1);
    expect(h.lease.state).toBe('active');
    h.advance(10_000);
    await tick();
    expect(h.acquireCalls()).toBe(2);
    h.advance(10_000);
    await tick();
    expect(h.acquireCalls()).toBe(3);
    h.advance(10_000);
    await tick();
    expect(h.acquireCalls()).toBe(4);
  });

  test('server-advised heartbeat interval is adopted from the lease answer', async () => {
    const h = createHarness({ heartbeatIntervalMs: 5_000 });
    h.lease.start();
    await tick();
    expect(h.lease.heartbeatIntervalMs).toBe(5_000);
    h.advance(5_000);
    await tick();
    expect(h.acquireCalls()).toBe(2);
  });

  test('runs authoritative reconciliation after acquire and every heartbeat', async () => {
    let reconciles = 0;
    const h = createHarness({ onActive: () => { reconciles += 1; } });
    h.lease.start();
    await tick();
    expect(reconciles).toBe(1);
    h.advance(10_000);
    await tick();
    expect(reconciles).toBe(2);
  });

  test('transport failure retries on capped backoff instead of giving up', async () => {
    const h = createHarness({ acquireResults: ['fail', 'fail', 'ok'] });
    h.lease.start();
    await tick();
    expect(h.lease.state).toBe('starting');
    h.advance(500);
    await tick();
    expect(h.acquireCalls()).toBe(2);
    h.advance(1_000);
    await tick();
    expect(h.acquireCalls()).toBe(3);
    expect(h.lease.state).toBe('active');
  });

  test('unavailable surface (dialogs.v1 off) parks released — no retry loop', async () => {
    const h = createHarness({ acquireResults: ['unavailable'] });
    h.lease.start();
    await tick();
    expect(h.lease.state).toBe('released');
    h.advance(60_000);
    await tick();
    expect(h.acquireCalls()).toBe(1);
  });

  test('release posts once, cancels heartbeats, is idempotent', async () => {
    const h = createHarness();
    h.lease.start();
    await tick();
    h.lease.release();
    h.lease.release();
    h.advance(60_000);
    await tick();
    expect(h.releaseCalls()).toBe(1);
    expect(h.acquireCalls()).toBe(1);
    expect(h.lease.state).toBe('released');
  });
});
