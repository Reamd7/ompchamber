/**
 * GridCore — the transport-free heart of server-side parsing.
 *
 * Feed it raw VT bytes; it parses them through xterm.js headless and
 * produces grid diff frames you can push over any channel:
 *
 *   const core = new GridCore({ cols, rows });
 *   core.onFrame = (frame) => send(frame);     // after each parse batch
 *   core.write(ptyBytes);                      // feed
 *   core.resize(cols, rows);                   // emits a full frame next
 *
 * This layer owns no PTY and no sockets — OpenChamber's terminal
 * runtime can embed it directly behind its existing WS protocol.
 */

import headless from '@xterm/headless';
import { DEFAULT_FG, DEFAULT_BG, paletteColor } from './palette.mjs';

const { Terminal } = headless;

/**
 * One parsed cell, fixed shape:
 * [codepoint, fg(24bit), bg(24bit), flags, width]
 * flags bits match ghostty-web CellFlags: 1 bold, 2 italic, 4 underline,
 * 8 strike, 16 inverse, 32 invisible, 128 faint.
 * @typedef {[number, number, number, number, number]} CellData
 */

/**
 * Cursor as [x, y, visible].
 * @typedef {[number, number, boolean]} CursorMsg
 */

/**
 * Complete screen state; also what snapshots reconcile from.
 * @typedef {object} FullFrame
 * @property {'full'} t
 * @property {number} cols
 * @property {number} rows
 * @property {CellData[][]} cells
 * @property {CursorMsg} cursor
 */

/**
 * Incremental row diff; keys are row indices as decimal strings (JSON
 * object keys).
 * @typedef {object} RowsFrame
 * @property {'rows'} t
 * @property {Record<string, CellData[]>} rowsMap
 * @property {CursorMsg} cursor
 */

/** Cursor moved with no content change. */
/**
 * @typedef {object} CursorFrame
 * @property {'cursor'} t
 * @property {CursorMsg} cursor
 */

/** Any grid frame variant. @typedef {FullFrame | RowsFrame | CursorFrame} GridFrame */

/**
 * @typedef {object} GridCoreOptions
 * @property {number} [cols] Initial columns (default 80).
 * @property {number} [rows] Initial rows (default 24).
 * @property {number} [maxCols] Column clamp (default 1000 — matches the
 *   OpenChamber runtime's terminal dimension validation).
 * @property {number} [maxRows] Row clamp (default 500).
 */

/**
 * @param {import('@xterm/headless').IBufferCell} cell
 * @param {boolean} isFg
 * @returns {number}
 */
function colorOf(cell, isFg) {
  if (isFg ? cell.isFgDefault() : cell.isBgDefault()) return isFg ? DEFAULT_FG : DEFAULT_BG;
  if (isFg ? cell.isFgRGB() : cell.isBgRGB()) {
    const n = isFg ? cell.getFgColor() : cell.getBgColor();
    return (n >>> 0) & 0xffffff;
  }
  const idx = isFg ? cell.getFgColor() : cell.getBgColor();
  return paletteColor(idx);
}

/**
 * @param {import('@xterm/headless').IBufferCell} cell
 * @returns {number}
 */
function flagsOf(cell) {
  return (
    (cell.isBold() ? 1 : 0) |
    (cell.isItalic() ? 2 : 0) |
    (cell.isUnderline() ? 4 : 0) |
    (cell.isStrikethrough() ? 8 : 0) |
    (cell.isInverse() ? 16 : 0) |
    (cell.isInvisible() ? 32 : 0) |
    (cell.isDim() ? 128 : 0)
  );
}

/**
 * @param {import('@xterm/headless').IBufferLine | undefined} line
 * @param {number} cols
 * @returns {{ cells: CellData[], h: number }}
 */
function encodeRow(line, cols) {
  const cells = new Array(cols);
  let h = 2166136261;
  for (let x = 0; x < cols; x++) {
    const c = line?.getCell(x);
    if (!c) {
      // Past the row's allocated length: blank cell, no style.
      cells[x] = [32, DEFAULT_FG, DEFAULT_BG, 0, 1];
      h = (h ^ 32) >>> 0;
      h = (h * 16777619) >>> 0;
      continue;
    }
    // getCodePoint exists at runtime (xterm >= 5.5) but is absent from
    // the shipped typings; probe it, else derive from getChars().
    const cp = /** @type {{ getCodePoint?: () => number }} */ (c).getCodePoint?.()
      ?? (c.getChars().codePointAt(0) ?? 32);
    const cell = /** @type {CellData} */ ([
      cp,
      colorOf(c, true),
      colorOf(c, false),
      flagsOf(c),
      c.getWidth ? c.getWidth() : 1,
    ]);
    cells[x] = cell;
    h = (h ^ (cp + (cell[1] << 8) + (cell[2] << 4) + cell[3] + cell[4])) >>> 0;
    h = (h * 16777619) >>> 0;
  }
  return { cells, h };
}

export class GridCore {
  /** @param {GridCoreOptions} [options] */
  constructor({ cols = 80, rows = 24, maxCols = 1000, maxRows = 500 } = {}) {
    this.maxCols = maxCols;
    this.maxRows = maxRows;
    this.cols = Math.max(2, Math.min(maxCols, cols | 0));
    this.rows = Math.max(2, Math.min(maxRows, rows | 0));
    this.term = new Terminal({ cols: this.cols, rows: this.rows, scrollback: 0, allowProposedApi: true });
    /** @type {number[] | null} */
    this.lastHashes = null;
    /** Called with a frame after each parsed batch.
     *  @type {((frame: GridFrame) => void) | null} */
    this.onFrame = null;
    this._drainQueued = false;
  }

  /** Feed VT bytes. Frames arrive via onFrame once parsing completes.
   *  @param {string | Uint8Array} data */
  write(data) {
    this.term.write(data, () => this._queueDrain());
  }

  /** @param {number} cols @param {number} rows */
  resize(cols, rows) {
    const c = Math.max(2, Math.min(this.maxCols, cols | 0));
    const r = Math.max(2, Math.min(this.maxRows, rows | 0));
    if (c === this.cols && r === this.rows) return;
    this.cols = c;
    this.rows = r;
    this.term.resize(c, r);
    this.lastHashes = null; // next drain emits a full frame
    this._queueDrain();
  }

  /** @returns {CursorMsg} */
  cursorMsg() {
    const b = this.term.buffer.active;
    const y = Math.min(this.rows - 1, Math.max(0, b.cursorY));
    return [Math.min(this.cols - 1, Math.max(0, b.cursorX)), y, true];
  }

  _queueDrain() {
    if (this._drainQueued) return;
    this._drainQueued = true;
    // setTimeout(0) over setImmediate: it coalesces bursts just the same,
    // stays scheduler-portable across runtimes, and — measured under
    // bun test — setImmediate never fires in some import graphs there,
    // freezing the whole event loop.
    setTimeout(() => {
      this._drainQueued = false;
      const frame = this.drain();
      if (frame && this.onFrame) this.onFrame(frame);
    }, 0);
  }

  /**
   * Force a complete frame (resets the diff baseline). Hosts use this to
   * materialize snapshots for newly attaching clients; the next drain()
   * diffs against this point.
   * @returns {FullFrame}
   */
  fullFrame() {
    this.lastHashes = null;
    return /** @type {FullFrame} */ (this.drain());
  }

  /** Compute the next diff frame. Also the sync API for hosts that prefer pull.
   *  @returns {GridFrame} */
  drain() {
    const buf = this.term.buffer.active;
    const { cols, rows } = this;
    /** @type {Record<string, CellData[]>} */
    const payloadRows = {};
    const hashes = new Array(rows);
    /** @type {number[] | null} */
    const prev = this.lastHashes;
    let full = prev === null;
    for (let y = 0; y < rows; y++) {
      const line = buf.getLine(y);
      const { cells, h } = line ? encodeRow(line, cols) : { cells: null, h: 0 };
      hashes[y] = h;
      if (full || h !== /** @type {number[]} */ (prev)[y]) payloadRows[y] = /** @type {CellData[]} */ (cells);
    }
    const cursor = this.cursorMsg();
    const changed = Object.keys(payloadRows).length;
    if (full || changed > rows * 0.6) {
      const cells = new Array(rows);
      for (let y = 0; y < rows; y++) {
        if (payloadRows[y]) {
          cells[y] = payloadRows[y];
        } else {
          const line = buf.getLine(y);
          cells[y] = line ? encodeRow(line, cols).cells : [];
        }
      }
      this.lastHashes = hashes;
      return { t: 'full', cols, rows, cells, cursor };
    }
    this.lastHashes = hashes;
    if (changed > 0) return { t: 'rows', rowsMap: payloadRows, cursor };
    return { t: 'cursor', cursor };
  }

  dispose() {
    // Deferred: xterm's async write-buffer continuation is a pending
    // setTimeout; disposing mid-write leaves that timer firing against a
    // dead instance and the process never settles. One tick later the
    // pending slice has run and dispose is safe.
    this.onFrame = null;
    const term = this.term;
    setTimeout(() => {
      try { term.dispose(); } catch { /* already torn down */ }
    }, 0);
  }
}
