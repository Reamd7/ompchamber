/**
 * Terminal transport abstraction — the renderer speaks one interface,
 * the environment picks the implementation:
 *   - browser:  WebSocket to the PTY server (token handshake)
 *   - Electron: IPC to the main-process PTY sessions (no network layer)
 */

export type ConnState = 'connecting' | 'open' | 'closed';

export interface TerminalTransport {
  /** Establish the session (spawn / handshake). Resolves when ready. */
  connect(grid: { cols: number; rows: number }): Promise<void>;
  /** Input → PTY. */
  write(data: string): void;
  /** Grid changed. */
  resize(cols: number, rows: number): void;
  /** Output ← PTY (stream chunks). */
  onData(cb: (chunk: string) => void): void;
  /** Connection lifecycle. */
  onConnState(cb: (s: ConnState) => void): void;
  close(): void;
}

/** WebSocket transport — browser build. */
export class WsTransport implements TerminalTransport {
  private ws: WebSocket | null = null;
  private dataCb: ((c: string) => void) | null = null;
  private connCb: ((s: ConnState) => void) | null = null;
  private closedByUs = false;
  private reconnectTimer: number | undefined;

  async connect(grid: { cols: number; rows: number }): Promise<void> {
    this.closedByUs = false;
    this.connCb?.('connecting');
    let token = '';
    try {
      const res = await fetch('/api/token', { cache: 'no-store' });
      token = (await res.json()).token;
    } catch {
      this.connCb?.('closed');
      this.reconnectTimer = window.setTimeout(() => this.connect(grid), 2000);
      return;
    }
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(
      `${proto}//${location.host}/ws?token=${encodeURIComponent(token)}&cols=${grid.cols}&rows=${grid.rows}`
    );
    this.ws = ws;
    ws.onopen = () => this.connCb?.('open');
    ws.onmessage = (ev) => this.dataCb?.(ev.data);
    ws.onclose = () => {
      if (this.closedByUs) return;
      this.connCb?.('closed');
      this.reconnectTimer = window.setTimeout(() => this.connect(grid), 2000);
    };
    ws.onerror = () => ws.close();
  }

  write(data: string): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(data);
  }

  resize(cols: number, rows: number): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'resize', cols, rows }));
    }
  }

  onData(cb: (c: string) => void): void {
    this.dataCb = cb;
  }

  onConnState(cb: (s: ConnState) => void): void {
    this.connCb = cb;
  }

  close(): void {
    this.closedByUs = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.close();
  }
}

/** Minimal shape of the preload bridge (see electron/preload.cjs). */
export interface TermAppBridge {
  spawn(grid: { cols: number; rows: number }): Promise<number>;
  write(session: number, data: string): void;
  resize(session: number, cols: number, rows: number): void;
  kill(session: number): void;
  onData(cb: (session: number, chunk: string) => void): void;
  onExit(cb: (session: number) => void): void;
}

/** IPC transport — Electron build. PTY lives in the main process. */
export class IpcTransport implements TerminalTransport {
  private session: number | null = null;
  private dataCb: ((c: string) => void) | null = null;
  private connCb: ((s: ConnState) => void) | null = null;

  constructor(private readonly bridge: TermAppBridge) {}

  async connect(grid: { cols: number; rows: number }): Promise<void> {
    this.connCb?.('connecting');
    try {
      this.session = await this.bridge.spawn(grid);
      this.connCb?.('open');
    } catch (e) {
      console.error('[term] spawn failed', e);
      this.connCb?.('closed');
    }
  }

  write(data: string): void {
    if (this.session !== null) this.bridge.write(this.session, data);
  }

  resize(cols: number, rows: number): void {
    if (this.session !== null) this.bridge.resize(this.session, cols, rows);
  }

  onData(cb: (c: string) => void): void {
    this.dataCb = cb;
    this.bridge.onData((session, chunk) => {
      if (session === this.session) this.dataCb?.(chunk);
    });
  }

  onConnState(cb: (s: ConnState) => void): void {
    this.connCb = cb;
    this.bridge.onExit((session) => {
      if (session === this.session) cb('closed');
    });
  }

  close(): void {
    if (this.session !== null) this.bridge.kill(this.session);
    this.session = null;
  }
}

/** Pick the transport for the current environment. */
export function createTransport(): TerminalTransport {
  const bridge = (window as unknown as { termApp?: TermAppBridge }).termApp;
  return bridge ? new IpcTransport(bridge) : new WsTransport();
}
