/**
 * omp 宿主入口的 worker 选择器分发表（中文说明）。
 *
 * 内嵌的 @oh-my-pi/pi-coding-agent 会用 `process.execPath` 加一个
 * `__omp_worker_*` argv 选择器把自己的可执行文件重新拉起为各类 worker
 * （daemon broker、LSP mux、blob broker、ONNX 推理 worker、JS-eval 内核等，
 * 见 SDK subprocess/worker-client.ts 的 resolveWorkerSpawnCmd）。打包产物里
 * `process.execPath` 就是 omp-host(.exe) 本身，而 omp CLI 的分发逻辑
 * （cli.ts 的 runWorkerEntrypoint）在其中永远不会执行——本表出现之前，每次
 * worker 拉起都会静默启动一个随机端口的完整 HTTP 宿主且无人回收（「数百个
 * omp-host.exe 进程」泄漏：daemon-broker 连接失败每 ~10s 重试一次就多一个
 * 僵尸进程）。
 *
 * 本表为内嵌引擎可能以「子进程」方式拉起的每个选择器镜像 SDK CLI 的分发；
 * 只在真正 CLI 内才有意义的选择器（无 parentPort 即硬抛的 worker_threads
 * 条目、omp stats 同步 worker、browser-relay CLI 命令）一律显式 no-serve
 * 退出：无法兑现的调用必须快速失败，绝不变成僵尸宿主。升级 SDK 版本时
 * 须同步维护本表。
 */

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

/** 内嵌 SDK 的包说明符前缀（拼接各条目的 module 路径用）。 */
const SDK = '@oh-my-pi/pi-coding-agent';

/** worker argv 选择器的统一前缀（__omp_worker_）。 */
export const WORKER_SELECTOR_PREFIX = '__omp_worker_';

/** 类型守卫：判断 argv 值是否为 worker 选择器。 */
export const isWorkerSelector = (value: string | undefined): value is string =>
  value !== undefined && value.startsWith(WORKER_SELECTOR_PREFIX);

// `browser-relay` is not a worker selector but the browser relay daemon spawns
// this exact argv when relay mode is enabled; without this guard it would
// leak a host exactly like the broker selectors did.
/** 是否为需要本模块接管的调用：worker 选择器，或 browser-relay——后者虽非
 * 选择器，但 relay 守护进程在启用 relay 模式时会以完全相同的 argv 拉起本
 * 进程，不加守卫它会像 broker 选择器一样泄漏一个僵尸宿主。 */
export const isDispatchableInvocation = (value: string | undefined): boolean =>
  isWorkerSelector(value) || value === 'browser-relay';
/** 每个可分发模块按名提供的启动导出；分发在调用前用 typeof 校验其可调用。 */
/** Starter export every dispatchable module provides by name; dispatch validates callability with a typeof check before invoking (its parse step). */
export type WorkerStarter = (
  transport?: IpcWorkerTransport,
  interceptor?: RejectionInterceptor,
) => void | Promise<void>;

/** 选择器加载出的模块命名空间读取视图：按导出名解析 starter（命名空间里
 * 其余的辅助函数/类分发从不读取）。 */
/** Module namespace a dispatchable selector loads, as dispatch reads it: the starter resolved by export name (namespaces also export helpers/classes dispatch never reads). */
export type WorkerModule = Record<string, WorkerStarter | undefined>;

/** 字面量 import('...') 惰性加载 thunk（bun build --compile 会内嵌对应模块）。 */
/** Literal `import('...')` thunk (bun build --compile embeds the module). */
export type WorkerLoadThunk = () => Promise<WorkerModule>;

/** 仅类型层面的适配器：把单个内嵌模块命名空间收窄为上文的 starter 读取
 * 视图；运行时 thunk 原样返回、不做任何包装。 */
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

/** env-server 类选择器：从环境变量读取配置启动的独立服务。 */
/** env-server selector: standalone server started from the environment. */
export interface EnvServerDispatch {
  // 判别值：env-server。
  kind: 'env-server';
  // SDK 模块说明符（错误信息与诊断用）。
  module: string;
  // 模块内启动函数的导出名。
  starter: string;
  // 模块加载 thunk。
  load: WorkerLoadThunk;
}

/** ipc-worker 类选择器：把类型化 transport 接到进程 IPC 上运行的 worker。 */
/** ipc-worker selector: typed transport wired onto process IPC. */
export interface IpcWorkerDispatch {
  // 判别值：ipc-worker。
  kind: 'ipc-worker';
  // SDK 模块说明符（错误信息与诊断用）。
  module: string;
  // 模块内启动函数的导出名。
  starter: string;
  // 模块加载 thunk。
  load: WorkerLoadThunk;
  // 是否向 starter 传入 RejectionInterceptor（js-eval 内核需要）。
  interceptorArg?: boolean;
  // 连接仍存活时 send 抛错是否上抛（而非触发关闭）。
  rethrowConnectedSendErrors?: boolean;
}

/** self-runner 类选择器：starter 自行接管进程余下的生命周期。 */
/** self-runner selector: the starter owns the rest of the process. */
export interface SelfRunnerDispatch {
  // 判别值：self-runner。
  kind: 'self-runner';
  // SDK 模块说明符（错误信息与诊断用）。
  module: string;
  // 模块内启动函数的导出名。
  starter: string;
  // 模块加载 thunk。
  load: WorkerLoadThunk;
}

/** 显式 no-serve 退出：无法以子进程方式兑现的选择器。 */
/** Explicit no-serve exit for a selector we cannot honor as a subprocess. */
export interface UnsupportedDispatch {
  // 判别值：unsupported。
  kind: 'unsupported';
  // 拒绝原因（写入 stderr 供诊断）。
  reason: string;
}

/** 单个 argv 选择器的描述符（WORKER_DISPATCH 的值类型，按 kind 判别）。 */
/** Descriptor for one argv selector (WORKER_DISPATCH's value shape). */
export type WorkerDispatchEntry =
  | EnvServerDispatch
  | IpcWorkerDispatch
  | SelfRunnerDispatch
  | UnsupportedDispatch;

/** 模块可实际加载并启动的分发条目（从联合中排除 UnsupportedDispatch）。 */
/** A dispatch entry whose module we can actually load and start. */
export type LoadableWorkerDispatch = Exclude<WorkerDispatchEntry, UnsupportedDispatch>;

/**
 * 选择器 → 分发描述符总表。覆盖内嵌引擎会以子进程拉起的全部选择器。
 * 各条目的 load 必须保持字面量 import('...') thunk——计算出的说明符在
 * bun build --compile 产物中会退化为运行时查找，继而在打包二进制内失败
 * （其中没有 node_modules 可供解析）。升级 SDK 时须同步维护此表。
 */
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

/** 按选择器名查表；未知选择器返回 null（由调用方决定退出方式）。 */
export const resolveWorkerDispatch = (arg: string): WorkerDispatchEntry | null => {
  if (!Object.prototype.hasOwnProperty.call(WORKER_DISPATCH, arg)) return null;
  // SAFETY: the hasOwnProperty guard above proves `arg` names one of the
  // table's declared selector keys, so the index is always in bounds.
  return WORKER_DISPATCH[arg as keyof typeof WORKER_DISPATCH];
};

// The js-eval kernel requires a rejection interceptor (SDK `RejectionInterceptor`
/** js-eval 内核所需的 unhandledRejection 拦截器：装上内核回调并返回卸载
 * 函数；回调自身抛错被吞掉，绝不放倒 worker。 */
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
/** 经 worker 进程 IPC 通道传递的单个可 JSON 序列化帧。帧协议归各选择器的
 * starter 所有（SDK 的 tiny/stt/tts/mnemopi/js-eval worker 协议）；本
 * transport 只原样摆渡、从不窥探内容。 */
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

/** 交给 ipc-worker starter 的类型化 transport（SDK 子进程契约）。 */
/**
 * Typed transport handed to ipc-worker starters (SDK subprocess contract):
 * `send` fire-and-forget, `sendAndFlush` callback-flushed, `onMessage`
 * returns its uninstall.
 */
export interface IpcWorkerTransport {
  // 即发即弃发送；通道已断开时触发关闭。
  send: (message: IpcWorkerMessage) => void;
  // 发送并等待回调级冲刷完成。
  sendAndFlush: (message: IpcWorkerMessage) => Promise<void>;
  // 注册消息处理函数，返回其卸载函数。
  onMessage: (handler: (message: IpcWorkerMessage) => void) => () => void;
}

/** ipc-worker 运行开关（js-eval 进程两者都需要）。 */
/** ipc-worker run knobs (the js-eval process needs both). */
export interface IpcWorkerRunOptions {
  // 连接仍存活时 send 抛错是否原样上抛。
  rethrowConnectedSendErrors?: boolean;
}

/** cli.ts runIpcSubprocessWorker 的子进程侧移植：把类型化 transport 接到
 * 进程 IPC、以长间隔定时器保活、父进程断开即 resolve 退出流程，最后对
 * 自身 SIGKILL——避免 onnxruntime 等原生 finalizer 在本进程内执行。 */
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
 * 运行 arg 选中的 worker：查表 → 动态加载模块 → typeof 校验 starter 可
 * 调用 → 按条目类别调用（env-server/self-runner 直接调，ipc-worker 经
 * 进程 IPC 运行器）。返回 true 表示选择器已分发，worker 接管本进程余下
 * 生命周期（调用方绝不能再起 HTTP 服务）；false 表示选择器不支持或未知
 * （调用方须以非零码退出且同样不得 serve）。
 */
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
/** 测试/生产接缝：loadModule 替换逐条目的字面量 import thunk，ipcWorker
 * 替换进程 IPC 运行器。 */
/** Test/production seams: `loadModule` replaces the per-entry literal import
 * thunk and `ipcWorker` the process-IPC runner. */
export interface WorkerDispatchDeps {
  // 模块加载接缝（生产使用条目自带的 import thunk）。
  loadModule?: (entry: LoadableWorkerDispatch) => Promise<WorkerModule>;
  // IPC worker 运行器接缝（生产使用 runIpcSubprocessWorker）。
  ipcWorker?: (start: (transport: IpcWorkerTransport) => void, options?: IpcWorkerRunOptions) => Promise<void>;
}

/** 运行 arg 选中的 worker（本函数才是上面那段「Run the worker」文档所述
 * 行为的落地处）：分发成功返回 true（worker 接管进程，调用方不得再
 * serve）；选择器不支持或未知返回 false（调用方以非零码退出、同样不得
 * serve）。deps 为测试接缝，生产使用条目字面量 import thunk 与
 * runIpcSubprocessWorker。 */
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
