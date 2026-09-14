// OS process access for the session process monitor
// (PLAN-session-process-monitor.md).
//
// Tree enumeration and termination ride pi-natives `Process`, resolved
// through the SDK's dependency context: isolated package layouts keep
// `@oh-my-pi/pi-natives` out of this package's specifier graph, but the
// package directory sits next to `@oh-my-pi/pi-coding-agent` in every
// supported install layout (bun `.bun` store scope dirs, npm hoisted
// `node_modules/@oh-my-pi/*`). `omp-host-natives.js` resolves tool binaries
// the same way.
//
// Resource numbers are NOT in the native surface, so each platform gets a
// narrow per-pid stats reader: /proc files on Linux, one `ps` spawn on
// macOS, one PowerShell `Get-Process` call on Windows. Readers run only for
// pids the ledger already tracks — never a full process-table scan.

import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const execFileAsync = promisify(execFile);

/** One process observed while walking the host's descendant tree. */
export interface ProcInfo {
  pid: number;
  ppid: number;
  argv: string;
  pgid: number | null;
}

/** Per-pid resource sample; either field may be absent on read failure. */
export interface ProcStats {
  rssBytes?: number;
  cpuMs?: number;
}

export interface ProcessPlatform {
  /** Every descendant of this host process. */
  enumerateTree(): Promise<ProcInfo[]>;
  /** Resource numbers for already-tracked pids. Missing entries = unknown. */
  sampleStats(pids: number[]): Promise<Map<number, ProcStats>>;
  /** Best-effort working directory (Linux /proc readlink; null elsewhere). */
  cwdOf(pid: number): string | null;
  /** True when pid is alive and still the same program (argv guard against reuse). */
  isAlive(pid: number, argv: string): boolean;
  /**
   * Terminate the process and the subtree we observed for it. The native
   * `terminate` walks children first, then escalates polite → hard kill.
   */
  terminate(pid: number): Promise<boolean>;
}

// ---------------------------------------------------------------------------
// pi-natives loading
// ---------------------------------------------------------------------------

interface NativeProcessRef {
  readonly pid: number;
  readonly ppid: number | null;
  args(): string[];
  children(): NativeProcessRef[];
  status(): 'running' | 'exited';
  groupId(): number | null;
  terminate(options?: { group?: boolean; gracefulMs?: number; timeoutMs?: number; signal?: unknown }): Promise<boolean>;
}

interface NativeProcessCtor {
  fromPid(pid: number): NativeProcessRef | null;
}

interface NativesModule {
  Process: NativeProcessCtor;
}

/**
 * Locate an installed package directory without relying on its exports map:
 * `resolve('<name>/package.json')` is exports-restricted for the scoped SDK
 * packages, so fall back to walking the resolver's node_modules roots —
 * direct links, bun's hoist root, and bun's `.bun/<scope+pkg@ver>` store
 * entries (same traversal as omp-host-natives.js `resolvePackageDir`).
 */
const resolvePackageDir = (requireHere: NodeRequire, name: string): string | null => {
  try {
    // Bun's require.resolve may return a file:// URL string instead of a
    // filesystem path — normalize before path.dirname consumes it.
    const resolved = requireHere.resolve(`${name}/package.json`);
    return path.dirname(resolved.startsWith('file:') ? fileURLToPath(resolved) : resolved);
  } catch {
    // Exports-restricted or absent from the plain lookup paths.
  }
  const storePrefix = name.replace('/', '+') + '@';
  for (const root of requireHere.resolve.paths(name) ?? []) {
    for (const candidate of [path.join(root, name), path.join(root, '.bun', 'node_modules', name)]) {
      if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
    }
    try {
      for (const entry of fs.readdirSync(path.join(root, '.bun'))) {
        if (!entry.startsWith(storePrefix)) continue;
        const candidate = path.join(root, '.bun', entry, 'node_modules', name);
        if (fs.existsSync(path.join(candidate, 'package.json'))) return candidate;
      }
    } catch {
      // Not a bun store root.
    }
  }
  return null;
};

/** Directory candidates for `@oh-my-pi/pi-natives/native/index.js`. */
const nativesEntryCandidates = (): string[] => {
  const requireHere = createRequire(import.meta.url);
  const candidates: string[] = [];
  // Scoped sibling: every supported layout keeps both packages under the
  // same `node_modules/@oh-my-pi/` scope directory. realpath first — the
  // located dir may be a junction into the store scope that actually holds
  // pi-natives (bun isolated layout).
  const sdkDir = resolvePackageDir(requireHere, '@oh-my-pi/pi-coding-agent');
  if (sdkDir) {
    let realDir = sdkDir;
    try {
      realDir = fs.realpathSync(sdkDir);
    } catch {
      // Unresolved dir still yields a valid sibling candidate.
    }
    candidates.push(path.join(path.dirname(realDir), 'pi-natives', 'native', 'index.js'));
  }
  // Generic walk over the SDK's own resolution paths (npm nested layouts).
  for (const dir of requireHere.resolve.paths('@oh-my-pi/pi-natives') ?? []) {
    candidates.push(path.join(dir, '@oh-my-pi', 'pi-natives', 'native', 'index.js'));
  }
  return candidates;
};

let nativesPromise: Promise<NativesModule | null> | null = null;

const loadNatives = (): Promise<NativesModule | null> => {
  nativesPromise ??= (async (): Promise<NativesModule | null> => {
    try {
      for (const entry of nativesEntryCandidates()) {
        if (!fs.existsSync(entry)) continue;
        // SAFETY: pi-natives' native/index.js is its public native entry
        // (exports-restricted, so specifiers cannot reach it); the module's
        // shape is pinned by native/index.d.ts in the same directory, and the
        // fromPid probe below fails closed when it isn't.
        const mod = (await import(pathToFileURL(entry).href)) as NativesModule;
        if (!mod.Process.fromPid(process.pid)) return null;
        return mod;
      }
      return null;
    } catch (error) {
      console.warn('[omp-host] pi-natives unresolved; processes.v1 stays unavailable:', error);
      return null;
    }
  })();
  return nativesPromise;
};

// ---------------------------------------------------------------------------
// Platform stats readers — only ever called with tracked pids
// ---------------------------------------------------------------------------

// Linux /proc: CLK_TCK is 100 on every supported kernel config; the stat
// fields sit after `comm`, which may itself contain spaces and parens, so
// parsing anchors on the last ')'.
const CLK_TCK = 100;
const PAGE_SIZE = 4096;

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

const linuxCwdOf = (pid: number): string | null => {
  try {
    return fs.readlinkSync(`/proc/${pid}/cwd`);
  } catch {
    return null;
  }
};

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

/** Declared shape of one `Get-Process | ConvertTo-Json` row; the boundary
 * cast asserts it once and every read below stays finite-checked. */
interface Win32ProcRow {
  Id?: number;
  CPU?: number;
  WorkingSet64?: number;
}

const win32SampleStats = async (pids: number[]): Promise<Map<number, ProcStats>> => {
  const out = new Map<number, ProcStats>();
  if (pids.length === 0) return out;
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
 * Build the platform adapter, or null when pi-natives cannot be resolved
 * (the engine then leaves the processes domain unmounted — capability
 * degradation instead of fabricated empty data).
 */
export const createProcessPlatform = async (): Promise<ProcessPlatform | null> => {
  const natives = await loadNatives();
  if (!natives) return null;
  const { Process } = natives;

  const platform = process.platform;

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

  const enumerateTree = async (): Promise<ProcInfo[]> => {
    const self = Process.fromPid(process.pid);
    if (!self) return [];
    const out: ProcInfo[] = [];
    walk(self, out, 0);
    return out;
  };

  const isAlive = (pid: number, argv: string): boolean => {
    const ref = Process.fromPid(pid);
    if (!ref) return false;
    try {
      return ref.status() === 'running' && ref.args().join(' ') === argv;
    } catch {
      return false;
    }
  };

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

/** Test seam. */
export const __resetNativesForTests = (): void => {
  nativesPromise = null;
};
