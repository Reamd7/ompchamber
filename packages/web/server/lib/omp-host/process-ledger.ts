// Session process monitor ledger (PLAN-session-process-monitor.md).
//
// Bash and eval tool calls run inside this host process, so every process a
// session produces is a host descendant while it lives. The SDK never reports
// spawned pids, so this ledger reconstructs ownership from observation:
// engine tool events open and close invocation windows, and a poller diffs
// the host's descendant tree against what is already tracked.
//
// Attribution is best-effort and every row carries how it was derived:
//   parent — ppid chains to an already-tracked process (forks, `&` children)
//   window — first seen while exactly one invocation window was open
//   argv   — several windows were open; the command line disambiguated
//   job    — the invocation backed an SDK async job still running
//   unattributed — spawn overlapped windows it could not be scored against;
//            kept visible and killable with its candidate session ids
//
// Honest limits (documented in DOCUMENTATION.md): POSIX `setsid`/double-fork
// daemons reparent out of the descendant tree and read as exited while still
// alive; output captured after a process detaches its fds is unreachable.
// Neither is papered over — rows show exited/uncertain rather than lying.
/**
 * 会话进程监控台账（中文摘要）：bash/eval 工具调用都运行在本 host 进程内，
 * 会话派生出的进程在存活期间都是 host 的后代；SDK 不上报 pid，本模块用
 * "工具事件开合调用窗口 + 轮询差分 host 后代进程树"的方式重建归属关系。
 * 归因是尽力而为的，每行数据都携带判定方式（见 ProcessAttribution）；
 * setsid/双 fork 守护进程会脱离后代树、进程脱离 fd 后的输出不可达——
 * 台账宁可显示 exited/uncertain 也不伪造状态。
 */

import type { ProcInfo, ProcStats, ProcessPlatform } from './process-platform.ts';
import { normalizeDirectoryKey } from './registry.ts';

/** 进程归属判定方式：window=仅一个调用窗口打开时首见；argv=多个窗口
 * 重叠时按命令行匹配区分；parent=ppid 链接到已跟踪进程（fork/`&` 子进程）；
 * job=该调用支撑的 SDK 异步 job 仍在运行；unattributed=无法判定（保留
 * 可见、可 kill，并携带候选会话 id）。 */
export type ProcessAttribution = 'window' | 'argv' | 'parent' | 'job' | 'unattributed';

// ---------------------------------------------------------------------------
// Wire-facing snapshot types (consumed by domain-processes routes and the UI
// zod schema in packages/ui/src/lib/api/omp.ts — keep the shapes aligned).
// ---------------------------------------------------------------------------

/** 快照中的单个 OS 进程成员：一个 pid 从首次被观测到判定退出的生命周期视图。 */
export interface OmpProcessMember {
  // 进程 id
  pid: number;
  // 父进程 id（parent 归因沿 ppid 链继承 owner）
  ppid: number;
  // 完整命令行（空格拼接；argv 归因的匹配对象）
  argv: string;
  // 可观测到的工作目录
  cwd?: string;
  // running=仍在后代树中；exited=差分判定已退出
  state: 'running' | 'exited';
  // 台账首次观测到该 pid 的时间（epoch ms）
  firstSeenAt: number;
  // 判定退出的时间；存活期间缺省
  exitedAt?: number;
  // 最近一次采样到的常驻内存（字节）
  rssBytes?: number;
  // 两次采样间隔折算出的 CPU 百分比（0-100，已按核数归一）
  cpuPercent?: number;
  // 该成员的归属判定方式
  attribution: ProcessAttribution;
  // 用户是否通过 kill 接口终止过此进程
  killedByUser?: boolean;
}

/** 快照条目：一次 bash/eval 调用（invocation）或一个未归属进程子树的聚合视图。 */
export interface OmpProcessEntry {
  /** Invocation key `${sessionID}${toolCallId}` or `proc:<pid>` for an unattributed root. */
  /** 条目主键（中文）：`${sessionID} ${toolCallId}`；未归属根为 `proc:<pid>`。 */
  key: string;
  // 所属会话 id；未归属根条目为 null
  sessionID: string | null;
  /** Sessions that owned an open window when an unattributed process appeared. */
  /** 未归属进程出现时持有打开窗口的会话 id 列表。 */
  candidateSessionIds?: string[];
  // 条目种类：bash 调用 / eval 调用 / 纯进程（未归属根）
  kind: 'bash' | 'eval' | 'process';
  // bash 命令串或 eval 摘要标签
  command: string;
  // 调用声明的工作目录
  cwd?: string;
  // killed=被用户终止；failed=调用以错误结束；exited=正常结束
  status: 'running' | 'exited' | 'killed' | 'failed';
  // 调用（或首个成员进程）的开始时间
  startedAt: number;
  // 调用结束时间；进行中缺省
  endedAt?: number;
  // 工具结果携带的退出码；可能为 null（未知）
  exitCode?: number | null;
  // 关联的 SDK 异步 job id（后台 bash/eval 任务）
  jobId?: string;
  // 成员归因的聚合：全部一致时为该值，混合时为 'mixed'
  attribution: ProcessAttribution | 'mixed';
  // 尚未退出的成员进程数
  liveCount: number;
  // 存活成员 rss 合计（至少一个成员有采样时才出现）
  totalRssBytes?: number;
  // 存活成员 CPU 百分比合计
  totalCpuPercent?: number;
  // 是否有可读输出尾部（含曾被截断的情况）
  hasOutput: boolean;
  // 成员进程列表（按首见时间、pid 升序）
  processes: OmpProcessMember[];
}

/** 目录级进程快照：domain-processes 路由与 UI zod schema 的对线形状（须保持一致）。 */
export interface OmpProcessSnapshot {
  // 单调递增修订号（每次状态变更 +1），读方据此判断快照新鲜度
  revision: number;
  // 快照生成时间（epoch ms）
  generatedAt: number;
  // 条目列表：running 在前，其余按开始时间倒序
  entries: OmpProcessEntry[];
}

/** 单个调用输出尾部的读取结果。 */
export interface OmpProcessOutput {
  // 调用主键
  key: string;
  // 保留的输出尾部文本
  output: string;
  // 追加过程中是否发生过截断
  truncated: boolean;
  // 是否仍在产出输出（有存活成员或调用未结束）
  live: boolean;
}

// ---------------------------------------------------------------------------
// Engine-facing event inputs. `args`/`details` arrive as runtime-shaped bags
// from the SDK event union; the hooks re-validate each field they read.
// ---------------------------------------------------------------------------

/** Typed view of the tool args the ledger reads — the engine boundary
 * asserts the SDK's declared contract once (see engine.ts). */
/** 台账读取的工具参数类型视图（中文）：SDK 事件联合的运行时形状已在引擎
 * 边界断言过一次，这里只声明台账实际读取的字段。 */
export interface LedgerToolArgs {
  // bash 命令串
  command?: string;
  // 声明的工作目录
  cwd?: string;
  // eval 语言（缺省按 js）
  language?: string;
  // eval 代码文本
  code?: string;
}

/** Typed view of the tool-result details the ledger reads. */
/** 工具结果 details 的类型视图（中文）：只读取 async.jobId 一个字段。 */
export interface LedgerToolDetails {
  // 受管异步 job 信息：jobId 使调用窗口延伸到 job 结束
  async?: { jobId?: string };
}

/** onToolStart 的输入：一次 bash/eval 调用开始（打开一个调用窗口）。 */
export interface LedgerToolStart {
  // 所属会话 id
  sessionID: string;
  // 会话目录（会归一化为目录 key）
  directory: string;
  // 工具调用 id（与 sessionID 拼成调用主键）
  toolCallId: string;
  // 工具名（仅 bash/eval 被跟踪）
  toolName: string;
  // 工具参数（command/cwd 或 language/code）
  args: LedgerToolArgs;
}

/** onToolUpdate 的输入：调用进行中的流式增量输出。 */
export interface LedgerToolUpdate {
  // 所属会话 id
  sessionID: string;
  // 会话目录
  directory: string;
  // 工具调用 id
  toolCallId: string;
  // 本批增量文本（空串会被忽略）
  text: string;
}

/** onToolEnd 的输入：调用结束，携带最终输出、退出码与可选的异步 job 关联。 */
export interface LedgerToolEnd {
  // 所属会话 id
  sessionID: string;
  // 会话目录
  directory: string;
  // 工具调用 id
  toolCallId: string;
  // 工具名（可选：结束事件不一定携带）
  toolName?: string;
  /** Normalized final output text. */
  /** 追加进输出尾部的归一化最终文本。 */
  output?: string;
  // 工具是否以错误结束（决定条目 failed 状态）
  isError?: boolean;
  // 退出码；可能为 null
  exitCode?: number | null;
  /** Tool result `details` — `details.async.jobId` links managed jobs. */
  /** details.async.jobId 把调用窗口延伸到受管 job 结束。 */
  details?: LedgerToolDetails;
}

/** kill 请求：定位条目 key，可选只终止其中单个 pid。 */
export interface LedgerKillRequest {
  // 发起 kill 的会话（权限校验主体）
  sessionID: string;
  // 条目主键（调用 key 或 proc:<pid>）
  key: string;
  // 省略则终止条目内全部存活成员
  pid?: number;
}

/** kill 结果：请求是否被接受，以及实际终止、跳过的 pid 明细。 */
export interface LedgerKillResult {
  // 请求是否被接受（找到条目且权限通过）
  ok: boolean;
  // 失败原因：条目不存在 / 会话无权限 / 参数无效
  error?: 'not-found' | 'forbidden' | 'invalid';
  // 成功终止的进程数
  killed: number;
  // 跳过的 pid（已退出、身份不符或终止失败）
  skipped: number[];
}

/** Timer handle union — the default deps return real NodeJS.Timeout handles;
 * injected test fakes need only satisfy the empty marker shape and flow back
 * through clearIntervalFn/clearTimeoutFn. */
/** 测试假定时器句柄（中文）：只需满足空标记形状，即可经清理函数回流。 */
export interface LedgerTimerFake {
  // 哨兵属性：仅用于构成可区分的标记类型，不承载值
  readonly __ledgerTimer?: never;
}
/** 定时器句柄：真实的 NodeJS.Timeout，或测试注入的假句柄。 */
export type LedgerTimerHandle = ReturnType<typeof setInterval> | LedgerTimerFake;

/** 台账依赖注入：平台观测/终止能力、job 状态查询、变更发布回调与全部时序参数。 */
export interface ProcessLedgerDeps {
  // 平台适配层：枚举后代树、采样资源、查询 cwd、终止进程
  platform: ProcessPlatform;
  /** Ids of running SDK async bash/eval jobs — extends windows for them. */
  /** 仍在运行的 SDK 异步 bash/eval job id（为它们延伸调用窗口）。 */
  runningJobIds?: () => string[];
  /** Cancel hook for job-backed entries (engine → AsyncJobManager.cancel). */
  /** job 支撑条目的取消钩子（engine → AsyncJobManager.cancel）。 */
  cancelJob?: (jobId: string, sessionID: string) => void;
  /** Called per dirty directory after a coalescing delay (engine publishes). */
  /** 每个脏目录在合并窗口结束后回调一次（由 engine 负责发布）。 */
  publishUpdate?: (directory: string) => void;
  // 时间源（默认 Date.now），测试可注入假时钟
  now?: () => number;
  // 轮询间隔（默认 2000ms）
  pollIntervalMs?: number;
  /** Post-end grace during which late `&` spawns still attribute. */
  /** 调用结束后的宽限期：期间迟到的 `&` 派生仍可归因。 */
  windowGraceMs?: number;
  /** How long exited invocations/processes stay listed. */
  /** 已退出的调用/进程在列表中的保留时长。 */
  retainExitedMs?: number;
  // 输出尾部字节上限（默认 64KB）
  outputTailBytes?: number;
  // 每会话保留的已完成调用条数上限（默认 50）
  maxInvocationsPerSession?: number;
  // 变更发布合并窗口（默认 400ms）
  publishCoalesceMs?: number;
  // CPU 核数（CPU 百分比归一用，默认 1）
  cpuCores?: number;
  // 轮询定时器注入点（测试假定时器）
  setIntervalFn?: (fn: () => void, ms: number) => LedgerTimerHandle;
  // 轮询定时器清理注入点
  clearIntervalFn?: (t: LedgerTimerHandle) => void;
  // 发布定时器注入点（测试假定时器）
  setTimeoutFn?: (fn: () => void, ms: number) => LedgerTimerHandle;
  // 发布定时器清理注入点
  clearTimeoutFn?: (t: LedgerTimerHandle) => void;
}

// ---------------------------------------------------------------------------

/** 默认轮询间隔：2000ms。 */
const DEFAULT_POLL_MS = 2000;
/** 默认窗口宽限期：调用结束后 3000ms 内的迟到派生仍可归因。 */
const DEFAULT_GRACE_MS = 3000;
/** 默认退出条目保留时长：10 分钟。 */
const DEFAULT_RETAIN_MS = 10 * 60_000;
/** 默认输出尾部上限：64KB。 */
const DEFAULT_TAIL_BYTES = 64 * 1024;
/** 默认每会话已完成调用条数上限：50。 */
const DEFAULT_MAX_INVOCATIONS = 50;
/** 默认变更发布合并窗口：400ms。 */
const DEFAULT_COALESCE_MS = 400;
/** 未归属根条目 key 前缀（后接根进程 pid）。 */
const UNATTRIBUTED_PREFIX = 'proc:';

/** 触发调用窗口跟踪的工具名集合（bash/eval）。 */
const TRACKED_TOOLS = new Set(['bash', 'eval']);

/** 内部调用记录：一次 bash/eval 调用从开始到结束（含宽限期/job 延伸）的窗口状态。 */
interface Invocation {
  // 调用主键：`${sessionID} ${toolCallId}`
  key: string;
  // 所属会话 id
  sessionID: string;
  // 归一化后的目录 key（快照过滤与发布定向）
  directory: string;
  // bash 或 eval
  kind: 'bash' | 'eval';
  // bash 命令串或 eval 摘要标签（argv 计分 token 的来源）
  command: string;
  // 声明的工作目录（多窗口计分的 cwd 加成）
  cwd?: string;
  // 开始时间（epoch ms）
  startedAt: number;
  // 结束时间；进行中缺省
  endedAt?: number;
  // 退出码；null 表示未知
  exitCode?: number | null;
  // 是否以错误结束（快照 failed 状态的来源）
  isError?: boolean;
  // 输出尾部文本（超限截头保留）
  outputTail: string;
  // 尾部是否发生过截断
  outputTruncated: boolean;
  // 关联的 SDK 异步 job id
  jobId?: string;
  // 用户是否终止过此调用
  killedByUser?: boolean;
}

/** 内部进程行：单个 pid 的归属与观测状态（含资源采样与退出判定）。 */
interface TrackedProc {
  // 进程 id
  pid: number;
  // 父进程 id（快照输出 + parent 归因）
  ppid: number;
  // 完整命令行；发生变化即判定为 PID 复用
  argv: string;
  // 进程组 id（可观测到时）
  pgid: number | null;
  // 工作目录（可观测到时）
  cwd?: string;
  // 首次观测时间
  firstSeenAt: number;
  // 判定退出时间；存活期间缺省
  exitedAt?: number;
  // 最近采样的常驻内存（字节）
  rssBytes?: number;
  // 最近采样的累计 CPU 毫秒（差分折算用）
  cpuMs?: number;
  // 折算出的 CPU 百分比（0-100，按核数归一）
  cpuPercent?: number;
  // 上次采样时间（cpuMs 差分的分母）
  lastSampleAt?: number;
  /** Invocation key or `proc:<rootPid>` for an unattributed subtree. */
  /** 所属调用 key，或未归属子树根 `proc:<pid>`。 */
  ownerKey: string;
  // 归属判定方式
  attribution: ProcessAttribution;
  // 未归属进程的候选会话 id
  candidateSessionIds?: string[];
  // 用户是否终止过此进程
  killedByUser?: boolean;
}

/** Aggregate resource view over an entry's live members. */
/** 条目存活成员的资源聚合视图（中文）：字段仅在存在采样时出现。 */
interface ProcTotals {
  // 存活成员 rss 合计（字节）
  rss?: number;
  // 存活成员 CPU 百分比合计
  cpu?: number;
}

/** Command tokens worth matching an argv against — basename of each word,
 * flags and `env VAR=` prefixes dropped, deduped. */
/** 提取命令中值得匹配 argv 的 token（中文）：逐词取 basename、丢弃
 * flag（- 开头）与 env 前缀（含 =）、去重，长度不足 2 的丢弃。 */
const commandTokens = (command: string): string[] => {
  const tokens: string[] = [];
  const seen = new Set<string>();
  for (const raw of command.split(/\s+/)) {
    if (!raw || raw.startsWith('-') || raw.includes('=')) continue;
    const base = raw.split('/').pop()?.toLowerCase();
    if (!base || base.length < 2 || seen.has(base)) continue;
    seen.add(base);
    tokens.push(base);
  }
  return tokens;
};

/** 生成 eval 调用的展示标签：语言 + 首个非空代码行（截断到 60 字符）。 */
const evalLabel = (args: LedgerToolArgs): string => {
  const language = args.language || 'js';
  const code = args.code ?? '';
  const first = code.split('\n').find((line) => line.trim().length > 0) ?? '';
  const snippet = first.trim().slice(0, 60);
  return snippet ? `eval ${language}: ${snippet}` : `eval ${language}`;
};

/** 向尾部追加 chunk 并只保留最后 max 字节；返回新尾部与是否发生截断。 */
const tailAppend = (tail: string, chunk: string, max: number) => {
  const next = tail + chunk;
  if (next.length <= max) return { tail: next, truncated: false };
  return { tail: next.slice(next.length - max), truncated: true };
};

/**
 * 进程台账：跟踪 bash/eval 调用派生的 host 后代进程，维护尽力而为的
 * 归属关系，产出目录级快照并支持定向 kill。
 *
 * 生命周期：构造即启动轮询定时器（真实句柄 unref，不单独保活 host）；
 * 工具事件钩子实时开合窗口并唤醒轮询；dispose 后停止一切跟踪。内部
 * 状态全部私有，外部读取只走 snapshot()/output()/revision。
 */
export class ProcessLedger {
  // 注入依赖（平台适配、job 查询、发布回调、定时器）
  readonly #deps: ProcessLedgerDeps;
  // 时间源
  readonly #now: () => number;
  // 轮询间隔（ms）
  readonly #pollMs: number;
  // 窗口宽限期（ms）
  readonly #graceMs: number;
  // 退出条目保留时长（ms）
  readonly #retainMs: number;
  // 输出尾部字节上限
  readonly #tailBytes: number;
  // 每会话调用条数上限
  readonly #maxInvocations: number;
  // 发布合并窗口（ms）
  readonly #coalesceMs: number;
  // CPU 核数（至少 1）
  readonly #cores: number;
  // 调用记录表：key -> Invocation
  readonly #invocations = new Map<string, Invocation>();
  // 进程跟踪表：pid -> TrackedProc
  readonly #procs = new Map<number, TrackedProc>();
  // 会话 -> 归一化目录（未归属条目的目录反查）
  readonly #sessionDirs = new Map<string, string>();
  // 快照修订号，每次标脏 +1
  #revision = 0;
  // 待发布变更的目录集合
  #dirtyDirectories = new Set<string>();
  // 轮询定时器句柄
  #timer: LedgerTimerHandle | null = null;
  // 发布合并定时器句柄
  #publishTimer: LedgerTimerHandle | null = null;
  // 进行中的一轮 tick（并发调用合流用）
  #tickPromise: Promise<void> | null = null;
  // dispose 后忽略事件与轮询
  #disposed = false;

  /**
   * 构造台账：读取时序依赖（缺省用 DEFAULT_* 常量）并启动轮询定时器。
   * 真实 interval 句柄会 unref——监控本身不得保活 host；注入的测试假
   * 句柄可能没有 unref，因此探测保持可选。
   */
  constructor(deps: ProcessLedgerDeps) {
    this.#deps = deps;
    this.#now = deps.now ?? (() => Date.now());
    this.#pollMs = deps.pollIntervalMs ?? DEFAULT_POLL_MS;
    this.#graceMs = deps.windowGraceMs ?? DEFAULT_GRACE_MS;
    this.#retainMs = deps.retainExitedMs ?? DEFAULT_RETAIN_MS;
    this.#tailBytes = deps.outputTailBytes ?? DEFAULT_TAIL_BYTES;
    this.#maxInvocations = deps.maxInvocationsPerSession ?? DEFAULT_MAX_INVOCATIONS;
    this.#coalesceMs = deps.publishCoalesceMs ?? DEFAULT_COALESCE_MS;
    this.#cores = Math.max(1, deps.cpuCores ?? 1);
    const setIntervalFn = deps.setIntervalFn ?? ((fn: () => void, ms: number) => setInterval(fn, ms));
    const timer = setIntervalFn(() => void this.tick(), this.#pollMs);
    // SAFETY: the real interval handle exposes unref; injected test fakes may
    // not, so the probe stays optional — the contract is "don't keep the
    // host alive for monitoring alone".
    (timer as { unref?: () => void }).unref?.();
    this.#timer = timer;
  }

  /** 当前修订号：每次状态变更递增，读方据此判断快照是否需要重建。 */
  get revision(): number {
    return this.#revision;
  }

  // -------------------------------------------------------------------------
  // Engine tool-event hooks
  // -------------------------------------------------------------------------

  /**
   * 工具开始钩子：为被跟踪的 bash/eval 调用建立调用记录并打开窗口，
   * 同时记录会话目录。非跟踪工具或已 dispose 时直接忽略；记录后立即
   * 唤醒一轮 tick——shell 子进程通常在一个轮询周期内就已出现。
   */
  onToolStart(input: LedgerToolStart): void {
    if (!TRACKED_TOOLS.has(input.toolName) || this.#disposed) return;
    const sessionID = input.sessionID;
    const directory = normalizeDirectoryKey(input.directory);
    this.#sessionDirs.set(sessionID, directory);
    const kind = input.toolName === 'bash' ? 'bash' : 'eval';
    const command = kind === 'bash' ? (input.args.command ?? '') : evalLabel(input.args);
    const cwd = input.args.cwd;
    const key = `${sessionID} ${input.toolCallId}`;
    const invocation: Invocation = {
      key,
      sessionID,
      directory,
      kind,
      command,
      startedAt: this.#now(),
      outputTail: '',
      outputTruncated: false,
    };
    if (cwd) invocation.cwd = cwd;
    this.#invocations.set(key, invocation);
    // Wake the loop promptly — the shell child usually exists within a tick.
    void this.tick();
  }

  /** 工具增量输出钩子：把流式文本追加进该调用的输出尾部（超限截头）；未知调用忽略。 */
  onToolUpdate(input: LedgerToolUpdate): void {
    const inv = this.#invocations.get(`${input.sessionID} ${input.toolCallId}`);
    if (!inv || !input.text) return;
    const next = tailAppend(inv.outputTail, input.text, this.#tailBytes);
    inv.outputTail = next.tail;
    inv.outputTruncated = inv.outputTruncated || next.truncated;
  }

  /**
   * 工具结束钩子：记录结束时间/错误标记/退出码并追加最终输出；details
   * 带 async.jobId 时记录 job 关联（受管异步 job 在调用结束后继续延伸
   * 窗口）。结束会把该调用所在目录标脏。
   */
  onToolEnd(input: LedgerToolEnd): void {
    const inv = this.#invocations.get(`${input.sessionID} ${input.toolCallId}`);
    if (!inv) return;
    inv.endedAt = this.#now();
    inv.isError = input.isError === true;
    inv.exitCode = input.exitCode ?? null;
    if (input.output) {
      const next = tailAppend(inv.outputTail, input.output, this.#tailBytes);
      inv.outputTail = next.tail;
      inv.outputTruncated = inv.outputTruncated || next.truncated;
    }
    // Managed async jobs: the tool call ends at registration while the job
    // keeps spawning — `details.async.jobId` extends its window.
    const jobId = input.details?.async?.jobId;
    if (jobId) inv.jobId = jobId;
    this.#markDirty(inv.directory);
  }

  // -------------------------------------------------------------------------
  // Polling
  // -------------------------------------------------------------------------

  /** 当前仍在运行的 SDK 异步 job id 集合（未注入 runningJobIds 时视为空）。 */
  #jobIdsRunning(): Set<string> {
    return new Set(this.#deps.runningJobIds?.() ?? []);
  }

  /** 窗口判定：调用未结束，或结束后仍在宽限期内，或其关联 job 仍在运行。 */
  #isOpen(inv: Invocation, now: number, runningJobs: Set<string>): boolean {
    if (inv.endedAt === undefined) return true;
    if (now - inv.endedAt <= this.#graceMs) return true;
    return inv.jobId !== undefined && runningJobs.has(inv.jobId);
  }

  /** 是否还有可跟踪的工作：任一窗口打开，或任一进程行仍存活。 */
  #hasWork(now: number, runningJobs: Set<string>): boolean {
    for (const inv of this.#invocations.values()) {
      if (this.#isOpen(inv, now, runningJobs)) return true;
    }
    for (const proc of this.#procs.values()) {
      if (!proc.exitedAt) return true;
    }
    return false;
  }

  /**
   * One enumeration/diff/stats pass. Called by the poll interval and by the
   * wake edges in the tool hooks; callers may await it (tests join through
   * it). Concurrent calls join the in-flight pass instead of double-running.
   */
  /** 单轮枚举/差分/采样流程（中文）：由轮询定时器与工具钩子的唤醒沿触发；
   * 调用方可 await（测试借此合流）；并发调用并入进行中的一轮，不重复执行。 */
  tick(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    this.#tickPromise ??= this.#doTick().finally(() => {
      this.#tickPromise = null;
    });
    return this.#tickPromise;
  }

  /**
   * tick 执行体：无工作直接返回；enumerateTree 失败则整轮放弃（绝不把
   * 枚举失败当成进程退出，下轮重试），成功后依次差分新进程、采样资源、
   * 清理过期条目。
   */
  async #doTick(): Promise<void> {
    const now = this.#now();
    const runningJobs = this.#jobIdsRunning();
    if (!this.#hasWork(now, runningJobs)) return;
    let samples: ProcInfo[];
    try {
      samples = await this.#deps.platform.enumerateTree();
    } catch {
      // A failed enumeration pass must not fabricate exits; retry next tick.
      return;
    }
    this.#diff(samples, now, runningJobs);
    await this.#sampleStats(now);
    this.#purge(now);
  }

  /**
   * 差分一轮枚举样本：未见过的 pid 交给 #adopt 归因；同 pid 但 argv 变化
   * 视为 PID 复用（旧行标记退出、样本重新归因）；已知行刷新 ppid/pgid；
   * 本轮未出现且未标退出的行判定退出并标脏其所属目录。
   */
  #diff(samples: ProcInfo[], now: number, runningJobs: Set<string>): void {
    const seen = new Set<number>();
    const fresh: ProcInfo[] = [];
    for (const sample of samples) {
      seen.add(sample.pid);
      const known = this.#procs.get(sample.pid);
      if (!known) {
        fresh.push(sample);
        continue;
      }
      // PID reuse: same pid holding a different program is a new process.
      if (known.argv !== sample.argv) {
        if (!known.exitedAt) known.exitedAt = now;
        fresh.push(sample);
        continue;
      }
      known.ppid = sample.ppid;
      known.pgid = sample.pgid;
    }
    for (const proc of this.#procs.values()) {
      if (!proc.exitedAt && !seen.has(proc.pid)) {
        proc.exitedAt = now;
        this.#markDirty(this.#directoryOfOwner(proc.ownerKey));
      }
    }
    for (const sample of fresh) this.#adopt(sample, now, runningJobs);
  }

  /** Open invocation windows a freshly seen pid could belong to. */
  /** 当前打开的调用窗口列表（新进程的归属候选）。 */
  #openCandidates(now: number, runningJobs: Set<string>): Invocation[] {
    const out: Invocation[] = [];
    for (const inv of this.#invocations.values()) {
      if (this.#isOpen(inv, now, runningJobs)) out.push(inv);
    }
    return out;
  }

  /**
   * 新进程归因：
   * 1) 父进程已跟踪 → 直接继承 owner 与候选会话（parent / 继承 unattributed）；
   * 2) 没有打开窗口 → 视为基础设施派生（MCP/LSP/worker），不跟踪；
   * 3) 单一窗口 → 除非窗口有存活 job 支撑，argv 必须命中至少一个命令
   *    token，否则落入 unattributed 根；命中则记 window/job；
   * 4) 多窗口 → argv token 计分 + cwd 命中加成，唯一最高分胜出
   *    （argv/job）；平分或零分落入 unattributed 根并保留候选会话 id。
   */
  #adopt(sample: ProcInfo, now: number, runningJobs: Set<string>): void {
    // Rule 1 — the parent is already tracked: inherit its owner outright.
    const parent = this.#procs.get(sample.ppid);
    if (parent && !parent.exitedAt) {
      const attribution = parent.attribution === 'unattributed' ? 'unattributed' : 'parent';
      this.#track(sample, now, parent.ownerKey, attribution, parent.candidateSessionIds);
      return;
    }
    const candidates = this.#openCandidates(now, runningJobs);
    if (candidates.length === 0) {
      // No open window: infra spawn (MCP/LSP/worker) — not ours to show.
      return;
    }
    if (candidates.length === 1) {
      const [inv] = candidates;
      const jobBacked = inv.jobId !== undefined && runningJobs.has(inv.jobId);
      // A lone open window must not swallow unrelated host spawns (daemon
      // broker, eval workers, conhost wrappers land inside arbitrary
      // windows): require one command token in the argv unless a live job
      // backs the window. Zero overlap files the process under an
      // unattributed root — visible and killable per the plan's fallback.
      const hay = sample.argv.toLowerCase();
      if (!jobBacked && !commandTokens(inv.command).some((token) => hay.includes(token))) {
        const ownerKey = `${UNATTRIBUTED_PREFIX}${sample.pid}`;
        this.#track(sample, now, ownerKey, 'unattributed', [inv.sessionID]);
        this.#markDirty(inv.directory);
        return;
      }
      this.#track(sample, now, inv.key, jobBacked ? 'job' : 'window');
      this.#markDirty(inv.directory);
      return;
    }
    // Overlapping windows: score argv tokens against each command.
    let best: Invocation | null = null;
    let bestScore = 0;
    let tied = false;
    const hay = sample.argv.toLowerCase();
    const sampleCwd = this.#deps.platform.cwdOf(sample.pid);
    for (const inv of candidates) {
      let score = 0;
      for (const token of commandTokens(inv.command)) {
        if (hay.includes(token)) score += 1;
      }
      if (inv.cwd && sampleCwd && inv.cwd === sampleCwd) score += 2;
      if (score > bestScore) {
        best = inv;
        bestScore = score;
        tied = false;
      } else if (score === bestScore) {
        tied = true;
      }
    }
    if (best && !tied && bestScore > 0) {
      const attribution = best.jobId && runningJobs.has(best.jobId) ? 'job' : 'argv';
      this.#track(sample, now, best.key, attribution);
      this.#markDirty(best.directory);
      return;
    }
    // Unresolvable: keep the process visible under its own root entry with
    // the sessions that could plausibly own it.
    const candidateSessionIds = [...new Set(candidates.map((inv) => inv.sessionID))];
    const ownerKey = `${UNATTRIBUTED_PREFIX}${sample.pid}`;
    this.#track(sample, now, ownerKey, 'unattributed', candidateSessionIds);
    this.#markDirty(this.#directoryOfOwner(ownerKey));
  }

  /** 落表：按归因结果创建 TrackedProc 行（pid/ppid/argv/pgid + owner 与候选会话）。 */
  #track(
    sample: ProcInfo,
    now: number,
    ownerKey: string,
    attribution: ProcessAttribution,
    candidateSessionIds?: string[],
  ): void {
    const proc: TrackedProc = {
      pid: sample.pid,
      ppid: sample.ppid,
      argv: sample.argv,
      pgid: sample.pgid,
      firstSeenAt: now,
      ownerKey,
      attribution,
    };
    if (candidateSessionIds) proc.candidateSessionIds = candidateSessionIds;
    this.#procs.set(sample.pid, proc);
  }

  /**
   * 对存活成员采样资源：更新 rss；用 cpuMs 差分除以采样间隔折算 CPU
   * 百分比（按核数归一并夹在 0-100）；采样失败整轮静默跳过。每个有
   * 更新的成员都标脏其所属目录。
   */
  async #sampleStats(now: number): Promise<void> {
    const live = [...this.#procs.values()].filter((proc) => !proc.exitedAt);
    if (live.length === 0) return;
    let stats: Map<number, ProcStats>;
    try {
      stats = await this.#deps.platform.sampleStats(live.map((proc) => proc.pid));
    } catch {
      return;
    }
    for (const proc of live) {
      const sample = stats.get(proc.pid);
      if (!sample) continue;
      if (sample.rssBytes !== undefined) proc.rssBytes = sample.rssBytes;
      if (sample.cpuMs !== undefined && proc.cpuMs !== undefined && proc.lastSampleAt) {
        const elapsed = now - proc.lastSampleAt;
        if (elapsed > 0) {
          const pct = ((sample.cpuMs - proc.cpuMs) / elapsed) * 100;
          proc.cpuPercent = Math.max(0, Math.min(100, pct / this.#cores));
        }
      }
      if (sample.cpuMs !== undefined) {
        proc.cpuMs = sample.cpuMs;
        proc.lastSampleAt = now;
      }
      this.#markDirty(this.#directoryOfOwner(proc.ownerKey));
    }
  }

  /**
   * 清理：删除超过保留期的已退出进程行；调用记录在已结束、无存活成员
   * 且过保留期后删除；再按会话限制已完成条数，超限时驱逐最旧的。
   */
  #purge(now: number): void {
    for (const [pid, proc] of this.#procs) {
      if (proc.exitedAt && now - proc.exitedAt > this.#retainMs) this.#procs.delete(pid);
    }
    // Drop invocations once ended, fully dead and past retention — plus cap
    // per-session count, evicting the oldest finished entries first.
    const finishedBySession = new Map<string, Invocation[]>();
    for (const [key, inv] of this.#invocations) {
      const alive = [...this.#procs.values()].some((proc) => proc.ownerKey === key && !proc.exitedAt);
      const stale = inv.endedAt !== undefined && !alive && now - inv.endedAt > this.#retainMs;
      if (stale) {
        this.#invocations.delete(key);
        continue;
      }
      if (inv.endedAt !== undefined && !alive) {
        const list = finishedBySession.get(inv.sessionID) ?? [];
        list.push(inv);
        finishedBySession.set(inv.sessionID, list);
      }
    }
    for (const list of finishedBySession.values()) {
      if (list.length <= this.#maxInvocations) continue;
      list.sort((a, b) => a.startedAt - b.startedAt);
      for (const inv of list.slice(0, list.length - this.#maxInvocations)) {
        this.#invocations.delete(inv.key);
      }
    }
  }

  /** 反查 ownerKey 的目录：调用记录直接取 directory；未归属根取首个候选会话的目录。 */
  #directoryOfOwner(ownerKey: string): string {
    if (ownerKey.startsWith(UNATTRIBUTED_PREFIX)) {
      const root = this.#procs.get(Number(ownerKey.slice(UNATTRIBUTED_PREFIX.length)));
      const first = root?.candidateSessionIds?.[0];
      return (first ? this.#sessionDirs.get(first) : undefined) ?? '';
    }
    return this.#invocations.get(ownerKey)?.directory ?? '';
  }

  /** 标脏：修订号 +1，登记目录（非空时），并调度合并发布。 */
  #markDirty(directory: string): void {
    this.#revision += 1;
    if (directory) this.#dirtyDirectories.add(directory);
    this.#schedulePublish();
  }

  /** 合并发布：延迟 coalesceMs 后把脏目录集合一次性交给 publishUpdate；窗口内的后续变更累积到下一轮。 */
  #schedulePublish(): void {
    if (this.#publishTimer || !this.#deps.publishUpdate) return;
    const setTimeoutFn = this.#deps.setTimeoutFn ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
    this.#publishTimer = setTimeoutFn(() => {
      this.#publishTimer = null;
      const dirty = [...this.#dirtyDirectories];
      this.#dirtyDirectories.clear();
      for (const directory of dirty) this.#deps.publishUpdate?.(directory);
    }, this.#coalesceMs);
  }

  // -------------------------------------------------------------------------
  // Reads
  // -------------------------------------------------------------------------

  /** 取某 owner 下的全部成员进程，按首见时间、pid 升序排列。 */
  #membersOf(ownerKey: string): TrackedProc[] {
    const members: TrackedProc[] = [];
    for (const proc of this.#procs.values()) {
      if (proc.ownerKey === ownerKey) members.push(proc);
    }
    members.sort((a, b) => a.firstSeenAt - b.firstSeenAt || a.pid - b.pid);
    return members;
  }

  /** 把内部 TrackedProc 投影为对线形态 OmpProcessMember（缺省字段不出现）。 */
  #memberView(proc: TrackedProc): OmpProcessMember {
    const view: OmpProcessMember = {
      pid: proc.pid,
      ppid: proc.ppid,
      argv: proc.argv,
      state: proc.exitedAt ? 'exited' : 'running',
      firstSeenAt: proc.firstSeenAt,
      attribution: proc.attribution,
    };
    if (proc.cwd) view.cwd = proc.cwd;
    if (proc.exitedAt) view.exitedAt = proc.exitedAt;
    if (proc.rssBytes !== undefined) view.rssBytes = proc.rssBytes;
    if (proc.cpuPercent !== undefined) view.cpuPercent = proc.cpuPercent;
    if (proc.killedByUser) view.killedByUser = true;
    return view;
  }

  /** 条目状态：用户终止过 → killed；有存活成员 → running；否则按调用结束的 isError 给 exited/failed。 */
  #entryStatus(inv: Invocation | null, members: TrackedProc[], killed: boolean): OmpProcessEntry['status'] {
    if (killed || members.some((proc) => proc.killedByUser)) return 'killed';
    if (members.some((proc) => !proc.exitedAt)) return 'running';
    if (!inv?.endedAt) return 'running';
    return inv.isError ? 'failed' : 'exited';
  }

  /** 聚合存活成员的 rss/cpu；仅当至少一个成员有对应采样时才输出该字段。 */
  #totals(members: TrackedProc[]): ProcTotals {
    const live = members.filter((proc) => !proc.exitedAt);
    const totals: ProcTotals = {};
    if (live.some((proc) => proc.rssBytes !== undefined)) {
      totals.rss = live.reduce((sum, proc) => sum + (proc.rssBytes ?? 0), 0);
    }
    if (live.some((proc) => proc.cpuPercent !== undefined)) {
      totals.cpu = live.reduce((sum, proc) => sum + (proc.cpuPercent ?? 0), 0);
    }
    return totals;
  }

  /**
   * 产出目录快照（纯读取）：按归一化目录过滤调用记录（无成员的跳过），
   * 再补充该目录下的未归属根条目；每条聚合成员视图、归因集合、状态与
   * 资源合计；running 条目排前，其余按开始时间倒序。
   */
  snapshot(directory: string): OmpProcessSnapshot {
    const dir = normalizeDirectoryKey(directory);
    const now = this.#now();
    const entries: OmpProcessEntry[] = [];
    for (const inv of this.#invocations.values()) {
      if (inv.directory !== dir) continue;
      const members = this.#membersOf(inv.key);
      if (members.length === 0) continue;
      const attributions = new Set(members.map((proc) => proc.attribution));
      const totals = this.#totals(members);
      const entry: OmpProcessEntry = {
        key: inv.key,
        sessionID: inv.sessionID,
        kind: inv.kind,
        command: inv.command,
        status: this.#entryStatus(inv, members, Boolean(inv.killedByUser)),
        startedAt: inv.startedAt,
        attribution: attributions.size === 1 ? (members[0]?.attribution ?? 'window') : 'mixed',
        liveCount: members.filter((proc) => !proc.exitedAt).length,
        hasOutput: inv.outputTail.length > 0 || inv.outputTruncated,
        processes: members.map((proc) => this.#memberView(proc)),
      };
      if (inv.cwd) entry.cwd = inv.cwd;
      if (inv.endedAt !== undefined) entry.endedAt = inv.endedAt;
      if (inv.exitCode !== undefined) entry.exitCode = inv.exitCode;
      if (inv.jobId) entry.jobId = inv.jobId;
      if (totals.rss !== undefined) entry.totalRssBytes = totals.rss;
      if (totals.cpu !== undefined) entry.totalCpuPercent = totals.cpu;
      entries.push(entry);
    }
    for (const proc of this.#procs.values()) {
      if (!proc.ownerKey.startsWith(UNATTRIBUTED_PREFIX)) continue;
      if (proc.ownerKey !== `${UNATTRIBUTED_PREFIX}${proc.pid}`) continue;
      if (this.#directoryOfOwner(proc.ownerKey) !== dir) continue;
      const members = this.#membersOf(proc.ownerKey);
      const totals = this.#totals(members);
      const entry: OmpProcessEntry = {
        key: proc.ownerKey,
        sessionID: null,
        kind: 'process',
        command: proc.argv,
        status: members.some((member) => !member.exitedAt) ? 'running' : 'exited',
        startedAt: proc.firstSeenAt,
        attribution: 'unattributed',
        liveCount: members.filter((member) => !member.exitedAt).length,
        hasOutput: false,
        processes: members.map((member) => this.#memberView(member)),
      };
      if (proc.candidateSessionIds) entry.candidateSessionIds = proc.candidateSessionIds;
      if (totals.rss !== undefined) entry.totalRssBytes = totals.rss;
      if (totals.cpu !== undefined) entry.totalCpuPercent = totals.cpu;
      entries.push(entry);
    }
    entries.sort((a, b) => {
      const rank = (entry: OmpProcessEntry) => (entry.status === 'running' ? 0 : 1);
      return rank(a) - rank(b) || b.startedAt - a.startedAt;
    });
    return { revision: this.#revision, generatedAt: now, entries };
  }

  /** 读取某个调用的输出尾部；key 未知返回 null；live 表示仍在产出输出。 */
  output(key: string): OmpProcessOutput | null {
    const inv = this.#invocations.get(key);
    if (!inv) return null;
    const live = this.#membersOf(key).some((proc) => !proc.exitedAt) || inv.endedAt === undefined;
    return { key, output: inv.outputTail, truncated: inv.outputTruncated, live };
  }

  // -------------------------------------------------------------------------
  // Kill
  // -------------------------------------------------------------------------

  /**
   * 终止条目（或其中单个 pid）：先校验 key 与会话归属（未归属根要求请求
   * 会话在候选列表中），失败返回 not-found/forbidden；逐个终止前用 argv
   * 复核进程身份（防 PID 复用误杀），已退出/身份不符/终止失败的收入
   * skipped；job 支撑的调用同步 cancelJob；有成功 kill 则标脏。
   */
  async kill(input: LedgerKillRequest): Promise<LedgerKillResult> {
    const now = this.#now();
    let ownerKey: string;
    if (input.key.startsWith(UNATTRIBUTED_PREFIX)) {
      const rootPid = Number(input.key.slice(UNATTRIBUTED_PREFIX.length));
      const root = this.#procs.get(rootPid);
      if (!root || root.ownerKey !== input.key) {
        return { ok: false, error: 'not-found', killed: 0, skipped: [] };
      }
      if (!root.candidateSessionIds?.includes(input.sessionID)) {
        return { ok: false, error: 'forbidden', killed: 0, skipped: [] };
      }
      ownerKey = input.key;
    } else {
      const inv = this.#invocations.get(input.key);
      if (!inv) return { ok: false, error: 'not-found', killed: 0, skipped: [] };
      if (inv.sessionID !== input.sessionID) {
        return { ok: false, error: 'forbidden', killed: 0, skipped: [] };
      }
      ownerKey = input.key;
    }
    const members = this.#membersOf(ownerKey);
    const targets = input.pid !== undefined
      ? members.filter((proc) => proc.pid === input.pid && !proc.exitedAt)
      : members.filter((proc) => !proc.exitedAt);
    if (input.pid !== undefined && targets.length === 0) {
      return { ok: false, error: 'not-found', killed: 0, skipped: [] };
    }
    let killed = 0;
    const skipped: number[] = [];
    for (const proc of targets) {
      // argv identity check: a reused pid is not the process the user saw.
      if (!this.#deps.platform.isAlive(proc.pid, proc.argv)) {
        proc.exitedAt ??= now;
        skipped.push(proc.pid);
        continue;
      }
      const ok = await this.#deps.platform.terminate(proc.pid);
      if (ok) {
        killed += 1;
        proc.killedByUser = true;
      } else {
        skipped.push(proc.pid);
      }
    }
    const inv = this.#invocations.get(ownerKey);
    if (inv?.jobId) this.#deps.cancelJob?.(inv.jobId, inv.sessionID);
    if (killed > 0) {
      if (inv) inv.killedByUser = true;
      this.#markDirty(this.#directoryOfOwner(ownerKey));
    }
    return { ok: true, killed, skipped };
  }

  /** 停止台账：置 disposed 标记并清理轮询、发布两个定时器（清理函数走依赖注入，缺省用全局 clear*）。 */
  dispose(): void {
    this.#disposed = true;
    if (this.#timer) {
      // SAFETY: the default fns only ever receive real timer handles.
      const clearIntervalFn = this.#deps.clearIntervalFn ?? ((t: LedgerTimerHandle) => clearInterval(t as NodeJS.Timeout));
      clearIntervalFn(this.#timer);
      this.#timer = null;
    }
    if (this.#publishTimer) {
      // SAFETY: the default fns only ever receive real timer handles.
      const clearTimeoutFn = this.#deps.clearTimeoutFn ?? ((t: LedgerTimerHandle) => clearTimeout(t as NodeJS.Timeout));
      clearTimeoutFn(this.#publishTimer);
      this.#publishTimer = null;
    }
  }
}
