// Domain module: approval + ask dialog bridge (spec 03 §5.1/§5.2/§5.3/§5.4/§5.6,
// master D6 R10/R11/R13). Self-contained by design — the coordinator mounts
// `createDomainDialogs(...).mount(route)` from the shared route table and wires
// the engine integration points listed on `createDomainDialogs` below.
//
// Three pieces:
// - `UiLeaseTable` — per-session UI attachment leases (R13): authenticated
//   clients heartbeat; `hasUI` is holder-count ≥ 1 and nothing else. SSE
//   liveness never counts as presence.
// - `PendingDialogRegistry` — the single dialog authority (D-C3): register,
//   presented-ack (T_answer anchor), respond (atomic settle, 双端竞答 → 409),
//   abort, T_present / T_answer / orphan-window protections, snapshot, and
//   R11 settleAll for every lifecycle exit.
// - `createDialogBridge` — the web `ExtensionUIContext` (D-C1): select /
//   confirm / input / askDialog / notify + editor forward into the registry
//   and resolve from browser responds; terminal-only members are explicit
//   no-ops (RPC-mode degradation shape, rpc-mode.ts:824-925).
//
// Events flow exclusively through the 05-channel `OmpEventBus`
// (omp.dialog.requested / omp.dialog.settled, both durable, directory scope).
// Envelope carries directory/sessionID (events.js authority); payloads do not.
/**
 * 【模块说明】审批 + ask 对话框桥接域模块（spec 03 §5.1/§5.2/§5.3/§5.4/§5.6，
 * master D6 R10/R11/R13）。设计上自成一体 —— 协调器从共享路由表挂载
 * `createDomainDialogs(...).mount(route)`，并接线下文 `createDomainDialogs`
 * 处列出的引擎集成点。
 *
 * 三大组件：
 * - `UiLeaseTable` —— 逐会话 UI 附着租约（R13）：已认证客户端心跳续约；
 *   `hasUI` 即持有者数 ≥ 1，别无他义。SSE 存活不计入在线。
 * - `PendingDialogRegistry` —— 对话框唯一权威（D-C3）：注册、展示确认
 *   （T_answer 锚点）、应答（原子结算，双端竞答 → 409）、中止、
 *   T_present / T_answer / 孤儿窗口保护、快照，以及一切生命周期退出的
 *   R11 settleAll。
 * - `createDialogBridge` —— web 版 `ExtensionUIContext`（D-C1）：select /
 *   confirm / input / askDialog / notify + editor 转发进注册表，并由浏览器
 *   应答结算；终端专属成员为显式 no-op（RPC 模式降级形态，
 *   rpc-mode.ts:824-925）。
 *
 * 事件只经 05 通道 `OmpEventBus` 流动（omp.dialog.requested /
 * omp.dialog.settled，均持久化、目录作用域）。信封携带 directory/sessionID
 * （events.js 权威）；载荷不携带。
 */

import crypto from 'node:crypto';
import { normalizeDirectoryKey } from './registry.ts';
import { ompFeatures, featureUnavailable } from './omp-parity.ts';
import type { OmpEventBus } from './events.ts';
import type { ChromeBridgeHandlers } from './domain-chrome.ts';
import type {
  AutocompleteProviderFactory,
  ExtensionAskDialogQuestion,
  ExtensionAskDialogResult,
  ExtensionCustomOptions,
  ExtensionUiComponent,
  ExtensionUiComponentFactory,
  ExtensionUIContext,
  ExtensionUIDialogOptions,
  ExtensionUISelectItem,
  ExtensionWidgetContent,
  ExtensionWidgetOptions,
  TerminalInputHandler,
} from '@oh-my-pi/pi-coding-agent/extensibility/extensions';

// Engineering defaults (spec 03 §5.1 D-C1b / §5.4.3-5 / OQ-11). Product-level
// configurability is a master ruling away; these are the built-in values.
/** 租约持有者 TTL：30 秒（约 3 次错过心跳后过期）。 */
export const LEASE_TTL_MS = 30_000;
/** 建议心跳间隔：10 秒。 */
export const LEASE_HEARTBEAT_MS = 10_000;
/** 租约丢失后的孤儿窗口：120 秒。 */
export const ORPHAN_WINDOW_MS = 120_000;
/** T_present：对话框注册后未被展示确认的上限：300 秒。 */
export const PRESENT_TTL_MS = 300_000;

/** 对话框 id 前缀。 */
const DIALOG_ID_PREFIX = 'dlg_';
/** 结算墓碑容量上限（防 409 查询表无限增长）。 */
const TOMBSTONE_CAP = 1024;

/** 对话框创建事件名（05 通道，持久化）。 */
const REQUESTED_EVENT = 'omp.dialog.requested';
/** 对话框结算事件名（05 通道，持久化）。 */
const SETTLED_EVENT = 'omp.dialog.settled';

// ---------------------------------------------------------------------------
// Shared contracts (spec 03 §5.1/§5.2). Exported names are the module's
// public surface; engine.ts re-points its `dialogs` field to DialogsDomain.
// ---------------------------------------------------------------------------

/** Dialog kinds the registry and bridge understand (spec 03 §5.2). */
/** 注册表与桥接理解的对话框种类（spec 03 §5.2）。 */
export type DialogKind = 'approval' | 'select' | 'confirm' | 'input' | 'editor' | 'ask';

/** Session scope shared by lease and dialog operations. */
/** 租约与对话框操作共享的会话作用域。 */
export interface SessionScope {
  /** 工作目录（规范化键的原料）。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
}

/** Client-supplied lease identity (acquire/release input). */
/** 客户端提供的租约身份（acquire / release 的输入）。 */
export interface LeaseClientInput extends SessionScope {
  /** 客户端自生成 UUID（引用计数持有者键）。 */
  clientId: string;
}

/** (directory, sessionId, leaseId) identity every lease callback receives. */
/** 每个租约回调都收到的 (directory, sessionId, leaseId) 身份。 */
export interface LeaseInfo {
  /** 规范化后的目录。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
  /** 租约 id。 */
  leaseId: string;
}

/** Presence snapshot consumers read (engine hasUI + diagnostics, R13). */
/** 消费方读取的在线快照（engine hasUI + 诊断，R13）。 */
export interface LeaseSnapshot {
  /** 是否仍有存活持有者（≥ 1）。 */
  hasUI: boolean;
  /** 存活持有者数。 */
  holders: number;
  /** 租约 id（无租约为 null）。 */
  leaseId: string | null;
  /** 最晚持有者的到期时间（无则为 null）。 */
  expiresAt: number | null;
}

/** Internal per-session lease record. */
/** 内部逐会话租约记录。 */
interface LeaseRecord {
  /** 租约 id。 */
  leaseId: string;
  /** 规范化目录（记录内保存）。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
  /** clientId → 到期时间戳的引用计数表。 */
  holders: Map<string, number>;
}

/** Constructor options for UiLeaseTable (engineering defaults, spec 03 §5.1 D-C1b). */
/** UiLeaseTable 构造选项（工程默认，spec 03 §5.1 D-C1b）。 */
export interface UiLeaseTableOptions {
  /** 持有者 TTL（默认 30s）。 */
  ttlMs?: number;
  /** 建议心跳间隔（默认 10s）。 */
  heartbeatIntervalMs?: number;
  /** 可注入时钟（测试）。 */
  now?: () => number;
  /** 可注入调度缝隙。 */
  schedule?: (fn: () => void, delayMs: number) => DialogTimerHandle;
  /** 可注入取消缝隙。 */
  cancel?: (handle: DialogTimerHandle) => void;
  /** 0→1 持有者边沿触发（UI 附着）。 */
  onAttach?: ((info: LeaseInfo) => void) | null;
  /** 最后持有者离开触发（UI 脱离），每租约恰一次。 */
  onDetach?: ((info: LeaseInfo) => void) | null;
  /** Fires after EVERY successful acquire (attach or heartbeat renew) — the
   * engine's idle-TTL refresh signal (plan §4.2: lease heartbeats renew
   * `lastUsedAt`), unlike onAttach which fires only on the 0→1 holder edge. */
  /** 每次成功 acquire（附着或心跳续约）后触发 —— 引擎的空闲 TTL 刷新信号（plan §4.2：租约心跳续期 lastUsedAt）；与仅在 0→1 边沿触发的 onAttach 不同。 */
  onAcquired?: ((info: LeaseInfo) => void) | null;
}

/** Timer handle the schedule seam issues: the default setTimeout's
 * NodeJS.Timeout, or a test fake's numeric id — cancel accepts both. */
/** 调度缝隙返回的定时器句柄：默认 setTimeout 的 NodeJS.Timeout，或测试假时钟的数字 id —— cancel 两者皆可接受。 */
export type DialogTimerHandle = NodeJS.Timeout | number;

/** R11 transcript-diagnostic note for every non-responded settle. */
/** 每次非应答结算的 R11 转写诊断附注。 */
export interface DialogDiagnosticNote {
  /** 作用域目录。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
  /** 对话框 id。 */
  dialogId: string;
  /** 对话框种类。 */
  kind: DialogKind;
  /** 结算结局。 */
  outcome: string;
  /** 结算原因（可空）。 */
  reason: string | null;
}

/** Constructor options for PendingDialogRegistry. */
/** PendingDialogRegistry 构造选项。 */
export interface PendingDialogRegistryOptions {
  /** omp 事件通道（05 权威）。 */
  bus?: OmpEventBus | null;
  /** T_present（默认 300s）。 */
  presentTtlMs?: number;
  /** 租约丢失宽限（默认 120s）。 */
  orphanWindowMs?: number;
  /** 可注入时钟。 */
  now?: () => number;
  /** 可注入调度缝隙。 */
  schedule?: (fn: () => void, delayMs: number) => DialogTimerHandle;
  /** 可注入取消缝隙。 */
  cancel?: (handle: DialogTimerHandle) => void;
  /** R11 转写诊断钩子（每次非应答结算触发）。 */
  onDiagnostic?: ((note: DialogDiagnosticNote) => void) | null;
}

/** Respond kinds accepted per dialog kind (spec 03 §5.2 RespondResult). */
/** 每种对话框接受的应答种类联合（spec 03 §5.2 RespondResult）。 */
export type RespondResult =
  | { kind: 'cancel' }
  | { kind: 'select'; value?: string }
  | { kind: 'confirm'; value: boolean }
  | { kind: 'input'; value?: string }
  | { kind: 'editor'; value?: string }
  | { kind: 'ask'; results: RespondAskItem[] }
  | { kind: 'chat' };

/** Ask-answer item the registry accepts from a client respond (spec 03 §5.2);
 * the bridge rehydrates question/options/multi when resolving the SDK result. */
/** 客户端应答中注册表接受的 ask 答案项（spec 03 §5.2）；桥在解析 SDK 结果时回填 question/options/multi。 */
export interface RespondAskItem {
  /** 问题 id。 */
  id: string;
  /** 选中的选项标签。 */
  selectedOptions: string[];
  /** 自定义输入。 */
  customInput?: string;
  /** 附注。 */
  note?: string;
  /** 超时自动提交标记。 */
  timedOut?: boolean;
}

/** Per-kind payload carried on a dialog (spec 03 §5.2 OmpDialog). */
/** 对话框携带的逐种类载荷（spec 03 §5.2 OmpDialog）；同屏至多一种非空。 */
export interface DialogPayload {
  /** 审批载荷（提示、审批模式、工具名、调用 id、层级、原因）。 */
  approval?: {
    prompt: string;
    approvalMode?: string;
    toolName?: string;
    toolCallId?: string;
    tier?: string;
    reason?: string;
  };
  /** 单选载荷（标题 + 选项标签）。 */
  select?: { title: string; options: string[] };
  /** 确认载荷（标题 + 消息）。 */
  confirm?: { title: string; message: string };
  /** 输入框载荷（标题 + 占位符）。 */
  input?: { title: string; placeholder?: string };
  /** 编辑器载荷（标题 + 占位符）。 */
  editor?: { title: string; placeholder?: string };
  /** ask 载荷（问题列表 + 超时毫秒）。 */
  ask?: { questions: ExtensionAskDialogQuestion[]; timeoutMs: number };
}

/** Public OmpDialog projection (spec 03 §5.2); internals never leak. */
/** 公开 OmpDialog 投影（spec 03 §5.2）；内部记录绝不外泄。 */
export interface OmpDialog extends DialogPayload {
  /** 对话框 id（dlg_ 前缀）。 */
  id: string;
  /** 会话 id。 */
  sessionId: string;
  /** 注册时间。 */
  createdAt: number;
  /** 展示确认时间（未确认为空）。 */
  presentedAt?: number;
  /** 对话框种类。 */
  kind: DialogKind;
}

/** Authoritative snapshot answer (reconnect reconciliation, D2 bootstrap step 4). */
/** 权威快照应答（重连对账，D2 bootstrap 第 4 步）。 */
export interface DialogsSnapshot {
  /** 按创建时间升序的待处理对话框列表。 */
  dialogs: OmpDialog[];
}

/** omp.dialog.requested payload (05-channel, durable; spec 03 §5.2). */
/** omp.dialog.requested 载荷（05 通道，持久化；spec 03 §5.2）。 */
interface DialogRequestedEventPayload {
  /** 对话框公开投影。 */
  dialog: OmpDialog;
}

/** omp.dialog.settled payload (05-channel, durable; spec 03 §5.2). */
/** omp.dialog.settled 载荷（05 通道，持久化；spec 03 §5.2）。 */
interface DialogSettledEventPayload {
  /** 对话框 id。 */
  dialogId: string;
  /** 会话 id。 */
  sessionId: string;
  /** 结算结局。 */
  outcome: string;
}

/** Payload shapes the registry publishes (#emit): requested | settled. */
/** 注册表经 #emit 发布的载荷形态：requested | settled。 */
type DialogEventPayload = DialogRequestedEventPayload | DialogSettledEventPayload;

/** Value the register() promise resolves with for non-rejected settles. */
/** 非拒绝结算下 register() promise 的 resolve 值。 */
export interface DialogSettlement {
  /** 结算结局（responded / cancelled / timeout）。 */
  outcome: string;
  /** 应答结果（非应答结算为 null）。 */
  result: RespondResult | null;
}

/** Rejection shape for aborted/orphan/T_present settles (`.outcome`, `.dialogId`). */
/** aborted / orphan / T_present 结算的拒绝形态（携带 .outcome、.dialogId 附加字段）。 */
export interface DialogRejection extends Error {
  /** 结算结局。 */
  outcome?: string;
  /** 对话框 id。 */
  dialogId?: string;
}

/** Registration handle returned by PendingDialogRegistry.register(). */
/** PendingDialogRegistry.register() 返回的注册句柄。 */
export interface DialogRegistration {
  /** 对话框 id。 */
  id: string;
  /** 结算 promise（应答类 resolve，拒绝类 reject）。 */
  promise: Promise<DialogSettlement>;
}

/** Result envelope for respond/presented/abort calls (spec 03 §5.2). */
/** respond / presented / abort 调用的结果信封（spec 03 §5.2）。 */
export interface RegistryOutcome {
  /** 是否成功。 */
  ok: boolean;
  /** 失败时映射的 HTTP 状态码。 */
  status?: number;
  /** 客户端可见错误文本。 */
  error?: string;
  /** 结算结局（失败时回带先前结局）。 */
  outcome?: string;
  /** 展示确认时间。 */
  presentedAt?: number;
  /** 重复 presented 确认标记。 */
  duplicate?: boolean;
  /** 回显的客户端 id。 */
  clientId?: string;
}

/** Internal pending-dialog record (never leaves the registry). */
/** 内部待处理对话框记录（绝不离开注册表）。 */
interface DialogRecord {
  /** 对话框 id。 */
  id: string;
  /** 规范化目录。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
  /** 对话框种类。 */
  kind: DialogKind;
  /** 逐种类载荷。 */
  payload: DialogPayload;
  /** 注册时间。 */
  createdAt: number;
  /** 展示确认时间（null = 未确认）。 */
  presentedAt: number | null;
  /** 孤儿窗口中标记。 */
  orphan: boolean;
  /** 结算结果（null = 待处理）。 */
  settled: { outcome: string; result?: RespondResult } | null;
  /** T_present / T_answer / 孤儿窗口三只定时器句柄。 */
  timers: { present: DialogTimerHandle | null; answer: DialogTimerHandle | null; orphan: DialogTimerHandle | null };
  /** 结算 promise 的 resolve。 */
  resolve: (value: DialogSettlement) => void;
  /** 结算 promise 的 reject。 */
  reject: (error: DialogRejection) => void;
}

/** Bounded 409 tombstone for settled dialogs. */
/** 已结算对话框的有界 409 墓碑。 */
interface DialogTombstone {
  /** 结算结局。 */
  outcome: string;
  /** 作用域目录（跨目录探测用）。 */
  directory: string;
}

/** validateRespondResult verdict: the proven RespondResult or the client-facing error. */
/** validateRespondResult 的裁决：已证实的 RespondResult 或客户端错误。 */
type RespondValidation = { ok: false; error: string } | { ok: true; result: RespondResult };

/** Approval enrichment returned by the engine's approvalContext hook (§5.3.1). */
/** 引擎 approvalContext 钩子返回的审批增补信息（§5.3.1）。 */
export interface ApprovalEnrichment {
  /** 触发审批的工具名。 */
  toolName?: string;
  /** 工具调用 id。 */
  toolCallId?: string;
  /** 审批层级。 */
  tier?: string;
  /** 审批原因。 */
  reason?: string;
  /** 会话审批模式。 */
  approvalMode?: string;
}

/** Notification forwarded by DialogBridge.notify(). */
/** DialogBridge.notify() 转发的通知。 */
export interface DialogNotifyNote {
  /** 通知文本。 */
  message: string;
  /** 通知类型（'info' | 'warning' | 'error'）。 */
  type: string;
  /** 作用域目录。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
}

/** Optional extras layered onto the cached per-session bridge (chrome, notify, enrichment). */
/** 叠加到缓存逐会话桥上的可选附加项（chrome、notify、审批增补）。 */
export interface DialogBridgeExtras {
  /** 审批增补提供器（可空）。 */
  approvalContext?: (() => ApprovalEnrichment) | null;
  /** 通知转发钩子（可空）。 */
  onNotify?: ((note: DialogNotifyNote) => void) | null;
  /** 扩展 chrome 转发表（可空）。 */
  chrome?: ChromeBridgeHandlers | null;
}

/** SDK contract for the factory `setEditorComponent` accepts —
 * (tui, theme, keybindings) => CustomEditor — extracted from the SDK's own
 * ExtensionUIContext member; the web bridge drops it uninvoked. */
/** setEditorComponent 接受的工厂 SDK 契约 —— (tui, theme, keybindings) => CustomEditor，取自 SDK 自身 ExtensionUIContext 成员；web 桥不经调用直接丢弃。 */
type SdkEditorComponentFactory = NonNullable<Parameters<ExtensionUIContext['setEditorComponent']>[0]>;

/** Dropped `custom()` factory sink shape (SDK contract; the web bridge never
 * invokes it). Mirrors the SDK's custom() factory with its concrete terminal
 * singletons; the return union also admits void stubs since the bridge
 * resolves custom() with void regardless of what the factory returns. */
/** 被丢弃的 `custom()` 工厂接收端形态（SDK 契约；web 桥绝不调用）：镜像 SDK custom() 工厂及其终端单例；返回联合含 void 桩，因为桥无论工厂返回什么都以 void 结算。 */
export type DialogCustomUiFactory = <T>(
  tui: Parameters<SdkEditorComponentFactory>[0],
  theme: Parameters<SdkEditorComponentFactory>[1],
  keybindings: Parameters<SdkEditorComponentFactory>[2],
  done: (result: T) => void,
) => ExtensionUiComponent | Promise<ExtensionUiComponent> | void;

/** Options for createDialogBridge (the per-session WebUIContext, D-C1). */
/** createDialogBridge 的选项（逐会话 WebUIContext，D-C1）。 */
export interface DialogBridgeOptions {
  /** UI 租约表（注册门禁）。 */
  leases: UiLeaseTable;
  /** 待处理对话框注册表。 */
  registry: PendingDialogRegistry;
  /** 作用域目录。 */
  directory: string;
  /** 会话 id。 */
  sessionId: string;
  /** 审批增补提供器（可空）。 */
  approvalContext?: (() => ApprovalEnrichment) | null;
  /** 通知转发钩子（可空）。 */
  onNotify?: ((note: DialogNotifyNote) => void) | null;
  /** chrome 转发表（可空）。 */
  chrome?: ChromeBridgeHandlers | null;
}

/**
 * The web `ExtensionUIContext` surface (spec 03 §5.1 D-C1): interactive
 * members resolve through the registry; terminal-only members are explicit
 * no-ops (RPC-mode degradation shape, rpc-mode.ts:824-925).
 */
/**
 * web 版 `ExtensionUIContext` 表面（spec 03 §5.1 D-C1）：交互成员经
 * 注册表结算；终端专属成员为显式 no-op（RPC 模式降级形态，
 * rpc-mode.ts:824-925）。
 */
export interface DialogBridge {
  /** ask 超时是否自展示确认起算（web 桥恒 true）。 */
  timeoutStartsOnPresentation: boolean;
  /** 单选：注册 select / approval 对话框并返回选中标签（取消为 undefined）。 */
  select(title: string, options: ExtensionUISelectItem[], dialogOptions?: ExtensionUIDialogOptions): Promise<string | undefined>;
  /** 确认：注册 confirm 对话框并返回布尔（取消为 false）。 */
  confirm(title: string, message: string, dialogOptions?: ExtensionUIDialogOptions): Promise<boolean>;
  /** 输入框：注册 input 对话框并返回文本（取消为 undefined）。 */
  input(title: string, placeholder?: string, dialogOptions?: ExtensionUIDialogOptions): Promise<string | undefined>;
  /** 多问题 ask：注册 ask 对话框并返回 SDK 的 ExtensionAskDialogResult。 */
  askDialog(questions: ExtensionAskDialogQuestion[], dialogOptions?: ExtensionUIDialogOptions): Promise<ExtensionAskDialogResult | undefined>;
  /** 编辑器：注册 editor 对话框并返回文本（取消为 undefined）。 */
  editor(title: string, prefill?: string, dialogOptions?: ExtensionUIDialogOptions, editorOptions?: { promptStyle?: boolean }): Promise<string | undefined>;
  /** 通知：转发给 onNotify 钩子（类型 info/warning/error）。 */
  notify(message: string, type?: 'info' | 'warning' | 'error'): void;
  /** 终端输入处理器注册（web no-op，返回空退订函数）。 */
  onTerminalInput(handler: TerminalInputHandler): () => void;
  /** 状态栏键值更新（chrome 表转发）。 */
  setStatus(key: string, text: string | undefined): void;
  /** 工作消息（web 记为可观测丢弃）。 */
  setWorkingMessage(message?: string): void;
  /** 组件挂载（chrome 表转发）。 */
  setWidget(key: string, content: ExtensionWidgetContent, options?: ExtensionWidgetOptions): void;
  /** 页脚组件工厂（web 记为丢弃）。 */
  setFooter(factory: ExtensionUiComponentFactory | undefined): void;
  /** 页眉组件工厂（web 记为丢弃）。 */
  setHeader(factory: ExtensionUiComponentFactory | undefined): void;
  /** 终端标题（web 记为丢弃）。 */
  setTitle(title: string): void;
  /** 自定义终端 UI 工厂（web 记为丢弃，以 void 结算）。 */
  custom(factory: DialogCustomUiFactory, options?: ExtensionCustomOptions): Promise<void>;
  /** 编辑器文本写入（web 记为丢弃）。 */
  setEditorText(text: string): void;
  /** 粘贴进编辑器（web 记为丢弃）。 */
  pasteToEditor(text: string): void;
  /** 编辑器文本读取（web 恒空串）。 */
  getEditorText(): string;
  /** 自动补全提供器注册（web no-op）。 */
  addAutocompleteProvider(factory: AutocompleteProviderFactory): void;
  /** 编辑器组件工厂设置（web no-op，工厂不被调用）。 */
  setEditorComponent(factory: SdkEditorComponentFactory | undefined): void;
  /** 主题对象（web 恒空对象）。 */
  readonly theme: Record<string, never>;
  /** 枚举全部主题（web 恒空数组）。 */
  getAllThemes(): Promise<{ name: string; path: string | undefined }[]>;
  /** 按名取主题（web 恒 undefined）。 */
  getTheme(name: string): Promise<undefined>;
  /** 切换主题（web 恒失败并说明不管理主题）。 */
  setTheme(theme: string): Promise<{ success: boolean; error?: string }>;
  /** 工具区展开态（web 恒 false）。 */
  getToolsExpanded(): boolean;
  /** 设置工具区展开态（web no-op）。 */
  setToolsExpanded(expanded: boolean): void;
}

/** Injectable clock seam for createDomainDialogs (tests). */
/** createDomainDialogs 的可注入时钟缝隙（测试）。 */
export interface DomainDialogsClock {
  /** 当前时间函数。 */
  now?: () => number;
  /** 调度函数。 */
  schedule?: (fn: () => void, delayMs: number) => DialogTimerHandle;
  /** 取消函数。 */
  cancel?: (handle: DialogTimerHandle) => void;
}

/** Engineering-default overrides (spec 03 §5.1/§5.4.3-5, OQ-11). */
/** 工程默认覆盖项（spec 03 §5.1/§5.4.3-5，OQ-11）。 */
export interface DomainDialogsConfig {
  /** 租约持有者 TTL 覆盖。 */
  leaseTtlMs?: number;
  /** 建议心跳间隔覆盖。 */
  leaseHeartbeatMs?: number;
  /** T_present 覆盖。 */
  presentTtlMs?: number;
  /** 孤儿窗口覆盖。 */
  orphanWindowMs?: number;
}

/** Engine integration hooks for createDomainDialogs (spec 03 §5.0 item 9). */
/** createDomainDialogs 的引擎集成钩子（spec 03 §5.0 第 9 项）。 */
export interface DomainDialogsOptions {
  /** omp 事件通道（05 权威）。 */
  bus?: OmpEventBus | null;
  /** 引擎钩子：已物化会话执行 UI 组装（集成点 2）。 */
  onSessionUiAttached?: ((info: SessionScope) => void) | null;
  /** 引擎钩子：执行 UI 拆解（集成点 3）；孤儿窗口先启动。 */
  onSessionUiDetached?: ((info: SessionScope) => void) | null;
  /** Fires on every successful lease acquire (attach or heartbeat renew);
   * the engine refreshes the session's idle TTL from it (plan §4.2). */
  /** 每次成功租约获取（附着或心跳续约）触发；引擎据此刷新会话空闲 TTL（plan §4.2）。 */
  onLeaseAcquired?: ((info: SessionScope) => void) | null;
  /** R11 转写诊断钩子（每次非应答结算）。 */
  onDiagnostic?: ((note: DialogDiagnosticNote) => void) | null;
  /** 测试缝隙（now/schedule/cancel）。 */
  clock?: DomainDialogsClock | null;
  /** 工程默认覆盖。 */
  config?: DomainDialogsConfig;
}

/** The dialogs domain consumed by the engine + endpoints (spec 03 §5.0 item 9). */
/** 引擎 + 端点消费的对话框域（spec 03 §5.0 第 9 项）。 */
export interface DialogsDomain {
  /** UI 租约表（hasUI 权威）。 */
  leases: UiLeaseTable;
  /** 待处理对话框注册表。 */
  registry: PendingDialogRegistry;
  /** Per-session WebUIContext (cached for the session lifetime). */
  /** 逐会话 WebUIContext（会话生命周期内缓存）。 */
  uiContextFor(directory: string, sessionId: string, bridgeOptions?: DialogBridgeExtras): DialogBridge;
  /** Consumers contract for creation-time hasUI + diagnostics (R13). */
  /** 创建时 hasUI + 诊断的消费方契约（R13）。 */
  hasUISnapshotFor(directory: string, sessionId: string): LeaseSnapshot;
  /** Mount the endpoint group onto the shared route table. */
  /** 把端点组挂载到共享路由表。 */
  mount(route: DialogsRouteMount, options?: DialogEndpointOptions): DialogsDomain;
  /**
   * Per-session release (docs/plan.md §6): settle pending dialogs and drop
   * the cached bridge so session eviction cannot leak a per-session
   * WebUIContext. Shutdown keeps `dispose`.
   */
  /** 逐会话释放（docs/plan.md §6）：结算待处理对话框并丢弃缓存桥；关停走 dispose。 */
  releaseSession(directory: string, sessionId: string, reason?: string): void;
  /** Host lifecycle exit: settle everything, drop every lease (R11). */
  /** host 生命周期退出：全部结算、丢弃所有租约（R11）；返回结算计数。 */
  dispose(reason?: string): Promise<number>;
}

/** Approve-step outcome for alwaysAllowTransaction (409 = already settled). */
/** alwaysAllowTransaction 审批步的结果（409 = 已被另一客户端结算）。 */
export interface ApproveOutcome {
  /** 是否成功。 */
  ok?: boolean;
  /** HTTP 状态码。 */
  status?: number;
  /** 409 冲突标记。 */
  alreadySettled?: boolean;
}


/** Ask-cancel shaped rejection: ask.ts:982-984 converts `AbortError` into a
 *  ToolAbortError; approval wrappers rethrow the message verbatim. */
/** 构造 ask 取消形态的拒绝：把 Error 的 name 置为 'AbortError'，使 ask.ts:982-984 将其转换为 ToolAbortError；审批包装器按原文重抛消息。 */
const abortError = (reason: string): Error => {
  const error = new Error(reason);
  error.name = 'AbortError';
  return error;
};

/** 按对话框种类选择拒绝形态：ask 用 AbortError（SDK 侧可识别取消），其余用普通 Error。 */
const rejectFor = (kind: DialogKind, reason: string): Error =>
  kind === 'ask' ? abortError(reason) : new Error(reason);

/** 拼接规范化目录 + NUL 分隔 + 会话 id 作为 Map 键（NUL 防目录与会话串碰撞）。 */
const sessionKey = (directory: string, sessionId: string): string =>
  `${normalizeDirectoryKey(directory)}\u0000${sessionId}`;

/**
 * Per-session UI attachment leases — the single `hasUI` authority (R13).
 *
 * A lease exists per (directory, sessionId); individual UI page instances are
 * reference-counted holders keyed by their client-generated UUID `clientId`.
 * Acquire is acquire-or-renew: repeating the same triple extends the holder's
 * expiry and returns the same leaseId. Holder expiry (TTL, 3 missed
 * heartbeats) or explicit release removes exactly that holder; the lease ends
 * — and `onDetach` fires exactly once — when the last holder leaves.
 * Nothing else renews: SSE connection liveness is not presence.
 */
/**
 * 逐会话 UI 附着租约 —— 唯一的 `hasUI` 权威（R13）。
 *
 * 每个 (directory, sessionId) 一份租约；各 UI 页面实例是以其客户端自
 * 生成 UUID `clientId` 为键的引用计数持有者。acquire 是获取或续约：
 * 重复同一三元组会延长该持有者的到期时间并返回相同 leaseId。持有者
 * 过期（TTL，约 3 次错过心跳）或显式 release 只移除该持有者；最后一个
 * 持有者离开时租约结束且 `onDetach` 恰好触发一次。其它任何东西都不
 * 续约：SSE 连接存活不是在线。
 */
export class UiLeaseTable {
  /**
   * @param {object} [options]
   * @param {number} [options.ttlMs] Holder TTL (default 30s).
   * @param {number} [options.heartbeatIntervalMs] Advised heartbeat (default 10s).
   * @param {() => number} [options.now] Injectable clock (tests).
   * @param {(fn: () => void, delayMs: number) => DialogTimerHandle} [options.schedule]
   * @param {(handle: DialogTimerHandle) => void} [options.cancel]
   * @param {(info: { directory: string, sessionId: string, leaseId: string }) => void} [options.onAttach]
   * @param {(info: { directory: string, sessionId: string, leaseId: string }) => void} [options.onDetach]
   */
  /** 持有者 TTL（ms）。 */
  ttlMs: number;
  /** 建议心跳间隔（ms）。 */
  heartbeatIntervalMs: number;
  /** 可注入时钟。 */
  #now: () => number;
  /** 可注入调度器。 */
  #schedule: (fn: () => void, delayMs: number) => DialogTimerHandle;
  /** 可注入取消器。 */
  #cancel: (handle: DialogTimerHandle) => void;
  /** 0→1 持有者边沿回调（UI 附着）。 */
  #onAttach: ((info: LeaseInfo) => void) | null;
  /** 最后持有者离开回调（UI 脱离），每租约恰一次。 */
  #onDetach: ((info: LeaseInfo) => void) | null;
  /** 每次成功 acquire 回调（附着或心跳续约）。 */
  #onAcquired: ((info: LeaseInfo) => void) | null;
  /** sessionKey → 租约记录表。 */
  #leases: Map<string, LeaseRecord>;
  /** 过期清扫定时器句柄（无待清扫时为 null）。 */
  #sweepHandle: DialogTimerHandle | null;

  /** 注入缝隙与回调，构造空租约表。 */
  constructor({
    ttlMs = LEASE_TTL_MS,
    heartbeatIntervalMs = LEASE_HEARTBEAT_MS,
    now = Date.now,
    schedule = setTimeout,
    cancel = clearTimeout,
    onAttach = null,
    onDetach = null,
    onAcquired = null,
  }: UiLeaseTableOptions = {}) {
    this.ttlMs = ttlMs;
    this.heartbeatIntervalMs = heartbeatIntervalMs;
    this.#now = now;
    this.#schedule = schedule;
    this.#cancel = cancel;
    this.#onAttach = onAttach;
    this.#onDetach = onDetach;
    this.#onAcquired = onAcquired;
    /** @type {Map<string, { leaseId: string, directory: string, sessionId: string, holders: Map<string, number> }>} */
    this.#leases = new Map();
    this.#sweepHandle = null;
  }

  /** Acquire-or-renew one holder. Idempotent per (directory, sessionId, clientId). */
  /** 获取或续约一个持有者：按 (directory, sessionId, clientId) 幂等；返回 leaseId、到期时间、建议心跳间隔与是否本次发生 0→1 附着。 */
  acquire({ directory, sessionId, clientId }: LeaseClientInput) {
    const key = sessionKey(directory, sessionId);
    let lease = this.#leases.get(key);
    const attached = !lease || lease.holders.size === 0;
    if (!lease) {
      lease = {
        leaseId: crypto.randomUUID(),
        directory: normalizeDirectoryKey(directory),
        sessionId,
        holders: new Map(),
      };
      this.#leases.set(key, lease);
    }
    lease.holders.set(clientId, this.#now() + this.ttlMs);
    this.#armSweep();
    if (attached) this.#onAttach?.(leaseInfo(lease));
    this.#onAcquired?.(leaseInfo(lease));
    return {
      leaseId: lease.leaseId,
      expiresAt: lease.holders.get(clientId),
      heartbeatIntervalMs: this.heartbeatIntervalMs,
      attached,
    };
  }

  /** Explicit release (page unload / leaving the session view). Idempotent. */
  /** 显式释放（页面卸载/离开会话视图）。幂等；返回是否释放、是否触发脱离及 leaseId。 */
  release({ directory, sessionId, clientId }: LeaseClientInput) {
    const lease = this.#leases.get(sessionKey(directory, sessionId));
    if (!lease) return { released: false, detached: false };
    const released = lease.holders.delete(clientId);
    const detached = released && lease.holders.size === 0;
    if (detached) this.#endLease(lease);
    else this.#armSweep();
    return { released, detached, ...(lease.leaseId ? { leaseId: lease.leaseId } : {}) };
  }

  /** True while at least one live holder remains (lazy expiry sweep first). */
  /** 至少一个存活持有者时为 true（先经 snapshot 做惰性过期清扫）。 */
  has(directory: string, sessionId: string): boolean {
    return this.snapshot(directory, sessionId).hasUI;
  }

  /**
   * Presence snapshot for consumers (engine materialize reads `hasUI` from
   * here; diagnostics read holder counts).
   */
  /** 供消费方的在线快照：engine materialize 读 hasUI，诊断读持有者数；顺带惰性清理过期持有者。 */
  snapshot(directory: string, sessionId: string): LeaseSnapshot {
    const lease = this.#leases.get(sessionKey(directory, sessionId));
    if (!lease) return { hasUI: false, holders: 0, leaseId: null, expiresAt: null };
    const t = this.#now();
    for (const [clientId, expiresAt] of lease.holders) {
      if (expiresAt <= t) lease.holders.delete(clientId);
    }
    if (lease.holders.size === 0) {
      this.#endLease(lease);
      return { hasUI: false, holders: 0, leaseId: null, expiresAt: null };
    }
    let expiresAt = -Infinity;
    for (const value of lease.holders.values()) expiresAt = Math.max(expiresAt, value);
    return { hasUI: true, holders: lease.holders.size, leaseId: lease.leaseId, expiresAt };
  }

  /** Drop every lease (host shutdown). Detach fires once per live lease. */
  /** 丢弃全部租约（host 关停）。每个存活租约各触发一次 detach；返回脱离信息列表。 */
  releaseAll(): LeaseInfo[] {
    const detached = [];
    for (const lease of [...this.#leases.values()]) {
      if (lease.holders.size > 0) detached.push(leaseInfo(lease));
      this.#endLease(lease);
    }
    return detached;
  }

  /** 结束一份租约：移出表、重挂清扫、触发 onDetach（恰一次）。 */
  #endLease(lease: LeaseRecord) {
    this.#leases.delete(sessionKey(lease.directory, lease.sessionId));
    this.#armSweep();
    this.#onDetach?.(leaseInfo(lease));
  }

  /** 重挂过期清扫定时器：对齐全部持有者的最近到期时刻，触发时经 snapshot 惰性过期并再次重挂。 */
  #armSweep() {
    if (this.#sweepHandle !== null) {
      this.#cancel(this.#sweepHandle);
      this.#sweepHandle = null;
    }
    let nextExpiry = Infinity;
    for (const lease of this.#leases.values()) {
      for (const expiresAt of lease.holders.values()) nextExpiry = Math.min(nextExpiry, expiresAt);
    }
    if (!Number.isFinite(nextExpiry)) return;
    this.#sweepHandle = this.#schedule(() => {
      this.#sweepHandle = null;
      // Force lazy expiry through the snapshot path (fires detach transitions).
      for (const lease of [...this.#leases.values()]) this.snapshot(lease.directory, lease.sessionId);
      // #endLease re-arms for ended leases; re-arm for the rest too so a
      // remaining holder always has a pending sweep.
      this.#armSweep();
    }, Math.max(0, nextExpiry - this.#now()));
  }
}

/** 把内部租约记录投影为回调身份（directory, sessionId, leaseId）。 */
const leaseInfo = (lease: LeaseRecord): LeaseInfo => ({
  directory: lease.directory,
  sessionId: lease.sessionId,
  leaseId: lease.leaseId,
});

// Respond kinds accepted per dialog kind (spec 03 §5.2 RespondResult).
/** 每种对话框接受的应答 kind 白名单（spec 03 §5.2 RespondResult）。 */
const RESPOND_KINDS = {
  approval: ['select', 'cancel'],
  select: ['select', 'cancel'],
  confirm: ['confirm', 'cancel'],
  input: ['input', 'cancel'],
  editor: ['input', 'editor', 'cancel'],
  ask: ['ask', 'chat', 'cancel'],
} satisfies Record<DialogKind, readonly string[]>;

/** Client respond input (POST /omp/dialogs/{id}/respond body fields); `result`
 * stays unvalidated until the registry's contract check below parses it. */
/** 客户端应答输入（POST /omp/dialogs/{id}/respond 的 body 字段）；`result` 在注册表契约检查解析前保持未验证。 */
export interface RespondRequestInput {
  /** 作用域目录（校验用）。 */
  directory?: string | null;
  /** 客户端 id（应答回显）。 */
  clientId?: string | null;
  /** 未验证的应答对象。 */
  result?: unknown;
}

/** Post-guard view of an unvalidated client respond object: every field is
 * narrowed by an explicit typeof / Array.isArray check before it is read. */
/** 未验证客户端应答对象的过守卫视图：每个字段都经显式 typeof / Array.isArray 检查收窄后才被读取。 */
interface UnvalidatedRespondBody {
  /** 未验证的 kind 字段。 */
  kind?: unknown;
  /** 未验证的 value 字段。 */
  value?: unknown;
  /** 未验证的 results 数组。 */
  results?: unknown;
  /** 未验证的问题 id。 */
  id?: unknown;
  /** 未验证的选中选项数组。 */
  selectedOptions?: unknown;
}

/**
 * Validate a RespondResult against the dialog's contract. Returns
 * { ok: true } or { ok: false, error }.
 */
/**
 * 依据对话框契约校验一个 RespondResult。返回 { ok: true, 已验证结果 }
 * 或 { ok: false, error 客户端错误文本 }。
 */
const validateRespondResult = (record: DialogRecord, request: RespondRequestInput): RespondValidation => {
  const raw = request.result;
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return { ok: false, error: 'result must be an object with a string kind' };
  }
  // SAFETY: the checks above proved `raw` is a plain non-array object; every
  // field below is read only through an explicit typeof/Array.isArray check.
  const result = raw as UnvalidatedRespondBody;
  if (typeof result.kind !== 'string') {
    return { ok: false, error: 'result must be an object with a string kind' };
  }
  const allowed = RESPOND_KINDS[record.kind] ?? [];
  if (!allowed.includes(result.kind)) {
    return { ok: false, error: `result kind "${result.kind}" is not valid for a "${record.kind}" dialog` };
  }
  if (result.kind === 'select') {
    const options: readonly unknown[] = record.kind === 'approval'
      ? ['Approve', 'Deny']
      : (record.payload?.select?.options ?? []);
    if (result.value !== undefined && !options.includes(result.value)) {
      return { ok: false, error: 'result.value must be one of the dialog options' };
    }
  } else if (result.kind === 'confirm') {
    if (typeof result.value !== 'boolean') {
      return { ok: false, error: 'result.value must be a boolean for a confirm dialog' };
    }
  } else if (result.kind === 'input' || result.kind === 'editor') {
    if (result.value !== undefined && typeof result.value !== 'string') {
      return { ok: false, error: 'result.value must be a string when present' };
    }
  } else if (result.kind === 'ask') {
    if (!Array.isArray(result.results)) {
      return { ok: false, error: 'result.results must be an array for an ask dialog' };
    }
    const questions = record.payload?.ask?.questions ?? [];
    // The SDK rejects a mismatched count after the fact (ask.ts:931-933);
    // rejecting here keeps the dialog answerable instead of burning it.
    if (result.results.length !== questions.length) {
      return { ok: false, error: 'result.results must answer every question' };
    }
    const questionById = new Map(questions.map((q) => [q.id, q]));
    for (const item of result.results) {
      if (typeof item !== 'object' || item === null || Array.isArray(item)) {
        return { ok: false, error: 'result.results contains an unknown question id' };
      }
      // SAFETY: plain non-array object per the check above.
      const answer = item as UnvalidatedRespondBody;
      if (typeof answer.id !== 'string' || !questionById.has(answer.id)) {
        return { ok: false, error: 'result.results contains an unknown question id' };
      }
      if (!Array.isArray(answer.selectedOptions)) {
        return { ok: false, error: 'result.results items must carry a selectedOptions array' };
      }
      const question = questionById.get(answer.id);
      if (!question) {
        return { ok: false, error: 'result.results items must reference declared questions' };
      }
      const labels = (question.options ?? []).map((o) => o?.label ?? o);
      for (const label of answer.selectedOptions) {
        if (!labels.includes(label)) {
          return { ok: false, error: 'selectedOptions contains a label outside the question options' };
        }
      }
    }
  }
  // SAFETY: the branches above proved `kind` is allowed for record.kind and
  // validated that kind's value/results shape, so the object now satisfies
  // the RespondResult union the registry settles with.
  return { ok: true, result: result as RespondResult };
};

/**
 * The pending-dialog authority (D-C3). Registration happens only from the
 * WebUIContext bridge (which itself requires a live lease), so every dialog
 * here was created while a client could answer it. Timeouts:
 * - T_present (from registration): the dialog was never presented — protects
 *   against invisible pendings, it is not a product timeout.
 * - T_answer (from presented-ack): ask dialogs with timeoutMs > 0 auto-submit
 *   the recommended (else first) option, mirroring ask.ts:176-182.
 * - Orphan window (from lease detach): bounded wait for a reconnecting
 *   client; expiry settles approval dialogs with an honest reject, ask
 *   dialogs through the abort path (spec 03 §5.6.1).
 *
 */
/**
 * 待处理对话框权威（D-C3）。只有 WebUIContext 桥会注册（而桥本身要求
 * 存活租约），故这里的每个对话框都创建于有客户端可应答之时。超时：
 * - T_present（自注册起）：对话框从未被展示 —— 防不可见挂起，非产品超时。
 * - T_answer（自展示确认起）：timeoutMs > 0 的 ask 对话框自动提交推荐
 *   （否则首个）选项，对齐 ask.ts:176-182。
 * - 孤儿窗口（自租约脱离起）：等待重连客户端的有界宽限；到期对审批
 *   对话框以诚实的拒绝结算，ask 对话框走中止路径（spec 03 §5.6.1）。
 */
export class PendingDialogRegistry {
  /**
   * @param {object} [options]
   * @param {import('./events.ts').OmpEventBus} [options.bus] omp event channel (05 authority).
   * @param {number} [options.presentTtlMs] T_present (default 300s).
   * @param {number} [options.orphanWindowMs] Lease-loss grace (default 120s).
   * @param {() => number} [options.now] Injectable clock.
   * @param {(fn: () => void, delayMs: number) => DialogTimerHandle} [options.schedule]
   * @param {(handle: DialogTimerHandle) => void} [options.cancel]
   * @param {(note: { directory: string, sessionId: string, dialogId: string, kind: string, outcome: string, reason: string | null }) => void} [options.onDiagnostic]
   *        R11 transcript-diagnostic hook: invoked for every non-responded
   *        settle so the engine can append a transcript note.
   */
  /** T_present 上限（默认 300 秒）。 */
  presentTtlMs: number;
  /** 孤儿窗口时长（默认 120 秒）。 */
  orphanWindowMs: number;
  #bus: OmpEventBus | null;
  #now: () => number;
  #schedule: (fn: () => void, delayMs: number) => DialogTimerHandle;
  #cancel: (handle: DialogTimerHandle) => void;
  #onDiagnostic: ((note: DialogDiagnosticNote) => void) | null;
  #dialogs: Map<string, DialogRecord>;
  #tombstones: Map<string, DialogTombstone>;

  /** 注入事件通道、超时参数、时钟/调度缝隙与 R11 诊断钩子，构造空注册表。 */
  constructor({
    bus = null,
    presentTtlMs = PRESENT_TTL_MS,
    orphanWindowMs = ORPHAN_WINDOW_MS,
    now = Date.now,
    schedule = setTimeout,
    cancel = clearTimeout,
    onDiagnostic = null,
  }: PendingDialogRegistryOptions = {}) {
    this.#bus = bus;
    this.presentTtlMs = presentTtlMs;
    this.orphanWindowMs = orphanWindowMs;
    this.#now = now;
    this.#schedule = schedule;
    this.#cancel = cancel;
    this.#onDiagnostic = onDiagnostic;
    /** @type {Map<string, object>} */
    this.#dialogs = new Map();
    /** @type {Map<string, { outcome: string, directory: string }>} bounded 409 tombstones */
    this.#tombstones = new Map();
  }

  /** Abort one dialog if still pending (signal path); no-op otherwise. */
  /** 信号路径：仍待处理则以 aborted 中止；否则 no-op。返回是否执行了中止。 */
  abortIfPendingSignal(dialogId: string, reason: string = 'dialog aborted by signal'): boolean {
    const record = this.#dialogs.get(dialogId);
    if (!record) return false;
    return this.#settle(record, 'aborted', { reason });
  }

  /**
   * Register a dialog. Emits the requested event and starts T_present.
   * @returns {{ id: string, promise: Promise<{ outcome: string, result: object | null }> }}
   *          The promise resolves for responded/cancelled outcomes and for
   *          the ask auto-submit timeout; it rejects (Error, `.outcome`) for
   *          aborted / orphan / T_present settles.
   */
  /**
   * 注册一个对话框：生成 dlg_ 前缀 id、发出 requested 事件并启动
   * T_present。返回句柄 —— promise 对 responded/cancelled 结局及 ask 自动
   * 提交超时 resolve；对 aborted / orphan / T_present 结局 reject
   * （携带 .outcome、.dialogId）。
   */
  register({ directory, sessionId, kind, payload }: {
    directory: string;
    sessionId: string;
    kind: DialogKind;
    payload?: DialogPayload | null;
  }): DialogRegistration {
    const id = DIALOG_ID_PREFIX + crypto.randomBytes(16).toString('base64url');
    // SAFETY: the executor runs synchronously, so both bindings are set
    // before the constructor returns.
    let resolve!: (value: DialogSettlement) => void;
    let reject!: (error: DialogRejection) => void;
    const promise = new Promise<DialogSettlement>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    const record: DialogRecord = {
      id,
      directory: normalizeDirectoryKey(directory),
      sessionId,
      kind,
      payload: payload ?? {},
      createdAt: this.#now(),
      presentedAt: null,
      orphan: false,
      settled: null,
      timers: { present: null, answer: null, orphan: null },
      resolve,
      reject,
    };
    this.#dialogs.set(id, record);
    record.timers.present = this.#schedule(() => this.#expirePresent(record), this.presentTtlMs);
    this.#emit(REQUESTED_EVENT, { dialog: snapshotDialog(record) }, record);
    return { id, promise };
  }

  /** Presented-ack: cancels T_present, anchors T_answer (idempotent). */
  /** 展示确认：取消 T_present、锚定 T_answer（幂等；重复确认回带 duplicate 与原时间）。 */
  presented(dialogId: string, { directory }: { directory?: string | null } = {}): RegistryOutcome {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope || !this.#scopeOk(dialogId, directory)) return scope ?? { ok: false, status: 404, error: 'dialog not found' };
    // SAFETY: scopeOk proved the record exists for this scope.
    const record = this.#dialogs.get(dialogId) as DialogRecord;
    if (record.presentedAt !== null) {
      return { ok: true, presentedAt: record.presentedAt, duplicate: true };
    }
    record.presentedAt = this.#now();
    if (record.timers.present !== null) {
      this.#cancel(record.timers.present);
      record.timers.present = null;
    }
    const timeoutMs = record.payload?.ask?.timeoutMs ?? 0;
    if (record.kind === 'ask' && timeoutMs > 0) {
      record.timers.answer = this.#schedule(() => this.#expireAnswer(record), timeoutMs);
    }
    return { ok: true, presentedAt: record.presentedAt };
  }

  /**
   * Atomic respond. Registry binding (directory, sessionId, dialogId) is
   * authoritative; the client directory is validation-only.
   */
  /** 原子应答：先作用域预检，再按契约校验 result（失败 400），后原子结算（已结算 409 回带先前 outcome）。 */
  respond(
    dialogId: string,
    { directory, clientId = null, ...request }: RespondRequestInput = {},
  ): RegistryOutcome {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope || !this.#scopeOk(dialogId, directory)) return scope ?? { ok: false, status: 404, error: 'dialog not found' };
    // SAFETY: scopeOk proved the record exists for this scope.
    const record = this.#dialogs.get(dialogId) as DialogRecord;
    const validation = validateRespondResult(record, request);
    if (validation.ok === false) return { ok: false, status: 400, error: validation.error };
    const outcome = validation.result.kind === 'cancel' ? 'cancelled' : 'responded';
    if (!this.#settle(record, outcome, { result: validation.result })) {
      // SAFETY: #settle returns false only when the record is already
      // settled, so the outcome read is present here.
      return { ok: false, status: 409, error: 'dialog already settled', outcome: (record.settled as { outcome: string }).outcome };
    }
    return { ok: true, outcome, ...(clientId ? { clientId } : {}) };
  }

  /** User/system abort of one dialog (Stop button on a single card). */
  /** 单个对话框的用户/系统中止（单卡 Stop 按钮）；已结算返回 409。 */
  abort(dialogId: string, { directory }: { directory?: string | null } = {}): RegistryOutcome {
    const scope = this.#scopeCheck(dialogId, directory);
    if (scope || !this.#scopeOk(dialogId, directory)) return scope ?? { ok: false, status: 404, error: 'dialog not found' };
    // SAFETY: scopeOk proved the record exists for this scope.
    const record = this.#dialogs.get(dialogId) as DialogRecord;
    if (!this.#settle(record, 'aborted', { reason: 'dialog aborted' })) {
      // SAFETY: #settle returns false only when the record is already
      // settled, so the outcome read is present here.
      return { ok: false, status: 409, error: 'dialog already settled', outcome: (record.settled as { outcome: string }).outcome };
    }
    return { ok: true, outcome: 'aborted' };
  }

  /** Abort every pending dialog of one session (user Stop, session dispose). */
  /** 中止某会话的全部待处理对话框（用户 Stop、会话 dispose）；返回结算计数。 */
  abortForSession({ directory, sessionId }: SessionScope, reason: string = 'dialog aborted') {
    let count = 0;
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (this.#settle(record, 'aborted', { reason })) count += 1;
    }
    return count;
  }

  /**
   * Lease lost: every pending dialog of the session enters the orphan window
   * (default 120s). A lease re-attach inside the window cancels it and the
   * dialogs resume waiting (spec 03 §5.6.1/§5.6.2-0).
   */
  /** 租约丢失：该会话每个待处理对话框进入孤儿窗口（默认 120 秒）；窗口内租约重新附着会取消它并恢复等待。 */
  enterOrphanWindow({ directory, sessionId }: SessionScope) {
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (record.orphan) continue;
      record.orphan = true;
      record.timers.orphan = this.#schedule(() => this.#expireOrphan(record), this.orphanWindowMs);
    }
  }

  /** Lease re-attached: cancel orphan timers, dialogs resume waiting. */
  /** 租约重新附着：取消孤儿定时器，对话框恢复等待。 */
  recoverOrphanWindow({ directory, sessionId }: SessionScope) {
    for (const record of this.#pendingOf(directory, sessionId)) {
      if (!record.orphan) continue;
      record.orphan = false;
      if (record.timers.orphan !== null) {
        this.#cancel(record.timers.orphan);
        record.timers.orphan = null;
      }
    }
  }

  /**
   * R11 lifecycle exit: atomically settle every pending dialog as aborted.
   * Resolvers reject with the exit name (`omp-host shutdown`, …) so the SDK
   * finishes the turn with a diagnostic error; settled events are emitted for
   * all. Returns the settled count.
   */
  /** R11 生命周期退出：把每个待处理对话框原子结算为 aborted（resolver 以退出名 reject）；全部发出 settled 事件。返回结算计数。 */
  settleAll(reason: string = 'omp-host shutdown') {
    let count = 0;
    for (const record of [...this.#dialogs.values()]) {
      if (this.#settle(record, 'aborted', { reason })) count += 1;
    }
    return count;
  }

  /** Authoritative snapshot (reconnect reconciliation, D2 bootstrap step 4). */
  /** 权威快照（重连对账，D2 bootstrap 第 4 步）；按创建时间升序、可按目录/会话过滤。 */
  snapshot({ directory, sessionId = null }: { directory?: string | null; sessionId?: string | null } = {}): DialogsSnapshot {
    const wanted = directory === undefined || directory === null ? null : normalizeDirectoryKey(directory);
    const dialogs = [];
    for (const record of this.#dialogs.values()) {
      if (wanted !== null && record.directory !== wanted) continue;
      if (sessionId !== null && record.sessionId !== sessionId) continue;
      dialogs.push(snapshotDialog(record));
    }
    dialogs.sort((a, b) => a.createdAt - b.createdAt || (a.id < b.id ? -1 : 1));
    return { dialogs };
  }

  /** Pending count for a session (idle sweeper guard, spec 03 §5.6.5a-3). */
  /** 某会话的待处理计数（空闲清扫守卫，spec 03 §5.6.5a-3）。 */
  pendingCount({ directory, sessionId }: SessionScope) {
    return this.#pendingOf(directory, sessionId).length;
  }

  /** 收集某作用域的全部待处理记录（线性扫描）。 */
  #pendingOf(directory: string, sessionId: string): DialogRecord[] {
    const wanted = normalizeDirectoryKey(directory);
    const out = [];
    for (const record of this.#dialogs.values()) {
      if (record.directory === wanted && record.sessionId === sessionId) out.push(record);
    }
    return out;
  }

  // 404 / 409 / 403 pre-check shared by respond / presented / abort.
  /** True when scopeCheck passed — i.e. the dialog record exists and matches. */
  /** scopeCheck 通过 —— 即对话框记录存在且目录作用域匹配。 */
  #scopeOk(dialogId: string, directory: string | null | undefined): boolean {
    return this.#scopeCheck(dialogId, directory) === null && this.#dialogs.has(dialogId);
  }

  /** respond/presented/abort 共享的预检：空 id 404、缺 directory 400、目录不匹配 403、命中墓碑 409、不存在 404。 */
  #scopeCheck(dialogId: string, directory: string | null | undefined) {
    if (typeof dialogId !== 'string' || dialogId === '') {
      return { ok: false, status: 404, error: 'dialog not found' };
    }
    if (directory === undefined || directory === null) {
      return { ok: false, status: 400, error: 'directory is required' };
    }
    const existing = this.#dialogs.get(dialogId);
    if (existing) {
      if (normalizeDirectoryKey(directory) !== existing.directory) {
        return { ok: false, status: 403, error: 'directory does not match dialog scope' };
      }
      return null;
    }
    const tombstone = this.#tombstones.get(dialogId);
    if (tombstone) {
      if (normalizeDirectoryKey(directory) !== tombstone.directory) {
        return { ok: false, status: 403, error: 'directory does not match dialog scope' };
      }
      return { ok: false, status: 409, error: 'dialog already settled', outcome: tombstone.outcome };
    }
    return { ok: false, status: 404, error: 'dialog not found' };
  }

  /** 原子结算：写结果、取消全部定时器、移出待表并入有界墓碑、按结局 resolve/reject、发 settled 事件、按需上报诊断；已结算返回 false。 */
  #settle(record: DialogRecord, outcome: string, { result = null, reason = null }: { result?: RespondResult | null; reason?: string | null } = {}) {
    if (record.settled) return false;
    record.settled = { outcome, ...(result ? { result } : {}) };
    // SAFETY: timer keys are the three declared DialogTimerHandle slots.
    for (const key of Object.keys(record.timers) as Array<keyof typeof record.timers>) {
      if (record.timers[key] !== null) {
        this.#cancel(record.timers[key]);
        record.timers[key] = null;
      }
    }
    this.#dialogs.delete(record.id);
    this.#tombstones.set(record.id, { outcome, directory: record.directory });
    if (this.#tombstones.size > TOMBSTONE_CAP) {
      const oldest = this.#tombstones.keys().next().value;
      if (oldest !== undefined) this.#tombstones.delete(oldest);
    }
    if (outcome === 'responded' || outcome === 'cancelled' || (outcome === 'timeout' && result)) {
      record.resolve({ outcome, result });
    } else {
      const error: DialogRejection = reason === null ? new Error(`dialog ${outcome}`) : rejectFor(record.kind, reason);
      error.outcome = outcome;
      error.dialogId = record.id;
      record.reject(error);
    }
    this.#emit(
      SETTLED_EVENT,
      { dialogId: record.id, sessionId: record.sessionId, outcome },
      record,
    );
    if (outcome !== 'responded') {
      this.#onDiagnostic?.({
        directory: record.directory,
        sessionId: record.sessionId,
        dialogId: record.id,
        kind: record.kind,
        outcome,
        reason: reason ?? (outcome === 'timeout' && result ? 'answer window timed out' : null),
      });
    }
    return true;
  }

  /** T_present 到期：以 timeout 结算；审批以原因拒绝，ask 走中止路径（区分在结算的 rejectFor）。 */
  #expirePresent(record: DialogRecord) {
    if (record.settled) return;
    record.timers.present = null;
    // Approval rejects with the reason; ask takes the abort path (the kind
    // distinction lives in #settle's rejectFor).
    this.#settle(record, 'timeout', { reason: 'dialog expired before presentation' });
  }

  /** T_answer 到期：为每个问题自动提交推荐（否则首个）选项并标记 timedOut，以 timeout + ask 结果结算。 */
  #expireAnswer(record: DialogRecord) {
    if (record.settled) return;
    record.timers.answer = null;
    const questions = record.payload?.ask?.questions ?? [];
    const results = questions.map((question) => {
      const options = Array.isArray(question.options) ? question.options : [];
      const recommended =
        typeof question.recommended === 'number' &&
        question.recommended >= 0 &&
        question.recommended < options.length
          ? question.recommended
          : null;
      const label = recommended !== null ? options[recommended].label : options[0]?.label;
      return {
        id: question.id,
        selectedOptions: label === undefined ? [] : [label],
        timedOut: true,
      };
    });
    this.#settle(record, 'timeout', { result: { kind: 'ask', results } });
  }

  /** 孤儿窗口到期：以 timeout 与"无 UI 租约"原因结算。 */
  #expireOrphan(record: DialogRecord) {
    if (record.settled) return;
    record.timers.orphan = null;
    this.#settle(record, 'timeout', { reason: 'dialog orphaned (no UI lease)' });
  }

  /** 经事件总线发布对话框事件（durable；目录/会话作用域在信封上）。 */
  #emit(type: string, payload: DialogEventPayload, record: DialogRecord) {
    this.#bus?.publish(type, payload, {
      directory: record.directory,
      sessionID: record.sessionId,
      durable: true,
    });
  }
}

/** Public OmpDialog projection (spec 03 §5.2); internals never leak. */
/** 把内部记录投影为公开 OmpDialog；内部字段（timers、resolver 等）绝不外泄。 */
const snapshotDialog = (record: DialogRecord): OmpDialog => ({
  id: record.id,
  sessionId: record.sessionId,
  createdAt: record.createdAt,
  ...(record.presentedAt !== null ? { presentedAt: record.presentedAt } : {}),
  kind: record.kind,
  ...record.payload,
});

/**
 * The web `ExtensionUIContext` (spec 03 §5.1 D-C1). One instance per
 * (directory, sessionId), owned by the engine session lifecycle. Dialogs are
 * only registered while a live lease exists — without one the call fails
 * closed with the SDK's own wording instead of creating a dialog nobody
 * could answer (R13). Terminal-only members are explicit no-ops, mirroring
 * the RPC degradation (rpc-mode.ts:824-925).
 *
 * @param {object} options
 * @param {UiLeaseTable} options.leases
 * @param {PendingDialogRegistry} options.registry
 * @param {string} options.directory
 * @param {string} options.sessionId
 * @param {() => { toolName?: string, toolCallId?: string, tier?: string, reason?: string, approvalMode?: string }} [options.approvalContext]
 *        Best-effort approval enrichment (engine correlates the newest
 *        pending tool_execution_start; absence is fine, spec 03 §5.3.1).
 * @param {(note: { message: string, type: string, directory: string, sessionId: string }) => void} [options.onNotify]
 */
/**
 * web 版 `ExtensionUIContext`（spec 03 §5.1 D-C1）。每个
 * (directory, sessionId) 一个实例，由引擎会话生命周期持有。仅当存活
 * 租约存在时才允许注册对话框 —— 没有租约时调用以 SDK 自身措辞失败
 * 关闭，而不是创建一个无人能答的对话框（R13）。终端专属成员为显式
 * no-op，镜像 RPC 降级（rpc-mode.ts:824-925）。
 */
export const createDialogBridge = ({
  leases,
  registry,
  directory,
  sessionId,
  approvalContext = null,
  onNotify = null,
  chrome = null,
}: DialogBridgeOptions): DialogBridge => {
  // 统一注册门禁：无存活租约即抛 'no interactive UI available'（与 SDK 审批门同一诚实性，wrapper.ts:307-322）。
  const register = (kind: DialogKind, payload: DialogPayload) => {
    if (!leases.has(directory, sessionId)) {
      // Same honesty as the SDK approval gate (wrapper.ts:307-322): no lease
      // means hasUI is false and nothing interactive may be pending.
      throw new Error('no interactive UI available');
    }
    return registry.register({ directory, sessionId, kind, payload });
  };

  // 等待注册 promise 结算并按 map 投影为各成员期望的 SDK 返回值。
  const settleValue = async <T,>(
    entry: DialogRegistration,
    map: (settled: DialogSettlement) => T,
  ): Promise<T> => {
    const settled = await entry.promise;
    return map(settled);
  };

  // 把 dialogOptions.signal 接到注册表中止路径：abort 时中止待处理对话框；结算后解绑监听。
  const wireSignal = (entry: DialogRegistration, dialogOptions?: ExtensionUIDialogOptions) => {
    const signal = dialogOptions?.signal;
    if (!signal) return entry;
    const onAbort = () => registry.abortIfPendingSignal(entry.id, 'dialog aborted by signal');
    signal.addEventListener('abort', onAbort, { once: true });
    const detach = () => signal.removeEventListener('abort', onAbort);
    entry.promise.then(detach, detach);
    return entry;
  };

  // 把 SDK 选项项（字符串或 {label} 对象）归一为标签字符串数组。
  const labels = (options: ExtensionUISelectItem[]): string[] =>
    (options ?? []).map((option) => (typeof option === 'string' ? option : option?.label ?? ''));

  // select 成员：恰为 'Approve'/'Deny' 双选项时识别为审批对话框并叠加增补上下文，否则注册普通单选；返回选中标签或 undefined。
  const select = async (
    title: string,
    options: ExtensionUISelectItem[],
    dialogOptions?: ExtensionUIDialogOptions,
  ) => {
    const optionLabels = labels(options);
    const isApproval =
      optionLabels.length === 2 && optionLabels[0] === 'Approve' && optionLabels[1] === 'Deny';
    let entry: DialogRegistration;
    if (isApproval) {
      const context = approvalContext?.() ?? {};
      entry = register('approval', {
        approval: {
          prompt: title,
          ...(context.approvalMode ? { approvalMode: context.approvalMode } : {}),
          ...(context.toolName ? { toolName: context.toolName } : {}),
          ...(context.toolCallId ? { toolCallId: context.toolCallId } : {}),
          ...(context.tier ? { tier: context.tier } : {}),
          ...(context.reason ? { reason: context.reason } : {}),
        },
      });
    } else {
      entry = register('select', { select: { title, options: optionLabels } });
    }
    wireSignal(entry, dialogOptions);
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' && settled.result?.kind === 'select'
        ? settled.result.value
        : undefined);
  };

  // confirm 成员：注册确认对话框；非应答结算一律返回 false。
  const confirm = async (
    title: string,
    message: string,
    dialogOptions?: ExtensionUIDialogOptions,
  ) => {
    const entry = wireSignal(register('confirm', { confirm: { title, message } }), dialogOptions);
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' && settled.result?.kind === 'confirm'
        ? Boolean(settled.result.value)
        : false);
  };

  // input/editor 共用投影：注册对应种类对话框；返回文本或 undefined。
  const inputLike = async (
    kind: 'input' | 'editor',
    title: string,
    placeholder: string | undefined,
    dialogOptions?: ExtensionUIDialogOptions,
  ) => {
    const field = { title, ...(placeholder !== undefined ? { placeholder } : {}) };
    const entry = wireSignal(
      register(kind, kind === 'editor' ? { editor: field } : { input: field }),
      dialogOptions,
    );
    return settleValue(entry, (settled) =>
      settled.outcome === 'responded' && (settled.result?.kind === 'input' || settled.result?.kind === 'editor')
        ? settled.result.value
        : undefined);
  };

  // askDialog 成员：问题透传（含超时换算），结算时把客户端答案项回填为 SDK 的 ExtensionAskDialogResult；cancel 触发 ask 工具中止（ask.ts:914-917）。
  const askDialog = async (
    questions: ExtensionAskDialogQuestion[],
    dialogOptions?: ExtensionUIDialogOptions,
  ): Promise<ExtensionAskDialogResult | undefined> => {
    const timeoutMs =
      typeof dialogOptions?.timeout === 'number' && dialogOptions.timeout > 0
        ? dialogOptions.timeout
        : 0;
    const passthrough = (questions ?? []).map((question) => ({
      id: question.id,
      question: question.question,
      ...(question.header !== undefined && question.header !== '' ? { header: question.header } : {}),
      options: (question.options ?? []).map((option) => ({
        label: option.label,
        ...(option.description ? { description: option.description } : {}),
        ...(option.preview ? { preview: option.preview } : {}),
      })),
      ...(question.multi !== undefined ? { multi: question.multi } : {}),
      ...(question.recommended !== undefined ? { recommended: question.recommended } : {}),
    }));
    const entry = wireSignal(register('ask', { ask: { questions: passthrough, timeoutMs } }), dialogOptions);
    return settleValue(entry, (settled) => {
      const result = settled.result;
      if (result?.kind === 'chat') return { kind: 'chat' };
      if (result?.kind !== 'ask') return undefined; // cancel → ask tool aborts (ask.ts:914-917)
      const byId = new Map(passthrough.map((question) => [question.id, question]));
      const results = result.results.map((item) => {
        const question = byId.get(item.id);
        return {
          id: item.id,
          question: question?.question ?? '',
          options: (question?.options ?? []).map((option) => option.label),
          multi: Boolean(question?.multi),
          selectedOptions: item.selectedOptions ?? [],
          ...(item.customInput ? { customInput: item.customInput } : {}),
          ...(item.note ? { note: item.note } : {}),
          ...(item.timedOut ? { timedOut: true } : {}),
        };
      });
      return { kind: 'submit', results };
    });
  };

  return {
    timeoutStartsOnPresentation: true,
    select,
    confirm,
    input: (title, placeholder, dialogOptions) => inputLike('input', title, placeholder, dialogOptions),
    askDialog,
    editor: (title, prefill, dialogOptions) => inputLike('editor', title, prefill, dialogOptions),
    notify: (message, type) =>
      onNotify?.({ message, type: type ?? 'info', directory, sessionId }),
    // Terminal-only surface (spec 03 §5.1 触点 4) — string-payload chrome
    // delegates to the extension chrome table when provided (spec 09 §5,
    // mirroring RpcExtensionUIRequest); TUI-bound members stay no-ops but
    // count as observable drops (09 R-E3).
    onTerminalInput: () => () => {},
    setStatus: (key, text) => chrome?.setStatus(key, text),
    setWorkingMessage: () => chrome?.noteDropped('setWorkingMessage'),
    setWidget: (key, content, options) => chrome?.setWidget(key, content, options),
    setFooter: () => chrome?.noteDropped('setFooter.factory'),
    setHeader: () => chrome?.noteDropped('setHeader.factory'),
    setTitle: () => chrome?.noteDropped('setTitle'),
    custom: async () => chrome?.noteDropped('custom.factory'),
    setEditorText: () => chrome?.noteDropped('setEditorText'),
    pasteToEditor: (text) => chrome?.noteDropped('pasteToEditor'),
    getEditorText: () => '',
    addAutocompleteProvider: () => {},
    setEditorComponent: () => {},
    get theme() {
      return {};
    },
    getAllThemes: async () => [],
    getTheme: async () => undefined,
    setTheme: async () => ({ success: false, error: 'the web dialog bridge does not manage themes' }),
    getToolsExpanded: () => false,
    setToolsExpanded: () => {},
  };
};

/**
 * "Always allow" advanced action — a transaction, not a button (spec 03
 * §5.3.2, master R10). Hard ordering: the settings write happens first and
 * only a successful write approves. A failed write never approves (the dialog
 * stays open; the caller surfaces the error and manual Approve/Deny remain).
 * A 409 from the approve step (another client settled first) leaves the
 * persisted override in place — same semantics as the settings page, auditable.
 *
 * @param {() => Promise<void>} settingsWrite Writes `tools.approval.<tool|policyKey> = "allow"`
 *        through the Ch06 settings channel. Rejection aborts the transaction.
 * @param {() => Promise<{ ok?: boolean, status?: number }>} approve Posts the Approve respond.
 * @returns {Promise<{ settingsWritten: true, approved: boolean, alreadySettled: boolean }>}
 * @throws the settings-write error (approve is never called), or a non-409
 *         approve error.
 */
/**
 * "总是允许"高级动作 —— 是事务而非按钮（spec 03 §5.3.2，master R10）。
 * 硬性顺序：先写设置，写成功才审批。写失败绝不审批（对话框保持打开；
 * 调用方呈现错误，手动 Approve/Deny 仍可用）。审批步返回 409（另一
 * 客户端先结算）时已写入的持久覆盖保留 —— 与设置页同语义、可审计。
 */
export const alwaysAllowTransaction = async (
  settingsWrite: () => Promise<void>,
  approve: () => Promise<ApproveOutcome | undefined>,
) => {
  await settingsWrite();
  // SAFETY: catches arrive as unknown; the probe reads only the two
  // conflict marker fields and treats everything else as non-conflict.
    // 探测审批结果是否为 409 冲突（status 409 或 alreadySettled 标记）。
  const isConflict = (cause: unknown): boolean => {
    // SAFETY: catches arrive untyped; the probe reads only the two conflict markers.
    const probe = cause as ApproveOutcome | null | undefined;
    return probe?.status === 409 || probe?.alreadySettled === true;
  };
  let approved = true;
  try {
    const result = await approve();
    if (isConflict(result)) approved = false;
  } catch (error) {
    if (isConflict(error)) approved = false;
    else throw error;
  }
  return { settingsWritten: true, approved, alreadySettled: !approved };
};

/** JSON Response 构造助手。 */
const json = <T extends object>(data: T, init?: ResponseInit): Response => Response.json(data, init);
/** 400 Response 构造助手。 */
const badRequest = (message: string): Response => json({ error: message }, { status: 400 });

/** 解析请求 JSON body；解析失败按空对象处理（各调用方再自行校验必填字段）。 */
const readJsonBody = async <T extends object>(request: Request): Promise<T> => {
  try {
    // SAFETY: boundary parse of an untrusted request body — callers only
    // trust fields after requireStrings / registry validation re-narrow what
    // they read; a malformed body lands in the catch below.
    return (await request.json()) as T;
  } catch {
    // SAFETY: an unparseable body is treated as absent; every caller then
    // validates its required fields, so this empty object is never read.
    return {} as T;
  }
};

/** 校验 body 中指定名字的字段均为非空字符串；返回错误文本或 null。 */
const requireStrings = (
  body: { directory?: unknown; sessionId?: unknown; clientId?: unknown } | null | undefined,
  names: ('directory' | 'sessionId' | 'clientId')[],
): string | null => {
  for (const name of names) {
    if (typeof body?.[name] !== 'string' || body[name] === '') {
      return `'${name}' is required`;
    }
  }
  return null;
};

/** POST /omp/dialogs/lease and /lease/release body (strings validated per request). */
/** POST /omp/dialogs/lease 与 /lease/release 的 body（字符串字段逐请求校验）。 */
export interface LeaseRequestBody {
  /** 作用域目录。 */
  directory?: string;
  /** 会话 id。 */
  sessionId?: string;
  /** 客户端 id（租约持有者键）。 */
  clientId?: string;
}

/** POST /omp/dialogs/{id}/respond|presented|abort body (result validated by the registry). */
/** POST /omp/dialogs/{id}/respond|presented|abort 的 body（result 由注册表校验）。 */
export interface DialogRouteBody {
  /** 作用域目录。 */
  directory?: string;
  /** 客户端 id（应答时回显）。 */
  clientId?: string;
  /** 未验证的应答对象。 */
  result?: unknown;
}

/** Per-request route context supplied by the host route table (host.ts). */
/** host 路由表提供的逐请求路由上下文（host.ts）。 */
export interface DialogsRouteContext {
  /** 路径参数（如 {id}）。 */
  params?: { [name: string]: string | undefined };
  /** 解析后的请求 URL。 */
  url?: URL;
  /** 请求头。 */
  headers?: Headers;
}

/** omp-host route handler (Basic auth is enforced by host.js outside these). */
/** omp-host 路由处理器（Basic auth 由 host.js 在外层强制执行）。 */
export type DialogsRouteHandler = (
  request: Request,
  ctx?: DialogsRouteContext,
) => Response | Promise<Response>;

/** `route(method, pattern, handler)` registration callback (host.ts / endpoints.ts). */
/** `route(method, pattern, handler)` 注册回调（host.ts / endpoints.ts）。 */
export type DialogsRouteMount = (
  method: string,
  pattern: string,
  handler: DialogsRouteHandler,
) => void;

/** Deps the dialog endpoint group mounts with (feature probe injectable). */
/** 对话框端点组挂载依赖（能力探针可注入）。 */
export interface DialogsRouteDeps {
  /** UI 租约表。 */
  leases: UiLeaseTable;
  /** 待处理对话框注册表。 */
  registry: PendingDialogRegistry;
  /** 能力探针（默认活体 ompFeatures）。 */
  feature?: (() => boolean) | null;
}

/** Extra mount options accepted by DialogsDomain.mount(). */
/** DialogsDomain.mount() 接受的附加挂载选项。 */
export interface DialogEndpointOptions {
  /** 能力探针覆盖（默认无）。 */
  feature?: (() => boolean) | null;
}

/**
 * Mount the dialog endpoint group (public paths /api/omp/dialogs*; the web
 * proxy strips /api, so omp-host routes are /omp/dialogs*). Every route is
 * feature-gated at request time against the shared capability table —
 * flipping `dialogs.v1` in omp-parity.js flips the whole surface without a
 * remount. Unattended = no lease = hasUI:false = SDK fail-closed preserved
 * (R13): the gate never changes session interactivity by itself.
 *
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {object} deps
 * @param {UiLeaseTable} deps.leases
 * @param {PendingDialogRegistry} deps.registry
 * @param {() => boolean} [deps.feature] Capability probe (default: live ompFeatures).
 */
/**
 * 挂载对话框端点组（公开路径 /api/omp/dialogs*；web 代理剥掉 /api，
 * 故 omp-host 路由为 /omp/dialogs*）。每个路由在请求时对照共享能力表
 * 做特性门禁 —— 在 omp-parity.js 翻转 `dialogs.v1` 即翻转整个表面而
 * 无需重新挂载。无值守 = 无租约 = hasUI:false = SDK 失败关闭得以保持
 * （R13）：门禁自身绝不改变会话交互性。
 */
export const registerDialogEndpoints = (
  route: DialogsRouteMount,
  { leases, registry, feature = null }: DialogsRouteDeps,
) => {
  // 能力探针：默认读活体 ompFeatures 的 dialogs.v1。
  const enabled = feature ?? (() => Boolean(ompFeatures()['dialogs.v1']));
  // 包装处理器：请求时探测特性，未启用时返回 featureUnavailable 响应。
  const gated = (handler: DialogsRouteHandler): DialogsRouteHandler => async (request, ctx) => {
    if (!enabled()) return featureUnavailable('dialogs.v1');
    return handler(request, ctx);
  };

  // GET /omp/dialogs：按 directory（可选 sessionId）返回权威快照。
  route('GET', '/omp/dialogs', gated(async (request) => {
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!directory) return badRequest('directory is required');
    return json(registry.snapshot({ directory }));
  }));

  // POST /omp/dialogs/lease：获取或续约会话 UI 租约（心跳入口）。
  route('POST', '/omp/dialogs/lease', gated(async (request) => {
    const body = await readJsonBody<LeaseRequestBody>(request);
    const missing = requireStrings(body, ['directory', 'sessionId', 'clientId']);
    if (missing) return badRequest(missing);
    // SAFETY: requireStrings above confirmed all three string fields.
    const leaseInput = body as Required<LeaseRequestBody>;
    const lease = leases.acquire({
      directory: leaseInput.directory,
      sessionId: leaseInput.sessionId,
      clientId: leaseInput.clientId,
    });
    return json({
      leaseId: lease.leaseId,
      expiresAt: lease.expiresAt,
      heartbeatIntervalMs: lease.heartbeatIntervalMs,
    });
  }));

  // POST /omp/dialogs/lease/release：显式释放一个持有者（页面卸载/离开视图）。
  route('POST', '/omp/dialogs/lease/release', gated(async (request) => {
    const body = await readJsonBody<LeaseRequestBody>(request);
    const missing = requireStrings(body, ['directory', 'sessionId', 'clientId']);
    if (missing) return badRequest(missing);
    // SAFETY: requireStrings above confirmed all three string fields.
    const releaseInput = body as Required<LeaseRequestBody>;
    const released = leases.release({
      directory: releaseInput.directory,
      sessionId: releaseInput.sessionId,
      clientId: releaseInput.clientId,
    });
    return json({ ok: true, released: released.released, detached: released.detached });
  }));

  // POST /omp/dialogs/{id}/respond：客户端应答（校验后原子结算；竞答 409）。
  route('POST', '/omp/dialogs/{id}/respond', gated(async (request, ctx) => {
    const body = await readJsonBody<DialogRouteBody>(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const dialogId = ctx?.params?.id;
    if (!dialogId) return badRequest('dialog id required');
    const outcome = registry.respond(dialogId, {
      directory: body.directory,
      clientId: typeof body.clientId === 'string' ? body.clientId : null,
      result: body.result,
    });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, outcome: outcome.outcome });
  }));

  // POST /omp/dialogs/{id}/presented：展示确认（取消 T_present、锚定 T_answer）。
  route('POST', '/omp/dialogs/{id}/presented', gated(async (request, ctx) => {
    const body = await readJsonBody<DialogRouteBody>(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const presentedId = ctx?.params?.id;
    if (!presentedId) return badRequest('dialog id required');
    const outcome = registry.presented(presentedId, { directory: body.directory });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, presentedAt: outcome.presentedAt });
  }));

  // POST /omp/dialogs/{id}/abort：用户/系统中止单个对话框（单卡 Stop）。
  route('POST', '/omp/dialogs/{id}/abort', gated(async (request, ctx) => {
    const body = await readJsonBody<DialogRouteBody>(request);
    if (typeof body?.directory !== 'string' || body.directory === '') {
      return badRequest('directory is required');
    }
    const abortId = ctx?.params?.id;
    if (!abortId) return badRequest('dialog id required');
    const outcome = registry.abort(abortId, { directory: body.directory });
    if (!outcome.ok) {
      return json(
        { error: outcome.error, ...(outcome.outcome ? { outcome: outcome.outcome } : {}) },
        { status: outcome.status },
      );
    }
    return json({ ok: true, outcome: outcome.outcome });
  }));
};

/**
 * Engine integration points the coordinator wires (spec 03 §5.0 item 9,
 * §5.1 D-C1b table). This module deliberately does not touch engine.js.
 *
 * 1. Creation-time `hasUI`: in `#materialize`, pass
 *      hasUI: dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI
 *    to createAgentSession (sdk.ts options.hasUI → toolSession.hasUI, the
 *    ask-tool registration gate). Verified: sdk.ts:562-563 / :1670.
 * 2. Lease flip 0→n on a materialized session: call
 *      session.extensionRunner?.initialize(actions, contextActions, commandActions, dialogs.uiContextFor(dir, sid), 'json')
 *    (repeat-initialize is a supported path, runner.ts:702-705; passing the
 *    uiContext sets runner.hasUI() true, runner.ts:698/:878-880) and then
 *      createAgentSessionResult.setToolUIContext(uiContext, true)
 *    (sdk.ts:3167-3169 → toolContextStore). The runner is reachable as the
 *    public getter `session.extensionRunner` (agent-session.ts:9425) — keep
 *    the CreateAgentSessionResult for setToolUIContext.
 * 3. Lease flip n→0: re-initialize with uiContext omitted (→ noOpUIContext,
 *    fail-closed approval gate), setToolUIContext(uiContext, false). The
 *    orphan windows and settled events are handled inside this domain
 *    (`onSessionUiDetached` below is the engine hook).
 * 4. Lifecycle exits (SIGTERM/SIGINT, dispose routes, session delete):
 *      dialogs.registry.settleAll('<exit name>')
 *    before disposing sessions; `onDiagnostic` receives per-dialog notes for
 *    the transcript. `dialogs.dispose(reason)` does settleAll + releaseAll.
 * 5. Idle sweeper guard: skip sessions where
 *      dialogs.registry.pendingCount({ directory, sessionId }) > 0.
 *
 * @param {DomainDialogsOptions} [options]
 * @param {import('./events.ts').OmpEventBus} [options.bus] omp event channel.
 * @param {(info: { directory: string, sessionId: string }) => void} [options.onSessionUiAttached]
 *        Engine hook: perform assembly (2) for an already-materialized session.
 * @param {(info: { directory: string, sessionId: string }) => void} [options.onSessionUiDetached]
 *        Engine hook: perform disassembly (3). Orphan windows start first so
 *        the registry state is already bounded when the hook runs.
 * @param {(note: object) => void} [options.onDiagnostic] R11 transcript hook.
 * @param {object} [options.clock] Test seam { now, schedule, cancel }.
 * @param {object} [options.config] { leaseTtlMs, leaseHeartbeatMs, presentTtlMs, orphanWindowMs }.
 */
/**
 * 协调器接线的引擎集成点（spec 03 §5.0 第 9 项、§5.1 D-C1b 表）。
 * 本模块刻意不触碰 engine.js。要点：
 * 1. 创建时 hasUI：在 #materialize 里向 createAgentSession 传
 *    hasUI: dialogs.hasUISnapshotFor(...).hasUI（ask 工具注册门禁）。
 * 2. 已物化会话的租约 0→n 翻转：以 uiContextFor(dir, sid) 重复
 *    initialize extensionRunner，再 setToolUIContext 挂上该上下文。
 * 3. 租约 n→0：以省略 uiContext 的方式重新初始化（noOpUIContext 失败
 *    关闭审批门）并 setToolUIContext(uiContext, false)；孤儿窗口与
 *    settled 事件在本域内处理（onSessionUiDetached 即引擎钩子）。
 * 4. 生命周期退出（SIGTERM/SIGINT、dispose 路由、会话删除）：先
 *    registry.settleAll('<退出名>') 再处置会话；dispose = settleAll +
 *    releaseAll，onDiagnostic 收每对话框的转写附注。
 * 5. 空闲清扫守卫：pendingCount > 0 的会话跳过不清扫。
 */
export const createDomainDialogs = ({
  bus = null,
  onSessionUiAttached = null,
  onSessionUiDetached = null,
  onLeaseAcquired = null,
  onDiagnostic = null,
  clock = null,
  config = {},
}: DomainDialogsOptions = {}): DialogsDomain => {
  const now = clock?.now ?? Date.now;
  const schedule = clock?.schedule ?? setTimeout;
  const cancel = clock?.cancel ?? clearTimeout;
  const registry = new PendingDialogRegistry({
    bus,
    now,
    schedule,
    cancel,
    ...(config.presentTtlMs !== undefined ? { presentTtlMs: config.presentTtlMs } : {}),
    ...(config.orphanWindowMs !== undefined ? { orphanWindowMs: config.orphanWindowMs } : {}),
    onDiagnostic,
  });
  const leases = new UiLeaseTable({
    now,
    schedule,
    cancel,
    ...(config.leaseTtlMs !== undefined ? { ttlMs: config.leaseTtlMs } : {}),
    ...(config.leaseHeartbeatMs !== undefined ? { heartbeatIntervalMs: config.leaseHeartbeatMs } : {}),
    onAttach: ({ directory, sessionId }) => {
      registry.recoverOrphanWindow({ directory, sessionId });
      onSessionUiAttached?.({ directory, sessionId });
    },
    onDetach: ({ directory, sessionId }) => {
      registry.enterOrphanWindow({ directory, sessionId });
      onSessionUiDetached?.({ directory, sessionId });
    },
    onAcquired: ({ directory, sessionId }) => {
      onLeaseAcquired?.({ directory, sessionId });
    },
  });
  /** @type {Map<string, DialogBridge>} one bridge per (directory, sessionId) */
  // 逐 (directory, sessionId) 缓存的桥实例表。
  const bridges = new Map<string, DialogBridge>();
  return {
    leases,
    registry,
    /** Per-session WebUIContext (cached for the session lifetime). */
    /** 逐会话 WebUIContext（会话生命周期内缓存；首建后附加项不再生效）。 */
    uiContextFor(directory: string, sessionId: string, bridgeOptions: DialogBridgeExtras = {}) {
      const key = sessionKey(directory, sessionId);
      let bridge = bridges.get(key);
      if (!bridge) {
        bridge = createDialogBridge({
          leases,
          registry,
          directory,
          sessionId,
          ...bridgeOptions,
        });
        bridges.set(key, bridge);
      }
      return bridge;
    },
    /** Consumers contract for creation-time hasUI + diagnostics (R13). */
    /** 创建时 hasUI + 诊断的消费方契约（R13）。 */
    hasUISnapshotFor(directory, sessionId) {
      return leases.snapshot(directory, sessionId);
    },
    /** Mount the endpoint group onto the shared route table. */
    /** 把端点组挂载到共享路由表；支持链式调用。 */
    mount(route: DialogsRouteMount, options: DialogEndpointOptions = {}) {
      registerDialogEndpoints(route, { leases, registry, ...options });
      return this;
    },
    /** Per-session release (plan §6): settle dialogs + drop the bridge. */
    /** 逐会话释放（plan §6）：结算待处理对话框并丢弃缓存桥，会话逐出不泄漏 WebUIContext；关停走 dispose。 */
    releaseSession(directory: string, sessionId: string, reason: string = 'session disposed') {
      registry.abortForSession({ directory, sessionId }, reason);
      bridges.delete(sessionKey(directory, sessionId));
    },
    /** Host lifecycle exit: settle everything, drop every lease (R11). */
    /** host 生命周期退出：全部结算、丢弃所有租约（R11）；返回结算计数。 */
    async dispose(reason: string = 'omp-host shutdown') {
      const settled = registry.settleAll(reason);
      leases.releaseAll();
      bridges.clear();
      return settled;
    },
  };
};
