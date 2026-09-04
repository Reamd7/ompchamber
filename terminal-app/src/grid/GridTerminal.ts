/**
 * GridTerminal — an IRenderable adapter over the server-parsed grid.
 *
 * The "server-side parsing" variant: the browser receives already-parsed
 * cell rows over WebSocket and this class shapes them into the exact
 * interface the existing Canvas/WebGL renderers consume (getViewportPool
 * / isRowDirty / beginFrame / ...). Zero VT parsing happens here.
 */

import type { GhosttyCell } from 'ghostty-web';

type Cursor = { x: number; y: number; visible: boolean };

interface GridMsg {
  t: 'full' | 'rows' | 'cursor';
  cols?: number;
  rows?: number;
  cells?: number[][][];
  rowsMap?: Record<string, number[][]>;
  cursor?: [number, number, boolean];
}

export class GridTerminal {
  private cols = 80;
  private rowsN = 24;
  private pool: GhosttyCell[] = [];
  private dirtyRows = new Set<number>();
  private cursor: Cursor = { x: 0, y: 0, visible: true };
  private needFull = true;

  /** Feed one grid message from the server. */
  ingest(msg: GridMsg): void {
    if (msg.t === 'full' && msg.cells) {
      this.cols = msg.cols ?? this.cols;
      this.rowsN = msg.rows ?? this.rowsN;
      this.pool = new Array(this.cols * this.rowsN);
      for (let y = 0; y < this.rowsN; y++) {
        this.decodeRow(y, msg.cells[y] ?? []);
      }
      for (let y = 0; y < this.rowsN; y++) this.dirtyRows.add(y);
      this.needFull = true;
    } else if (msg.t === 'rows' && msg.rowsMap) {
      for (const [ys, cells] of Object.entries(msg.rowsMap)) {
        const y = Number(ys);
        if (y >= 0 && y < this.rowsN) {
          this.decodeRow(y, cells);
          this.dirtyRows.add(y);
        }
      }
    }
    if (msg.cursor) {
      const [x, y, visible] = msg.cursor;
      if (this.cursor.x !== x || this.cursor.y !== y) {
        this.dirtyRows.add(y);
        this.dirtyRows.add(this.cursor.y);
      }
      this.cursor = { x, y, visible };
    }
  }

  private decodeRow(y: number, cells: number[][]): void {
    const base = y * this.cols;
    for (let x = 0; x < this.cols; x++) {
      const c = cells[x];
      this.pool[base + x] = c
        ? {
            codepoint: c[0],
            fg_r: (c[1] >> 16) & 255,
            fg_g: (c[1] >> 8) & 255,
            fg_b: c[1] & 255,
            bg_r: (c[2] >> 16) & 255,
            bg_g: (c[2] >> 8) & 255,
            bg_b: c[2] & 255,
            flags: c[3] & 0xff,
            width: c[4] & 0xff,
            hyperlink_id: 0,
            grapheme_len: 0,
          }
        : {
            codepoint: 32,
            fg_r: 204,
            fg_g: 204,
            fg_b: 204,
            bg_r: 0,
            bg_g: 0,
            bg_b: 0,
            flags: 0,
            width: 1,
            hyperlink_id: 0,
            grapheme_len: 0,
          };
    }
  }

  // ---- IRenderable surface (what the renderers consume) ----

  getLine(y: number): GhosttyCell[] | null {
    return this.pool.slice(y * this.cols, (y + 1) * this.cols);
  }

  getCursor(): Cursor {
    return this.cursor;
  }

  getDimensions(): { cols: number; rows: number } {
    return { cols: this.cols, rows: this.rowsN };
  }

  isRowDirty(y: number): boolean {
    return this.dirtyRows.has(y);
  }

  needsFullRedraw(): boolean {
    const v = this.needFull;
    return v;
  }

  /**
   * Frame protocol: renderers call beginFrame() first; return a
   * DirtyState-like value (2 = has viewport changes) derived from the
   * pending dirty rows, mirroring the wasm contract.
   */
  beginFrame(): number {
    return this.dirtyRows.size > 0 ? 2 : 0;
  }

  endFrame(): void {
    /* cache-style no-op; dirty cleared by clearDirty after the frame */
  }

  getViewportPool(): GhosttyCell[] | null {
    return this.pool.length ? this.pool : null;
  }

  clearDirty(): void {
    this.dirtyRows.clear();
    this.needFull = false;
  }
}
