/**
 * GridPtySession — GridCore wired to a node-pty process.
 *
 * Hosts that manage their own process lifetime (OpenChamber's runtime)
 * should use GridCore directly; this convenience layer is for standalone
 * servers (terminal-app, tests).
 */

import pty from '@lydell/node-pty';
import { GridCore } from './core.mjs';

export class GridPtySession {
  constructor({ cols = 80, rows = 24, shell, cwd, env, name } = {}) {
    this.core = new GridCore({ cols, rows });
    this.core.onFrame = (frame) => this.onFrame?.(frame);

    const shellPath = shell || process.env.ComSpec || 'cmd.exe';
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

  /** Input → PTY. */
  write(data) {
    this.proc.write(data);
  }

  resize(cols, rows) {
    const c = Math.max(2, Math.min(500, cols | 0));
    const r = Math.max(2, Math.min(300, rows | 0));
    if (c === this.core.cols && r === this.core.rows) return;
    this.proc.resize(c, r);
    this.core.resize(c, r);
  }

  /** Parsed-grid frames (from the core). */
  onFrame = null;
  /** PTY exited. */
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
