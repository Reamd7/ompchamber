import { describe, expect, test } from 'vitest';
import { OmpEventBus, WireEventBus, estimateJsonValueSize } from './events.ts';

// Ring cap/gap contracts (docs/plan.md §5.1/§5.2, acceptance §10.1 event
// ring). Byte figures are the bus's declared serialized estimate (UTF-16
// units + per-node overhead), never a JS-heap claim.

describe('RingEventBus caps', () => {
  test('entry cap bounds durable replay for the all-durable wire bus', () => {
    const bus = new WireEventBus({ capacity: 3 });
    for (let i = 0; i < 10; i += 1) bus.emit('message.updated', { i }, '/a');
    const replay = bus.replay;
    expect(replay).toHaveLength(3);
    expect(replay.map((entry) => entry.eventId)).toEqual([8, 9, 10]);
    expect(bus.retainedCount).toBe(3);
    expect(bus.stats().evictedEntries).toBe(7);
  });

  test('10000 events satisfy entry and byte caps simultaneously', () => {
    // Fixed small caps: any unbounded growth fails immediately.
    const bus = new WireEventBus({ capacity: 128, maxBytes: 16 * 1024 });
    for (let i = 0; i < 10_000; i += 1) {
      bus.emit('message.part.updated', { sessionID: 's', part: { id: `p${i}`, text: 'x'.repeat(64) } }, '/a');
    }
    const stats = bus.stats();
    expect(stats.retainedEntries).toBeLessThanOrEqual(128);
    expect(stats.retainedBytes).toBeLessThanOrEqual(16 * 1024);
    expect(stats.nextEventId).toBe(10_001);
    expect(bus.tailEventId()).toBe(10_000);
  });

  test('cumulative toolPartial growth is bounded by the byte cap', () => {
    const bus = new WireEventBus({ capacity: 2048, maxBytes: 8 * 1024 });
    // One part whose output accumulates: each event carries the full text so
    // far — the shape that made count-only caps insufficient.
    let output = '';
    for (let i = 0; i < 400; i += 1) {
      output += 'x'.repeat(64);
      bus.emit('message.part.updated', { sessionID: 's', part: { id: 'p', output } }, '/a');
    }
    const stats = bus.stats();
    expect(stats.retainedBytes).toBeLessThanOrEqual(8 * 1024);
    expect(stats.retainedEntries).toBeLessThanOrEqual(2048);
    expect(stats.evictedEntries).toBeGreaterThan(0);
  });

  test('over-budget single events skip replay but still reach live subscribers', () => {
    const bus = new WireEventBus({ capacity: 16, maxBytes: 64 * 1024, maxEventBytes: 512 });
    const live: string[] = [];
    bus.subscribeSince(0, (entry) => live.push(entry.envelope.type));
    bus.emit('message.updated', { i: 1 }, '/a');
    bus.emit('message.part.updated', { part: { output: 'y'.repeat(4096) } }, '/a');
    bus.emit('message.updated', { i: 3 }, '/a');

    // Live delivery saw every event, including the over-budget one.
    expect(live).toEqual(['message.updated', 'message.part.updated', 'message.updated']);
    // Replay kept the small events; the big one never entered the ring.
    expect(bus.replay.map((entry) => entry.eventId)).toEqual([1, 3]);
    expect(bus.stats().overBudgetEvents).toBe(1);
  });

  test('rejected ids become holes that force gap verdicts for older cursors', () => {
    const bus = new WireEventBus({ capacity: 16, maxBytes: 64 * 1024, maxEventBytes: 256 });
    bus.emit('message.updated', { i: 1 }, '/a');
    bus.emit('message.part.updated', { part: { output: 'y'.repeat(2048) } }, '/a'); // id 2: hole
    bus.emit('message.updated', { i: 3 }, '/a');

    // Cursor 0 is a fresh client (no Last-Event-ID): it replays what the
    // ring has and bootstraps authoritatively; the wire contract keeps
    // that `ok`. Cursors that still need the skipped id must resync.
    expect(bus.replayState(0).status).toBe('ok');
    expect(bus.replayState(1).status).toBe('gap'); // needs skipped id 2
    expect(bus.replayState(2).status).toBe('ok'); // past the hole
    expect(bus.replayState(3).status).toBe('ok'); // at the tail
  });

  test('adjacent rejected ids merge into one hole range', () => {
    const bus = new WireEventBus({ capacity: 16, maxBytes: 64 * 1024, maxEventBytes: 256 });
    bus.emit('message.updated', { i: 0 }, '/a');
    bus.emit('message.part.updated', { part: { output: 'y'.repeat(512) } }, '/a'); // 2
    bus.emit('message.part.updated', { part: { output: 'y'.repeat(512) } }, '/a'); // 3
    bus.emit('message.updated', { i: 4 }, '/a');
    expect(bus.stats().holes).toBe(1); // merged 2..3, not two ranges
    expect(bus.replayState(1).status).toBe('gap');
    expect(bus.replayState(3).status).toBe('ok');
  });

  test('hole overflow collapses to uncertain and recovers after eviction', () => {
    // MAX_HOLES is 64: force 65 separated rejections without eviction
    // pressure (a small ring would subsume holes into its floor first).
    const bus = new WireEventBus({ capacity: 1000, maxBytes: 64 * 1024 * 1024, maxEventBytes: 256 });
    for (let i = 0; i < 65; i += 1) {
      bus.emit('message.updated', { i }, '/a');
      bus.emit('message.part.updated', { part: { output: 'y'.repeat(512) } }, '/a');
    }
    const collapsed = bus.stats();
    expect(collapsed.uncertain).toBe(true);
    expect(bus.replayState(bus.tailEventId() ?? 0).status).toBe('gap'); // uncertain: nobody trusted

    // Push enough events for the floor to pass the watermark, then the bus
    // must become provable again for cursors at/after the floor.
    for (let i = 0; i < 2000; i += 1) bus.emit('message.updated', { i }, '/a');
    const recovered = bus.stats();
    expect(recovered.uncertain).toBe(false);
    expect(bus.replayState(recovered.nextEventId - 1).status).toBe('ok');
  });

  test('holes below the ring floor are subsumed by the floor verdict', () => {
    const bus = new WireEventBus({ capacity: 4, maxBytes: 64 * 1024, maxEventBytes: 256 });
    bus.emit('message.part.updated', { part: { output: 'y'.repeat(512) } }, '/a'); // hole id 1
    for (let i = 0; i < 8; i += 1) bus.emit('message.updated', { i }, '/a'); // ids 2..9, ring 6..9
    const stats = bus.stats();
    expect(stats.uncertain).toBe(false);
    // Old cursor below the floor still verdicts gap via the floor itself.
    expect(bus.replayState(1).status).toBe('gap');
    expect(bus.replayState(6).status).toBe('ok');
  });
});

describe('RingEventBus replayState', () => {
  test('restart when the cursor is at or after nextEventId', () => {
    const bus = new WireEventBus({ capacity: 8 });
    bus.emit('message.updated', { i: 1 }, '/a');
    expect(bus.replayState(5).status).toBe('restart');
    expect(bus.replayState(2).status).toBe('restart');
    expect(bus.stats().resyncRecommended).toBe(2);
  });

  test('empty ring does not crash and classifies volatile-only omp cursors as ok', () => {
    const bus = new OmpEventBus();
    bus.publish('omp.notice.raised', { a: 1 }, { directory: '/a', durable: false });
    bus.publish('omp.session.settled', { b: 2 }, { directory: '/a', durable: false });
    // Ring is empty; cursor below nextEventId points at volatile ids that
    // were live-only by design — replaying newer ids is contiguous.
    expect(bus.replayState(1).status).toBe('ok');
    // Cursor at/after nextEventId is a restart, with an empty ring.
    expect(bus.replayState(9).status).toBe('restart');
  });

  test('cursor predating an evicted-all ring verdicts gap', () => {
    const bus = new OmpEventBus({ capacity: 2 });
    for (let i = 0; i < 5; i += 1) bus.publish('omp.notice.raised', { i }, { directory: '/a', durable: true });
    expect(bus.replay.length).toBe(2);
    expect(bus.replayState(1).status).toBe('gap');
    expect(bus.replayState(3).status).toBe('ok');
  });

  test('epoch identifies the bus instance', () => {
    expect(new WireEventBus().epoch).not.toBe(new WireEventBus().epoch);
    const bus = new OmpEventBus();
    expect(bus.stats().epoch).toBe(bus.epoch);
  });
});

describe('RingEventBus subscribeSince boundaries', () => {
  test('listener emitting during replay neither misses nor duplicates events', () => {
    const bus = new WireEventBus({ capacity: 64 });
    bus.emit('message.updated', { i: 1 }, '/a');
    bus.emit('message.updated', { i: 2 }, '/a');
    const seen: number[] = [];
    let reentered = false;
    bus.subscribeSince(0, (entry) => {
      seen.push(Number(entry.envelope.properties.i));
      if (!reentered) {
        reentered = true;
        bus.emit('message.updated', { i: 99 }, '/a'); // synchronous reentrant emit
      }
    });
    expect(seen).toEqual([1, 2, 99]);
  });

  test('a reentrant emit that compacts the ring mid-walk cannot skip a replay entry', () => {
    // Regression: subscribeSince used to index into the live array — a
    // listener emit that pushed #head past the compaction threshold spliced
    // the array mid-iteration and silently skipped a still-retained event
    // (observed: seen [256,258] with 257 in the ring).
    const bus = new WireEventBus({ capacity: 2 });
    for (let i = 0; i < 257; i += 1) bus.emit('message.updated', { i }, '/a');
    const seen: number[] = [];
    let reentered = false;
    bus.subscribeSince(0, (entry) => {
      seen.push(entry.eventId);
      if (!reentered) {
        reentered = true;
        // Evict one more entry while the replay walk is inside the ring —
        // the compaction splice must not disturb the snapshot iteration.
        bus.emit('message.updated', { i: 999 }, '/a');
      }
    });
    expect(seen).toEqual([256, 257, 258]);
    expect(bus.replay.map((entry) => entry.eventId)).toEqual([257, 258]);
  });

  test('listener emitting during the pending flush keeps pending order', () => {
    const bus = new WireEventBus({ capacity: 64 });
    bus.emit('message.updated', { i: 1 }, '/a');
    const seen: number[] = [];
    let emitted = false;
    bus.subscribeSince(0, (entry) => {
      seen.push(Number(entry.envelope.properties.i));
      if (!emitted && seen.length === 1) {
        // Fires while the first replayed entry is being delivered: buffered,
        // then flushed after the replay walk — before any direct delivery.
        bus.emit('message.updated', { i: 50 }, '/a');
        emitted = true;
      }
    });
    expect(seen).toEqual([1, 50]);
  });

  test('a reentrant emit during the pending flush cannot overtake older pending entries', () => {
    const bus = new WireEventBus({ capacity: 64 });
    bus.emit('message.updated', { i: 1 }, '/a');
    bus.emit('message.updated', { i: 2 }, '/a');
    const seen: number[] = [];
    const emitted = new Set<number>();
    bus.subscribeSince(0, (entry) => {
      seen.push(Number(entry.envelope.properties.i));
      if (!emitted.has(50) && entry.envelope.properties.i === 1) {
        // Two live events land mid-replay and queue into pending.
        emitted.add(50);
        bus.emit('message.updated', { i: 50 }, '/a');
        bus.emit('message.updated', { i: 60 }, '/a');
      }
      if (!emitted.has(70) && entry.envelope.properties.i === 50) {
        // Reentrant emit while the pending window is flushing: without the
        // pending-window guard it would jump ahead of the still-queued 60.
        emitted.add(70);
        bus.emit('message.updated', { i: 70 }, '/a');
      }
    });
    expect(seen).toEqual([1, 2, 50, 60, 70]);
  });

  test('listener throw during replay unregisters the subscriber', () => {
    const bus = new WireEventBus({ capacity: 64 });
    bus.emit('message.updated', { i: 1 }, '/a');
    bus.emit('message.updated', { i: 2 }, '/a');
    expect(() =>
      bus.subscribeSince(0, (entry) => {
        if (entry.eventId === 1) throw new Error('listener boom');
      }),
    ).toThrow('listener boom');
    expect(bus.subscribers.size).toBe(0);
    // The bus itself keeps working for later subscribers.
    const seen: number[] = [];
    bus.subscribeSince(0, (entry) => seen.push(entry.eventId));
    expect(seen).toEqual([1, 2]);
  });
});

describe('estimateJsonValueSize', () => {
  test('bounded traversal verdicts over-budget instead of walking forever', () => {
    const wide: string[] = [];
    for (let i = 0; i < 100_000; i += 1) wide.push(`entry-${i}`);
    expect(estimateJsonValueSize(wide, 1024)).toBeNull();
    expect(estimateJsonValueSize({ ok: true }, 1024)).toBeGreaterThan(0);
    expect(estimateJsonValueSize('x'.repeat(2000), 1024)).toBeNull();
  });

  test('object keys count toward the estimate — wide records cannot undercount', () => {
    const bare = estimateJsonValueSize({ '': 'v' }, 4096);
    const keyed = estimateJsonValueSize({ kkkkkkkkkk: 'v' }, 4096);
    expect(keyed).not.toBeNull();
    expect(bare).not.toBeNull();
    expect(keyed! - bare!).toBe(10);
  });

  test('a wide record whose serialization exceeds the byte cap never enters the ring', () => {
    const bus = new WireEventBus({ capacity: 16, maxBytes: 100, maxEventBytes: 64 * 1024 });
    // ~600 bytes serialized: under the per-event limit but over the ring's
    // byte cap — it must evict immediately rather than count as ~34.
    bus.emit('message.part.updated', { sessionID: 's', part: { id: 'p', output: 'y'.repeat(512) } }, '/a');
    expect(bus.replay).toHaveLength(0);
    expect(bus.stats().retainedBytes).toBeLessThanOrEqual(100);
    bus.emit('message.updated', { i: 1 }, '/a');
    expect(bus.replay.map((entry) => entry.eventId)).toEqual([2]);
  });
});
