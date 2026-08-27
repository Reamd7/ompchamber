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

const MAX_SESSIONS = 20;
const MAX_HISTORY_BYTES = 512 * 1024;
const MAX_INPUT_CHARS = 65_536;
const IDLE_TIMEOUT_MS = 30 * 60 * 1000;
const TERMINATION_GRACE_MS = 1000;
// Universal grid floor: TUI apps (btop, htop, vim) misrender or refuse to
// start below the 80x24 VT standard. Viewports narrower than the floor render
// the grid CSS-scaled (TerminalViewport driverScale) instead of shrinking the
// PTY, so one narrow window never cripples every attached device.
const MIN_TERMINAL_COLS = 80;
const MIN_TERMINAL_ROWS = 24;
const validateSize = (value, max) => Number.isInteger(value) && value >= 1 && value <= max;
const trimHistory = (history) => {
  const bytes = Buffer.from(history);
  if (bytes.byteLength <= MAX_HISTORY_BYTES) return history;
  let start = bytes.byteLength - MAX_HISTORY_BYTES;
  while (start < bytes.byteLength && (bytes[start] & 0xc0) === 0x80) start += 1;
  return bytes.subarray(start).toString('utf8');
};

export function createTerminalRuntime({
  app, server, fs, path, uiAuthController, buildAugmentedPath, searchPathFor, isExecutable,
  isRequestOriginAllowed, rejectWebSocketUpgrade, TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS,
  loadPtyProvider, terminalTerminationGraceMs = TERMINATION_GRACE_MS,
}) {
  const sessions = new Map();
  const pendingSessionCreates = new Map();
  const pendingSessionRestarts = new Map();
  const connections = new Set();
  const pendingTerminations = new Set();
  const runtime = typeof globalThis.Bun === 'undefined' ? 'node' : 'bun';
  let ptyProviderPromise = null;
  let wsServer = new WebSocketServer({ noServer: true, maxPayload: TERMINAL_WS_MAX_PAYLOAD_BYTES });
  const shellResolver = createTerminalShellResolver({ fs, path, searchPathFor, isExecutable, buildAugmentedPath });

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
        const options = { name: 'xterm-256color', cwd, cols, rows, env, ...(process.platform === 'win32' ? { useConpty: true } : {}) };
        const process_ = provider.spawn(launch.executable, launch.args, options);
        if (shellIntegration) shellIntegration.scheduleCleanup();
        return { process: process_, backend: provider.backend, shell: resolvedShell.id, loginShell };
      } catch (error) { lastError = error; }
    }
    throw lastError ?? new Error('No executable shell found');
  };

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

  const killProcess = (ptyProcess, force = false) => {
    if (!ptyProcess) return;
    if (process.platform !== 'win32' && Number.isInteger(ptyProcess.pid) && ptyProcess.pid > 0) {
      try { process.kill(-ptyProcess.pid, force ? 'SIGKILL' : 'SIGTERM'); } catch { /* already gone */ }
    }
    try { ptyProcess.kill(force ? 'SIGKILL' : undefined); } catch { /* already gone */ }
  };

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

  const send = (socket, message) => {
    if (socket?.readyState !== 1) return false;
    try { socket.send(createTerminalWsControlFrame(message), { binary: true }); return true; } catch { return false; }
  };

  const closeAttachments = (sessionId, code, message) => {
    for (const connection of connections) {
      if (!connection.attachments.delete(sessionId)) continue;
      send(connection.socket, { t: 'error', v: 3, s: sessionId, code, message, fatal: true });
    }
  };

  const snapshot = (session) => ({
    t: 'snapshot', v: 3, s: session.id, q: session.sequence, history: session.history,
    status: session.status, exitCode: session.exitCode, signal: session.signal,
    runtime, ptyBackend: session.backend,
  });

  const publish = (session, event) => {
    session.sequence += 1;
    const message = { ...event, v: 3, s: session.id, q: session.sequence };
    for (const connection of connections) {
      const attachment = connection.attachments.get(session.id);
      if (!attachment) continue;
      if (attachment.initializing) attachment.pending.push(message);
      else send(connection.socket, message);
    }
  };

  const drainEvents = (session) => {
    if (session.draining) return;
    session.draining = true;
    try {
      while (session.eventQueue.length > 0) {
        const event = session.eventQueue.shift();
        if (event.process !== session.process) continue;
        if (event.type === 'output') {
          const theme = consumeTerminalThemeQueries(session.pendingThemeControlSequence, event.data, {
            themeMode: session.themeMode,
            background: session.terminalBackground,
            foreground: session.terminalForeground,
            modeEnabled: session.themeModeEnabled,
          }, { respondToPrimaryDeviceAttributes: true });
          session.pendingThemeControlSequence = theme.pending;
          session.themeModeEnabled = theme.modeEnabled;
          for (const response of theme.responses) session.process?.write(response);
          const sanitized = sanitizeTerminalHistoryChunk(session.pendingHistoryControlSequence, event.data);
          session.pendingHistoryControlSequence = sanitized.pending;
          session.history = trimHistory(session.history + sanitized.visible);
          session.lastActivity = Date.now();
          publish(session, { t: 'output', d: event.data, ...(sanitized.visible !== event.data ? { r: sanitized.visible } : {}) });
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
    } finally { session.draining = false; }
  };

  const wire = (session, ptyProcess) => {
    ptyProcess.onData((data) => { session.eventQueue.push({ type: 'output', process: ptyProcess, data }); drainEvents(session); });
    ptyProcess.onExit(({ exitCode, signal }) => { session.eventQueue.push({ type: 'exit', process: ptyProcess, exitCode, signal }); drainEvents(session); });
  };

  const validateCwd = async (cwd) => {
    if (typeof cwd !== 'string' || !cwd.trim()) throw new Error('cwd is required');
    const stats = await fs.promises.stat(cwd).catch(() => null);
    if (!stats?.isDirectory()) throw new Error('Invalid working directory');
  };

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

  const startSession = async (session, { cwd, cols, rows, themeMode = 'dark', terminalBackground, terminalForeground, shell, loginShell }, clear = true) => {
    await validateCwd(cwd);
    const spawned = await spawnPty({ cwd, cols, rows, themeMode, shell, loginShell });
    if (clear) { session.history = ''; session.pendingHistoryControlSequence = ''; session.pendingThemeControlSequence = ''; session.themeModeEnabled = false; }
    session.cwd = cwd; session.cols = cols; session.rows = rows; session.process = spawned.process;
    session.backend = spawned.backend; session.shell = spawned.shell; session.loginShell = spawned.loginShell; session.status = 'running'; session.exitCode = null; session.signal = null;
    session.themeMode = themeMode === 'light' ? 'light' : 'dark'; session.terminalBackground = terminalBackground; session.terminalForeground = terminalForeground;
    session.lastActivity = Date.now(); session.eventQueue.length = 0;
    wire(session, spawned.process);
  };

  const createSession = async ({ sessionId, cwd, cols = 80, rows = 24, themeMode, terminalBackground, terminalForeground, shell = 'auto', loginShell = false }) => {
    if (!validateSize(cols, 1000) || !validateSize(rows, 500)) throw new Error('Invalid terminal dimensions');
    // Floor the initial grid too: the first paint (before attach negotiation)
    // must satisfy the same TUI minimum as the negotiated grid.
    cols = Math.max(MIN_TERMINAL_COLS, cols);
    rows = Math.max(MIN_TERMINAL_ROWS, rows);
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
      const session = existing ?? { id, sequence: 0, history: '', pendingHistoryControlSequence: '', pendingThemeControlSequence: '', eventQueue: [], draining: false, viewportDriver: null };
      await startSession(session, { cwd, cols, rows, themeMode, terminalBackground, terminalForeground, shell: normalizedShell, loginShell });
      sessions.set(id, session);
      return session;
    })();
    const pendingEntry = { cwd: resolvedCwd, shell: normalizedShell, loginShell, promise: creation };
    pendingSessionCreates.set(id, pendingEntry);
    try { return await creation; }
    finally { if (pendingSessionCreates.get(id) === pendingEntry) pendingSessionCreates.delete(id); }
  };

  const collectViewportSizes = (sessionId) => {
    const sizes = [];
    for (const connection of connections) {
      const attachment = connection.attachments.get(sessionId);
      if (attachment && attachment.cols > 0 && attachment.rows > 0) {
        sizes.push({ cols: attachment.cols, rows: attachment.rows });
      }
    }
    return sizes;
  };
  const recomputeGrid = (session) => {
    // DRIVEN mode: PTY size is the driver's viewport, not min-size. The 80x24
    // floor does NOT apply here: claiming control is an explicit act, and a
    // narrow-screen user zooming in below 80 columns is choosing bigger text
    // over TUI compatibility for this session (followers can reclaim anytime).
    if (session.viewportDriver) {
      const { cols, rows } = session.viewportDriver;
      if (cols === session.cols && rows === session.rows) return;
      session.cols = cols; session.rows = rows;
      if (session.status === 'running' && session.process) {
        try { session.process.resize(cols, rows); } catch { /* process exited */ }
      }
      publish(session, { t: 'driverChanged', driverId: session.viewportDriver.connectionId, cols, rows });
      return;
    }
    // IDLE mode: min-size across all attachments, floored to the TUI minimum.
    // Passive narrow devices must not drag the shared grid below 80x24;
    // devices narrower than the floor CSS-scale the grid client-side.
    const sizes = collectViewportSizes(session.id);
    if (sizes.length === 0) return;
    const cols = Math.max(MIN_TERMINAL_COLS, Math.min(...sizes.map((s) => s.cols)));
    const rows = Math.max(MIN_TERMINAL_ROWS, Math.min(...sizes.map((s) => s.rows)));
    if (cols === session.cols && rows === session.rows) return;
    session.cols = cols; session.rows = rows;
    if (session.status === 'running' && session.process) {
      try { session.process.resize(cols, rows); } catch { /* process exited */ }
    }
    publish(session, { t: 'resized', cols, rows });
  };

  const broadcastDriverChanged = (session) => {
    if (session.viewportDriver) {
      publish(session, { t: 'driverChanged', driverId: session.viewportDriver.connectionId, cols: session.viewportDriver.cols, rows: session.viewportDriver.rows });
    } else {
      publish(session, { t: 'driverChanged', driverId: null, cols: session.cols, rows: session.rows });
    }
  };


  wsServer.on('connection', (socket) => {
    const connection = { socket, attachments: new Map(), connectionId: randomUUID() };
    connections.add(connection);
    send(socket, { t: 'hello', v: 3, connectionId: connection.connectionId });
    const heartbeat = setInterval(() => { try { socket.ping(); } catch { /* closed */ } }, TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS);
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
        const attachment = { initializing: true, pending: [], cols: 0, rows: 0 };
        if (typeof message.cols === 'number' && message.cols > 0) attachment.cols = Math.min(message.cols, 1000);
        if (typeof message.rows === 'number' && message.rows > 0) attachment.rows = Math.min(message.rows, 500);
        connection.attachments.set(id, attachment);
        const initial = snapshot(session);
        send(socket, initial);
        for (const event of attachment.pending) if (event.q > initial.q) send(socket, event);
        attachment.pending.length = 0; attachment.initializing = false;
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
        const cols = Number.isFinite(message.cols) ? Math.max(1, Math.min(message.cols, 1000)) : session.cols;
        const rows = Number.isFinite(message.rows) ? Math.max(1, Math.min(message.rows, 500)) : session.rows;
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
        send(socket, snapshot(session));
        if (session.viewportDriver) {
          send(socket, { t: 'driverChanged', v: 3, s: session.id, driverId: session.viewportDriver.connectionId, cols: session.viewportDriver.cols, rows: session.viewportDriver.rows });
        }
        return;
      }
      if (message.t === 'write') {
        if (typeof message.d !== 'string' || !message.d || message.d.length > MAX_INPUT_CHARS) { send(socket, { t: 'error', v: 3, s: id, code: 'BAD_INPUT', message: 'Invalid terminal input', fatal: false }); return; }
        if (session.status !== 'running' || !session.process) { send(socket, { t: 'error', v: 3, s: id, code: 'NOT_RUNNING', message: 'Terminal is not running', fatal: false }); return; }
        try { session.process.write(message.d); session.lastActivity = Date.now(); } catch { send(socket, { t: 'error', v: 3, s: id, code: 'WRITE_FAILED', message: 'Failed to write to terminal', fatal: false }); }
      }
    });
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

  app.get('/api/terminal/shells', async (_req, res) => {
    try {
      const shells = await shellResolver.list();
      res.json(shells.map(({ id, name, supportsLogin }) => ({ id, name, supportsLogin })));
    } catch (error) {
      res.status(500).json({ error: error?.message || 'Failed to list terminal shells' });
    }
  });
  app.get('/api/terminal/list', (_req, res) => {
    const list = [];
    for (const [id, s] of sessions) {
      list.push({ sessionId: id, cwd: s.cwd, status: s.status, cols: s.cols, rows: s.rows, shell: s.shell });
    }
    res.json(list);
  });
  app.post('/api/terminal/create', async (req, res) => {
    try { const session = await createSession(req.body ?? {}); res.json({ sessionId: session.id, cols: session.cols, rows: session.rows, status: session.status }); }
    catch (error) { res.status(error?.message === 'Maximum terminal sessions reached' ? 429 : 400).json({ error: error?.message || 'Failed to create terminal session' }); }
  });
  app.post('/api/terminal/:sessionId/resize', (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    const { cols, rows } = req.body ?? {};
    if (!validateSize(cols, 1000) || !validateSize(rows, 500)) return res.status(400).json({ error: 'Invalid terminal dimensions' });
    // Same TUI floor as the negotiated grid; a direct resize must not
    // reintroduce sub-80 widths, and other devices must follow the change.
    const flooredCols = Math.max(MIN_TERMINAL_COLS, cols);
    const flooredRows = Math.max(MIN_TERMINAL_ROWS, rows);
    try {
      if (session.status === 'running') session.process?.resize(flooredCols, flooredRows);
      session.cols = flooredCols; session.rows = flooredRows;
      publish(session, { t: 'resized', cols: flooredCols, rows: flooredRows });
      res.json({ success: true, cols: flooredCols, rows: flooredRows });
    }
    catch (error) { res.status(500).json({ error: error?.message || 'Failed to resize terminal' }); }
  });
  app.post('/api/terminal/:sessionId/appearance', (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    applyAppearance(session, req.body ?? {});
    res.json({ success: true });
  });
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
      session.history = ''; session.pendingHistoryControlSequence = ''; session.pendingThemeControlSequence = ''; session.themeModeEnabled = false; session.status = 'running'; session.exitCode = null; session.signal = null; session.eventQueue.length = 0;
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
  app.delete('/api/terminal/:sessionId', async (req, res) => {
    const session = sessions.get(req.params.sessionId);
    if (!session) return res.status(404).json({ error: 'Terminal session not found' });
    sessions.delete(session.id);
    closeAttachments(session.id, 'CLOSED', 'Terminal closed');
    await terminateProcess(session.process);
    res.json({ success: true });
  });
  app.post('/api/terminal/force-kill', (req, res) => {
    const { sessionId, cwd } = req.body ?? {}; let killedCount = 0;
    const killedSessionIds = [];
    for (const [id, session] of sessions) {
      if ((sessionId && id !== sessionId) || (!sessionId && cwd && session.cwd !== cwd)) continue;
      sessions.delete(id); closeAttachments(id, 'KILLED', 'Terminal was killed'); void terminateProcess(session.process, true); killedSessionIds.push(id); killedCount += 1;
    }
    res.json({ success: true, killedCount, killedSessionIds });
  });

  const idleSweep = setInterval(() => {
    const now = Date.now();
    for (const [id, session] of sessions) {
      const attached = [...connections].some((connection) => connection.attachments.has(id));
      if (!attached && now - session.lastActivity > IDLE_TIMEOUT_MS) {
        sessions.delete(id); closeAttachments(id, 'IDLE_TIMEOUT', 'Terminal expired after being idle'); void terminateProcess(session.process, true);
      }
    }
  }, 5 * 60 * 1000);

  const shutdown = async () => {
    server.off('upgrade', upgradeHandler); clearInterval(idleSweep);
    await Promise.allSettled([...pendingSessionRestarts.values()]);
    for (const session of sessions.values()) void terminateProcess(session.process, true);
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
