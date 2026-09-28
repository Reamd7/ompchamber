/**
 * 全局消息流 hub：server 进程内的单例事件源，维护一条到 OpenCode
 * `/global/event` 的上游 SSE 连接（经 createUpstreamSseReader），
 * 把事件扇出给订阅者（global WS bridge 等），并保留一段有界
 * replay 环供断线客户端按 Last-Event-ID 续传。
 *
 * 核心不变量（docs/plan.md §5.4）：replay 环受条数与字节双上限约束；
 * 上游重启（epoch 变化）或 resync 控制帧使全部保留帧与客户端游标
 * 失效；generation 计数使 stop() 后旧 reader 的迟到事件全部被丢弃。
 */

import { createUpstreamSseReader } from './upstream-reader.js';

// Raised from 512 → 2048 to improve recovery after brief disconnects during
// long-running agent sessions where many events accumulate quickly.
/** replay 环最多保留的事件条数（docs/plan.md §5.4 的条数上限）。 */
const MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT = 2048;
// Byte cap alongside the entry cap (docs/plan.md §5.4): count-only trimming
// still lets a few huge tool-output events pin unbounded memory. The
// estimate is serialized UTF-16 units + per-node overhead, not a JS-heap
// measure.
/** replay 环的字节估算上限：与条数上限双保险，防少数超大事件钉死内存。 */
const MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES = 8 * 1024 * 1024;
/** 体积估算中每个数组/对象节点的固定开销（按 UTF-16 单位计）。 */
const ESTIMATE_NODE_OVERHEAD = 8;
/** 体积估算最多遍历的节点数，超出即返回钳制值，防病态结构拖垮估算本身。 */
const ESTIMATE_MAX_NODES = 2048;
/** 体积估算的钳制上限：遍历节点数或累计值越限时直接返回该值。 */
const ESTIMATE_CLAMP_BYTES = 32 * 1024 * 1024;

/**
 * Non-empty-string arm check for untrusted boundary values. The prototype
 * tag discriminates without `typeof` narrowing (repo isString idiom).
 */
/** 不可信边界值收敛：非字符串或空串返回 null（不依赖 typeof 窄化）。 */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

/** Bounded conservative serialized-size estimate (UTF-16 units). */
/**
 * 保守估算 payload 的序列化体积（UTF-16 单位，含键名与节点开销）。
 * 用显式栈做迭代遍历，避免深结构递归爆栈；节点数或累计值越限时
 * 返回钳制值——淘汰逻辑只需要"足够大"的近似，不需要精确字节数。
 */
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

/**
 * 创建全局消息流 hub（每个 server 进程一个）。
 *
 * 参数：buildOpenCodeUrl/getOpenCodeAuthHeaders/fetchImpl 描述上游
 * 连接；upstreamStallTimeoutMs/upstreamReconnectDelayMs 透传给 SSE
 * 读取器；replayLimit/replayMaxBytes 控制 replay 环双上限（测试可调小）。
 *
 * 返回 { start, stop, isConnected, hasConnected, subscribeEvent,
 * subscribeStatus, tailEventId, replayAfter, getStats }：事件订阅收到
 * 归一化事件（envelope/payload/directory/eventId/eventName）；状态订阅
 * 收到 connect/disconnect/restart/initial-error/error；replayAfter
 * 以 { status: 'ok'|'gap', events } 应答续传请求，gap 表示必须 resync。
 */
export function createGlobalMessageStreamHub({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  fetchImpl = fetch,
  upstreamStallTimeoutMs,
  upstreamReconnectDelayMs,
  replayLimit = MESSAGE_STREAM_GLOBAL_REPLAY_LIMIT,
  replayMaxBytes = MESSAGE_STREAM_GLOBAL_REPLAY_MAX_BYTES,
}) {
  // 事件订阅者集合；fanout 时复制遍历，单个订阅者抛错互不影响。
  const eventSubscribers = new Set();
  // 状态订阅者集合（connect/disconnect/restart/error）。
  const statusSubscribers = new Set();
  /** Retained replay: `[{ event, bytes }]`, oldest first — head-indexed, so
   *  eviction never pays a linear shift() per entry (docs/plan.md §5.1). */
  let replay = [];
  let replayHead = 0;
  let replayBytes = 0;

  // 当前上游连接的 AbortController（stop 时中止）。
  let controller = null;
  // 活动的 SSE 读取器实例；null 表示 hub 处于停止状态。
  let reader = null;
  // 上游当前是否连接（onConnect/onDisconnect 维护）。
  let connected = false;
  // 本轮 start 后是否成功连接过（用于区分 initial-error 与 error）。
  let everConnected = false;
  // 最近一次 buildUrl 是否抛错（区分"服务不可用"与"连接失败"上报）。
  let buildUrlFailed = false;
  /** Bumped on every stop()/start(): late events from a stopped reader are dropped. */
  let generation = 0;
  /** Upstream boot identity when the upstream advertises one (omp host). */
  let upstreamEpoch = null;
  /** (generation, epoch) a restart was already broadcast for: the same
   *  connect's resync control frame must not notify clients twice. */
  let restartBroadcastGen = -1;
  let restartBroadcastEpoch = null;
  // 累计统计：淘汰条目/字节、resync 次数、上游重启次数。
  const stats = { evictedEntries: 0, evictedBytes: 0, resyncs: 0, restarts: 0 };

  /**
   * 调用单个订阅者并吞掉其失败：同步异常与异步 rejection 都只
   * console.warn——一个订阅者损坏不得中断其余订阅者的 fanout。
   */
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

  /** 把状态通知扇出给全部状态订阅者（复制遍历，允许中途退订）。 */
  const notifyStatus = (status) => {
    for (const subscriber of Array.from(statusSubscribers)) {
      notifySubscriber('status', subscriber, status);
    }
  };

  /** 把归一化事件扇出给全部事件订阅者（复制遍历，允许中途退订）。 */
  const notifyEvent = (normalized) => {
    for (const subscriber of Array.from(eventSubscribers)) {
      notifySubscriber('event', subscriber, normalized);
    }
  };

  // 头索引达到该阈值时一次性 splice 压缩数组，摊销逐条淘汰的成本。
  const REPLAY_COMPACT_THRESHOLD = 1024;

  /** Newest retained event id, or null when nothing live remains in the ring. */
  /** 环内最新保留事件的 id；环内无存活条目（全淘汰/清空）时为 null。 */
  const retainedTailEventId = () =>
    replay.length > replayHead ? replay[replay.length - 1].event?.eventId ?? null : null;

  /**
   * 清空 replay 环（epoch 变化或上游 resync 时调用）：
   * 存活条目计入淘汰统计，返回 reason 供调用方记录清理原因。
   */
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

  /**
   * 追加事件到 replay 环并执行双上限淘汰：超过条数或字节上限时
   * 从最旧的头部逐条淘汰并记账；头索引达到压缩阈值后整体归零。
   */
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

  /**
   * 把上游事件归一化为内部形状：directory 缺省 'global'，
   * eventId/eventName 经 stringOrNull 收敛（缺失为 undefined），
   * envelope 原样透传供订阅者读取原始元数据。
   */
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

  /**
   * 启动 hub（已有 reader 时直接返回）。以 replay 尾部游标 + 记住的
   * epoch 续连上游，避免重复摄入整个环；generation +1 使旧 reader
   * 的迟到回调全部失效。
   */
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
    const replayTail = retainedTailEventId();
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
        restartBroadcastGen = gen;
        restartBroadcastEpoch = epoch;
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
            // A connect-time resync is the host's verdict on the stale-epoch
            // echo — the epoch-change handler already broadcast the restart
            // for this (generation, epoch); sending another resync to every
            // client would double the reconcile burst. Mid-stream synthesized
            // resyncs (over-budget drops) and later verdicts still notify.
            const duplicateRestart =
              event.synthesized !== true &&
              gen === restartBroadcastGen &&
              upstreamEpoch === restartBroadcastEpoch;
            if (!duplicateRestart) {
              notifyStatus({ type: 'restart', epoch: upstreamEpoch, reason: 'upstream-resync' });
            }
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
  /**
   * 停止 hub：bump generation 丢弃旧 reader 的迟到事件、中止
   * controller、清空连接状态。replay 环有意保留——重连的浏览器
   * 客户端仍需其后续段；epoch 安全由下一次 start 的上游校验兜底。
   */
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
    /** 上游当前是否处于连接状态。 */
    isConnected() {
      return connected;
    },
    /** 本轮 start 后是否至少成功连接过一次。 */
    hasConnected() {
      return everConnected;
    },
    /** 订阅事件流；返回退订函数。 */
    subscribeEvent(subscriber) {
      eventSubscribers.add(subscriber);
      return () => {
        eventSubscribers.delete(subscriber);
      };
    },
    /** 订阅状态流；返回退订函数。 */
    subscribeStatus(subscriber) {
      statusSubscribers.add(subscriber);
      return () => {
        statusSubscribers.delete(subscriber);
      };
    },
    /** Newest retained event id, or null when the replay is empty. */
    /** 最新保留事件 id；replay 环为空时为 null。 */
    tailEventId() {
      return retainedTailEventId();
    },
    /**
     * Replay after `eventId`. `gap` means the requested id is no longer
     * retained (evicted, cleared, or from another boot): the caller must
     * resync, never serve a silent suffix (docs/plan.md §5.4). Only a
     * missing cursor (fresh client) is `ok` with no events.
     */
    /** 从 eventId 之后续传：ok 返回后续事件，gap 表示必须 resync（详见上方英文）。 */
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
    /** 运行统计快照：保留/淘汰、resync/restart、epoch 与订阅者数量。 */
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
