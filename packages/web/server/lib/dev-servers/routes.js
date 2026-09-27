/**
 * Dev-server discovery.
 *
 * Answers "what is listening on this machine that I could preview". The old
 * approach guessed from `package.json` scripts, which told us what *could* be
 * started, never what was actually running — so it was wrong exactly when the
 * user needed it. Enumerating listening sockets reports the truth.
 *
 * Discovery is advisory. A failed scan reports failure; it never reports an
 * empty list, because a caller cannot tell "nothing is running" from "the scan
 * broke" and would render the wrong empty state.
 */
/**
 * dev server 发现（中文说明）：枚举本机真实监听的 socket，回答"有什么
 * 可以预览"。发现是 advisory 的：扫描失败时报告失败而非空列表，调用方
 * 才能区分"没在跑"与"扫描坏了"。
 */
import fsPromises from 'node:fs/promises';

import {
  parseLsofListeners,
  parseNetstatListeners,
  parseProcNetTcpListeners,
  selectDevServerCandidates,
} from './parse.js';

/** 单次枚举命令（lsof/netstat）的硬超时。 */
const SCAN_TIMEOUT_MS = 2_500;
/** Enumeration is cheap but not free; a short cache absorbs panel re-renders. */
/** 枚举便宜但不是免费；短缓存用于吸收面板的重复渲染。 */
const CACHE_TTL_MS = 3_000;

/**
 * 执行一个子命令并收集 stdout；spawn 抛错、进程出错、超时、或非零退出
 * 且无输出时统一返回 null（表示"该枚举来源不可用"），成功返回 stdout。
 * 结束时总是 kill 子进程并清理定时器，只结算一次。
 */
const runCommand = (spawn, command, args, timeoutMs) => new Promise((resolve) => {
  let child;
  try {
    child = spawn(command, args, { windowsHide: true, stdio: ['ignore', 'pipe', 'ignore'] });
  } catch {
    resolve(null);
    return;
  }

  let stdout = '';
  let settled = false;
  /** 结算子命令：保证只结算一次，清理定时器并 kill 子进程后再 resolve。 */
  const finish = (value) => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    try { child.kill(); } catch { /* already exited */ }
    resolve(value);
  };

  const timer = setTimeout(() => finish(null), timeoutMs);
  child.stdout?.on('data', (chunk) => { stdout += String(chunk); });
  child.on('error', () => finish(null));
  child.on('close', (code) => finish(code === 0 || stdout ? stdout : null));
});

/**
 * Reads the kernel's socket tables. Containers routinely ship without `lsof`,
 * and a deployed OMPChamber is precisely where discovery has to work, so this
 * is tried whenever the command is unavailable.
 */
/** 读 /proc/net/tcp 与 tcp6 并按端口合并去重；两表都读不到（非 Linux）时返回 null 表示来源不可用。 */
const readProcListeners = async (readFile) => {
  const tables = await Promise.all(['/proc/net/tcp', '/proc/net/tcp6'].map(
    (path) => readFile(path, 'utf8').catch(() => null),
  ));
  if (tables.every((table) => table === null)) return null;
  const byPort = new Map();
  for (const table of tables) {
    if (table === null) continue;
    for (const entry of parseProcNetTcpListeners(table)) {
      if (!byPort.has(entry.port)) byPort.set(entry.port, entry);
    }
  }
  return [...byPort.values()].sort((left, right) => left.port - right.port);
};

/**
 * 创建 dev server 扫描器：Windows 走 netstat，其它平台优先 lsof，
 * lsof 缺失时回退 /proc/net/tcp。成功结果短缓存，失败不缓存
 * （瞬时故障不应压制下一次尝试）。
 */
export const createDevServerScanner = ({ spawn, platform, readFile = fsPromises.readFile }) => {
  // 上一次成功发现的结果与时间戳。
  let cache = null;

  /** 执行一次平台相关的监听 socket 枚举；返回 { ok, listeners } 或 { ok: false, reason }。 */
  const scan = async () => {
    const isWindows = platform === 'win32';
    if (isWindows) {
      const output = await runCommand(spawn, 'netstat', ['-ano', '-p', 'TCP'], SCAN_TIMEOUT_MS);
      if (output === null) return { ok: false, reason: 'netstat-unavailable' };
      return { ok: true, listeners: parseNetstatListeners(output) };
    }

    const output = await runCommand(spawn, 'lsof', ['-iTCP', '-sTCP:LISTEN', '-P', '-n', '-F', 'pcn'], SCAN_TIMEOUT_MS);
    if (output !== null) return { ok: true, listeners: parseLsofListeners(output) };

    const procListeners = await readProcListeners(readFile);
    if (procListeners !== null) return { ok: true, listeners: procListeners };

    return { ok: false, reason: 'no-listener-source' };
  };

  return {
    /**
     * @param {{ ownPorts?: number[] }} options
     * @returns {Promise<{ ok: true, servers: Array<{ port: number, pid: number|null, command: string, url: string }> } | { ok: false, reason: string }>}
     */
    /** 发现可预览的 dev server：过滤自身端口/进程与基础设施端口并补上 localhost URL；命中缓存直接返回。 */
    async discover({ ownPorts = [] } = {}) {
      const now = Date.now();
      if (cache && now - cache.at < CACHE_TTL_MS) return cache.value;

      const result = await scan();
      if (!result.ok) {
        // Not cached: a transient failure should not suppress the next attempt.
        return result;
      }

      const servers = selectDevServerCandidates(result.listeners, {
        ownPorts,
        ownPids: [process.pid],
      }).map((entry) => ({
        ...entry,
        url: `http://localhost:${entry.port}/`,
      }));

      const value = { ok: true, servers };
      cache = { at: now, value };
      return value;
    },
  };
};

/** 在 express app 上注册 GET /api/dev-servers：返回候选列表；扫描不可用回 503，异常回 500。 */
export function registerDevServerRoutes(app, { scanner, getOwnPorts }) {
  // 发现路由：把自己的监听端口传给扫描器排除自身。
  app.get('/api/dev-servers', async (req, res) => {
    try {
      const ownPorts = typeof getOwnPorts === 'function' ? getOwnPorts() : [];
      const result = await scanner.discover({ ownPorts: Array.isArray(ownPorts) ? ownPorts : [] });
      if (!result.ok) {
        res.status(503).json({ error: 'Port discovery is unavailable', reason: result.reason });
        return;
      }
      res.json({ servers: result.servers });
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Port discovery failed' });
    }
  });
}
