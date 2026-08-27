import { describe, test, expect, mock } from 'bun:test';
import {
  WORKER_SELECTOR_PREFIX,
  isWorkerSelector,
  isDispatchableInvocation,
  resolveWorkerDispatch,
  runWorkerDispatch,
} from './worker-dispatch.ts';
import type {
  IpcWorkerRunOptions,
  IpcWorkerTransport,
  LoadableWorkerDispatch,
} from './worker-dispatch.ts';
import type { RejectionInterceptor } from '@oh-my-pi/pi-coding-agent/eval/js/worker-core';

// Every selector the SDK CLI dispatches (pi-coding-agent src/cli.ts
// runWorkerEntrypoint + the protocol constants: launch/protocol.ts,
// lsp/mux/protocol.ts, blob-broker/protocol.ts, tools/computer/protocol.ts,
// launch/terminal-output-worker-protocol.ts). A selector missing from our
// table regresses to the zombie-host leak this module exists to prevent, so
// the full list is asserted here to force a deliberate decision on bumps.
const SDK_SELECTORS = [
  '__omp_worker_tiny_inference',
  '__omp_worker_stats_sync',
  '__omp_worker_tab',
  '__omp_worker_js_eval',
  '__omp_worker_js_eval_process',
  '__omp_worker_stt',
  '__omp_worker_tts',
  '__omp_worker_mnemopi_embed',
  '__omp_worker_daemon_broker',
  '__omp_worker_lsp_mux',
  '__omp_worker_blob_broker',
  '__omp_worker_computer',
  '__omp_worker_terminal_output',
];

describe('worker-dispatch selector recognition', () => {
  test('recognizes the reserved selector prefix', () => {
    expect(WORKER_SELECTOR_PREFIX).toBe('__omp_worker_');
    expect(isWorkerSelector('__omp_worker_daemon_broker')).toBe(true);
    expect(isWorkerSelector('serve')).toBe(false);
    expect(isWorkerSelector(undefined)).toBe(false);
  });

  test('browser-relay is treated as a dispatchable invocation', () => {
    expect(isDispatchableInvocation('browser-relay')).toBe(true);
    expect(isDispatchableInvocation('__omp_worker_stt')).toBe(true);
    expect(isDispatchableInvocation('serve')).toBe(false);
    expect(isDispatchableInvocation(undefined)).toBe(false);
  });

  test('every SDK worker selector has an explicit disposition', () => {
    for (const selector of SDK_SELECTORS) {
      const entry = resolveWorkerDispatch(selector);
      expect(entry).not.toBeNull();
      expect(['env-server', 'ipc-worker', 'self-runner', 'unsupported']).toContain(entry.kind);
    }
  });

  test('unknown selectors resolve to null and never serve', () => {
    expect(resolveWorkerDispatch('__omp_worker_from_the_future')).toBeNull();
    expect(resolveWorkerDispatch('serve')).toBeNull();
  });
});

describe('worker-dispatch execution', () => {
  const captureStderr = () => {
    const chunks = [];
    const original = process.stderr.write.bind(process.stderr);
    process.stderr.write = (chunk) => {
      chunks.push(String(chunk));
      return true;
    };
    return () => {
      process.stderr.write = original;
      return chunks.join('');
    };
  };

  test('env-server selectors await the starter with no arguments', async () => {
    const starter = mock(async () => {});
    const loadModule = mock(async (_entry: LoadableWorkerDispatch) => ({ startDaemonBrokerFromEnvironment: starter }));
    const dispatched = await runWorkerDispatch('__omp_worker_daemon_broker', { loadModule });
    expect(dispatched).toBe(true);
    expect(loadModule.mock.calls[0][0].module).toBe('@oh-my-pi/pi-coding-agent/launch/broker');
    expect(starter).toHaveBeenCalledTimes(1);
    expect(starter.mock.calls[0]).toHaveLength(0);
  });

  test('ipc-worker selectors hand the starter a transport over process IPC', async () => {
    const starter = mock((_transport: IpcWorkerTransport) => {});
    const loadModule = mock(async () => ({ startSttWorker: starter }));
    let started;
    const ipcWorker = mock(async (start: (transport: IpcWorkerTransport) => void, options?: IpcWorkerRunOptions) => {
      started = true;
      expect(options).toBeUndefined();
      const transport = { send: () => {}, sendAndFlush: async () => {}, onMessage: () => () => {} };
      start(transport);
      expect(starter).toHaveBeenCalledWith(transport);
    });
    const dispatched = await runWorkerDispatch('__omp_worker_stt', { loadModule, ipcWorker });
    expect(dispatched).toBe(true);
    expect(started).toBe(true);
  });

  test('js eval process receives a rejection interceptor and rethrowing sends', async () => {
    const starter = mock((_transport: IpcWorkerTransport, _interceptor: RejectionInterceptor) => {});
    const loadModule = mock(async () => ({ startJsEvalProcess: starter }));
    let seenOptions;
    await runWorkerDispatch('__omp_worker_js_eval_process', {
      loadModule,
      ipcWorker: async (start, options) => {
        seenOptions = options;
        const transport = { send: () => {}, sendAndFlush: async () => {}, onMessage: () => () => {} };
        start(transport);
      },
    });
    expect(seenOptions).toEqual({ rethrowConnectedSendErrors: true });
    expect(starter.mock.calls[0]).toHaveLength(2);
    expect(typeof starter.mock.calls[0][1]).toBe('function');
  });

  test('self-runner selectors call the starter directly', async () => {
    const starter = mock(() => {});
    const loadModule = mock(async () => ({ startComputerWorker: starter }));
    const dispatched = await runWorkerDispatch('__omp_worker_computer', { loadModule });
    expect(dispatched).toBe(true);
    expect(starter).toHaveBeenCalledTimes(1);
    expect(starter.mock.calls[0]).toHaveLength(0);
  });

  test('unsupported selectors refuse to run instead of serving', async () => {
    const restore = captureStderr();
    const dispatched = await runWorkerDispatch('browser-relay', {});
    const output = restore();
    expect(dispatched).toBe(false);
    expect(output).toContain('browser-relay');
    expect(output).toContain('refusing selector');
  });

  test('thread-only selectors refuse to run', async () => {
    const restore = captureStderr();
    const dispatched = await runWorkerDispatch('__omp_worker_js_eval', {});
    const output = restore();
    expect(dispatched).toBe(false);
    expect(output).toContain('worker_threads');
  });

  test('unknown selectors report refusal', async () => {
    const restore = captureStderr();
    const dispatched = await runWorkerDispatch('__omp_worker_unknown', {});
    restore();
    expect(dispatched).toBe(false);
  });

  test('a worker entry missing its export is a hard error, not a silent serve', async () => {
    const loadModule = mock(async () => ({}));
    expect(runWorkerDispatch('__omp_worker_daemon_broker', { loadModule })).rejects.toThrow(
      /does not export startDaemonBrokerFromEnvironment/,
    );
  });
});
