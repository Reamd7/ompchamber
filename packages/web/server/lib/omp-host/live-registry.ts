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
//   `failed` may re-enter `evicting` only through `retryEvict` — the engine's
//   cooldown-gated re-dispose path, never a fresh writer.
// - `failed` tombstones retain nothing but metadata; the SDK object a failed
//   disposal could not release is retained by the engine's dispose-promise
//   chain (and the record's `retryDispose` closure), deliberately, so the
//   same file cannot gain a second writer.
// - Idle TTL bookkeeping uses the injected monotonic clock (default
//   performance.now); wall clock stays out of TTL math (plan §4.2).
/**
 * LiveSessionRegistry —— 宿主持有的 live AgentSession 生命周期注册表
 * （docs/plan.md §3.2-3.4，phase 2）。
 *
 * 单宿主进程、以转录文件为权威路径下的保证：
 * - 每个会话容器以 `normalize(directory)\0sessionID` 为复合键：同一
 *   session id 出现在两个目录即是两条记录，彼此永不满足对方的查找；
 * - 每键操作闸门（withOperation）串行化 materialize/evict/move/
 *   delete/reload，防止两个异步调用方在同键上并发通过检查后同时行动；
 * - 驱逐状态机 materializing → live → evicting →（cold | failed），
 *   evicting/failed 阻断新写者，失败的 dispose 变成可观测的隔离墓碑
 *   （failed），仅能经 retryEvict（引擎冷却门控的重处置路径）重返
 *   evicting；
 * - TTL 记账只用注入的单调时钟（默认 performance.now），墙钟不参与
 *   TTL 数学（plan §4.2）。
 */

import { normalizeDirectoryKey } from './registry.ts';

/** Composite session key: `normalize(directory)\0sessionID` (plan §3.2). */
/** 由目录与会话 id 组装复合键：`normalize(directory)\0sessionID`。 */
export const sessionKey = (directory: string, sessionId: string): string =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionId}`;

/** 会话生命周期状态：materializing（物化中）→ live（存活）→
 *  evicting（驱逐中）→ cold（已释放）/ failed（隔离墓碑）。 */
export type LiveSessionState = 'materializing' | 'live' | 'evicting' | 'failed';

/** Retryable rejection: the key is mid-eviction or quarantined (plan §3.4). */
/** 可重试的拒绝：目标键正处于驱逐中或被隔离（plan §3.4），
 *  宿主将其映射为 HTTP 409。 */
export class SessionBusyError extends Error {
  /** 拒绝原因码；host-shutting-down 表示宿主正在整体关停。 */
  readonly code: 'session-evicting' | 'session-failed' | 'host-shutting-down';
  /** Wire `sessionID` when the refusal is session-scoped (host maps 409). */
  readonly sessionId: string | null;
  /** 构造拒绝错误：固化 code 与（可选的）session-scoped sessionId。 */
  constructor(code: SessionBusyError['code'], message: string, sessionId: string | null = null) {
    super(message);
    this.name = 'SessionBusyError';
    this.code = code;
    this.sessionId = sessionId;
  }
}

/** 一个 live 会话的注册表记录：状态机、宿主 payload、TTL 记账与
 *  驱逐/隔离所需的一次性字段全部挂在这里。 */
export interface LiveRecord<TPayload> {
  /** 复合键 `normalize(directory)\0sessionID`。 */
  readonly key: string;
  /** 归一化后的目录键（正斜杠、大写盘符）。 */
  readonly directory: string;
  /** 原始 session id（跨目录不唯一，见 bySessionId）。 */
  readonly sessionId: string;
  /** 当前生命周期状态（由本注册表的状态迁移方法维护）。 */
  state: LiveSessionState;
  /** Host payload; nulled when eviction starts (host refs drop first). */
  payload: TPayload | null;
  /** Engine's single disposal promise for this record (plan §3.4 step 4). */
  disposePromise: Promise<'disposed' | 'failed'> | null;
  /** Monotonic (registry clock). Refreshed only by user-visible use. */
  lastUsedAt: number;
  /** 引擎侧进行中的 live 操作计数（plan §3.2，sweeper 的驱逐守卫信号）。 */
  /** Active withLiveSession operations (plan §3.2). */
  inFlight: number;
  /** 记录创建时刻（注册表单调时钟）。 */
  readonly createdAt: number;
  /**
   * Set when a disposal failed: quarantine reason for diagnostics.
   * `attempts` counts every settled dispose call — the initial one plus
   * each cooldown-gated retry — so the sweeper can stop resurrecting a
   * permanently failing SDK object.
   */
  failure: { reason: string; at: number; attempts: number } | null;
  /**
   * Engine-installed re-dispose closure for the failed tombstone: retains
   * the SDK object the failed dispose could not release so a retry can
   * finish the job — and so the file cannot gain a second writer.
   */
  retryDispose: (() => Promise<void>) | null;
}

/** 注册表统计快照：各状态计数与不平衡 endUse 的计数器。 */
export interface LiveRegistryStats {
  /** materializing 状态的记录数。 */
  materializing: number;
  /** live 状态的记录数。 */
  live: number;
  /** evicting 状态的记录数。 */
  evicting: number;
  /** failed（隔离墓碑）状态的记录数。 */
  failed: number;
  /** Total non-cold records (any state). */
  total: number;
  /**
   * endUse calls that arrived with nothing in flight — an unbalanced
   * begin/end pair is a bug; count it instead of silently clamping.
   */
  inFlightUnderflow: number;
}

/** LiveSessionRegistry 构造选项。 */
export interface LiveSessionRegistryOptions {
  /** 单调时钟注入点；默认 performance.now，测试可替换（plan §4.2）。 */
  /** Monotonic clock; test-injectable (plan §4.2). */
  now?: () => number;
}

/**
 * 按（目录, session id）组织 live 会话记录的注册表：复合键存储、
 * per-key 操作闸门、materialize/evict 状态机与 TTL/touch 记账。
 * TPayload 是宿主挂在每条记录上的引擎 payload 类型。
 */
export class LiveSessionRegistry<TPayload> {
  /** 注入的单调时钟（构造时固定，TTL 与时间戳统一来源）。 */
  readonly now: () => number;
  /** 复合键 → 记录；cold 键不占行，failed 保留为墓碑。 */
  #records = new Map<string, LiveRecord<TPayload>>();
  /** 复合键 → 排队中的操作 Promise（withOperation 的串行队列）。 */
  #gates = new Map<string, Promise<unknown>>();
  /** 无配对 beginUse 的 endUse 次数（记账 bug 的观测计数器）。 */
  #inFlightUnderflow = 0;

  /** 构造：固定注入的单调时钟（默认 performance.now）并初始化空表。 */
  constructor({ now }: LiveSessionRegistryOptions = {}) {
    this.now = now ?? (() => performance.now());
  }

  /** 读取任意状态的记录；键为 cold（从未物化或已释放）时返回 null。 */
  /** Record in any state, or null when the key is cold. */
  get(directory: string, sessionId: string): LiveRecord<TPayload> | null {
    return this.#records.get(sessionKey(directory, sessionId)) ?? null;
  }

  /** 按复合键直接取记录；cold 键返回 null。 */
  byKey(key: string): LiveRecord<TPayload> | null {
    return this.#records.get(key) ?? null;
  }

  /** 仅当记录处于 live 且 payload 仍挂载时返回它，否则返回 null。 */
  /** Only a `live` record with its payload attached. */
  getLive(directory: string, sessionId: string): LiveRecord<TPayload> | null {
    const record = this.get(directory, sessionId);
    return record && record.state === 'live' && record.payload !== null ? record : null;
  }

  /** 跨目录按 session id 查唯一记录：cold 返回 null；同一 id 出现在两个
   *  目录（几乎不可能）返回 undefined 表示歧义，调用方必须按未命中处理。 */
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

  /** 打开 materializing 状态（新建或复用既有记录）；键处于 evicting 或
   *  failed 时抛 SessionBusyError 拒绝新写者。 */
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
      retryDispose: null,
    };
    this.#records.set(key, record);
    return record;
  }

  /** materializing → live：挂载 payload 并刷新 TTL。 */
  /** materializing → live. */
  commitMaterialize(record: LiveRecord<TPayload>, payload: TPayload): LiveRecord<TPayload> {
    record.payload = payload;
    record.state = 'live';
    record.lastUsedAt = this.now();
    return record;
  }

  /** setup 失败时 materializing → cold；调用方必须已释放全部资源
   *  （manager、订阅、域句柄），这里只删去重行（plan §3.3）。 */
  /**
   * materializing → cold after a failed setup. The caller must have already
   * released every resource (manager, subscription, domain handles) — this
   * only drops the dedup row (plan §3.3).
   */
  failMaterialize(record: LiveRecord<TPayload>): void {
    if (record.state !== 'materializing') return;
    this.#records.delete(record.key);
  }

  /** 在单键上串行化一个变更边界：操作按序排队，被拒绝的操作不会毒化
   *  队列；对已持有的键重入属编程错误（会死锁）。 */
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

  /** live → evicting；记录不处于 live 时返回 false。 */
  /** live → evicting. Returns false when the record is not live. */
  beginEvict(record: LiveRecord<TPayload>): boolean {
    if (record.state !== 'live') return false;
    record.state = 'evicting';
    return true;
  }

  /** failed → evicting 的有界重处置尝试（plan §3.4 恢复规则）：隔离
   *  墓碑并非终身——瞬时失败（Windows 文件锁、杀毒扫描）由 sweeper
   *  冷却门控重试，而非永久 409。 */
  /**
   * failed → evicting for a bounded re-dispose attempt (plan §3.4 recovery
   * rule): the tombstone is not a life sentence — a transient dispose
   * failure (Windows file lock, AV scan) gets cooldown-gated retries from
   * the sweeper instead of a permanent 409.
   */
  retryEvict(record: LiveRecord<TPayload>): boolean {
    if (record.state !== 'failed') return false;
    record.state = 'evicting';
    return true;
  }

  /** evicting 收尾：成功 → cold（删行）；失败 → failed 墓碑（键保持
   *  阻塞，仅进程重启/冷却重试/手动恢复可清除，plan §3.4）。 */
  /**
   * evicting → cold on success; evicting → failed tombstone otherwise (the
   * key stays blocked; only a process restart, a cooldown-gated retry, or
   * explicit manual recovery clears it — plan §3.4).
   */
  finishEvict(record: LiveRecord<TPayload>, ok: boolean, reason?: string): void {
    if (record.state !== 'evicting') return;
    if (ok) {
      this.#records.delete(record.key);
      return;
    }
    record.state = 'failed';
    record.failure = {
      reason: reason ?? 'dispose failed',
      at: this.now(),
      attempts: (record.failure?.attempts ?? 0) + 1,
    };
    record.payload = null;
  }

  /** 用户可见的使用：刷新 TTL（单调时钟）。 */
  /** User-visible use: refresh the TTL (monotonic clock). */
  touch(record: LiveRecord<TPayload>): void {
    record.lastUsedAt = this.now();
  }

  /** 计一次进行中的 live 操作（sweeper 的驱逐守卫）。 */
  /** Count one in-flight live operation (sweeper guard). */
  beginUse(record: LiveRecord<TPayload>): void {
    record.inFlight += 1;
  }

  /** 与 beginUse 配对的收尾；无配对时不归零而是累计 underflow 计数。 */
  endUse(record: LiveRecord<TPayload>): void {
    if (record.inFlight <= 0) {
      // An endUse without a beginUse is an accounting bug — observable in
      // stats() rather than silently clamped to zero.
      this.#inFlightUnderflow += 1;
      return;
    }
    record.inFlight -= 1;
  }

  /** 给 sweeper 的有序快照拷贝；遍历期间可安全变更底层表。 */
  /** Snapshot for the sweeper (ordered copy; safe to iterate while mutating). */
  snapshot(): LiveRecord<TPayload>[] {
    return [...this.#records.values()];
  }

  /** 仍持有 live/materializing 记录的目录集合（判断目录是否还活跃）。 */
  liveDirectories(): Set<string> {
    const dirs = new Set<string>();
    for (const record of this.#records.values()) {
      if (record.state === 'live' || record.state === 'materializing') dirs.add(record.directory);
    }
    return dirs;
  }

  /** 统计快照：各状态计数、非 cold 总数与 endUse underflow 计数。 */
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
    return { materializing, live, evicting, failed, total: this.#records.size, inFlightUnderflow: this.#inFlightUnderflow };
  }

  /** 测试/诊断用：手动摘除隔离墓碑（手动恢复路径）；非 failed 返回 false。 */
  /** Test/diagnostics: drop a quarantine tombstone (manual recovery path). */
  clearFailure(directory: string, sessionId: string): boolean {
    const record = this.get(directory, sessionId);
    if (!record || record.state !== 'failed') return false;
    this.#records.delete(record.key);
    return true;
  }
}
