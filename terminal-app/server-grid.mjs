#!/usr/bin/env node
/**
 * terminal-app grid server — server-side parsing variant.
 *
 * Architecture under test (the "grid push" variant):
 *   PTY -> xterm.js headless (parse ONCE, here) -> row diff -> WS
 *   -> client renders directly (zero VT parsing in the browser).
 *
 * The client keeps its WebGL/Canvas renderers untouched; they receive
 * an IRenderable-shaped grid fed by these messages:
 *   {t:'full', cols, rows, cells:[[c]...], cursor:[x,y]}
 *   {t:'rows', rows:{y:[c...]}, cursor:[x,y]}
 *   {t:'resize', cols, rows}
 * Cell array: [codepoint, fg(r<<16|g<<8|b), bg(24bit), flags, width]
 * flags bits: 1 bold, 2 italic, 4 underline, 8 strike, 16 inverse,
 *             32 invisible, 128 faint (CellFlags-compatible).
 */

import crypto from 'node:crypto';
import http from 'node:http';
import { WebSocketServer } from 'ws';
import pty from '@lydell/node-pty';
import headless from '@xterm/headless';
const { Terminal } = headless;

const PORT = Number(process.env.GRID_PORT ?? 8082);

/** Palette matching ghostty-vt's built-in theme resolution (differential
 * harness value: Tomorrow-style), used to resolve palette-index colors. */
const PALETTE = [];
for (let i = 0; i < 16; i++) {
  PALETTE.push([
    [204, 204, 204], [204, 102, 102], [181, 189, 104], [222, 147, 95],
    [129, 162, 190], [178, 148, 187], [138, 190, 183], [204, 204, 204],
    [117, 117, 117], [241, 141, 133], [219, 200, 106], [233, 190, 126],
    [138, 178, 235], [213, 161, 216], [148, 216, 209], [255, 255, 255],
  ][i]);
}
for (let i = 16; i < 232; i++) {
  const c = i - 16;
  const steps = [0, 95, 135, 175, 215, 255];
  PALETTE.push([
    steps[Math.floor(c / 36)],
    steps[Math.floor((c % 36) / 6)],
    steps[c % 6],
  ]);
}
for (let i = 232; i < 256; i++) {
  const g = 8 + (i - 232) * 10;
  PALETTE.push([g, g, g]);
}
const DEFAULT_FG = (204 << 16) | (204 << 8) | 204;
const DEFAULT_BG = 0;

function colorOf(cell, isFg) {
  if (isFg ? cell.isFgDefault() : cell.isBgDefault()) return isFg ? DEFAULT_FG : DEFAULT_BG;
  if (isFg ? cell.isFgRGB() : cell.isBgRGB()) {
    const n = isFg ? cell.getFgColor() : cell.getBgColor();
    return (n >>> 0) & 0xffffff;
  }
  const idx = isFg ? cell.getFgColor() : cell.getBgColor();
  const p = PALETTE[idx] ?? [204, 204, 204];
  return (p[0] << 16) | (p[1] << 8) | p[2];
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

/** Serialize one row of cells; also return a cheap content hash. */
function encodeRow(line, cols) {
  const cells = [];
  let h = 2166136261;
  for (let x = 0; x < cols; x++) {
    const c = line.getCell(x);
    const cp = c.getCodePoint ? c.getCodePoint() : c.getChars().codePointAt(0) || 32;
    const cell = [
      cp,
      colorOf(c, true),
      colorOf(c, false),
      flagsOf(c),
      c.getWidth ? c.getWidth() : 1,
    ];
    cells.push(cell);
    h = (h ^ (cp + (cell[1] << 8) + (cell[2] << 4) + cell[3] + cell[4])) >>> 0;
    h = (h * 16777619) >>> 0;
  }
  return { cells, h };
}

const server = http.createServer((req, res) => {
  res.writeHead(404);
  res.end();
});
const wss = new WebSocketServer({ noServer: true });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname !== '/gridws') {
    socket.destroy();
    return;
  }
  // Token is issued by the sibling pty server (/api/token via the vite
  // proxy); this local experiment server accepts any non-empty token.
  if (!url.searchParams.get('token')) {
    socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    const cols = Math.max(2, Math.min(500, Number(url.searchParams.get('cols')) || 110));
    const rows = Math.max(2, Math.min(300, Number(url.searchParams.get('rows')) || 30));

    const shell = process.env.ComSpec || 'cmd.exe';
    const proc = pty.spawn(shell, [], {
      name: 'xterm-256color',
      cols,
      rows,
      cwd: process.env.USERPROFILE || process.cwd(),
      env: { ...process.env, TERM: 'xterm-256color' },
    });

    const term = new Terminal({ cols, rows, scrollback: 0, allowProposedApi: true });

    let lastHashes = null;
    let flushScheduled = false;

    const cursorMsg = () => {
      const b = term.buffer.active;
      const y = Math.min(rows - 1, Math.max(0, b.cursorY));
      return [Math.min(cols - 1, b.cursorX), y, true];
    };

    const flush = () => {
      flushScheduled = false;
      if (ws.readyState !== ws.OPEN) return;
      const buf = term.buffer.active;
      const payloadRows = {};
      const hashes = new Array(rows);
      let full = lastHashes === null;
      for (let y = 0; y < rows; y++) {
        const line = buf.getLine(y);
        const { cells, h } = line ? encodeRow(line, cols) : { cells: null, h: 0 };
        hashes[y] = h;
        if (full || h !== lastHashes[y]) payloadRows[y] = cells;
      }
      const changed = Object.keys(payloadRows).length;
      // Heuristic: pushing nearly everything costs more than a full frame
      // with the same wire format — switch to full when >60% rows changed.
      if (changed > rows * 0.6) full = true;
      if (full) {
        const cells = [];
        for (let y = 0; y < rows; y++) cells.push(payloadRows[y] ?? null);
        // Fill any nulls (unchanged rows in a full frame)
        for (let y = 0; y < rows; y++) {
          if (!cells[y]) {
            const line = buf.getLine(y);
            cells[y] = line ? encodeRow(line, cols).cells : [];
          }
        }
        ws.send(JSON.stringify({ t: 'full', cols, rows, cells, cursor: cursorMsg() }));
      } else if (changed > 0) {
        ws.send(JSON.stringify({ t: 'rows', rowsMap: payloadRows, cursor: cursorMsg() }));
      } else {
        const c = cursorMsg();
        ws.send(JSON.stringify({ t: 'cursor', cursor: c }));
      }
      lastHashes = hashes;
    };

    const scheduleFlush = () => {
      if (flushScheduled) return;
      flushScheduled = true;
      setImmediate(flush); // coalesce bursts within the same tick
    };

    proc.onData((data) => {
      // xterm.write parses asynchronously; flush from its completion
      // callback so the diff sees the parsed state, not the pre-parse
      // snapshot (flushing early silently misses every change).
      term.write(data, scheduleFlush);
    });
    proc.onExit(() => ws.readyState === ws.OPEN && ws.close());

    ws.on('message', (raw) => {
      const text = raw.toString();
      if (text.startsWith('{')) {
        try {
          const msg = JSON.parse(text);
          if (msg.type === 'resize' && msg.cols && msg.rows) {
            const c2 = Math.max(2, Math.min(500, msg.cols | 0));
            const r2 = Math.max(2, Math.min(300, msg.rows | 0));
            term.resize(c2, r2);
            proc.resize(c2, r2);
            lastHashes = null; // force full frame
            scheduleFlush();
            return;
          }
        } catch {
          /* fall through as input */
        }
      }
      proc.write(text);
    });
    ws.on('close', () => proc.kill());
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[terminal-app] grid server on http://127.0.0.1:${PORT}`);
});
