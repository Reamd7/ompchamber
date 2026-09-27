/**
 * omp 宿主的事件总线模块（中文说明）。
 *
 * 核心是 `RingEventBus`：带条目数与字节双上限的有界重放总线（docs/plan.md
 * §5.1）。`emit` 通过 `durable` 标志区分持久/易失事件：持久条目进入重放环，
 * 供 SSE Last-Event-ID 续订；易失条目（loader、toast、控制帧）只送达在线
 * 订阅者——重放它们会复活过期的 UI 状态。wire 总线是全持久的
 * `RingEventBus`，字节级线上行为（`{id,type,properties}` 信封、单调 id、
 * 目录路由）保持不变。
 *
 * `OmpEventBus` 是唯一的 omp 原生事件通道（spec 05 §5.2，master D6-R1），
 * 信封为 { id, type, directory, sessionID?, schemaVersion, createdAt,
 * payload }，使用进程级全局单调 id；持久条目与 wire 环共用同一套容量约束。
 *
 * 注意：本模块中所有「bytes」数值都是 UTF-16 码元加每节点固定开销的保守
 * 序列化估算，不是 JS 堆测量值，不得当作堆指标上报（plan §5.1/§9.1）。
 */

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

/** wire 总线重放环的条目数上限（可保留的持久事件条数）。 */
const WIRE_REPLAY_CAPACITY = 2048;
/** omp 原生事件总线重放环的条目数上限（比 wire 环更小）。 */
const OMP_REPLAY_CAPACITY = 512;
// Provisional byte budgets (plan D4: concrete values to be re-tuned from
// phase-0 event-size sampling). Wire events are dominated by
// message.part.updated snapshots; the single-event budget must admit large
// tool outputs without letting one event monopolize the ring.
/** wire 重放环的字节预算（暂定值，待 phase-0 事件体积采样后重调）。 */
const WIRE_REPLAY_MAX_BYTES = 8 * 1024 * 1024;
/** omp 重放环的字节预算（暂定值，同上）。 */
const OMP_REPLAY_MAX_BYTES = 2 * 1024 * 1024;
/** 单事件预算系数：maxEventBytes 默认取 maxBytes 的 1/4（下限 256KiB）。 */
const SINGLE_EVENT_BUDGET_FRACTION = 4;

/** 累积被逐出槽位达到该阈值后，压缩一次环形存储数组。 */
/** Compact the ring storage once this many evicted slots accumulate. */
const RING_COMPACT_THRESHOLD = 256;
/** 追踪的 hole 区间数上限；超出后 gap 状态坍缩为 `uncertain`。 */
/** Max tracked hole ranges before the gap state collapses to `uncertain`. */
const MAX_HOLES = 64;

/** 可 JSON 序列化的值：SSE 传输信封的 payload 契约。 */
/** Serializable JSON value: the envelope payload contract for SSE transport. */
export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/** wire 总线事件信封：OpenCode 兼容的 {id,type,properties} 形状。 */
export interface WireEventEnvelope {
  // 事件 id（总线单调 id 的字符串形式）。
  id: string;
  // 事件类型名（如 message.updated）。
  type: string;
  // 不透明负载；总线只透传、从不解析。
  properties: Record<string, JsonValue>;
}

/** omp 原生事件信封：携带目录/会话定位与 schema 版本的完整元数据。 */
export interface OmpEventEnvelope {
  // 进程级全局单调递增的事件 id。
  id: number;
  // 已注册的公开事件名（omp.<domain>.<event>）。
  type: string;
  // 事件所属的会话目录（作用域路由键）。
  directory: string;
  // 可选：事件关联的会话 id。
  sessionID?: string;
  // 信封 schema 版本（消费端兼容判断用）。
  schemaVersion: string;
  // 发布时刻的 epoch 毫秒时间戳。
  createdAt: number;
  // 不透明 JSON 对象负载（不得携带 directory/sessionID）。
  payload: object;
}

/** 总线内部条目：信封外加路由与持久化元数据（追加后冻结）。 */
export interface BusEntry<TEnvelope = WireEventEnvelope> {
  // 总线分配的数值单调 id（wire 信封 id 的数值源）。
  eventId: number;
  // 对外广播的事件信封。
  envelope: TEnvelope;
  // 作用域路由用的目录键（空串表示全局）。
  directory: string;
  // 是否进入重放环（false = 仅送达在线订阅者）。
  durable: boolean;
  // 追加时捕获的序列化体积估算；仅重放环内条目携带。
  /** Serialized-size estimate captured at append; ring entries only. */
  size?: number;
}

/** 总线订阅回调：接收每条广播条目（含易失条目）。 */
export type BusListener<TEnvelope = WireEventEnvelope> = (entry: BusEntry<TEnvelope>) => void;

/** Last-Event-ID 续订的重放判定结果。 */
export type ReplayState =
  // 现有重放环可完整衔接客户端游标。
  | { status: 'ok' }
  // 游标属于另一次启动（id 已重置），必须整体重同步。
  | { status: 'restart' }
  // 存在空洞或游标早于环下限；oldest 为当前可证明的最早持久 id。
  | { status: 'gap'; oldest: number };

/** 总线诊断快照（plan §9.1：计数器直读，不拷贝条目）。 */
export interface BusStats {
  // 每次启动唯一的总线身份标识，用于 restart 检测。
  /** Per-bus identity; stable for the bus's lifetime, differs per boot. */
  epoch: string;
  // 下一个待分配的事件 id。
  nextEventId: number;
  // 当前保留的持久条目数（O(1) 读取）。
  /** Retained durable entries (O(1)). */
  retainedEntries: number;
  // 保留条目的序列化体积估算（UTF-16 单位，申报口径）。
  /** Estimated retained serialized size (UTF-16 units; declared estimate). */
  retainedBytes: number;
  // 重放环条目数上限。
  capacity: number;
  // 重放环字节预算上限。
  maxBytes: number;
  // 单个持久事件的体积预算。
  maxEventBytes: number;
  // 为满足上限而逐出的持久条目累计数。
  /** Durable entries evicted to satisfy the caps. */
  evictedEntries: number;
  // 逐出条目的体积估算累计。
  evictedBytes: number;
  // 因超出单事件预算而跳过重放的持久事件数。
  /** Durable events skipped from replay for exceeding maxEventBytes. */
  overBudgetEvents: number;
  // 追踪中的 hole 区间数（仍在下限之上的被拒 id 区间）。
  /** Tracked hole ranges (rejected id intervals still above the floor). */
  holes: number;
  // hole 追踪溢出标记：置位后所有游标都必须重同步。
  /** True when hole tracking overflowed: every cursor must resync. */
  uncertain: boolean;
  // replayState 判定为非 ok 的累计次数。
  /** replayState verdicts that were not `ok`. */
  resyncRecommended: number;
  // 当前在线订阅者数量。
  subscribers: number;
}

/** RingEventBus 构造选项（各字段均有按总线种类区分的默认值）。 */
interface RingEventBusOptions {
  // 重放环条目数上限。
  capacity?: number;
  // 重放环字节预算。
  maxBytes?: number;
  // 单持久事件预算；超出者跳过重放环。
  /** Single-durable-event budget; over-budget events skip the ring. */
  maxEventBytes?: number;
  // emit 未显式指定 durable 时的默认持久性。
  durableDefault?: boolean;
}

/** 被拒绝/逐出形成的 id 空洞区间（闭区间；随下限推进被吸收）。 */
interface HoleRange {
  // 区间起始 id（含）。
  from: number;
  // 区间结束 id（含）。
  to: number;
}

/** 体积估算：每个数组/对象节点的固定开销（UTF-16 单位）。 */
const ESTIMATE_NODE_OVERHEAD = 8;
/** 体积估算：每个字符串标量的固定开销（不含内容长度）。 */
const ESTIMATE_STRING_OVERHEAD = 4;
/** 遍历防护上限：对不可信 payload 不做无界遍历（plan §5.1）。 */
/** Traversal guard for untrusted payloads (plan §5.1: no unbounded walks). */
const ESTIMATE_MAX_NODES = 4096;

/**
 * 保守估算一个 `JsonValue` 的序列化体积（UTF-16 码元 + 每节点开销）。
 * 有界遍历：超过 maxNodes 个节点、或累计估算超过 byteCutoff 即停止并
 * 返回 `null`（表示超预算）；绝不递归字符串化。联合分支按结构（null /
 * 数组 / 对象 / 标量）判定，不依赖 typeof。
 */
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

/**
 * 有界重放事件总线：持久条目进入环形缓冲供 Last-Event-ID 续订，易失条目
 * 仅广播给在线订阅者。条目数与字节双上限、超预算事件跳环记 hole、全局
 * 保守的 gap 检测——详见模块头（plan §5.1/§5.2）。
 */
export class RingEventBus<TEnvelope = WireEventEnvelope> {
  /** 重放环条目数上限。 */
  capacity: number;
  /** 重放环字节预算上限（估算口径）。 */
  maxBytes: number;
  /** 单持久事件体积预算；超出者不进环、只记 hole。 */
  maxEventBytes: number;
  /** emit 未指定 durable 时的默认持久性。 */
  durableDefault: boolean;
  /** 每次启动唯一的身份标识，供 restart 检测比较（plan §5.2.1）。 */
  /** Per-boot identity for restart detection (plan §5.2.1). */
  readonly epoch: string;
  /** 下一个待分配的单调事件 id。 */
  nextEventId: number;
  /** 在线订阅者集合（插入序即通知序）。 */
  subscribers: Set<BusListener<TEnvelope>>;

  /** 环形存储数组：#head 之前是已逐出的死槽。 */
  #entries: Array<BusEntry<TEnvelope>> = [];
  /** 最老存活条目的下标（压缩前只前进、不搬移元素）。 */
  #head = 0;
  /** 存活条目的体积估算合计。 */
  #bytes = 0;
  /** 为满足上限逐出的条目累计数。 */
  #evictedEntries = 0;
  /** 逐出条目的体积估算累计。 */
  #evictedBytes = 0;
  /** 超出单事件预算、跳过重放环的事件累计数。 */
  #overBudgetEvents = 0;
  /** replayState 判定为非 ok 的累计次数。 */
  #resyncRecommended = 0;
  /** 追踪中的 id 空洞区间列表（升序，相邻段可合并）。 */
  #holes: HoleRange[] = [];
  /** hole 追踪溢出标记：置位期间所有游标一律判 gap。 */
  #uncertain = false;
  /** 不确定态水位线：环下限越过它后自动解除 uncertain。 */
  /** Hole-collapse watermark: ids below it stay unprovable this cycle. */
  #uncertainUntilId = 0;
  /** 迄今保证既未逐出也未被拒绝的最小持久 id。 */
  /** Smallest durable id guaranteed never evicted or rejected so far. */
  #floorId = 1;

  /** 按选项构造总线；maxEventBytes 缺省取 maxBytes/4（下限 256KiB），并生成新 epoch。 */
  constructor({ capacity = WIRE_REPLAY_CAPACITY, maxBytes = WIRE_REPLAY_MAX_BYTES, maxEventBytes, durableDefault = true }: RingEventBusOptions = {}) {
    this.capacity = capacity;
    this.maxBytes = maxBytes;
    this.maxEventBytes = maxEventBytes ?? Math.max(256 * 1024, Math.floor(maxBytes / SINGLE_EVENT_BUDGET_FRACTION));
    this.durableDefault = durableDefault;
    this.epoch = randomUUID();
    this.nextEventId = 1;
    this.subscribers = new Set();
  }

  /** 保留的持久条目（最旧在前）；每次调用分配一份拷贝。 */
  /** Retained durable entries, oldest first. Allocates a copy per call. */
  get replay(): Array<BusEntry<TEnvelope>> {
    return this.#entries.slice(this.#head);
  }

  /** O(1) 诊断访问器（plan §9.1：读计数器不拷贝条目）。 */
  /** O(1) diagnostics accessors (plan §9.1: no copying for counters). */
  get retainedCount(): number {
    return this.#entries.length - this.#head;
  }

  /** 当前保留条目的体积估算合计（O(1)）。 */
  get retainedBytes(): number {
    return this.#bytes;
  }

  /** 最新保留持久条目的 id；环空时返回 null。 */
  /** Newest retained durable id, or `null` when the ring is empty. */
  tailEventId(): number | null {
    return this.#entries.length > this.#head ? this.#entries[this.#entries.length - 1].eventId : null;
  }

  /** 生成诊断快照：epoch、容量、逐出/hole/重同步计数与订阅数。 */
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
   * 构造并广播一条事件信封（持久性默认取 durableDefault）：分配单调 id，
   * 持久条目入重放环并受上限约束，易失条目仅达在线订阅者；返回发出的信封。
   */
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
   * 先重放 eventId 大于 lastEventId 的条目、再转为在线订阅；易失条目绝不重放。
   * 订阅者在重放遍历开始前注册并缓冲期间新到事件，因此会同步 emit 的监听器
   * 既不漏收也不重复收（plan §5.3）；重放期间监听器抛错会先注销再传播。
   * 返回注销函数。directory 可选，用于按目录过滤。
   */
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
   * Last-Event-ID 续订的缺口检测（05 §5.2.1，plan §5.2）：
   * restart = 客户端 id 不小于 nextEventId（id 已重置，属另一次启动）；
   * gap = 游标所需 id 落入 hole 或早于环下限（oldest 为当前最早可证 id）；
   * ok = 保留环可证明完整衔接该游标。非 ok 判定计入 resyncRecommended。
   */
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

  /** replayState 的判定核心（不计数）：依次按重启 / 不确定 / 下限 / hole 判定。 */
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

  /** 把条目广播给全部在线订阅者；单个订阅者抛错只导致其自身被移除。 */
  protected notifySubscribers(entry: BusEntry<TEnvelope>): void {
    for (const subscriber of [...this.subscribers]) {
      try {
        subscriber(entry);
      } catch {
        this.subscribers.delete(subscriber);
      }
    }
  }

  /** 持久条目入环：先估体积，超预算则记 hole 跳环（绝不截断负载），否则追加并执行上限逐出。 */
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

  /** 逐出最老条目直到同时满足条目数与字节上限；顺带压缩死槽、吸收下限以下的 hole、老化不确定态。 */
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

  /** 记录一个被拒 id：与上一区间相邻则合并；超出 MAX_HOLES 则坍缩为 uncertain。 */
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

/** wire 总线：OpenCode 兼容信封，全部事件持久（线上行为与既有协议一致）。 */
/** Wire bus: OpenCode-compatible envelopes, everything durable (unchanged). */
export class WireEventBus extends RingEventBus<WireEventEnvelope> {
  /** 以 wire 默认容量构造（durableDefault 固定为 true）。 */
  constructor({ capacity, maxBytes, maxEventBytes }: { capacity?: number; maxBytes?: number; maxEventBytes?: number } = {}) {
    super({ capacity, maxBytes, maxEventBytes, durableDefault: true });
  }
}

/** omp 原生事件总线：全元数据信封，默认易失、显式 durable 才进重放环。 */
export class OmpEventBus extends RingEventBus<OmpEventEnvelope> {
  /** 信封携带的 schema 版本号。 */
  schemaVersion: string;

  /** 以 omp 默认容量构造；schemaVersion 缺省 '1.0'。 */
  constructor({ capacity, maxBytes, maxEventBytes, schemaVersion = '1.0' }: { capacity?: number; maxBytes?: number; maxEventBytes?: number; schemaVersion?: string } = {}) {
    super({ capacity: capacity ?? OMP_REPLAY_CAPACITY, maxBytes: maxBytes ?? OMP_REPLAY_MAX_BYTES, maxEventBytes, durableDefault: false });
    this.schemaVersion = schemaVersion;
  }

  /**
   * 发布一条 omp 原生事件（已注册名 omp.<domain>.<event>）：分配全局单调
   * id、组装完整信封，并按 scope.durable 决定是否入环。payload 不得携带
   * directory/sessionID——二者属于信封字段。返回发出的信封。
   */
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
