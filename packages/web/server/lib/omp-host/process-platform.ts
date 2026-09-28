/**
 * 会话进程监视器所需的 OS 进程访问层（中文说明，详见
 * PLAN-session-process-monitor.md）。
 *
 * 进程树枚举与终止经由 pi-natives `Process` 完成。隔离的包布局使
 * `@oh-my-pi/pi-natives` 不进入本包的说明符图，故此处不直接 import 该
 * addon：内嵌 SDK 在模块求值期已加载它，本模块复用那个常驻实例（见
 * findResidentNatives）。
 *
 * 资源数值不在原生面上，因此每个平台各有一个窄的按 pid 采样器：Linux 读
 * /proc 文件、macOS 单次 `ps` 调用、Windows 单次 PowerShell `Get-Process`。
 * 采样器只对 ledger 已追踪的 pid 运行——绝不做全进程表扫描。
 */

// OS process access for the session process monitor
// (PLAN-session-process-monitor.md).
//
// Tree enumeration and termination ride pi-natives `Process`. Isolated
// package layouts keep `@oh-my-pi/pi-natives` out of this package's
// specifier graph, so the addon is not imported here: the embedded SDK loads
// it at module-evaluation time, and this module reuses that resident
// instance (see `findResidentNatives`).
//
// Resource numbers are NOT in the native surface, so each platform gets a
// narrow per-pid stats reader: /proc files on Linux, one `ps` spawn on
// macOS, one PowerShell `Get-Process` call on Windows. Readers run only for
// pids the ledger already tracks — never a full process-table scan.

import fs from 'node:fs';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';

/** promisify 的 execFile，供 macOS/Windows 的子进程采样调用。 */
const execFileAsync = promisify(execFile);

/** 遍历宿主后代进程树时观测到的一个进程。 */
/** One process observed while walking the host's descendant tree. */
export interface ProcInfo {
  // 进程 id。
  pid: number;
  // 父进程 id（树遍历中的父节点）。
  ppid: number;
  // argv 拼接成的单行字符串（pid 复用守卫的比较基准）。
  argv: string;
  // 进程组 id；不可得时为 null。
  pgid: number | null;
}

/** 单 pid 的资源采样；读取失败的字段可能缺席。 */
/** Per-pid resource sample; either field may be absent on read failure. */
export interface ProcStats {
  // 常驻内存字节数。
  rssBytes?: number;
  // 累计 CPU 时间（毫秒）。
  cpuMs?: number;
}

/** 进程域的平台适配器接口：树枚举/终止由 pi-natives 驱动，资源采样按
 * 平台分派到各自的窄采样器。 */
export interface ProcessPlatform {
  // 枚举本宿主进程的全部后代。
  /** Every descendant of this host process. */
  enumerateTree(): Promise<ProcInfo[]>;
  // 采样已追踪 pid 的资源数值；缺失项代表未知。
  /** Resource numbers for already-tracked pids. Missing entries = unknown. */
  sampleStats(pids: number[]): Promise<Map<number, ProcStats>>;
  // 尽力而为的工作目录（Linux /proc readlink；其它平台恒 null）。
  /** Best-effort working directory (Linux /proc readlink; null elsewhere). */
  cwdOf(pid: number): string | null;
  // pid 存活且 argv 一致（防 pid 复用误认）时为 true。
  /** True when pid is alive and still the same program (argv guard against reuse). */
  isAlive(pid: number, argv: string): boolean;
  // 终止该进程及其观测到的子树：原生 terminate 先走子进程、再按
  // 礼貌信号 → 硬杀逐级升级。
  /**
   * Terminate the process and the subtree we observed for it. The native
   * `terminate` walks children first, then escalates polite → hard kill.
   */
  terminate(pid: number): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// pi-natives loading
// ---------------------------------------------------------------------------

/** pi-natives Process.fromPid 返回的进程引用（本模块消费的最小类型面）。 */
interface NativeProcessRef {
  // 进程 id。
  readonly pid: number;
  // 父进程 id；不可得时为 null。
  readonly ppid: number | null;
  // 读取 argv 数组。
  args(): string[];
  // 枚举直接子进程引用。
  children(): NativeProcessRef[];
  // 查询 running / exited 状态。
  status(): 'running' | 'exited';
  // 进程组 id；不可得时为 null。
  groupId(): number | null;
  // 原生终止：可选整组终止、宽限时长、总超时与信号参数。
  terminate(options?: { group?: boolean; gracefulMs?: number; timeoutMs?: number; signal?: unknown }): Promise<boolean>;
}

/** pi-natives Process 构造面。 */
interface NativeProcessCtor {
  // 按 pid 建立引用；进程不存在时返回 null。
  fromPid(pid: number): NativeProcessRef | null;
}

/** pi-natives addon 的模块导出形状。 */
interface NativesModule {
  // Process 构造器（fromPid 入口）。
  Process: NativeProcessCtor;
}

/** pi-natives 加载器 require 的 addon 文件名模式：
 * pi_natives.<platform>-<arch>[-variant].node。 */
/** The addon file pi-natives' loader `require`s: `pi_natives.<platform>-<arch>[-variant].node`. */
const NATIVE_ADDON_FILENAME = /[\\/]pi_natives\.[^\\/]+\.node$/;

/**
 * 找到内嵌 SDK 已加载进本进程的 pi_natives addon：扫描 CommonJS 缓存中
 * 文件名匹配的 .node 条目，并用 fromPid(自身 pid) 探测确认导出可用；
 * 找不到返回 null（进程域随之下线——能力降级，而非伪造空数据）。
 */
/**
 * Find the pi_natives addon the embedded SDK already loaded into this process.
 *
 * `@oh-my-pi/pi-coding-agent` imports `@oh-my-pi/pi-natives` at module
 * evaluation, and that loader `require`s the `.node` file by absolute path,
 * so by the time the engine constructs the addon sits in the CommonJS cache.
 * Reading it from there hands back the very instance the SDK uses (one
 * dlopen, one Tokio runtime) in every runtime shape: workspace source,
 * npm-installed source, and `bun build --compile` binaries.
 *
 * Resolving the package through `require.resolve` instead is not an option:
 * pi-natives is exports-restricted, and inside a compiled binary Bun's
 * standalone resolver spins forever on
 * `require.resolve('@oh-my-pi/pi-coding-agent/package.json')` whenever cwd
 * has no node_modules — which is every packaged desktop launch.
 */
const findResidentNatives = (): NativesModule | null => {
  const cache = createRequire(import.meta.url).cache;
  for (const filename of Object.keys(cache)) {
    if (!NATIVE_ADDON_FILENAME.test(filename)) continue;
    // SAFETY: the filename pins the pi_natives addon, whose export shape
    // native/index.d.ts declares; the fromPid probe fails closed otherwise.
    const mod = cache[filename]?.exports as NativesModule | undefined;
    if (!mod?.Process.fromPid(process.pid)) continue;
    return mod;
  }
  return null;
};

// ---------------------------------------------------------------------------
// Platform stats readers — only ever called with tracked pids
// ---------------------------------------------------------------------------

// Linux /proc: CLK_TCK is 100 on every supported kernel config; the stat
// fields sit after `comm`, which may itself contain spaces and parens, so
// parsing anchors on the last ')'.
/** Linux /proc stat 的时钟滴答率（所有受支持内核配置均为 100）。 */
const CLK_TCK = 100;
/** Linux 内存页大小（statm 的 rss 换算用）。 */
const PAGE_SIZE = 4096;

/** Linux 采样器：同步读 /proc/<pid>/stat 与 statm 得到 cpuMs/rssBytes；
 * stat 读取失败整体返回 null，statm 失败只缺席 rssBytes。 */
const linuxProcStats = (pid: number): ProcStats | null => {
  const stats: ProcStats = {};
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    const close = stat.lastIndexOf(')');
    const fields = stat.slice(close + 2).split(' ');
    // fields[0] = state (field 3 overall); utime/stime are fields 14/15.
    const utime = Number(fields[11]);
    const stime = Number(fields[12]);
    if (Number.isFinite(utime) && Number.isFinite(stime)) {
      stats.cpuMs = ((utime + stime) / CLK_TCK) * 1000;
    }
  } catch {
    return null;
  }
  try {
    const statm = fs.readFileSync(`/proc/${pid}/statm`, 'utf8');
    const resident = Number(statm.split(' ')[1]);
    if (Number.isFinite(resident)) stats.rssBytes = resident * PAGE_SIZE;
  } catch {
    // stat gone between the two reads; cpu alone is still useful.
  }
  return stats;
};

/** Linux 工作目录：readlink /proc/<pid>/cwd，任何失败返回 null。 */
const linuxCwdOf = (pid: number): string | null => {
  try {
    return fs.readlinkSync(`/proc/${pid}/cwd`);
  } catch {
    return null;
  }
};

/** 解析 ps 的 time= 输出（[[dd-]hh:]mm:ss，累计 CPU 时间）为毫秒数。 */
/** `ps` `time=` prints [[dd-]hh:]mm:ss — cumulative CPU time. */
const parsePsTime = (text: string): number | undefined => {
  const m = /^(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+)$/.exec(text.trim());
  if (!m) return undefined;
  const days = Number(m[1] ?? 0);
  const hours = Number(m[2] ?? 0);
  const minutes = Number(m[3]);
  const seconds = Number(m[4]);
  return ((days * 24 + hours) * 3600 + minutes * 60 + seconds) * 1000;
};

/** macOS 采样器：对已追踪 pid 发起单次 `ps` 调用并解析 pid/time/rss 三
 * 列；所列 pid 全部已死时 ps 以非零退出，按空结果处理。 */
const darwinSampleStats = async (pids: number[]): Promise<Map<number, ProcStats>> => {
  const out = new Map<number, ProcStats>();
  if (pids.length === 0) return out;
  try {
    const { stdout } = await execFileAsync('ps', ['-o', 'pid=,time=,rss=', '-p', pids.join(',')]);
    for (const line of stdout.split('\n')) {
      const parts = line.trim().split(/\s+/);
      if (parts.length < 3) continue;
      const pid = Number(parts[0]);
      if (!Number.isInteger(pid)) continue;
      const stats: ProcStats = {};
      const cpuMs = parsePsTime(parts[1]);
      if (cpuMs !== undefined) stats.cpuMs = cpuMs;
      const rssKb = Number(parts[2]);
      if (Number.isFinite(rssKb)) stats.rssBytes = rssKb * 1024;
      out.set(pid, stats);
    }
  } catch {
    // `ps` exits nonzero when every listed pid has died — treat as empty.
  }
  return out;
};

/** Get-Process | ConvertTo-Json 单行的声明形状；边界断言只做一次，后续
 * 每个字段读取都做有限性检查。 */
/** Declared shape of one `Get-Process | ConvertTo-Json` row; the boundary
 * cast asserts it once and every read below stays finite-checked. */
interface Win32ProcRow {
  // 进程 id。
  Id?: number;
  // 累计 CPU 秒数。
  CPU?: number;
  // 工作集字节数。
  WorkingSet64?: number;
}

/** Windows 采样器：单次 PowerShell Get-Process 批量查询并解析 JSON 行；
 * 命令异常死亡时仍尽力回收已写到 stdout 的部分行。 */
const win32SampleStats = async (pids: number[]): Promise<Map<number, ProcStats>> => {
  const out = new Map<number, ProcStats>();
  if (pids.length === 0) return out;
  // 解析并收集 stdout 中的 JSON 行（单行对象/多行数组皆可）。
  const collect = (stdout: string | undefined): void => {
    if (!stdout) return;
    try {
      const parsed: unknown = JSON.parse(stdout);
      // SAFETY: ConvertTo-Json emits an object for one row, an array for
      // many; every field read below is re-checked with isFinite/isInteger.
      const rows = (Array.isArray(parsed) ? parsed : [parsed]) as Win32ProcRow[];
      for (const row of rows) {
        const pid = row?.Id;
        if (pid === undefined || !Number.isInteger(pid)) continue;
        const stats: ProcStats = {};
        if (row.CPU !== undefined && Number.isFinite(row.CPU)) stats.cpuMs = row.CPU * 1000;
        if (row.WorkingSet64 !== undefined && Number.isFinite(row.WorkingSet64)) stats.rssBytes = row.WorkingSet64;
        out.set(pid, stats);
      }
    } catch {
      // Truncated JSON — nothing salvageable.
    }
  };
  try {
    const { stdout } = await execFileAsync('powershell.exe', [
      '-NoProfile', '-NonInteractive', '-Command',
      // `; exit 0`: Get-Process exits non-zero when any listed pid is dead
      // or protected, which would poison the whole batch every tick — the
      // rows it did read still land on stdout, so force a clean exit.
      `Get-Process -Id ${pids.join(',')} -ErrorAction SilentlyContinue | Select-Object Id,CPU,WorkingSet64 | ConvertTo-Json -Compress; exit 0`,
    ]);
    collect(stdout);
  } catch (err) {
    // Salvage partial rows if the command still emitted JSON before dying.
    const partial = (err as { stdout?: unknown }).stdout;
    collect(typeof partial === 'string' ? partial : undefined);
  }
  return out;
};

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/**
 * 构造平台适配器；本进程内无常驻 pi-natives addon 时返回 null（引擎随即
 * 不挂载 processes 域——能力降级，而不是伪造空数据）。
 */
/**
 * Build the platform adapter, or null when no pi-natives addon is resident
 * (the engine then leaves the processes domain unmounted — capability
 * degradation instead of fabricated empty data).
 */
export const createProcessPlatform = (): ProcessPlatform | null => {
  const natives = findResidentNatives();
  if (!natives) {
    console.warn('[omp-host] pi-natives addon not resident in this process; processes.v1 stays unavailable');
    return null;
  }
  const { Process } = natives;

  const platform = process.platform;

    // 平台分派的采样入口：Linux 逐 pid 读 /proc，macOS/win32 批量子进程，
    // 其余平台返回空表。
  const sampleStats = async (pids: number[]): Promise<Map<number, ProcStats>> => {
    if (platform === 'linux') {
      const out = new Map<number, ProcStats>();
      for (const pid of pids) {
        const stats = linuxProcStats(pid);
        if (stats) out.set(pid, stats);
      }
      return out;
    }
    if (platform === 'darwin') return darwinSampleStats(pids);
    if (platform === 'win32') return win32SampleStats(pids);
    return new Map();
  };

  // 深度优先收集后代进程信息（深度上限 32，防病态树楔死遍历）。
  const walk = (ref: NativeProcessRef, out: ProcInfo[], depth: number): void => {
    // Depth guard: a malicious or pathological tree cannot wedge the walk.
    if (depth > 32) return;
    let children: NativeProcessRef[];
    try {
      children = ref.children();
    } catch {
      return;
    }
    for (const child of children) {
      try {
        out.push({
          pid: child.pid,
          ppid: ref.pid,
          argv: child.args().join(' '),
          pgid: child.groupId(),
        });
      } catch {
        continue;
      }
      walk(child, out, depth + 1);
    }
  };

  // 从自身 pid 出发枚举整棵后代树。
  const enumerateTree = async (): Promise<ProcInfo[]> => {
    const self = Process.fromPid(process.pid);
    if (!self) return [];
    const out: ProcInfo[] = [];
    walk(self, out, 0);
    return out;
  };

  // 存活且 argv 一致才认定为同一程序（防 pid 复用误认）。
  const isAlive = (pid: number, argv: string): boolean => {
    const ref = Process.fromPid(pid);
    if (!ref) return false;
    try {
      return ref.status() === 'running' && ref.args().join(' ') === argv;
    } catch {
      return false;
    }
  };

  // 原生语义终止：先子后父、礼貌信号、宽限后硬杀；Windows 整树硬杀。
  const terminate = async (pid: number): Promise<boolean> => {
    const ref = Process.fromPid(pid);
    if (!ref) return false;
    try {
      // Native semantics: children first, polite signal, then hard-kill
      // after the grace; on Windows the tree is hard-killed (no signals).
      return await ref.terminate({ gracefulMs: 1500, timeoutMs: 4000 });
    } catch {
      return false;
    }
  };

  return {
    enumerateTree,
    sampleStats,
    cwdOf: platform === 'linux' ? linuxCwdOf : () => null,
    isAlive,
    terminate,
  };
};
