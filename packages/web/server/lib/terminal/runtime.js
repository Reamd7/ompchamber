/**
 * 终端运行时（WebSocket 版）：为 Web UI 提供基于 node-pty/bun-pty 的交互式
 * shell 会话。核心职责：PTY 进程生命周期（创建/重启/优雅终止/空闲回收）、
 * 回放历史（512 KiB 尾部契约的 chunk deque）、输出事件多路广播（byte feed
 * 与 grid feed 两种按 attachment 订阅的模式）、发送端流量控制（ack 驱动的
 * lag 抑制）、多设备视口协商（隐式最小宽度 + 显式 driver 抢占）、
 * OSC 133 shell 集成与终端主题查询应答，以及 /api/terminal/* HTTP 路由
 * 与 /api/terminal/ws 的 upgrade 握手。
 */
import { randomUUID } from 'node:crypto';
import { WebSocketServer } from 'ws';
import {
  TERMINAL_WS_MAX_PAYLOAD_BYTES,
  TERMINAL_WS_PATH,
  createTerminalWsControlFrame,
  parseRequestPathname,
  readTerminalWsControlFrame,
} from './terminal-ws-protocol.js';
import { sanitizeTerminalHistoryChunk } from './history.js';
import { consumeTerminalThemeQueries, terminalThemeModeReport } from './theme-response.js';
import { Osc133Scanner, buildZshOsc133Wrapper, buildBashOsc133Rc } from './shell-integration.js';
import * as osModule from 'node:os';
import { createTerminalShellResolver, getTerminalShellLoginArgs, normalizeTerminalShell } from './shells.js';
import { stripAppImageArgv0Leak, resolveLinuxPtyLaunch } from '../inherited-env.js';
// Vendored from packages/terminal-server/src (same repo, synced on change):
// the published CLI tarball ships server sources raw, and a workspace
// dependency cannot resolve inside it — npm installs fail on the
// workspace:* specifier. A relative import travels with the tarball.
import { GridCore } from './vendor/ompchamber-terminal-server/index.mjs';
import { createRequire } from 'node:module';

/** 单个 server 实例允许同时存在的终端会话上限；超出后 create 返回 429。 */
const MAX_SESSIONS = 20;
/** 回放历史（snapshot.history）的字节上限：物化时只保留最近 512 KiB 尾部。 */
const MAX_HISTORY_BYTES = 512 * 1024;
/** 历史 deque 的松弛上限：允许暂留一个超限 chunk，物化快照仍受 MAX_HISTORY_BYTES 约束。 */
// History deque slack: one extra chunk may be retained before trimming, so
// materialized snapshots stay within MAX_HISTORY_BYTES + this slack.
const MAX_HISTORY_CHUNK_BYTES = 128 * 1024;
/** 发送端流控三阈值（字节）：进入抑制 / 退出抑制恢复 / 从不 ack 连接的兜底抑制。 */
// Send-side flow control. Bun's server WebSocket exposes no send-buffer
// signal and buffers unbounded data per socket, so consumers acknowledge the
// sequences they applied and each attachment tracks sent-but-unacknowledged
// bytes. Past SEND_LAG_ENTER_BYTES the attachment is suppressed: output
// frames are skipped for it (sequence numbers still advance, so its next
// live frame gap-triggers the client's resync), and it receives a snapshot
// once its lag drains below SEND_LAG_EXIT_BYTES. Connections that never
// acknowledge are suppressed after SEND_LAG_FALLBACK_BYTES as a fail-safe.
const SEND_LAG_ENTER_BYTES = 4 * 1024 * 1024;
/** 退出抑制阈值：积压降至此以下后恢复发送（先补发快照完成重同步）。 */
const SEND_LAG_EXIT_BYTES = 1 * 1024 * 1024;
/** 兜底阈值：从未 ack 过的连接发送总量超过即直接抑制。 */
const SEND_LAG_FALLBACK_BYTES = 8 * 1024 * 1024;
/** 单条 write 输入帧允许的最大字符数，超出即拒绝（BAD_INPUT）。 */
const MAX_INPUT_CHARS = 65_536;
/** 会话空闲回收窗口：无 attachment 且超过该时长无活动（claim/touch/输入）即被 sweep 强杀。 */
const IDLE_TIMEOUT_MS = 30 * 60 * 1000;
/** terminateProcess 先 SIGTERM 后升级 SIGKILL 的默认宽限期（可被注入覆盖）。 */
const TERMINATION_GRACE_MS = 1000;
/** 校验终端行列数是否为 1..max 的整数；用于拒绝非法 create/resize 参数。 */
const validateSize = (value, max) => Number.isInteger(value) && value >= 1 && value <= max;
/** 把历史字节裁剪到 MAX_HISTORY_BYTES 尾部；起始处跳过 UTF-8 续字节避免切出乱码。 */
const trimHistoryBytes = (bytes) => {
  if (bytes.byteLength <= MAX_HISTORY_BYTES) return bytes.toString('utf8');
  let start = bytes.byteLength - MAX_HISTORY_BYTES;
  while (start < bytes.byteLength && (bytes[start] & 0xc0) === 0x80) start += 1;
  return bytes.subarray(start).toString('utf8');
};
/** 清空历史 chunk deque（会话首次启动或重启时复位）。 */
// History is a chunk deque: appending is O(chunk) and the byte-exact 512 KiB
// tail contract is applied when the text is materialized (snapshot path
// only). A single accumulating string instead copies ~512 KiB per PTY chunk,
// which is gigabytes of memcpy under a flood.
const historyChunksReset = (session) => { session.historyChunks = []; session.historyBytes = 0; };
/** 追加一段可见输出到历史 deque；超过 MAX_HISTORY_BYTES+松弛量时从头部整块丢弃。 */
const appendHistory = (session, visible) => {
  const chunks = session.historyChunks;
  chunks.push(visible);
  session.historyBytes += Buffer.byteLength(visible);
  while (session.historyBytes > MAX_HISTORY_BYTES + MAX_HISTORY_CHUNK_BYTES && chunks.length > 1) {
    session.historyBytes -= Buffer.byteLength(chunks[0]);
    chunks.shift();
  }
};
/** 物化历史文本：拼接全部 chunk 并套用字节级尾部裁剪（snapshot.history 的数据源）。 */
const historyText = (session) => {
  const bytes = Buffer.from(session.historyChunks.join(''));
  // A dropped head chunk can split a UTF-8 sequence; skip leading
  // continuation bytes exactly as the tail trim does.
  let start = 0;
  while (start < bytes.byteLength && (bytes[start] & 0xc0) === 0x80) start += 1;
  return trimHistoryBytes(bytes.subarray(start));
};

/**
 * 构建终端运行时并把它挂载到传入的 app/server 上（HTTP 路由 + WS upgrade）。
 * @param {object} deps 依赖注入（便于测试替换）：app（express 实例）、server（http server，
 *   承接 'upgrade' 事件）、fs/path（文件系统抽象）、uiAuthController（UI 鉴权，升级前校验
 *   session token）、buildAugmentedPath/searchPathFor/isExecutable（PATH 检索工具）、
 *   isRequestOriginAllowed/rejectWebSocketUpgrade（origin 白名单与升级拒绝）、
 *   TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS（WS 心跳间隔）、loadPtyProvider（PTY 后端加载器，
 *   缺省时自动在 bun-pty 与 node-pty 间选择）、terminalTerminationGraceMs（终止宽限）
 * @returns {{ shutdown: () => Promise<void> }} 仅暴露 shutdown：摘除路由、终止全部会话、关闭 WS server
 */
export function createTerminalRuntime({
  app, server, fs, path, uiAuthController, buildAugmentedPath, searchPathFor, isExecutable,
  isRequestOriginAllowed, rejectWebSocketUpgrade, TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS,
  loadPtyProvider, terminalTerminationGraceMs = TERMINATION_GRACE_MS,
}) {
  // 会话表：sessionId -> session 状态对象（进程、历史 deque、grid、claims 等）。
  const sessions = new Map();
  // 进行中的 create 请求：并发同 id 创建只 spawn 一次，同时用于 cwd/shell/login 冲突检测。
  const pendingSessionCreates = new Map();
  // 每个 session 串行化的 restart promise 链，防止并发重启交错改写状态。
  const pendingSessionRestarts = new Map();
  // 活跃 WS 连接集合；每个 connection 持有 attachments: sessionId -> attachment 状态。
  const connections = new Set();
  // 尚未落定的 PTY 终止 promise；shutdown 时统一等待，避免僵尸进程。
  const pendingTerminations = new Set();
  // 宿主运行时标识（node|bun），随 snapshot 下发供客户端做能力判断。
  const runtime = typeof globalThis.Bun === 'undefined' ? 'node' : 'bun';
  // PTY provider 的单例加载 promise（bun-pty 优先，回退 node-pty）。
  let ptyProviderPromise = null;
  // noServer 模式的 WS server；maxPayload 限制单帧 64 KiB 防滥用。
  let wsServer = new WebSocketServer({ noServer: true, maxPayload: TERMINAL_WS_MAX_PAYLOAD_BYTES });
  // shell 发现/解析器（见 shells.js），服务于 spawnPty 与 /api/terminal/shells。
  const shellResolver = createTerminalShellResolver({ fs, path, searchPathFor, isExecutable, buildAugmentedPath });

  /** 惰性加载 PTY provider 并缓存 promise：Bun 下优先 bun-pty，失败或 Node 下回退 node-pty。 */
  const getPtyProvider = async () => {
    if (!ptyProviderPromise) {
      ptyProviderPromise = loadPtyProvider ? loadPtyProvider() : (async () => {
        if (typeof globalThis.Bun !== 'undefined') {
          try { const pty = await import('bun-pty'); return { spawn: pty.spawn, backend: 'bun-pty' }; } catch { /* fall through */ }
        }
        const pty = await import('node-pty');
        return { spawn: pty.spawn, backend: 'node-pty' };
      })();
    }
    return ptyProviderPromise;
  };

  // 探测 node-pty 自带的 conpty.dll 是否随包存在（仅 Windows，详见下方英文说明）。
  // Windows: the bundled conpty.dll lives next to node-pty's native module
  // (prebuilds/win32-*/conpty/conpty.dll). Resolve its presence once; when a
  // packaging gap omits it, spawnPty silently falls back to the OS-built-in
  // pseudoconsole instead of failing every terminal.
  let ptyDllUsable = false;
  if (process.platform === 'win32') {
    try {
      const pkgRoot = path.dirname(createRequire(import.meta.url).resolve('node-pty/package.json'));
      const arch = process.arch === 'arm64' ? 'win32-arm64' : 'win32-x64';
      ptyDllUsable = fs.existsSync(path.join(pkgRoot, 'prebuilds', arch, 'conpty', 'conpty.dll'));
    } catch {
      ptyDllUsable = false;
    }
  }

  /**
   * 解析 shell 并启动一个 PTY 子进程。
   * 依次尝试解析结果中的每个可执行文件；组装环境（增强 PATH、TERM/真彩色、按主题的
   * COLORFGBG），清理 daemon IPC fd 与 AppImage ARGV0 泄漏，注入 OSC 133 shell 集成，
   * Linux 上按需用 `env -u ARGV0` 包装启动；Windows 优先捆绑 conpty.dll（吞吐约 3x，
   * 加载失败则粘性回退内置 pseudoconsole）。
   * @returns {{ process: object, backend: string, shell: string, loginShell: boolean, conptyDll: boolean }}
   * @throws 全部候选失败时抛出最后一个错误（或 No executable shell found）
   */
  const spawnPty = async ({ cwd, cols, rows, themeMode, shell, loginShell }) => {
    const provider = await getPtyProvider();
    const resolvedShell = await shellResolver.resolve(shell);
    let lastError = null;
    for (const executable of resolvedShell.executables) {
      const args = loginShell ? getTerminalShellLoginArgs(executable) : [];
      if (!args) throw new Error(`Terminal shell "${resolvedShell.id}" does not support login mode`);
      try {
        const env = { ...process.env, PATH: buildAugmentedPath(), TERM: 'xterm-256color', COLORTERM: 'truecolor', COLORFGBG: themeMode === 'light' ? '0;15' : '15;0' };
        // The daemon's IPC fd is closed inside the PTY. An explicit override is
        // required because bun-pty also inherits Bun's native process environment.
        env.NODE_CHANNEL_FD = '';
        delete env.BASH_XTRACEFD; delete env.BASH_ENV; delete env.ENV; delete env.ELECTRON_RUN_AS_NODE;
        // AppImage exports ARGV0; zsh would otherwise rewrite argv[0] for every command (#2588).
        // bun-pty also merges the native OS environ, so wrap with `env -u ARGV0` on Linux.
        stripAppImageArgv0Leak(env);
        const shellIntegration = injectShellIntegration({ shellId: resolvedShell.id, executable, args, env, loginShell });
        const launch = resolveLinuxPtyLaunch(executable, shellIntegration ? shellIntegration.args : args);
        // Windows: prefer the conpty.dll bundled with node-pty (a current
        // Terminal-era build) over the OS-built-in pseudoconsole — measured
        // 3x read throughput on output floods (42 vs 14 MiB/s). Detection
        // only checks file presence (Electron's patched fs can see files
        // inside app.asar that LoadLibraryW cannot load), so a load failure
        // falls back to the built-in path for this and all future spawns.
        const wantDll = process.platform === 'win32' && ptyDllUsable;
        const baseOptions = { name: 'xterm-256color', cwd, cols, rows, env };
        let process_;
        let usedDll = false;
        if (wantDll) {
          try {
            process_ = provider.spawn(launch.executable, launch.args, { ...baseOptions, useConptyDll: true });
            usedDll = true;
          } catch (dllError) {
            ptyDllUsable = false; // sticky: one failed load retires DLL mode
            lastError = dllError;
          }
        }
        if (!process_) process_ = provider.spawn(launch.executable, launch.args, baseOptions);
        if (shellIntegration) shellIntegration.scheduleCleanup();
        return {
          process: process_, backend: provider.backend, shell: resolvedShell.id, loginShell,
          conptyDll: usedDll,
        };
      } catch (error) { lastError = error; }
    }
    throw lastError ?? new Error('No executable shell found');
  };

  /**
   * （中文说明）向 shell 启动参数注入 OSC 133 命令边界标记，运行时据此发布
   * command-finished 事件（未读角标等）。尽力而为：失败绝不阻塞 spawn。
   */
  /**
   * OSC 133 shell-integration injection: emit command-boundary markers so the
   * runtime can publish command-finished events (unread badges, OSC 133
   * consumers). zsh gets a temporary ZDOTDIR whose .zshenv hands control back
   * to the user's config immediately; bash gets an --rcfile that chains into
   * the user's bashrc. Injection is best-effort: skipped on Windows, for
   * login shells, when the injected fs lacks sync writes (tests), or when the
   * user opts out via OMPCHAMBER_NO_SHELL_INTEGRATION.
   * @returns {{ args: string[], scheduleCleanup: () => void } | null}
   */
  const injectShellIntegration = ({ shellId, executable, args, env, loginShell }) => {
    if (process.platform === 'win32') return null;
    if (loginShell) return null;
    if (process.env.OMPCHAMBER_NO_SHELL_INTEGRATION === '1') return null;
    if (!fs?.writeFileSync || !fs?.mkdtempSync || !fs?.rmSync) return null;
    if (!path?.join) return null;
    const os = osModule?.tmpdir ? osModule : null;
    if (!os) return null;
    const home = env.HOME || '/root';
    let wrapperFile = null;
    try {
      if (shellId === 'zsh' && !env.ZDOTDIR) {
        const zdotdir = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-zdot-'));
        wrapperFile = zdotdir;
        fs.writeFileSync(path.join(zdotdir, '.zshenv'), buildZshOsc133Wrapper(home));
        env.ZDOTDIR = zdotdir;
        return {
          args,
          scheduleCleanup: () => {
            const timer = setTimeout(() => { try { fs.rmSync(zdotdir, { recursive: true, force: true }); } catch { /* already gone */ } }, 60_000);
            timer.unref?.();
          },
        };
      }
      if (shellId === 'bash') {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-bash-'));
        const rcfile = path.join(dir, 'oc-bashrc');
        wrapperFile = dir;
        fs.writeFileSync(rcfile, buildBashOsc133Rc(path.join(home, '.bashrc')));
        return {
          args: ['--rcfile', rcfile, '-i', ...args],
          scheduleCleanup: () => {
            const timer = setTimeout(() => { try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* already gone */ } }, 60_000);
            timer.unref?.();
          },
        };
      }
    } catch {
      // Best-effort: a failed injection must never block the spawn.
      if (wrapperFile) { try { fs.rmSync(wrapperFile, { recursive: true, force: true }); } catch { /* ignore */ } }
      return null;
    }
    return null;
  };

  /** 杀掉 PTY 进程：POSIX 上先对整个进程组发信号，再对 PTY 本体发；force 时用 SIGKILL。 */
  const killProcess = (ptyProcess, force = false) => {
    if (!ptyProcess) return;
    if (process.platform !== 'win32' && Number.isInteger(ptyProcess.pid) && ptyProcess.pid > 0) {
      try { process.kill(-ptyProcess.pid, force ? 'SIGKILL' : 'SIGTERM'); } catch { /* already gone */ }
    }
    try { ptyProcess.kill(force ? 'SIGKILL' : undefined); } catch { /* already gone */ }
  };

  /** 优雅终止：先 SIGTERM（或 PTY kill），等待 onExit 或宽限期到后强制 SIGKILL；promise 记入 pendingTerminations 供 shutdown 等待。 */
  const terminateProcess = (ptyProcess, force = false) => {
    if (!ptyProcess) return Promise.resolve();
    if (force) { killProcess(ptyProcess, true); return Promise.resolve(); }
    let termination;
    termination = new Promise((resolve) => {
      let settled = false;
      let disposable = null;
      const finish = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        disposable?.dispose?.();
        resolve();
      };
      const timeout = setTimeout(() => { killProcess(ptyProcess, true); finish(); }, terminalTerminationGraceMs);
      try { disposable = ptyProcess.onExit(() => finish()); } catch { /* backend is already gone */ }
      killProcess(ptyProcess, false);
    }).finally(() => pendingTerminations.delete(termination));
    pendingTerminations.add(termination);
    return termination;
  };

  /** 向 WS 发送一条二进制控制帧；socket 未就绪（readyState!==1）或发送异常返回 false。 */
  const send = (socket, message) => {
    if (socket?.readyState !== 1) return false;
    try { socket.send(createTerminalWsControlFrame(message), { binary: true }); return true; } catch { return false; }
  };

  /** 向所有仍 attach 该 session 的连接广播致命 error 帧并解除 attach（会话关闭/被杀/超时回收时调用）。 */
  const closeAttachments = (sessionId, code, message) => {
    for (const connection of connections) {
      if (!connection.attachments.delete(sessionId)) continue;
      send(connection.socket, { t: 'error', v: 3, s: sessionId, code, message, fatal: true });
    }
  };

  /** 构造 snapshot 帧：byte feed 携带物化历史文本，grid feed 置空 history 并附当前全量 grid 帧。 */
  const snapshot = (session, { grid = false } = {}) => ({
    t: 'snapshot', v: 3, s: session.id, q: session.sequence, history: grid ? '' : historyText(session),
    status: session.status, exitCode: session.exitCode, signal: session.signal,
    runtime, ptyBackend: session.backend,
    // Grid-fed attachments reconcile from the parsed screen state; the raw
    // byte replay is dead weight for them, so their snapshot swaps history
    // for the current full grid frame.
    ...(grid ? { grid: session.grid ? session.grid.fullFrame() : null } : {}),
  });

  // （中文说明）判断该 session 是否仍有 grid feed 的 attachment 在订阅。
  // Server-side parsing feed: one GridCore per session turns the raw PTY
  // stream into parsed grid diff frames, published to attachments that
  // opted in via feed:'grid'. Attachments on the default byte feed are
  // untouched — the two feeds coexist per attachment, not per session.
  const hasGridAttachment = (sessionId) => {
    for (const connection of connections) {
      const attachment = connection.attachments.get(sessionId);
      if (attachment?.feed === 'grid') return true;
    }
    return false;
  };

  /** 重建会话的 GridCore 解析器并注册 onFrame：仅 running 且有 grid attachment 时发布帧（避免 byte feed 客户端看到无法消费的序号漂移）。 */
  const resetGrid = (session, cols, rows) => {
    session.grid?.dispose();
    session.grid = new GridCore({ cols, rows });
    // Frames publish only while a grid-fed attachment is watching — publish
    // itself advances the session sequence, and byte-feed clients must not
    // see sequence numbers drift for events they cannot observe. A grid
    // attachment arriving later materializes from fullFrame() instead.
    session.grid.onFrame = (frame) => {
      if (session.status !== 'running' || !hasGridAttachment(session.id)) return;
      publish(session, { t: 'grid', g: frame });
    };
  };

  /** 计算某个 attachment 的发送积压：全部已发送未 ack 条目的字节之和。 */
  const lagOf = (attachment) => {
    let lag = 0;
    for (const entry of attachment.pendingAcks) lag += entry.bytes;
    return lag;
  };

  // （中文说明）向单个 attachment 发送快照，并把其字节计入流控账户。
  // Send a snapshot to one attachment and account for its bytes like any
  // other frame, so a client that attaches and never reads is bounded too.
  const sendSnapshotTo = (connection, session) => {
    const attachment = connection.attachments.get(session.id);
    if (!attachment || attachment.initializing) return;
    const gridFeed = attachment.feed === 'grid';
    const snap = snapshot(session, { grid: gridFeed });
    if (!send(connection.socket, snap)) return;
    const bytes = gridFeed ? (snap.grid ? Buffer.byteLength(JSON.stringify(snap.grid)) : 0) + 256 : Buffer.byteLength(snap.history) + 256;
    attachment.sentBytes += bytes;
    attachment.pendingAcks.push({ q: session.sequence, bytes });
  };

  /** 会话事件广播核心：先递增序号，再按 attachment 的 feed 类型与流控状态分发 output/grid 帧（抑制时跳过但序号照走），其余事件直接发送。 */
  const publish = (session, event) => {
    session.sequence += 1;
    const message = { ...event, v: 3, s: session.id, q: session.sequence };
    const isOutput = event.t === 'output';
    const isGrid = event.t === 'grid';
    const gridBytes = isGrid ? Buffer.byteLength(JSON.stringify(event.g)) : 0;
    for (const connection of connections) {
      const attachment = connection.attachments.get(session.id);
      if (!attachment) continue;
      if (attachment.initializing) {
        // Grid-fed attachments keep no byte replay, so buffered snapshots
        // and their first grid frame are all they need from the queue.
        if (isOutput && attachment.feed === 'grid') continue;
        attachment.pending.push(message);
        continue;
      }
      if (isOutput && attachment.feed === 'grid') continue;
      if (isOutput || isGrid) {
        if (isGrid && attachment.feed !== 'grid') continue;
        const suppress = attachment.suppressed
          || lagOf(attachment) > SEND_LAG_ENTER_BYTES
          || (!attachment.ackedOnce && attachment.sentBytes > SEND_LAG_FALLBACK_BYTES);
        if (suppress) {
          // Sequence numbers keep advancing, so the next live frame this
          // attachment receives gap-triggers its own resync.
          attachment.suppressed = true;
          continue;
        }
        if (!send(connection.socket, message)) continue;
        const bytes = isGrid ? gridBytes : event.d.length;
        attachment.sentBytes += bytes;
        attachment.pendingAcks.push({ q: session.sequence, bytes });
      } else {
        send(connection.socket, message);
      }
    }
  };

  // （中文说明）ack 处理：裁剪已确认前缀；被抑制的 attachment 积压排干后重发快照完成重同步。
  // An attachment recovers (lag drained) by acknowledgment: prune the
  // acknowledged prefix, and if it was suppressed, resynchronize it with a
  // snapshot before live output resumes.
  const applyAck = (connection, sessionId, q) => {
    const attachment = connection.attachments.get(sessionId);
    if (!attachment || !Number.isFinite(q)) return;
    attachment.ackedOnce = true;
    let index = 0;
    while (index < attachment.pendingAcks.length && attachment.pendingAcks[index].q <= q) index += 1;
    if (index > 0) attachment.pendingAcks.splice(0, index);
    if (attachment.suppressed && lagOf(attachment) <= SEND_LAG_EXIT_BYTES) {
      attachment.suppressed = false;
      const session = sessions.get(sessionId);
      if (session) sendSnapshotTo(connection, session);
    }
  };

  // （中文说明）事件队列的分片参数：每片字节数与墙钟时间上限。
  // The drain is sliced by bytes and wall time and yields between slices so a
  // flood cannot monopolize the event loop: HTTP and other sockets keep
  // being served while megabytes stream through. Small outputs (interactive
  // typing) always finish synchronously in the first slice.
  const DRAIN_SLICE_BYTES = 256 * 1024;
  const DRAIN_SLICE_MS = 4;

  /** 分片消费事件队列（每片 256 KiB / 4ms，setImmediate 让出事件循环）：output 走主题应答、历史清洗、广播、grid 解析与 OSC 133 扫描；exit 落定终态并广播。 */
  const drainEvents = (session) => {
    if (session.draining) return;
    session.draining = true;
    const token = session.drainToken;
    const step = () => {
      let sliced = 0;
      const deadline = Date.now() + DRAIN_SLICE_MS;
      while (session.eventQueue.length > 0 && sliced < DRAIN_SLICE_BYTES && Date.now() < deadline) {
        const event = session.eventQueue.shift();
        if (event.type === 'output') session.queueBytes -= event.data.length;
        if (event.process !== session.process) continue;
        if (event.type === 'output') {
          sliced += event.data.length;
          const theme = consumeTerminalThemeQueries(session.pendingThemeControlSequence, event.data, {
            themeMode: session.themeMode,
            background: session.terminalBackground,
            foreground: session.terminalForeground,
            modeEnabled: session.themeModeEnabled,
          }, { respondToPrimaryDeviceAttributes: session.respondPrimaryDA !== false });
          session.pendingThemeControlSequence = theme.pending;
          session.themeModeEnabled = theme.modeEnabled;
          for (const response of theme.responses) session.process?.write(response);
          const sanitized = sanitizeTerminalHistoryChunk(session.pendingHistoryControlSequence, event.data);
          session.pendingHistoryControlSequence = sanitized.pending;
          appendHistory(session, sanitized.visible);
          // Output is not "activity" for lifetime purposes: an orphaned
          // chatty process (its client crashed without closing) must still
          // become idle-reapable. lastOutputAt tracks recency for
          // diagnostics; lifetime follows claims/touches/input.
          session.lastOutputAt = Date.now();
          publish(session, { t: 'output', d: event.data, ...(sanitized.visible !== event.data ? { r: sanitized.visible } : {}) });
          if (session.grid) {
            session.grid.write(event.data);
          }
          // OSC 133 shell integration: detect command boundary markers
          if (!session.osc133) session.osc133 = new Osc133Scanner();
          for (const oscEvent of session.osc133.scan(event.data)) {
            if (oscEvent.kind === 'command-finished') {
              publish(session, { t: 'command-finished', exitCode: oscEvent.exitCode });
            }
          }
        } else {
          session.status = 'exited';
          session.exitCode = Number.isInteger(event.exitCode) ? event.exitCode : null;
          session.signal = Number.isInteger(event.signal) ? event.signal : null;
          session.process = null;
          publish(session, { t: 'exit', exitCode: session.exitCode, signal: session.signal });
        }
      }
      if (session.eventQueue.length > 0) { setImmediate(step); return; }
      session.draining = false;
    };
    step();
  };

  /** 把 PTY 的 onData/onExit 事件接入会话事件队列并触发 drain。 */
  const wire = (session, ptyProcess) => {
    ptyProcess.onData((data) => {
      session.eventQueue.push({ type: 'output', process: ptyProcess, data });
      session.queueBytes += data.length;
      drainEvents(session);
    });
    ptyProcess.onExit(({ exitCode, signal }) => { session.eventQueue.push({ type: 'exit', process: ptyProcess, exitCode, signal }); drainEvents(session); });
  };

  /** 校验 cwd：非空字符串且真实存在为目录，否则抛错（create/restart 共用）。 */
  const validateCwd = async (cwd) => {
    if (typeof cwd !== 'string' || !cwd.trim()) throw new Error('cwd is required');
    const stats = await fs.promises.stat(cwd).catch(() => null);
    if (!stats?.isDirectory()) throw new Error('Invalid working directory');
  };

  /** 应用主题外观（themeMode/前景/背景）；有变化且 2031 模式已启用时向 PTY 写入当前明暗模式上报序列。 */
  const applyAppearance = (session, { themeMode, terminalBackground, terminalForeground }) => {
    const previous = [session.themeMode, session.terminalBackground, session.terminalForeground];
    if (themeMode === 'light' || themeMode === 'dark') session.themeMode = themeMode;
    if (typeof terminalBackground === 'string') session.terminalBackground = terminalBackground;
    if (typeof terminalForeground === 'string') session.terminalForeground = terminalForeground;
    const changed = previous[0] !== session.themeMode || previous[1] !== session.terminalBackground || previous[2] !== session.terminalForeground;
    if (changed && session.themeModeEnabled) {
      try { session.process?.write(terminalThemeModeReport(session.themeMode)); } catch { /* process exited */ }
    }
  };

  /** 在给定 session 对象上启动新 PTY 并接通全部管线；clear=true（首次创建）时先复位历史与主题协商状态。 */
  const startSession = async (session, { cwd, cols, rows, themeMode = 'dark', terminalBackground, terminalForeground, shell, loginShell }, clear = true) => {
    await validateCwd(cwd);
    const spawned = await spawnPty({ cwd, cols, rows, themeMode, shell, loginShell });
    if (clear) { historyChunksReset(session); session.pendingHistoryControlSequence = ''; session.pendingThemeControlSequence = ''; session.themeModeEnabled = false; }
    session.cwd = cwd; session.cols = cols; session.rows = rows; session.process = spawned.process;
    session.backend = spawned.backend; session.shell = spawned.shell; session.loginShell = spawned.loginShell; session.status = 'running'; session.exitCode = null; session.signal = null;
    session.themeMode = themeMode === 'light' ? 'light' : 'dark'; session.terminalBackground = terminalBackground; session.terminalForeground = terminalForeground;
    session.lastActivity = Date.now();
    resetGrid(session, cols, rows);
    // Invalidate any scheduled drain slice and drop its pending events.
    session.drainToken = (session.drainToken ?? 0) + 1; session.draining = false; session.eventQueue.length = 0; session.queueBytes = 0;
    // The bundled conpty.dll probes primary device attributes during its own
    // startup handshake; answering that probe writes the response into the
    // shell's input line. Only answer DA probes on the classic backend, where
    // they can only come from the shell itself (Fish's startup query).
    session.respondPrimaryDA = !spawned.conptyDll;
    wire(session, spawned.process);
  };

  /** 创建（或复用/合并）终端会话：校验尺寸/shell/login，合并同 id 并发创建并检测 cwd/shell/login 冲突，超过 MAX_SESSIONS 拒绝；返回 session 对象。 */
  const createSession = async ({ sessionId, cwd, cols = 80, rows = 24, themeMode, terminalBackground, terminalForeground, shell = 'auto', loginShell = false }) => {
    if (!validateSize(cols, 1000) || !validateSize(rows, 500)) throw new Error('Invalid terminal dimensions');
    if (typeof loginShell !== 'boolean') throw new Error('Invalid terminal login mode');
    const normalizedShell = normalizeTerminalShell(shell);
    if (!normalizedShell) throw new Error('Invalid terminal shell');
    const id = typeof sessionId === 'string' && sessionId.trim() ? sessionId.trim() : randomUUID();
    if (id.length > 128) throw new Error('Invalid terminal session id');
    const existing = sessions.get(id);
    const resolvedCwd = path.resolve(cwd);
    if (existing?.status === 'running') {
      if (path.resolve(existing.cwd) !== resolvedCwd) throw new Error('Terminal session belongs to a different working directory');
      applyAppearance(existing, { themeMode, terminalBackground, terminalForeground });
      return existing;
    }
    const pending = pendingSessionCreates.get(id);
    if (pending) {
      if (pending.cwd !== resolvedCwd) throw new Error('Terminal session belongs to a different working directory');
      if (pending.shell !== normalizedShell) throw new Error('Terminal session is already being created with a different shell');
      if (pending.loginShell !== loginShell) throw new Error('Terminal session is already being created with a different login mode');
      const session = await pending.promise;
      applyAppearance(session, { themeMode, terminalBackground, terminalForeground });
      return session;
    }
    if (!existing && sessions.size + pendingSessionCreates.size >= MAX_SESSIONS) throw new Error('Maximum terminal sessions reached');
    const creation = (async () => {
      const session = existing ?? { id, sequence: 0, historyChunks: [], historyBytes: 0, pendingHistoryControlSequence: '', pendingThemeControlSequence: '', eventQueue: [], queueBytes: 0, draining: false, drainToken: 0, viewportDriver: null, createdAt: Date.now(), claims: new Map() };
      await startSession(session, { cwd, cols, rows, themeMode, terminalBackground, terminalForeground, shell: normalizedShell, loginShell });
      sessions.set(id, session);
      return session;
    })();
    const pendingEntry = { cwd: resolvedCwd, shell: normalizedShell, loginShell, promise: creation };
    pendingSessionCreates.set(id, pendingEntry);
    try { return await creation; }
    finally { if (pendingSessionCreates.get(id) === pendingEntry) pendingSessionCreates.delete(id); }
  };

  /** 收集该 session 所有 attachment 上报的 (connectionId, cols, rows)，供隐式视口协商。 */
  const collectViewportSizes = (sessionId) => {
    const sizes = [];
    for (const connection of connections) {
      const attachment = connection.attachments.get(sessionId);
      if (attachment && attachment.cols > 0 && attachment.rows > 0) {
        sizes.push({ connectionId: connection.connectionId, cols: attachment.cols, rows: attachment.rows });
      }
    }
    return sizes;
  };
  /** 视口协商核心：DRIVEN 模式把 PTY 锁定到 driver 的有效宽度；IDLE 模式取全部 attachment 的最小宽/最小高并广播 resized（ownerId 随行）。 */
  const recomputeGrid = (session) => {
    // DRIVEN (forced ownership): the grid is locked to the claimer's effective
    // width and follows their container/zoom changes. No floor, no negotiation.
    if (session.viewportDriver) {
      const { cols, rows } = session.viewportDriver;
      if (cols === session.cols && rows === session.rows) return;
      session.cols = cols; session.rows = rows;
      if (session.status === 'running' && session.process) {
        try { session.process.resize(cols, rows); } catch { /* process exited */ }
      session.grid?.resize(cols, rows);
        session.grid?.resize(cols, rows);
      }
      publish(session, { t: 'driverChanged', driverId: session.viewportDriver.connectionId, cols, rows });
      return;
    }
    // IDLE (implicit ownership): the grid is the pure minimum effective width
    // across attachments — no floor. The narrowest device IS the owner; the
    // identity rides along on the broadcast so clients can badge it. Ownership
    // is bidirectional: any device narrowing below the current min takes it,
    // and the owner widening past a sibling hands it back.
    const sizes = collectViewportSizes(session.id);
    if (sizes.length === 0) return;
    let owner = sizes[0];
    for (const size of sizes) if (size.cols < owner.cols || (size.cols === owner.cols && size.rows < owner.rows)) owner = size;
    const cols = owner.cols;
    const rows = Math.min(...sizes.map((s) => s.rows));
    if (cols === session.cols && rows === session.rows && session.implicitOwnerId === owner.connectionId) return;
    session.cols = cols; session.rows = rows; session.implicitOwnerId = owner.connectionId;
    if (session.status === 'running' && session.process) {
      try { session.process.resize(cols, rows); } catch { /* process exited */ }
    }
    publish(session, { t: 'resized', ownerId: owner.connectionId, cols, rows });
  };

  /** 广播当前 driver 归属与尺寸；driverId:null 表示回到隐式协商模式。 */
  const broadcastDriverChanged = (session) => {
    if (session.viewportDriver) {
      publish(session, { t: 'driverChanged', driverId: session.viewportDriver.connectionId, cols: session.viewportDriver.cols, rows: session.viewportDriver.rows });
    } else {
      publish(session, { t: 'driverChanged', driverId: null, cols: session.cols, rows: session.rows });
    }
  };


  /** 新 WS 连接：注册到 connections、下发 hello（含 connectionId）、启动心跳，并挂载消息分发与关闭清理。 */
  wsServer.on('connection', (socket) => {
    const connection = { socket, attachments: new Map(), connectionId: randomUUID() };
    connections.add(connection);
    send(socket, { t: 'hello', v: 3, connectionId: connection.connectionId });
    const heartbeat = setInterval(() => { try { socket.ping(); } catch { /* closed */ } }, TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS);
    // 控制帧分发：ping/hello/detach/attach/viewport/claimViewport/releaseViewport/resync/ack/write。
    socket.on('message', (raw, isBinary) => {
      if (!isBinary) { send(socket, { t: 'error', v: 3, code: 'BAD_FRAME', message: 'Binary control frame required', fatal: false }); return; }
      const message = readTerminalWsControlFrame(raw);
      if (!message || message.v !== 3 || typeof message.t !== 'string') { send(socket, { t: 'error', v: 3, code: 'BAD_FRAME', message: 'Invalid terminal frame', fatal: false }); return; }
      if (message.t === 'ping') { send(socket, { t: 'pong', v: 3 }); return; }
      if (message.t === 'hello') return;
      const id = typeof message.s === 'string' ? message.s : '';
      if (!id) { send(socket, { t: 'error', v: 3, code: 'BAD_FRAME', message: 'Session id required', fatal: false }); return; }
      if (message.t === 'detach') {
        connection.attachments.delete(id);
        const session = sessions.get(id);
        if (session) {
          // A driver that detaches must release the role, otherwise the
          // ownership outlives the attachment and the close-time cleanup
          // (which walks attachments only) never releases it.
          if (session.viewportDriver?.connectionId === connection.connectionId) {
            session.viewportDriver = null;
            recomputeGrid(session);
            broadcastDriverChanged(session);
          } else {
            recomputeGrid(session);
          }
        }
        return;
      }
      const session = sessions.get(id);
      if (!session) { send(socket, { t: 'error', v: 3, s: id, code: 'SESSION_NOT_FOUND', message: 'Terminal session not found', fatal: true }); return; }
      if (message.t === 'attach') {
        const attachment = { initializing: true, pending: [], cols: 0, rows: 0, pendingAcks: [], ackedOnce: false, suppressed: false, sentBytes: 0, feed: message.feed === 'grid' ? 'grid' : 'bytes' };
        if (typeof message.cols === 'number' && message.cols > 0) attachment.cols = Math.min(message.cols, 1000);
        if (typeof message.rows === 'number' && message.rows > 0) attachment.rows = Math.min(message.rows, 500);
        connection.attachments.set(id, attachment);
        const gridFeed = attachment.feed === 'grid';
        const initial = snapshot(session, { grid: gridFeed });
        const sentInitial = send(socket, initial);
        for (const event of attachment.pending) if (event.q > initial.q && !(gridFeed && event.t === 'output')) send(socket, event);
        attachment.pending.length = 0; attachment.initializing = false;
        if (sentInitial) {
          const bytes = gridFeed ? (initial.grid ? Buffer.byteLength(JSON.stringify(initial.grid)) : 0) + 256 : Buffer.byteLength(initial.history) + 256;
          attachment.sentBytes += bytes;
          attachment.pendingAcks.push({ q: initial.q, bytes });
        }
        // In DRIVEN mode, send the current driver info; don't recompute (PTY is driver-sized)
        if (session.viewportDriver) {
          send(socket, { t: 'driverChanged', v: 3, s: session.id, driverId: session.viewportDriver.connectionId, cols: session.viewportDriver.cols, rows: session.viewportDriver.rows });
        } else {
          recomputeGrid(session);
        }
        return;
      }
      if (message.t === 'viewport') {
        const attachment = connection.attachments.get(id);
        if (attachment) {
          if (typeof message.cols === 'number' && message.cols > 0) attachment.cols = Math.min(message.cols, 1000);
          if (typeof message.rows === 'number' && message.rows > 0) attachment.rows = Math.min(message.rows, 500);
          // In DRIVEN mode, viewport updates from the driver resize the PTY
          if (session.viewportDriver && session.viewportDriver.connectionId === connection.connectionId) {
            session.viewportDriver.cols = attachment.cols;
            session.viewportDriver.rows = attachment.rows;
          }
          recomputeGrid(session);
        }
        return;
      }
      if (message.t === 'claimViewport') {
        // Only an attached client may drive the viewport; a claim from an
        // unattached (or detached) connection would orphan the driver role.
        if (!connection.attachments.has(id)) {
          send(socket, { t: 'error', v: 3, s: id, code: 'NOT_ATTACHED', message: 'Attach before claiming the viewport', fatal: false });
          return;
        }
        const cols = Number.isFinite(message.cols) ? Math.max(2, Math.min(message.cols, 1000)) : session.cols;
        const rows = Number.isFinite(message.rows) ? Math.max(2, Math.min(message.rows, 500)) : session.rows;
        session.viewportDriver = { connectionId: connection.connectionId, cols, rows };
        // changes; only broadcast explicitly for the same-size claim.
        if (cols === session.cols && rows === session.rows) broadcastDriverChanged(session);
        else recomputeGrid(session);
        return;
      }
      if (message.t === 'releaseViewport') {
        if (session.viewportDriver && session.viewportDriver.connectionId === connection.connectionId) {
          session.viewportDriver = null;
          recomputeGrid(session);
          broadcastDriverChanged(session);
        }
        return;
      }
      if (message.t === 'resync') {
        sendSnapshotTo(connection, session);
        if (session.viewportDriver) {
          send(socket, { t: 'driverChanged', v: 3, s: session.id, driverId: session.viewportDriver.connectionId, cols: session.viewportDriver.cols, rows: session.viewportDriver.rows });
        }
        return;
      }
      if (message.t === 'ack') {
        // Client-applied sequence for one session: {t:'ack', v:3, s, q}.
        if (Number.isFinite(message.q)) applyAck(connection, id, message.q);
        return;
      }
      if (message.t === 'write') {
        if (typeof message.d !== 'string' || !message.d || message.d.length > MAX_INPUT_CHARS) { send(socket, { t: 'error', v: 3, s: id, code: 'BAD_INPUT', message: 'Invalid terminal input', fatal: false }); return; }
        if (session.status !== 'running' || !session.process) { send(socket, { t: 'error', v: 3, s: id, code: 'NOT_RUNNING', message: 'Terminal is not running', fatal: false }); return; }
        try { session.process.write(message.d); session.lastActivity = Date.now(); } catch { send(socket, { t: 'error', v: 3, s: id, code: 'WRITE_FAILED', message: 'Failed to write to terminal', fatal: false }); }
      }
    });
    /** 连接关闭清理：停心跳、摘除 connection、释放其持有的 driver 角色并对受影响 session 重算视口（含已 detach 会话的兜底扫描）。 */
    const cleanup = () => {
      clearInterval(heartbeat);
      const attached = [...connection.attachments.keys()];
      connection.attachments.clear();
      connections.delete(connection);
      for (const sid of attached) {
        const session = sessions.get(sid);
        if (!session) continue;
        if (session.viewportDriver?.connectionId === connection.connectionId) {
          session.viewportDriver = null;
          recomputeGrid(session);
          broadcastDriverChanged(session);
        } else {
          recomputeGrid(session);
        }
      }
      // Safety net: release any driver role this connection still holds on
      // sessions it detached from earlier (detach already releases, but a
      // raced detach/claim ordering could leave the role behind).
      for (const session of sessions.values()) {
        if (session.viewportDriver?.connectionId === connection.connectionId) {
          session.viewportDriver = null;
          recomputeGrid(session);
          broadcastDriverChanged(session);
        }
      }
    };
    socket.on('close', cleanup); socket.on('error', () => {});
  });


  /** HTTP upgrade 处理：仅接管 /api/terminal/ws；启用 UI 鉴权时先校验 session token 与 origin（401/403 拒绝），成功后交给 wsServer 完成握手。 */
  const upgradeHandler = (req, socket, head) => {
    if (parseRequestPathname(req.url) !== TERMINAL_WS_PATH) return;
    void (async () => {
      try {
        if (uiAuthController?.enabled) {
          if (!await uiAuthController.ensureSessionToken(req, null)) { rejectWebSocketUpgrade(socket, 401, 'UI authentication required'); return; }
          if (!await isRequestOriginAllowed(req)) { rejectWebSocketUpgrade(socket, 403, 'Invalid origin'); return; }
        }
        if (!wsServer) { rejectWebSocketUpgrade(socket, 500, 'Terminal WebSocket unavailable'); return; }
        wsServer.handleUpgrade(req, socket, head, (ws) => wsServer.emit('connection', ws, req));
      } catch { rejectWebSocketUpgrade(socket, 500, 'Upgrade failed'); }
    })();
  };
  server.on('upgrade', upgradeHandler);

  /** GET /api/terminal/shells — 列出可用 shell（id/name/supportsLogin），失败 500。 */
  app.get('/api/terminal/shells', async (_req, res) => {
    try {
      const shells = await shellResolver.list();
      res.json(shells.map(({ id, name, supportsLogin }) => ({ id, name, supportsLogin })));
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to list terminal shells' });
    }
  });
  /** GET /api/terminal/sessions — 列出全部会话；可选 ?cwd= 按已解析的工作目录精确过滤。 */
  app.get('/api/terminal/sessions', (req, res) => {
    const rawCwd = typeof req.query?.cwd === 'string' ? req.query.cwd.trim() : '';
    const cwdFilter = rawCwd ? path.resolve(rawCwd) : null;
    const list = [];
    for (const session of sessions.values()) {
      if (cwdFilter && path.resolve(session.cwd) !== cwdFilter) continue;
      list.push({
        sessionId: session.id,
        cwd: session.cwd,
        status: session.status,
        createdAt: Number.isInteger(session.createdAt) ? session.createdAt : null,
      });
    }
    res.json({ sessions: list });
  });
  /** POST /api/terminal/touch — 会话心跳续活；携带 claimant（窗口级客户端实例 id）时记录 claim，作为条件删除的存活依据。 */
  app.post('/api/terminal/touch', (req, res) => {
    const rawIds = Array.isArray(req.body?.sessionIds) ? req.body.sessionIds : [];
    // A claimant is a per-window terminal client instance id. Touching with
    // one records a claim: the session is referenced by that window's tab
    // projection and must survive that window closing a DIFFERENT tab, while
    // a conditional delete (below) can release exactly this claimer's stake.
    const claimant = typeof req.body?.claimant === 'string' && req.body.claimant.trim() && req.body.claimant.length <= 128 ? req.body.claimant.trim() : null;
    const now = Date.now();
    let touched = 0;
    for (const id of rawIds) {
      if (typeof id !== 'string') continue;
      const session = sessions.get(id);
      if (!session) continue;
      session.lastActivity = now;
      if (claimant) session.claims.set(claimant, now);
      touched += 1;
    }
    res.json({ touched });
  });
  /** POST /api/terminal/create — 创建终端会话；达到 MAX_SESSIONS 返回 429，其余参数错误 400。 */
  app.post('/api/terminal/create', async (req, res) => {
    try { const session = await createSession(req.body ?? {}); res.json({ sessionId: session.id, cols: session.cols, rows: session.rows, status: session.status }); }
    catch (error) { res.status(error?.message === 'Maximum terminal sessions reached' ? 429 : 400).json({ error: error?.message || 'Failed to create terminal session' }); }
  });
  /** POST /api/terminal/:sessionId/resize — 直接调整 PTY 与 grid 尺寸并广播 resized（无下限：尺寸策略归协商模型所有）。 */
  app.post('/api/terminal/:sessionId/resize', (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    const { cols, rows } = req.body ?? {};
    if (!validateSize(cols, 1000) || !validateSize(rows, 500)) return res.status(400).json({ error: 'Invalid terminal dimensions' });
    // Direct resize still broadcasts so other devices follow; no floor —
    // the negotiation model owns sizing policy, this route just applies.
    try {
      if (session.status === 'running') session.process?.resize(cols, rows);
      session.grid?.resize(cols, rows);
      session.cols = cols; session.rows = rows;
      publish(session, { t: 'resized', cols, rows });
      res.json({ success: true, cols, rows });
    }
    catch (error) { res.status(500).json({ error: error?.message || 'Failed to resize terminal' }); }
  });
  /** POST /api/terminal/:sessionId/appearance — 更新主题外观并按需向 PTY 上报明暗模式；会话不存在 404。 */
  app.post('/api/terminal/:sessionId/appearance', (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    applyAppearance(session, req.body ?? {});
    res.json({ success: true });
  });
  /** POST /api/terminal/:sessionId/restart — 与同 session 在途 restart 串行：spawn 新 PTY、复位历史/grid/主题协商、优雅终止旧进程并广播 restarted（history 置空）。 */
  app.post('/api/terminal/:sessionId/restart', async (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    const cwd = req.body?.cwd ?? session.cwd;
    const cols = req.body?.cols ?? session.cols;
    const rows = req.body?.rows ?? session.rows;
    const themeMode = req.body?.themeMode ?? session.themeMode;
    const terminalBackground = req.body?.terminalBackground ?? session.terminalBackground;
    const terminalForeground = req.body?.terminalForeground ?? session.terminalForeground;
    const shell = req.body?.shell ?? 'auto';
    const loginShell = req.body?.loginShell ?? false;
    const previousRestart = pendingSessionRestarts.get(session.id) ?? Promise.resolve();
    const restart = previousRestart.catch(() => {}).then(async () => {
      await validateCwd(cwd);
      if (!validateSize(cols, 1000) || !validateSize(rows, 500)) throw new Error('Invalid terminal dimensions');
      if (typeof loginShell !== 'boolean') throw new Error('Invalid terminal login mode');
      const oldProcess = session.process;
      const spawned = await spawnPty({ cwd, cols, rows, themeMode, shell, loginShell });
      session.process = spawned.process; session.backend = spawned.backend; session.shell = spawned.shell; session.loginShell = spawned.loginShell; session.cwd = cwd; session.cols = cols; session.rows = rows;
      historyChunksReset(session); session.pendingHistoryControlSequence = ''; session.pendingThemeControlSequence = ''; session.themeModeEnabled = false; session.status = 'running'; session.exitCode = null; session.signal = null;
      session.drainToken = (session.drainToken ?? 0) + 1; session.draining = false; session.eventQueue.length = 0; session.queueBytes = 0;
      resetGrid(session, cols, rows);
      session.respondPrimaryDA = !spawned.conptyDll;
      session.themeMode = themeMode === 'light' ? 'light' : 'dark'; session.terminalBackground = terminalBackground; session.terminalForeground = terminalForeground;
       wire(session, spawned.process); void terminateProcess(oldProcess); publish(session, { t: 'restarted', history: '' });
    });
    pendingSessionRestarts.set(session.id, restart);
    try {
      await restart;
      res.json({ sessionId: session.id, cols, rows, status: session.status });
    } catch (error) { res.status(400).json({ error: error?.message || 'Failed to restart terminal' }); }
    finally { if (pendingSessionRestarts.get(session.id) === restart) pendingSessionRestarts.delete(session.id); }
  });
  /** DELETE /api/terminal/:sessionId — 带 ?claimant= 时仅释放该 claimant 的 claim（仍有活跃 claim 则不杀，返回 killed:false）；否则强制销毁会话并通知全部 attachment。 */
  app.delete('/api/terminal/:sessionId', async (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    // A delete carrying ?claimant= is a tab close: it releases ONLY that
    // client's claim and kills the process iff no other live claim remains
    // (another window/device still shows the session). A claim is live while
    // its touch is within the idle window — expired claims from crashed
    // clients must not keep sessions undead. A delete without a claimant is
    // an explicit destructive kill and ignores claims entirely.
    const claimant = typeof req.query?.claimant === 'string' && req.query.claimant.trim() && req.query.claimant.length <= 128 ? req.query.claimant.trim() : null;
    if (claimant && session.claims instanceof Map) {
      session.claims.delete(claimant);
      const now = Date.now();
      for (const [id, touchedAt] of session.claims) {
        if (now - touchedAt > IDLE_TIMEOUT_MS) session.claims.delete(id);
      }
      if (session.claims.size > 0) {
        res.json({ success: true, released: true, killed: false });
        return;
      }
    }
    sessions.delete(session.id);
    session.grid?.dispose(); session.grid = null;
    closeAttachments(session.id, 'CLOSED', 'Terminal closed');
    await terminateProcess(session.process);
    res.json({ success: true, released: true, killed: true });
  });
  /** POST /api/terminal/force-kill — 按 sessionId 或 cwd 批量强杀会话（忽略 claim），返回被杀数量与 id 列表。 */
  app.post('/api/terminal/force-kill', (req, res) => {
    const { sessionId, cwd } = req.body ?? {}; let killedCount = 0;
    const killedSessionIds = [];
    for (const [id, session] of sessions) {
      if ((sessionId && id !== sessionId) || (!sessionId && cwd && session.cwd !== cwd)) continue;
      sessions.delete(id); session.grid?.dispose(); session.grid = null; closeAttachments(id, 'KILLED', 'Terminal was killed'); void terminateProcess(session.process, true); killedSessionIds.push(id); killedCount += 1;
    }
    res.json({ success: true, killedCount, killedSessionIds });
  });

  /** 每 5 分钟的空闲回收：无任何 attachment 且超过 IDLE_TIMEOUT_MS 无活动的会话被强杀并通知。 */
  const idleSweep = setInterval(() => {
    const now = Date.now();
    for (const [id, session] of sessions) {
      const attached = [...connections].some((connection) => connection.attachments.has(id));
      if (!attached && now - session.lastActivity > IDLE_TIMEOUT_MS) {
        sessions.delete(id); session.grid?.dispose(); session.grid = null; closeAttachments(id, 'IDLE_TIMEOUT', 'Terminal expired after being idle'); void terminateProcess(session.process, true);
      }
    }
  }, 5 * 60 * 1000);

  /** 停机：摘 upgrade 监听与 sweep、等待在途 restart 落定、强杀全部会话、等待终止 promise，最后在 1s 上限内关闭 WS server。 */
  const shutdown = async () => {
    server.off('upgrade', upgradeHandler); clearInterval(idleSweep);
    await Promise.allSettled([...pendingSessionRestarts.values()]);
    for (const session of sessions.values()) { void terminateProcess(session.process, true); session.grid?.dispose(); session.grid = null; }
    sessions.clear();
    await Promise.allSettled([...pendingTerminations]);
    if (!wsServer) return;
    for (const client of wsServer.clients) client.terminate();
    await Promise.race([
      new Promise((resolve) => wsServer.close(resolve)),
      new Promise((resolve) => setTimeout(resolve, 1000)),
    ]);
    wsServer = null;
  };
  return { shutdown };
}
