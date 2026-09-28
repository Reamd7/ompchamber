/**
 * Parsers for listening-socket enumeration.
 *
 * Two platforms, two formats, one shape out. Both parsers are pure so the
 * fiddly parts — grouped records, IPv6 brackets, wildcard binds — are covered
 * by tests instead of by running the tools.
 */

/**
 * 监听 socket 枚举的各平台解析器（中文说明）。
 *
 * 两个平台、两种输出格式，统一归一为同一种 { port, pid, command } 形态。
 * 解析器全部为纯函数，分组记录、IPv6 方括号、通配绑定这些细节由测试
 * 覆盖，无需真的运行 lsof/netstat。
 */
/** Hosts that mean "this machine" when a socket reports its bind address. */
/** 含义为"本机"的绑定地址（loopback 与通配符）。 */
const LOOPBACK_TOKENS = new Set(['127.0.0.1', 'localhost', '[::1]', '::1']);
/** Wildcard binds are reachable over loopback too. */
/** 通配绑定同样可以经 loopback 访问。 */
const WILDCARD_TOKENS = new Set(['*', '0.0.0.0', '[::]', '::']);

/**
 * Ports that are listening but are never the thing a user wants to preview.
 * Kept deliberately short: guessing too aggressively hides real dev servers.
 */
/** 在监听但用户绝不会想预览的端口；刻意保持很短：猜得太激进会藏掉真正的 dev server。 */
const IGNORED_PORTS = new Set([
  22,    // ssh
  53,    // dns
  445,   // smb
  631,   // cups
  5432,  // postgres
  3306,  // mysql
  6379,  // redis
  27017, // mongodb
  9229,  // node inspector
]);

/** 拆出 "host:port"；兼容 [::1]:5173 形式的方括号 IPv6，格式非法时返回 null。 */
const splitHostPort = (value) => {
  const raw = String(value || '').trim();
  if (!raw) return null;

  // IPv6 arrives bracketed: [::1]:5173
  if (raw.startsWith('[')) {
    const close = raw.indexOf(']');
    if (close === -1) return null;
    const host = raw.slice(0, close + 1);
    const rest = raw.slice(close + 1);
    if (!rest.startsWith(':')) return null;
    return { host, port: rest.slice(1) };
  }

  const separator = raw.lastIndexOf(':');
  if (separator === -1) return null;
  return { host: raw.slice(0, separator), port: raw.slice(separator + 1) };
};

/** 字符串转 1..65535 的合法端口号，非法时返回 null。 */
const toPort = (value) => {
  const port = Number.parseInt(String(value || '').trim(), 10);
  return Number.isInteger(port) && port > 0 && port <= 65535 ? port : null;
};

/**
 * True when a bind address can be reached from this machine over loopback.
 * A socket bound to a specific LAN address only is intentionally excluded:
 * `http://localhost:<port>` would not reach it.
 */
/** 该绑定地址能否从本机经 loopback 访问（loopback 或通配绑定均视为可达）。 */
export const isLocallyReachableHost = (host) => {
  const value = String(host || '').trim().toLowerCase();
  return LOOPBACK_TOKENS.has(value) || WILDCARD_TOKENS.has(value);
};

/** 端口是否属于永远不作为预览目标的已知基础设施端口。 */
const isIgnoredDevPort = (port) => IGNORED_PORTS.has(port);

/**
 * Parses `lsof -iTCP -sTCP:LISTEN -P -n -F pcn`.
 *
 * The `-F` format emits one field per line, prefixed by a letter, and is
 * stateful: `p`/`c` lines open a process record and every following `n` line
 * belongs to it until the next `p`. A single process commonly reports the same
 * port twice (IPv4 and IPv6), so results are de-duplicated by port.
 */
/** 解析 lsof -F pcn 输出：按进程分组归并 socket，按端口去重，结果按端口升序。 */
export const parseLsofListeners = (output) => {
  const byPort = new Map();
  let pid = null;
  let command = '';

  for (const line of String(output || '').split('\n')) {
    if (!line) continue;
    const tag = line[0];
    const value = line.slice(1);

    if (tag === 'p') {
      const parsedPid = Number.parseInt(value, 10);
      pid = Number.isInteger(parsedPid) ? parsedPid : null;
      command = '';
      continue;
    }
    if (tag === 'c') {
      command = value.trim();
      continue;
    }
    if (tag !== 'n') continue;

    // `n` values look like `*:5173`, `127.0.0.1:5173`, or `[::1]:5173`.
    // Established sockets contain `->`; LISTEN filtering should exclude them,
    // but the guard keeps a mixed invocation honest.
    if (value.includes('->')) continue;

    const parsed = splitHostPort(value);
    if (!parsed) continue;
    const port = toPort(parsed.port);
    if (port === null) continue;
    if (!isLocallyReachableHost(parsed.host)) continue;

    const existing = byPort.get(port);
    if (existing && existing.pid !== null) continue;
    byPort.set(port, { port, pid, command });
  }

  return [...byPort.values()].sort((left, right) => left.port - right.port);
};

/**
 * Parses `netstat -ano -p TCP` on Windows, where no per-process command name is
 * available without a second call; `command` stays empty and callers fall back
 * to the port alone.
 */
/** 解析 Windows 的 netstat -ano -p TCP 输出：无进程名可用（command 留空），按端口去重并升序。 */
export const parseNetstatListeners = (output) => {
  const byPort = new Map();

  for (const line of String(output || '').split('\n')) {
    const parts = line.trim().split(/\s+/);
    if (parts.length < 4) continue;
    if (!/^tcp$/i.test(parts[0])) continue;
    if (!/^LISTENING$/i.test(parts[3])) continue;

    const parsed = splitHostPort(parts[1]);
    if (!parsed) continue;
    const port = toPort(parsed.port);
    if (port === null) continue;
    if (!isLocallyReachableHost(parsed.host)) continue;

    const pid = Number.parseInt(parts[4] ?? '', 10);
    if (byPort.has(port)) continue;
    byPort.set(port, { port, pid: Number.isInteger(pid) ? pid : null, command: '' });
  }

  return [...byPort.values()].sort((left, right) => left.port - right.port);
};

/**
 * Narrows raw listeners to the ones worth offering as a preview target.
 *
 * `ownPorts` removes OMPChamber's own listeners — offering the user a preview
 * of the app they are already looking at is pure noise.
 */
/** 从原始监听列表里筛出值得作为预览目标的条目：剔除自身端口/进程与已知基础设施端口。 */
export const selectDevServerCandidates = (listeners, { ownPorts = [], ownPids = [] } = {}) => {
  const excludedPorts = new Set(ownPorts.filter((port) => Number.isInteger(port)));
  const excludedPids = new Set(ownPids.filter((pid) => Number.isInteger(pid)));

  return listeners.filter((entry) => {
    if (excludedPorts.has(entry.port)) return false;
    if (entry.pid !== null && excludedPids.has(entry.pid)) return false;
    if (isIgnoredDevPort(entry.port)) return false;
    return true;
  });
};

/** Linux reports LISTEN as state 0A in /proc/net/tcp. */
/** Linux 在 /proc/net/tcp 中用状态码 0A 表示 LISTEN。 */
const PROC_STATE_LISTEN = '0A';
/** Wildcard binds, as /proc writes them: IPv4 0.0.0.0 and IPv6 :: */
/** /proc 写法的通配绑定：IPv4 0.0.0.0 与 IPv6 ::。 */
const PROC_WILDCARD_ADDRESSES = new Set(['00000000', '00000000000000000000000000000000']);
/** Loopback: 127.0.0.1 (little-endian per word) and ::1 */
/** loopback 地址：127.0.0.1（按字小端序）与 ::1。 */
const PROC_LOOPBACK_ADDRESSES = new Set(['0100007F', '00000000000000000000000001000000']);

/**
 * Parses `/proc/net/tcp` and `/proc/net/tcp6`.
 *
 * The fallback for hosts without `lsof`, which is most containers — and a
 * deployed OMPChamber is exactly where a dev server needs discovering. Reads a
 * kernel file rather than shelling out, so it cannot be defeated by a missing
 * binary or a stripped PATH.
 *
 * No process name or pid: mapping a socket to its owner means walking every
 * /proc/<pid>/fd, which is far more work than the label is worth.
 */
/** 解析 /proc/net/tcp 与 tcp6：无 lsof 环境（多数容器）的兜底；无进程名与 pid，按端口去重并升序。 */
export const parseProcNetTcpListeners = (output) => {
  const byPort = new Map();

  for (const line of String(output || '').split('\n')) {
    const parts = line.trim().split(/\s+/);
    // sl, local_address, rem_address, st, ...
    if (parts.length < 4) continue;
    if (parts[3] !== PROC_STATE_LISTEN) continue;

    const [address, portHex] = String(parts[1] || '').split(':');
    if (!address || !portHex) continue;

    const normalizedAddress = address.toUpperCase();
    if (!PROC_WILDCARD_ADDRESSES.has(normalizedAddress) && !PROC_LOOPBACK_ADDRESSES.has(normalizedAddress)) {
      continue;
    }

    const port = Number.parseInt(portHex, 16);
    if (!Number.isInteger(port) || port <= 0 || port > 65535) continue;
    if (byPort.has(port)) continue;
    byPort.set(port, { port, pid: null, command: '' });
  }

  return [...byPort.values()].sort((left, right) => left.port - right.port);
};
