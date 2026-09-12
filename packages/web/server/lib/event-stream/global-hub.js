import { createUpstreamSseReader } from './upstream-reader.js';

// Raised from 512 → 2048 to improve recovery after brief disconnects during
// long-running agent sessions where many events accumulate quickly.
const MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT = 2048;
// Byte cap alongside the entry cap (docs/plan.md §5.4): count-only trimming
// still lets a few huge tool-output events pin unbounded memory. The
// estimate is serialized UTF-16 units + per-node overhead, not a JS-heap
// measure.
const MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES = 8 * 1024 * 1024;
const ESTIMATE_NODE_OVERHEAD = 8;
const ESTIMATE_MAX_NODES = 2048;
const ESTIMATE_CLAMP_BYTES = 32 * 1024 * 1024;

/**
 * Non-empty-string arm check for untrusted boundary values. The prototype
 * tag discriminates without `typeof` narrowing (repo isString idiom).
 */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

/** Bounded conservative serialized-size estimate (UTF-16 units). */
function estimatePayloadBytes(value) {
  let total = 0;
  let nodes = 0;
  const stack = [value];
  while (stack.length > 0) {
    nodes += 1;
    if (nodes > ESTIMATE_MAX_NODES) return ESTIMATE_CLAMP_BYTES;
    const current = stack.pop();
    if (current === null || current === undefined) {
      total += 4;
    } else if (Array.isArray(current)) {
      total += ESTIMATE_NODE_OVERHEAD;
      for (const item of current) stack.push(item);
    } else if (current instanceof Object) {
      total += ESTIMATE_NODE_OVERHEAD;
      // Keys serialize too — a value-only walk undercounts wide records.
      for (const [key, item] of Object.entries(current)) {
        total += key.length;
        stack.push(item);
      }
    } else {
      // string/number/boolean (or a smuggled primitive): the string form's
      // length stands in for the serialized size.
      total += 4 + String(current).length;
    }
    if (total > ESTIMATE_CLAMP_BYTES) return ESTIMATE_CLAMP_BYTES;
  }
  return total;
}

export function createGlobalMessageStreamHub({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  fetchImpl = fetch,
  upstreamStallTimeoutMs,
  upstreamReconnectDelayMs,
  replayLimit = MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT,
  replayMaxBytes = MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES,
}) {
  const eventSubscribers = new Set();
  const statusSubscribers = new Set();
  /** Retained replay: `[{ event, bytes }]`, oldest first — head-indexed, so
   *  eviction never pays a linear shift() per entry (docs/plan.md §5.1). */
  let replay = [];
  let replayHead = 0;
  let replayBytes = 0;

  let controller = null;
  let reader = null;
  let connected = false;
  let everConnected = false;
  let buildUrlFailed = false;
  /** Bumped on every stop()/start(): late events from a stopped reader are dropped. */
  let generation = 0;
  /** Upstream boot identity when the upstream advertises one (omp host). */
  let upstreamEpoch = null;
  const stats = { evictedEntries: 0, evictedBytes: 0, resyncs: 0, restarts: 0 };

  const notifySubscriber = (kind, subscriber, payload) => {
    try {
      const result = subscriber(payload);
      if (result != null && result.catch instanceof Function) {
        result.catch((error) => {
          console.warn(`Global message stream ${kind} subscriber failed:`, error);
        });
      }
    } catch (error) {
      console.warn(`Global message stream ${kind} subscriber failed:`, error);
    }
  };

  const notifyStatus = (status) => {
    for (const subscriber of Array.from(statusSubscribers)) {
      notifySubscriber('status', subscriber, status);
    }
  };

  const notifyEvent = (normalized) => {
    for (const subscriber of Array.from(eventSubscribers)) {
      notifySubscriber('event', subscriber, normalized);
    }
  };

  const REPLAY_COMPACT_THRESHOLD = 1024;

  const clearReplay = (reason) => {
    const retained = replay.length - replayHead;
    if (retained > 0) {
      stats.evictedEntries += retained;
      stats.evictedBytes += replayBytes;
    }
    replay = [];
    replayHead = 0;
    replayBytes = 0;
    return reason;
  };

  const pushReplay = (normalized) => {
    const bytes = estimatePayloadBytes(normalized.payload);
    replay.push({ event: normalized, bytes });
    replayBytes += bytes;
    while (replay.length - replayHead > replayLimit || replayBytes > replayMaxBytes) {
      const oldest = replay[replayHead];
      replayHead += 1;
      replayBytes -= oldest.bytes;
      stats.evictedEntries += 1;
      stats.evictedBytes += oldest.bytes;
    }
    if (replayHead >= REPLAY_COMPACT_THRESHOLD) {
      replay.splice(0, replayHead);
      replayHead = 0;
    }
  };

  const normalizeEvent = (event) => {
    const envelope = event?.envelope;
    return {
      envelope,
      payload: event.payload,
      directory: stringOrNull(envelope?.directory) ?? 'global',
      eventId: stringOrNull(envelope?.eventId) ?? undefined,
      eventName: stringOrNull(event?.eventName) ?? undefined,
    };
  };

  const start = () => {
    if (reader) {
      return;
    }

    generation += 1;
    const gen = generation;
    controller = new AbortController();
    // Resume a retained ring: the fresh reader's cursor is the hub's replay
    // tail + remembered epoch. Without them the upstream replays its whole
    // ring into an already-populated buffer — duplicate retained entries and
    // a duplicate live burst for attached clients.
    const replayTail = replay.length > 0 ? replay[replay.length - 1].event?.eventId : null;
    reader = createUpstreamSseReader({
      signal: controller.signal,
      initialLastEventId: stringOrNull(replayTail) ?? '',
      initialEpoch: upstreamEpoch ?? '',
      stallTimeoutMs: upstreamStallTimeoutMs,
      reconnectDelayMs: upstreamReconnectDelayMs,
      fetchImpl,
      buildUrl: () => {
        buildUrlFailed = false;
        try {
          return new URL(buildOpenCodeUrl('/global/event', ''));
        } catch {
          buildUrlFailed = true;
          throw new Error('OpenCode service unavailable');
        }
      },
      getHeaders: getOpenCodeAuthHeaders,
      onConnect() {
        if (gen !== generation) return;
        connected = true;
        const wasReady = everConnected;
        everConnected = true;
        notifyStatus({ type: 'connect', wasReady });
      },
      onDisconnect({ reason }) {
        if (gen !== generation) return;
        connected = false;
        notifyStatus({ type: 'disconnect', reason });
      },
      onEpochChange({ epoch, changed }) {
        if (gen !== generation) return;
        // Compare against the hub's remembered epoch, not just the reader's
        // `changed` flag: after stop()/start() a fresh reader has no prior
        // epoch (changed:false on its first connect), yet a rebooted
        // upstream still makes every retained frame and client cursor
        // untrustworthy (docs/plan.md §5.4).
        const rebooted = changed || (upstreamEpoch !== null && upstreamEpoch !== epoch);
        upstreamEpoch = epoch;
        if (!rebooted) return;
        stats.restarts += 1;
        clearReplay('upstream-epoch-change');
        notifyStatus({ type: 'restart', epoch, reason: 'upstream-epoch-change' });
      },
      onEvent(event) {
        if (gen !== generation) return;
        if (event.eventName === 'omp.stream.boot') {
          // Transport identity metadata, never a business event.
          return;
        }
        if (event.payload === null || event.payload === undefined) {
          // Upstream control frame (data-less): the retained replay cannot
          // bridge any client cursor anymore. Controls never replay.
          if (event.eventName === 'omp.stream.resync') {
            stats.resyncs += 1;
            clearReplay('upstream-resync');
            notifyStatus({ type: 'restart', epoch: upstreamEpoch, reason: 'upstream-resync' });
          }
          return;
        }
        const normalized = normalizeEvent(event);
        if (normalized.eventId) {
          pushReplay(normalized);
        }
        notifyEvent(normalized);
      },
      onError(error) {
        if (gen !== generation) return;
        if (controller?.signal.aborted) {
          return;
        }

        notifyStatus({
          type: everConnected ? 'error' : 'initial-error',
          error,
          buildUrlFailed,
        });
      },
    });

    void reader.start();
  };
  const stop = () => {
    generation += 1;
    connected = false;
    reader?.stop();
    if (controller && !controller.signal.aborted) {
      controller.abort();
    }
    reader = null;
    controller = null;
    everConnected = false;
    buildUrlFailed = false;
    // Retained replay survives stop()/start(): a reconnecting browser
    // client still needs its suffix, and stopHubIfUnused stops the hub
    // whenever the last WS client leaves. Epoch safety lives on the next
    // start's upstream epoch check — a rebooted upstream clears the replay
    // and forces client resyncs there (docs/plan.md §5.4); the generation
    // bump above already drops every late event from the old reader.
  };

  return {
    start,
    stop,
    isConnected() {
      return connected;
    },
    hasConnected() {
      return everConnected;
    },
    subscribeEvent(subscriber) {
      eventSubscribers.add(subscriber);
      return () => {
        eventSubscribers.delete(subscriber);
      };
    },
    subscribeStatus(subscriber) {
      statusSubscribers.add(subscriber);
      return () => {
        statusSubscribers.delete(subscriber);
      };
    },
    /** Newest retained event id, or null when the replay is empty. */
    tailEventId() {
      return replay.length > replayHead ? replay[replay.length - 1].event.eventId ?? null : null;
    },
    /**
     * Replay after `eventId`. `gap` means the requested id is no longer
     * retained (evicted, cleared, or from another boot): the caller must
     * resync, never serve a silent suffix (docs/plan.md §5.4). Only a
     * missing cursor (fresh client) is `ok` with no events.
     */
    replayAfter(eventId) {
      if (!eventId) {
        return { status: 'ok', events: [] };
      }
      let index = -1;
      for (let i = replayHead; i < replay.length; i += 1) {
        if (replay[i].event.eventId === eventId) {
          index = i;
          break;
        }
      }
      if (index === -1) {
        return { status: 'gap', events: [] };
      }
      return { status: 'ok', events: replay.slice(index + 1).map((entry) => entry.event) };
    },
    getStats() {
      return {
        retainedEntries: replay.length - replayHead,
        retainedBytes: replayBytes,
        replayLimit,
        replayMaxBytes,
        evictedEntries: stats.evictedEntries,
        evictedBytes: stats.evictedBytes,
        resyncs: stats.resyncs,
        restarts: stats.restarts,
        upstreamEpoch,
        eventSubscribers: eventSubscribers.size,
        statusSubscribers: statusSubscribers.size,
      };
    },
  };
}
