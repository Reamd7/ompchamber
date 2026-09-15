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

import type { ProcInfo, ProcStats, ProcessPlatform } from './process-platform.ts';
import { normalizeDirectoryKey } from './registry.ts';

export type ProcessAttribution = 'window' | 'argv' | 'parent' | 'job' | 'unattributed';

// ---------------------------------------------------------------------------
// Wire-facing snapshot types (consumed by domain-processes routes and the UI
// zod schema in packages/ui/src/lib/api/omp.ts — keep the shapes aligned).
// ---------------------------------------------------------------------------

export interface OmpProcessMember {
  pid: number;
  ppid: number;
  argv: string;
  cwd?: string;
  state: 'running' | 'exited';
  firstSeenAt: number;
  exitedAt?: number;
  rssBytes?: number;
  cpuPercent?: number;
  attribution: ProcessAttribution;
  killedByUser?: boolean;
}

export interface OmpProcessEntry {
  /** Invocation key `${sessionID}${toolCallId}` or `proc:<pid>` for an unattributed root. */
  key: string;
  sessionID: string | null;
  /** Sessions that owned an open window when an unattributed process appeared. */
  candidateSessionIds?: string[];
  kind: 'bash' | 'eval' | 'process';
  command: string;
  cwd?: string;
  status: 'running' | 'exited' | 'killed' | 'failed';
  startedAt: number;
  endedAt?: number;
  exitCode?: number | null;
  jobId?: string;
  attribution: ProcessAttribution | 'mixed';
  liveCount: number;
  totalRssBytes?: number;
  totalCpuPercent?: number;
  /** True while consecutive enumeration/sampling failures make the reported
   * stats unreliable — the UI marks the row rather than showing dead numbers. */
  statsStale?: boolean;
  hasOutput: boolean;
  processes: OmpProcessMember[];
}

export interface OmpProcessSnapshot {
  revision: number;
  generatedAt: number;
  entries: OmpProcessEntry[];
}

export interface OmpProcessOutput {
  key: string;
  output: string;
  truncated: boolean;
  live: boolean;
}

// ---------------------------------------------------------------------------
// Engine-facing event inputs. `args`/`details` arrive as runtime-shaped bags
// from the SDK event union; the hooks re-validate each field they read.
// ---------------------------------------------------------------------------

/** Typed view of the tool args the ledger reads — the engine boundary
 * asserts the SDK's declared contract once (see engine.ts). */
export interface LedgerToolArgs {
  command?: string;
  cwd?: string;
  language?: string;
  code?: string;
}

/** Typed view of the tool-result details the ledger reads. */
export interface LedgerToolDetails {
  async?: { jobId?: string };
}

export interface LedgerToolStart {
  sessionID: string;
  directory: string;
  toolCallId: string;
  toolName: string;
  args: LedgerToolArgs;
}

export interface LedgerToolUpdate {
  sessionID: string;
  directory: string;
  toolCallId: string;
  text: string;
}

export interface LedgerToolEnd {
  sessionID: string;
  directory: string;
  toolCallId: string;
  toolName?: string;
  /** Normalized final output text. */
  output?: string;
  isError?: boolean;
  exitCode?: number | null;
  /** Tool result `details` — `details.async.jobId` links managed jobs. */
  details?: LedgerToolDetails;
}

export interface LedgerKillRequest {
  sessionID: string;
  key: string;
  pid?: number;
}

export interface LedgerKillResult {
  ok: boolean;
  error?: 'not-found' | 'forbidden' | 'invalid';
  killed: number;
  skipped: number[];
}

/** Timer handle union — the default deps return real NodeJS.Timeout handles;
 * injected test fakes need only satisfy the empty marker shape and flow back
 * through clearIntervalFn/clearTimeoutFn. */
export interface LedgerTimerFake {
  readonly __ledgerTimer?: never;
}
export type LedgerTimerHandle = ReturnType<typeof setInterval> | LedgerTimerFake;

export interface ProcessLedgerDeps {
  platform: ProcessPlatform;
  /** Ids of running SDK async bash/eval jobs — extends windows for them. */
  runningJobIds?: () => string[];
  /** Cancel hook for job-backed entries (engine → AsyncJobManager.cancel). */
  cancelJob?: (jobId: string, sessionID: string) => void;
  /** Called per dirty directory after a coalescing delay (engine publishes). */
  publishUpdate?: (directory: string) => void;
  now?: () => number;
  pollIntervalMs?: number;
  /** Post-end grace during which late `&` spawns still attribute. */
  windowGraceMs?: number;
  /** How long exited invocations/processes stay listed. */
  retainExitedMs?: number;
  outputTailBytes?: number;
  maxInvocationsPerSession?: number;
  publishCoalesceMs?: number;
  cpuCores?: number;
  setIntervalFn?: (fn: () => void, ms: number) => LedgerTimerHandle;
  clearIntervalFn?: (t: LedgerTimerHandle) => void;
  setTimeoutFn?: (fn: () => void, ms: number) => LedgerTimerHandle;
  clearTimeoutFn?: (t: LedgerTimerHandle) => void;
}

// ---------------------------------------------------------------------------

const DEFAULT_POLL_MS = 2000;
const DEFAULT_GRACE_MS = 3000;
const DEFAULT_RETAIN_MS = 10 * 60_000;
const DEFAULT_TAIL_BYTES = 64 * 1024;
const DEFAULT_MAX_INVOCATIONS = 50;
const DEFAULT_COALESCE_MS = 400;
const UNATTRIBUTED_PREFIX = 'proc:';
// Sampling health: failures mark live entries `statsStale` once they persist,
// and the stats reader backs off exponentially so a broken platform cannot
// spin a powershell/proc spawn storm every poll tick.
const STATS_STALE_FAILURES = 3;
const STATS_BACKOFF_MAX_MS = 30_000;

const TRACKED_TOOLS = new Set(['bash', 'eval']);

interface Invocation {
  key: string;
  sessionID: string;
  directory: string;
  kind: 'bash' | 'eval';
  command: string;
  cwd?: string;
  startedAt: number;
  endedAt?: number;
  exitCode?: number | null;
  isError?: boolean;
  outputTail: string;
  outputTruncated: boolean;
  jobId?: string;
  killedByUser?: boolean;
}

interface TrackedProc {
  pid: number;
  ppid: number;
  argv: string;
  pgid: number | null;
  cwd?: string;
  firstSeenAt: number;
  exitedAt?: number;
  rssBytes?: number;
  cpuMs?: number;
  cpuPercent?: number;
  lastSampleAt?: number;
  /** Invocation key or `proc:<rootPid>` for an unattributed subtree. */
  ownerKey: string;
  attribution: ProcessAttribution;
  candidateSessionIds?: string[];
  killedByUser?: boolean;
}

/** Aggregate resource view over an entry's live members. */
interface ProcTotals {
  rss?: number;
  cpu?: number;
}

/** Command tokens worth matching an argv against — basename of each word,
 * flags and `env VAR=` prefixes dropped, deduped. */
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

const evalLabel = (args: LedgerToolArgs): string => {
  const language = args.language || 'js';
  const code = args.code ?? '';
  const first = code.split('\n').find((line) => line.trim().length > 0) ?? '';
  const snippet = first.trim().slice(0, 60);
  return snippet ? `eval ${language}: ${snippet}` : `eval ${language}`;
};

const tailAppend = (tail: string, chunk: string, max: number) => {
  const next = tail + chunk;
  if (next.length <= max) return { tail: next, truncated: false };
  return { tail: next.slice(next.length - max), truncated: true };
};

export class ProcessLedger {
  readonly #deps: ProcessLedgerDeps;
  readonly #now: () => number;
  readonly #pollMs: number;
  readonly #graceMs: number;
  readonly #retainMs: number;
  readonly #tailBytes: number;
  readonly #maxInvocations: number;
  readonly #coalesceMs: number;
  readonly #cores: number;
  readonly #invocations = new Map<string, Invocation>();
  readonly #procs = new Map<number, TrackedProc>();
  readonly #sessionDirs = new Map<string, string>();
  #revision = 0;
  #dirtyDirectories = new Set<string>();
  #timer: LedgerTimerHandle | null = null;
  #publishTimer: LedgerTimerHandle | null = null;
  #tickPromise: Promise<void> | null = null;
  #disposed = false;
  // Diagnostics counters (plan: "给 /omp/diagnostics 加 pollMs/pollCount/
  // spawnCount 计数器实测") — O(1) reads, reset only on dispose.
  #pollCount = 0;
  #lastPollMs = 0;
  #spawnCount = 0;
  #sampleFailuresTotal = 0;
  #consecutiveFailures = 0;
  #statsStale = false;
  #statsBackoffUntil = 0;

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

  get revision(): number {
    return this.#revision;
  }

  // -------------------------------------------------------------------------
  // Engine tool-event hooks
  // -------------------------------------------------------------------------

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

  onToolUpdate(input: LedgerToolUpdate): void {
    const inv = this.#invocations.get(`${input.sessionID} ${input.toolCallId}`);
    if (!inv || !input.text) return;
    const next = tailAppend(inv.outputTail, input.text, this.#tailBytes);
    inv.outputTail = next.tail;
    inv.outputTruncated = inv.outputTruncated || next.truncated;
  }

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

  #jobIdsRunning(): Set<string> {
    return new Set(this.#deps.runningJobIds?.() ?? []);
  }

  #isOpen(inv: Invocation, now: number, runningJobs: Set<string>): boolean {
    if (inv.endedAt === undefined) return true;
    if (now - inv.endedAt <= this.#graceMs) return true;
    return inv.jobId !== undefined && runningJobs.has(inv.jobId);
  }

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
  tick(): Promise<void> {
    if (this.#disposed) return Promise.resolve();
    this.#tickPromise ??= this.#doTick().finally(() => {
      this.#tickPromise = null;
    });
    return this.#tickPromise;
  }

  async #doTick(): Promise<void> {
    const startedAt = this.#now();
    const now = startedAt;
    const runningJobs = this.#jobIdsRunning();
    if (!this.#hasWork(now, runningJobs)) return;
    let samples: ProcInfo[];
    try {
      samples = await this.#deps.platform.enumerateTree();
      this.#pollCount += 1;
    } catch {
      // A failed enumeration pass must not fabricate exits; retry next tick.
      // Persistent failure also makes every reported stat unreliable.
      this.#noteFailure(now);
      return;
    }
    this.#diff(samples, now, runningJobs);
    await this.#sampleStats(now);
    this.#purge(now);
    this.#lastPollMs = this.#now() - startedAt;
  }

  #noteFailure(now: number): void {
    this.#consecutiveFailures += 1;
    this.#sampleFailuresTotal += 1;
    if (this.#consecutiveFailures >= STATS_STALE_FAILURES) this.#statsStale = true;
    const shift = Math.min(this.#consecutiveFailures - 1, 8);
    this.#statsBackoffUntil = now + Math.min(this.#pollMs * 2 ** shift, STATS_BACKOFF_MAX_MS);
  }

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
  #openCandidates(now: number, runningJobs: Set<string>): Invocation[] {
    const out: Invocation[] = [];
    for (const inv of this.#invocations.values()) {
      if (this.#isOpen(inv, now, runningJobs)) out.push(inv);
    }
    return out;
  }

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
    this.#spawnCount += 1;
  }

  async #sampleStats(now: number): Promise<void> {
    if (now < this.#statsBackoffUntil) return;
    const live = [...this.#procs.values()].filter((proc) => !proc.exitedAt);
    if (live.length === 0) return;
    let stats: Map<number, ProcStats>;
    try {
      stats = await this.#deps.platform.sampleStats(live.map((proc) => proc.pid));
    } catch {
      this.#noteFailure(now);
      return;
    }
    this.#consecutiveFailures = 0;
    this.#statsStale = false;
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

  #directoryOfOwner(ownerKey: string): string {
    if (ownerKey.startsWith(UNATTRIBUTED_PREFIX)) {
      const root = this.#procs.get(Number(ownerKey.slice(UNATTRIBUTED_PREFIX.length)));
      const first = root?.candidateSessionIds?.[0];
      return (first ? this.#sessionDirs.get(first) : undefined) ?? '';
    }
    return this.#invocations.get(ownerKey)?.directory ?? '';
  }

  #markDirty(directory: string): void {
    this.#revision += 1;
    if (directory) this.#dirtyDirectories.add(directory);
    this.#schedulePublish();
  }

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

  #membersOf(ownerKey: string): TrackedProc[] {
    const members: TrackedProc[] = [];
    for (const proc of this.#procs.values()) {
      if (proc.ownerKey === ownerKey) members.push(proc);
    }
    members.sort((a, b) => a.firstSeenAt - b.firstSeenAt || a.pid - b.pid);
    return members;
  }

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

  #entryStatus(inv: Invocation | null, members: TrackedProc[], killed: boolean): OmpProcessEntry['status'] {
    if (killed || members.some((proc) => proc.killedByUser)) return 'killed';
    if (members.some((proc) => !proc.exitedAt)) return 'running';
    if (!inv?.endedAt) return 'running';
    return inv.isError ? 'failed' : 'exited';
  }

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
      if (this.#statsStale && entry.liveCount > 0) entry.statsStale = true;
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
      if (this.#statsStale && entry.liveCount > 0) entry.statsStale = true;
      entries.push(entry);
    }
    entries.sort((a, b) => {
      const rank = (entry: OmpProcessEntry) => (entry.status === 'running' ? 0 : 1);
      return rank(a) - rank(b) || b.startedAt - a.startedAt;
    });
    return { revision: this.#revision, generatedAt: now, entries };
  }

  /** Counters-only health view for `/omp/diagnostics` — no argv/payload. */
  diagnostics() {
    const now = this.#now();
    const runningJobs = this.#jobIdsRunning();
    let liveProcesses = 0;
    for (const proc of this.#procs.values()) {
      if (!proc.exitedAt) liveProcesses += 1;
    }
    let openWindows = 0;
    for (const inv of this.#invocations.values()) {
      if (this.#isOpen(inv, now, runningJobs)) openWindows += 1;
    }
    return {
      pollCount: this.#pollCount,
      lastPollMs: this.#lastPollMs,
      spawnCount: this.#spawnCount,
      trackedProcesses: this.#procs.size,
      liveProcesses,
      openWindows,
      sampleFailures: this.#sampleFailuresTotal,
      consecutiveFailures: this.#consecutiveFailures,
      statsStale: this.#statsStale,
    };
  }

  output(key: string): OmpProcessOutput | null {
    const inv = this.#invocations.get(key);
    if (!inv) return null;
    const live = this.#membersOf(key).some((proc) => !proc.exitedAt) || inv.endedAt === undefined;
    return { key, output: inv.outputTail, truncated: inv.outputTruncated, live };
  }

  // -------------------------------------------------------------------------
  // Kill
  // -------------------------------------------------------------------------

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
