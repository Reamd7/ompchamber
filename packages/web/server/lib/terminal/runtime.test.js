import { EventEmitter } from 'node:events';
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import express from 'express';
import { WebSocket } from 'ws';

import { createTerminalRuntime } from './runtime.js';
import { createTerminalWsControlFrame, readTerminalWsControlFrame } from './terminal-ws-protocol.js';

function createResponse() {
  return {
    statusCode: 200,
    body: null,
    status(code) {
      this.statusCode = code;
      return this;
    },
    json(payload) {
      this.body = payload;
      return this;
    },
  };
}

function createRuntime(server, overrides = {}) {
  const app = overrides.app ?? {
    post() {},
    get() {},
    delete() {},
  };

  return createTerminalRuntime({
    app,
    server,
    express: { text: () => (_req, _res, next) => next?.() },
    fs,
    path,
    uiAuthController: null,
    buildAugmentedPath: () => process.env.PATH || '',
    searchPathFor: () => null,
    isExecutable: () => false,
    isRequestOriginAllowed: async () => true,
    rejectWebSocketUpgrade() {},
    TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS: 30_000,
    TERMINAL_INPUT_WS_REBIND_WINDOW_MS: 1_000,
    TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW: 3,
    ...overrides,
  });
}

describe('terminal runtime', () => {
  const createHarness = (overrides = {}) => {
    const routes = { get: new Map(), post: new Map(), delete: new Map() };
    const processes = [];
    const app = {
      post(route, handler) { routes.post.set(route, handler); },
      get(route, handler) { routes.get.set(route, handler); },
      delete(route, handler) { routes.delete.set(route, handler); },
    };
    const loadPtyProvider = async () => ({
      backend: 'fake-pty',
      spawn: (shell, args, options) => {
        const dataHandlers = new Set();
        const exitHandlers = new Set();
        const process = {
          pid: 123 + processes.length,
          shell,
          args,
          options,
          writes: [],
          resizes: [],
          killed: false,
          kills: [],
          write(data) { this.writes.push(data); },
          resize(cols, rows) { this.resizes.push([cols, rows]); },
          kill(signal) { this.killed = true; this.kills.push(signal ?? 'SIGTERM'); },
          onData(handler) { dataHandlers.add(handler); return { dispose: () => dataHandlers.delete(handler) }; },
          onExit(handler) { exitHandlers.add(handler); return { dispose: () => exitHandlers.delete(handler) }; },
          emitData(data) { for (const handler of dataHandlers) handler(data); },
          emitExit(exitCode = 0, signal = 0) { for (const handler of exitHandlers) handler({ exitCode, signal }); },
        };
        processes.push(process);
        return process;
      },
    });
    const server = new EventEmitter();
    const runtime = createRuntime(server, {
      app,
      loadPtyProvider,
      terminalTerminationGraceMs: 10,
      fs: { promises: { stat: async () => ({ isDirectory: () => true }) } },
      searchPathFor: () => '/bin/sh',
      isExecutable: () => true,
      ...overrides,
    });
    return { routes, processes, runtime };
  };

  it('rejects regular files as terminal working directories', async () => {
    const postRoutes = new Map();
    const app = {
      post(route, ...handlers) {
        postRoutes.set(route, handlers.at(-1));
      },
      get() {},
      delete() {},
    };
    const server = new EventEmitter();
    const runtime = createRuntime(server, {
      app,
      fs: {
        promises: {
          stat: async () => ({ isDirectory: () => false }),
        },
      },
      uiAuthController: { enabled: false },
      buildAugmentedPath: () => '',
      TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS: 1000,
      TERMINAL_INPUT_WS_REBIND_WINDOW_MS: 1000,
    });

    try {
      const createRoute = postRoutes.get('/api/terminal/create');
      const res = createResponse();

      await createRoute({ body: { cwd: '/tmp/not-a-directory' } }, res);

      expect(res.statusCode).toBe(400);
      expect(res.body).toEqual({ error: 'Invalid working directory' });
    } finally {
      await runtime.shutdown();
    }
  });

  it('removes its websocket upgrade listener on shutdown', async () => {
    const server = new EventEmitter();
    const runtime = createRuntime(server);

    expect(server.listenerCount('upgrade')).toBe(1);

    await runtime.shutdown();

    expect(server.listenerCount('upgrade')).toBe(0);
  });

  it('creates client-identified sessions and forwards bounded resize operations', async () => {
    const harness = createHarness();
    try {
      const response = createResponse();
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-1', cwd: '/repo', cols: 120, rows: 40, themeMode: 'light', terminalBackground: '#faf8f0', terminalForeground: '#1b1b1b' } }, response);
      expect(response.body).toEqual({ sessionId: 'term-1', cols: 120, rows: 40, status: 'running' });
      expect(harness.processes[0].options.cwd).toBe('/repo');
      expect(harness.processes[0].options.env.COLORFGBG).toBe('0;15');
      expect(harness.processes[0].options.env.NODE_CHANNEL_FD).toBe('');
      expect(harness.processes[0].options.env).not.toHaveProperty('ARGV0');
      expect(harness.processes[0].options.env).not.toHaveProperty('ELECTRON_RUN_AS_NODE');
      if (process.platform === 'linux') {
        expect(harness.processes[0].shell).toMatch(/\/env$/);
        expect(harness.processes[0].args.slice(0, 3)).toEqual(['-u', 'ARGV0', expect.any(String)]);
      }
      harness.processes[0].emitData('\u001b[?2031h\u001b]10;?\u0007\u001b]11;?\u0007\u001b[0c');
      expect(harness.processes[0].writes).toEqual(['\u001b]10;rgb:1b1b/1b1b/1b1b\u001b\\', '\u001b]11;rgb:fafa/f8f8/f0f0\u001b\\', '\u001b[?1;2c']);

      const appearance = createResponse();
      harness.routes.post.get('/api/terminal/:sessionId/appearance')({ params: { sessionId: 'term-1' }, body: { themeMode: 'dark' } }, appearance);
      expect(appearance.body).toEqual({ success: true });
      expect(harness.processes[0].writes.at(-1)).toBe('\u001b[?997;1n');

      const resize = createResponse();
      harness.routes.post.get('/api/terminal/:sessionId/resize')({ params: { sessionId: 'term-1' }, body: { cols: 200, rows: 60 } }, resize);
      expect(resize.statusCode).toBe(200);
      expect(harness.processes[0].resizes).toEqual([[200, 60]]);

      const invalid = createResponse();
      harness.routes.post.get('/api/terminal/:sessionId/resize')({ params: { sessionId: 'term-1' }, body: { cols: 1001, rows: 60 } }, invalid);
      expect(invalid.statusCode).toBe(400);
    } finally { await harness.runtime.shutdown(); }
  });

  it('lists sessions scoped to a working directory and refreshes activity via touch', async () => {
    const harness = createHarness();
    try {
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-a', cwd: '/repo' } }, createResponse());
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-b', cwd: '/other' } }, createResponse());

      const all = createResponse();
      harness.routes.get.get('/api/terminal/sessions')({ query: {} }, all);
      expect(all.body.sessions.map((s) => s.sessionId).sort()).toEqual(['term-a', 'term-b']);

      const scoped = createResponse();
      harness.routes.get.get('/api/terminal/sessions')({ query: { cwd: '/repo' } }, scoped);
      expect(scoped.body.sessions).toEqual([
        { sessionId: 'term-a', cwd: '/repo', status: 'running', createdAt: expect.any(Number) },
      ]);

      const touch = createResponse();
      harness.routes.post.get('/api/terminal/touch')({ body: { sessionIds: ['term-a', 'missing', 42] } }, touch);
      expect(touch.body).toEqual({ touched: 1 });

      const malformed = createResponse();
      harness.routes.post.get('/api/terminal/touch')({ body: {} }, malformed);
      expect(malformed.body).toEqual({ touched: 0 });
    } finally { await harness.runtime.shutdown(); }
  });

  it('strips AppImage ARGV0 from PTY child environments', async () => {
    const previousArgv0 = process.env.ARGV0;
    process.env.ARGV0 = '/path/to/OMPChamber/OMPChamber-1.17.2-linux-x86_64.AppImage';
    const harness = createHarness();
    try {
      const response = createResponse();
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-argv0', cwd: '/repo', cols: 80, rows: 24 } }, response);
      expect(response.statusCode).toBe(200);
      expect(harness.processes[0].options.env).not.toHaveProperty('ARGV0');
      if (process.platform === 'linux') {
        expect(harness.processes[0].shell).toMatch(/\/env$/);
        expect(harness.processes[0].args[0]).toBe('-u');
        expect(harness.processes[0].args[1]).toBe('ARGV0');
      }
    } finally {
      if (previousArgv0 === undefined) delete process.env.ARGV0;
      else process.env.ARGV0 = previousArgv0;
      await harness.runtime.shutdown();
    }
  });

  it('lists available shells and uses the selected shell for create and restart', async () => {
    const executables = new Set(['/bin/zsh', '/bin/bash', '/bin/sh']);
    const harness = createHarness({
      fs: {
        promises: {
          stat: async () => ({ isDirectory: () => true }),
          readFile: async () => '/bin/zsh\n/bin/bash\n/bin/false\n',
        },
      },
      searchPathFor: (name) => executables.has(`/bin/${name}`) ? `/bin/${name}` : null,
      isExecutable: (candidate) => executables.has(candidate),
    });
    try {
      const listed = createResponse();
      await harness.routes.get.get('/api/terminal/shells')({}, listed);
      expect(listed.body).toEqual(expect.arrayContaining([
        { id: 'auto', name: 'Auto', supportsLogin: true },
        { id: 'zsh', name: 'zsh', supportsLogin: true },
        { id: 'bash', name: 'bash', supportsLogin: true },
        { id: 'sh', name: 'sh', supportsLogin: false },
      ]));

      const created = createResponse();
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-shell', cwd: '/repo', shell: 'zsh', loginShell: true } }, created);
      expect(created.statusCode).toBe(200);
      if (process.platform === 'linux') {
        expect(harness.processes[0].shell).toMatch(/\/env$/);
        expect(harness.processes[0].args).toEqual(['-u', 'ARGV0', '/bin/zsh', '-l']);
      } else {
        expect(harness.processes[0].shell).toBe('/bin/zsh');
        expect(harness.processes[0].args).toEqual(['-l']);
      }

      const restarted = createResponse();
      await harness.routes.post.get('/api/terminal/:sessionId/restart')({ params: { sessionId: 'term-shell' }, body: { shell: 'bash', loginShell: true } }, restarted);
      expect(restarted.statusCode).toBe(200);
      if (process.platform === 'linux') {
        expect(harness.processes[1].shell).toMatch(/\/env$/);
        expect(harness.processes[1].args).toEqual(['-u', 'ARGV0', '/bin/bash', '-l']);
      } else {
        expect(harness.processes[1].shell).toBe('/bin/bash');
        expect(harness.processes[1].args).toEqual(['-l']);
      }
    } finally { await harness.runtime.shutdown(); }
  });

  it('rejects invalid and unavailable explicit shells', async () => {
    const harness = createHarness({
      fs: {
        promises: {
          stat: async () => ({ isDirectory: () => true }),
          readFile: async () => '/bin/sh\n',
        },
      },
      searchPathFor: (name) => name === 'sh' ? '/bin/sh' : null,
      isExecutable: (candidate) => candidate === '/bin/sh',
    });
    try {
      for (const [shell, error] of [
        ['zsh -c whoami', 'Invalid terminal shell'],
        ['fish', 'Terminal shell "fish" is not available'],
      ]) {
        const response = createResponse();
        await harness.routes.post.get('/api/terminal/create')({ body: { cwd: '/repo', shell } }, response);
        expect(response.statusCode).toBe(400);
        expect(response.body).toEqual({ error });
      }
      expect(harness.processes).toHaveLength(0);
    } finally { await harness.runtime.shutdown(); }
  });

  it('rejects invalid and unsupported login modes', async () => {
    const harness = createHarness({
      fs: {
        promises: {
          stat: async () => ({ isDirectory: () => true }),
          readFile: async () => '/bin/sh\n',
        },
      },
      searchPathFor: (name) => name === 'sh' ? '/bin/sh' : null,
      isExecutable: (candidate) => candidate === '/bin/sh',
    });
    try {
      for (const [loginShell, error] of [
        ['true', 'Invalid terminal login mode'],
        [true, 'Terminal shell "sh" does not support login mode'],
      ]) {
        const response = createResponse();
        await harness.routes.post.get('/api/terminal/create')({ body: { cwd: '/repo', shell: 'sh', loginShell } }, response);
        expect(response.statusCode).toBe(400);
        expect(response.body).toEqual({ error });
      }
      expect(harness.processes).toHaveLength(0);
    } finally { await harness.runtime.shutdown(); }
  });

  it('preserves the running process when a replacement shell is unavailable', async () => {
    const harness = createHarness({
      fs: {
        promises: {
          stat: async () => ({ isDirectory: () => true }),
          readFile: async () => '/bin/sh\n',
        },
      },
      searchPathFor: (name) => name === 'sh' ? '/bin/sh' : null,
      isExecutable: (candidate) => candidate === '/bin/sh',
    });
    try {
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-1', cwd: '/repo', shell: 'sh' } }, createResponse());
      const restarted = createResponse();

      await harness.routes.post.get('/api/terminal/:sessionId/restart')({ params: { sessionId: 'term-1' }, body: { shell: 'fish' } }, restarted);

      expect(restarted.statusCode).toBe(400);
      expect(restarted.body.error).toBe('Terminal shell "fish" is not available');
      expect(harness.processes).toHaveLength(1);
      expect(harness.processes[0].killed).toBe(false);
    } finally { await harness.runtime.shutdown(); }
  });

  it('deduplicates concurrent creates and rejects cross-directory id reuse', async () => {
    const harness = createHarness();
    try {
      const create = harness.routes.post.get('/api/terminal/create');
      const first = createResponse();
      const second = createResponse();
      await Promise.all([
        create({ body: { sessionId: 'term-shared', cwd: '/repo' } }, first),
        create({ body: { sessionId: 'term-shared', cwd: '/repo' } }, second),
      ]);
      expect(harness.processes).toHaveLength(1);
      expect(first.body.sessionId).toBe('term-shared');
      expect(second.body.sessionId).toBe('term-shared');

      const conflicting = createResponse();
      await create({ body: { sessionId: 'term-shared', cwd: '/other' } }, conflicting);
      expect(conflicting.statusCode).toBe(400);
      expect(conflicting.body.error).toBe('Terminal session belongs to a different working directory');
      expect(harness.processes).toHaveLength(1);
    } finally { await harness.runtime.shutdown(); }
  });

  it('rejects concurrent creates with conflicting shell preferences', async () => {
    const harness = createHarness();
    try {
      const create = harness.routes.post.get('/api/terminal/create');
      const first = createResponse();
      const conflicting = createResponse();
      await Promise.all([
        create({ body: { sessionId: 'term-shared', cwd: '/repo', shell: 'auto' } }, first),
        create({ body: { sessionId: 'term-shared', cwd: '/repo', shell: 'zsh' } }, conflicting),
      ]);

      expect(first.statusCode).toBe(200);
      expect(conflicting.statusCode).toBe(400);
      expect(conflicting.body.error).toBe('Terminal session is already being created with a different shell');
      expect(harness.processes).toHaveLength(1);
    } finally { await harness.runtime.shutdown(); }
  });

  it('rejects concurrent creates with conflicting login modes', async () => {
    const harness = createHarness();
    try {
      const create = harness.routes.post.get('/api/terminal/create');
      const first = createResponse();
      const conflicting = createResponse();
      await Promise.all([
        create({ body: { sessionId: 'term-shared', cwd: '/repo', shell: 'auto', loginShell: false } }, first),
        create({ body: { sessionId: 'term-shared', cwd: '/repo', shell: 'auto', loginShell: true } }, conflicting),
      ]);

      expect(first.statusCode).toBe(200);
      expect(conflicting.statusCode).toBe(400);
      expect(conflicting.body.error).toBe('Terminal session is already being created with a different login mode');
      expect(harness.processes).toHaveLength(1);
    } finally { await harness.runtime.shutdown(); }
  });

  it('restarts atomically with the same identity and closes the previous process', async () => {
    const harness = createHarness();
    try {
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-1', cwd: '/repo' } }, createResponse());
      const restarted = createResponse();
      await harness.routes.post.get('/api/terminal/:sessionId/restart')({ params: { sessionId: 'term-1' }, body: { cwd: '/other', cols: 90, rows: 30 } }, restarted);
      expect(restarted.body).toEqual({ sessionId: 'term-1', cols: 90, rows: 30, status: 'running' });
      expect(harness.processes).toHaveLength(2);
      expect(harness.processes[0].killed).toBe(true);
      expect(harness.processes[1].options.cwd).toBe('/other');
    } finally { await harness.runtime.shutdown(); }
  });

  it('serializes concurrent restarts without orphaning replacement processes', async () => {
    const harness = createHarness();
    try {
      const create = harness.routes.post.get('/api/terminal/create');
      const restart = harness.routes.post.get('/api/terminal/:sessionId/restart');
      await create({ body: { sessionId: 'term-1', cwd: '/repo' } }, createResponse());
      const first = createResponse();
      const second = createResponse();

      await Promise.all([
        restart({ params: { sessionId: 'term-1' }, body: { cwd: '/first' } }, first),
        restart({ params: { sessionId: 'term-1' }, body: { cwd: '/second' } }, second),
      ]);

      expect(first.statusCode).toBe(200);
      expect(second.statusCode).toBe(200);
      expect(harness.processes).toHaveLength(3);
      expect(harness.processes[0].killed).toBe(true);
      expect(harness.processes[1].killed).toBe(true);
      expect(harness.processes[2].killed).toBe(false);
      expect(harness.processes[2].options.cwd).toBe('/second');
    } finally { await harness.runtime.shutdown(); }
  });

  it('retains exited sessions until explicit close', async () => {
    const harness = createHarness();
    try {
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-1', cwd: '/repo' } }, createResponse());
      harness.processes[0].emitData('last output');
      harness.processes[0].emitExit(7, 0);
      const resize = createResponse();
      harness.routes.post.get('/api/terminal/:sessionId/resize')({ params: { sessionId: 'term-1' }, body: { cols: 80, rows: 24 } }, resize);
      expect(resize.statusCode).toBe(200);
      const closed = createResponse();
      await harness.routes.delete.get('/api/terminal/:sessionId')({ params: { sessionId: 'term-1' } }, closed);
      expect(closed.body).toEqual({ success: true });
    } finally { await harness.runtime.shutdown(); }
  });

  it('escalates close to SIGKILL when a running process ignores SIGTERM', async () => {
    const harness = createHarness();
    try {
      await harness.routes.post.get('/api/terminal/create')({ body: { sessionId: 'term-1', cwd: '/repo' } }, createResponse());
      await harness.routes.delete.get('/api/terminal/:sessionId')({ params: { sessionId: 'term-1' } }, createResponse());
      expect(harness.processes[0].kills).toEqual(['SIGTERM', 'SIGKILL']);
    } finally { await harness.runtime.shutdown(); }
  });

  it('runs snapshot-first attach, scoped I/O, replay, reconnect, and close over a real websocket', async () => {
    const app = express();
    app.use(express.json());
    const server = http.createServer(app);
    const processes = [];
    const loadPtyProvider = async () => ({
      backend: 'fake-pty',
      spawn: () => {
        const data = new Set();
        const exits = new Set();
        const process = {
          pid: 99123,
          killed: false,
          writes: [],
          write(value) { this.writes.push(value); }, resize() {}, kill() { this.killed = true; },
          onData(handler) { data.add(handler); return { dispose: () => data.delete(handler) }; },
          onExit(handler) { exits.add(handler); return { dispose: () => exits.delete(handler) }; },
          emitData(value) { for (const handler of data) handler(value); },
          emitExit(exitCode) { for (const handler of exits) handler({ exitCode, signal: 0 }); },
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
    const address = server.address();
    const base = `http://127.0.0.1:${address.port}`;
    const socketUrl = `ws://127.0.0.1:${address.port}/api/terminal/ws`;
    const sockets = [];

    const open = async () => {
      const socket = new WebSocket(socketUrl);
      sockets.push(socket);
      const messages = [];
      socket.on('message', (raw) => messages.push(readTerminalWsControlFrame(raw)));
      await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
      const next = async (type, sessionId) => {
        for (let attempt = 0; attempt < 100; attempt += 1) {
          const index = messages.findIndex((message) => message?.t === type && (!sessionId || message.s === sessionId));
          if (index >= 0) return messages.splice(index, 1)[0];
          await new Promise((resolve) => setTimeout(resolve, 2));
        }
        throw new Error(`Timed out waiting for ${type}`);
      };
      await next('hello');
      return { socket, next, messages };
    };

    try {
      const created = await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-live', cwd: '/repo', cols: 80, rows: 24 }),
      });
      expect(created.status).toBe(200);
      const secondCreated = await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-second', cwd: '/other', cols: 80, rows: 24 }),
      });
      expect(secondCreated.status).toBe(200);

      const first = await open();
      first.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-live' }));
      first.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-second' }));
      expect(await first.next('snapshot', 'term-live')).toMatchObject({ s: 'term-live', q: 0, history: '', status: 'running' });
      expect(await first.next('snapshot', 'term-second')).toMatchObject({ s: 'term-second', q: 0, history: '', status: 'running' });
      first.socket.send(createTerminalWsControlFrame({ t: 'write', v: 3, s: 'term-live', d: 'echo ok\r' }));
      first.socket.send(createTerminalWsControlFrame({ t: 'write', v: 3, s: 'term-second', d: 'pwd\r' }));
      first.socket.send(createTerminalWsControlFrame({ t: 'write', v: 3, s: 'term-live', d: 'echo next\r' }));
      await new Promise((resolve) => setTimeout(resolve, 5));
      expect(processes[0].writes).toEqual(['echo ok\r', 'echo next\r']);
      expect(processes[1].writes).toEqual(['pwd\r']);

      processes[1].emitData('/other\r\n');
      expect(await first.next('output', 'term-second')).toMatchObject({ s: 'term-second', q: 1, d: '/other\r\n' });
      first.socket.send(createTerminalWsControlFrame({ t: 'detach', v: 3, s: 'term-second' }));
      await new Promise((resolve) => setTimeout(resolve, 5));
      processes[1].emitData('detached\r\n');
      await new Promise((resolve) => setTimeout(resolve, 5));
      expect(first.messages.some((message) => message?.t === 'output' && message.s === 'term-second')).toBe(false);

      processes[0].emitData('ok\r\n');
      expect(await first.next('output', 'term-live')).toMatchObject({ s: 'term-live', q: 1, d: 'ok\r\n' });
      processes[0].emitData('\u001b[6n');
      expect(await first.next('output', 'term-live')).toMatchObject({ s: 'term-live', q: 2, d: '\u001b[6n', r: '' });
      const secondClosed = await fetch(`${base}/api/terminal/term-second`, { method: 'DELETE' });
      expect(secondClosed.status).toBe(200);
      first.socket.close();

      const second = await open();
      second.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-live' }));
      expect(await second.next('snapshot')).toMatchObject({ s: 'term-live', q: 2, history: 'ok\r\n', status: 'running' });
      processes[0].emitExit(7);
      expect(await second.next('exit')).toMatchObject({ s: 'term-live', q: 3, exitCode: 7 });

      const closed = await fetch(`${base}/api/terminal/term-live`, { method: 'DELETE' });
      expect(closed.status).toBe(200);
      expect(await second.next('error')).toMatchObject({ s: 'term-live', code: 'CLOSED', fatal: true });

      await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-kill', cwd: '/repo' }),
      });
      second.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-kill' }));
      await second.next('snapshot');
      const killed = await fetch(`${base}/api/terminal/force-kill`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ cwd: '/repo' }),
      });
      expect(await killed.json()).toEqual({ success: true, killedCount: 1, killedSessionIds: ['term-kill'] });
      expect(await second.next('error')).toMatchObject({ s: 'term-kill', code: 'KILLED', fatal: true });
      expect(processes[2].killed).toBe(true);
    } finally {
      for (const socket of sockets) socket.terminate();
      await runtime.shutdown();
      server.closeAllConnections?.();
      await new Promise((resolve) => server.close(resolve));
    }
  }, 15_000);

  it('bounds flood output: ack-driven suppression, snapshot recovery, capped history, and live HTTP', async () => {
    const app = express();
    app.use(express.json());
    const server = http.createServer(app);
    const processes = [];
    const loadPtyProvider = async () => ({
      backend: 'fake-pty',
      spawn: () => {
        const data = new Set();
        const exits = new Set();
        const process = {
          pid: 99777,
          killed: false,
          writes: [],
          write(value) { this.writes.push(value); }, resize() {}, kill() { this.killed = true; },
          onData(handler) { data.add(handler); return { dispose: () => data.delete(handler) }; },
          onExit(handler) { exits.add(handler); return { dispose: () => exits.delete(handler) }; },
          emitData(value) { for (const handler of data) handler(value); },
          emitExit(exitCode) { for (const handler of exits) handler({ exitCode, signal: 0 }); },
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
    const address = server.address();
    const base = `http://127.0.0.1:${address.port}`;
    const socketUrl = `ws://127.0.0.1:${address.port}/api/terminal/ws`;
    const sockets = [];
    const open = async () => {
      const socket = new WebSocket(socketUrl);
      sockets.push(socket);
      const messages = [];
      socket.on('message', (raw) => messages.push(readTerminalWsControlFrame(raw)));
      await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
      const next = async (type, sessionId) => {
        for (let attempt = 0; attempt < 500; attempt += 1) {
          const index = messages.findIndex((message) => message?.t === type && (!sessionId || message.s === sessionId));
          if (index >= 0) return messages.splice(index, 1)[0];
          await new Promise((resolve) => setTimeout(resolve, 2));
        }
        throw new Error(`Timed out waiting for ${type}`);
      };
      await next('hello');
      return { socket, next, messages };
    };

    try {
      await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-flood', cwd: '/repo', cols: 80, rows: 24 }),
      });
      const client = await open();
      client.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-flood' }));
      await client.next('snapshot', 'term-flood');

      // Small output flows live without any acknowledgment.
      processes[0].emitData('hello\r\n');
      expect(await client.next('output', 'term-flood')).toMatchObject({ d: 'hello\r\n' });

      // Flood ~12.6 MiB in 64 KiB chunks while never acknowledging. Suppression
      // must engage (fallback path: no acks, >8 MiB sent), bounding what this
      // consumer receives. HTTP must keep answering throughout the drain.
      const CHUNKS = 200;
      const chunk = `${'x'.repeat(64 * 1024 - 1)}\n`;
      const latencies = [];
      const probe = (async () => {
        for (let i = 0; i < 40; i += 1) {
          const started = Date.now();
          const response = await fetch(`${base}/api/terminal/sessions`);
          latencies.push(Date.now() - started);
          expect(response.status).toBe(200);
          await new Promise((resolve) => setTimeout(resolve, 25));
        }
      })();
      for (let i = 0; i < CHUNKS; i += 1) processes[0].emitData(chunk);
      await probe;
      await new Promise((resolve) => setTimeout(resolve, 250));
      const outputs = client.messages.filter((message) => message?.t === 'output' && message.s === 'term-flood');
      let receivedBytes = 0;
      for (const message of outputs) receivedBytes += message.d?.length ?? 0;
      expect(outputs.length).toBeLessThan(CHUNKS);
      expect(receivedBytes).toBeLessThan(9 * 1024 * 1024);
      expect(Math.max(...latencies)).toBeLessThan(1000);

      // Exit is never suppressed.
      processes[0].emitExit(3);
      expect(await client.next('exit', 'term-flood')).toMatchObject({ exitCode: 3 });

      // Acknowledging drains the lag: the attachment recovers with a snapshot
      // whose history is capped and reflects the tail, and live output resumes.
      let lastQ = 0;
      for (const message of client.messages) if (typeof message?.q === 'number' && message.s === 'term-flood') lastQ = Math.max(lastQ, message.q);
      client.socket.send(createTerminalWsControlFrame({ t: 'ack', v: 3, s: 'term-flood', q: lastQ }));
      const recovery = await client.next('snapshot', 'term-flood');
      expect(Buffer.byteLength(recovery.history)).toBeLessThanOrEqual(512 * 1024 + 1024);
      expect(recovery.history.endsWith('x\n')).toBe(true);

      // A fresh attachment after the flood gets the same bounded tail.
      const late = await open();
      late.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-flood' }));
      const lateSnapshot = await late.next('snapshot', 'term-flood');
      expect(Buffer.byteLength(lateSnapshot.history)).toBeLessThanOrEqual(512 * 1024 + 1024);
      late.socket.close();
    } finally {
      for (const socket of sockets) socket.terminate();
      await runtime.shutdown();
      server.closeAllConnections?.();
      await new Promise((resolve) => server.close(resolve));
    }
  }, 30_000);

  it('runs viewport-driver claim, follow, release, and disconnect-release over real websockets', async () => {
    const app = express();
    app.use(express.json());
    const server = http.createServer(app);
    const resizes = [];
    const loadPtyProvider = async () => ({
      backend: 'fake-pty',
      spawn: () => ({
        pid: 99456,
        writes: [],
        write() {},
        resize(cols, rows) { resizes.push([cols, rows]); },
        kill() {},
        onData() { return { dispose() {} }; },
        onExit() { return { dispose() {} }; },
      }),
    });
    const runtime = createRuntime(server, {
      app, loadPtyProvider,
      terminalTerminationGraceMs: 10,
      fs: { promises: { stat: async () => ({ isDirectory: () => true }) } },
      searchPathFor: () => '/bin/sh', isExecutable: () => true,
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    const base = `http://127.0.0.1:${address.port}`;
    const socketUrl = `ws://127.0.0.1:${address.port}/api/terminal/ws`;
    const sockets = [];

    const open = async () => {
      const socket = new WebSocket(socketUrl);
      sockets.push(socket);
      const messages = [];
      socket.on('message', (raw) => messages.push(readTerminalWsControlFrame(raw)));
      await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
      const next = async (type) => {
        for (let attempt = 0; attempt < 100; attempt += 1) {
          const index = messages.findIndex((message) => message?.t === type && (type === 'hello' || message.s === 'term-drv'));
          if (index >= 0) return messages.splice(index, 1)[0];
          await new Promise((resolve) => setTimeout(resolve, 2));
        }
        throw new Error(`Timed out waiting for ${type}`);
      };
      const hello = await next('hello');
      return { socket, next, connectionId: hello.connectionId };
    };

    try {
      const created = await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-drv', cwd: '/repo', cols: 80, rows: 24 }),
      });
      expect(created.status).toBe(200);

      const big = await open();
      big.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-drv', cols: 100, rows: 30 }));
      await big.next('snapshot');
      const small = await open();
      small.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-drv', cols: 60, rows: 20 }));
      await small.next('snapshot');
      await new Promise((resolve) => setTimeout(resolve, 5));
      // IDLE min-size: the grid is the pure minimum effective width across
      // attachments (no floor); the narrowest device implicitly owns it.
      expect(resizes.at(-1)).toEqual([60, 20]);

      // Claim: the big screen takes control; PTY follows its viewport even
      // though the small screen is narrower.
      big.socket.send(createTerminalWsControlFrame({ t: 'claimViewport', v: 3, s: 'term-drv', cols: 100, rows: 30 }));
      const smallSeesDriver = await small.next('driverChanged');
      expect(smallSeesDriver).toMatchObject({ cols: 100, rows: 30 });
      expect(typeof smallSeesDriver.driverId).toBe('string');
      expect(smallSeesDriver.driverId).not.toBe(small.connectionId);
      const bigSeesDriver = await big.next('driverChanged');
      expect(bigSeesDriver.driverId).toBe(big.connectionId);
      expect(resizes.at(-1)).toEqual([100, 30]);

      // In DRIVEN mode a follower viewport update must NOT resize the PTY.
      small.socket.send(createTerminalWsControlFrame({ t: 'viewport', v: 3, s: 'term-drv', cols: 50, rows: 15 }));
      await new Promise((resolve) => setTimeout(resolve, 5));
      expect(resizes.at(-1)).toEqual([100, 30]);

      // Release: back to IDLE min-size across remaining attachments.
      big.socket.send(createTerminalWsControlFrame({ t: 'releaseViewport', v: 3, s: 'term-drv' }));
      const released = await small.next('driverChanged');
      expect(released.driverId).toBeNull();
      expect(resizes.at(-1)).toEqual([50, 15]);
      const bigSeesRelease = await big.next('driverChanged');
      expect(bigSeesRelease.driverId).toBeNull();
      // Re-claim from the other side, then drop the driver connection:
      // the driver role must auto-release and the grid must renegotiate.
      small.socket.send(createTerminalWsControlFrame({ t: 'claimViewport', v: 3, s: 'term-drv', cols: 120, rows: 40 }));
      await big.next('driverChanged');
      expect(resizes.at(-1)).toEqual([120, 40]);
      small.socket.terminate();
      const afterDrop = await big.next('driverChanged');
      expect(afterDrop.driverId).toBeNull();
      expect(resizes.at(-1)).toEqual([100, 30]);

      // A releaseViewport from a non-driver must be ignored.
      big.socket.send(createTerminalWsControlFrame({ t: 'releaseViewport', v: 3, s: 'term-drv' }));
      await new Promise((resolve) => setTimeout(resolve, 5));
      expect(resizes.at(-1)).toEqual([100, 30]);
    } finally {
      for (const socket of sockets) socket.terminate();
      await runtime.shutdown();
      server.closeAllConnections?.();
      await new Promise((resolve) => server.close(resolve));
    }
  }, 15_000);

  it('implicit ownership is the dynamic minimum and moves with resizes', async () => {
    const app = express();
    app.use(express.json());
    const server = http.createServer(app);
    const events = [];
    const loadPtyProvider = async () => ({
      backend: 'fake-pty',
      spawn: () => ({
        pid: 99777,
        writes: [],
        write() {},
        resize(cols, rows) { events.push(['resize', cols, rows]); },
        kill() {},
        onData() { return { dispose() {} }; },
        onExit() { return { dispose() {} }; },
      }),
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
      const next = async (type) => {
        for (let attempt = 0; attempt < 100; attempt += 1) {
          const index = messages.findIndex((message) => message?.t === type && (type === 'hello' || message.s === 'term-imp'));
          if (index >= 0) return messages.splice(index, 1)[0];
          await new Promise((resolve) => setTimeout(resolve, 2));
        }
        throw new Error(`Timed out waiting for ${type}`);
      };
      await next('hello');
      return { socket, next };
    };

    try {
      await fetch(`${base}/api/terminal/create`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sessionId: 'term-imp', cwd: '/repo', cols: 90, rows: 30 }),
      });

      // A alone: implicit owner is A, grid follows A's viewport exactly.
      const a = await open();
      a.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-imp', cols: 100, rows: 30 }));
      const alone = await a.next('resized');
      expect(alone).toMatchObject({ ownerId: expect.any(String), cols: 100, rows: 30 });
      const ownerA = alone.ownerId;

      // B joins narrower: grid drops to B's width (pure minimum, no floor)
      // and ownership moves to B's connection.
      const b = await open();
      b.socket.send(createTerminalWsControlFrame({ t: 'attach', v: 3, s: 'term-imp', cols: 60, rows: 20 }));
      const narrowed = await a.next('resized');
      expect(narrowed).toMatchObject({ cols: 60, rows: 20 });
      expect(narrowed.ownerId).not.toBe(ownerA);

      // B widens past A: ownership falls back to A and the grid grows.
      b.socket.send(createTerminalWsControlFrame({ t: 'viewport', v: 3, s: 'term-imp', cols: 120, rows: 40 }));
      const widened = await a.next('resized');
      expect(widened).toMatchObject({ ownerId: ownerA, cols: 100, rows: 30 });

      // A claims forced ownership: B cannot retake it by narrowing further;
      // the grid stays locked to A's effective width.
      a.socket.send(createTerminalWsControlFrame({ t: 'claimViewport', v: 3, s: 'term-imp', cols: 100, rows: 30 }));
      await b.next('driverChanged');
      b.socket.send(createTerminalWsControlFrame({ t: 'viewport', v: 3, s: 'term-imp', cols: 40, rows: 15 }));
      await new Promise((resolve) => setTimeout(resolve, 10));
      expect(events.at(-1)).toEqual(['resize', 100, 30]);

      // A releases: implicit negotiation resumes at the new minimum (B's 40).
      a.socket.send(createTerminalWsControlFrame({ t: 'releaseViewport', v: 3, s: 'term-imp' }));
      const released = await b.next('driverChanged');
      expect(released.driverId).toBeNull();
      expect(events.at(-1)).toEqual(['resize', 40, 15]);
    } finally {
      for (const socket of sockets) socket.terminate();
      await runtime.shutdown();
      server.closeAllConnections?.();
      await new Promise((resolve) => server.close(resolve));
    }
  }, 15_000);
});
