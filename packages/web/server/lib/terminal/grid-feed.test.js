import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import express from 'express';
import { WebSocket } from 'ws';

import { createTerminalRuntime } from './runtime.js';
import { createTerminalWsControlFrame, readTerminalWsControlFrame } from './terminal-ws-protocol.js';

/**
 * Server-side parsing feed (GridCore) contract tests over the real terminal
 * WS protocol: grid-fed attachments receive parsed grid frames instead of
 * raw output, snapshots reconcile them from the parsed screen, and the byte
 * feed coexists unchanged on the same session.
 *
 * Harness mirrors runtime.test.js's real-websocket shape (http + express +
 * ws over loopback) — verified equivalent before these cases were written.
 */

function createRuntime(server, overrides = {}) {
  const app = overrides.app ?? { post() {}, get() {}, delete() {} };
  return createTerminalRuntime({
    app, server, fs, path,
    uiAuthController: null,
    buildAugmentedPath: () => process.env.PATH || '',
    searchPathFor: () => null,
    isExecutable: () => false,
    isRequestOriginAllowed: async () => true,
    rejectWebSocketUpgrade() {},
    TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS: 30_000,
    ...overrides,
  });
}

const rowText = (cells) => cells.map((cell) => (cell[0] >= 32 ? String.fromCodePoint(cell[0]) : ' ')).join('').trimEnd();

const createHarness = async () => {
  const app = express();
  app.use(express.json());
  const server = http.createServer(app);
  const processes = [];
  const loadPtyProvider = async () => ({
    backend: 'fake-pty',
    spawn: () => {
      const dataHandlers = new Set();
      const exitHandlers = new Set();
      const process = {
        pid: 99700 + processes.length,
        killed: false,
        writes: [],
        resizes: [],
        write(value) { this.writes.push(value); },
        resize(cols, rows) { this.resizes.push([cols, rows]); },
        kill() { this.killed = true; },
        onData(handler) { dataHandlers.add(handler); return { dispose: () => dataHandlers.delete(handler) }; },
        onExit(handler) { exitHandlers.add(handler); return { dispose: () => exitHandlers.delete(handler) }; },
        emitData(value) { for (const handler of dataHandlers) handler(value); },
        emitExit(exitCode) { for (const handler of exitHandlers) handler({ exitCode, signal: 0 }); },
      };
      processes.push(process);
      return process;
    },
  });
  const runtime = createRuntime(server, {
    app, loadPtyProvider,
    terminalTerminationGraceMs: 10,
    fs: { promises: { stat: async () => ({ isDirectory: () => true }) } },
    searchPathFor: () => '/bin/sh', isExecutable: () => true,
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const socketUrl = `ws://127.0.0.1:${server.address().port}/api/terminal/ws`;
  const sockets = [];

  const open = async () => {
    const socket = new WebSocket(socketUrl);
    sockets.push(socket);
    const messages = [];
    socket.on('message', (raw) => messages.push(readTerminalWsControlFrame(raw)));
    await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
    const next = async (type, sessionId) => {
      for (let attempt = 0; attempt < 200; attempt += 1) {
        const index = messages.findIndex((message) => message?.t === type && (!sessionId || message.s === sessionId));
        if (index >= 0) return messages.splice(index, 1)[0];
        await new Promise((resolve) => setTimeout(resolve, 2));
      }
      throw new Error(`Timed out waiting for ${type}`);
    };
    const settle = async (ms = 150) => new Promise((resolve) => setTimeout(resolve, ms));
    await next('hello');
    return { socket, next, messages, settle };
  };

  const create = async (sessionId, extra = {}) => {
    const res = await fetch(`${base}/api/terminal/create`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ sessionId, cwd: '/repo', cols: 40, rows: 6, ...extra }),
    });
    expect(res.status).toBe(200);
    return res.json();
  };

  return {
    base, processes, open, create,
    post: (route, body) => fetch(`${base}${route}`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
    cleanup: async () => {
      for (const socket of sockets) socket.close();
      await runtime.shutdown();
      await new Promise((resolve) => server.close(resolve));
    },
  };
};

const attach = (client, sessionId, feed) =>
  client.socket.send(createTerminalWsControlFrame(
    feed === 'grid' ? { t: 'attach', v: 3, s: sessionId, feed: 'grid' } : { t: 'attach', v: 3, s: sessionId }
  ));

describe('terminal runtime grid feed', () => {
  it('delivers parsed grid frames to grid-fed attachments and raw output to byte-fed ones on the same session', async () => {
    const harness = await createHarness();
    try {
      await harness.create('term-grid');
      const gridClient = await harness.open();
      const byteClient = await harness.open();
      attach(gridClient, 'term-grid', 'grid');
      attach(byteClient, 'term-grid');
      await gridClient.next('snapshot', 'term-grid');
      await byteClient.next('snapshot', 'term-grid');

      harness.processes[0].emitData('\x1b[32mhello\x1b[0m grid');

      // The attach snapshot already materialized a full frame (and reset
      // the diff baseline), so live output arrives as an incremental rows
      // frame. Both shapes carry the same per-cell encoding.
      const gridFrame = await gridClient.next('grid', 'term-grid');
      const row0 = gridFrame.g.t === 'rows' ? gridFrame.g.rowsMap[0] : gridFrame.g.cells[0];
      expect(rowText(row0)).toBe('hello grid');
      // Green fg on the first content cell (ghostty-vt aligned palette).
      expect((row0[0][1] >>> 16) & 255).toBe(181);

      const output = await byteClient.next('output', 'term-grid');
      expect(output.d).toBe('\x1b[32mhello\x1b[0m grid');

      await gridClient.settle();
      expect(gridClient.messages.some((m) => m?.t === 'output' && m.s === 'term-grid')).toBe(false);
    } finally {
      await harness.cleanup();
    }
  }, 10_000);

  it('snapshots grid-fed reconnects from the parsed screen with no byte history', async () => {
    const harness = await createHarness();
    try {
      await harness.create('term-snap');
      const first = await harness.open();
      attach(first, 'term-snap', 'grid');
      await first.next('snapshot', 'term-snap');

      harness.processes[0].emitData('persisted text\r\n');
      await first.next('grid', 'term-snap');
      first.socket.close();

      const second = await harness.open();
      attach(second, 'term-snap', 'grid');
      const snapshot = await second.next('snapshot', 'term-snap');
      expect(snapshot.history).toBe('');
      expect(snapshot.grid).not.toBeNull();
      expect(snapshot.grid.t).toBe('full');
      expect(rowText(snapshot.grid.cells[0])).toContain('persisted text');
    } finally {
      await harness.cleanup();
    }
  }, 10_000);

  it('propagates resizes to the grid and emits a full frame at the new dimensions', async () => {
    const harness = await createHarness();
    try {
      await harness.create('term-resize');
      const client = await harness.open();
      attach(client, 'term-resize', 'grid');
      await client.next('snapshot', 'term-resize');

      const resized = await harness.post('/api/terminal/term-resize/resize', { cols: 20, rows: 4 });
      expect(resized.status).toBe(200);

      const frame = await client.next('grid', 'term-resize');
      expect(frame.g.t).toBe('full');
      expect(frame.g.cols).toBe(20);
      expect(frame.g.rows).toBe(4);
    } finally {
      await harness.cleanup();
    }
  }, 10_000);

  it('resets the parsed screen on restart', async () => {
    const harness = await createHarness();
    try {
      await harness.create('term-restart');
      const client = await harness.open();
      attach(client, 'term-restart', 'grid');
      await client.next('snapshot', 'term-restart');
      harness.processes[0].emitData('stale content\r\n');
      await client.next('grid', 'term-restart');

      const restarted = await harness.post('/api/terminal/term-restart/restart', { cwd: '/repo' });
      expect(restarted.status).toBe(200);
      await client.next('restarted', 'term-restart');

      harness.processes[1].emitData('fresh shell\r\n');
      const frame = await client.next('grid', 'term-restart');
      expect(frame.g.t).toBe('full');
      const allRows = frame.g.cells.map(rowText).filter(Boolean).join('\n');
      expect(allRows).not.toContain('stale content');
      expect(allRows).toContain('fresh shell');
    } finally {
      await harness.cleanup();
    }
  }, 10_000);

  it('keeps byte-feed sequence numbers unchanged while no grid attachment is watching', async () => {
    const harness = await createHarness();
    try {
      await harness.create('term-seq');
      const client = await harness.open();
      attach(client, 'term-seq');
      const first = await client.next('snapshot', 'term-seq');

      harness.processes[0].emitData('one\r\n');
      const output = await client.next('output', 'term-seq');
      // No grid attachment: the output event advances the sequence by
      // exactly one, exactly as before the grid capability existed.
      expect(output.q).toBe(first.q + 1);
    } finally {
      await harness.cleanup();
    }
  }, 10_000);
});
