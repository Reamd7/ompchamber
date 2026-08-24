import { afterEach, describe, expect, it, vi } from 'vitest';

import { createGracefulShutdownRuntime } from './shutdown-runtime.js';

const createRuntime = (server, overrides = {}) => createGracefulShutdownRuntime({
  process: { exit: vi.fn() },
  shutdownTimeoutMs: 1000,
  shutdownWatchdogTimeoutMs: 5_000,
  getExitOnShutdown: () => false,
  getIsShuttingDown: () => false,
  setIsShuttingDown: vi.fn(),
  syncToHmrState: vi.fn(),
  openCodeWatcherRuntime: { stop: vi.fn() },
  sessionRuntime: { dispose: vi.fn() },
  scheduledTasksRuntime: { stop: vi.fn() },
  getHealthCheckInterval: () => null,
  clearHealthCheckInterval: vi.fn(),
  getTerminalRuntime: () => null,
  setTerminalRuntime: vi.fn(),
  getMessageStreamRuntime: () => null,
  setMessageStreamRuntime: vi.fn(),
  shouldSkipOpenCodeStop: () => true,
  getOpenCodePort: () => null,
  getOpenCodeProcess: () => null,
  setOpenCodeProcess: vi.fn(),
  killProcessOnPort: vi.fn(),
  waitForPortRelease: vi.fn(async () => true),
  getServer: () => server,
  getUiAuthController: () => null,
  setUiAuthController: vi.fn(),
  getActiveTunnelController: () => null,
  setActiveTunnelController: vi.fn(),
  tunnelAuthController: { clearActiveTunnel: vi.fn() },
  ...overrides,
});

describe('graceful shutdown runtime', () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('clears the server close timeout when the server closes first', async () => {
    vi.useFakeTimers();
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const server = {
      close: vi.fn((callback) => {
        callback();
      }),
    };

    const runtime = createRuntime(server);
    await runtime.gracefulShutdown({ exitProcess: false });

    await vi.advanceTimersByTimeAsync(1000);

    expect(warnSpy).not.toHaveBeenCalledWith('Server close timeout reached, forcing shutdown');
    expect(vi.getTimerCount()).toBe(0);
  });

  it('watchdog force-exits and names the phase when a shutdown stage hangs', async () => {
    vi.useFakeTimers();
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    const processExit = vi.fn();
    const runtime = createRuntime(null, {
      process: { exit: processExit },
      getMessageStreamRuntime: () => ({ close: () => new Promise(() => {}) }),
      setMessageStreamRuntime: vi.fn(),
    });

    const shutdown = runtime.gracefulShutdown({ exitProcess: true });
    await vi.advanceTimersByTimeAsync(5_000);

    expect(processExit).toHaveBeenCalledWith(1);
    expect(errorSpy).toHaveBeenCalledWith(
      expect.stringContaining('Graceful shutdown timed out after 5000ms (stuck at: closing message stream runtime)')
    );
    // The hung runShutdown promise never settles; dropping it must be silent.
    shutdown.catch(() => {});
  });

  it('watchdog unblocks an embedded (exitProcess: false) caller without exiting the process', async () => {
    vi.useFakeTimers();
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    const processExit = vi.fn();
    const runtime = createRuntime(null, {
      process: { exit: processExit },
      getMessageStreamRuntime: () => ({ close: () => new Promise(() => {}) }),
      setMessageStreamRuntime: vi.fn(),
    });

    const shutdown = runtime.gracefulShutdown({ exitProcess: false });
    await vi.advanceTimersByTimeAsync(5_000);

    expect(processExit).not.toHaveBeenCalled();
    expect(errorSpy).toHaveBeenCalled();
    shutdown.catch(() => {});
  });
});
