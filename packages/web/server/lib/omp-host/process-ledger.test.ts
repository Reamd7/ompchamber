// process-ledger tests — PLAN-session-process-monitor.md §验证.
//
// Contracts under test:
//  - attribution: parent inheritance, single-window, argv disambiguation,
//    job-extended windows, unattributed retention with candidate sessions;
//    zero-candidate infra spawns never enter the ledger;
//  - lifecycle: exit marking, pid-reuse separation, retention purge;
//  - kill: entry/tree scope, single-pid scope, ownership guards, argv
//    identity check, job cancellation;
//  - transport: revision bumps coalesce into one publish per directory.
/**
 * ProcessLedger（会话进程台账）的行为契约测试，对应
 * PLAN-session-process-monitor.md §验证。
 *
 * 通过注入假 platform（可变进程树）、可控单调时钟与可手动触发的
 * interval/timeout 定时器，覆盖四组契约：归属（attribution，含 job
 * 延展窗口与零候选基础设施 spawn 不入账）、生命周期（退出标记、
 * pid 复用隔离）、kill（范围/所有权/argv 身份/job 取消）、传输
 * （revision 递增按目录合并为一次 publish）。
 */

import { describe, expect, test } from 'bun:test';
import { ProcessLedger } from './process-ledger.ts';
import type { ProcInfo, ProcStats, ProcessPlatform } from './process-platform.ts';

/** 测试基准目录（所有窗口与快照查询共用）。 */
const DIR = '/repo';
/** 会话 A 的 session id。 */
const SES_A = 'ses_a';
/** 会话 B 的 session id。 */
const SES_B = 'ses_b';

/** 假想的宿主进程 pid，作为测试进程树的根。 */
const HOST_PID = 1000;

/** 构造一条 ProcInfo：pid 同时充当 pgid，ppid 与 argv 由参数给定。 */
const proc = (pid: number, ppid: number, argv: string): ProcInfo => ({ pid, ppid, argv, pgid: pid });

/** 可变的进程平台假面：`tree` 是每轮 enumerateTree 能看到的进程树；
 *  terminate 记录被杀 pid 并将其从存活集合剔除，供副作用断言。 */
/** Mutable fake OS surface: `tree` is what each enumerate pass sees. */
const fakePlatform = (tree: ProcInfo[], stats?: Map<number, ProcStats>) => {
  const terminated: number[] = [];
  const alive = new Set(tree.map((p) => p.pid));
  const platform: ProcessPlatform = {
    enumerateTree: () => Promise.resolve(tree),
    sampleStats: (pids) =>
      Promise.resolve(new Map(pids.flatMap((pid) => (stats?.has(pid) ? [[pid, stats.get(pid)!]] : [])))),
    cwdOf: () => null,
    isAlive: (pid, argv) => alive.has(pid) && tree.find((p) => p.pid === pid)?.argv === argv,
    terminate: (pid) => {
      terminated.push(pid);
      alive.delete(pid);
      return Promise.resolve(true);
    },
  };
  return { platform, terminated, alive };
};

/** 定时器回调：interval 周期采样与延迟 publish 共用的空参函数。 */
type TimerFn = () => void;
/** 捕获注入的 interval/timeout 回调，让测试手动触发以驱动台账节拍。 */
interface TimerCapture {
  /** 周期采样回调（由 setIntervalFn 注入）。 */
  interval?: TimerFn;
  /** 延迟合并 publish 回调（由 setTimeoutFn 注入）。 */
  timeout?: TimerFn;
}

/** 构造一个定时器完全由测试驱动的台账实例：注入假 platform、可前进的
 *  单调时钟、publish 记录数组与定时器捕获；advance(ms) 用于推进时钟。 */
/** A ledger whose timers are fully test-driven. `tick()` is awaitable. */
const setup = (
  tree: ProcInfo[],
  extra?: Partial<ConstructorParameters<typeof ProcessLedger>[0]> & { stats?: Map<number, ProcStats> },
) => {
  const { stats, ...deps } = extra ?? {};
  const timers: TimerCapture = {};
  const published: string[] = [];
  const { platform, terminated } = fakePlatform(tree, stats);
  let clock = 10_000;
  const ledger = new ProcessLedger({
    platform,
    now: () => clock,
    publishUpdate: (directory) => published.push(directory),
    setIntervalFn: (fn) => {
      timers.interval = fn;
      return {};
    },
    clearIntervalFn: () => {},
    setTimeoutFn: (fn) => {
      timers.timeout = fn;
      return {};
    },
    clearTimeoutFn: () => {},
    ...deps,
  });
  return {
    ledger,
    published,
    terminated,
    timers,
    advance: (ms: number) => {
      clock += ms;
    },
  };
};

/** 触发一次 bash 工具开始事件，为该 toolCallId 打开归属窗口。 */
const startBash = (ledger: ProcessLedger, sessionID: string, toolCallId: string, command: string) =>
  ledger.onToolStart({ sessionID, directory: DIR, toolCallId, toolName: 'bash', args: { command } });

/** 触发一次 bash 工具结束事件；details.async.jobId 存在时窗口转为 job 延展。 */
const endBash = (ledger: ProcessLedger, sessionID: string, toolCallId: string, details?: { async?: { jobId?: string } }) =>
  ledger.onToolEnd({ sessionID, directory: DIR, toolCallId, toolName: 'bash', isError: false, details });

/** 归属契约：单一窗口吸收、父进程继承、argv 消歧、未归属保留与 job 延展窗口。 */
describe('ProcessLedger — attribution', () => {
  test('a process first seen inside the single open window attributes by window', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 600')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 600');
    await ledger.tick();
    const snap = ledger.snapshot(DIR);
    expect(snap.entries).toHaveLength(1);
    const entry = snap.entries[0]!;
    expect(entry.sessionID).toBe(SES_A);
    expect(entry.kind).toBe('bash');
    expect(entry.status).toBe('running');
    expect(entry.processes[0]!.attribution).toBe('window');
    expect(entry.processes[0]!.pid).toBe(2001);
  });

  test('a lone open window does not absorb argv-foreign host spawns', async () => {
    const tree = [proc(2001, HOST_PID, 'bun src/cli.ts __omp_worker_daemon_broker')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'curl -s http://localhost/health');
    await ledger.tick();
    const snap = ledger.snapshot(DIR);
    const entry = snap.entries.find((e) => e.sessionID === null)!;
    expect(entry.attribution).toBe('unattributed');
    expect(entry.candidateSessionIds).toEqual([SES_A]);
    expect(entry.processes[0]!.pid).toBe(2001);
  });

  test('children of an attributed process inherit the owner (parent)', async () => {
    const tree = [proc(2001, HOST_PID, 'bash -c dev server'), proc(2002, 2001, 'node dev-server.js')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'npm run dev');
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries[0]!;
    expect(entry.processes).toHaveLength(2);
    const child = entry.processes.find((p) => p.pid === 2002)!;
    expect(child.attribution).toBe('parent');
    expect(entry.liveCount).toBe(2);
  });

  test('overlapping windows disambiguate on argv tokens', async () => {
    const tree = [proc(2001, HOST_PID, 'python -m http.server 8000')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'python -m http.server 8000');
    startBash(ledger, SES_B, 'call_2', 'sleep 600');
    await ledger.tick();
    const snap = ledger.snapshot(DIR);
    const entry = snap.entries.find((e) => e.sessionID === SES_A)!;
    expect(entry.processes[0]!.attribution).toBe('argv');
  });

  test('overlapping windows with no argv match stay unattributed and killable', async () => {
    const tree = [proc(2001, HOST_PID, 'some-daemon --flag')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'npm run dev');
    startBash(ledger, SES_B, 'call_2', 'make build');
    await ledger.tick();
    const snap = ledger.snapshot(DIR);
    const entry = snap.entries.find((e) => e.sessionID === null)!;
    expect(entry.attribution).toBe('unattributed');
    expect(entry.candidateSessionIds).toEqual([SES_A, SES_B]);
    const kill = await ledger.kill({ sessionID: SES_A, key: entry.key });
    expect(kill.ok).toBe(true);
    expect(kill.killed).toBe(1);
  });

  test('a spawn with no open window (infra) is never tracked', async () => {
    const tree = [proc(2001, HOST_PID, 'mcp-server --stdio')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'echo hi');
    endBash(ledger, SES_A, 'call_1');
    // Let the grace window lapse, then the infra process appears.
    const snap = ledger.snapshot(DIR);
    expect(snap.entries).toHaveLength(0);
  });

  test('a job-backed invocation keeps its window open past tool end', async () => {
    const tree: ProcInfo[] = [];
    const { ledger, timers } = setup(tree, { runningJobIds: () => ['job_1'] });
    startBash(ledger, SES_A, 'call_1', 'sleep 600');
    endBash(ledger, SES_A, 'call_1', { async: { jobId: 'job_1' } });
    // Job still running long past grace: a new process must still attribute.
    const { ledger: _ } = { ledger };
    tree.push(proc(2001, HOST_PID, 'sleep 600'));
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries[0]!;
    expect(entry.jobId).toBe('job_1');
    expect(entry.processes[0]!.attribution).toBe('job');
    expect(timers.interval).toBeDefined();
  });
});

/** 生命周期契约：消失 pid 标记 exited、pid 复用换新行、cpu 百分比由累计差分推导。 */
describe('ProcessLedger — lifecycle', () => {
  test('a disappeared pid is marked exited, not dropped', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 5')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 5');
    await ledger.tick();
    tree.length = 0;
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries[0]!;
    expect(entry.processes[0]!.state).toBe('exited');
    expect(entry.liveCount).toBe(0);
  });

  test('a reused pid becomes a fresh process, the stale row exits', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 5')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 5');
    await ledger.tick();
    tree[0] = proc(2001, HOST_PID, 'other-tool --x');
    startBash(ledger, SES_A, 'call_2', 'other-tool --x');
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries.find((e) => e.key.endsWith('call_2'))!;
    expect(entry.processes[0]!.pid).toBe(2001);
    expect(entry.status).toBe('running');
  });

  test('cpu percent derives from cumulative cpuMs deltas', async () => {
    const tree = [proc(2001, HOST_PID, 'burn')];
    const stats = new Map<number, ProcStats>([[2001, { cpuMs: 0, rssBytes: 1024 }]]);
    const { ledger, advance } = setup(tree, { stats, cpuCores: 4 });
    startBash(ledger, SES_A, 'call_1', 'burn');
    await ledger.tick();
    // Second sample: +1000ms cpu over +2000ms wall, 4 cores → 12.5%
    stats.set(2001, { cpuMs: 1000, rssBytes: 2048 });
    advance(2000);
    await ledger.tick();
    const member = ledger.snapshot(DIR).entries[0]!.processes[0]!;
    expect(Math.abs((member.cpuPercent ?? -1) - 12.5)).toBeLessThan(0.01);
    expect(member.rssBytes).toBe(2048);
  });
});

/** 输出尾巴契约：onToolUpdate 增量累积且按 outputTailBytes 字节上限截断。 */
describe('ProcessLedger — output tail', () => {
  test('updates accumulate and the tail is bounded', async () => {
    const tree = [proc(2001, HOST_PID, 'cat')];
    const { ledger } = setup(tree, { outputTailBytes: 16 });
    startBash(ledger, SES_A, 'call_1', 'cat');
    ledger.onToolUpdate({ sessionID: SES_A, directory: DIR, toolCallId: 'call_1', text: '0123456789' });
    ledger.onToolUpdate({ sessionID: SES_A, directory: DIR, toolCallId: 'call_1', text: 'abcdefghij' });
    const out = ledger.output(`${SES_A} call_1`)!;
    expect(out.output).toBe('456789abcdefghij');
    expect(out.truncated).toBe(true);
    expect(out.live).toBe(true);
  });
});

/** kill 契约：entry 树级终止、跨会话禁止、argv 不匹配跳过、job 取消联动。 */
describe('ProcessLedger — kill', () => {
  test('entry kill terminates every live member and marks the entry', async () => {
    const tree = [proc(2001, HOST_PID, 'parent'), proc(2002, 2001, 'child')];
    const { ledger, terminated } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'parent');
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries[0]!;
    const result = await ledger.kill({ sessionID: SES_A, key: entry.key });
    expect(result.ok).toBe(true);
    expect(result.killed).toBe(2);
    expect(terminated.sort()).toEqual([2001, 2002]);
    expect(ledger.snapshot(DIR).entries[0]!.status).toBe('killed');
  });

  test('a wrong session cannot kill another session\'s entry', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 5')];
    const { ledger } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 5');
    await ledger.tick();
    const entry = ledger.snapshot(DIR).entries[0]!;
    const result = await ledger.kill({ sessionID: SES_B, key: entry.key });
    expect(result.ok).toBe(false);
    expect(result.error).toBe('forbidden');
  });

  test('a pid that no longer matches its argv is skipped, not killed', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 5')];
    const { ledger, terminated } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 5');
    await ledger.tick();
    // Reuse: same pid now runs something else on the OS side.
    tree[0] = proc(2001, HOST_PID, 'reused --different');
    const result = await ledger.kill({ sessionID: SES_A, key: `${SES_A} call_1`, pid: 2001 });
    expect(result.killed).toBe(0);
    expect(result.skipped).toEqual([2001]);
    expect(terminated).toEqual([]);
  });

  test('killing a job-backed entry also cancels the SDK job', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 600')];
    const cancelled: string[] = [];
    const { ledger } = setup(tree, { cancelJob: (jobId) => cancelled.push(jobId) });
    startBash(ledger, SES_A, 'call_1', 'sleep 600');
    endBash(ledger, SES_A, 'call_1', { async: { jobId: 'job_9' } });
    await ledger.tick();
    await ledger.kill({ sessionID: SES_A, key: `${SES_A} call_1` });
    expect(cancelled).toEqual(['job_9']);
  });
});

/** 传输契约：同目录多次变更合并为一次 publish，revision 单调递增。 */
describe('ProcessLedger — transport', () => {
  test('changes coalesce into one publish per directory', async () => {
    const tree = [proc(2001, HOST_PID, 'sleep 5')];
    const { ledger, published, timers } = setup(tree);
    startBash(ledger, SES_A, 'call_1', 'sleep 5');
    endBash(ledger, SES_A, 'call_1');
    await ledger.tick();
    timers.timeout?.();
    expect(published).toEqual([DIR]);
    expect(ledger.revision).toBeGreaterThan(0);
  });
});
