// Domain module: approval + ask dialog bridge (spec 03 §5.1/§5.2/§5.3/§5.4/§5.6,
// master D6 R10/R11/R13). Self-contained by design — the coordinator mounts
// `createDomainDialogs(...).mount(route)` from the shared route table and wires
// the engine integration points listed on `createDomainDialogs` below.
//
// Three pieces:
// - `UiLeaseTable` — per-session UI attachment leases (R13): authenticated
//   clients heartbeat; `hasUI` is holder-count ≥ 1 and nothing else. SSE
//   liveness never counts as presence.
// - `PendingDialogRegistry` — the single dialog authority (D-C3): register,
//   presented-ack (T_answer anchor), respond (atomic settle, 双端竞答 → 409),
//   abort, T_present / T_answer / orphan-window protections, snapshot, and
//   R11 settleAll for every lifecycle exit.
// - `createDialogBridge` — the web `ExtensionUIContext` (D-C1): select /
//   confirm / input / askDialog / notify + editor forward into the registry
//   and resolve from browser responds; terminal-only members are explicit
//   no-ops (RPC-mode degradation shape, rpc-mode.ts:824-925).
//
// Events flow exclusively through the 05-channel `OmpEventBus`
// (omp.dialog.requested / omp.dialog.settled, both durable, directory scope).
// Envelope carries directory/sessionID (events.js authority); payloads do not.

import crypto from 'node:crypto';
import { normalizeDirectoryKey } from './registry.js';
import { ompFeatures, featureUnavailable } from './omp-parity.js';

// Engineering defaults (spec 03 §5.1 D-C1b / §5.4.3-5 / OQ-11). Product-level
// configurability is a master ruling away; these are the built-in values.
export const LEASE_TTL_MS = 30_000;
export const LEASE_HEARTBEAT_MS = 10_000;
export const ORPHAN_WINDOW_MS = 120_000;
export const PRESENT_TTL_MS = 300_000;

const DIALOG_ID_PREFIX = 'dlg_';
const TOMBSTONE_CAP = 1024;

const REQUESTED_EVENT = 'omp.dialog.requested';
const SETTLED_EVENT = 'omp.dialog.settled';

/** Ask-cancel shaped rejection: ask.ts:982-984 converts `AbortError` into a
 *  ToolAbortError; approval wrappers rethrow the message verbatim. */
const abortError = (reason) => {
  const error = new Error(reason);
  error.name = 'AbortError';
  return error;
};

const rejectFor = (kind, reason) =>
  kind === 'ask' ? abortError(reason) : new Error(reason);

const sessionKey = (directory, sessionId) =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionId}`;

/**
 * Per-session UI attachment leases — the single `hasUI` authority (R13).
 *
 * A lease exists per (directory, sessionId); individual UI page instances are
 * reference-counted holders keyed by their client-generated UUID `clientId`.
 * Acquire is acquire-or-renew: repeating the same triple extends the holder's
 * expiry and returns the same leaseId. Holder expiry (TTL, 3 missed
 * heartbeats) or explicit release removes exactly that holder; the lease ends
 * — and `onDetach` fires exactly once — when the last holder leaves.
 * Nothing else renews: SSE connection liveness is not presence.
 */
export class UiLeaseTable {
  /**
   * @param {object} [options]
   * @param {number} [options.ttlMs] Holder TTL (default 30s).
   * @param {number} [options.heartbeatIntervalMs] Advised heartbeat (default 10s).
   * @param {() => number} [options.now] Injectable clock (tests).
   * @param {(fn: () => void, delayMs: number) => unknown} [options.schedule]
   * @param {(handle: unknown) => void} [options.cancel]
   * @param {(info: { directory: string, sessionId: string, leaseId: string }) => void} [options.onAttach]
   * @param {(info: { directory: string, sessionId: string, leaseId: string }) => void} [options.onDetach]
   */
  #now;
  #schedule;
  #cancel;
  #onAttach;
  #onDetach;
  #leases;
  #sweepHandle;

  constructor({
    ttlMs = LEASE_TTL_MS,
    heartbeatIntervalMs = LEASE_HEARTBEAT_MS,
    now = Date.now,
    schedule = setTimeout,
    cancel = clearTimeout,
    onAttach = null,
    onDetach = null,
  } = {}) {
    this.ttlMs = ttlMs;
    this.heartbeatIntervalMs = heartbeatIntervalMs;
    this.#now = now;
    this.#schedule = schedule;
    this.#cancel = cancel;
    this.#onAttach = onAttach;
    this.#onDetach = onDetach;
    /** @type {Map<string, { leaseId: string, directory: string, sessionId: string, holders: Map<string, number> }>} */
    this.#leases = new Map();
    this.#sweepHandle = null;
  }

  /** Acquire-or-renew one holder. Idempotent per (directory, sessionId, clientId). */
  acquire({ directory, sessionId, clientId }) {
    const key = sessionKey(directory, sessionId);
    let lease = this.#leases.get(key);
    const attached = !lease || lease.holders.size === 0;
    if (!lease) {
      lease = {
        leaseId: crypto.randomUUID(),
        directory: normalizeDirectoryKey(directory),
        sessionId,
        holders: new Map(),
      };
      this.#leases.set(key, lease);
    }
    lease.holders.set(clientId, this.#now() + this.ttlMs);
    this.#armSweep();
    if (attached) this.#onAttach?.(leaseInfo(lease));
    return {
      leaseId: lease.leaseId,
      expiresAt: lease.holders.get(clientId),
      heartbeatIntervalMs: this.heartbeatIntervalMs,
      attached,
    };
  }

  /** Explicit release (page unload / leaving the session view). Idempotent. */
  release({ directory, sessionId, clientId }) {
    const lease = this.#leases.get(sessionKey(directory, sessionId));
    if (!lease) return { released: false, detached: false };
    const released = lease.holders.delete(clientId);
    const detached = released && lease.holders.size === 0;
    if (detached) this.#endLease(lease);
    else this.#armSweep();
    return { released, detached, ...(lease.leaseId ? { leaseId: lease.leaseId } : {}) };
  }

  /** True while at least one live holder remains (lazy expiry sweep first). */
  has(directory, sessionId) {
    return this.snapshot(directory, sessionId).hasUI;
  }

  /**
   * Presence snapshot for consumers (engine materialize reads `hasUI` from
   * here; diagnostics read holder counts).
   */
  snapshot(directory, sessionId) {
    const lease = this.#leases.get(sessionKey(directory, sessionId));
    if (!lease) return { hasUI: false, holders: 0, leaseId: null, expiresAt: null };
    const t = this.#now();
    for (const [clientId, expiresAt] of lease.holders) {
      if (expiresAt <= t) lease.holders.delete(clientId);
    }
    if (lease.holders.size === 0) {
      this.#endLease(lease);
      return { hasUI: false, holders: 0, leaseId: null, expiresAt: null };
    }
    let expiresAt = -Infinity;
    for (const value of lease.holders.values()) expiresAt = Math.max(expiresAt, value);
    return { hasUI: true, holders: lease.holders.size, leaseId: lease.leaseId, expiresAt };
  }

  /** Drop every lease (host shutdown). Detach fires once per live lease. */
  releaseAll() {
    const detached = [];
    for (const lease of [...this.#leases.values()]) {
      if (lease.holders.size > 0) detached.push(leaseInfo(lease));
      this.#endLease(lease);
    }
    return detached;
  }

  #endLease(lease) {
    this.#leases.delete(sessionKey(lease.directory, lease.sessionId));
    this.#armSweep();
    this.#onDetach?.(leaseInfo(lease));
  }

  #armSweep() {
    if (this.#sweepHandle !== null) {
      this.#cancel(this.#sweepHandle);
      this.#sweepHandle = null;
    }
    let nextExpiry = Infinity;
    for (const lease of this.#leases.values()) {
      for (const expiresAt of lease.holders.values()) nextExpiry = Math.min(nextExpiry, expiresAt);
    }
    if (!Number.isFinite(nextExpiry)) return;
    this.#sweepHandle = this.#schedule(() => {
      this.#sweepHandle = null;
      // Force lazy expiry through the snapshot path (fires detach transitions).
      for (const lease of [...this.#leases.values()]) this.snapshot(lease.directory, lease.sessionId);
      // #endLease re-arms for ended leases; re-arm for the rest too so a
      // remaining holder always has a pending sweep.
      this.#armSweep();
    }, Math.max(0, nextExpiry - this.#now()));
  }
}

const leaseInfo = (lease) => ({
  directory: lease.directory,
  sessionId: lease.sessionId,
  leaseId: lease.leaseId,
});

// Respond kinds accepted per dialog kind (spec 03 §5.2 RespondResult).
const RESPOND_KINDS = {
  approval: ['select', 'cancel'],
  select: ['select', 'cancel'],
  confirm: ['confirm', 'cancel'],
  input: ['input', 'cancel'],
  editor: ['input', 'editor', 'cancel'],
  ask: ['ask', 'chat', 'cancel'],
};

const isPlainObject = (value) =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

/**
 * Validate a RespondResult against the dialog's contract. Returns
 * { ok: true } or { ok: false, error }.
 */
const validateRespondResult = (record, result) => {
  if (!isPlainObject(result) || typeof result.kind !== 'string') {
    return { ok: false, error: 'result must be an object with a string kind' };
  }
  const allowed = RESPOND_KINDS[record.kind] ?? [];
  if (!allowed.includes(result.kind)) {
    return { ok: false, error: `result kind "${result.kind}" is not valid for a "${record.kind}" dialog` };
  }
  if (result.kind === 'select') {
    const options = record.kind === 'approval'
      ? ['Approve', 'Deny']
      : (record.payload?.select?.options ?? []);
    if (result.value !== undefined && !options.includes(result.value)) {
      return { ok: false, error: 'result.value must be one of the dialog options' };
    }
  } else if (result.kind === 'confirm') {
    if (typeof result.value !== 'boolean') {
      return { ok: false, error: 'result.value must be a boolean for a confirm dialog' };
    }
  } else if (result.kind === 'input' || result.kind === 'editor') {
    if (result.value !== undefined && typeof result.value !== 'string') {
      return { ok: false, error: 'result.value must be a string when present' };
    }
  } else if (result.kind === 'ask') {
    if (!Array.isArray(result.results)) {
      return { ok: false, error: 'result.results must be an array for an ask dialog' };
    }
    const questions = record.payload?.ask?.questions ?? [];
    // The SDK rejects a mismatched count after the fact (ask.ts:931-933);
    // rejecting here keeps the dialog answerable instead of burning it.
    if (result.results.length !== questions.length) {
      return { ok: false, error: 'result.results must answer every question' };
    }
    const questionById = new Map(questions.map((q) => [q.id, q]));
    for (const item of result.results) {
      if (!isPlainObject(item) || typeof item.id !== 'string' || !questionById.has(item.id)) {
        return { ok: false, error: 'result.results contains an unknown question id' };
      }
      if (!Array.isArray(item.selectedOptions)) {
        return { ok: false, error: 'result.results items must carry a selectedOptions array' };
      }
      const labels = (questionById.get(item.id).options ?? []).map((o) => o?.label ?? o);
      for (const label of item.selectedOptions) {
        if (!labels.includes(label)) {
          return { ok: false, error: 'selectedOptions contains a label outside the question options' };
        }
      }
    }
  }
  return { ok: true };
};

/**
 * The pending-dialog authority (D-C3). Registration happens only from the
 * WebUIContext bridge (which itself requires a live lease), so every dialog
 * here was created while a client could answer it. Timeouts:
 * - T_present (from registration): the dialog was never presented — protects
 *   against invisible pendings, it is not a product timeout.
 * - T_answer (from presented-ack): ask dialogs with timeoutMs > 0 auto-submit
 *   the recommended (else first) option, mirroring ask.ts:176-182.
 * - Orphan window (from lease detach): bounded wait for a reconnecting
 *   client; expiry settles approval dialogs with an honest reject, ask
 *   dialogs through the abort path (spec 03 §5.6.1).
 *
 * Settles are atomic: first settle wins, later responds get 409 (双端竞答),
 * wrong-directory requests get 403 (scope binding is registry-authoritative).
 */
export class PendingDialogRegistry {
  /**
   * @param {object} [options]
   * @param {import('./events.js').OmpEventBus} [options.bus] omp event channel (05 authority).
   * @param {number} [options.presentTtlMs] T_present (default 300s).
   * @param {number} [options.orphanWindowMs] Lease-loss grace (default 120s).
   * @param {() => number} [options.now] Injectable clock.
   * @param {(fn: () => void, delayMs: number) => unknown} [options.schedule]
   * @param {(handle: unknown) => void} [options.cancel]
   * @param {(note: { directory: string, sessionId: string, dialogId: string, kind: string, outcome: string, reason: string | null }) => void} [options.onDiagnostic]
   *        R11 transcript-diagnostic hook: invoked for every non-responded
   *        settle so the engine can append a transcript note.
   */
  #bus;
  #now;
  #schedule;
  #cancel;
  #onDiagnostic;
  #dialogs;
  #tombstones;

  constructor({
    bus = null,
    presentTtlMs = PRESENT_TTL_MS,
    orphanWindowMs = ORPHAN_WINDOW_MS,
    now = Date.now,
    schedule = setTimeout,
    cancel = clearTimeout,
    onDiagnostic = null,
  } = {}) {
    this.#bus = bus;
    this.presentTtlMs = presentTtlMs;
    this.orphanWindowMs = orphanWindowMs;
    this.#now = now;
    this.#schedule = schedule;
    this.#cancel = cancel;
    this.#onDiagnostic = onDiagnostic;
    /** @type {Map<string, object>} */
    this.#dialogs = new Map();
    /** @type {Map<string, { outcome: string, directory: string }>} bounded 409 tombstones */
    this.#tombstones = new Map();
  }

  /** Abort one dialog if still pending (signal path); no-op otherwise. */
  abortIfPendingSignal(dialogId, reason = 'dialog aborted by signal') {
    const record = this.#dialogs.get(dialogId);
    if (!record) return false;
    return this.#settle(record, 'aborted', { reason });
  }

  /** Abort every pending dialog of one session (user Stop, session dispose). */
  abortForSession({ directory, sessionId }, reason = 'dialog aborted') {
    let count = 0;
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (this.#settle(record, 'aborted', { reason })) count += 1;
    }
    return count;
  }
  /**
   * Register a dialog. Emits the requested event and starts T_present.
   * @returns {{ id: string, promise: Promise<{ outcome: string, result: object | null }> }}
   *          The promise resolves for responded/cancelled outcomes and for
   *          the ask auto-submit timeout; it rejects (Error, `.outcome`) for
   *          aborted / orphan / T_present settles.
   */
  register({ directory, sessionId, kind, payload }) {
    const id = DIALOG_ID_PREFIX + crypto.randomBytes(16).toString('base64url');
    const record = {
      id,
      directory: normalizeDirectoryKey(directory),
      sessionId,
      kind,
      payload: payload ?? {},
      createdAt: this.#now(),
      presentedAt: null,
      orphan: false,
      settled: null,
      timers: { present: null, answer: null, orphan: null },
    };
    let resolve;
    let reject;
    const promise = new Promise((res, rej) => {
      resolve = res;
      reject = rej;
    });
    record.resolve = resolve;
    record.reject = reject;
    this.#dialogs.set(id, record);
    record.timers.present = this.#schedule(() => this.#expirePresent(record), this.presentTtlMs);
    this.#emit(REQUESTED_EVENT, { dialog: snapshotDialog(record) }, record);
    return { id, promise };
  }

  /** Presented-ack: cancels T_present, anchors T_answer (idempotent). */
  presented(dialogId, { directory } = {}) {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope) return scope;
    const record = this.#dialogs.get(dialogId);
    if (record.presentedAt !== null) {
      return { ok: true, presentedAt: record.presentedAt, duplicate: true };
    }
    record.presentedAt = this.#now();
    if (record.timers.present !== null) {
      this.#cancel(record.timers.present);
      record.timers.present = null;
    }
    const timeoutMs = record.payload?.ask?.timeoutMs ?? 0;
    if (record.kind === 'ask' && timeoutMs > 0) {
      record.timers.answer = this.#schedule(() => this.#expireAnswer(record), timeoutMs);
    }
    return { ok: true, presentedAt: record.presentedAt };
  }

  /**
   * Atomic respond. Registry binding (directory, sessionId, dialogId) is
   * authoritative; the client directory is validation-only.
   */
  respond(dialogId, { directory, clientId = null, result } = {}) {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope) return scope;
    const record = this.#dialogs.get(dialogId);
    const validation = validateRespondResult(record, result);
    if (!validation.ok) return { ok: false, status: 400, error: validation.error };
    const outcome = result.kind === 'cancel' ? 'cancelled' : 'responded';
    if (!this.#settle(record, outcome, { result })) {
      return { ok: false, status: 409, error: 'dialog already settled', outcome: record.settled.outcome };
    }
    return { ok: true, outcome, ...(clientId ? { clientId } : {}) };
  }

  /** User/system abort of one dialog (Stop button on a single card). */
  abort(dialogId, { directory } = {}) {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope) return scope;
    const record = this.#dialogs.get(dialogId);
    if (!this.#settle(record, 'aborted', { reason: 'dialog aborted' })) {
      return { ok: false, status: 409, error: 'dialog already settled', outcome: record.settled.outcome };
    }
    return { ok: true, outcome: 'aborted' };
  }

  /** Abort every pending dialog of one session (user Stop, session dispose). */
  abortForSession({ directory, sessionId }, reason = 'dialog aborted') {
    let count = 0;
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (this.#settle(record, 'aborted', { reason })) count += 1;
    }
    return count;
  }

  /**
   * Lease lost: every pending dialog of the session enters the orphan window
   * (default 120s). A lease re-attach inside the window cancels it and the
   * dialogs resume waiting (spec 03 §5.6.1/§5.6.2-0).
   */
  enterOrphanWindow({ directory, sessionId }) {
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (record.orphan) continue;
      record.orphan = true;
      record.timers.orphan = this.#schedule(() => this.#expireOrphan(record), this.orphanWindowMs);
    }
  }

  /** Lease re-attached: cancel orphan timers, dialogs resume waiting. */
  recoverOrphanWindow({ directory, sessionId }) {
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (!record.orphan) continue;
      record.orphan = false;
      if (record.timers.orphan !== null) {
        this.#cancel(record.timers.orphan);
        record.timers.orphan = null;
      }
    }
  }

  /**
   * R11 lifecycle exit: atomically settle every pending dialog as aborted.
   * Resolvers reject with the exit name (`omp-host shutdown`, …) so the SDK
   * finishes the turn with a diagnostic error; settled events are emitted for
   * all. Returns the settled count.
   */
  settleAll(reason = 'omp-host shutdown') {
    let count = 0;
    for (const record of [...this.#dialogs.values()]) {
      if (this.#settle(record, 'aborted', { reason })) count += 1;
    }
    return count;
  }

  /** Authoritative snapshot (reconnect reconciliation, D2 bootstrap step 4). */
  snapshot({ directory, sessionId = null } = {}) {
    const wanted = directory === undefined || directory === null ? null : normalizeDirectoryKey(directory);
    const dialogs = [];
    for (const record of this.#dialogs.values()) {
      if (wanted !== null && record.directory !== wanted) continue;
      if (sessionId !== null && record.sessionId !== sessionId) continue;
      dialogs.push(snapshotDialog(record));
    }
    dialogs.sort((a, b) => a.createdAt - b.createdAt || (a.id < b.id ? -1 : 1));
    return { dialogs };
  }

  /** Pending count for a session (idle sweeper guard, spec 03 §5.6.5a-3). */
  pendingCount({ directory, sessionId }) {
    return this.#pendingOf(directory, sessionId).length;
  }

  #pendingOf(directory, sessionId) {
    const wanted = normalizeDirectoryKey(directory);
    const out = [];
    for (const record of this.#dialogs.values()) {
      if (record.directory === wanted && record.sessionId === sessionId) out.push(record);
    }
    return out;
  }

  // 404 / 409 / 403 pre-check shared by respond / presented / abort.
  #scopeCheck(dialogId, directory) {
    if (typeof dialogId !== 'string' || dialogId === '') {
      return { ok: false, status: 404, error: 'dialog not found' };
    }
    if (directory === undefined || directory === null) {
      return { ok: false, status: 400, error: 'directory is required' };
    }
    if (this.#dialogs.has(dialogId)) {
      if (normalizeDirectoryKey(directory) !== this.#dialogs.get(dialogId).directory) {
        return { ok: false, status: 403, error: 'directory does not match dialog scope' };
      }
      return null;
    }
    const tombstone = this.#tombstones.get(dialogId);
    if (tombstone) {
      if (normalizeDirectoryKey(directory) !== tombstone.directory) {
        return { ok: false, status: 403, error: 'directory does not match dialog scope' };
      }
      return { ok: false, status: 409, error: 'dialog already settled', outcome: tombstone.outcome };
    }
    return { ok: false, status: 404, error: 'dialog not found' };
  }

  #settle(record, outcome, { result = null, reason = null } = {}) {
    if (record.settled) return false;
    record.settled = { outcome, ...(result ? { result } : {}) };
    for (const key of Object.keys(record.timers)) {
      if (record.timers[key] !== null) {
        this.#cancel(record.timers[key]);
        record.timers[key] = null;
      }
    }
    this.#dialogs.delete(record.id);
    this.#tombstones.set(record.id, { outcome, directory: record.directory });
    if (this.#tombstones.size > TOMBSTONE_CAP) {
      const oldest = this.#tombstones.keys().next().value;
      this.#tombstones.delete(oldest);
    }
    if (outcome === 'responded' || outcome === 'cancelled' || (outcome === 'timeout' && result)) {
      record.resolve({ outcome, result });
    } else {
      const error = reason === null ? new Error(`dialog ${outcome}`) : rejectFor(record.kind, reason);
      error.outcome = outcome;
      error.dialogId = record.id;
      record.reject(error);
    }
    this.#emit(
      SETTLED_EVENT,
      { dialogId: record.id, sessionId: record.sessionId, outcome },
      record,
    );
    if (outcome !== 'responded') {
      this.#onDiagnostic?.({
        directory: record.directory,
        sessionId: record.sessionId,
        dialogId: record.id,
        kind: record.kind,
        outcome,
        reason: reason ?? (outcome === 'timeout' && result ? 'answer window timed out' : null),
      });
    }
    return true;
  }

  #expirePresent(record) {
    if (record.settled) return;
    record.timers.present = null;
    // Approval rejects with the reason; ask takes the abort path (the kind
    // distinction lives in #settle's rejectFor).
    this.#settle(record, 'timeout', { reason: 'dialog expired before presentation' });
  }

  #expireAnswer(record) {
    if (record.settled) return;
    record.timers.answer = null;
    const questions = record.payload?.ask?.questions ?? [];
    const results = questions.map((question) => {
      const options = Array.isArray(question.options) ? question.options : [];
      const recommended =
        typeof question.recommended === 'number' &&
        question.recommended >= 0 &&
        question.recommended < options.length
          ? question.recommended
          : null;
      const label = recommended !== null ? options[recommended].label : options[0]?.label;
      return {
        id: question.id,
        selectedOptions: label === undefined ? [] : [label],
        timedOut: true,
      };
    });
    this.#settle(record, 'timeout', { result: { kind: 'ask', results } });
  }

  #expireOrphan(record) {
    if (record.settled) return;
    record.timers.orphan = null;
    this.#settle(record, 'timeout', { reason: 'dialog orphaned (no UI lease)' });
  }

  #emit(type, payload, record) {
    this.#bus?.publish(type, payload, {
      directory: record.directory,
      sessionID: record.sessionId,
      durable: true,
    });
  }
}

/** Public OmpDialog projection (spec 03 §5.2); internals never leak. */
const snapshotDialog = (record) => ({
  id: record.id,
  sessionId: record.sessionId,
  createdAt: record.createdAt,
  ...(record.presentedAt !== null ? { presentedAt: record.presentedAt } : {}),
  kind: record.kind,
  ...record.payload,
});

/**
 * The web `ExtensionUIContext` (spec 03 §5.1 D-C1). One instance per
 * (directory, sessionId), owned by the engine session lifecycle. Dialogs are
 * only registered while a live lease exists — without one the call fails
 * closed with the SDK's own wording instead of creating a dialog nobody
 * could answer (R13). Terminal-only members are explicit no-ops, mirroring
 * the RPC degradation (rpc-mode.ts:824-925).
 *
 * @param {object} options
 * @param {UiLeaseTable} options.leases
 * @param {PendingDialogRegistry} options.registry
 * @param {string} options.directory
 * @param {string} options.sessionId
 * @param {() => { toolName?: string, toolCallId?: string, tier?: string, reason?: string, approvalMode?: string }} [options.approvalContext]
 *        Best-effort approval enrichment (engine correlates the newest
 *        pending tool_execution_start; absence is fine, spec 03 §5.3.1).
 * @param {(note: { message: string, type: string, directory: string, sessionId: string }) => void} [options.onNotify]
 */
export const createDialogBridge = ({
  leases,
  registry,
  directory,
  sessionId,
  approvalContext = null,
  onNotify = null,
  chrome = null,
}) => {
  const register = (kind, payload) => {
    if (!leases.has(directory, sessionId)) {
      // Same honesty as the SDK approval gate (wrapper.ts:307-322): no lease
      // means hasUI is false and nothing interactive may be pending.
      throw new Error('no interactive UI available');
    }
    return registry.register({ directory, sessionId, kind, payload });
  };

  const settleValue = async (entry, map) => {
    const settled = await entry.promise;
    return map(settled);
  };

  const wireSignal = (entry, dialogOptions) => {
    const signal = dialogOptions?.signal;
    if (!signal) return entry;
    const onAbort = () => registry.abortIfPendingSignal(entry.id, 'dialog aborted by signal');
    signal.addEventListener('abort', onAbort, { once: true });
    const detach = () => signal.removeEventListener('abort', onAbort);
    entry.promise.then(detach, detach);
    return entry;
  };

  const labels = (options) =>
    (options ?? []).map((option) => (typeof option === 'string' ? option : option?.label ?? ''));

  const select = async (title, options, dialogOptions) => {
    const optionLabels = labels(options);
    const isApproval =
      optionLabels.length === 2 && optionLabels[0] === 'Approve' && optionLabels[1] === 'Deny';
    let entry;
    if (isApproval) {
      const context = approvalContext?.() ?? {};
      entry = register('approval', {
        approval: {
          prompt: title,
          ...(context.approvalMode ? { approvalMode: context.approvalMode } : {}),
          ...(context.toolName ? { toolName: context.toolName } : {}),
          ...(context.toolCallId ? { toolCallId: context.toolCallId } : {}),
          ...(context.tier ? { tier: context.tier } : {}),
          ...(context.reason ? { reason: context.reason } : {}),
        },
      });
    } else {
      entry = register('select', { select: { title, options: optionLabels } });
    }
    wireSignal(entry, dialogOptions);
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' ? settled.result.value : undefined);
  };

  const confirm = async (title, message, dialogOptions) => {
    const entry = wireSignal(register('confirm', { confirm: { title, message } }), dialogOptions);
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' && settled.result.kind === 'confirm'
        ? Boolean(settled.result.value)
        : false);
  };

  const inputLike = async (kind, title, placeholder, dialogOptions) => {
    const entry = wireSignal(
      register(kind, { [kind]: { title, ...(placeholder !== undefined ? { placeholder } : {}) } }),
      dialogOptions,
    );
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' && settled.result.kind !== 'cancel'
        ? settled.result.value
        : undefined);
  };

  const askDialog = async (questions, dialogOptions) => {
    const timeoutMs =
      typeof dialogOptions?.timeout === 'number' && dialogOptions.timeout > 0
        ? dialogOptions.timeout
        : 0;
    const passthrough = (questions ?? []).map((question) => ({
      id: question.id,
      question: question.question,
      ...(question.header !== undefined && question.header !== '' ? { header: question.header } : {}),
      options: (question.options ?? []).map((option) => ({
        label: option.label,
        ...(option.description ? { description: option.description } : {}),
        ...(option.preview ? { preview: option.preview } : {}),
      })),
      ...(question.multi !== undefined ? { multi: question.multi } : {}),
      ...(question.recommended !== undefined ? { recommended: question.recommended } : {}),
    }));
    const entry = wireSignal(register('ask', { ask: { questions: passthrough, timeoutMs } }), dialogOptions);
    return settleValue(entry, (settled) => {
      const result = settled.result;
      if (result?.kind === 'chat') return { kind: 'chat' };
      if (result?.kind !== 'ask') return undefined; // cancel → ask tool aborts (ask.ts:914-917)
      const byId = new Map(passthrough.map((question) => [question.id, question]));
      const results = result.results.map((item) => {
        const question = byId.get(item.id);
        return {
          id: item.id,
          question: question?.question ?? '',
          options: (question?.options ?? []).map((option) => option.label),
          multi: Boolean(question?.multi),
          selectedOptions: item.selectedOptions ?? [],
          ...(item.customInput ? { customInput: item.customInput } : {}),
          ...(item.note ? { note: item.note } : {}),
          ...(item.timedOut ? { timedOut: true } : {}),
        };
      });
      return { kind: 'submit', results };
    });
  };

  return {
    timeoutStartsOnPresentation: true,
    select,
    confirm,
    input: (title, placeholder, dialogOptions) => inputLike('input', title, placeholder, dialogOptions),
    askDialog,
    editor: (title, prefill, dialogOptions) => inputLike('editor', title, prefill, dialogOptions),
    notify: (message, type) =>
      onNotify?.({ message, type: type ?? 'info', directory, sessionId }),
    // Terminal-only surface (spec 03 §5.1 触点 4) — string-payload chrome
    // delegates to the extension chrome table when provided (spec 09 §5,
    // mirroring RpcExtensionUIRequest); TUI-bound members stay no-ops but
    // count as observable drops (09 R-E3).
    onTerminalInput: () => () => {},
    setStatus: (key, text) => chrome?.setStatus(key, text),
    setWorkingMessage: () => chrome?.noteDropped('setWorkingMessage'),
    setWidget: (key, content, options) => chrome?.setWidget(key, content, options),
    setFooter: () => chrome?.noteDropped('setFooter.factory'),
    setHeader: () => chrome?.noteDropped('setHeader.factory'),
    setTitle: () => chrome?.noteDropped('setTitle'),
    custom: async () => chrome?.noteDropped('custom.factory'),
    setEditorText: () => chrome?.noteDropped('setEditorText'),
    pasteToEditor: (text) => chrome?.noteDropped('pasteToEditor'),
    getEditorText: () => '',
    addAutocompleteProvider: () => {},
    setEditorComponent: () => {},
    get theme() {
      return {};
    },
    getAllThemes: async () => [],
    getTheme: async () => undefined,
    setTheme: async () => ({ success: false, error: 'the web dialog bridge does not manage themes' }),
    getToolsExpanded: () => false,
    setToolsExpanded: () => {},
  };
};

/**
 * "Always allow" advanced action — a transaction, not a button (spec 03
 * §5.3.2, master R10). Hard ordering: the settings write happens first and
 * only a successful write approves. A failed write never approves (the dialog
 * stays open; the caller surfaces the error and manual Approve/Deny remain).
 * A 409 from the approve step (another client settled first) leaves the
 * persisted override in place — same semantics as the settings page, auditable.
 *
 * @param {() => Promise<unknown>} settingsWrite Writes `tools.approval.<tool|policyKey> = "allow"`
 *        through the Ch06 settings channel. Rejection aborts the transaction.
 * @param {() => Promise<{ ok?: boolean, status?: number }>} approve Posts the Approve respond.
 * @returns {Promise<{ settingsWritten: true, approved: boolean, alreadySettled: boolean }>}
 * @throws the settings-write error (approve is never called), or a non-409
 *         approve error.
 */
export const alwaysAllowTransaction = async (settingsWrite, approve) => {
  await settingsWrite();
  const isConflict = (value) => value?.status === 409 || value?.alreadySettled === true;
  let approved = true;
  try {
    const result = await approve();
    if (isConflict(result)) approved = false;
  } catch (error) {
    if (isConflict(error)) approved = false;
    else throw error;
  }
  return { settingsWritten: true, approved, alreadySettled: !approved };
};

const json = (data, init) => Response.json(data, init);
const badRequest = (message) => json({ error: message }, { status: 400 });

const readJsonBody = async (request) => {
  try {
    return await request.json();
  } catch {
    return {};
  }
};

const requireStrings = (body, names) => {
  for (const name of names) {
    if (typeof body?.[name] !== 'string' || body[name] === '') {
      return `'${name}' is required`;
    }
  }
  return null;
};

/**
 * Mount the dialog endpoint group (public paths /api/omp/dialogs*; the web
 * proxy strips /api, so omp-host routes are /omp/dialogs*). Every route is
 * feature-gated at request time against the shared capability table —
 * flipping `dialogs.v1` in omp-parity.js flips the whole surface without a
 * remount. Unattended = no lease = hasUI:false = SDK fail-closed preserved
 * (R13): the gate never changes session interactivity by itself.
 *
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {object} deps
 * @param {UiLeaseTable} deps.leases
 * @param {PendingDialogRegistry} deps.registry
 * @param {() => boolean} [deps.feature] Capability probe (default: live ompFeatures).
 */
export const registerDialogEndpoints = (route, { leases, registry, feature = null } = {}) => {
  const enabled = feature ?? (() => Boolean(ompFeatures()['dialogs.v1']));
  const gated = (handler) => async (request, ctx) => {
    if (!enabled()) return featureUnavailable('dialogs.v1');
    return handler(request, ctx);
  };

  route('GET', '/omp/dialogs', gated(async (request) => {
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!directory) return badRequest('directory is required');
    return json(registry.snapshot({ directory }));
  }));

  route('POST', '/omp/dialogs/lease', gated(async (request) => {
    const body = await readJsonBody(request);
    const missing = requireStrings(body, ['directory', 'sessionId', 'clientId']);
    if (missing) return badRequest(missing);
    const lease = leases.acquire({
      directory: body.directory,
      sessionId: body.sessionId,
      clientId: body.clientId,
    });
    return json({
      leaseId: lease.leaseId,
      expiresAt: lease.expiresAt,
      heartbeatIntervalMs: lease.heartbeatIntervalMs,
    });
  }));

  route('POST', '/omp/dialogs/lease/release', gated(async (request) => {
    const body = await readJsonBody(request);
    const missing = requireStrings(body, ['directory', 'sessionId', 'clientId']);
    if (missing) return badRequest(missing);
    const released = leases.release({
      directory: body.directory,
      sessionId: body.sessionId,
      clientId: body.clientId,
    });
    return json({ ok: true, released: released.released, detached: released.detached });
  }));

  route('POST', '/omp/dialogs/{id}/respond', gated(async (request, ctx) => {
    const body = await readJsonBody(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const outcome = registry.respond(ctx.params.id, {
      directory: body.directory,
      clientId: typeof body.clientId === 'string' ? body.clientId : null,
      result: body.result,
    });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, outcome: outcome.outcome });
  }));

  route('POST', '/omp/dialogs/{id}/presented', gated(async (request, ctx) => {
    const body = await readJsonBody(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const outcome = registry.presented(ctx.params.id, { directory: body.directory });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, presentedAt: outcome.presentedAt });
  }));

  route('POST', '/omp/dialogs/{id}/abort', gated(async (request, ctx) => {
    const body = await readJsonBody(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const outcome = registry.abort(ctx.params.id, { directory: body.directory });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, outcome: outcome.outcome });
  }));
};

/**
 * Engine integration points the coordinator wires (spec 03 §5.0 item 9,
 * §5.1 D-C1b table). This module deliberately does not touch engine.js.
 *
 * 1. Creation-time `hasUI`: in `#materialize`, pass
 *      hasUI: dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI
 *    to createAgentSession (sdk.ts options.hasUI → toolSession.hasUI, the
 *    ask-tool registration gate). Verified: sdk.ts:562-563 / :1670.
 * 2. Lease flip 0→n on a materialized session: call
 *      session.extensionRunner?.initialize(actions, contextActions, commandActions, dialogs.uiContextFor(dir, sid), 'json')
 *    (repeat-initialize is a supported path, runner.ts:702-705; passing the
 *    uiContext sets runner.hasUI() true, runner.ts:698/:878-880) and then
 *      createAgentSessionResult.setToolUIContext(uiContext, true)
 *    (sdk.ts:3167-3169 → toolContextStore). The runner is reachable as the
 *    public getter `session.extensionRunner` (agent-session.ts:9425) — keep
 *    the CreateAgentSessionResult for setToolUIContext.
 * 3. Lease flip n→0: re-initialize with uiContext omitted (→ noOpUIContext,
 *    fail-closed approval gate), setToolUIContext(uiContext, false). The
 *    orphan windows and settled events are handled inside this domain
 *    (`onSessionUiDetached` below is the engine hook).
 * 4. Lifecycle exits (SIGTERM/SIGINT, dispose routes, session delete):
 *      dialogs.registry.settleAll('<exit name>')
 *    before disposing sessions; `onDiagnostic` receives per-dialog notes for
 *    the transcript. `dialogs.dispose(reason)` does settleAll + releaseAll.
 * 5. Idle sweeper guard: skip sessions where
 *      dialogs.registry.pendingCount({ directory, sessionId }) > 0.
 *
 * @param {object} [options]
 * @param {import('./events.js').OmpEventBus} [options.bus] omp event channel.
 * @param {(info: { directory: string, sessionId: string }) => void} [options.onSessionUiAttached]
 *        Engine hook: perform assembly (2) for an already-materialized session.
 * @param {(info: { directory: string, sessionId: string }) => void} [options.onSessionUiDetached]
 *        Engine hook: perform disassembly (3). Orphan windows start first so
 *        the registry state is already bounded when the hook runs.
 * @param {(note: object) => void} [options.onDiagnostic] R11 transcript hook.
 * @param {object} [options.clock] Test seam { now, schedule, cancel }.
 * @param {object} [options.config] { leaseTtlMs, leaseHeartbeatMs, presentTtlMs, orphanWindowMs }.
 */
export const createDomainDialogs = ({
  bus = null,
  onSessionUiAttached = null,
  onSessionUiDetached = null,
  onDiagnostic = null,
  clock = null,
  config = {},
} = {}) => {
  const now = clock?.now ?? Date.now;
  const schedule = clock?.schedule ?? setTimeout;
  const cancel = clock?.cancel ?? clearTimeout;
  const registry = new PendingDialogRegistry({
    bus,
    now,
    schedule,
    cancel,
    ...(config.presentTtlMs !== undefined ? { presentTtlMs: config.presentTtlMs } : {}),
    ...(config.orphanWindowMs !== undefined ? { orphanWindowMs: config.orphanWindowMs } : {}),
    onDiagnostic,
  });
  const leases = new UiLeaseTable({
    now,
    schedule,
    cancel,
    ...(config.leaseTtlMs !== undefined ? { ttlMs: config.leaseTtlMs } : {}),
    ...(config.leaseHeartbeatMs !== undefined ? { heartbeatIntervalMs: config.leaseHeartbeatMs } : {}),
    onAttach: ({ directory, sessionId }) => {
      registry.recoverOrphanWindow({ directory, sessionId });
      onSessionUiAttached?.({ directory, sessionId });
    },
    onDetach: ({ directory, sessionId }) => {
      registry.enterOrphanWindow({ directory, sessionId });
      onSessionUiDetached?.({ directory, sessionId });
    },
  });
  /** @type {Map<string, object>} one bridge per (directory, sessionId) */
  const bridges = new Map();
  return {
    leases,
    registry,
    /** Per-session WebUIContext (cached for the session lifetime). */
    uiContextFor(directory, sessionId, bridgeOptions = {}) {
      const key = sessionKey(directory, sessionId);
      let bridge = bridges.get(key);
      if (!bridge) {
        bridge = createDialogBridge({
          leases,
          registry,
          directory,
          sessionId,
          ...bridgeOptions,
        });
        bridges.set(key, bridge);
      }
      return bridge;
    },
    /** Consumers contract for creation-time hasUI + diagnostics (R13). */
    hasUISnapshotFor(directory, sessionId) {
      return leases.snapshot(directory, sessionId);
    },
    /** Mount the endpoint group onto the shared route table. */
    mount(route, options = {}) {
      registerDialogEndpoints(route, { leases, registry, ...options });
      return this;
    },
    /** Host lifecycle exit: settle everything, drop every lease (R11). */
    async dispose(reason = 'omp-host shutdown') {
      const settled = registry.settleAll(reason);
      leases.releaseAll();
      bridges.clear();
      return settled;
    },
  };
};
