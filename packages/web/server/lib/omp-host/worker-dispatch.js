// Worker-selector dispatch for the omp host entrypoint.
//
// The embedded @oh-my-pi/pi-coding-agent relaunches its own executable into
// worker modes (daemon broker, LSP mux, blob broker, ONNX inference workers,
// the JS-eval kernel, ...) using `process.execPath` plus a `__omp_worker_*`
// argv selector (SDK `subprocess/worker-client.ts` `resolveWorkerSpawnCmd`).
// In the packaged app `process.execPath` IS omp-host(.exe) itself, and the omp
// CLI's dispatch (`cli.ts` `runWorkerEntrypoint`) never runs inside it. Before
// this table existed every worker spawn silently booted a full HTTP host on a
// random port that nothing ever tore down — the "hundreds of omp-host.exe
// processes" leak: each failed daemon-broker connect spawned one more zombie
// (~1 per 10s retry cycle while the browser tool kept retrying).
//
// This mirrors the SDK CLI's dispatch for every selector the embedded engine
// can spawn as a SUBPROCESS. Selectors that only make sense inside the real
// CLI (worker-thread entries that hard-throw without `parentPort`, the omp
// stats sync worker, the `browser-relay` CLI command) are explicit
// no-serve exits: an invocation we cannot honor must fail fast, never become
// a zombie host. Keep this table in sync when bumping the SDK version.

const SDK = '@oh-my-pi/pi-coding-agent';

export const WORKER_SELECTOR_PREFIX = '__omp_worker_';

export const isWorkerSelector = (value) =>
  typeof value === 'string' && value.startsWith(WORKER_SELECTOR_PREFIX);

// `browser-relay` is not a worker selector but the browser relay daemon spawns
// this exact argv when relay mode is enabled; without this guard it would
// leak a host exactly like the broker selectors did.
export const isDispatchableInvocation = (value) =>
  isWorkerSelector(value) || value === 'browser-relay';

const WORKER_DISPATCH = {
  '__omp_worker_daemon_broker': {
    kind: 'env-server', module: `${SDK}/launch/broker`, starter: 'startDaemonBrokerFromEnvironment',
    // NOTE: loaders must stay literal `import('...')` thunks — a computed
    // specifier survives `bun build --compile` as a runtime lookup and then
    // fails inside the packaged binary (no node_modules to resolve against).
    load: () => import('@oh-my-pi/pi-coding-agent/launch/broker'),
  },
  '__omp_worker_lsp_mux': {
    kind: 'env-server', module: `${SDK}/lsp/mux/server`, starter: 'startLspMuxFromEnvironment',
    load: () => import('@oh-my-pi/pi-coding-agent/lsp/mux/server'),
  },
  '__omp_worker_blob_broker': {
    kind: 'env-server', module: `${SDK}/blob-broker/server`, starter: 'startBlobBrokerFromEnvironment',
    load: () => import('@oh-my-pi/pi-coding-agent/blob-broker/server'),
  },
  '__omp_worker_tiny_inference': {
    kind: 'ipc-worker', module: `${SDK}/tiny/worker`, starter: 'startTinyTitleWorker',
    load: () => import('@oh-my-pi/pi-coding-agent/tiny/worker'),
  },
  '__omp_worker_stt': {
    kind: 'ipc-worker', module: `${SDK}/stt/asr-worker`, starter: 'startSttWorker',
    load: () => import('@oh-my-pi/pi-coding-agent/stt/asr-worker'),
  },
  '__omp_worker_tts': {
    kind: 'ipc-worker', module: `${SDK}/tts/tts-worker`, starter: 'startTtsWorker',
    load: () => import('@oh-my-pi/pi-coding-agent/tts/tts-worker'),
  },
  '__omp_worker_mnemopi_embed': {
    kind: 'ipc-worker', module: `${SDK}/mnemopi/embed-worker`, starter: 'startMnemopiEmbedWorker',
    load: () => import('@oh-my-pi/pi-coding-agent/mnemopi/embed-worker'),
  },
  '__omp_worker_js_eval_process': {
    kind: 'ipc-worker', module: `${SDK}/eval/js/process-entry`, starter: 'startJsEvalProcess',
    load: () => import('@oh-my-pi/pi-coding-agent/eval/js/process-entry'),
    interceptorArg: true, rethrowConnectedSendErrors: true,
  },
  '__omp_worker_computer': {
    kind: 'self-runner', module: `${SDK}/tools/computer/worker-entry`, starter: 'startComputerWorker',
    load: () => import('@oh-my-pi/pi-coding-agent/tools/computer/worker-entry'),
  },
  '__omp_worker_stats_sync': {
    kind: 'unsupported',
    reason: 'stats sync worker belongs to the omp TUI stats package, not the embedded host',
  },
  '__omp_worker_tab': {
    kind: 'unsupported',
    reason: 'tab worker is a worker_threads entry and cannot run as a subprocess',
  },
  '__omp_worker_js_eval': {
    kind: 'unsupported',
    reason: 'js eval worker is a worker_threads entry and cannot run as a subprocess',
  },
  '__omp_worker_terminal_output': {
    kind: 'unsupported',
    reason: 'terminal output worker is a worker_threads entry and cannot run as a subprocess',
  },
  'browser-relay': {
    kind: 'unsupported',
    reason: 'browser relay needs the omp CLI command graph; run `omp browser-relay` manually',
  },
};

/** Descriptor for a known selector, or null for unknown selectors. */
export const resolveWorkerDispatch = (arg) =>
  Object.prototype.hasOwnProperty.call(WORKER_DISPATCH, arg) ? WORKER_DISPATCH[arg] : null;

// The js-eval kernel requires a rejection interceptor (SDK `RejectionInterceptor`
// = `(handler) => uninstall`); pi-utils is not a direct workspace dependency, so
// provide the process-level equivalent: forward unhandled rejections to the
// kernel (a cell that rejects must fail that cell, not kill the process).
const interceptUnhandledRejections = (interceptor) => {
  const listener = (reason) => {
    try {
      interceptor(reason);
    } catch {
      // The kernel's own handling must never take the worker down.
    }
  };
  process.on('unhandledRejection', listener);
  return () => process.off('unhandledRejection', listener);
};

// Port of cli.ts `runIpcSubprocessWorker` (child side): wire the worker's
// typed transport onto process IPC, stay alive while idle, and SIGKILL on
// parent disconnect so native finalizers (onnxruntime) never run here.
const runIpcSubprocessWorker = async (start, options = {}) => {
  const { promise: shuttingDown, resolve: shutdown } = Promise.withResolvers();
  const ipcSend = () => process.send;
  const send = (message) => {
    const sender = ipcSend();
    if (!sender) {
      shutdown();
      return;
    }
    try {
      sender.call(process, message);
    } catch (error) {
      if (options.rethrowConnectedSendErrors && process.connected) throw error;
      shutdown();
    }
  };
  const sendAndFlush = (message) => {
    const sender = ipcSend();
    if (!sender) {
      shutdown();
      return Promise.resolve();
    }
    const { promise, resolve } = Promise.withResolvers();
    try {
      sender.call(process, message, () => resolve());
    } catch {
      shutdown();
      resolve();
    }
    return promise;
  };
  start({
    send,
    sendAndFlush,
    onMessage(handler) {
      const wrap = (data) => handler(data);
      process.on('message', wrap);
      return () => process.off('message', wrap);
    },
  });
  const keepalive = setInterval(() => {}, 2 ** 30);
  process.on('disconnect', () => shutdown());
  try {
    await shuttingDown;
  } finally {
    clearInterval(keepalive);
  }
  process.kill(process.pid, 'SIGKILL');
};

/**
 * Run the worker selected by `arg`.
 *
 * Returns true when the selector was dispatched (the worker owns the rest of
 * this process's lifetime — callers must NOT serve), false when the selector
 * is unsupported or unknown (caller must exit non-zero without serving).
 *
 * `deps.loadModule` / `deps.ipcWorker` exist for tests; production uses the
 * per-entry literal import thunk so `bun build --compile` embeds the worker
 * module in the binary.
 */
export const runWorkerDispatch = async (arg, deps = {}) => {
  const loadModule = deps.loadModule ?? ((entry) => entry.load());
  const ipcWorker = deps.ipcWorker ?? runIpcSubprocessWorker;
  const entry = resolveWorkerDispatch(arg);
  if (!entry) return false;
  if (entry.kind === 'unsupported') {
    process.stderr.write(`[omp-host] refusing selector ${arg}: ${entry.reason}\n`);
    return false;
  }

  const module = await loadModule(entry);
  const starter = module?.[entry.starter];
  if (typeof starter !== 'function') {
    throw new Error(`worker entry ${entry.module} does not export ${entry.starter}`);
  }

  if (entry.kind === 'env-server') {
    await starter();
    return true;
  }
  if (entry.kind === 'self-runner') {
    starter();
    return true;
  }
  await ipcWorker(
    (transport) => (entry.interceptorArg ? starter(transport, interceptUnhandledRejections) : starter(transport)),
    entry.rethrowConnectedSendErrors ? { rethrowConnectedSendErrors: true } : undefined,
  );
  return true;
};
