// Event buses for the omp host.
//
// `RingEventBus` — bounded-replay bus with entry AND byte caps
// (docs/plan.md §5.1). `emit` takes a `durable` flag: durable entries enter
// the replay ring for Last-Event-ID resume; volatile entries (loaders,
// toasts, control frames) only reach live subscribers, because replaying
// them would resurrect stale UI state. The wire bus is an all-durable
// `RingEventBus`; byte-level wire behavior (envelope
// `{id,type,properties}`, monotonic ids, directory routing) is unchanged.
//
// Caps and gaps:
// - Durable entries are evicted oldest-first until both `replay.length <=
//   capacity` and `replayBytes <= maxBytes` hold. `capacity` alone is not
//   enough: a single `message.part.updated` can carry cumulative tool
//   output.
// - A durable event whose estimated size exceeds `maxEventBytes` is never
//   truncated: it skips the ring entirely (live subscribers still get it)
//   and its id is recorded as a hole. Any reconnecting cursor that needs a
//   skipped id gets a `gap` verdict and must resync (断流不是空状态).
// - `replayState` classifies `ok | restart | gap`. Gap detection is global
//   and conservative across directories: a scoped subscriber whose
//   directory's events were all retained can still be told to resync when
//   some other id range was dropped. Correct-but-heavier beats silently
//   serving a suffix (plan §5.2).
// - `epoch` is a per-bus-instance identity. Numeric id comparison alone
//   cannot prove a cursor belongs to this boot; consumers that care must
//   compare epochs (plan §5.2.1).
//
// `OmpEventBus` — the single omp-native event channel (spec 05 §5.2,
// master D6-R1). Envelopes carry
//   { id, type, directory, sessionID?, schemaVersion, createdAt, payload }
// with a process-global monotonic id; durable entries are subject to the
// same caps as the wire ring.
//
// Units disclaimer: every "bytes" value here is a conservative serialized
// estimate in UTF-16 code units plus fixed per-node overhead. It is NOT a
// JS-heap measure and must not be reported as one (plan §5.1/§9.1).

import { randomUUID } from 'node:crypto';

const WIRE_REPLAY_CAPACITY = 2048;
const OMP_REPLAY_CAPACITY = 512;
// Provisional byte budgets (plan D4: concrete values to be re-tuned from
// phase-0 event-size sampling). Wire events are dominated by
// message.part.updated snapshots; the single-event budget must admit large
// tool outputs without letting one event monopolize the ring.
const WIRE_REPLAY_MAX_BYTES = 8 * 1024 * 1024;
const OMP_REPLAY_MAX_BYTES = 2 * 1024 * 1024;
const SINGLE_EVENT_BUDGET_FRACTION = 4;

/** Compact the ring storage once this many evicted slots accumulate. */
const RING_COMPACT_THRESHOLD = 256;
/** Max tracked hole ranges before the gap state collapses to `uncertain`. */
const MAX_HOLES = 64;

/** Serializable JSON value: the envelope payload contract for SSE transport. */
export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

export interface WireEventEnvelope {
  id: string;
  type: string;
  properties: Record<string, JsonValue>;
}

export interface OmpEventEnvelope {
  id: number;
  type: string;
  directory: string;
  sessionID?: string;
  schemaVersion: string;
  createdAt: number;
  payload: object;
}

export interface BusEntry<TEnvelope = WireEventEnvelope> {
  eventId: number;
  envelope: TEnvelope;
  directory: string;
  durable: boolean;
  /** Serialized-size estimate captured at append; ring entries only. */
  size?: number;
}

export type BusListener<TEnvelope = WireEventEnvelope> = (entry: BusEntry<TEnvelope>) => void;

export type ReplayState =
  | { status: 'ok' }
  | { status: 'restart' }
  | { status: 'gap'; oldest: number };

export interface BusStats {
  /** Per-bus identity; stable for the bus's lifetime, differs per boot. */
  epoch: string;
  nextEventId: number;
  /** Retained durable entries (O(1)). */
  retainedEntries: number;
  /** Estimated retained serialized size (UTF-16 units; declared estimate). */
  retainedBytes: number;
  capacity: number;
  maxBytes: number;
  maxEventBytes: number;
  /** Durable entries evicted to satisfy the caps. */
  evictedEntries: number;
  evictedBytes: number;
  /** Durable events skipped from replay for exceeding maxEventBytes. */
  overBudgetEvents: number;
  /** Tracked hole ranges (rejected id intervals still above the floor). */
  holes: number;
  /** True when hole tracking overflowed: every cursor must resync. */
  uncertain: boolean;
  /** replayState verdicts that were not `ok`. */
  resyncRecommended: number;
  subscribers: number;
}

interface RingEventBusOptions {
  capacity?: number;
  maxBytes?: number;
  /** Single-durable-event budget; over-budget events skip the ring. */
  maxEventBytes?: number;
  durableDefault?: boolean;
}

interface HoleRange {
  from: number;
  to: number;
}

const ESTIMATE_NODE_OVERHEAD = 8;
const ESTIMATE_STRING_OVERHEAD = 4;
/** Traversal guard for untrusted payloads (plan §5.1: no unbounded walks). */
const ESTIMATE_MAX_NODES = 4096;

/**
 * Conservative serialized-size estimate for a `JsonValue`, in UTF-16 code
 * units plus per-node overhead. Bounded: traversal stops (returning `null`
 * = over budget) after `maxNodes` nodes or once the accumulated estimate
 * passes `byteCutoff`. Never recursively stringifies. Union-arm branching
 * is by structure (null / Array / Object / scalar), never `typeof`.
 */
export const estimateJsonValueSize = (value: JsonValue, byteCutoff: number, maxNodes = ESTIMATE_MAX_NODES): number | null => {
  let total = 0;
  let nodes = 0;
  const stack: JsonValue[] = [value];
  while (stack.length > 0) {
    nodes += 1;
    if (nodes > maxNodes) return null;
    const current = stack.pop() ?? null;
    if (current === null) {
      total += 4;
    } else if (Array.isArray(current)) {
      total += ESTIMATE_NODE_OVERHEAD;
      for (const item of current) stack.push(item);
    } else if (current instanceof Object) {
      total += ESTIMATE_NODE_OVERHEAD;
      // Object keys serialize too — a value-only walk undercounts wide
      // records into the byte cap.
      for (const [key, item] of Object.entries(current)) {
        total += key.length;
        stack.push(item);
      }
    } else {
      // string | number | boolean: the scalar's string form is a fair
      // stand-in for all three serialized arms (and tolerates the odd
      // non-JSON value a caller smuggles past the types).
      total += ESTIMATE_STRING_OVERHEAD + String(current).length;
    }
    if (total > byteCutoff) return null;
  }
  return total;
};

export class RingEventBus<TEnvelope = WireEventEnvelope> {
  capacity: number;
  maxBytes: number;
  maxEventBytes: number;
  durableDefault: boolean;
  /** Per-boot identity for restart detection (plan §5.2.1). */
  readonly epoch: string;
  nextEventId: number;
  subscribers: Set<BusListener<TEnvelope>>;

  #entries: Array<BusEntry<TEnvelope>> = [];
  #head = 0;
  #bytes = 0;
  #evictedEntries = 0;
  #evictedBytes = 0;
  #overBudgetEvents = 0;
  #resyncRecommended = 0;
  #holes: HoleRange[] = [];
  #uncertain = false;
  /** Hole-collapse watermark: ids below it stay unprovable this cycle. */
  #uncertainUntilId = 0;
  /** Smallest durable id guaranteed never evicted or rejected so far. */
  #floorId = 1;

  constructor({ capacity = WIRE_REPLAY_CAPACITY, maxBytes = WIRE_REPLAY_MAX_BYTES, maxEventBytes, durableDefault = true }: RingEventBusOptions = {}) {
    this.capacity = capacity;
    this.maxBytes = maxBytes;
    this.maxEventBytes = maxEventBytes ?? Math.max(256 * 1024, Math.floor(maxBytes / SINGLE_EVENT_BUDGET_FRACTION));
    this.durableDefault = durableDefault;
    this.epoch = randomUUID();
    this.nextEventId = 1;
    this.subscribers = new Set();
  }

  /** Retained durable entries, oldest first. Allocates a copy per call. */
  get replay(): Array<BusEntry<TEnvelope>> {
    return this.#entries.slice(this.#head);
  }

  /** O(1) diagnostics accessors (plan §9.1: no copying for counters). */
  get retainedCount(): number {
    return this.#entries.length - this.#head;
  }

  get retainedBytes(): number {
    return this.#bytes;
  }

  /** Newest retained durable id, or `null` when the ring is empty. */
  tailEventId(): number | null {
    return this.#entries.length > this.#head ? this.#entries[this.#entries.length - 1].eventId : null;
  }

  stats(): BusStats {
    return {
      epoch: this.epoch,
      nextEventId: this.nextEventId,
      retainedEntries: this.retainedCount,
      retainedBytes: this.#bytes,
      capacity: this.capacity,
      maxBytes: this.maxBytes,
      maxEventBytes: this.maxEventBytes,
      evictedEntries: this.#evictedEntries,
      evictedBytes: this.#evictedBytes,
      overBudgetEvents: this.#overBudgetEvents,
      holes: this.#holes.length,
      uncertain: this.#uncertain,
      resyncRecommended: this.#resyncRecommended,
      subscribers: this.subscribers.size,
    };
  }

  /**
   * Build and broadcast an event envelope.
   * @param {string} type
   * @param {object} properties Opaque payload — the bus wraps it, never inspects it.
   * @param {string} directory Session directory used for scoped routing.
   * @param {{ durable?: boolean }} [options]
   */
  emit<P extends object>(type: string, properties: P, directory: string | null | undefined, { durable }: { durable?: boolean } = {}): TEnvelope {
    const isDurable = durable ?? this.durableDefault;
    const eventId = this.nextEventId++;
    // SAFETY: TEnvelope is a structural envelope over exactly these fields.
    const envelope = { id: String(eventId), type, properties } as TEnvelope;
    const entry: BusEntry<TEnvelope> = { eventId, envelope, directory: directory ?? '', durable: isDurable };
    if (isDurable) this.appendDurable(entry, envelope);
    Object.freeze(entry);
    this.notifySubscribers(entry);
    return envelope;
  }

  /**
   * Replay entries with eventId greater than `lastEventId`, then subscribe.
   * Volatile entries never replay. Returns an unsubscribe function.
   *
   * Sequence boundary: the wrapped subscriber is registered BEFORE the
   * replay walk and buffers events emitted while the walk runs, so a
   * listener that synchronously emits neither misses nor double-receives
   * events (plan §5.3). A listener throw during replay unregisters the
   * subscriber before propagating.
   */
  subscribeSince(lastEventId: number, listener: BusListener<TEnvelope>, { directory }: { directory?: string } = {}): () => boolean {
    // `cursor` is the replay/pending-dedup threshold (starts at the
    // client's requested id). `delivered` tracks what THIS subscription
    // already handed to the listener; live events below a stale cursor
    // (restart-shaped requests) must still flow (plan §5.3).
    let cursor = lastEventId;
    let delivered = Number.NEGATIVE_INFINITY;
    const pending: BusEntry<TEnvelope>[] = [];
    let replaying = true;
    const pass = (entry: BusEntry<TEnvelope>): boolean => {
      if (directory && entry.directory !== directory) return false;
      return entry.eventId > delivered;
    };
    const wrapped = (entry: BusEntry<TEnvelope>) => {
      if (!pass(entry)) return;
      if (replaying) {
        pending.push(entry);
        return;
      }
      delivered = entry.eventId;
      listener(entry);
    };
    this.subscribers.add(wrapped);
    try {
      // Snapshot the walk: a listener emit that compacts the ring
      // (splice(0, head)) must not shift the indices mid-iteration — a
      // shifted entry would be skipped with no hole recorded, silently
      // violating the sequence boundary this method exists to provide.
      for (const entry of this.#entries.slice(this.#head)) {
        if (entry.eventId <= cursor) continue;
        if (directory && entry.directory !== directory) continue;
        cursor = entry.eventId;
        delivered = entry.eventId;
        listener(entry);
      }
    } catch (error) {
      this.subscribers.delete(wrapped);
      throw error;
    }
    // Reentrant emits during the flush queue behind the pending window
    // (`replaying` stays true), so a newer id can never reach the listener
    // before an older pending one — the flush drains pending in id order.
    try {
      for (let index = 0; index < pending.length; index += 1) {
        const entry = pending[index];
        if (entry.eventId <= delivered) continue;
        if (directory && entry.directory !== directory) continue;
        delivered = entry.eventId;
        listener(entry);
      }
    } finally {
      replaying = false;
    }
    return () => this.subscribers.delete(wrapped);
  }

  /**
   * Gap detection for Last-Event-ID resume (05 §5.2.1, plan §5.2).
   * - `restart`: client id is at/after our next id → ids reset (different
   *   boot). Numeric comparison only catches this direction; epoch-aware
   *   consumers must also compare `bus.epoch` for the other direction.
   * - `gap`: a hole (rejected/evicted id range) intersects the ids the
   *   client still needs, or the client cursor predates the ring floor.
   * - `ok`: the retained ring provably bridges from the client cursor.
   */
  replayState(lastEventId: number): ReplayState {
    const state = this.#classify(lastEventId);
    if (state.status !== 'ok') this.#resyncRecommended += 1;
    return state;
  }

  #classify(lastEventId: number): ReplayState {
    if (lastEventId <= 0) return { status: 'ok' };
    if (lastEventId >= this.nextEventId) return { status: 'restart' };
    if (this.#uncertain) return { status: 'gap', oldest: this.#floorId };
    // The cursor needs every durable id in (lastEventId, nextEventId).
    // Durable ids below the floor were evicted; volatile ids never entered
    // the ring and were live-only by design.
    if (lastEventId < this.#floorId - 1) return { status: 'gap', oldest: this.#floorId };
    for (const hole of this.#holes) {
      if (hole.to > lastEventId) return { status: 'gap', oldest: this.#floorId };
    }
    return { status: 'ok' };
  }

  protected notifySubscribers(entry: BusEntry<TEnvelope>): void {
    for (const subscriber of [...this.subscribers]) {
      try {
        subscriber(entry);
      } catch {
        this.subscribers.delete(subscriber);
      }
    }
  }

  protected appendDurable(entry: BusEntry<TEnvelope>, envelope: TEnvelope): void {
    // SAFETY: every bus envelope is JSON-serializable by construction — the
    // wire contract is {id,type,properties} over JsonValue. TEnvelope is the
    // structural subtype; the estimator only reads it as the JSON arm.
    const size = estimateJsonValueSize(envelope as JsonValue, this.maxEventBytes);
    if (size === null) {
      // Over single-event budget: never truncate payloads (plan §5.1). The
      // event stays live-only and its id becomes a hole for resync logic.
      this.#overBudgetEvents += 1;
      this.#recordHole(entry.eventId);
      return;
    }
    entry.size = size;
    this.#entries.push(entry);
    this.#bytes += size;
    this.#evictToCaps();
  }

  #evictToCaps(): void {
    let evicted = false;
    while (this.#entries.length > this.#head) {
      const count = this.#entries.length - this.#head;
      if (count <= this.capacity && this.#bytes <= this.maxBytes) break;
      const oldest = this.#entries[this.#head];
      // Size was captured at append — re-estimating here would redo the
      // bounded traversal once per eviction.
      const size = oldest.size ?? 0;
      this.#bytes -= size;
      this.#head += 1;
      this.#evictedEntries += 1;
      this.#evictedBytes += size;
      this.#floorId = oldest.eventId + 1;
      evicted = true;
    }
    if (this.#head >= RING_COMPACT_THRESHOLD) {
      this.#entries.splice(0, this.#head);
      this.#head = 0;
    }
    if (!evicted) return;
    // Holes fully below the floor are subsumed by it (a cursor below the
    // floor already verdicts `gap` without per-hole state).
    if (this.#holes.length > 0) {
      this.#holes = this.#holes.filter((hole) => hole.to >= this.#floorId);
    }
    // Uncertainty ages out once every unprovable id has been evicted.
    if (this.#uncertain && this.#floorId > this.#uncertainUntilId) {
      this.#uncertain = false;
      this.#uncertainUntilId = 0;
    }
  }

  #recordHole(eventId: number): void {
    const last = this.#holes[this.#holes.length - 1];
    if (last && last.to === eventId - 1) {
      last.to = eventId;
      return;
    }
    if (this.#holes.length >= MAX_HOLES) {
      // Hole metadata is itself bounded (plan §5.1): collapse to uncertain
      // instead of growing one range per rejected event. Recovery is
      // automatic once the ring floor passes every dropped range.
      this.#uncertain = true;
      this.#uncertainUntilId = Math.max(this.#uncertainUntilId, eventId);
      this.#holes = [];
      return;
    }
    this.#holes.push({ from: eventId, to: eventId });
  }
}

/** Wire bus: OpenCode-compatible envelopes, everything durable (unchanged). */
export class WireEventBus extends RingEventBus<WireEventEnvelope> {
  constructor({ capacity, maxBytes, maxEventBytes }: { capacity?: number; maxBytes?: number; maxEventBytes?: number } = {}) {
    super({ capacity, maxBytes, maxEventBytes, durableDefault: true });
  }
}

export class OmpEventBus extends RingEventBus<OmpEventEnvelope> {
  schemaVersion: string;

  constructor({ capacity, maxBytes, maxEventBytes, schemaVersion = '1.0' }: { capacity?: number; maxBytes?: number; maxEventBytes?: number; schemaVersion?: string } = {}) {
    super({ capacity: capacity ?? OMP_REPLAY_CAPACITY, maxBytes: maxBytes ?? OMP_REPLAY_MAX_BYTES, maxEventBytes, durableDefault: false });
    this.schemaVersion = schemaVersion;
  }

  /**
   * Emit an omp-native event (registered name `omp.<domain>.<event>`).
   * Payload must NOT carry directory/sessionID — those live on the envelope.
   * @param {string} type Registered public name.
   * @param {object | null | undefined} payload Opaque JSON object payload.
   * @param {{ directory: string, sessionID?: string, durable?: boolean }} scope
   */
  publish<P extends object>(type: string, payload: P | null | undefined, { directory, sessionID, durable }: { directory?: string; sessionID?: string; durable?: boolean }): OmpEventEnvelope {
    const eventId = this.nextEventId++;
    const envelope: OmpEventEnvelope = {
      id: eventId,
      type,
      directory: directory ?? '',
      schemaVersion: this.schemaVersion,
      createdAt: Date.now(),
      payload: payload ?? {},
    };
    if (sessionID) envelope.sessionID = sessionID;
    const entry: BusEntry<OmpEventEnvelope> = { eventId, envelope, directory: directory ?? '', durable: Boolean(durable) };
    if (entry.durable) this.appendDurable(entry, envelope);
    Object.freeze(entry);
    this.notifySubscribers(entry);
    return envelope;
  }
}
