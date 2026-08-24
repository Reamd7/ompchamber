import { describe, expect, test } from 'bun:test';
import { randomUUID as _randomUUID } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { createServerStartupRuntime } from './server-startup-runtime.js';


/**
 * The desktop app embeds this server and nothing restarts it, so shutting down
 * on a single uncaught exception turned every stray socket error into "the
 * instance is unreachable until restarted". Only a sustained storm shuts down.
 */
describe('uncaught exception policy', () => {
  const setup = () => {
    const fakeProcess = new EventEmitter();
    let shutdowns = 0;
    const runtime = createServerStartupRuntime({
      process: fakeProcess,
      gracefulShutdown: () => { shutdowns += 1; },
      getSignalsAttached: () => true,
      setSignalsAttached: () => {},
      syncToHmrState: () => {},
    });
    runtime.attachProcessHandlers({ attachSignals: false });
    return { fakeProcess, shutdowns: () => shutdowns };
  };

  test('a single uncaught exception keeps the server running', () => {
    const { fakeProcess, shutdowns } = setup();
    fakeProcess.emit('uncaughtException', new Error('setTypeOfService EINVAL'));
    expect(shutdowns()).toBe(0);
  });

  test('a storm of uncaught exceptions still shuts down', () => {
    const { fakeProcess, shutdowns } = setup();
    for (let i = 0; i < 11; i += 1) {
      fakeProcess.emit('uncaughtException', new Error(`stray ${i}`));
    }
    expect(shutdowns()).toBeGreaterThan(0);
  });

  test('an unhandled rejection is logged without shutting down', () => {
    const { fakeProcess, shutdowns } = setup();
    fakeProcess.emit('unhandledRejection', new Error('late failure'), Promise.resolve());
    expect(shutdowns()).toBe(0);
  });
});

describe('EADDRINUSE bind retry', () => {
  const crypto = { randomUUID: _randomUUID };
  const createFakeServer = ({ failuresBeforeSuccess = 0, errorCode = 'EADDRINUSE' } = {}) => {
    const emitter = new EventEmitter();
    let attempts = 0;
    emitter.address = () => ({ port: 4321 });
    emitter.listen = (_port, _host, onListening) => {
      attempts += 1;
      if (attempts <= failuresBeforeSuccess) {
        queueMicrotask(() => {
          emitter.emit('error', Object.assign(new Error('listen EADDRINUSE'), { code: errorCode }));
        });
        return;
      }
      queueMicrotask(() => {
        emitter.emit('listening');
        onListening?.();
      });
    };
    return { server: emitter, attempts: () => attempts };
  };

  const createListenRuntime = (server, overrides = {}) => createServerStartupRuntime({
    process: { env: {} },
    crypto,
    server,
    gracefulShutdown: () => {},
    getSignalsAttached: () => true,
    setSignalsAttached: () => {},
    syncToHmrState: () => {},
    bindRetryIntervalMs: 1,
    bindRetryWindowMs: 2_000,
    ...overrides,
  });

  test('waits out EADDRINUSE until the previous instance releases the port', async () => {
    const logSpy = mockConsoleLog();
    const { server, attempts } = createFakeServer({ failuresBeforeSuccess: 2 });

    const result = await createListenRuntime(server).startListeningAndMaybeTunnel({
      port: 4321,
      bindHost: '127.0.0.1',
    });

    expect(result.activePort).toBe(4321);
    expect(attempts()).toBe(3);
    expect(logSpy.calls.some((line) => line.includes('Port 4321 is still in use'))).toBe(true);
    logSpy.restore();
  });

  test('a non-EADDRINUSE listen error rejects immediately without retrying', async () => {
    const { server, attempts } = createFakeServer({ failuresBeforeSuccess: 5, errorCode: 'EACCES' });

    await expect(createListenRuntime(server).startListeningAndMaybeTunnel({
      port: 80,
      bindHost: '0.0.0.0',
    })).rejects.toThrow('listen EADDRINUSE');
    expect(attempts()).toBe(1);
  });

  test('EADDRINUSE that never clears rejects once the retry window closes', async () => {
    const { server, attempts } = createFakeServer({ failuresBeforeSuccess: Number.MAX_SAFE_INTEGER });

    await expect(createListenRuntime(server, { bindRetryWindowMs: 0 }).startListeningAndMaybeTunnel({
      port: 4321,
      bindHost: '127.0.0.1',
    })).rejects.toThrow('EADDRINUSE');
    expect(attempts()).toBe(1);
  });
});

const mockConsoleLog = () => {
  const calls = [];
  const original = console.log;
  console.log = (...args) => {
    calls.push(args.join(' '));
  };
  return { calls, restore: () => { console.log = original; } };
};
