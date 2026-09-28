/**
 * server-startup-runtime 测试套件：未捕获异常策略（单个异常存活、持续
 * 异常风暴才关停、unhandledRejection 仅记录不关停）与 EADDRINUSE 端口
 * 占用重试（等前一个实例释放端口、非占用错误立即失败、重试窗口耗尽后
 * 拒绝）。
 */
import { describe, expect, test } from 'bun:test';
import { randomUUID as _randomUUID } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { createServerStartupRuntime } from './server-startup-runtime.js';


/**
 * 中文补充：桌面应用内嵌本服务且没有任何东西会重启它，若单个未捕获
 * 异常就关停，任何零星 socket 错误都会变成“实例失联直到手动重启”。
 * 因此只有持续的异常风暴才触发关停。
 */
/**
 * The desktop app embeds this server and nothing restarts it, so shutting down
 * on a single uncaught exception turned every stray socket error into "the
 * instance is unreachable until restarted". Only a sustained storm shuts down.
 */
describe('uncaught exception policy', () => {
  // 构造挂好进程处理器的事件型 process 桩，并统计 gracefulShutdown 次数。
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

// EADDRINUSE 绑定重试套件。
describe('EADDRINUSE bind retry', () => {
  // 满足运行时 crypto.randomUUID 需求的最小桩。
  const crypto = { randomUUID: _randomUUID };
  // 伪造 net.Server：前 N 次 listen 以指定错误码失败，之后触发 listening。
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

  // 以 1ms 重试间隔 / 2s 窗口创建监听运行时；overrides 可覆盖时序参数。
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

/** 临时替换 console.log 以捕获输出行，返回 calls 与 restore。 */
const mockConsoleLog = () => {
  const calls = [];
  const original = console.log;
  console.log = (...args) => {
    calls.push(args.join(' '));
  };
  return { calls, restore: () => { console.log = original; } };
};
