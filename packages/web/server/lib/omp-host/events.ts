// Event buses for the omp host.
//
// `RingEventBus` — generalized bounded-replay bus. `emit` takes a `durable`
// flag: durable entries enter the replay ring for Last-Event-ID resume;
// volatile entries (loaders, toasts, control frames) only reach live
// subscribers, because replaying them would resurrect stale UI state.
// The wire bus is an all-durable `RingEventBus`; byte-level wire behavior
// (envelope `{id,type,properties}`, monotonic ids, directory routing) is
// unchanged.
//
// `OmpEventBus` — the single omp-native event channel (spec 05 §5.2,
// master D6-R1). Envelopes carry
//   { id, type, directory, sessionID?, schemaVersion, createdAt, payload }
// with a process-global monotonic id. The replay ring keeps the most recent
// `capacity` durable entries; `replayState` reports gaps so the SSE endpoint
// can emit `omp.stream.resync` before replay (断流不是空状态, master D2).

const WIRE_REPLAY_CAPACITY = 2048;
const OMP_REPLAY_CAPACITY = 512;

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
}

export type BusListener<TEnvelope = WireEventEnvelope> = (entry: BusEntry<TEnvelope>) => void;

export type ReplayState =
  | { status: 'ok' }
  | { status: 'restart' }
  | { status: 'gap'; oldest: number };

interface RingEventBusOptions {
  capacity?: number;
  durableDefault?: boolean;
}

export class RingEventBus<TEnvelope = WireEventEnvelope> {
  capacity: number;
  durableDefault: boolean;
  replay: Array<BusEntry<TEnvelope>>;
  nextEventId: number;
  subscribers: Set<BusListener<TEnvelope>>;

  constructor({ capacity = WIRE_REPLAY_CAPACITY, durableDefault = true }: RingEventBusOptions = {}) {
    this.capacity = capacity;
    this.durableDefault = durableDefault;
    this.replay = [];
    this.nextEventId = 1;
    this.subscribers = new Set();
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
    const entry = { eventId, envelope, directory: directory ?? '', durable: isDurable };
    if (isDurable) {
      this.replay.push(entry);
    }
    for (const subscriber of [...this.subscribers]) {
      try {
        subscriber(entry);
      } catch {
        this.subscribers.delete(subscriber);
      }
    }
    return envelope;
  }

  /**
   * Replay entries with eventId greater than `lastEventId`, then subscribe.
   * Volatile entries never replay. Returns an unsubscribe function.
   */
  subscribeSince(lastEventId: number, listener: BusListener<TEnvelope>, { directory }: { directory?: string } = {}): () => boolean {
    for (const entry of this.replay) {
      if (entry.eventId <= lastEventId) continue;
      if (directory && entry.directory !== directory) continue;
      listener(entry);
    }
    const wrapped = (entry: BusEntry<TEnvelope>) => {
      if (directory && entry.directory !== directory) return;
      listener(entry);
    };
    this.subscribers.add(wrapped);
    return () => this.subscribers.delete(wrapped);
  }
}

/** Wire bus: OpenCode-compatible envelopes, everything durable (unchanged). */
export class WireEventBus extends RingEventBus<WireEventEnvelope> {
  constructor({ capacity = WIRE_REPLAY_CAPACITY }: { capacity?: number } = {}) {
    super({ capacity, durableDefault: true });
  }
}

export class OmpEventBus extends RingEventBus<OmpEventEnvelope> {
  schemaVersion: string;

  constructor({ capacity = OMP_REPLAY_CAPACITY, schemaVersion = '1.0' }: { capacity?: number; schemaVersion?: string } = {}) {
    super({ capacity, durableDefault: false });
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
    const envelope = {
      id: eventId,
      type,
      directory: directory ?? '',
      ...(sessionID ? { sessionID } : {}),
      schemaVersion: this.schemaVersion,
      createdAt: Date.now(),
      payload: payload ?? {},
    };
    const entry = { eventId, envelope, directory: directory ?? '', durable: Boolean(durable) };
    if (entry.durable) {
      this.replay.push(entry);
      if (this.replay.length > this.capacity) this.replay.shift();
    }
    for (const subscriber of [...this.subscribers]) {
      try {
        subscriber(entry);
      } catch {
        this.subscribers.delete(subscriber);
      }
    }
    return envelope;
  }

  /**
   * Gap detection for Last-Event-ID resume (05 §5.2.1).
   * - `restart`: client id is ahead of our tail → process restarted, ids reset.
   * - `gap`: client id is older than the oldest durable ring entry (or the
   *   ring already evicted it) → events were lost beyond the bounded replay.
   * - `ok`: replayable.
   */
  replayState(lastEventId: number): ReplayState {
    if (lastEventId <= 0) return { status: 'ok' };
    if (lastEventId >= this.nextEventId) return { status: 'restart' };
    const oldest = this.replay.length > 0 ? this.replay[0].eventId : null;
    if (oldest === null || lastEventId < oldest - 1) return { status: 'gap', oldest };
    // lastEventId may point at a volatile id that never entered the ring;
    // replaying everything newer than it is still contiguous.
    return { status: 'ok' };
  }
}
