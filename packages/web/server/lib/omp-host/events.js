// Event bus for the omp host's OpenCode-compatible SSE surface.
//
// Wire events are `{ id, type, properties }` envelopes. The bus assigns
// monotonic ids, keeps a bounded replay ring for Last-Event-ID resume, and
// tags every event with the directory of its session so `/event` streams can
// be scoped per project directory while `/global/event` fans out everything.

const REPLAY_CAPACITY = 2048;

export class WireEventBus {
  constructor({ capacity = REPLAY_CAPACITY } = {}) {
    this.capacity = capacity;
    /** @type {Array<{ eventId: number, envelope: Record<string, unknown>, directory: string }>} */
    this.replay = [];
    this.nextEventId = 1;
    /** @type {Set<(entry: { eventId: number, envelope: Record<string, unknown>, directory: string }) => void>} */
    this.subscribers = new Set();
  }

  /**
   * Build and broadcast an event envelope.
   * @param {string} type
   * @param {Record<string, unknown>} properties
   * @param {string} directory Session directory used for scoped routing.
   */
  emit(type, properties, directory) {
    const eventId = this.nextEventId++;
    const envelope = { id: String(eventId), type, properties };
    const entry = { eventId, envelope, directory: directory ?? '' };
    this.replay.push(entry);
    if (this.replay.length > this.capacity) this.replay.shift();
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
   * Returns an unsubscribe function.
   */
  subscribeSince(lastEventId, listener, { directory } = {}) {
    for (const entry of this.replay) {
      if (entry.eventId <= lastEventId) continue;
      if (directory && entry.directory !== directory) continue;
      listener(entry);
    }
    const wrapped = (entry) => {
      if (directory && entry.directory !== directory) return;
      listener(entry);
    };
    this.subscribers.add(wrapped);
    return () => this.subscribers.delete(wrapped);
  }
}
