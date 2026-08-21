/**
 * omp dialog lease — the per-session UI attachment lease client (spec 03
 * §5.1 D-C1b / R13; server: UiLeaseTable). One lease per (directory,
 * sessionId) held by the chat view; `hasUI` on the engine is holder-count
 * ≥ 1 and nothing else — this heartbeat is the only thing that keeps an
 * approval/ask-able session interactive.
 *
 * Testability contract (§8.3): the machine exposes `advance(now)` and runs
 * deterministically under an injected clock+scheduler pair. Production wires
 * Date.now/setTimeout/clearTimeout; tests drive time by calling advance().
 * No test may depend on real timers.
 *
 * Discipline:
 * - acquire is idempotent per clientId (server holder map) — heartbeat is a
 *   re-acquire at the server-advised interval.
 * - Failures retry with capped backoff (never give up while started; a
 *   dropped heartbeat must heal, not strand the lease).
 * - release is idempotent, best-effort POST; the TTL expires the holder
 *   server-side even if the POST is lost.
 */

import type { OmpDialogsAPI } from '@/lib/api/omp';

export type OmpDialogLeaseState = 'idle' | 'starting' | 'active' | 'released';

export interface OmpDialogLeaseClock {
  now(): number;
  schedule(fn: () => void, delayMs: number): unknown;
  cancel(handle: unknown): void;
}

export interface OmpDialogLeaseOptions {
  api: Pick<OmpDialogsAPI, 'acquireLease' | 'releaseLease'>;
  directory: string;
  sessionId: string;
  clientId: string;
  clock?: Partial<OmpDialogLeaseClock>;
  /** Fallback until the server answer advises one (LEASE_HEARTBEAT_MS). */
  heartbeatIntervalMs?: number;
  /** Called after every successful acquire/heartbeat. */
  onActive?: () => void | Promise<void>;
}

const DEFAULT_HEARTBEAT_MS = 10_000;
const RETRY_BASE_MS = 500;
const RETRY_CAP_MS = 10_000;

interface ScheduledWork {
  at: number;
  fn: () => void;
  handle?: unknown;
}

export class OmpDialogLease {
  readonly directory: string;
  readonly sessionId: string;
  readonly clientId: string;

  #api: OmpDialogLeaseOptions['api'];
  #now: () => number;
  #schedule: (fn: () => void, delayMs: number) => unknown;
  #cancel: (handle: unknown) => void;
  #heartbeatIntervalMs: number;
  #onActive: (() => void | Promise<void>) | undefined;
  #state: OmpDialogLeaseState = 'idle';
  #work: ScheduledWork[] = [];
  #heartbeatMs: number;

  constructor(options: OmpDialogLeaseOptions) {
    this.#api = options.api;
    this.directory = options.directory;
    this.sessionId = options.sessionId;
    this.clientId = options.clientId;
    this.#now = options.clock?.now ?? Date.now;
    this.#schedule = options.clock?.schedule ?? ((fn: () => void, delayMs: number) => setTimeout(fn, delayMs));
    this.#cancel = options.clock?.cancel
      ?? ((handle: unknown) => clearTimeout(handle as Parameters<typeof clearTimeout>[0]));
    this.#heartbeatMs = options.heartbeatIntervalMs ?? DEFAULT_HEARTBEAT_MS;
    this.#onActive = options.onActive;
    this.#heartbeatIntervalMs = this.#heartbeatMs;
  }

  get state(): OmpDialogLeaseState {
    return this.#state;
  }

  /** Server-advised heartbeat interval once the first acquire answers. */
  get heartbeatIntervalMs(): number {
    return this.#heartbeatIntervalMs;
  }

  /** Starts acquire + heartbeat. Idempotent while not released. */
  start(): void {
    if (this.#state !== 'idle' && this.#state !== 'released') return;
    this.#state = 'starting';
    void this.#acquire();
  }

  /**
   * Deterministic driver: runs every scheduled callback whose deadline has
   * passed at `at` (defaults to the injected clock), earliest first. Tests
   * call this instead of waiting on real timers.
   */
  advance(at: number = this.#now()): void {
    const due = this.#work.filter((entry) => entry.at <= at).sort((a, b) => a.at - b.at);
    for (const entry of due) {
      this.#removeWork(entry);
      if (entry.handle !== undefined) this.#cancel(entry.handle);
      entry.fn();
    }
  }

  /** Stops the heartbeat and releases the holder. Idempotent. */
  release(): void {
    if (this.#state === 'released' || this.#state === 'idle') {
      this.#state = 'released';
      return;
    }
    this.#state = 'released';
    for (const entry of [...this.#work]) {
      this.#removeWork(entry);
      if (entry.handle !== undefined) this.#cancel(entry.handle);
    }
    // Best-effort: the server TTL expires the holder even if this is lost.
    void this.#api.releaseLease({
      directory: this.directory,
      sessionId: this.sessionId,
      clientId: this.clientId,
    }).catch(() => undefined);
  }

  #addWork(fn: () => void, delayMs: number): ScheduledWork {
    const entry: ScheduledWork = { at: this.#now() + delayMs, fn };
    if (delayMs <= 0) {
      fn();
      return entry;
    }
    entry.handle = this.#schedule(() => {
      this.#removeWork(entry);
      entry.fn();
    }, delayMs);
    if (entry.handle !== undefined) this.#work.push(entry);
    return entry;
  }

  #removeWork(entry: ScheduledWork): void {
    const index = this.#work.indexOf(entry);
    if (index >= 0) this.#work.splice(index, 1);
  }

  #armHeartbeat(): void {
    if (this.#isReleased()) return;
    this.#addWork(() => void this.#acquire(), this.#heartbeatIntervalMs);
  }

  #armRetry(attempt: number): void {
    if (this.#isReleased()) return;
    const delay = Math.min(RETRY_CAP_MS, RETRY_BASE_MS * 2 ** Math.min(attempt, 5));
    this.#addWork(() => void this.#acquire(attempt + 1), delay);
  }

  async #acquire(attempt = 0): Promise<void> {
    if (this.#isReleased()) return;
    const outcome = await this.#api.acquireLease({
      directory: this.directory,
      sessionId: this.sessionId,
      clientId: this.clientId,
    });
    if (this.#isReleased()) return;
    if (outcome.ok) {
      this.#heartbeatIntervalMs = outcome.lease.heartbeatIntervalMs > 0
        ? outcome.lease.heartbeatIntervalMs
        : this.#heartbeatMs;
      this.#state = 'active';
      await this.#onActive?.();
      if (!this.#isReleased()) this.#armHeartbeat();
      return;
    }
    if (outcome.unavailable) {
      // dialogs.v1 off / old engine: nothing to hold — stay quiet rather
      // than burn a retry loop against a surface that is not there.
      this.#state = 'released';
      return;
    }
    this.#armRetry(attempt);
  }

  /** Fresh read per call — release() may flip state across awaits, which
   * field narrowing inside one method must not hide. */
  #isReleased(): boolean {
    return this.#state === 'released';
  }
}

/** Stable per-page client identity (server holder key). */
let pageClientId: string | null = null;
export const getOmpDialogClientId = (): string => {
  if (pageClientId === null) {
    const cryptoObj = typeof globalThis.crypto !== 'undefined' ? globalThis.crypto : undefined;
    pageClientId = typeof cryptoObj?.randomUUID === 'function'
      ? cryptoObj.randomUUID()
      : `oc-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  }
  return pageClientId;
};

/** Test seam — a fresh identity per test case. */
export const __resetOmpDialogClientIdForTests = (): void => {
  pageClientId = null;
};
