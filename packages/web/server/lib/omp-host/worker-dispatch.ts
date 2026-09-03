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

import type { RejectionInterceptor } from '@oh-my-pi/pi-coding-agent/eval/js/worker-core';

const SDK = '@oh-my-pi/pi-coding-agent';

export const WORKER_SELECTOR_PREFIX = '__omp_worker_';

export const isWorkerSelector = (value: string | undefined): value is string =>
  value !== undefined && value.startsWith(WORKER_SELECTOR_PREFIX);

// `browser-relay` is not a worker selector but the browser relay daemon spawns
// this exact argv when relay mode is enabled; without this guard it would
// leak a host exactly like the broker selectors did.
export const isDispatchableInvocation = (value: string | undefined): boolean =>
  isWorkerSelector(value) || value === 'browser-relay';
/** Starter export every dispatchable module provides by name; dispatch validates callability with a typeof check before invoking (its parse step). */
export type WorkerStarter = (
  transport?: IpcWorkerTransport,
  interceptor?: RejectionInterceptor,
) => void | Promise<void>;

/** Module namespace a dispatchable selector loads, as dispatch reads it: the starter resolved by export name (namespaces also export helpers/classes dispatch never reads). */
export type WorkerModule = Record<string, WorkerStarter | undefined>;

/** Literal `import('...')` thunk (bun build --compile embeds the module). */
export type WorkerLoadThunk = () => Promise<WorkerModule>;

/**
 * Type-only adapter collapsing one embedded module namespace to the starter
 * read-view above. The literal import thunks return their real heterogeneous
 * namespaces; this is the single boundary where they erase to the one shape
 * dispatch reads. The thunk itself is returned unchanged.
 */
const starterLoad = <M,>(load: () => Promise<M>): WorkerLoadThunk =>
  // SAFETY: namespace-to-read-view erasure only — the runtime thunk is
  // returned unchanged and runWorkerDispatch re-validates the starter is
  // callable via typeof before invoking it.
  load as WorkerLoadThunk;

/** env-server selector: standalone server started from the environment. */
export interface EnvServerDispatch {
  kind: 'env-server';
  module: string;
  starter: string;
  load: WorkerLoadThunk;
}

/** ipc-worker selector: typed transport wired onto process IPC. */
export interface IpcWorkerDispatch {
  kind: 'ipc-worker';
  module: string;
  starter: string;
  load: WorkerLoadThunk;
  interceptorArg?: boolean;
  rethrowConnectedSendErrors?: boolean;
}

/** self-runner selector: the starter owns the rest of the process. */
export interface SelfRunnerDispatch {
  kind: 'self-runner';
  module: string;
  starter: string;
  load: WorkerLoadThunk;
}

/** Explicit no-serve exit for a selector we cannot honor as a subprocess. */
export interface UnsupportedDispatch {
  kind: 'unsupported';
  reason: string;
}

/** Descriptor for one argv selector (WORKER_DISPATCH's value shape). */
export type WorkerDispatchEntry =
  | EnvServerDispatch
  | IpcWorkerDispatch
  | SelfRunnerDispatch
  | UnsupportedDispatch;

/** A dispatch entry whose module we can actually load and start. */
export type LoadableWorkerDispatch = Exclude<WorkerDispatchEntry, UnsupportedDispatch>;

const WORKER_DISPATCH = {
  '__omp_worker_daemon_broker': {
    kind: 'env-server', module: `${SDK}/launch/broker`, starter: 'startDaemonBrokerFromEnvironment',
    // NOTE: loaders must stay literal `import('...')` thunks — a computed
    // specifier survives `bun build --compile` as a runtime lookup and then
    // fails inside the packaged binary (no node_modules to resolve against).
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/launch/broker')),
  },
  '__omp_worker_lsp_mux': {
    kind: 'env-server', module: `${SDK}/lsp/mux/server`, starter: 'startLspMuxFromEnvironment',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/lsp/mux/server')),
  },
  '__omp_worker_blob_broker': {
    kind: 'env-server', module: `${SDK}/blob-broker/server`, starter: 'startBlobBrokerFromEnvironment',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/blob-broker/server')),
  },
  '__omp_worker_tiny_inference': {
    kind: 'ipc-worker', module: `${SDK}/tiny/worker`, starter: 'startTinyTitleWorker',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/tiny/worker')),
  },
  '__omp_worker_stt': {
    kind: 'ipc-worker', module: `${SDK}/stt/asr-worker`, starter: 'startSttWorker',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/stt/asr-worker')),
  },
  '__omp_worker_tts': {
    kind: 'ipc-worker', module: `${SDK}/tts/tts-worker`, starter: 'startTtsWorker',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/tts/tts-worker')),
  },
  '__omp_worker_mnemopi_embed': {
    kind: 'ipc-worker', module: `${SDK}/mnemopi/embed-worker`, starter: 'startMnemopiEmbedWorker',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/mnemopi/embed-worker')),
  },
  '__omp_worker_js_eval_process': {
    kind: 'ipc-worker', module: `${SDK}/eval/js/process-entry`, starter: 'startJsEvalProcess',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/eval/js/process-entry')),
    interceptorArg: true, rethrowConnectedSendErrors: true,
  },
  '__omp_worker_computer': {
    kind: 'self-runner', module: `${SDK}/tools/computer/worker-entry`, starter: 'startComputerWorker',
    load: starterLoad(() => import('@oh-my-pi/pi-coding-agent/tools/computer/worker-entry')),
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
} satisfies Record<string, WorkerDispatchEntry>;

export const resolveWorkerDispatch = (arg: string): WorkerDispatchEntry | null => {
  if (!Object.prototype.hasOwnProperty.call(WORKER_DISPATCH, arg)) return null;
  // SAFETY: the hasOwnProperty guard above proves `arg` names one of the
  // table's declared selector keys, so the index is always in bounds.
  return WORKER_DISPATCH[arg as keyof typeof WORKER_DISPATCH];
};

// The js-eval kernel requires a rejection interceptor (SDK `RejectionInterceptor`
const interceptUnhandledRejections: RejectionInterceptor = (interceptor) => {
  // Route through the generic EventEmitter view: bun-types' NodeJS.Process
  // merge redeclares `off` with only its memoryPressure overload, hiding the
  // generic removal the SDK type graph pulls in (see packages/web
  // tsconfig.server.json + @types/node 24 / bun-types overrides.d.ts).
  const bus: NodeJS.EventEmitter = process;
  const listener = (reason: Error | undefined, _promise: Promise<unknown>) => {
    try {
      interceptor(reason);
    } catch {
      // The kernel's own handling must never take the worker down.
    }
  };
  bus.on('unhandledRejection', listener);
  return () => bus.off('unhandledRejection', listener);
};

// Port of cli.ts `runIpcSubprocessWorker` (child side): wire the worker's
// typed transport onto process IPC, stay alive while idle, and SIGKILL on
// parent disconnect so native finalizers (onnxruntime) never run here.
/**
 * One JSON-serializable frame crossing the worker's process-IPC channel.
 * The frame protocol is owned by the selector's starter (the SDK's
 * tiny/stt/tts/mnemopi/js-eval worker protocols); this transport ferries
 * frames verbatim and never inspects them.
 */
export type IpcWorkerMessage =
  | string
  | number
  | boolean
  | null
  | readonly IpcWorkerMessage[]
  | { [field: string]: IpcWorkerMessage };

/**
 * Typed transport handed to ipc-worker starters (SDK subprocess contract):
 * `send` fire-and-forget, `sendAndFlush` callback-flushed, `onMessage`
 * returns its uninstall.
 */
export interface IpcWorkerTransport {
  send: (message: IpcWorkerMessage) => void;
  sendAndFlush: (message: IpcWorkerMessage) => Promise<void>;
  onMessage: (handler: (message: IpcWorkerMessage) => void) => () => void;
}

/** ipc-worker run knobs (the js-eval process needs both). */
export interface IpcWorkerRunOptions {
  rethrowConnectedSendErrors?: boolean;
}

const runIpcSubprocessWorker = async (
  start: (transport: IpcWorkerTransport) => void,
  options: IpcWorkerRunOptions = {},
): Promise<void> => {
  const { promise: shuttingDown, resolve: shutdown } = Promise.withResolvers<void>();
  const ipcSend = () => process.send;
  const send = (message: IpcWorkerMessage) => {
    const sender = ipcSend();
    if (!sender) {
      shutdown();
      return;
    }
    try {
      // SAFETY: process.send's bound form is (message, callback, handle);
      // the optional args are omitted exactly as the direct call did.
      (sender as (message: IpcWorkerMessage, callback?: () => void, handle?: NodeJS.ProcessEnv) => boolean).call(process, message);
    } catch (error) {
      if (options.rethrowConnectedSendErrors && process.connected) throw error;
      shutdown();
    }
  };
  const sendAndFlush = (message: IpcWorkerMessage) => {
    const sender = ipcSend();
    if (!sender) {
      shutdown();
      return Promise.resolve();
    }
    const { promise, resolve } = Promise.withResolvers<void>();
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
      // SAFETY: process 'message' payloads on this channel are the starter's
      // JSON frames; the transport ferries them verbatim by contract. The
      // generic EventEmitter view avoids the bun-types `off` merge (above).
      const bus: NodeJS.EventEmitter = process;
      const wrap = (data: IpcWorkerMessage) => handler(data);
      bus.on('message', wrap);
      return () => bus.off('message', wrap);
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
/** Test/production seams: `loadModule` replaces the per-entry literal import
 * thunk and `ipcWorker` the process-IPC runner. */
export interface WorkerDispatchDeps {
  loadModule?: (entry: LoadableWorkerDispatch) => Promise<WorkerModule>;
  ipcWorker?: (start: (transport: IpcWorkerTransport) => void, options?: IpcWorkerRunOptions) => Promise<void>;
}

export const runWorkerDispatch = async (arg: string, deps: WorkerDispatchDeps = {}): Promise<boolean> => {
  const loadModule = deps.loadModule ?? ((entry: LoadableWorkerDispatch) => entry.load());
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
