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
 * Frame shapes (JSON-serializable):
 *   { t:'full',   cols, rows, cells: number[][][], cursor: [x,y,visible] }
 *   { t:'rows',   rowsMap: { [y]: number[][] },  cursor }
 *   { t:'cursor', cursor }
 * Cell: [codepoint, fg(24bit), bg(24bit), flags, width]
 * flags bits match CellFlags: 1 bold, 2 italic, 4 underline, 8 strike,
 * 16 inverse, 32 invisible, 128 faint.
 *
 * This layer owns no PTY and no sockets — OpenChamber's terminal
 * runtime can embed it directly behind its existing WS protocol.
 */

import headless from '@xterm/headless';
import { DEFAULT_FG, DEFAULT_BG, paletteColor } from './palette.mjs';

const { Terminal } = headless;

/** Coalesce bursts: parse callbacks arriving in the same tick share one drain. */

function colorOf(cell, isFg) {
  if (isFg ? cell.isFgDefault() : cell.isBgDefault()) return isFg ? DEFAULT_FG : DEFAULT_BG;
  if (isFg ? cell.isFgRGB() : cell.isBgRGB()) {
    const n = isFg ? cell.getFgColor() : cell.getBgColor();
    return (n >>> 0) & 0xffffff;
  }
  const idx = isFg ? cell.getFgColor() : cell.getBgColor();
  return paletteColor(idx);
}

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

export class GridCore {
  constructor({ cols = 80, rows = 24 } = {}) {
    this.cols = Math.max(2, Math.min(500, cols | 0));
    this.rows = Math.max(2, Math.min(300, rows | 0));
    this.term = new Terminal({ cols: this.cols, rows: this.rows, scrollback: 0, allowProposedApi: true });
    this.lastHashes = null;
    /** Called with a frame after each parsed batch. */
    this.onFrame = null;
    this._drainQueued = false;
  }

  /** Feed VT bytes. Frames arrive via onFrame once parsing completes. */
  write(data) {
    this.term.write(data, () => this._queueDrain());
  }

  resize(cols, rows) {
    const c = Math.max(2, Math.min(500, cols | 0));
    const r = Math.max(2, Math.min(300, rows | 0));
    if (c === this.cols && r === this.rows) return;
    this.cols = c;
    this.rows = r;
    this.term.resize(c, r);
    this.lastHashes = null; // next drain emits a full frame
    this._queueDrain();
  }

  cursorMsg() {
    const b = this.term.buffer.active;
    const y = Math.min(this.rows - 1, Math.max(0, b.cursorY));
    return [Math.min(this.cols - 1, Math.max(0, b.cursorX)), y, true];
  }

  _queueDrain() {
    if (this._drainQueued) return;
    this._drainQueued = true;
    setImmediate(() => {
      this._drainQueued = false;
      const frame = this.drain();
      if (frame && this.onFrame) this.onFrame(frame);
    });
  }

  /** Compute the next diff frame. Also the sync API for hosts that prefer pull. */
  drain() {
    const buf = this.term.buffer.active;
    const { cols, rows } = this;
    const payloadRows = {};
    const hashes = new Array(rows);
    let full = this.lastHashes === null;
    for (let y = 0; y < rows; y++) {
      const line = buf.getLine(y);
      const { cells, h } = line ? encodeRow(line, cols) : { cells: null, h: 0 };
      hashes[y] = h;
      if (full || h !== this.lastHashes[y]) payloadRows[y] = cells;
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
    this.term.dispose();
  }
}

function encodeRow(line, cols) {
  const cells = new Array(cols);
  let h = 2166136261;
  for (let x = 0; x < cols; x++) {
    const c = line.getCell(x);
    const cp = c.getCodePoint ? c.getCodePoint() : (c.getChars().codePointAt(0) || 32);
    const cell = [
      cp,
      colorOf(c, true),
      colorOf(c, false),
      flagsOf(c),
      c.getWidth ? c.getWidth() : 1,
    ];
    cells[x] = cell;
    h = (h ^ (cp + (cell[1] << 8) + (cell[2] << 4) + cell[3] + cell[4])) >>> 0;
    h = (h * 16777619) >>> 0;
  }
  return { cells, h };
}
