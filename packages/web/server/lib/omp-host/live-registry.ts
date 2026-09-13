// LiveSessionRegistry — host-owned lifecycle for live AgentSessions
// (docs/plan.md §3.2-3.4, phase 2).
//
// Guarantees (single host process, canonical transcript path):
// - Every per-session container is keyed by `normalize(directory)\0sessionID`,
//   so the same session id under two directories is two distinct records and
//   can never satisfy each other's lookups.
// - Per-key operation gate: materialize, evict, move, delete and reload
//   serialize per key, so two async callers cannot both pass a check on the
//   same file and act on it concurrently. Inner helpers assume the caller
//   holds the gate; callers must never re-enter withOperation on a key they
//   already hold (the engine's mutation boundaries are the only gate users).
// - Eviction state machine: materializing → live → evicting → (cold | failed).
//   `evicting` and `failed` block new writers for that key; a failed dispose
//   becomes an observable quarantine tombstone instead of a deleted map row.
// - `failed` tombstones retain nothing but metadata; the SDK object a failed
//   disposal could not release is retained by the engine's dispose-promise
//   chain, deliberately, so the same file cannot gain a second writer.
// - Idle TTL bookkeeping uses the injected monotonic clock (default
//   performance.now); wall clock stays out of TTL math (plan §4.2).

import { normalizeDirectoryKey } from './registry.ts';

/** Composite session key: `normalize(directory)\0sessionID` (plan §3.2). */
export const sessionKey = (directory: string, sessionId: string): string =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionId}`;

export type LiveSessionState = 'materializing' | 'live' | 'evicting' | 'failed';

/** Retryable rejection: the key is mid-eviction or quarantined (plan §3.4). */
export class SessionBusyError extends Error {
  readonly code: 'session-evicting' | 'session-failed' | 'host-shutting-down';
  /** Wire `sessionID` when the refusal is session-scoped (host maps 409). */
  readonly sessionId: string | null;
  constructor(code: SessionBusyError['code'], message: string, sessionId: string | null = null) {
    super(message);
    this.name = 'SessionBusyError';
    this.code = code;
    this.sessionId = sessionId;
  }
}

export interface LiveRecord<TPayload> {
  readonly key: string;
  readonly directory: string;
  readonly sessionId: string;
  state: LiveSessionState;
  /** Host payload; nulled when eviction starts (host refs drop first). */
  payload: TPayload | null;
  /** Engine's single disposal promise for this record (plan §3.4 step 4). */
  disposePromise: Promise<'disposed' | 'failed'> | null;
  /** Monotonic (registry clock). Refreshed only by user-visible use. */
  lastUsedAt: number;
  /** Active withLiveSession operations (plan §3.2). */
  inFlight: number;
  readonly createdAt: number;
  /** Set when a disposal failed: quarantine reason for diagnostics. */
  failure: { reason: string; at: number } | null;
}

export interface LiveRegistryStats {
  materializing: number;
  live: number;
  evicting: number;
  failed: number;
  /** Total non-cold records (any state). */
  total: number;
}

export interface LiveSessionRegistryOptions {
  /** Monotonic clock; test-injectable (plan §4.2). */
  now?: () => number;
}

export class LiveSessionRegistry<TPayload> {
  readonly now: () => number;
  #records = new Map<string, LiveRecord<TPayload>>();
  #gates = new Map<string, Promise<unknown>>();

  constructor({ now }: LiveSessionRegistryOptions = {}) {
    this.now = now ?? (() => performance.now());
  }

  /** Record in any state, or null when the key is cold. */
  get(directory: string, sessionId: string): LiveRecord<TPayload> | null {
    return this.#records.get(sessionKey(directory, sessionId)) ?? null;
  }

  byKey(key: string): LiveRecord<TPayload> | null {
    return this.#records.get(key) ?? null;
  }

  /** Only a `live` record with its payload attached. */
  getLive(directory: string, sessionId: string): LiveRecord<TPayload> | null {
    const record = this.get(directory, sessionId);
    return record && record.state === 'live' && record.payload !== null ? record : null;
  }

  /**
   * Unique record by session id across directories. Returns null for cold
   * ids and undefined-ambiguous for the (near-impossible) same-id-two-dirs
   * case, which callers must treat as not-found rather than guessing.
   */
  bySessionId(sessionId: string): LiveRecord<TPayload> | null | undefined {
    let found: LiveRecord<TPayload> | null = null;
    for (const record of this.#records.values()) {
      if (record.sessionId !== sessionId) continue;
      if (found !== null) return undefined;
      found = record;
    }
    return found;
  }

  /** Open the materializing state; throws on evicting/failed/closing keys. */
  beginMaterialize(directory: string, sessionId: string): LiveRecord<TPayload> {
    const key = sessionKey(directory, sessionId);
    const existing = this.#records.get(key);
    if (existing) {
      if (existing.state === 'evicting') {
        throw new SessionBusyError('session-evicting', `session ${sessionId} is evicting`, sessionId);
      }
      if (existing.state === 'failed') {
        throw new SessionBusyError(
          'session-failed',
          `session ${sessionId} is quarantined after a failed disposal (${existing.failure?.reason ?? 'unknown'})`,
          sessionId,
        );
      }
      // materializing (gate race) or live (caller checked) — surface as-is.
      return existing;
    }
    const record: LiveRecord<TPayload> = {
      key,
      directory: normalizeDirectoryKey(directory),
      sessionId,
      state: 'materializing',
      payload: null,
      disposePromise: null,
      lastUsedAt: this.now(),
      inFlight: 0,
      createdAt: this.now(),
      failure: null,
    };
    this.#records.set(key, record);
    return record;
  }

  /** materializing → live. */
  commitMaterialize(record: LiveRecord<TPayload>, payload: TPayload): LiveRecord<TPayload> {
    record.payload = payload;
    record.state = 'live';
    record.lastUsedAt = this.now();
    return record;
  }

  /**
   * materializing → cold after a failed setup. The caller must have already
   * released every resource (manager, subscription, domain handles) — this
   * only drops the dedup row (plan §3.3).
   */
  failMaterialize(record: LiveRecord<TPayload>): void {
    if (record.state !== 'materializing') return;
    this.#records.delete(record.key);
  }

  /**
   * Serialize a mutation boundary on one key. Operations queue; a rejected
   * operation never poisons the queue. Re-entrant use on a held key is a
   * programming error and deadlocks — only mutation boundaries may enter.
   */
  withOperation<T>(key: string, operation: () => Promise<T>): Promise<T> {
    const previous = this.#gates.get(key) ?? Promise.resolve();
    const run = previous.then(operation, operation);
    const settled = run.then(
      () => {
        if (this.#gates.get(key) === settled) this.#gates.delete(key);
      },
      () => {
        if (this.#gates.get(key) === settled) this.#gates.delete(key);
      },
    );
    this.#gates.set(key, settled);
    return run;
  }

  /** live → evicting. Returns false when the record is not live. */
  beginEvict(record: LiveRecord<TPayload>): boolean {
    if (record.state !== 'live') return false;
    record.state = 'evicting';
    return true;
  }

  /**
   * evicting → cold on success; evicting → failed tombstone otherwise (the
   * key stays blocked; only a process restart or explicit manual recovery
   * clears it — plan §3.4).
   */
  finishEvict(record: LiveRecord<TPayload>, ok: boolean, reason?: string): void {
    if (record.state !== 'evicting') return;
    if (ok) {
      this.#records.delete(record.key);
      return;
    }
    record.state = 'failed';
    record.failure = { reason: reason ?? 'dispose failed', at: this.now() };
    record.payload = null;
  }

  /** User-visible use: refresh the TTL (monotonic clock). */
  touch(record: LiveRecord<TPayload>): void {
    record.lastUsedAt = this.now();
  }

  /** Count one in-flight live operation (sweeper guard). */
  beginUse(record: LiveRecord<TPayload>): void {
    record.inFlight += 1;
  }

  endUse(record: LiveRecord<TPayload>): void {
    record.inFlight = Math.max(0, record.inFlight - 1);
  }

  /** Snapshot for the sweeper (ordered copy; safe to iterate while mutating). */
  snapshot(): LiveRecord<TPayload>[] {
    return [...this.#records.values()];
  }

  liveDirectories(): Set<string> {
    const dirs = new Set<string>();
    for (const record of this.#records.values()) {
      if (record.state === 'live' || record.state === 'materializing') dirs.add(record.directory);
    }
    return dirs;
  }

  stats(): LiveRegistryStats {
    let materializing = 0;
    let live = 0;
    let evicting = 0;
    let failed = 0;
    for (const record of this.#records.values()) {
      if (record.state === 'materializing') materializing += 1;
      else if (record.state === 'live') live += 1;
      else if (record.state === 'evicting') evicting += 1;
      else failed += 1;
    }
    return { materializing, live, evicting, failed, total: this.#records.size };
  }

  /** Test/diagnostics: drop a quarantine tombstone (manual recovery path). */
  clearFailure(directory: string, sessionId: string): boolean {
    const record = this.get(directory, sessionId);
    if (!record || record.state !== 'failed') return false;
    this.#records.delete(record.key);
    return true;
  }
}
