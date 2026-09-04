/**
 * Grid transport — browser side of the server-parsing variant.
 * WebSocket carrying parsed-grid messages into a GridTerminal.
 */

import type { TerminalTransport, ConnState } from '../transport';
import { GridTerminal } from './GridTerminal';

interface GridMsg {
  t: 'full' | 'rows' | 'cursor';
  [k: string]: unknown;
}

export class GridTransport implements TerminalTransport {
  readonly model = new GridTerminal();
  private ws: WebSocket | null = null;
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
      `${proto}//${location.host}/gridws?token=${encodeURIComponent(token)}&cols=${grid.cols}&rows=${grid.rows}`
    );
    this.ws = ws;
    ws.onopen = () => this.connCb?.('open');
    ws.onmessage = (ev) => {
      try {
        this.model.ingest(JSON.parse(ev.data) as GridMsg);
        this.onFrame?.();
      } catch {
        /* malformed frame: skip */
      }
    };
    ws.onclose = () => {
      if (this.closedByUs) return;
      this.connCb?.('closed');
      this.reconnectTimer = window.setTimeout(() => this.connect(grid), 2000);
    };
    ws.onerror = () => ws.close();
  }

  /** Renderer hook: called after every ingested frame. */
  onFrame: (() => void) | null = null;

  write(data: string): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(data);
  }

  resize(cols: number, rows: number): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'resize', cols, rows }));
    }
  }

  onData(_cb: (c: string) => void): void {
    // Grid variant: data arrives already parsed via ingest(); the
    // TerminalPane drives rendering from onFrame instead.
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
