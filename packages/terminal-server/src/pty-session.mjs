/**
 * GridPtySession — GridCore wired to a node-pty process.
 *
 * Hosts that manage their own process lifetime (OpenChamber's runtime)
 * should use GridCore directly; this convenience layer is for standalone
 * servers (terminal-app, tests).
 */

import pty from '@lydell/node-pty';
import { GridCore } from './core.mjs';

/**
 * @typedef {import('./core.mjs').GridFrame} GridFrame
 * @typedef {import('@lydell/node-pty').IPty} IPty
 */

/**
 * @typedef {object} GridPtySessionOptions
 * @property {number} [cols]
 * @property {number} [rows]
 * @property {string} [shell] Executable path (default: ComSpec or cmd.exe).
 * @property {string} [cwd]
 * @property {Record<string, string | undefined>} [env]
 * @property {string} [name] TERM value reported to the PTY.
 */

export class GridPtySession {
  /** @param {GridPtySessionOptions} [options] */
  constructor({ cols = 80, rows = 24, shell, cwd, env, name } = {}) {
    this.core = new GridCore({ cols, rows });
    this.core.onFrame = (frame) => this.onFrame?.(frame);

    const shellPath = shell || process.env.ComSpec || 'cmd.exe';
    /** @type {IPty} */
    this.proc = pty.spawn(shellPath, [], {
      name: name || 'xterm-256color',
      cols: this.core.cols,
      rows: this.core.rows,
      cwd: cwd || process.env.USERPROFILE || process.cwd(),
      env: env || { ...process.env, TERM: 'xterm-256color' },
    });

    this.proc.onData((data) => this.core.write(data));
    this.proc.onExit(({ exitCode }) => this.onExit?.(exitCode));
  }

  /** Input → PTY. @param {string} data */
  write(data) {
    this.proc.write(data);
  }

  /** @param {number} cols @param {number} rows */
  resize(cols, rows) {
    const c = Math.max(2, Math.min(500, cols | 0));
    const r = Math.max(2, Math.min(300, rows | 0));
    if (c === this.core.cols && r === this.core.rows) return;
    this.proc.resize(c, r);
    this.core.resize(c, r);
  }

  /** Parsed-grid frames (from the core).
   *  @type {((frame: GridFrame) => void) | null} */
  onFrame = null;
  /** PTY exited. @type {((exitCode: number | undefined) => void) | null} */
  onExit = null;

  dispose() {
    this.core.dispose();
    try {
      this.proc.kill();
    } catch {
      /* already gone */
    }
  }
}
