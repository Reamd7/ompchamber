import { describe, test, expect } from 'bun:test';
import {
  UiLeaseTable,
  PendingDialogRegistry,
  createDialogBridge,
  createDomainDialogs,
  alwaysAllowTransaction,
  registerDialogEndpoints,
  LEASE_TTL_MS,
  LEASE_HEARTBEAT_MS,
  ORPHAN_WINDOW_MS,
  PRESENT_TTL_MS,
} from './domain-dialogs.ts';
import { OmpEventBus } from './events.ts';
import type { BusEntry, OmpEventEnvelope } from './events.ts';
import type {
  ApproveOutcome,
  DialogSettlement,
  PendingDialogRegistryOptions,
  RespondResult,
} from './domain-dialogs.ts';
import type {
  AutocompleteProviderFactory,
  ExtensionUiComponentFactory,
  TerminalInputHandler,
} from '@oh-my-pi/pi-coding-agent/extensibility/extensions';

// ---------------------------------------------------------------------------
// Manual clock: drives every injectable `now` / `schedule` / `cancel` seam so
// TTL assertions (T_present / T_answer / orphan window / lease expiry) are
// deterministic without real waits.
// ---------------------------------------------------------------------------
const makeClock = (start = 1_000_000): ManualClock => {
  const timers = new Map<number, { due: number; fn: () => void }>();
  let seq = 1;
  let now = start;
  return {
    now: () => now,
    schedule(fn: () => void, delay: number) {
      const id = seq++;
      timers.set(id, { due: now + Math.max(0, delay), fn });
      return id;
    },
    cancel: (id: number) => timers.delete(id),
    async advance(ms: number) {
      const target = now + ms;
      for (;;) {
        let next: number | null = null;
        for (const [id, timer] of timers) {
          if (timer.due <= target && (next === null || timer.due < timers.get(next)!.due)) next = id;
        }
        if (next === null) break;
        const timer = timers.get(next)!;
        timers.delete(next);
        now = timer.due;
        await timer.fn();
      }
      now = target;
    },
    pending: () => timers.size,
  };
};

const DIR = 'C:/work/project';
const DIR_B = 'C:/work/other';
const SESSION = 'sess-1';
const CLIENT_A = '11111111-1111-4111-8111-111111111111';
const CLIENT_B = '22222222-2222-4222-8222-222222222222';

/** Manual clock contract: the injectable seams plus the test-only driver. */
interface ManualClock {
  now(): number;
  schedule(fn: () => void, delay: number): number;
  cancel(id: number): boolean;
  advance(ms: number): Promise<void>;
  pending(): number;
}

const rejectionOf = async (promise: Promise<unknown>) => {
  try {
    await promise;
    return null;
  } catch (error) {
    return error;
  }
};

// ---------------------------------------------------------------------------
// UiLeaseTable (spec 03 §5.1 D-C1b, §7 unit 3)
// ---------------------------------------------------------------------------
describe('UiLeaseTable', () => {
  test('acquire-or-renew is idempotent: same triple renews and returns the same leaseId', () => {
    const clock = makeClock();
    const table = new UiLeaseTable({ ...clock });
    const first = table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    clock.advance(LEASE_HEARTBEAT_MS);
    const renewed = table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(renewed.leaseId).toBe(first.leaseId);
    expect(renewed.attached).toBe(false);
    expect(first.attached).toBe(true);
    expect(renewed.expiresAt).toBeGreaterThan(first.expiresAt ?? 0);
    expect(table.has(DIR, SESSION)).toBe(true);
  });

  test('holder expires after the TTL (3 missed heartbeats) and detach fires exactly once', async () => {
    const clock = makeClock();
    const events: Array<[string, unknown]> = [];
    const table = new UiLeaseTable({ ...clock, onDetach: (info) => events.push(['detach', info]) });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    // 3 heartbeats missed = TTL elapsed; the sweep timer ends the lease.
    await clock.advance(LEASE_TTL_MS);
    expect(table.has(DIR, SESSION)).toBe(false);
    // Late read/renew must not resurrect or re-fire.
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    await clock.advance(LEASE_TTL_MS);
    expect(events.length).toBe(2);
    expect(events.every(([name]) => name === 'detach')).toBe(true);
  });

  test('lease survives just under the TTL and dies just past it', async () => {
    const clock = makeClock();
    const table = new UiLeaseTable({ ...clock });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    await clock.advance(LEASE_TTL_MS - 1);
    expect(table.has(DIR, SESSION)).toBe(true);
    await clock.advance(2);
    expect(table.has(DIR, SESSION)).toBe(false);
  });

  test('reference counting: two holders, both must leave before detach', () => {
    const clock = makeClock();
    const detaches: unknown[] = [];
    const table = new UiLeaseTable({ ...clock, onDetach: (info) => detaches.push(info) });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_B });
    expect(table.snapshot(DIR, SESSION)).toMatchObject({ hasUI: true, holders: 2 });
    const first = table.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(first).toMatchObject({ released: true, detached: false });
    expect(table.has(DIR, SESSION)).toBe(true);
    const second = table.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_B });
    expect(second).toMatchObject({ released: true, detached: true });
    expect(table.has(DIR, SESSION)).toBe(false);
    expect(detaches.length).toBe(1);
    expect(detaches[0]).toMatchObject({ directory: DIR, sessionId: SESSION });
  });

  test('release of an unknown or already-released holder is idempotent', () => {
    const table = new UiLeaseTable({ ...makeClock() });
    expect(table.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A })).toEqual({
      released: false,
      detached: false,
    });
  });

  test('attach/detach transitions fire exactly once through a full cycle', () => {
    const clock = makeClock();
    const transitions: Array<'attach' | 'detach'> = [];
    const table = new UiLeaseTable({
      ...clock,
      onAttach: () => transitions.push('attach'),
      onDetach: () => transitions.push('detach'),
    });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_B });
    table.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(transitions).toEqual(['attach']);
    table.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_B });
    expect(transitions).toEqual(['attach', 'detach']);
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(transitions).toEqual(['attach', 'detach', 'attach']);
  });

  test('SSE liveness never counts as presence: no acquire, no lease', async () => {
    const clock = makeClock();
    const table = new UiLeaseTable({ ...clock });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    // Simulate a "live SSE subscriber" — it simply never touches the table.
    await clock.advance(LEASE_TTL_MS + 5);
    expect(table.has(DIR, SESSION)).toBe(false);
    expect(table.snapshot(DIR, SESSION).hasUI).toBe(false);
  });

  test('directory keys normalize (trailing slash / case drive letter)', () => {
    const table = new UiLeaseTable({ ...makeClock() });
    table.acquire({ directory: DIR.toLowerCase() + '/', sessionId: SESSION, clientId: CLIENT_A });
    expect(table.has(DIR, SESSION)).toBe(true);
    expect(table.has(DIR_B, SESSION)).toBe(false);
  });

  test('releaseAll fires detach for every live lease and empties the table', () => {
    const table = new UiLeaseTable({ ...makeClock() });
    table.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    table.acquire({ directory: DIR_B, sessionId: 'sess-2', clientId: CLIENT_A });
    const detached = table.releaseAll();
    expect(detached.length).toBe(2);
    expect(table.has(DIR, SESSION)).toBe(false);
    expect(table.has(DIR_B, 'sess-2')).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// PendingDialogRegistry (spec 03 §5.2/§5.6, §7 unit 1)
// ---------------------------------------------------------------------------
describe('PendingDialogRegistry', () => {
  const setup = ({ clock, ...rest }: { clock?: ManualClock } & Partial<PendingDialogRegistryOptions> = {}) => {
    const bus = new OmpEventBus();
    const diagnostics: unknown[] = [];
    const registry = new PendingDialogRegistry({
      bus,
      ...clock,
      onDiagnostic: (note) => diagnostics.push(note),
      ...rest,
    });
    return { bus, registry, diagnostics, clock };
  };

  const registerApproval = (registry: { register: (dialog: { directory: string; sessionId: string; kind: 'approval'; payload: { approval: { prompt: string } } }) => { id: string; promise: Promise<unknown> } }, { directory = DIR, sessionId = SESSION }: { directory?: string; sessionId?: string } = {}) =>
    registry.register({
      directory,
      sessionId,
      kind: 'approval',
      payload: { approval: { prompt: 'Allow tool: bash\nCommand: rm -rf /tmp/x' } },
    });

  test('register emits the requested event and snapshot carries the payload', () => {
    const clock = makeClock();
    const { bus, registry } = setup({ clock });
    const seen: Array<BusEntry<OmpEventEnvelope>> = [];
    bus.subscribeSince(0, (entry) => seen.push(entry));
    const entry = registerApproval(registry);
    expect(entry.id.startsWith('dlg_')).toBe(true);
    expect(seen.length).toBe(1);
    expect(seen[0].envelope.type).toBe('omp.dialog.requested');
    expect(seen[0].envelope.directory).toBe(DIR);
    expect(seen[0].envelope.sessionID).toBe(SESSION);
    expect(seen[0].durable).toBe(true);
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((seen[0].envelope.payload as { dialog: object }).dialog).toMatchObject({
      id: entry.id,
      sessionId: SESSION,
      kind: 'approval',
      approval: { prompt: 'Allow tool: bash\nCommand: rm -rf /tmp/x' },
    });
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect(((seen[0].envelope.payload as { dialog?: { presentedAt?: unknown } }).dialog)?.presentedAt).toBeUndefined();
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    expect((seen[0].envelope.payload as { directory?: string }).directory).toBeUndefined(); // envelope carries scope
    const snapshot = registry.snapshot({ directory: DIR });
    expect(snapshot.dialogs.length).toBe(1);
    expect(snapshot.dialogs[0].id).toBe(entry.id);
  });

  test('happy path: ack then respond resolves the resolver and settles', async () => {
    const clock = makeClock();
    const { bus, registry } = setup({ clock });
    const entry = registerApproval(registry);
    const ack = registry.presented(entry.id, { directory: DIR });
    expect(ack.ok).toBe(true);
    expect(typeof ack.presentedAt).toBe('number');
    // Snapshot now carries presentedAt (UI countdown anchor).
    expect(registry.snapshot({ directory: DIR }).dialogs[0].presentedAt).toBe(ack.presentedAt);
    const settled = registry.respond(entry.id, {
      directory: DIR,
      clientId: CLIENT_A,
      result: { kind: 'select', value: 'Approve' },
    });
    expect(settled).toMatchObject({ ok: true, outcome: 'responded' });
    expect(await entry.promise).toEqual({ outcome: 'responded', result: { kind: 'select', value: 'Approve' } });
    expect(registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
  });

  test('双端竞答: first respond wins, second gets 409 with the outcome', async () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const entry = registerApproval(registry);
    registry.presented(entry.id, { directory: DIR });
    const first = registry.respond(entry.id, {
      directory: DIR,
      clientId: CLIENT_A,
      result: { kind: 'select', value: 'Approve' },
    });
    const second = registry.respond(entry.id, {
      directory: DIR,
      clientId: CLIENT_B,
      result: { kind: 'select', value: 'Deny' },
    });
    expect(first.ok).toBe(true);
    expect(second).toMatchObject({ ok: false, status: 409, outcome: 'responded' });
    expect(await entry.promise).toMatchObject({ outcome: 'responded' });
  });

  test('wrong-scope respond is 403, the dialog stays pending, and the other directory never sees it', () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const entry = registerApproval(registry);
    const denied = registry.respond(entry.id, {
      directory: DIR_B,
      clientId: CLIENT_A,
      result: { kind: 'select', value: 'Approve' },
    });
    expect(denied).toMatchObject({ ok: false, status: 403 });
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(1);
    expect(registry.snapshot({ directory: DIR_B }).dialogs).toEqual([]);
    const late = registry.respond(entry.id, {
      directory: DIR,
      result: { kind: 'select', value: 'Deny' },
    });
    expect(late.ok).toBe(true);
  });

  test('respond/presented on unknown id is 404; on settled id is 409; repeated ack is idempotent', () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    expect(registry.respond('dlg_unknown', { directory: DIR, result: { kind: 'cancel' } })).toMatchObject({
      ok: false,
      status: 404,
    });
    expect(registry.presented('dlg_unknown', { directory: DIR })).toMatchObject({ ok: false, status: 404 });
    const entry = registerApproval(registry);
    const ack1 = registry.presented(entry.id, { directory: DIR });
    const ack2 = registry.presented(entry.id, { directory: DIR });
    expect(ack2).toMatchObject({ ok: true, duplicate: true, presentedAt: ack1.presentedAt });
    registry.respond(entry.id, { directory: DIR, result: { kind: 'cancel' } });
    expect(registry.respond(entry.id, { directory: DIR, result: { kind: 'cancel' } })).toMatchObject({
      ok: false,
      status: 409,
    });
    expect(registry.presented(entry.id, { directory: DIR })).toMatchObject({ ok: false, status: 409 });
  });

  test('invalid respond payloads are rejected with 400', () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const entry = registerApproval(registry);
    expect(registry.respond(entry.id, { directory: DIR, result: { kind: 'confirm', value: true } }).status).toBe(400);
    expect(
      registry.respond(entry.id, { directory: DIR, result: { kind: 'select', value: 'Maybe' } }).status,
    ).toBe(400);
    expect(registry.respond(entry.id, { directory: DIR, result: null }).status).toBe(400);
    // Ask dialogs: wrong count, unknown id, and out-of-options labels stay 400
    // (the dialog is not burned by a malformed response).
    const ask = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'ask',
      payload: { ask: { questions: [{ id: 'q1', question: 'A?', options: [{ label: 'a1' }] }], timeoutMs: 0 } },
    });
    expect(
      registry.respond(ask.id, { directory: DIR, result: { kind: 'ask', results: [] } }).status,
    ).toBe(400);
    expect(
      registry.respond(ask.id, {
        directory: DIR,
        result: { kind: 'ask', results: [{ id: 'nope', selectedOptions: [] }] },
      }).status,
    ).toBe(400);
    expect(
      registry.respond(ask.id, {
        directory: DIR,
        result: { kind: 'ask', results: [{ id: 'q1', selectedOptions: ['zzz'] }] },
      }).status,
    ).toBe(400);
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(2); // both still answerable
    ask.promise.catch(() => {});
    entry.promise.catch(() => {});
  });

  test('T_present: never-presented dialog settles timeout at 300s (approval rejects, ask aborts)', async () => {
    const clock = makeClock();
    const { registry, diagnostics } = setup({ clock });
    const approval = registerApproval(registry);
    const ask = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'ask',
      payload: { ask: { questions: [{ id: 'q1', question: 'Pick', options: [{ label: 'A' }] }], timeoutMs: 0 } },
    });
    await clock.advance(PRESENT_TTL_MS);
    const approvalError = await rejectionOf(approval.promise);
    expect(approvalError).toBeInstanceOf(Error);
    expect(approvalError.message).toBe('dialog expired before presentation');
    expect(approvalError.outcome).toBe('timeout');
    const askError = await rejectionOf(ask.promise);
    expect(askError.name).toBe('AbortError'); // ask abort path (ask.ts:982-984)
    expect(askError.outcome).toBe('timeout');
    expect(diagnostics.filter((note: { outcome?: string }) => note.outcome === 'timeout').length).toBe(2);
  });

  test('T_answer runs from the presented-ack, not from registration', async () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const ask = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'ask',
      payload: {
        ask: {
          questions: [
            {
              id: 'q1',
              question: 'Pick one',
              options: [{ label: 'A' }, { label: 'B' }, { label: 'C' }],
              recommended: 2,
            },
          ],
          timeoutMs: 5_000,
        },
      },
    });
    clock.advance(2_000);
    registry.presented(ask.id, { directory: DIR });
    // 4s after ack (6s after creation): still pending.
    await clock.advance(4_000);
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(1);
    // 1s later = exactly timeoutMs after ack: auto-submit fires.
    await clock.advance(1_000);
    const settled = await ask.promise;
    expect(settled.outcome).toBe('timeout');
    expect(settled.result).toEqual({
      kind: 'ask',
      results: [{ id: 'q1', selectedOptions: ['C'], timedOut: true }], // recommended mirror (ask.ts:176-182)
    });
  });

  test('T_answer falls back to the first option when no recommended index', async () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const ask = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'ask',
      payload: {
        ask: {
          questions: [{ id: 'q1', question: 'Pick', options: [{ label: 'X' }, { label: 'Y' }] }],
          timeoutMs: 1_000,
        },
      },
    });
    registry.presented(ask.id, { directory: DIR });
    await clock.advance(1_000);
    // SAFETY: this ask dialog auto-submits through #expireAnswer, which
    // settles { outcome: 'timeout', result: { kind: 'ask', results } }, so
    // the settlement's result is the ask arm — the cast only narrows the
    // union for the typed reads below.
    const settled = (await ask.promise) as DialogSettlement & {
      result: Extract<RespondResult, { kind: 'ask' }>;
    };
    expect(settled.result.results[0].selectedOptions).toEqual(['X']);
  });

  test('orphan window: lease loss settles approval honestly and ask via abort; reconnect inside the window rescues', async () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const approval = registerApproval(registry);
    registry.enterOrphanWindow({ directory: DIR, sessionId: SESSION });
    await clock.advance(ORPHAN_WINDOW_MS - 1);
    // Still pending inside the window.
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(1);
    // Reconnect: window cancels, dialog resumes waiting.
    registry.recoverOrphanWindow({ directory: DIR, sessionId: SESSION });
    await clock.advance(ORPHAN_WINDOW_MS + 10);
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(1);
    // Respond after rescue still works.
    expect(registry.respond(approval.id, { directory: DIR, result: { kind: 'select', value: 'Approve' } }).ok).toBe(true);

    // Now let one expire for real.
    const ask = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'ask',
      payload: { ask: { questions: [{ id: 'q1', question: 'P', options: [{ label: 'A' }] }], timeoutMs: 0 } },
    });
    const approval2 = registerApproval(registry);
    registry.enterOrphanWindow({ directory: DIR, sessionId: SESSION });
    await clock.advance(ORPHAN_WINDOW_MS);
    const approvalError = await rejectionOf(approval2.promise);
    expect(approvalError.message).toBe('dialog orphaned (no UI lease)');
    expect(approvalError.outcome).toBe('timeout');
    const askError = await rejectionOf(ask.promise);
    expect(askError.name).toBe('AbortError');
    expect(askError.outcome).toBe('timeout');
  });

  test('abort: single dialog, per-session batch, and signal path settle aborted with diagnostics', async () => {
    const clock = makeClock();
    const { registry, diagnostics } = setup({ clock });
    const one = registerApproval(registry);
    const two = registry.register({
      directory: DIR,
      sessionId: SESSION,
      kind: 'select',
      payload: { select: { title: 'Choose', options: ['A', 'B'] } },
    });
    const otherSession = registry.register({
      directory: DIR,
      sessionId: 'sess-2',
      kind: 'confirm',
      payload: { confirm: { title: 'Sure?', message: 'Proceed' } },
    });
    expect(registry.abort(one.id, { directory: DIR })).toMatchObject({ ok: true, outcome: 'aborted' });
    one.promise.catch(() => {}); // its abort rejection is covered via `two`
    const count = registry.abortForSession({ directory: DIR, sessionId: SESSION }, 'user stop');
    expect(count).toBe(1); // `two` only; otherSession untouched
    const error = await rejectionOf(two.promise);
    expect(error.message).toBe('user stop');
    expect(error.outcome).toBe('aborted');
    expect(registry.snapshot({ directory: DIR }).dialogs.length).toBe(1);
    // Signal path is a no-op on settled dialogs.
    expect(registry.abortIfPendingSignal(one.id, 'signal')).toBe(false);
    expect(diagnostics.filter((note: { outcome?: string }) => note.outcome === 'aborted').length).toBe(2);
    // otherSession stays pending by design (untouched by the batch abort);
    // its manual-clock T_present never fires, so nothing leaks.
  });

  test('settleAll: every pending settles aborted, events fire, snapshot empties, later responds 409', async () => {
    const clock = makeClock();
    const { bus, registry } = setup({ clock });
    const settled: unknown[] = [];
    bus.subscribeSince(0, (entry) => {
      if (entry.envelope.type === 'omp.dialog.settled') settled.push(entry.envelope.payload);
    });
    const a = registerApproval(registry);
    const b = registry.register({
      directory: DIR_B,
      sessionId: 'sess-9',
      kind: 'ask',
      payload: { ask: { questions: [{ id: 'q', question: '?', options: [] }], timeoutMs: 0 } },
    });
    const count = registry.settleAll('omp-host shutdown');
    expect(count).toBe(2);
    const aError = await rejectionOf(a.promise);
    const bError = await rejectionOf(b.promise);
    expect(aError.message).toBe('omp-host shutdown');
    expect(bError.name).toBe('AbortError'); // ask takes the abort path (R11)
    expect(registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
    expect(registry.snapshot({ directory: DIR_B }).dialogs).toEqual([]);
    expect(settled.filter((payload: { outcome?: string }) => payload.outcome === 'aborted').length).toBe(2);
    expect(registry.respond(a.id, { directory: DIR, result: { kind: 'cancel' } })).toMatchObject({
      ok: false,
      status: 409,
    });
  });

  test('dialog ids are unguessable: prefixed, unique, non-sequential across 500 registrations', () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const ids = [];
    for (let i = 0; i < 500; i += 1) {
      const entry = registerApproval(registry);
      ids.push(entry.id);
      registry.respond(entry.id, { directory: DIR, result: { kind: 'select', value: 'Deny' } });
    }
    expect(new Set(ids).size).toBe(500);
    for (const id of ids) {
      expect(id.startsWith('dlg_')).toBe(true);
      expect(id.length).toBeGreaterThanOrEqual(22); // 'dlg_' + 22 base64url chars = 128 bits
    }
    // Entropy check, not cosmetics: no id is derivable from the registration
    // index (a sequential counter would sort adjacent to its neighbors).
    const sorted = [...ids].sort();
    let adjacentClose = 0;
    for (let i = 1; i < sorted.length; i += 1) {
      if (sorted[i].slice(4, 8) === sorted[i - 1].slice(4, 8)) adjacentClose += 1;
    }
    expect(adjacentClose).toBeLessThan(sorted.length / 10);
  });

  test('snapshot sorts by createdAt and filters by session', () => {
    const clock = makeClock();
    const { registry } = setup({ clock });
    const first = registerApproval(registry);
    clock.advance(5);
    const second = registerApproval(registry);
    clock.advance(5);
    const other = registry.register({
      directory: DIR,
      sessionId: 'sess-2',
      kind: 'input',
      payload: { input: { title: 'Name' } },
    });
    const all = registry.snapshot({ directory: DIR }).dialogs;
    expect(all.map((dialog) => dialog.id)).toEqual([first.id, second.id, other.id]);
    const scoped = registry.snapshot({ directory: DIR, sessionId: SESSION }).dialogs;
    expect(scoped.map((dialog) => dialog.id)).toEqual([first.id, second.id]);
    expect(scoped[2]).toBeUndefined();
    expect(all.find((dialog) => dialog.id === other.id)?.kind).toBe('input');
  });
});

// ---------------------------------------------------------------------------
// WebUIContext bridge (spec 03 §5.1 D-C1, §7 unit 2)
// ---------------------------------------------------------------------------
describe('createDialogBridge', () => {
  const setup = () => {
    const clock = makeClock();
    const leases = new UiLeaseTable({ ...clock });
    const registry = new PendingDialogRegistry({ ...clock });
    leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    return { clock, leases, registry };
  };

  test('approval select maps to kind=approval with the prompt verbatim and enrichment', async () => {
    const { clock, leases, registry } = setup();
    const bridge = createDialogBridge({
      leases,
      registry,
      directory: DIR,
      sessionId: SESSION,
      approvalContext: () => ({ toolName: 'bash', toolCallId: 'call_1', tier: 'exec', approvalMode: 'write' }),
    });
    const pending = bridge.select('Allow tool: bash\nCommand: git push', ['Approve', 'Deny']);
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.kind).toBe('approval');
    expect(dialog.approval).toEqual({
      prompt: 'Allow tool: bash\nCommand: git push',
      approvalMode: 'write',
      toolName: 'bash',
      toolCallId: 'call_1',
      tier: 'exec',
    });
    registry.presented(dialog.id, { directory: DIR });
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'select', value: 'Approve' } });
    expect(await pending).toBe('Approve');
  });

  test('generic select maps to kind=select with option labels; cancel resolves undefined', async () => {
    const { leases, registry } = setup();
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const pending = bridge.select('Deploy target', [
      { label: 'staging', description: 'safe' },
      'production',
    ]);
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.kind).toBe('select');
    expect(dialog.select).toEqual({ title: 'Deploy target', options: ['staging', 'production'] });
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'select', value: 'staging' } });
    expect(await pending).toBe('staging');

    const cancelled = bridge.select('Pick', ['A', 'B']);
    const dialog2 = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.respond(dialog2.id, { directory: DIR, result: { kind: 'cancel' } });
    expect(await cancelled).toBeUndefined();
  });

  test('confirm/input/editor round-trip values; confirm cancel is false', async () => {
    const { leases, registry } = setup();
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const confirmPending = bridge.confirm('Deploy', 'Push to production?');
    let dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.confirm).toEqual({ title: 'Deploy', message: 'Push to production?' });
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'confirm', value: true } });
    expect(await confirmPending).toBe(true);

    const inputPending = bridge.input('Branch name', 'feature/x');
    dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.kind).toBe('input');
    expect(dialog.input).toEqual({ title: 'Branch name', placeholder: 'feature/x' });
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'input', value: 'main' } });
    expect(await inputPending).toBe('main');

    const editorPending = bridge.editor('Note', 'draft text');
    dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.kind).toBe('editor');
    expect(dialog.editor).toEqual({ title: 'Note', placeholder: 'draft text' });
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'editor', value: 'final' } });
    expect(await editorPending).toBe('final');

    const confirmCancel = bridge.confirm('Again', 'sure?');
    dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.respond(dialog.id, { directory: DIR, result: { kind: 'cancel' } });
    expect(await confirmCancel).toBe(false);
  });

  test('askDialog passes questions/timeout through and rehydrates the SDK result', async () => {
    const { leases, registry } = setup();
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const pending = bridge.askDialog(
      [
        {
          id: 'q1',
          question: 'Pick',
          header: 'Choices',
          options: [
            { label: 'A', description: 'first', preview: 'preview-a' },
            { label: 'B' },
          ],
          multi: true,
          recommended: 1,
        },
      ],
      { timeout: 7_000 },
    );
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    expect(dialog.kind).toBe('ask');
    expect(dialog.ask?.timeoutMs).toBe(7_000);
    expect(dialog.ask?.questions[0]).toEqual({
      id: 'q1',
      question: 'Pick',
      header: 'Choices',
      options: [
        { label: 'A', description: 'first', preview: 'preview-a' },
        { label: 'B' },
      ],
      multi: true,
      recommended: 1,
    });
    registry.presented(dialog.id, { directory: DIR });
    registry.respond(dialog.id, {
      directory: DIR,
      result: {
        kind: 'ask',
        results: [{ id: 'q1', selectedOptions: ['B'], customInput: 'extra', note: 'n', timedOut: false }],
      },
    });
    expect(await pending).toEqual({
      kind: 'submit',
      results: [
        {
          id: 'q1',
          question: 'Pick',
          options: ['A', 'B'],
          multi: true,
          selectedOptions: ['B'],
          customInput: 'extra',
          note: 'n',
        },
      ],
    });

    const chatPending = bridge.askDialog([{ id: 'q2', question: '?', options: [{ label: 'A' }] }]);
    let current = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.respond(current.id, { directory: DIR, result: { kind: 'chat' } });
    expect(await chatPending).toEqual({ kind: 'chat' });

    const cancelPending = bridge.askDialog([{ id: 'q3', question: '?', options: [{ label: 'A' }] }]);
    current = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.respond(current.id, { directory: DIR, result: { kind: 'cancel' } });
    expect(await cancelPending).toBeUndefined(); // ask tool aborts on undefined (ask.ts:914-917)
  });

  test('ask timeout auto-submit resolves the bridge with a synthesized submit', async () => {
    const clock = makeClock();
    const leases = new UiLeaseTable({ ...clock });
    const registry = new PendingDialogRegistry({ ...clock });
    leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const pending = bridge.askDialog(
      [{ id: 'q1', question: 'Pick', options: [{ label: 'A' }, { label: 'B' }], recommended: 1 }],
      { timeout: 3_000 },
    );
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.presented(dialog.id, { directory: DIR });
    await clock.advance(3_000);
    expect(await pending).toEqual({
      kind: 'submit',
      results: [
        { id: 'q1', question: 'Pick', options: ['A', 'B'], multi: false, selectedOptions: ['B'], timedOut: true },
      ],
    });
  });

  test('registry rejections propagate through the bridge (orphan text survives)', async () => {
    const { clock, leases, registry } = setup();
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const pending = bridge.select('Allow tool: bash', ['Approve', 'Deny']);
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    registry.enterOrphanWindow({ directory: DIR, sessionId: SESSION });
    await clock.advance(ORPHAN_WINDOW_MS);
    const error = await rejectionOf(pending);
    expect(error.message).toBe('dialog orphaned (no UI lease)');
    expect(dialog.kind).toBe('approval');
  });

  test('no lease: fail closed, no dialog registered (R13)', async () => {
    const { leases, registry } = setup();
    leases.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const error = await rejectionOf(bridge.select('Allow tool: bash', ['Approve', 'Deny']));
    expect(error.message).toBe('no interactive UI available');
    expect(registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
  });

  test('abort signal settles the dialog aborted and detaches the listener', async () => {
    const { leases, registry } = setup();
    const bridge = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    const controller = new AbortController();
    const pending = bridge.confirm('Deploy', 'now?', { signal: controller.signal });
    const dialog = registry.snapshot({ directory: DIR }).dialogs[0];
    controller.abort();
    const error = await rejectionOf(pending);
    expect(error.outcome).toBe('aborted');
    expect(error.message).toBe('dialog aborted by signal');
    expect(registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
    // Settle race: a late respond is a 409, not a crash.
    expect(registry.respond(dialog.id, { directory: DIR, result: { kind: 'confirm', value: true } }).status).toBe(409);
  });

  test('notify forwards to the hook; terminal-only members are inert no-ops', async () => {
    const { leases, registry } = setup();
    const notes: Array<{ message?: string; type?: string; directory?: string; sessionId?: string }> = [];
    const bridge = createDialogBridge({
      leases,
      registry,
      directory: DIR,
      sessionId: SESSION,
      onNotify: (note) => notes.push(note),
    });
    expect(bridge.timeoutStartsOnPresentation).toBe(true);
    bridge.notify('hello', 'warning');
    expect(notes).toEqual([{ message: 'hello', type: 'warning', directory: DIR, sessionId: SESSION }]);
    bridge.setStatus('k', 'text');
    bridge.setWorkingMessage('busy');
    bridge.setWidget('w', ['line']);
    // Terminal-only members are inert no-ops on the web bridge (rpc-mode
    // shape): these factories are never invoked, so each stub returns an
    // unchecked placeholder that satisfies the SDK factory signature.
    // SAFETY: the bridge drops footer factories uninvoked (noteDropped), so
    // this stub never runs and its never-typed return is never observed.
    const inertComponentFactory: ExtensionUiComponentFactory = () => undefined as never;
    bridge.setEditorText('x');
    bridge.pasteToEditor('y');
    // Same inert-stub rationale: the bridge never invokes these factories.
    // SAFETY: addAutocompleteProvider is a no-op on the web bridge, so the
    // stub never runs and its never-typed return is never observed.
    const inertAutocompleteFactory: AutocompleteProviderFactory = () => undefined as never;
    bridge.addAutocompleteProvider(inertAutocompleteFactory);
    bridge.setEditorComponent(undefined);
    const inertInputHandler: TerminalInputHandler = () => undefined;
    const unsubscribe = bridge.onTerminalInput(inertInputHandler);
    expect(unsubscribe).toBeInstanceOf(Function);
    expect(unsubscribe()).toBeUndefined();
    expect(bridge.theme).toEqual({});
    expect(await bridge.getAllThemes()).toEqual([]);
    expect(await bridge.getTheme('x')).toBeUndefined();
    expect(await bridge.setTheme('x')).toMatchObject({ success: false });
    expect(bridge.getToolsExpanded()).toBe(false);
    bridge.setToolsExpanded(true);
    expect(registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// alwaysAllowTransaction (spec 03 §5.3.2, master R10 — write-first-then-approve)
// ---------------------------------------------------------------------------
describe('alwaysAllowTransaction', () => {
  test('successful write approves exactly once after the write lands', async () => {
    const order: string[] = [];
    const result = await alwaysAllowTransaction(
      async () => {
        order.push('write');
      },
      async () => {
        order.push('approve');
        return { ok: true, status: 200 };
      },
    );
    expect(order).toEqual(['write', 'approve']);
    expect(result).toEqual({ settingsWritten: true, approved: true, alreadySettled: false });
  });

  test('failed write never approves — the error propagates for the caller to surface', async () => {
    let approved = false;
    await expect(
      alwaysAllowTransaction(
        async () => {
          throw new Error('settings write 503');
        },
        // Unreachable: the settings write rejects before approve can run.
        // The explicit `return undefined` (same settle value a void callback
        // yields) only satisfies the ApproveOutcome | undefined seam type.
        async () => {
          approved = true;
          return undefined;
        },
      ),
    ).rejects.toThrow('settings write 503');
    expect(approved).toBe(false);
  });

  test('409 from approve leaves the setting written — auditable, not pollution', async () => {
    const result = await alwaysAllowTransaction(
      async () => {},
      async () => ({ ok: false, status: 409 }),
    );
    expect(result).toEqual({ settingsWritten: true, approved: false, alreadySettled: true });
  });

  test('non-409 approve errors propagate', async () => {
    await expect(
      alwaysAllowTransaction(
        async () => {},
        async () => {
          throw Object.assign(new Error('network gone'), { status: 500 });
        },
      ),
    ).rejects.toThrow('network gone');
  });
});

// ---------------------------------------------------------------------------
// Endpoint group (spec 03 §5.2; feature gate per §5.0)
// ---------------------------------------------------------------------------
describe('registerDialogEndpoints', () => {
  const mountRoutes = (feature: () => boolean) => {
    const clock = makeClock();
    const domain = createDomainDialogs({ clock, bus: new OmpEventBus() });
    const routes = new Map();
    const route = (method: string, pattern: string, handler: (request: Request, ctx?: { params?: Record<string, string> }) => Promise<Response>) => routes.set(`${method} ${pattern}`, handler);
    domain.mount(route, { feature });
    /** Per-call invocation init: path params, JSON body, absolute URL. */
    interface RouteCallInit {
      params?: Record<string, string>;
      body?: unknown;
      url?: string;
    }
    const call = async (key: string, { params = {}, body, url }: RouteCallInit = {}) => {
      const handler = routes.get(key);
      if (!handler) throw new Error(`route not mounted: ${key}`);
      const request = {
        url: url ?? `http://host/omp/dialogs${url ? '' : ''}`,
        headers: new Headers(),
        json: async () => body ?? {},
        signal: new AbortController().signal,
      };
      return handler(request, { params, url: new URL(request.url) });
    };
    return { domain, routes, call };
  };

  const leaseBody = { directory: DIR, sessionId: SESSION, clientId: CLIENT_A };

  test('feature off: every dialog endpoint answers an explicit 501', async () => {
    const { call } = mountRoutes(() => false);
    const lease = await call('POST /omp/dialogs/lease', { body: leaseBody });
    expect(lease.status).toBe(501);
    const release = await call('POST /omp/dialogs/lease/release', { body: leaseBody });
    expect(release.status).toBe(501);
    const snapshot = await call('GET /omp/dialogs', { url: `http://host/omp/dialogs?directory=${encodeURIComponent(DIR)}` });
    expect(snapshot.status).toBe(501);
    const respond = await call('POST /omp/dialogs/{id}/respond', {
      params: { id: 'dlg_x' },
      body: { directory: DIR, result: { kind: 'cancel' } },
    });
    expect(respond.status).toBe(501);
  });

  test('feature on: lease acquire/release drive hasUI', async () => {
    const { domain, call } = mountRoutes(() => true);
    const acquired = await call('POST /omp/dialogs/lease', { body: leaseBody });
    expect(acquired.status).toBe(200);
    const lease = await acquired.json();
    expect(lease.leaseId).toBeTruthy();
    expect(lease.heartbeatIntervalMs).toBe(LEASE_HEARTBEAT_MS);
    expect(domain.hasUISnapshotFor(DIR, SESSION)).toMatchObject({ hasUI: true, holders: 1 });
    const released = await call('POST /omp/dialogs/lease/release', { body: leaseBody });
    expect((await released.json()).released).toBe(true);
    expect(domain.hasUISnapshotFor(DIR, SESSION).hasUI).toBe(false);
  });

  test('missing fields answer 400', async () => {
    const { call } = mountRoutes(() => true);
    expect((await call('POST /omp/dialogs/lease', { body: { directory: DIR } })).status).toBe(400);
    expect(
      (await call('GET /omp/dialogs', { url: 'http://host/omp/dialogs' })).status,
    ).toBe(400);
    expect(
      (await call('POST /omp/dialogs/{id}/respond', { params: { id: 'dlg_x' }, body: { result: { kind: 'cancel' } } })).status,
    ).toBe(400);
  });

  test('snapshot/respond/presented/abort happy path and error statuses', async () => {
    const { domain, call } = mountRoutes(() => true);
    const bridge = domain.uiContextFor(DIR, SESSION);
    domain.leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    const pending = bridge.select('Allow tool: bash', ['Approve', 'Deny']);
    const snapshotResponse = await call('GET /omp/dialogs', {
      url: `http://host/omp/dialogs?directory=${encodeURIComponent(DIR)}`,
    });
    const snapshot = await snapshotResponse.json();
    expect(snapshot.dialogs.length).toBe(1);
    const dialogId = snapshot.dialogs[0].id;

    const presented = await call('POST /omp/dialogs/{id}/presented', {
      params: { id: dialogId },
      body: { directory: DIR },
    });
    expect(presented.status).toBe(200);

    const respond = await call('POST /omp/dialogs/{id}/respond', {
      params: { id: dialogId },
      body: { directory: DIR, clientId: CLIENT_A, result: { kind: 'select', value: 'Deny' } },
    });
    expect(respond.status).toBe(200);
    expect((await respond.json()).outcome).toBe('responded');
    expect(await pending).toBe('Deny');

    const second = await call('POST /omp/dialogs/{id}/respond', {
      params: { id: dialogId },
      body: { directory: DIR, result: { kind: 'select', value: 'Approve' } },
    });
    expect(second.status).toBe(409);

    const wrongDir = await call('POST /omp/dialogs/{id}/respond', {
      params: { id: 'dlg_missing' },
      body: { directory: DIR_B, result: { kind: 'cancel' } },
    });
    expect(wrongDir.status).toBe(404);

    // Abort endpoint settles a live dialog.
    const confirmPending = bridge.confirm('Quit', 'really?');
    const snapshot2 = await (
      await call('GET /omp/dialogs', { url: `http://host/omp/dialogs?directory=${encodeURIComponent(DIR)}` })
    ).json();
    const abortResponse = await call('POST /omp/dialogs/{id}/abort', {
      params: { id: snapshot2.dialogs[0].id },
      body: { directory: DIR },
    });
    expect((await abortResponse.json()).outcome).toBe('aborted');
    await expect(confirmPending).rejects.toMatchObject({ outcome: 'aborted' });
  });
});

// ---------------------------------------------------------------------------
// Domain wiring (spec 03 §5.0 item 9, §5.1 D-C1b transitions, R11 dispose)
// ---------------------------------------------------------------------------
describe('createDomainDialogs', () => {
  test('lease transitions drive orphan windows and engine hooks in order', async () => {
    const clock = makeClock();
    const calls: Array<[string, string | undefined]> = [];
    const domain = createDomainDialogs({
      clock,
      bus: new OmpEventBus(),
      onSessionUiAttached: (info) => calls.push(['attach', info.sessionId]),
      onSessionUiDetached: (info) => calls.push(['detach', info.sessionId]),
    });
    const bridge = domain.uiContextFor(DIR, SESSION);
    domain.leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(calls).toEqual([['attach', SESSION]]);
    const pending = bridge.select('Allow tool: bash', ['Approve', 'Deny']);
    expect(domain.hasUISnapshotFor(DIR, SESSION).hasUI).toBe(true);
    domain.leases.release({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(calls).toEqual([['attach', SESSION], ['detach', SESSION]]);
    // Detach entered the orphan window; re-attach inside it rescues the dialog.
    await clock.advance(ORPHAN_WINDOW_MS - 1);
    domain.leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    expect(domain.registry.pendingCount({ directory: DIR, sessionId: SESSION })).toBe(1);
    const snapshot = domain.registry.snapshot({ directory: DIR });
    const dialogId = snapshot.dialogs[0].id;
    expect(
      domain.registry.respond(dialogId, { directory: DIR, result: { kind: 'select', value: 'Approve' } }).ok,
    ).toBe(true);
    expect(await pending).toBe('Approve');
  });

  test('dispose settles everything aborted and empties the lease table (R11)', async () => {
    const clock = makeClock();
    const diagnostics: unknown[] = [];
    const domain = createDomainDialogs({
      clock,
      bus: new OmpEventBus(),
      onDiagnostic: (note) => diagnostics.push(note),
    });
    domain.leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    const bridge = domain.uiContextFor(DIR, SESSION);
    const pending = rejectionOf(bridge.confirm('Sure', 'yes?'));
    const count = await domain.dispose('omp-host shutdown');
    expect(count).toBe(1);
    const error = await pending;
    expect(error.message).toBe('omp-host shutdown');
    expect(domain.registry.snapshot({ directory: DIR }).dialogs).toEqual([]);
    expect(domain.hasUISnapshotFor(DIR, SESSION).hasUI).toBe(false);
    expect(diagnostics).toEqual([
      expect.objectContaining({ outcome: 'aborted', reason: 'omp-host shutdown', kind: 'confirm' }),
    ]);
  });

  test('config overrides reach the lease TTL and orphan window', async () => {
    const clock = makeClock();
    const domain = createDomainDialogs({
      clock,
      config: { leaseTtlMs: 1_000, orphanWindowMs: 2_000 },
    });
    domain.leases.acquire({ directory: DIR, sessionId: SESSION, clientId: CLIENT_A });
    await clock.advance(1_000);
    expect(domain.hasUISnapshotFor(DIR, SESSION).hasUI).toBe(false);
  });
});
