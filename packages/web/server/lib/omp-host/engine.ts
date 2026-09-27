// omp engine manager: embeds @oh-my-pi/pi-coding-agent sessions behind the
// OpenCode-compatible wire surface.
//
// One HostSession per OMPChamber session id. Transcripts live in omp's
// SessionManager JSONL files (cwd-derived directory); OMPChamber-specific
// metadata lives in the sidecar registry. Cold reads project the persisted
// transcript without materializing an agent; the first prompt (or any live
// operation) materializes a full AgentSession whose event stream is projected
// into wire events on the host bus.
/**
 * omp 引擎管理器（模块级说明）：把 @oh-my-pi/pi-coding-agent 的会话嵌入到
 * OpenCode 兼容的 wire 协议表面之后。每个 OMPChamber 会话 id 对应一个
 * HostSession；转录落在 omp SessionManager 的 JSONL 文件（目录由 cwd 推导），
 * OMPChamber 特有元数据存放在 sidecar 注册表。冷读直接投影持久化转录、
 * 不物化 agent；首次 prompt（或任何 live 操作）才物化完整 AgentSession，
 * 其事件流被投影为宿主总线上的 wire 事件。
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { AgentRegistry, ModelRegistry, SessionManager, BUILTIN_TOOLS, createAgentSession, discoverAuthStorage } from '@oh-my-pi/pi-coding-agent';
import { discoverAgents, refreshAgentDiscovery } from '@oh-my-pi/pi-coding-agent/task';
import { getConfigDirs } from '@oh-my-pi/pi-coding-agent/config';
import { initializeExtensions } from '@oh-my-pi/pi-coding-agent/modes/runtime-init';
import { isTodoPhase } from '@oh-my-pi/pi-coding-agent/tools/todo';
import { registerPersistedSubagents } from '@oh-my-pi/pi-coding-agent/registry/persisted-agents';
import { buildSkillPromptMessage, parseSkillInvocation } from '@oh-my-pi/pi-coding-agent/extensibility/skills';
import { SKILL_PROMPT_MESSAGE_TYPE } from '@oh-my-pi/pi-coding-agent/session/messages';
import { getSessionSlashCommands } from '@oh-my-pi/pi-coding-agent/extensibility/extensions/get-commands-handler';
import type { ExtensionUIContext } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.ts';
import type { SessionMeta, SessionMetadataValue } from './registry.ts';
import { LiveSessionRegistry, SessionBusyError, sessionKey, type LiveRecord, type LiveSessionState } from './live-registry.ts';
import { withColdManager } from './cold-reader.ts';
import { readSessionEventRows, readSessionScalars, readTranscriptMessagePage } from './cold-transcript-page.ts';
import { classifyExternalChange, fileSignature, tailEntryIdOf, type FileSignature } from './dual-write.ts';
import { WireEventBus, OmpEventBus } from './events.ts';
import {
  StreamProjector,
  normalizeToolExecutionResult,
  projectConversation,
  projectCustomMessage,
  projectDeveloperMessage,
  projectDividerMessage,
  projectExecutionMessage,
  projectUserMessage,
  buildTurnStateStamper,
  projectTurnEventDivider,
  wireMessageId,
  executionWireId,
  deterministicWireId,
  resolveWireIdToEntryId,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.ts';
import type { UsageInput, ProjectedContentInput, ProjectedMessage, AssistantMessageInput, ShellExecutionMessageInput, WireIdMessageInput, UserProjectionOptions } from './projection.ts';
import { createSettingsStore } from './domain-models.ts';
import { createDomainDialogs } from './domain-dialogs.ts';
import {
  ModeDomainError,
  createModesDomain,
  mapBackedStore,
  migrateSidecarAgents,
  personaFor,
  serializeAgentMarkdown,
} from './domain-modes.ts';
import type { PreparePlanReviewResult } from './domain-modes.ts';
import { createDomainChrome } from './domain-chrome.ts';
import { createProcessDomain } from './domain-processes.ts';
import { ProcessLedger } from './process-ledger.ts';
import { createProcessPlatform } from './process-platform.ts';
import { errorText, errorCode, ompFeatures } from './omp-parity.ts';
import { revealCommand } from './domain-plugins.ts';
import {
  createUriDomain,
  createLocalProtocolOptions,
  buildEntryTreeSnapshot,
  ARTIFACTS_MAX_FILES_PER_SESSION,
  artifactsDirForSessionFile,
} from './domain-uri.ts';
import { resolveLocalUrlToPath } from '@oh-my-pi/pi-coding-agent/internal-urls/local-protocol';
import type { AgentSession, AgentSessionEvent, AuthStorage, CreateAgentSessionResult, SessionInfo, SessionEntry } from '@oh-my-pi/pi-coding-agent';
import type { BashExecutionMessage, CustomMessage, HookMessage } from '@oh-my-pi/pi-coding-agent';
import { isPersistentShellCdCommand } from '@oh-my-pi/pi-coding-agent/exec/bash-executor';
import type { BashResult } from '@oh-my-pi/pi-coding-agent/exec/bash-executor';
import type { SettingsStore, RegistryModel } from './domain-models.ts';
import type { DialogsDomain } from './domain-dialogs.ts';
import type { ModesDomain } from './domain-modes.ts';
import type { DomainChrome } from './domain-chrome.ts';
import type { ProcessDomain } from './domain-processes.ts';
import type { LedgerToolArgs, LedgerToolDetails, LedgerToolEnd } from './process-ledger.ts';
import type { AgentsSnapshotEntry, DiskScanRow, UriDomain } from './domain-uri.ts';
/** 空闲会话的存活上限（30 分钟）：live 记录静默超过该时长即成为逐出候选。 */
const IDLE_SESSION_TTL_MS = 30 * 60 * 1000;
// Idle-session sweep period (plan D2: 60s; the count gate is gone — quantity
// is not a memory bound, idle lifetime is).
/** 空闲清扫周期（plan D2：60s；数量门槛已移除——数量不是内存上界，空闲时长才是）。 */
const IDLE_SWEEP_INTERVAL_MS = 60_000;
// Drain budget for a single eviction's agentSession.dispose (SDK default is
// 5s; allow headroom for flush) and the global shutdown deadline for all
// disposals combined (plan §3.4).
/** 单次逐出 agentSession.dispose 的排空预算（SDK 默认 5s，此处留出 flush 余量）。 */
const EVICT_DRAIN_TIMEOUT_MS = 8_000;
/** shutdown 时全部 dispose 合计的全局截止时限（plan §3.4）。 */
const SHUTDOWN_DISPOSE_DEADLINE_MS = 10_000;
// Failed-tombstone recovery (plan §3.4): a transient dispose failure
// (Windows file lock, AV scan) must not 409 the session until a restart.
// The sweeper retries the retained re-dispose closure past a cooldown, up
// to a bounded total attempt count.
/** 失败墓碑的重试冷却（plan §3.4）：瞬时 dispose 失败（Windows 文件锁、杀毒扫描）在冷却期满并低于最大尝试次数前不得让会话持续 409。 */
const FAILED_DISPOSE_RETRY_COOLDOWN_MS = 5 * 60 * 1000;
/** 失败墓碑的最大重试次数：超过后保持隔离直到进程重启。 */
const FAILED_DISPOSE_MAX_ATTEMPTS = 3;

// Bound on how long engine.abort waits for AgentSession.abort's teardown
// (post-prompt drain + agent idle). The pi drain has no internal timeout on
// the abort path (dispose caps it at 5s; abort does not), so one signal-blind
// tool or never-settling post-prompt task would park the stop request forever.
/** engine.abort 等待 AgentSession.abort 收尾（post-prompt 排空与 agent 空闲）的上限：pi 的排空在 abort 路径上没有内置超时（dispose 有 5s 上限而 abort 没有），单个对信号无响应的工具会永远挂住 Stop 请求。 */
const ABORT_TEARDOWN_TIMEOUT_MS = 10_000;
// SAFETY: single boundary cast — DialogBridge is the deliberate web
// degradation of the SDK's ExtensionUIContext (stub theme; custom()
// resolves void instead of the generic T). The extension runner only
// consumes the web-capable subset at runtime.
/** SAFETY 单点边界转换：DialogBridge 是 SDK ExtensionUIContext 在 web 场景的有意降级（stub 主题、custom() 返回 void 而非泛型 T）；扩展运行时只消费 web 可用的子集。 */
const asExtensionUiContext = <T,>(bridge: T): ExtensionUIContext | undefined =>
  bridge as ExtensionUIContext | undefined;

/**
 * Session-level persona key (02 §5.1 D-B3): unset and the deleted
 * build/plan pair map to the standard session; any other name is a persona.
 */
/** 会话级 persona 键（02 §5.1 D-B3）：未设置与已删除的 build、plan 一对都归一为 standard 会话；其它名字即 persona。 */
const personaKeyFor = (name: string | undefined): string => (!name || name === 'build' || name === 'plan' ? 'standard' : name);

/** Wire `agent` projection: the standard session keeps the legacy 'build' id. */
/** wire agent 投影：standard 会话保留旧的 'build' id。 */
const wireAgentFor = (personaKey: string): string => (personaKey === 'standard' ? 'build' : personaKey);

/** 提取消息内容的纯文本：字符串直接返回；内容块数组只拼接 type 为 text 的块（其余如图片忽略），非数组返回空串。 */
const textOfContent = (content: ProjectedContentInput | null | undefined): string => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && b.type === 'text')
    .map((b) => b.text)
    .join('');
};

/** 把 {provider, id} 模型引用序列化为 provider/id 选择器字符串；空引用返回 undefined。 */
const modelSelector = (model: { provider: string; id: string } | null | undefined): string | undefined => (model ? `${model.provider}/${model.id}` : undefined);

/** Persona record (spec 02 §5.2): mirrored into the personas sidecar. */
/** Persona 记录（spec 02 §5.2）：镜像持久化到 personas sidecar。 */
interface Persona {
  /** persona 名称（同时作为 personas Map 的键）。 */
  name: string;
  /** 展示用描述。 */
  description?: string;
  /** 构造会话时覆盖顶层 system prompt 的提示词。 */
  systemPrompt?: string;
  /** persona 允许的工具名白名单；缺省表示不覆盖工具集。 */
  tools?: string[];
}

/** SDK AgentSession 的类型别名，便于在 engine 内与宿主侧包装区分。 */
type SdkAgentSession = AgentSession;
/** createAgentSession 返回的 setToolUIContext 句柄类型（UI 租约注入 SDK 的入口）。 */
type SdkSetToolUIContext = CreateAgentSessionResult['setToolUIContext'];

/** Plugin discovery snapshot frozen at session materialization (plugins.v1). */
/** 会话物化时冻结的插件发现快照（plugins.v1）。 */
interface AppliedPluginsSnapshot {
  /** 快照生成时间（毫秒时间戳）。 */
  appliedAt: number;
  /** 该会话绑定的扩展模块路径（已 resolve 为绝对路径）。 */
  extensionPaths: string[];
  /** 生效中的插件名称列表。 */
  pluginNames: string[];
}

/** Engine-side record for one live omp-host session id. */
/** 一个 live omp-host 会话 id 在引擎侧的完整宿主记录。 */
interface HostSession {
  /** Registry key (`directory\0sessionID`) — lifecycle lookups go through it. */
  /** 注册表键（目录与 sessionID 以 NUL 连接）——生命周期查找都经由它。 */
  key: string;
  /** omp 会话 id。 */
  sessionId: string;
  /** 会话所属的项目目录键（normalizeDirectoryKey 之后）。 */
  directory: string;
  /** SDK AgentSession 实例；null 表示尚未物化或已被逐出。 */
  agentSession: SdkAgentSession | null;
  /** createAgentSession 的结果句柄（此处只保留 setToolUIContext 供 UI 租约注入）。 */
  sdkResult: Pick<CreateAgentSessionResult, 'setToolUIContext'>;

  /** 当前生效的 persona key（'standard' 表示无 persona）。 */
  currentPersona: string;
  /** 当前 assistant 轮的流式投影器；轮结束后保留以承接异步任务更新。 */
  projector: StreamProjector | null;
  /** 待匹配的客户端回显用户消息 wire id：message_start 时与规范公式 id 建立映射。 */
  pendingUserWireId: string | null;
  /**
   * Client-echoed user wire ids (canonical → client messageID, plan phase 5
   * compact mapping): scoped to this live record so the entries die with the
   * session instead of accumulating process-wide. Assistant ids need no
   * entry — the stable formula makes live and cold ids identical.
   */
  /** 客户端回显的用户 wire id 映射（规范 id 到客户端 messageID，plan phase 5 紧凑映射）：作用域限于本 live 记录，随会话消亡而非进程级累积。assistant id 无需条目——稳定公式使 live 与冷 id 天然一致。 */
  wireIdEchoes: Map<string, string>;
  /**
   * Deferred `!` echo bridges (executeBash): a mid-turn bash record defers to
   * the SDK's pendingMessages flush, so it is not in session.messages when the
   * dispatch resolves. Each entry carries the dispatch-time live id; the next
   * #wireIdResolver call drains it once the record lands, registering the
   * canonical → live echo so cold projections keep emitting the live row id.
   */
  /** 延迟注册的 `!` 回显桥（executeBash）：轮中的 bash 记录延迟到 SDK 的 pendingMessages flush，派发结算时还不在 session.messages。每项携带派发时 live id；记录落地后由下一次 #wireIdResolver 调用排空并注册规范 id 到 live 回显的映射，冷投影得以持续发出 live 行 id。 */
  pendingShellEchoes?: Array<{ liveId: string; command: string; started: number; output: string; exitCode: number | undefined; cancelled: boolean }>;
  /** 最近一条用户侧消息的 wire id（assistant 消息的 parentID 锚点）。 */
  lastUserWireId: string | null;
  /** tail-sync 已同步过的条目键集合（role、customType、timestamp），保证幂等。 */
  syncedEntryKeys?: Set<string>;
  /** 最近一条已结算 assistant 消息的 wire id（retry、fallback 事件的连接键）。 */
  lastAssistantWireId: string | null;
  /** 进入"等待异步恢复"状态的时间戳；null 表示不在等待（agent_end 的 isTerminal 为 false 时置位）。 */
  awaitingAsyncSince: number | null;
  /** 本会话私有的 AgentRegistry 实例（SDK 进程级全局 registry 只允许一个 Main agent，而本宿主并发嵌入多个顶层会话）。 */
  agentRegistry: AgentRegistry;
  /** Cross-turn finalized tool parts — async-job task updates revive them. */
  /** 跨轮次的已结算工具部件——异步任务的 task 更新会复活它们。 */
  finalToolParts: Map<string, { id: string; messageID: string; toolName: string }>;
  /** 扩展 UI 是否已完成 initializeExtensions。 */
  extensionUiInitialized: boolean;
  /** 进行中的 initializeExtensions Promise；null 表示没有进行中的初始化。 */
  extensionUiPromise: Promise<unknown> | null;
  /** plan 模式的 proposal handler 是否已挂到 SDK 会话。 */
  planHandlerAttached: boolean;
  /** 物化时冻结的插件应用快照；null 表示快照未生成或失败。 */
  appliedPlugins: AppliedPluginsSnapshot | null;
  /** Per-turn tool-result pairing map (tool_execution_end → message_end settle). */
  /** 本轮的工具结果配对表（tool_execution_end 到 message_end 结算）。 */
  turnToolResults?: Map<string, { content?: unknown; isError?: boolean; timestamp?: number }> | null;
  /** SDK 会话事件订阅的退订函数。 */
  unsubscribe?: () => void;
  /** Agent-registry event subscription feeding the agent-runs aggregator. */
  /** 供给 agent-runs 聚合器的 agent 注册表事件订阅退订函数。 */
  unsubscribeAgentRegistry?: () => void;
  /** onSessionNameChanged unsubscribe (plan §3.3.2 — never leak the callback). */
  /** onSessionNameChanged 的退订函数（plan §3.3.2——绝不泄漏回调）。 */
  nameUnsubscribe?: () => void;
  /** Transcript identity at materialize; dual-write detection (plan §8). */
  /** 物化时的转录身份签名；双写检测用（plan §8）。 */
  fileSignature: FileSignature | null;
}

/** The SessionManager surface #infoFromManager reads (SDK SessionManager). */
/** #infoFromManager 读取的 SessionManager 表面（SDK SessionManager 的结构子集）。 */
type SessionManagerLike = {
  /** 读取转录头部（含 timestamp），用于取创建时间。 */
  getHeader(): { timestamp?: string } | null | undefined;
  /** 读取全部条目（含 timestamp），用于取最后修改时间。 */
  getEntries(): Array<{ timestamp?: string }> | null | undefined;
  /** 读取会话 id。 */
  getSessionId(): string;
  /** 读取录制时的工作目录。 */
  getCwd(): string | undefined;
  /** 读取会话标题。 */
  getSessionName(): string | undefined;
};

/** #tailSyncTranscript result: wire ids emitted this pass + divider anchor. */
/** #tailSyncTranscript 的结果：本轮投影出的 wire id 行列表 + 最近一次 compaction 分隔锚点。 */
interface TailSyncTail {
  /** 本轮投影出的消息行（wire id 与角色；wire id 可能为 null）。 */
  projected: Array<{ wireId: string | null; role: string }>;
  /** 最近一条 compactionSummary 分隔消息的 wire id；无则 null。 */
  lastCompactionId: string | null;
}

/** Wire `Session` record as projected by #wireSession (vendored contract's
 * fields, server-side copy; `revert` is attached only by the revert flow). */
/** wire 协议的 Session 记录（#wireSession 投影的服务端副本；revert 字段仅在 revert 流程中挂载）。 */
interface WireSessionRecord {
  /** 会话 id（omp sessionID）。 */
  id: string;
  /** URL slug——与 id 相同（OpenCode 兼容字段）。 */
  slug: string;
  /** 目录派生的项目 id（prj_ 前缀 + sha256 前 20 位）。 */
  projectID: string;
  /** 会话所属目录（已归一化）。 */
  directory: string;
  /** subagent 父会话 id；仅 subagent 会话携带。 */
  parentID?: string;
  /** Fork lineage (§5.4): wire parentID stays subagent-only; a user fork
   * must remain a normal promptable session in the shared UI. */
  /** fork 谱系（§5.4）：wire parentID 保持仅 subagent 语义；用户 fork 必须仍是共享 UI 中可正常 prompt 的会话。 */
  forkParentID?: string;
  /** 会话标题；缺省为 'Untitled'。 */
  title: string;
  /** persona 对应的 wire agent 标识（standard 会话省略该字段）。 */
  agent?: string;
  /** 会话当前模型（providerID/modelID 拆分形式）；未解析时省略。 */
  model?: { id: string; providerID: string };
  /** sidecar 中的自定义元数据键值对。 */
  metadata?: Record<string, SessionMetadataValue>;
  /** 创建/更新/归档时间戳（毫秒）。 */
  time: { created: number; updated: number; archived?: number };
  /** revert 状态（保留边界消息 id）；仅 revert 流程填充。 */
  revert?: { messageID: string };
  /**
   * Live-registry state when a non-cold record exists; absent = cold
   * (no writer, no entries mirror in memory). 'failed' is the quarantine
   * tombstone — the SDK object is still held pending retry.
   */
  /** 存在非冷记录时的 live 注册表状态；缺席即冷（无写方、内存无条目镜像）。'failed' 是隔离墓碑——SDK 对象仍被持有等待重试。 */
  live?: LiveSessionState;
  /**
   * Transcript bytes — the honest proxy for a session's memory cost
   * (measured heap ≈ 1.3× the jsonl size): `fileSignature.size` for live
   * records, the SessionManager.list size for cold ones.
   */
  /** 转录字节数——会话内存成本的诚实代理（实测堆占用约为 jsonl 大小的 1.3 倍）：live 记录取 fileSignature.size，冷记录取 SessionManager.list 的 size。 */
  transcriptBytes?: number;
}
/** The SessionInfo fields the wire projection reads. Synthesized rows
 * (registry-only, live-only) provide exactly these; full SDK SessionInfos
 * are structurally assignable. `created`/`modified` tolerate Date-or-string
 * because cold reads hand through transcript timestamps unparsed. */
/** wire 投影读取的 SessionInfo 字段子集：合成行（仅注册表、仅 live）恰好提供这些字段，完整 SDK SessionInfo 结构上可赋值。created/modified 兼容 Date 或字符串——冷读直接透传未解析的转录时间戳。 */
interface SessionListInfo {
  /** 会话 id。 */
  id: string;
  /** 转录记录的工作目录。 */
  cwd: string;
  /** 会话标题（可能未设置）。 */
  title?: string;
  /** 创建时间（Date 或 ISO 字符串）。 */
  created: Date | string;
  /** 最后修改时间（Date 或 ISO 字符串）。 */
  modified: Date | string;
  /** Transcript file bytes when the listing knows them (SessionManager.list). */
  /** 列表已知时的转录文件字节数（SessionManager.list 提供）。 */
  size?: number;
}

/**
 * Subagent sessionFiles live at `<sessionsRoot>/<ts>_<sessionID>/<agent>.jsonl`
 * (SDK artifacts layout). Returns the owning session id, or undefined for
 * layouts that do not carry one (in-memory runs, unexpected shapes).
 */
/** 从 subagent 会话文件路径解析出宿主会话 id：SDK artifacts 布局为 sessionsRoot 下的 <ts>_<sessionID>/<agent>.jsonl；路径形态不含 id（内存运行、异常布局）时返回 undefined。 */
const sessionIDFromSessionFile = (sessionFile: string | null | undefined): string | undefined => {
  if (sessionFile === null || sessionFile === undefined || sessionFile.length === 0) return undefined;
  const dirName = path.basename(path.dirname(sessionFile));
  const separator = dirName.indexOf('_');
  if (separator <= 0) return undefined;
  const sessionID = dirName.slice(separator + 1);
  return sessionID.length > 0 ? sessionID : undefined;
};

/** Case-insensitive on win32 (sessions roots arrive with mixed separators). */
/** 判断 candidate 是否位于 root 之下（不含 root 本身）；win32 上先小写化再比较，兼容混合分隔符的 sessions 根路径。 */
const isPathUnder = (candidate: string, root: string): boolean => {
  const relative = path.relative(root, candidate);
  if (relative === '') return false;
  const normalized = process.platform === 'win32' ? relative.toLowerCase() : relative;
  return !normalized.startsWith('..') && !path.isAbsolute(normalized);
};

/**
 * Read a transcript's session id from its leading bytes only. SessionManager
 * parses the whole file; the child-id scan touches every nested transcript in
 * a directory and must stay bounded — the session header rides one of the
 * first lines (a title entry may precede it).
 */
/** 头部探测窗口大小（256 KiB）：子会话 id 扫描会触碰目录下每个嵌套转录，必须保持有界。 */
const CHILD_ID_PROBE_BYTES = 262144;
/** 只读转录文件的前若干行解析 type 为 session 的条目得到会话 id（头部通常在最初几行，标题行可能先行）；探测窗口内未命中返回 undefined。 */
const readSessionHeaderId = async (sessionFile: string): Promise<string | undefined> => {
  const handle = await fs.promises.open(sessionFile, 'r').catch(() => null);
  if (!handle) return undefined;
  try {
    const buffer = Buffer.alloc(CHILD_ID_PROBE_BYTES);
    const { bytesRead } = await handle.read(buffer, 0, CHILD_ID_PROBE_BYTES, 0);
    const text = buffer.subarray(0, bytesRead).toString('utf8');
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed.startsWith('{')) continue;
      let entry: unknown;
      try {
        entry = JSON.parse(trimmed);
      } catch {
        continue; // header not reached within the probe window
      }
      if (entry && typeof entry === 'object' && 'type' in entry && 'id' in entry) {
        const typed = entry as { type?: unknown; id?: unknown };
        if (typed.type === 'session' && typeof typed.id === 'string' && typed.id) return typed.id;
      }
    }
    return undefined;
  } finally {
    await handle.close().catch(() => {});
  }
};

/**
 * omp 宿主引擎：管理 wire 协议会话表面与嵌入式 omp AgentSession 之间的
 * 全部生命周期——惰性物化、空闲逐出、SDK 事件到双轨道事件的投影、
 * settings/dialogs/modes/chrome/URI/进程等领域对象的装配，以及 shutdown
 * 时的有界排空。
 */
export class OmpHostEngine {
  /** live 会话注册表：以目录与 sessionID 拼接为键的宿主记录及其生命周期状态机。 */
  #live: LiveSessionRegistry<HostSession>;
  /** True once shutdown started: new writers are rejected (plan §3.4). */
  /** shutdown 已开始为 true：拒绝新的写方（plan §3.4）。 */
  #closing = false;
  /** Test seams: monotonic TTL clock + agent factory (defaults: real SDK). */
  /** 单调 TTL 时钟（默认 performance.now；测试可注入替身）。 */
  #now: () => number;
  /** agent 工厂接缝（默认 SDK createAgentSession；生命周期测试可注入替身）。 */
  #createAgentSessionImpl: typeof createAgentSession;
  /** Bound on unknown-event diagnostic keys (plan §6: no unbounded key set). */
  /** 未知事件诊断键的数量上限（plan §6：键集合必须有界）。 */
  static #UNKNOWN_EVENT_KEYS_MAX = 64;
  /** Cap on per-session diagnostic rows — data-proportional but bounded. */
  /** 每会话诊断行数上限——随数据增长但有界。 */
  static #DIAGNOSTIC_SESSION_ROWS_MAX = 256;

  /** SDK 认证存储（#boot 时发现）；boot 完成前为 null。 */
  authStorage: AuthStorage | null;
  /** 模型注册表（#boot 时创建并刷新）；boot 完成前为 null。 */
  modelRegistry: ModelRegistry | null;
  /** 会话元数据 sidecar 注册表：存放标题、persona、模型选择器等 OMPChamber 特有元数据。 */
  registry: SessionMetaRegistry;
  /** OpenCode 兼容的 wire 事件总线。 */
  bus: WireEventBus;
  /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
  /** omp 原生事件通道（spec 05 §5.2，master D6-R1 单一权威）。 */
  ompBus: OmpEventBus;
  /** Personas (OC-original optional layer, spec 02 §5.2/R12). */
  /** persona 表（OC 特有可选层，spec 02 §5.2/R12）；持久化为注册表根目录下的 JSON。 */
  personas: Map<string, Persona>;
  /** Per-directory keyed Settings store (spec 06 §5.1, master R6). */
  /** 按目录键控的 Settings 存储（spec 06 §5.1，master R6）；boot 失败时降级为 null。 */
  settingsStore: SettingsStore | null;
  // Approval/ask dialog domain (spec 03, master R10/R11/R13). Lease-driven
  // hasUI: unattended sessions never hold a lease → SDK fail-closed.
  /** 审批/ask 对话领域（spec 03，master R10/R11/R13）。租约驱动 hasUI：无人值守会话不持租约，SDK fail-closed。 */
  dialogs: DialogsDomain;
  // Modes/plan/goal/personas/agent-definitions domain (spec 02).
  /** modes/plan/goal/personas/agent 定义领域（spec 02）。 */
  modesDomain: ModesDomain;
  /** 扩展 chrome 表领域（spec 09 §5）：字符串载荷的 widget/status 投影。 */
  chrome: DomainChrome;
  /** URI 桥/会话树/agent-runs/jobs 领域（spec 04）。 */
  uriDomain: UriDomain;
  /** Session process monitor (PLAN-session-process-monitor.md). The ledger
   * needs the pi-natives Process API the SDK already loaded; null when
   * `createProcessPlatform` finds no resident addon, in which case the
   * domain keeps answering featureUnavailable. */
  /** 会话进程监视器（PLAN-session-process-monitor.md）。台账需要 SDK 已加载的 pi-natives Process API；createProcessPlatform 找不到常驻 addon 时为 null，领域层持续回答 featureUnavailable。 */
  processLedger: ProcessLedger | null;
  /** 进程领域（对外查询表面），包装 processLedger 并叠加特性门控。 */
  processDomain: ProcessDomain;
  /** #boot 的失败原因；成功引导后为 null。 */
  bootError: unknown;
  /** #boot 的去重 Promise；未启动或失败清空后为 null。 */
  bootPromise: Promise<void> | null;
  /** 空闲清扫定时器（IDLE_SWEEP_INTERVAL_MS 周期；已 unref 不阻塞进程退出）。 */
  sweeper: ReturnType<typeof setInterval>;
  /** Engine-wide subscription to the SDK's process-global agent registry. */
  /** 引擎级订阅 SDK 进程级 agent 注册表（跨会话 subagent ref 的来源）。 */
  unsubscribeGlobalRegistry: (() => void) | null = null;
  /** Per-directory historical run rows (nested transcript scan), one shot. */
  /** 每目录的历史运行行（嵌套转录扫描结果），一次性缓存。 */
  #diskRowsByDirectory = new Map<string, Array<DiskScanRow & { file: string }>>();
  /** In-flight directory scans (dedup). */
  /** 进行中的目录扫描（并发去重表）。 */
  #diskScanInFlight = new Map<string, Promise<void>>();
  /** In-flight removed-run rehydrations per host session file (dedup). */
  /** 每宿主会话文件进行中的"被移除运行"再水化（去重集合）。 */
  #rehydrateInFlight = new Set<string>();
  /**
   * Subagent transcript path -> the child's own sessionID, read once from the
   * jsonl header (the path layout `<ts>_<hostID>/<task>.jsonl` carries only
   * the host id). Identity is immutable, so the cache never invalidates.
   */
  /** subagent 转录路径到子会话自身 sessionID 的缓存（从 jsonl 头读一次；身份不可变故永不失效）。 */
  #childSessionIdByFile = new Map<string, string>();
  /** In-flight #warmChildSessionIds run (dedup). */
  /** 进行中的 #warmChildSessionIds 运行（去重）。 */
  #childSessionIdWarm: Promise<void> | null = null;
  /** Lazily-created counters for AgentSessionEvent members with no manifest case. */
  /** 无 manifest 对应的 AgentSessionEvent 成员的惰性计数器。 */
  unknownEventCounts: Map<string, number> | undefined;
  /** How long abort() waits for the agent teardown before force-disposing. */
  /** abort() 等待 agent 收尾的时长，超时则强制 dispose（测试可注入）。 */
  abortTeardownTimeoutMs: number;
  /** Single-eviction SDK drain budget (test injectable, plan §3.4). */
  /** 单次逐出的 SDK 排空预算（测试可注入，plan §3.4）。 */
  evictDrainTimeoutMs: number;
  /** Global shutdown deadline across all disposals (test injectable). */
  /** shutdown 全部 dispose 合计的全局截止（测试可注入）。 */
  shutdownDisposeDeadlineMs: number;

  /**
   * 构造引擎并装配全部领域对象。同步完成——settings、dialogs、modes、
   * chrome、URI、进程等领域工厂都是同步的，端点挂载不等待 #boot；空闲
   * 清扫定时器随之启动并 unref。可选参数均为测试接缝：时钟、agent 工厂
   * 与各超时预算。
   */
  constructor({
    agentDir,
    abortTeardownTimeoutMs,
    evictDrainTimeoutMs,
    shutdownDisposeDeadlineMs,
    now,
    createAgentSession: createAgentSessionImpl,
  }: {
    agentDir?: string;
    abortTeardownTimeoutMs?: number;
    evictDrainTimeoutMs?: number;
    shutdownDisposeDeadlineMs?: number;
    /** Monotonic clock for idle TTL (plan §4.2; test injectable). */
    now?: () => number;
    /** Agent factory seam for lifecycle tests (default: SDK createAgentSession). */
    createAgentSession?: typeof createAgentSession;
  } = {}) {
    this.authStorage = null;
    this.modelRegistry = null;
    this.registry = new SessionMetaRegistry({ agentDir });
    this.bus = new WireEventBus();
    /** How long abort() waits for the agent teardown before force-disposing (test injectable). */
    this.abortTeardownTimeoutMs = abortTeardownTimeoutMs ?? ABORT_TEARDOWN_TIMEOUT_MS;
    this.evictDrainTimeoutMs = evictDrainTimeoutMs ?? EVICT_DRAIN_TIMEOUT_MS;
    this.shutdownDisposeDeadlineMs = shutdownDisposeDeadlineMs ?? SHUTDOWN_DISPOSE_DEADLINE_MS;
    this.#now = now ?? (() => performance.now());
    this.#createAgentSessionImpl = createAgentSessionImpl ?? createAgentSession;
    this.#live = new LiveSessionRegistry<HostSession>({ now: this.#now });
    /** omp-native event channel (spec 05 §5.2, master D6-R1 single authority). */
    this.ompBus = new OmpEventBus();
    /** Personas (OC-original optional layer, spec 02 §5.2/R12). */
    this.personas = new Map();
    /** Per-directory keyed Settings store (spec 06 §5.1, master R6). */
    this.settingsStore = null;
    // Approval/ask dialog domain (spec 03, master R10/R11/R13). Lease-driven
    // hasUI: unattended sessions never hold a lease → SDK fail-closed.
    this.dialogs = createDomainDialogs({
      onSessionUiAttached: ({ directory, sessionId }) => {
        void this.#attachDialogUi(directory, sessionId).catch((error) => {
          console.warn('[omp-host] failed to attach dialog UI:', errorText(error));
        });
      },
      onSessionUiDetached: ({ directory, sessionId }) => this.#detachDialogUi(directory, sessionId),
      // Plan §4.2: every lease acquire (attach or heartbeat renew) refreshes
      // the owning live record's idle TTL — "a viewer is attached" keeps the
      // session resident, and the 30-minute clock only starts counting when
      // the last holder leaves or its heartbeat lapses.
      onLeaseAcquired: ({ directory, sessionId }) => {
        const hostSession = this.#liveHostAnywhere(directory, sessionId);
        if (!hostSession) return;
        const record = this.#live.byKey(hostSession.key);
        if (record && record.state === 'live') this.#live.touch(record);
      },
      onDiagnostic: (note) => console.warn('[omp-host] dialog lifecycle:', note)
    });
    // Modes/plan/goal/personas/agent-definitions domain (spec 02).
    this.modesDomain = createModesDomain({
      publishFor:
        (sessionId, directoryKey) =>
        (type, payload, options = {}) =>
          this.ompBus.publish(type, payload, {
            directory: directoryKey,
            sessionID: sessionId,
            durable: options.durable !== false
          }),
      appendFor: (sessionId, directoryKey) => (mode, data) => {
        const hostSession = this.#liveHostAnywhere(directoryKey, sessionId);
        const manager = hostSession?.agentSession?.sessionManager;
        if (!manager?.appendModeChange) return undefined;
        const entryId = manager.appendModeChange(mode, data);
        if (hostSession) this.#syncPlanProposalHandler(hostSession, mode);
        return entryId;
      },
      sessionContextFor: (sessionId, directoryKey) => {
        const hostSession = this.#liveHostAnywhere(directoryKey, sessionId);
        try {
          return hostSession?.agentSession?.sessionManager?.buildSessionContext?.();
        } catch {
          return undefined;
        }
      },
      // omp agent discovery chain as the definitions authority (02 §5.2):
      // reads come from discoverAgents (project > user > extensions >
      // bundled), writes are .md files in the user/project agents dirs.
      agentDefinitions: {
        discover: (directory) => discoverAgents(directory ?? process.cwd()),
        writeFile: async (filePath, content) => {
          await fs.promises.mkdir(path.dirname(filePath), { recursive: true });
          await fs.promises.writeFile(filePath, content, 'utf8');
        },
        deleteFile: async (filePath) => {
          try {
            await fs.promises.unlink(filePath);
            return true;
          } catch {
            return false;
          }
        },
        readFile: async (filePath) => fs.promises.readFile(filePath, 'utf8'),
        // Hot reload (02 §5.2 refresh): the SDK memoizes create-time discovery
        // per cwd and every task tool advertises that list to the model;
        // refreshAgentDiscovery republishes the fresh set to live sessions.
        onDefinitionsChanged: (directory) => refreshAgentDiscovery(directory ?? process.cwd()),
        // Reveal in file manager (plugins.v1 parity): reuse the plugins
        // domain's platform builder instead of a second opener implementation.
        revealFile: async (filePath) => {
          const { execFile } = await import('node:child_process');
          const { promisify } = await import('node:util');
          const { command, args } = revealCommand(process.platform, filePath);
          await promisify(execFile)(command, args, { windowsHide: true });
        },
        userAgentsDir: this.#userAgentsDir(),
        projectAgentsDirFor: (directory) => path.join(path.resolve(directory), '.omp', 'agents')
      },
      personasStore: mapBackedStore(this.personas, () => this.savePersonas()),
      allowedTools: new Set(Object.keys(BUILTIN_TOOLS ?? {})),
      settingsProjectScopes: true,
      // Effective task.* override read for the definitions projection
      // (02 §5.2): the keyed Settings merged view per directory.
      overridesFor: async (directoryKey) => {
        const store = this.settingsStore;
        if (!store?.settingsFor) return null;
        try {
          const settings = await store.settingsFor(directoryKey ?? undefined);
          return {
            disabledAgents: settings.get('task.disabledAgents'),
            modelOverrides: settings.get('task.agentModelOverrides'),
            prewalk: settings.get('task.agentPrewalk'),
            advisor: settings.get('task.agentAdvisor')
          };
        } catch {
          return null;
        }
      }
    });
    // Extension chrome table (spec 09 §5): string-payload widget/status
    // projection mirroring RpcExtensionUIRequest. Volatile events; the
    // snapshot GET is the reconnect authority (D2).
    this.chrome = createDomainChrome({
      publishFor: (directory, payload) =>
        this.ompBus.publish('omp.chrome.updated', payload, {
          directory,
          durable: false
        })
    });
    // URI bridge / session tree / agent-runs / jobs (spec 04). The factory
    // is synchronous and every engine dependency is a lazy closure, so it is
    // created here (not in async #boot) and mounted synchronously by
    // endpoints.js at route-registration time.
    this.uriDomain = createUriDomain({
      features: () => ompFeatures(),
      localOptionsFor: async (sessionId, directoryKey) => {
        const artifactsDir = await this.#artifactsDirFor(sessionId, directoryKey);
        return artifactsDir ? createLocalProtocolOptions(sessionId, directoryKey, artifactsDir) : null;
      },
      sessionTreeData: async (directory) => this.listSessions({ directory: directory ?? undefined }),
      // Cold entry tree (plan §7.1): build the snapshot inside withColdManager
      // so the cold manager is closed and its entries mirror released on
      // every path — the old contract handed a raw manager to the URI domain
      // and never closed it.
      entryTreeFor: async (sessionID, directory) => {
        const directoryKey = normalizeDirectoryKey(directory);
        const file = await this.#findSessionFile(sessionID, directoryKey);
        if (!file) return null;
        const tree = await withColdManager(file.path, (manager) =>
          buildEntryTreeSnapshot({ sessionID, directory: directoryKey, manager }),
        );
        return { tree };
      },
      localFiles: (sessionID, directory) =>
        this.#listLocalFiles(sessionID, normalizeDirectoryKey(directory)),
      agentsSnapshot: () => {
        // Per-session registries carry each live session's own agent ref;
        // subagent refs land in the SDK's process-global registry instead
        // (task executor uses AgentRegistry.global() throughout), so both
        // sources must feed the aggregator. Global refs map back to a live
        // session via the sessionFile layout `<ts>_<sessionID>/<agent>.jsonl`.
        const records = this.#live
          .snapshot()
          .filter((record) => record.state === 'live' && record.payload);
        const entries: AgentsSnapshotEntry[] = records.map((record) => ({
          sessionID: record.sessionId,
          directory: record.directory,
          registry: record.payload!.agentRegistry
        }));
        for (const ref of AgentRegistry.global?.().list() ?? []) {
          const sessionID = sessionIDFromSessionFile(ref.sessionFile);
          if (!sessionID) continue;
          const owner = records.find((record) => record.sessionId === sessionID);
          if (!owner) continue; // cold/parked runs surface via diskScan
          const entry = entries.find((candidate) => candidate.directory === owner.directory);
          if (!entry) continue;
          (entry.refs ??= []).push(ref);
        }
        return entries;
      },
      publish: (type, payload, scope) => this.ompBus.publish(type, payload, scope),
      liveSessionIds: () =>
        this.#live.snapshot().filter((record) => record.state === 'live' || record.state === 'materializing').map((record) => record.sessionId),
      childSessionIdFor: (sessionFile) => this.#childSessionIdCached(sessionFile),
      diskScan: (directory) => (this.#diskRowsByDirectory.get(normalizeDirectoryKey(directory)) ?? null)?.map(({ file, ...row }) => row) ?? null,
      warmDiskScan: (directory) => this.#ensureDiskRows(normalizeDirectoryKey(directory)),
    });
    // The task executor registers every subagent into the SDK's process-global
    // registry (AgentRegistry.global()), not the per-session registries above.
    // One engine-wide subscription keeps the aggregator current for subagent
    // spawn/status/activity changes across every live session.
    // The process-global registry is an additional subagent-ref source; a
    // minimal/embedded SDK surface may omit it — per-session registries
    // still feed the aggregator on their own.
    const globalRegistry = AgentRegistry.global?.();
    this.unsubscribeGlobalRegistry = globalRegistry?.onChange((event) => {
      // A newly registered run owns a fresh transcript: drop the one-shot
      // disk-row cache for its directory so a later listing can discover it
      // (status/activity churn must not trigger rescans). Rehydration
      // registrations land here too, extending the same invalidation.
      if (event.type === 'registered') this.#invalidateDiskRowsFor(event.ref.sessionFile);
      // A run ref that just left the registry while its owning session is
      // still live (idle-TTL park, one-shot settle, corpse reclaim) would
      // drop its row mid-view: re-register it from the transcript so the
      // viewer never sees it disappear.
      if (event.type === 'removed' && event.ref.kind !== 'advisor') this.#rehydrateRemovedRun(event.ref.sessionFile);
      // Warm the childSessionID cache for new transcripts (one open per file),
      // then refresh: warmed rows re-publish with childSessionID set so the
      // UI drill-in can open the run's read-only session view.
      void this.#warmChildSessionIds().then(() => {
        this.uriDomain?.aggregator.refresh();
      });
      this.uriDomain?.aggregator.refresh();
    });
    // Session process monitor: the platform adapter reuses the pi-natives
    // addon the SDK loaded at import time, so the ledger is ready before the
    // first request; without a resident addon the domain stays unavailable.
    const processPlatform = createProcessPlatform();
    this.processLedger = processPlatform
      ? new ProcessLedger({
          platform: processPlatform,
          cpuCores: os.cpus().length,
          runningJobIds: () => this.#runningAsyncJobIds(),
          cancelJob: (jobId, sessionID) => this.#cancelAsyncJob(jobId, sessionID),
          publishUpdate: (directory) => {
            const ledger = this.processLedger;
            if (!ledger) return;
            this.ompBus.publish('omp.processes.updated', { revision: ledger.revision }, { directory, durable: true });
          }
        })
      : null;
    this.processDomain = createProcessDomain({
      features: () => ompFeatures(),
      ledger: () => this.processLedger
    });
    this.bootError = null;
    this.bootPromise = null;
    this.sweeper = setInterval(() => this.#sweepIdleSessions(), IDLE_SWEEP_INTERVAL_MS);
    this.sweeper.unref?.();
  }
  /**
   * 一次性引导（幂等，bootPromise 去重）：发现认证存储、创建并刷新模型
   * 注册表、加载 personas、执行 sidecar 到 omp agent 的迁移（失败不阻塞）、
   * 创建按目录的 Settings 存储（失败降级为 null）。失败记录 bootError、
   * 清空 bootPromise 并抛出，下一个调用者可重试。
   */
  async #boot() {
    if (this.bootPromise) return this.bootPromise;
    this.bootPromise = (async () => {
      this.authStorage = await discoverAuthStorage(this.registry.agentDir);
      this.modelRegistry = new ModelRegistry(this.authStorage);
      await this.modelRegistry.refresh();
      this.#loadPersonas();
      // Sidecar → omp agent migration (02 §6.2) runs before the request
      // surface opens; failure keeps the sidecar and never blocks boot.
      await this.#migrateAgentsSidecar().catch((error) => {
        console.warn('[omp-host] agent sidecar migration failed:', errorText(error));
      });
      // Per-directory keyed Settings store (spec 06 §5.1, master R6). The
      // boot instance doubles as the global-write executor; sessions inject
      // their directory's instance via options.settings (sdk.ts:1273-1275).
      if (!this.settingsStore) {
        try {
          this.settingsStore = await createSettingsStore({
            cwd: process.cwd(),
            agentDir: this.registry.agentDir
          });
        } catch (error) {
          // Degrade to no-injection (pre-R6 behavior) instead of bricking
          // every session; the settings endpoints surface the error.
          console.warn('[omp-host] settings store unavailable:', errorText(error));
          this.settingsStore = null;
        }
      }
    })();
    try {
      await this.bootPromise;
    } catch (error) {
      this.bootError = error;
      this.bootPromise = null;
      throw error;
    }
    return this.bootPromise;
  }

  /**
   * 为会话设置或清除对话 UI 上下文：hasUI 为 true 时构造 dialogs 领域的
   * uiContext（挂 chrome 桥），并保证扩展运行时只初始化一次（首次挂 UI 时
   * initializeExtensions，此后复用）；hasUI 为 false 时传 undefined。最后经
   * #applyToolUiContext 把上下文注入 SDK。
   */
  async #setDialogUiContext(hostSession: HostSession, directory: string, sessionId: string, hasUI: boolean) {
    const uiContext = hasUI
      ? this.dialogs.uiContextFor(directory, sessionId, {
          chrome: this.chrome.bridgeHandlersFor(directory, sessionId)
        })
      : undefined;
    if (hasUI && !hostSession.extensionUiInitialized) {
      if (!hostSession.extensionUiPromise && hostSession.agentSession) {
        hostSession.extensionUiPromise = initializeExtensions(hostSession.agentSession, {
          uiContext: asExtensionUiContext(uiContext),
          mode: 'json',
          reportSendError: (action, error) => {
            console.warn(`[omp-host] ${action} failed:`, errorText(error));
          },
          reportRuntimeError: (error) => {
            console.warn('[omp-host] extension runtime error:', error?.error ?? error);
          },
          onShutdown: () => {}
        })
          .then(() => {
            hostSession.extensionUiInitialized = true;
          })
          .finally(() => {
            hostSession.extensionUiPromise = null;
          });
      }
      await hostSession.extensionUiPromise;
    }
    // SAFETY: DialogBridge is the deliberate web degradation of ExtensionUIContext
    // (asExtensionUiContext seam); the SDK consumes the same subset and treats
    // an undefined context as "no UI" when the lease is absent.
    this.#applyToolUiContext(hostSession.sdkResult, asExtensionUiContext(uiContext), hasUI);
  }

  /**
   * Lease attach/detach → SDK tool UI context (R13: lease is hasUI
   * authority). A UI lease IS a session access: a client viewing the session
   * implies the engine should hold it live, so an attach that races ahead of
   * lazy materialization pulls the session in instead of dropping the
   * extension UI initialization on the floor.
   */
  /** 租约挂载/卸载映射到 SDK 工具 UI 上下文（R13：租约是 hasUI 权威）。UI 租约即一次会话访问：领先于惰性物化的挂载会把会话拉进来，而不是丢掉扩展 UI 初始化。 */
  #applyToolUiContext(sdkResult: Pick<CreateAgentSessionResult, 'setToolUIContext'> | undefined, uiContext: ReturnType<typeof asExtensionUiContext>, hasUI: boolean): void {
    // SAFETY: web degradation seam — undefined means "no UI bridge" and
    // pairs with hasUI=false; the SDK member accepts it at runtime.
    (sdkResult?.setToolUIContext as ((uiContext: ExtensionUIContext | undefined, hasUI: boolean) => void) | undefined)?.(uiContext, hasUI);
  }

  /** 租约获得时挂 UI：找不到 live 记录则先物化会话，再设置 UI 上下文。 */
  async #attachDialogUi(directory: string, sessionId: string) {
    const hostSession = this.#liveHostAnywhere(directory, sessionId) ?? (await this.#materialize(sessionId, directory));
    if (!hostSession) return;
    await this.#setDialogUiContext(hostSession, directory, sessionId, true);
  }

  /** 租约失去时卸 UI：向 SDK 传 undefined 与 hasUI=false；会话可能已被 dispose，吞掉异常。 */
  #detachDialogUi(directory: string, sessionId: string) {
    const hostSession = this.#liveHostAnywhere(directory, sessionId);
    if (!hostSession) return;
    try {
      this.#applyToolUiContext(hostSession.sdkResult, undefined, false);
    } catch {
      // Session may already be disposed.
    }
  }

  /**
   * Plan mode ↔ xd://propose bridge (spec 02 §5.5): entering plan attaches
   * the review bridge; any other mode clears the handler.
   */
  /** plan 模式与 xd://propose 的桥（spec 02 §5.5）：进入 plan 模式挂评审桥；其它模式清除 handler。 */
  #syncPlanProposalHandler(hostSession: HostSession, mode: string) {
    const session = hostSession?.agentSession;
    if (!session?.setPlanProposalHandler) return;
    if (mode === 'plan') {
      const bridge = this.modesDomain.bridgeFor(hostSession.sessionId, hostSession.directory);
      // SAFETY: AgentSession satisfies PlanProposalSession (preparePlanForReview)
      // structurally; the mode domain narrows to the single member it calls.
      // The mode domain consumes only preparePlanForReview (PlanProposalSession);
      // delegate through an adapter so the SDK AgentSession keeps its own type.
      const planSession = {
        preparePlanForReview: async (title: string): Promise<PreparePlanReviewResult> => {
          // SAFETY: AgentToolResult<PlanApprovalDetails> IS the
          // PreparePlanReviewResult shape by design (02 §5.5): { content, details }.
          return (await session.preparePlanForReview(title)) as PreparePlanReviewResult;
        }
      };
      // SAFETY: PlanReviewToolResult is the AgentToolResult shape by design
      // (02 §5.5); the SDK handler and the mode hook return the same wire form.
      const hook = bridge.hookFor(planSession);
      // SAFETY: hook's PlanReviewToolResult is the handler's AgentToolResult arm.
      session.setPlanProposalHandler(hook as Parameters<NonNullable<typeof session.setPlanProposalHandler>>[0]);
      hostSession.planHandlerAttached = true;
    } else if (hostSession.planHandlerAttached) {
      session.setPlanProposalHandler(null);
      hostSession.planHandlerAttached = false;
    }
  }

  /** personas 持久化 JSON 的路径（注册表根目录下的 ompchamber-personas.json）。 */
  #personasConfigPath() {
    return path.join(this.registry.registryRoot, 'ompchamber-personas.json');
  }

  /** 从 sidecar JSON 加载 personas 到内存 Map；文件不存在或损坏时静默保持为空。 */
  #loadPersonas() {
    try {
      const parsed = JSON.parse(fs.readFileSync(this.#personasConfigPath(), 'utf8'));
      for (const persona of Array.isArray(parsed?.personas) ? parsed.personas : []) {
        if (persona && typeof persona.name === 'string') this.personas.set(persona.name, persona);
      }
    } catch {
      // No personas yet.
    }
  }

  /** 把内存中的 personas 全量写回 sidecar JSON（先确保注册表根目录存在）。 */
  savePersonas() {
    fs.mkdirSync(this.registry.registryRoot, { recursive: true });
    fs.writeFileSync(this.#personasConfigPath(), JSON.stringify({ personas: [...this.personas.values()] }, null, 2));
  }

  /** Public settings-store accessor for endpoint handlers. */
  /** 端点处理器使用的 settings 存储访问器：等待 #boot 完成后返回 store（降级 boot 时为 null）。 */
  async settingsStoreReady() {
    await this.#boot();
    return this.settingsStore;
  }

  /**
   * omp user-scope agents dir (SDK discovery order: `~/.omp/agent/agents`,
   * pi-utils getConfigDirs with source '.omp'). Falls back to the derived
   * path when config dirs are unavailable.
   */
  /** omp 用户级 agents 目录（SDK 发现顺序 ~/.omp/agent/agents，getConfigDirs 的 '.omp' 来源）；配置目录不可用时回退到推导路径。 */
  #userAgentsDir() {
    try {
      const entry = getConfigDirs('agents', { project: false }).find((dir) => dir?.source === '.omp' && typeof dir?.path === 'string');
      if (entry) return entry.path;
    } catch {
      // Derived fallback below.
    }
    return path.join(os.homedir(), '.omp', 'agent', 'agents');
  }

  /**
   * One-time sidecar → omp migration (02 §6.2): each legacy
   * `ompchamber-agents.json` record becomes a user-scope worker `.md`
   * (frontmatter description/tools, body prompt) plus a mirrored persona so
   * existing `meta.agent` sessions keep resolving. Runs before the request
   * surface opens; any failure keeps the sidecar for an idempotent retry.
   */
  /** 一次性 sidecar 到 omp 的迁移（02 §6.2）：把每条 ompchamber-agents.json 记录写成用户级 worker .md（frontmatter 描述/工具、正文 prompt）并镜像一个 persona，使既有 meta.agent 会话继续解析。在请求表面打开前运行；任何失败保留 sidecar 以便幂等重试。 */
  async #migrateAgentsSidecar() {
    const sidecarPath = path.join(this.registry.registryRoot, 'ompchamber-agents.json');
    const userAgentsDir = this.#userAgentsDir();
    let done = false;
    const result = await migrateSidecarAgents({
      loadRecords: () => {
        const parsed = JSON.parse(fs.readFileSync(sidecarPath, 'utf8'));
        return Array.isArray(parsed?.agents) ? parsed.agents : [];
      },
      agentExists: async (name) => {
        const { agents } = await discoverAgents(process.cwd());
        return agents.some((agent) => agent?.name === name);
      },
      writeAgent: async (record) => {
        await fs.promises.mkdir(userAgentsDir, { recursive: true });
        await fs.promises.writeFile(
          path.join(userAgentsDir, `${record.name}.md`),
          serializeAgentMarkdown({
            name: record.name,
            description: typeof record.description === 'string' && record.description.trim() ? record.description : record.name,
            systemPrompt: typeof record.prompt === 'string' ? record.prompt : '',
            ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {})
          }),
          'utf8'
        );
      },
      personaExists: (name) => this.personas.has(name),
      mirrorPersona: (record) => {
        this.personas.set(record.name, {
          name: record.name,
          ...(record.description ? { description: record.description } : {}),
          ...(typeof record.prompt === 'string' && record.prompt ? { systemPrompt: record.prompt } : {}),
          ...(Array.isArray(record.tools) && record.tools.length > 0 ? { tools: record.tools } : {})
        });
      },
      markDone: () => {
        try {
          fs.renameSync(sidecarPath, `${sidecarPath}.migrated-${Date.now()}`);
          this.savePersonas();
          done = true;
        } catch (error) {
          console.warn('[omp-host] sidecar migration markDone failed:', errorText(error));
        }
      },
      log: (message, error) => console.warn('[omp-host] agent sidecar migration:', message, error ?? '')
    });
    if (done && result.migrated > 0) {
      console.log(`[omp-host] migrated ${result.migrated} sidecar agent(s) to ${userAgentsDir} (+persona mirrors)`);
    }
    return result;
  }

  /**
   * Idle reaper (plan §4): every live record idle beyond the TTL is a
   * candidate — there is no live-count gate (quantity is not a memory
   * bound). Candidate selection happens outside the gate, but the evict
   * decision re-reads state, TTL, inFlight, leases, pending dialogs and
   * every SDK activity signal INSIDE the per-key gate, with no unprotected
   * await between the checks and beginDispose.
   */
  /** 空闲回收器（plan §4）：超过 TTL 的 live 记录均为候选（没有 live 数量门槛）；逐出决策在每键门内重读全部守卫（状态、TTL、inFlight、租约、待处理对话、SDK 活动信号）后做出，检查与 beginDispose 之间没有未受保护的 await。同时按冷却期重试 failed 墓碑的 dispose，并做周期性 token 清扫。 */
  #sweepIdleSessions() {
    const now = this.#now();
    for (const record of this.#live.snapshot()) {
      if (record.state === 'live') {
        if (now - record.lastUsedAt < IDLE_SESSION_TTL_MS) continue;
        void this.#live
          .withOperation(record.key, async () => {
            const current = this.#live.byKey(record.key);
            if (!current || current !== record || current.state !== 'live') return;
            if (this.#now() - current.lastUsedAt < IDLE_SESSION_TTL_MS) return;
            if (current.inFlight > 0) return;
            if (this.#recordIsActive(current)) return;
            this.#evictRecord(current, 'idle-ttl');
          })
          .catch((error) => {
            console.warn('[omp-host] idle sweep error:', errorText(error));
          });
        continue;
      }
      if (record.state !== 'failed') continue;
      const failure = record.failure;
      if (!failure || failure.attempts >= FAILED_DISPOSE_MAX_ATTEMPTS) continue;
      if (now - failure.at < FAILED_DISPOSE_RETRY_COOLDOWN_MS) continue;
      void this.#live
        .withOperation(record.key, async () => {
          const current = this.#live.byKey(record.key);
          if (!current || current !== record || current.state !== 'failed') return;
          await this.#retryFailedDispose(current);
        })
        .catch((error) => {
          console.warn('[omp-host] failed-dispose retry error:', errorText(error));
        });
    }
    // Periodic transport hygiene (plan §6): tokens expire even without mints.
    this.uriDomain?.tokens?.sweep?.();
  }

  /**
   * Bounded re-dispose of a quarantined record (plan §3.4 recovery rule):
   * re-enters `evicting` — the key stays blocked to writers — then runs the
   * retained `retryDispose` closure detached on `record.disposePromise`,
   * exactly like #evictRecord, so a hung drain cannot occupy the gate. A
   * tombstone with nothing retained simply releases; a settle failure
   * re-tombs with a bumped attempt count.
   */
  /** 隔离记录的有界重 dispose（plan §3.4 恢复规则）：重新进入 evicting（键对写方保持关闭），再脱离门运行保留的 retryDispose 闭包——与 #evictRecord 一致，挂死的排空不会占用门。无保留闭包的墓碑直接释放；结算失败则重新入墓并累加尝试次数。 */
  async #retryFailedDispose(record: LiveRecord<HostSession>): Promise<void> {
    if (!this.#live.retryEvict(record)) return;
    const retry = record.retryDispose;
    if (!retry) {
      this.#live.finishEvict(record, true);
      return;
    }
    record.disposePromise = (async (): Promise<'disposed' | 'failed'> => {
      try {
        await retry();
        this.#live.finishEvict(record, true);
        return 'disposed';
      } catch (error) {
        console.warn(`[omp-host] session ${record.sessionId} disposal retry failed:`, errorText(error));
        this.#live.finishEvict(record, false, errorText(error));
        return 'failed';
      }
    })();
    await this.#awaitDisposalBounded(record);
  }

  /** Interval target; public so the lifecycle tests drive it deterministically. */
  /** 定时器目标函数的公开包装：生命周期测试用它确定性地驱动清扫。 */
  sweepIdleSessionsNow() {
    this.#sweepIdleSessions();
  }

  /** Full activity guard set (plan §4.1) read from the live SDK session. */
  /** 从 live SDK 会话读取的完整活动守卫集合（plan §4.1）：流式、中止、重试、压缩、交接、bash、eval、待处理消息、post-prompt 工作、排队消息、待处理异步、待处理对话框、UI 租约——任一命中即活跃（不可逐出）。 */
  #recordIsActive(record: LiveRecord<HostSession>): boolean {
    const hostSession = record.payload;
    if (!hostSession) return false;
    if (hostSession.awaitingAsyncSince !== null) return true;
    const session = hostSession.agentSession;
    if (!session) return false;
    if (
      session.isStreaming ||
      session.isAborting ||
      session.isRetrying ||
      session.isCompacting ||
      session.isGeneratingHandoff ||
      session.isBashRunning ||
      session.isEvalRunning ||
      session.hasPendingBashMessages ||
      session.hasPendingPythonMessages ||
      session.hasPostPromptWork ||
      session.queuedMessageCount > 0
    ) {
      return true;
    }
    try {
      if (session.hasPendingAsyncWork()) return true;
    } catch {
      // Getter must not veto eviction by throwing.
    }
    if (this.dialogs.registry.pendingCount({ directory: record.directory, sessionId: record.sessionId }) > 0) {
      return true;
    }
    if (this.dialogs.hasUISnapshotFor(record.directory, record.sessionId).holders > 0) {
      return true;
    }
    return false;
  }

  /**
   * Tear the host side off a record: subscriptions, domain handles, UI.
   * Every release settles independently (plan §3.3 step 6): a throwing
   * unsubscribe must not skip the remaining teardown or — worse — abort
   * #evictRecord before the disposal promise exists, which would strand an
   * evicting record that can never finish.
   */
  /** 拆掉记录的宿主侧句柄：订阅、领域句柄、UI。每个释放独立结算（plan §3.3 步骤 6）：抛错的 unsubscribe 不得跳过其余拆解，更不得在 dispose promise 出现前中止 #evictRecord——那会搁浅一条永远无法完成的 evicting 记录。 */
  #releaseHostHandles(hostSession: HostSession): void {
    const failures: string[] = [];
    const attempt = (release: () => void) => {
      try {
        release();
      } catch (error) {
        failures.push(errorText(error));
      }
    };
    attempt(() => {
      hostSession.unsubscribe?.();
      hostSession.unsubscribe = undefined;
    });
    attempt(() => {
      hostSession.nameUnsubscribe?.();
      hostSession.nameUnsubscribe = undefined;
    });
    attempt(() => {
      hostSession.unsubscribeAgentRegistry?.();
      hostSession.unsubscribeAgentRegistry = undefined;
    });
    hostSession.extensionUiPromise = null;
    hostSession.extensionUiInitialized = false;
    const { sessionId, directory } = hostSession;
    attempt(() => this.modesDomain?.release?.(sessionId, directory));
    attempt(() => this.dialogs?.releaseSession?.(directory, sessionId, 'session disposed'));
    attempt(() => this.uriDomain?.descriptors?.releaseForSession?.(directory, sessionId));
    attempt(() => this.uriDomain?.aggregator?.releaseForSession?.(directory, sessionId));
    // Client-echoed user wire ids need no explicit release here: they live on
    // the HostSession record (plan phase 5 compact mapping) and die with it.
    if (failures.length > 0) {
      console.warn(`[omp-host] partial host-handle release failure for ${sessionId}:`, failures.join('; '));
    }
  }

  /** 解析会话的 artifacts 目录：live 会话优先其 SessionManager 的当前值；否则从冷转录文件路径推导，找不到文件时返回 null。 */
  async #artifactsDirFor(sessionId: string, directoryKey: string) {
    const manager = this.#liveHostAnywhere(directoryKey, sessionId)?.agentSession?.sessionManager;
    const liveDir = manager?.getArtifactsDir?.();
    if (typeof liveDir === 'string' && liveDir) return liveDir;
    const file = await this.#findSessionFile(sessionId, directoryKey);
    return file ? artifactsDirForSessionFile(file.path) : null;
  }

  /** Release per-directory host state when the directory goes quiet (plan §6). */
  /** 目录静默（无任何 live 记录）时释放按目录的宿主状态：chrome 目录句柄与 sidecar 元数据内存缓存——磁盘仍是权威，下次访问重新加载。 */
  #maybeReleaseDirectoryState(directory: string): void {
    if (this.#live.liveDirectories().has(normalizeDirectoryKey(directory))) return;
    this.chrome?.releaseDirectory?.(directory);
    // Sidecar meta map: disk stays authoritative; the next access reloads.
    this.registry.release(directory);
  }

  /**
   * Evict one live record (plan §3.4). MUST run inside the record's
   * operation gate. Order: beginDispose (sync, before the first await) →
   * host teardown → session.idle → the single saved dispose promise.
   *
   * The host-side transition is SYNCHRONOUS: by the time this returns, the
   * record is `evicting` with its unsubscribe/domain handles torn off and
   * the SDK disposal running detached. Callers decide whether — and how
   * long — to await the returned promise; awaiting it inside a gate body
   * would let a never-settling disposal occupy the gate forever.
   */
  /** 逐出一条 live 记录（plan §3.4）。必须在记录的操作门内运行。顺序：beginDispose（同步，先于首个 await）→ 宿主拆解 → session.idle 事件 → 唯一的已保存 dispose promise。宿主侧转换是同步的：返回时记录已是 evicting、句柄已拆、SDK dispose 在后台运行；是否等待、等多久由调用方决定。 */
  #evictRecord(
    record: LiveRecord<HostSession>,
    reason: string,
    { emitIdle = true }: { emitIdle?: boolean } = {},
  ): Promise<'disposed' | 'failed'> {
    if (!this.#live.beginEvict(record)) {
      return record.disposePromise ?? Promise.resolve('disposed');
    }
    const hostSession = record.payload;
    const agentSession = hostSession?.agentSession ?? null;
    record.payload = null;
    try {
      // Step 1: reject new SDK work before the host's first await.
      agentSession?.beginDispose?.();
    } catch {
      // beginDispose must not block teardown when it throws.
    }
    if (hostSession) {
      // Step 2: host handles come off synchronously; late events after this
      // point can only land on an already-evicting record.
      this.#releaseHostHandles(hostSession);
      hostSession.agentSession = null;
      this.#maybeReleaseDirectoryState(hostSession.directory);
    }
    if (emitIdle) {
      // Step 3: clients learn the session left live state without waiting
      // for a possibly slow SDK drain.
      this.bus.emit('session.idle', { sessionID: record.sessionId }, record.directory);
    }
    if (!agentSession) {
      this.#live.finishEvict(record, true);
      if (emitIdle) this.#emitColdSessionUpdated(record);
      return Promise.resolve('disposed');
    }
    // Step 4: one saved, detached dispose promise; its settle (success or
    // failure) is the only thing that can move the record out of `evicting`.
    // Failed disposal → observable quarantine tombstone; the SDK object
    // stays referenced by this promise chain so the same file cannot gain a
    // second writer (plan §3.4). errorText bounds the rejection to a
    // message string before it reaches domain state.
    const disposePromise = (async (): Promise<'disposed' | 'failed'> => {
      try {
        await agentSession.dispose({ drainTimeoutMs: this.evictDrainTimeoutMs });
        this.#live.finishEvict(record, true);
        if (emitIdle) this.#emitColdSessionUpdated(record);
        return 'disposed';
      } catch (rejection) {
        console.warn(`[omp-host] session ${record.sessionId} disposal failed (${reason}):`, errorText(rejection));
        this.#live.finishEvict(record, false, errorText(rejection));
        return 'failed';
      }
    })();
    record.disposePromise = disposePromise;
    // The failed tombstone keeps a re-dispose path (plan §3.4 recovery): the
    // closure retains the SDK object so the sweeper's cooldown-gated retry
    // can finish a transient failure instead of quarantining until restart.
    record.retryDispose = async () => {
      await agentSession.dispose({ drainTimeoutMs: this.evictDrainTimeoutMs });
    };
    return disposePromise;
  }

  /**
   * After a record leaves the registry, clients holding its session row see
   * `live` flip to absent (cold) without waiting for a list refresh —
   * `session.updated` replaces the stored record wholesale.
   */
  /** 记录离开注册表后，持有该会话行的客户端无需等列表刷新即可看到 live 字段翻转为缺席（冷）——session.updated 会整体替换已存储的记录。 */
  #emitColdSessionUpdated(record: LiveRecord<HostSession>) {
    // A dispose that settled late must not clobber a newer live record for
    // the same id — directory moves and prompt-time rematerialization both
    // emit their own session.updated carrying the current state.
    if (this.#live.bySessionId(record.sessionId)) return;
    const meta = this.registry.get(record.directory, record.sessionId);
    const now = Date.now();
    this.bus.emit(
      'session.updated',
      {
        sessionID: record.sessionId,
        info: this.#wireSession(
          {
            id: record.sessionId,
            cwd: record.directory,
            title: meta?.title,
            created: new Date(meta?.timeCreated ?? now),
            modified: new Date(meta?.timeUpdated ?? now),
          },
          record.directory,
          meta ?? undefined,
        ),
      },
      record.directory,
    );
  }

  /** Bounded wait for a record's disposal (never parks on a hung drain). */
  /** 有界等待记录的 dispose 结算（超时为 2 倍排空预算），绝不挂在无限排空上；结算后清掉定时器避免拖住事件循环。 */
  #awaitDisposalBounded(record: LiveRecord<HostSession>): Promise<'disposed' | 'failed' | 'timeout'> {
    const disposal = record.disposePromise ?? Promise.resolve('disposed' as const);
    let timeoutTimer: ReturnType<typeof setTimeout> | undefined;
    const settled = Promise.race([
      disposal,
      new Promise((resolve) => {
        timeoutTimer = setTimeout(() => resolve('timeout' as const), this.evictDrainTimeoutMs * 2);
      }),
    ]);
    // The losing timer arm must not keep the event loop busy (or hold a
    // shutdown open) for the full drain window after disposal wins.
    return settled.finally(() => clearTimeout(timeoutTimer)) as Promise<'disposed' | 'failed' | 'timeout'>;
  }

  /**
   * Directory-keyed live lookup (plan §3.2): the requested directory must
   * own the record. Cross-directory same-id records never satisfy each other.
   */
  /** 按目录键的 live 查找（plan §3.2）：请求目录必须拥有该记录；跨目录同 id 记录互不满足。 */
  #liveHost(directory: string | null | undefined, sessionId: string): HostSession | null {
    if (!directory) return null;
    return this.#live.getLive(directory, sessionId)?.payload ?? null;
  }

  /**
   * Live lookup when the caller's directory may differ from the owning one
   * (getSession/updateSession semantics: a live session owns its registry
   * entry and answers first). The exact-key hit is preferred; the id-only
   * fallback returns the unique live record and refuses the ambiguous
   * two-directories case instead of guessing (plan §3.2).
   */
  /** 调用方目录可能与拥有目录不同时的 live 查找（getSession/updateSession 语义：live 会话拥有其注册表行并优先应答）。先精确键命中；退化为仅按 id 返回唯一 live 记录，两目录歧义时返回 null 而不是猜测。 */
  #liveHostAnywhere(directory: string | null | undefined, sessionId: string): HostSession | null {
    const keyed = this.#liveHost(directory, sessionId);
    if (keyed) return keyed;
    const byId = this.#live.bySessionId(sessionId);
    return byId && byId.state === 'live' ? byId.payload : null;
  }

  /** 由 cwd 推导该项目的 sessions 根目录（SDK SessionManager 布局，可指定 agentDir）。 */
  #sessionDirFor(cwd: string) {
    return SessionManager.getDefaultSessionDir(cwd, this.registry.agentDir);
  }

  /** 目录键派生稳定项目 id：sha256 前 20 位十六进制加 prj_ 前缀。 */
  #projectId(directoryKey: string) {
    const hash = crypto.createHash('sha256').update(directoryKey).digest('hex');
    return `prj_${hash.slice(0, 20)}`;
  }
  /** Bounded walk depth for #listLocalFiles — local:// roots are shallow
   *  (plans, handoff notes, scratch); anything deeper is a runaway, not data. */
  /** #listLocalFiles 的目录行走深度上限——local:// 根很浅（plans、handoff、scratch）；更深的层级是失控而不是数据。 */
  static #LOCAL_WALK_MAX_DEPTH = 8;

  /**
   * Read-only file rows for one session's local:// root (artifacts browse,
   * spec 04). Returns null when the session is unknown to the directory;
   * an absent root is authoritative empty. Pure stat walk — no content
   * leaves this method; refs are '/'-joined relatives, never absolute paths.
   */
  /** 只读列出某会话 local:// 根下的文件行（artifacts 浏览，spec 04）。会话对该目录未知返回 null；根不存在是权威的空集。纯 stat 行走——任何内容都不离开本方法；ref 为以 '/' 连接的相对路径，绝不泄漏绝对路径。 */
  async #listLocalFiles(sessionId: string, directoryKey: string) {
    const artifactsDir = await this.#artifactsDirFor(sessionId, directoryKey);
    if (!artifactsDir) return null;
    const options = createLocalProtocolOptions(sessionId, directoryKey, artifactsDir);
    const root = resolveLocalUrlToPath('local://', options);
    const files: Array<{ ref: string; size?: number; modifiedAt?: number }> = [];
    let truncated = false;
    const walk = async (relative: string, depth: number) => {
      let entries;
      try {
        entries = await fs.promises.readdir(relative ? path.join(root, relative) : root, {
          withFileTypes: true,
        });
      } catch (error) {
        if (errorCode(error) === 'ENOENT') return; // no local root yet — authoritative empty
        throw error;
      }
      for (const entry of entries) {
        const childRef = relative ? `${relative}/${entry.name}` : entry.name;
        if (entry.isDirectory()) {
          if (depth >= OmpHostEngine.#LOCAL_WALK_MAX_DEPTH) {
            truncated = true;
            continue;
          }
          await walk(childRef, depth + 1);
        } else if (entry.isFile()) {
          if (files.length >= ARTIFACTS_MAX_FILES_PER_SESSION) {
            truncated = true;
            return;
          }
          const stat = await fs.promises.stat(path.join(root, childRef)).catch((): null => null);
          files.push({
            ref: childRef,
            size: stat?.size ?? 0,
            modifiedAt: stat?.mtimeMs ?? 0,
          });
        }
      }
    };
    await walk('', 0);
    return { files, truncated };
  }

  /**
   * Wire Session record for an omp SessionInfo + registry metadata.
   */
  /** 把 omp SessionInfo 与注册表元数据投影为 wire Session 记录：合并 live 状态、persona、模型（live 会话实际模型优先于 sidecar 投影）与转录字节数。 */
  #wireSession(info: SessionListInfo, directoryKey: string, meta: SessionMeta | undefined, live?: HostSession | undefined): WireSessionRecord {
    // Any non-cold record (materializing through failed tombstone) means the
    // transcript still occupies memory; absent record = cold.
    const record = this.#live.get(normalizeDirectoryKey(info.cwd || directoryKey), info.id);
    // The live session's actual model wins over the sidecar projection — a
    // roles-resolved session (no registry selector) still reports the model
    // it is really running (spec 01 §5.5 badge seeding).
    const selector = meta?.model
      ? splitModelSelector(meta.model)
      : live?.agentSession?.model
        ? {
            providerID: live.agentSession.model.provider,
            modelID: live.agentSession.model.id
          }
        : null;
    return {
      id: info.id,
      slug: info.id,
      projectID: this.#projectId(directoryKey),
      directory: normalizeDirectoryKey(info.cwd || directoryKey),
      parentID: meta?.parentID,
      // Fork lineage rides a dedicated field: wire `parentID` means subagent
      // parentage and the shared UI makes parentID sessions read-only
      // ("subagent sessions cannot be prompted"). A user fork must stay a
      // normal promptable session.
      forkParentID: meta?.forkParentID,
      title: meta?.title ?? info.title ?? 'Untitled',
      ...(personaKeyFor(meta?.persona ?? meta?.agent) !== 'standard' ? { agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)) } : {}),
      ...(selector ? { model: { id: selector.modelID, providerID: selector.providerID } } : {}),
      ...(meta?.metadata ? { metadata: meta.metadata } : {}),
      ...(record ? { live: record.state } : {}),
      ...(() => {
        const transcriptBytes = record?.payload?.fileSignature?.size ?? info.size;
        return Number.isFinite(transcriptBytes) ? { transcriptBytes } : {};
      })(),
      time: {
        created: info.created instanceof Date ? info.created.getTime() : Date.parse(info.created) || Date.now(),
        updated: info.modified instanceof Date ? info.modified.getTime() : Date.parse(info.modified) || Date.now(),
        ...(meta?.timeArchived ? { archived: meta.timeArchived } : {})
      }
    };
  }

  /** 列出一个目录的会话（wire 记录）：磁盘转录加上仅注册表会话（转录被外部清理时仍列出，保留删除/归档簿记）。subagent 运行不进入列表——只读下钻走 getSession/getMessagesPage 的 subagent 解析。 */
  async listSessions({ directory }: { directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const cwd = directory ?? '';
    const infos = await SessionManager.list(cwd, this.#sessionDirFor(cwd), undefined);
    const metas = this.registry.entries(directoryKey);
    const out = [];
    const seen = new Set();
    for (const info of infos) {
      seen.add(info.id);
      out.push(this.#wireSession(info, directoryKey, metas.get(info.id), this.#liveHost(directoryKey, info.id) ?? undefined));
    }
    // Registry-only sessions (omp transcript pruned externally) stay listed so
    // deletion/archival bookkeeping keeps working.
    for (const [id, meta] of metas) {
      if (seen.has(id)) continue;
      out.push(
        this.#wireSession(
          {
            id,
            cwd: directory ?? '',
            title: meta.title,
            created: new Date(meta.timeCreated ?? Date.now()),
            modified: new Date(meta.timeUpdated ?? Date.now())
          },
          directoryKey,
          meta
        )
      );
    }
    // Subagent runs no longer join the session list (maintainer ruling: the
    // sidebar stays host-sessions-only). Their conversations remain readable
    // through getSession/getMessagesPage's subagent resolution for the
    // read-only drill-in.
    return out;
  }


  /** 跨所有目录列出会话，按目录键分组返回 Map；archived=false 时过滤已归档行。 */
  async listAllSessions({ archived }: { archived?: boolean } = {}) {
    await this.#boot();
    const infos = await SessionManager.listAll();
    const byDirectory = new Map();
    for (const info of infos) {
      const directoryKey = normalizeDirectoryKey(info.cwd);
      const meta = (this.registry.get(directoryKey, info.id) ?? undefined);
      if (archived === false && meta?.timeArchived) continue;
      const list = byDirectory.get(directoryKey) ?? [];
      list.push(this.#wireSession(info, directoryKey, meta));
      byDirectory.set(directoryKey, list);
    }
    return byDirectory;
  }

  /** 创建空会话：落一个空转录文件、写入注册表元数据（标题、父会话、persona、模型选择器），广播 session.created 并返回 wire 记录。 */
  async createSession({ directory, title, parentID, agent, model }: { directory?: string; title?: string; parentID?: string; agent?: string; model?: { providerID?: string; modelID?: string } }) {
    await this.#boot();
    const cwd = normalizeDirectoryKey(directory);
    const sessionFile = SessionManager.createEmptySessionFile(cwd);
    const manager = await SessionManager.open(sessionFile, this.#sessionDirFor(cwd));
    const sessionId = manager.getSessionId();
    const now = Date.now();
    this.registry.update(cwd, sessionId, {
      timeCreated: now,
      timeUpdated: now,
      ...(title ? { title } : {}),
      ...(parentID ? { parentID } : {}),
      // The wire `agent` param is a persona name (or the legacy build/plan
      // ids, which normalize away); store the normalized persona key.
      ...(agent && personaKeyFor(agent) !== 'standard' ? { persona: personaKeyFor(agent) } : {}),
      ...(model ? { model: `${model.providerID ?? ''}/${model.modelID ?? ''}` || undefined } : {})
    });
    await manager.close();
    const session = this.#wireSession(
      {
        id: sessionId,
        cwd,
        title,
        created: new Date(now),
        modified: new Date(now)
      },
      cwd,
      (this.registry.get(cwd, sessionId) ?? undefined)
    );
    this.bus.emit('session.created', { sessionID: sessionId, info: session }, cwd);
    return session;
  }

  /** 读取单个会话的 wire 记录：live 优先；其次头部标量流式冷读；再次冷 manager 全量兜底（无效头部时保留重写/铸造行为）；最后尝试 subagent 转录的只读解析（挂 parentID 使 UI 视为不可 prompt 的子会话）。全部未命中返回 null。 */
  async getSession({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    if (live) return this.#wireSessionFromLive(live);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (file) {
      // Header scalars stream without a manager (plan §7.2); null → the
      // manager arm below keeps the invalid-header rewrite/mint behavior.
      const scalars = await readSessionScalars(file.path);
      if (scalars) {
        const createdParsed = scalars.createdIso ? Date.parse(scalars.createdIso) : NaN;
        const created = Number.isFinite(createdParsed) ? createdParsed : Date.now();
        const modifiedParsed = scalars.modifiedIso ? Date.parse(scalars.modifiedIso) : NaN;
        const modified = Number.isFinite(modifiedParsed) ? modifiedParsed : created;
        const info = {
          id: scalars.id,
          // SessionManager.open's fallback for a missing/deleted recorded cwd is
          // the launch project dir; the host process never setProjectDir's, so
          // process.cwd() is the same value.
          cwd: scalars.cwd ?? process.cwd(),
          title: scalars.title,
          created: new Date(created),
          modified: new Date(Number.isFinite(modified) ? modified : created)
        };
        return this.#wireSession(info, directoryKey, (this.registry.get(directoryKey, sessionID) ?? undefined));
      }
      return withColdManager(file.path, (manager) => {
        const info = this.#infoFromManager(manager, file.path, directoryKey);
        return this.#wireSession(info, directoryKey, (this.registry.get(directoryKey, sessionID) ?? undefined));
      });
    }
    // Subagent sessions (task-tool runs) resolve read-only: the registry ref
    // locates the transcript, and wire parentID (host session) makes the
    // shared UI treat it as a non-promptable child session.
    const subagent = await this.#findSubagentSessionFile(sessionID, directoryKey);
    if (subagent) {
      const manager = await SessionManager.open(subagent.path);
      try {
        const info = this.#infoFromManager(manager, subagent.path, directoryKey);
        const parentID = sessionIDFromSessionFile(subagent.path);
        const base = this.#wireSession(
          { ...info, title: info.title || subagent.ref.displayName || info.id },
          directoryKey,
          undefined
        );
        return parentID ? { ...base, parentID } : base;
      } finally {
        await manager.close();
      }
    }
    return null;
  }

  /** 从 SessionManager 提取 SessionListInfo：头部时间戳为创建时间、最后一条条目时间戳为修改时间，缺失时以当前时间或创建时间兜底。 */
  #infoFromManager(manager: SessionManagerLike, filePath: string, directoryKey: string) {
    const header = manager.getHeader();
    const entries = manager.getEntries() ?? [];
    const last = entries[entries.length - 1];
    const created = header?.timestamp ? Date.parse(header.timestamp) : Date.now();
    const modified = last?.timestamp ? Date.parse(last.timestamp) : created;
    return {
      id: manager.getSessionId(),
      cwd: manager.getCwd() || directoryKey,
      title: manager.getSessionName(),
      created: new Date(created),
      modified: new Date(Number.isFinite(modified) ? modified : created)
    };
  }

  /** 在目录键的 sessions 根下列出转录并按 sessionID 精确匹配；命中返回 {path, dir}，未命中返回 null。 */
  async #findSessionFile(sessionID: string, directoryKey: string) {
    const dir = this.#sessionDirFor(directoryKey);
    const infos = await SessionManager.list(directoryKey, dir);
    const hit = infos.find((info) => info.id === sessionID);
    if (hit) return { path: hit.path, dir };
    return null;
  }

  /**
   * Resolve a subagent session's transcript file by the child's own
   * sessionID. Scope: the transcript must live under `directoryKey`'s
   * sessions root (the dir-key layout owns the file), and only read paths may
   * use this — update/delete/materialize/fork keep host-only semantics via
   * #findSessionFile.
   */
  /** 按子会话自身的 sessionID 解析 subagent 转录文件。作用域：转录必须位于 directoryKey 的 sessions 根下，且仅读路径可用——更新/删除/物化/fork 保持仅宿主语义（走 #findSessionFile）。 */
  async #findSubagentSessionFile(sessionID: string, directoryKey: string) {
    const root = path.resolve(this.#sessionDirFor(directoryKey));
    for (const ref of AgentRegistry.global?.().list() ?? []) {
      const sessionFile = ref.sessionFile;
      if (!sessionFile) continue;
      if (!isPathUnder(path.resolve(sessionFile), root)) continue;
      const childID = await this.#childSessionIdFor(sessionFile);
      if (childID === sessionID) return { path: sessionFile, ref };
    }
    // Historical runs (post-restart, no registry ref): the one-shot disk
    // scan already mapped this directory's transcripts to child ids.
    await this.#ensureDiskRows(directoryKey);
    const cold = (this.#diskRowsByDirectory.get(directoryKey) ?? []).find(
      (row) => row.childSessionID === sessionID
    );
    if (cold && isPathUnder(path.resolve(cold.file), root)) {
      return { path: cold.file, ref: { displayName: cold.displayName ?? cold.agentId } };
    }
    return null;
  }

  /** 读取（并缓存）subagent 转录头部的子会话 id：缓存命中直接返回，否则读文件头并写入缓存。 */
  async #childSessionIdFor(sessionFile: string): Promise<string | undefined> {
    const cached = this.#childSessionIdByFile.get(sessionFile);
    if (cached) return cached;
    const id = await readSessionHeaderId(sessionFile);
    if (id) this.#childSessionIdByFile.set(sessionFile, id);
    return id;
  }
  /**
   * Warm the childSessionID cache for every global-registry transcript under
   * the live directories so agent-runs rows can carry `childSessionID`
   * synchronously (projectAgentRun has no async surface). One open per file,
   * ever; a refresh follows so warmed rows re-publish.
   */
  /** 为 live 目录下全部全局注册表转录预热 childSessionID 缓存，使 agent-runs 行能同步携带 childSessionID（projectAgentRun 无异步表面）。每文件只 open 一次；随后 refresh 让预热行重新发布。 */
  #warmChildSessionIds(): Promise<void> {
    this.#childSessionIdWarm ??= (async () => {
      const roots = [...this.#live.liveDirectories()].map((directory) =>
        path.resolve(this.#sessionDirFor(normalizeDirectoryKey(directory))));
      const pending: string[] = [];
      for (const ref of AgentRegistry.global?.().list() ?? []) {
        const sessionFile = ref.sessionFile;
        if (!sessionFile || this.#childSessionIdByFile.has(sessionFile)) continue;
        if (!roots.some((root) => isPathUnder(path.resolve(sessionFile), root))) continue;
        pending.push(sessionFile);
      }
      await Promise.all(pending.map((file) => this.#childSessionIdFor(file)));
    })().finally(() => {
      this.#childSessionIdWarm = null;
    });
    return this.#childSessionIdWarm;
  }

  /** Sync cache read for snapshot projection (agentsSnapshot rows). */
  /** 快照投影（agentsSnapshot 行）用的同步缓存读；未命中返回 undefined，不触发 IO。 */
  #childSessionIdCached(sessionFile: string | null | undefined): string | undefined {
    if (!sessionFile) return undefined;
    return this.#childSessionIdByFile.get(sessionFile);
  }

  /**
   * Re-register this session's settled subagent runs from their transcripts
   * (SDK `registerPersistedSubagents`): the SDK reclaims refs on its own
   * schedule (idle park, corpse reclaim), and consumers — agent-runs rows,
   * child-session reads — must not depend on that schedule. Bounded streamed
   * metadata reads only; no session materialization, no revival wiring.
   */
  /** 从转录重新注册本会话已结算的 subagent 运行（SDK registerPersistedSubagents）：SDK 按自己的节奏回收 ref（空闲停泊、尸体回收），而 agent-runs 行、子会话读取等消费者不能依赖那个节奏。仅有界的流式元数据读取；不物化会话、不接线复活。 */
  async #rehydrateSubagentRefs(sessionFile: string): Promise<void> {
    // Same invariant as the engine-wide subscription above: a
    // minimal/embedded SDK surface may omit the process-global registry,
    // and there is nothing to re-register into without it.
    const globalRegistry = AgentRegistry.global?.();
    if (!globalRegistry) return;
    try {
      await registerPersistedSubagents(globalRegistry, sessionFile);
    } catch (error) {
      console.warn('[omp-host] subagent rehydration failed:', error instanceof Error ? error.message : String(error));
      return;
    }
    // Warm child ids first so the refreshed rows carry childSessionID
    // synchronously (projectAgentRun has no async surface).
    await this.#warmChildSessionIds();
    this.uriDomain?.aggregator.refresh();
  }

  /**
   * Drop the disk-row cache for every live directory whose sessions root
   * contains this transcript, so the next listing rescans and discovers it.
   * Called on registry `registered` events — a new run means a new file the
   * one-shot scan may have already missed.
   */
  /** 作废 sessions 根包含该转录的所有 live 目录的磁盘行缓存，使下次列表重扫时能发现它。注册表 registered 事件时调用——新运行意味着一次性扫描可能已错过的新文件。 */
  #invalidateDiskRowsFor(sessionFile: string | null | undefined): void {
    if (!sessionFile) return;
    const file = path.resolve(sessionFile);
    for (const directory of this.#live.liveDirectories()) {
      const key = normalizeDirectoryKey(directory);
      if (!this.#diskRowsByDirectory.has(key)) continue;
      const root = path.resolve(this.#sessionDirFor(key));
      if (isPathUnder(file, root)) this.#diskRowsByDirectory.delete(key);
    }
  }

  /**
   * Re-register a run whose ref just left the registry, but only while its
   * owning session is live — a viewer is watching the row disappear. Nobody
   * watching → the SDK's memory policy wins; the next materialization
   * rehydrates. Coalesced per host session file so a settling batch triggers
   * one pass. Re-registration is CAS-safe against a fresh spawn claiming the
   * id (reclaimDeadCorpse handles the corpse the same way).
   */
  /** 重新注册刚离开注册表、但其宿主会话仍 live 的运行——有观看者正在看这行消失。无人观看时尊重 SDK 内存策略，下次物化再水化。按宿主会话文件合并去重；重注册对抢占同一 id 的新 spawn 是 CAS 安全的。 */
  #rehydrateRemovedRun(runFile: string | null | undefined): void {
    if (!runFile) return;
    const artifactsDir = path.dirname(runFile);
    const sessionId = sessionIDFromSessionFile(path.join(artifactsDir, 'x.jsonl'));
    if (!sessionId) return;
    if (!this.#live.snapshot().some((record) => record.state === 'live' && record.sessionId === sessionId)) return;
    const hostFile = `${artifactsDir}.jsonl`;
    if (this.#rehydrateInFlight.has(hostFile)) return;
    this.#rehydrateInFlight.add(hostFile);
    void this.#rehydrateSubagentRefs(hostFile).finally(() => {
      this.#rehydrateInFlight.delete(hostFile);
    });
  }

  /** One-shot per directory: scan nested transcripts into cached rows. */
  /** 每目录一次性的嵌套转录扫描：已有缓存则复用、进行中扫描则等待，否则扫描并把结果写入缓存。 */
  async #ensureDiskRows(directoryKey: string): Promise<void> {
    if (!directoryKey || this.#diskRowsByDirectory.has(directoryKey)) return;
    const inFlight = this.#diskScanInFlight.get(directoryKey);
    if (inFlight) return inFlight;
    const scan = (async () => {
      const rows = await this.#scanDiskRows(directoryKey);
      if (rows) this.#diskRowsByDirectory.set(directoryKey, rows);
    })().finally(() => {
      this.#diskScanInFlight.delete(directoryKey);
    });
    this.#diskScanInFlight.set(directoryKey, scan);
    return scan;
  }

  /**
   * Historical run rows for one directory: every nested transcript under the
   * directory's sessions root (`<ts>_<hostID>/<task>.jsonl`). Host transcripts
   * live at the top level and never match; registry rows win over these.
   */
  /** 一个目录的历史运行行：sessions 根下每个嵌套转录（<ts>_<hostID>/<task>.jsonl 布局）。顶层宿主转录永不匹配；注册表行优先于这些行。目录未知返回 null（不缓存任何内容）。 */
  async #scanDiskRows(directoryKey: string): Promise<Array<DiskScanRow & { file: string }> | null> {
    const root = this.#sessionDirFor(directoryKey);
    let entries: import('node:fs').Dirent[];
    try {
      entries = await fs.promises.readdir(root, { withFileTypes: true });
    } catch {
 return null; // unknown directory — no rows, nothing cached
    }
    const rows: Array<DiskScanRow & { file: string }> = [];
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const hostID = sessionIDFromSessionFile(path.join(root, entry.name, 'x.jsonl'));
      if (!hostID) continue;
      let files: string[];
      try {
        files = await fs.promises.readdir(path.join(root, entry.name));
      } catch {
        continue;
      }
      for (const file of files) {
        if (!file.endsWith('.jsonl')) continue;
        const filePath = path.join(root, entry.name, file);
        const stats = await fs.promises.stat(filePath).catch(() => null);
        if (!stats) continue;
        const agentId = file.slice(0, -'.jsonl'.length);
        const childID = await this.#childSessionIdFor(filePath);
        const row: DiskScanRow & { file: string } = {
          file: filePath,
          sessionID: hostID,
          agentId,
          directory: directoryKey,
          displayName: agentId,
          kind: 'sub',
          // Aggregator disk rows are 'historical' by construction.
          createdAt: Math.round(stats.birthtimeMs) || 0,
          lastActivity: Math.round(stats.mtimeMs) || 0,
          hasTranscript: true
        };
        if (childID) row.childSessionID = childID;
        rows.push(row);
      }
    }
    return rows;
  }

  /** 更新会话元数据（标题、metadata、归档时间）：live 会话按其拥有目录写回并同步 SDK 会话名；冷会话要求目录确实拥有转录或注册表行，否则返回 null 拒绝（避免制造幻影注册表行）。成功后广播 session.updated。 */
  async updateSession({ sessionID, directory, title, metadata, timeArchived }: { sessionID: string; directory?: string; title?: string; metadata?: Record<string, SessionMetadataValue>; timeArchived?: number }) {
    await this.#boot();
    const live = this.#liveHostAnywhere(directory, sessionID);
    // A live session owns its registry entry under its own directory, and
    // getSession answers from the live record first regardless of the
    // requested directory. Writing the patch under a differing requested
    // directory would return and broadcast an update that was never applied,
    // and strand the patch as a phantom registry entry that listings under
    // the owning directory never read.
    const directoryKey = live ? normalizeDirectoryKey(live.directory) : normalizeDirectoryKey(directory);
    if (!live) {
      // Idle sessions are on-disk records owned by exactly one directory:
      // transcript and registry entry both live there. An update addressed to
      // a directory that owns neither is mis-addressed — writing it would
      // fabricate a phantom registry entry and answer with a session no
      // listing (keyed by the transcript's own cwd) can ever observe, so the
      // caller sees success while nothing takes effect. Refuse; registry-only
      // bookkeeping (transcript pruned externally) stays updatable.
      const hadRegistryEntry = (this.registry.get(directoryKey, sessionID) ?? undefined) != null;
      if (!hadRegistryEntry && !(await this.#findSessionFile(sessionID, directoryKey))) {
        return null;
      }
    }
    const patch: Partial<SessionMeta> = { timeUpdated: Date.now() };
    if (typeof title === 'string') patch.title = title;
    if (metadata !== undefined) patch.metadata = metadata;
    if (timeArchived !== undefined) patch.timeArchived = timeArchived || undefined;
    const meta = this.registry.update(directoryKey, sessionID, patch);
    if (live && typeof title === 'string') {
      await live.agentSession?.setSessionName(title, 'user').catch(() => {});
    }
    const session = await this.getSession({
      sessionID,
      directory: directoryKey
    });
    if (session) {
      this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    }
    return (
      session ??
      this.#wireSession(
        {
          id: sessionID,
          cwd: directoryKey,
          created: new Date(),
          modified: new Date()
        },
        directoryKey,
        meta
      )
    );
  }

  /**
   * Manual release of an idle resident session — the memory monitor's
   * "release" action (`POST /omp/sessions/{id}/release`). Returns
   * `'cold'` when nothing is resident (idempotent no-op), `'active'` when
   * the session still has work or UI leases (the caller surfaces it as a
   * conflict), `'released'` once the eviction handoff is accepted — the
   * post-dispose `session.updated` flips client rows to cold.
   */
  /** 手动释放空闲常驻会话——内存监视器的 release 动作。无可常驻返回 'cold'（幂等 no-op）；仍有工作或 UI 租约返回 'active'（调用方作为冲突呈现）；逐出交接被接受返回 'released'（dispose 后的 session.updated 把客户端行翻为冷）。 */
  async releaseSession({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const anywhere = this.#liveHostAnywhere(directoryKey, sessionID);
    const fromKey = anywhere ? normalizeDirectoryKey(anywhere.directory) : directoryKey;
    const key = sessionKey(fromKey, sessionID);
    let outcome: 'cold' | 'active' | 'released' = 'cold';
    await this.#live.withOperation(key, async () => {
      const record = this.#live.get(fromKey, sessionID);
      if (!record) return;
      if (record.state !== 'live') {
        // Still resident but mid-transition — report active so the UI
        // does not claim a release that did not happen.
        outcome = 'active';
        return;
      }
      if (record.inFlight > 0 || this.#recordIsActive(record)) {
        outcome = 'active';
        return;
      }
      this.#evictRecord(record, 'manual-release');
      outcome = 'released';
    });
    return outcome;
  }

  /** 删除会话：整个变更串行在拥有键的门内——先取响应快照，再逐出 live 记录、有界等待 dispose（失败或超时以可重试的 SessionBusyError 拒绝）、释放领域句柄、删注册表行、强制删除转录文件。完成后广播 session.deleted。 */
  async deleteSession({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const fromKey = live ? normalizeDirectoryKey(live.directory) : directoryKey;
    const key = sessionKey(fromKey, sessionID);
    // Response snapshot before teardown (a live manager still answers the
    // title); the mutation itself runs entirely inside the gate below.
    const info = live ? this.#wireSessionFromLive(live) : await this.getSession({ sessionID, directory: fromKey });
    // Serialize the WHOLE delete under the owning key's gate — evict,
    // bounded disposal wait, registry row, transcript removal (plan §3.4).
    // Await INSIDE the gate: a re-materialize squeezing between disposal
    // and rmSync would resurrect a second writer on the file being deleted.
    // A disposal that fails or outlives the bounded wait refuses the delete
    // with a retryable busy error — unlinking a transcript a live writer
    // still holds is the POSIX orphan-write / Windows locked-file hazard.
    await this.#live.withOperation(key, async () => {
      const record = this.#live.get(fromKey, sessionID);
      if (record && record.state === 'live') {
        this.#evictRecord(record, 'delete', { emitIdle: false });
      }
      const blocking = this.#live.byKey(key);
      if (blocking) {
        const outcome = await this.#awaitDisposalBounded(blocking);
        if (outcome !== 'disposed') {
          throw new SessionBusyError(
            outcome === 'failed' ? 'session-failed' : 'session-evicting',
            `session ${sessionID} is not deletable: disposal ${outcome === 'timeout' ? 'did not settle' : `failed (${blocking.failure?.reason ?? 'unknown'})`}`,
            sessionID,
          );
        }
      }
      const file = await this.#findSessionFile(sessionID, fromKey);
      this.uriDomain?.descriptors?.releaseForSession?.(fromKey, sessionID);
      this.uriDomain?.aggregator?.releaseForSession?.(fromKey, sessionID);
      this.registry.remove(fromKey, sessionID);
      if (file) {
        // Removal failures propagate: answering "deleted" while a writer
        // still holds the transcript is the lie this gate exists to kill.
        fs.rmSync(file.path, { force: true });
      }
    });
    this.#maybeReleaseDirectoryState(fromKey);
    this.bus.emit('session.deleted', { sessionID }, fromKey);
    return info;
  }
  /** 从 live 宿主记录直接组装 wire Session 记录：标题优先 SDK 会话名，时间取注册表元数据兜底当前时间。 */
  #wireSessionFromLive(live: HostSession) {
    const meta = this.registry.get(live.directory, live.sessionId);
    const agentSession = live.agentSession;
    const now = Date.now();
    return this.#wireSession(
      {
        id: live.sessionId,
        cwd: live.directory,
        title: agentSession?.sessionManager.getSessionName() ?? meta?.title,
        created: new Date(meta?.timeCreated ?? now),
        modified: new Date(meta?.timeUpdated ?? now)
      },
      live.directory,
      meta ?? undefined,
      live
    );
  }

  /** Cold message projection from the persisted transcript. */
  /** 持久化转录的冷消息投影（#projectedMessages 的公开包装）。 */
  async getMessages({ sessionID, directory }: { sessionID: string; directory?: string }) {
    return this.#projectedMessages(sessionID, directory);
  }

  /**
   * Paged cold projection for the message-history route: applies the
   * limit/before window over the full projection and reports the
   * next-older cursor (see paginateProjectedMessages).
   */
  /** 消息历史路由的分页冷投影：优先窗口化流式冷读（不物化整份转录），再在全量投影上应用 limit/before 窗口并报告 next-older 游标（见 paginateProjectedMessages）。 */
  async getMessagesPage({ sessionID, directory, limit, before }: { sessionID: string; directory?: string; limit?: number; before?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const liveCount = live?.agentSession?.messages?.length ?? -1;
    const file = await this.#findSessionFile(sessionID, directoryKey);
    const externalChange = live
      ? classifyExternalChange(live.fileSignature, file?.path ?? '')
      : 'unchanged';
    if (file) {
      // Windowed cold read (plan §7.2): metadata pass + bounded content pass
      // produce the requested page without materializing the transcript;
      // labeled fallbacks keep the full-materialization arm below.
      const agent = wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent));
      const streamed = await readTranscriptMessagePage(file.path, {
        sessionID,
        directory: directoryKey,
        agent,
        wireIdFor,
        limit,
        before,
      });
      if (streamed) {
        const liveArmWins = externalChange !== 'dirty' && liveCount >= 0 && liveCount >= streamed.fileMessageCount;
        if (liveArmWins) {
          return paginateProjectedMessages(
            this.#mergeTurnEventDividers(
              projectConversation(live?.agentSession?.messages ?? [], {
                sessionID,
                directory: directoryKey,
                agent,
                wireIdFor
              }),
              streamed.dividerEntries,
              sessionID
            ),
            { limit, before }
          );
        }
        if (streamed.fileMessageCount > 0 || liveCount < 0) return streamed.page;
        return null;
      }
    }
    const projected = await this.#projectedMessages(sessionID, directory);
    if (!projected) return null;
    return paginateProjectedMessages(projected, { limit, before });
  }
  /** 消息投影核心：文件转录（transcript:true）是显示真相（保留压缩前历史与分隔条目），仅当 live 镜像至少与文件一样完整且无脏外部改写时才读 live 列表；无文件时退化为 live 列表或 subagent 转录；全部未命中返回 null。 */
  async #projectedMessages(sessionID: string, directory: string | null | undefined) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const wireIdFor = this.#wireIdResolver(directoryKey, sessionID);
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    const liveSession = live?.agentSession ?? null;
    const liveCount = liveSession?.messages?.length ?? -1;

    // File transcript (transcript: true) is the display truth: it keeps the
    // full history including pre-compaction user turns and divider entries.
    // A live session's runtime context is post-compaction (users folded into
    // the summary), which used to blank the UI — so read the live list only
    // when it is at least as complete as the file.
    const file = await this.#findSessionFile(sessionID, directoryKey);
    // Dual-write classification (plan §8): an external rewrite that shrank
    // or mutated the transcript must not be masked by a longer, now-stale
    // live mirror — dirty means the file arm is the display truth outright.
    const externalChange = live
      ? classifyExternalChange(live.fileSignature, file?.path ?? '')
      : 'unchanged';
    if (file) {
      // Labeled fallback (plan §7.1): the file arm needs entries for
      // turn-state stampers and dividers, so it full-materializes through a
      // cold manager that withColdManager closes and releases in finally.
      return withColdManager(file.path, (manager) => {
        const context = manager.buildSessionContext({ transcript: true });
        const fileMessages = context.messages ?? [];
        const entries = manager.getEntries() ?? [];
        // Exact per-message snapshots: fold the transcript's model_change /
        // thinking_level_change log so every user message carries the state
        // it was sent with (SDK user messages persist neither).
        const turnStateFor = buildTurnStateStamper(entries, { wireIdFor });
        // Timeline dividers for the same turn-state entries: model and mode
        // switches render as slim dividers at their point in the log.
        const mergeDividers = (projected: ProjectedMessage[]) => this.#mergeTurnEventDividers(projected, entries, sessionID);
        // Dirty external rewrite: the file is the truth even when the stale
        // live mirror is longer (plan §8.1 step 4).
        const liveArmWins = externalChange !== 'dirty' && liveCount >= 0 && liveCount >= fileMessages.length;
        if (liveArmWins) {
          return mergeDividers(
            projectConversation(liveSession?.messages ?? [], {
              sessionID,
              directory: directoryKey,
              agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
              wireIdFor,
              turnStateFor
            })
          );
        }
        if (fileMessages.length > 0 || liveCount < 0) {
          return mergeDividers(
            projectConversation(fileMessages, {
              sessionID,
              directory: directoryKey,
              agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
              wireIdFor,
              turnStateFor
            })
          );
        }
        // SAFETY: both consume arms returned ProjectedMessage[]; the null
        // arm matches the outer no-file/no-live contract.
        return null as ProjectedMessage[] | null;
      });
    }
    if (liveCount >= 0) {
      return projectConversation(liveSession?.messages ?? [], {
        sessionID,
        directory: directoryKey,
        agent: wireAgentFor(personaKeyFor(meta?.persona ?? meta?.agent)),
        wireIdFor
      });
    }
    // Subagent transcripts (task-tool runs): resolved read-only through the
    // global registry; projected with the child's own sessionID so the
    // embedded read-only chat's parts/message ids stay self-consistent.
    const subagent = await this.#findSubagentSessionFile(sessionID, directoryKey);
    if (subagent) {
      const manager = await SessionManager.open(subagent.path);
      try {
        const context = manager.buildSessionContext({ transcript: true });
        return projectConversation(context.messages ?? [], {
          sessionID,
          directory: directoryKey
        });
      } finally {
        await manager.close().catch(() => {});
      }
    }
    return null;
  }

  /**
   * Insert turn-event dividers (model/mode switches) into a projected
   * conversation at their transcript position: before the first message
   * created at or after the entry's timestamp, or at the end. Entries the
   * divider projection rejects (init bookkeeping without a role tag) are
   * skipped, keeping deterministic ids stable across re-projections.
   */
  /** 把轮次事件分隔条（模型/模式切换）按转录位置插入投影会话：插在创建时间不早于条目时间戳的首条消息之前，否则追加到末尾。被分隔投影拒绝的条目（无角色标签的 init 簿记）跳过，保持确定性 id 跨投影稳定。 */
  #mergeTurnEventDividers(projected: ProjectedMessage[], entries: readonly SessionEntry[], sessionID: string) {
    const dividers = [];
    for (const entry of entries) {
      const wire = projectTurnEventDivider(entry, { sessionID });
      if (wire) dividers.push(wire);
    }
    if (dividers.length === 0) return projected;
    const out = [...projected];
    for (const wire of dividers) {
      const at = out.findIndex((item) => (item.info.time?.created ?? 0) >= (wire.info.time?.created ?? 0));
      out.splice(at === -1 ? out.length : at, 0, wire);
    }
    return out;
  }

  /**
   * The thinking level a turn actually runs with: the session's explicit
   * pick when set, else the model's configured default (inherit), else
   * unknown (models without a thinking surface).
   */
  /** 一轮实际运行的思考级别：会话显式选择优先，其次模型配置默认（inherit），再否则未知（无思考面的模型）。 */
  #effectiveThinkingLevel(session: AgentSession) {
    if (session.thinkingLevel !== undefined && session.thinkingLevel !== null) {
      return session.thinkingLevel;
    }
    const model = session.model;
    if (!model?.provider || !model?.id) return undefined;
    const entry = this.availableModels().find((candidate) => candidate.provider === model.provider && candidate.id === model.id);
    const defaultLevel = entry?.thinking?.defaultLevel;
    return typeof defaultLevel === 'string' && defaultLevel.length > 0 ? defaultLevel : undefined;
  }

  /**
   * Echo resolver for projections of a session that is live in this process
   * (plan phase 5 compact mapping): canonical id → the id the client already
   * saw. User entries map the canonical formula id to the echoed client
   * messageID; assistant entries exist only on the SDK's rare
   * timestamp-rewrite paths (see the message_end handler) — the stable
   * formula makes live and cold assistant ids identical otherwise, so the
   * map stays empty for normal turns. `!` execution records map their
   * canonical execution id to the dispatch-time live id (see executeBash).
   */
  /** 本进程内 live 会话投影的回显解析器（plan phase 5 紧凑映射）：规范 id 映射到客户端已见的 id。用户条目把规范公式 id 映到回显的客户端 messageID；assistant 条目仅存在于 SDK 罕见的时间戳重写路径；`!` 执行记录把规范执行 id 映到派发时的 live id（见 executeBash）。 */
  #wireIdResolver(directoryKey: string, sessionID: string) {
    const live = this.#liveHostAnywhere(directoryKey, sessionID);
    if (live?.pendingShellEchoes?.length) {
      // Deferred `!` records land at the turn's pendingMessages flush; bridge
      // their canonical ids to the live rows the dispatch already emitted as
      // soon as they become visible in the session's message list. Records
      // append in completion order while pending entries queue in dispatch
      // order, so two concurrent same-command runs can resolve out of order —
      // the full result fingerprint (output/exitCode/cancelled, verbatim in
      // the record) identifies the run regardless of landing order, and the
      // claimed set keeps a matched record from resolving twice.
      const messages = live.agentSession?.messages ?? [];
      const claimed = new Set<object>();
      live.pendingShellEchoes = live.pendingShellEchoes.filter((pending) => {
        const record = messages.find(
          (m): m is BashExecutionMessage =>
            m.role === 'bashExecution'
            && m.command === pending.command
            && m.output === pending.output
            && m.exitCode === pending.exitCode
            && m.cancelled === pending.cancelled
            && m.timestamp >= pending.started
            && !claimed.has(m)
        );
        if (!record) return true;
        claimed.add(record);
        live.wireIdEchoes.set(executionWireId(record), pending.liveId);
        return false;
      });
      if (!live.pendingShellEchoes.length) live.pendingShellEchoes = undefined;
    }
    const echoes = live?.wireIdEchoes;
    if (!echoes || echoes.size === 0) return undefined;
    return (message: WireIdMessageInput | null | undefined) => {
      if (message?.role === 'bashExecution' || message?.role === 'pythonExecution') {
        // SAFETY: the role check narrows the wire-id input to the execution
        // shape executionWireId reads (command/code/output/exitCode/cancelled).
        return echoes.get(executionWireId(message as ShellExecutionMessageInput));
      }
      if (message?.role !== 'user' && message?.role !== 'assistant') return undefined;
      return echoes.get(deterministicWireId(message));
    };
  }

  /**
   * Wire join key for a retry update (P4, field-loss plan). The SDK's
   * persistenceKey addresses the persisted assistant entry
   * ('assistant:<ts>:<provider>:<model>:<responseId>:<stopReason>'); the UI
   * joins omp.retry.ended notes by projected WIRE id (the TUI joins the same
   * update onto its component by persistenceKey — entryId is persistence
   * layer only). Resolve the timestamp segment to the live assistant
   * message and derive its wire id (the stable formula id, identical to the
   * id the streaming projector emitted), then fall back to the most recent
   * settled assistant wire id (the TUI's FIFO analog). The raw key is the
   * last resort so the payload stays joinable-shaped even when nothing
   * matches.
   */
  /** retry 更新的 wire 连接键（P4 字段丢失计划）：把 persistenceKey 的时间戳段解析回 live assistant 消息并推导其 wire id（与流式投影器发出的稳定公式 id 一致）；退化为最近结算的 assistant wire id（TUI 的 FIFO 类比），最后才用原始键——保证载荷始终可连接成形。 */
  #retryWireIdFor(hostSession: HostSession, update: { entryId?: string; persistenceKey?: string }): string {
    const key = update.persistenceKey ?? update.entryId ?? '';
    const timestamp = Number.parseInt(key.split(':')[1] ?? '', 10);
    const messages = hostSession.agentSession?.messages ?? [];
    const isAssistant = (m: AgentSession['messages'][number]): m is AgentSession['messages'][number] & { role: 'assistant' } =>
      m?.role === 'assistant';
    const match = Number.isFinite(timestamp)
      ? [...messages].filter(isAssistant).reverse().find((m) => m.timestamp === timestamp)
      : undefined;
    if (match) return hostSession.wireIdEchoes.get(deterministicWireId(match)) ?? deterministicWireId(match);
    return hostSession.lastAssistantWireId ?? key;
  }

  /** 把 wire 模型选择器解析为 SDK 可用模型：先按 provider/id 精确匹配，再退化为仅按 id；不可用时返回 undefined。 */
  #resolveModel(selector: { providerID?: string; modelID?: string } | undefined) {
    if (!selector) return undefined;
    const available = this.#sdkModels();
    const wanted = `${selector.providerID}/${selector.modelID}`;
    return available.find((model) => `${model.provider}/${model.id}` === wanted) ?? available.find((model) => model.id === selector.modelID);
  }

  /**
   * Materialize (or join) the live session for one key (plan §3.2/§3.3).
   * Runs inside the per-key operation gate: concurrent callers serialize,
   * the second one joins the freshly committed record. An evicting record
   * is awaited once (bounded by the drain budget) and retried exactly once;
   * a quarantined (failed) or shutting-down key rejects with a retryable
   * SessionBusyError instead of silently building a second writer.
   */
  /** 物化（或加入）某键的 live 会话（plan §3.2/§3.3）：在每键操作门内运行，并发调用者串行、后来者加入新提交的记录。evicting 记录有界等待后恰重试一次；隔离（failed）或正在关闭的键以可重试的 SessionBusyError 拒绝，绝不悄悄造出第二个写方。 */
  async #materialize(sessionId: string, directoryKey: string): Promise<HostSession | null> {
    const key = sessionKey(directoryKey, sessionId);
    return this.#live.withOperation(key, () => this.#materializeGated(sessionId, directoryKey));
  }

  /** #materialize 的门内主体：处理 evicting 的有界等待与单次重试、closing 拒绝，然后 beginMaterialize → #materializeNow → commit/fail；失败路径释放目录状态并把原始错误抛回。 */
  async #materializeGated(sessionId: string, directoryKey: string): Promise<HostSession | null> {
    await this.#boot();
    for (let attempt = 0; ; attempt += 1) {
      const liveRecord = this.#live.getLive(directoryKey, sessionId);
      if (liveRecord) {
        // A materialize is a user-visible live operation: refresh the TTL.
        this.#live.touch(liveRecord);
        return liveRecord.payload;
      }
      const record = this.#live.get(directoryKey, sessionId);
      if (record?.state === 'evicting') {
        if (attempt >= 1) {
          throw new SessionBusyError('session-evicting', `session ${sessionId} is evicting`, sessionId);
        }
        // Wait for the single disposal promise — bounded: a disposal that
        // never settles must park this caller at a retryable busy error,
        // not forever (plan §3.4 timeout rule).
        let evictWaitTimer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([
          record.disposePromise ?? Promise.resolve('disposed' as const),
          new Promise((resolve) => {
            evictWaitTimer = setTimeout(resolve, this.evictDrainTimeoutMs * 2);
          }),
        ])
          .catch(() => {})
          .finally(() => clearTimeout(evictWaitTimer));
        continue;
      }
      if (this.#closing) {
        throw new SessionBusyError('host-shutting-down', 'host is shutting down', sessionId);
      }
      break;
    }
    const record = this.#live.beginMaterialize(directoryKey, sessionId);
    try {
      const hostSession = await this.#materializeNow(sessionId, directoryKey, record);
      if (!hostSession) {
        // Session file absent — cold miss, not a setup failure.
        this.#live.failMaterialize(record);
        this.#maybeReleaseDirectoryState(directoryKey);
        return null;
      }
      this.#live.commitMaterialize(record, hostSession);
      return hostSession;
    } catch (error) {
      // #materializeNow already released its resources in its finally
      // block (plan §3.3); drop the dedup row and rethrow.
      this.#live.failMaterialize(record);
      this.#maybeReleaseDirectoryState(directoryKey);
      throw error;
    }
  }


  /**
   * Build one live session. Runs inside the key's gate with a
   * `materializing` record open. Every step after the first await is
   * guarded by a try/finally that releases each installed resource
   * exactly once (plan §3.3): event unsubscribe, name unsubscribe,
   * agentSession.dispose, temporary-manager close, and the domain handles.
   */
  /** 构建一个 live 会话。在键的门内、materializing 记录打开时运行。首个 await 之后的每一步都被 try/finally 守护，每个已安装资源恰好释放一次（plan §3.3）：事件退订、名称退订、agentSession.dispose、临时 manager 关闭、领域句柄。 */
  async #materializeNow(
    sessionId: string,
    directoryKey: string,
    record: LiveRecord<HostSession>,
  ): Promise<HostSession | null> {
    const file = await this.#findSessionFile(sessionId, directoryKey);
    if (!file) return null;
    const manager = await SessionManager.open(file.path, this.#sessionDirFor(directoryKey));
    let agentCreated = false;
    let hostSession: HostSession | null = null;
    try {
      const meta = this.registry.get(directoryKey, sessionId);
      // Model comes from the session's persisted selector when set; otherwise
      // createAgentSession resolves the settings default (defaultModel /
      // defaultProvider) exactly like the TUI. Pinning getAvailable()[0] here
      // used to override the user's configured default with whichever model
      // happened to sort first.
      const model = this.#resolveModel(meta?.model ? splitModelSelector(meta.model) : undefined);
      // Persona overlay (02 §5.1 D-B2): 'build'/'plan'/unset → standard
      // session; a persona name → top-level systemPrompt/toolset override;
      // unknown name (deleted persona) → degrade to standard with a notice.
      const personaState = personaFor(meta, this.personas);
      if (personaState.status === 'missing') {
        console.warn(`[omp-host] session ${sessionId} references unknown persona "${personaState.name}"; using a standard session`);
      }
      const persona = personaState.status === 'active' ? personaState.persona : null;
      const agentRegistry = new AgentRegistry();
      // Shared across every projector generation of this session: async-job
      // task updates outlive the turn that spawned them.
      const finalToolParts = new Map<string, { id: string; messageID: string; toolName: string }>();
      const { session, setToolUIContext } = await this.#createAgentSessionImpl({
        cwd: directoryKey,
        sessionManager: manager,
        authStorage: this.authStorage ?? undefined,
        modelRegistry: this.modelRegistry ?? undefined,
        // Per-directory keyed Settings injection (spec 06 §5.1, master R6):
        // the session consumes this directory's global+project layering.
        // Absent store (degraded boot) falls back to the SDK singleton.
        ...(this.settingsStore ? { settings: await this.settingsStore.settingsFor(directoryKey) } : {}),
        // One registry per session: the SDK's global registry admits a single
        // "Main" agent per process generation, and omp-host embeds several
        // concurrent top-level sessions. The instance is retained on the
        // host session for the agent-runs aggregator (spec 04 §5.5).
        agentRegistry,
        // R13: hasUI authority is the per-session UI lease, never the
        // capability. No lease at creation → fail-closed for approval tools.
        hasUI: this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI,
        // R7/R8: local:// resolution stays session-pinned to THIS session's
        // artifacts dir (TUI parity, spec 04 §5.2.3); zero global mutation.
        localProtocolOptions: createLocalProtocolOptions(sessionId, directoryKey, () =>
          manager.getArtifactsDir(),
        ),
        ...(model ? { model } : {}),
        // Persona overlay (02 §5.1 D-B2): constructor-time systemPrompt and
        // toolset come from the persona resource; the deleted build/plan
        // agent pair and the planYolo mapping never reach createAgentSession
        // (plan mode is a session mode driven by the mode endpoints, §5.8).
        ...(persona?.systemPrompt ? { systemPrompt: persona.systemPrompt } : {}),
        ...(Array.isArray(persona?.tools) && persona.tools.length > 0 ? { toolNames: persona.tools } : {})
      });
      agentCreated = true;
      hostSession = {
        key: sessionKey(directoryKey, sessionId),
        sessionId,
        directory: directoryKey,
        agentSession: null,
        currentPersona: personaKeyFor(meta?.persona ?? meta?.agent),
        projector: null,
        pendingUserWireId: null,
        wireIdEchoes: new Map(),
        lastUserWireId: null,
        syncedEntryKeys: new Set(),
        lastAssistantWireId: null,
        // agent_end {isTerminal:false} keeps the session busy until a later
        // terminal settle (spec 05 §5.7); status snapshots must not downgrade.
        awaitingAsyncSince: null,
        // Retained for the agent-runs aggregator (spec 04 §5.5).
        agentRegistry,
        finalToolParts,
        // CreateAgentSessionResult handle for setToolUIContext (spec 03 R13).
        sdkResult: { setToolUIContext },
        extensionUiInitialized: false,
        extensionUiPromise: null,
        planHandlerAttached: false,
        // Plugin application snapshot (plugins.v1): the discovery set this
        // session bound at materialization — feeds the Settings → Plugins
        // "applied in sessions" projection and stays frozen for the session's
        // lifetime (TS extension modules are not rebound by reload).
        appliedPlugins: null,
        // Dual-write identity at materialize (plan §8): later external
        // writes are classified against this.
        fileSignature: fileSignature(file.path, tailEntryIdOf(file.path))
      };
      hostSession.appliedPlugins = await this.#snapshotAppliedPlugins(directoryKey);
      hostSession.agentSession = session;
      // Publish the payload on the record BEFORE the lease await below: the
      // engine-event guard accepts materializing records whose payload
      // matches, so SDK events emitted between subscribe and
      // commitMaterialize are processed instead of silently dropped.
      record.payload = hostSession;
      hostSession.unsubscribe = session.subscribe((event) => {
        try {
          this.#handleEngineEvent(hostSession!, event);
        } catch (error) {
          console.error('[omp-host] event projection error:', error);
        }
      });
      // Registry events drive the agent-runs aggregator (spec 04 §5.5): without
      // this wiring the snapshot stays at revision 0 forever — rows never appear
      // and omp.agents.updated never publishes, so every UI consumer (work-status
      // rows, header badge, transcript row resolution) sees an empty world even
      // while subagents run. Coalescing lives inside the aggregator (notify
      // schedules one flush per directory); refresh() is the rebuild step.
      hostSession.unsubscribeAgentRegistry = agentRegistry.onChange?.(() => {
        this.uriDomain?.aggregator.refresh();
      });
      this.uriDomain?.aggregator.refresh();
      // Modes tracker for this session (cold-recovery + mode_change appends).
      this.modesDomain?.trackerFor(sessionId, directoryKey);
      // Apply and await the lease context before publishing the record so a
      // lease that raced materialization still gets its extension UI.
      if (this.dialogs.hasUISnapshotFor(directoryKey, sessionId).hasUI) {
        await this.#setDialogUiContext(hostSession, directoryKey, sessionId, true);
      }
      hostSession.nameUnsubscribe = session.sessionManager?.onSessionNameChanged?.(() => {
        const info = this.#wireSessionFromLive(hostSession!);
        this.registry.update(directoryKey, sessionId, {
          title: info.title,
          timeUpdated: Date.now()
        });
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directoryKey);
      });
      // Cold wire records carry no model until a switch writes the registry
      // (the wire selector is registry → live): seed the registry once per
      // materialization from the session's actual model, so after an idle
      // eviction the session's row keeps reporting the model it runs instead
      // of dropping the field (which left the UI's authoritative badge and
      // switch-rollback target null). Seeding with the model the transcript
      // already holds is a no-op for the SDK — it appends no model_change.
      const liveModelSelector = modelSelector(session.model);
      if (liveModelSelector && meta?.model !== liveModelSelector) {
        this.registry.update(directoryKey, sessionId, { model: liveModelSelector });
      }
      // A warmed session flips client rows to live (with its model) instead
      // of waiting for the next list refresh — `session.updated` replaces
      // the stored record wholesale.
      const warmInfo = this.#wireSessionFromLive(hostSession);
      this.bus.emit('session.updated', { sessionID: sessionId, info: warmInfo }, directoryKey);
      // Consumption-time rehydration (docs/plans/subagent-run-visibility): the
      // SDK may have reclaimed this session's subagent refs (idle park, corpse
      // reclaim) while the UI was away. Re-register them from the transcripts
      // this session owns so agent-runs rows and child-session reads resolve
      // for the viewer. Fire-and-forget: registration is CAS-safe against live
      // refs and never blocks the materialization path.
      void this.#rehydrateSubagentRefs(file.path);
      return hostSession;
    } catch (error) {
      // Failure cleanup (plan §3.3): every installed resource is released
      // independently and idempotently; the original error propagates.
      const cleanup = hostSession;
      await Promise.allSettled([
        (async () => {
          cleanup?.unsubscribe?.();
          if (cleanup) cleanup.unsubscribe = undefined;
        })(),
        (async () => {
          cleanup?.nameUnsubscribe?.();
          if (cleanup) cleanup.nameUnsubscribe = undefined;
        })(),
        (async () => {
          if (cleanup) this.#releaseHostHandles(cleanup);
        })(),
        (async () => {
          const agent = cleanup?.agentSession;
          if (cleanup) cleanup.agentSession = null;
          if (agentCreated && agent) await agent.dispose({ drainTimeoutMs: this.evictDrainTimeoutMs });
        })(),
        (async () => {
          if (!agentCreated) await manager.close().catch(() => {});
        })(),
      ]);
      throw error;
    }
  }

  /**
   * Discovery-set snapshot for a freshly materialized session (plugins.v1).
   * Runs right after createAgentSession so the SDK's discovery caches are warm
   * and return exactly what the session just bound. Failures degrade to null
   * — never block session setup on the settings projection.
   */
  /** 新物化会话的发现集快照（plugins.v1）：紧跟 createAgentSession 执行——此时 SDK 发现缓存是热的，返回的正是该会话刚绑定的集合。失败降级为 null，绝不因设置投影阻塞会话建立。 */
  async #snapshotAppliedPlugins(directoryKey: string) {
    try {
      const { discoverExtensionPaths } = await import('@oh-my-pi/pi-coding-agent/extensibility/extensions');
      const { getEnabledPlugins } = await import('@oh-my-pi/pi-coding-agent/extensibility/plugins');
      const directory = directoryKey ?? process.cwd();
      const [extensionPaths, plugins] = await Promise.all([discoverExtensionPaths([], directory), getEnabledPlugins(directory)]);
      return {
        appliedAt: Date.now(),
        extensionPaths: extensionPaths.map((item) => path.resolve(item)),
        pluginNames: plugins.map((plugin) => plugin.name)
      };
    } catch (error) {
      console.warn('[omp-host] applied-plugins snapshot failed:', errorText(error));
      return null;
    }
  }

  /** Live per-session plugin application snapshots (plugins.v1 projection). */
  /** live 会话的插件应用快照列表（plugins.v1 投影）：仅返回 agentSession 与快照俱在的记录。 */
  appliedPluginsSnapshots(): Array<{ sessionId: string; directory: string } & AppliedPluginsSnapshot> {
    return this.#live
      .snapshot()
      .filter((record) => record.state === 'live' && record.payload)
      .map((record) => record.payload!)
      .filter((hostSession): hostSession is HostSession & { appliedPlugins: AppliedPluginsSnapshot } =>
        Boolean(hostSession.agentSession && hostSession.appliedPlugins))
      .map((hostSession) => ({
        sessionId: hostSession.sessionId,
        directory: hostSession.directory,
        ...hostSession.appliedPlugins
      }));
  }

  /**
   * Hot-reload plugin state for live sessions in a directory (plugins.v1):
   * mirrors omp's `/reload-plugins` — invalidate the process-global discovery
   * caches, republish task/agent definitions, and refresh skills + slash
   * commands on every live session of that directory. TS extension module
   * bindings stay frozen (sessions rebind at next materialization).
   */
  /** 热重载某目录 live 会话的插件状态（plugins.v1）：镜像 omp 的 /reload-plugins——作废进程级发现缓存、重新发布 task/agent 定义、刷新该目录每个 live 会话的 skills 与 slash 命令。TS 扩展模块绑定保持冻结（会话在下次物化时重绑）。 */
  async reloadAppliedPlugins(directory: string | null, sessionId: string | null = null) {
    const directoryKey = normalizeDirectoryKey(directory ?? process.cwd());
    let projectRegistryPath = null;
    try {
      const { resolveActiveProjectRegistryPath } = await import('@oh-my-pi/pi-coding-agent/discovery/helpers');
      projectRegistryPath = await resolveActiveProjectRegistryPath(directoryKey);
    } catch {
      projectRegistryPath = null;
    }
    try {
      const { clearPluginRootsAndCaches } = await import('@oh-my-pi/pi-coding-agent/discovery/helpers');
      clearPluginRootsAndCaches(projectRegistryPath ? [projectRegistryPath] : undefined);
    } catch (error) {
      console.warn('[omp-host] reload cache invalidation failed:', errorText(error));
    }
    try {
      const { refreshAgentDiscovery } = await import('@oh-my-pi/pi-coding-agent/task');
      await refreshAgentDiscovery(directoryKey);
    } catch (error) {
      console.warn('[omp-host] reload agent discovery refresh failed:', errorText(error));
    }
    let sessionsRefreshed = 0;
    for (const hostSession of this.#live.snapshot().map((record) => record.payload).filter((payload): payload is HostSession => payload !== null)) {
      if (hostSession.directory !== directoryKey || !hostSession.agentSession) continue;
      if (sessionId && hostSession.sessionId !== sessionId) continue;
      try {
        await hostSession.agentSession.refreshSkills?.();
        sessionsRefreshed += 1;
      } catch (error) {
        console.warn('[omp-host] reload skills refresh failed:', hostSession.sessionId, errorText(error));
      }
    }
    return { sessionsRefreshed };
  }
  /**
   * omp-native publish helper (spec 05 §5.2.1 envelope; master D6-R1 single
   * channel). Payload never carries directory/sessionID.
   */
  /** omp 原生发布助手（spec 05 §5.2.1 信封；master D6-R1 单通道）：目录与会话 id 由事件作用域携带，载荷本身永不包含它们。 */
  #ompPublish<P extends object>(hostSession: HostSession, type: string, payload: P | null | undefined, { durable }: { durable?: boolean } = {}) {
    return this.ompBus.publish(type, payload, {
      directory: hostSession.directory,
      sessionID: hostSession.sessionId,
      durable: Boolean(durable)
    });
  }

  /**
   * Project + emit one live custom/hook message on both tracks (spec 05
   * §5.1 row 9). Returns the projected wire message id. `display:false`
   * messages emit only the omp event (UI won't build a card; cold projection
   * drops them too — double guard, 05 §5.8.2 T3).
   */
  /** 在双轨道上投影并发出一条 live custom/hook 消息（spec 05 §5.1 行 9）：wire 轨道发 message.updated 与 parts，omp 轨道发 durable 的 omp.custom.appended。display:false 只发 omp 事件（UI 不建卡；冷投影也会丢弃——双重守卫，05 §5.8.2 T3）。返回投影后的 wire 消息 id。 */
  #emitCustomLive(hostSession: HostSession, message: CustomMessage | HookMessage) {
    const { sessionId, directory } = hostSession;
    const projected = projectCustomMessage(message, {
      sessionID: sessionId,
      agent: wireAgentFor(hostSession.currentPersona),
      parentID: hostSession.lastUserWireId || undefined
    });
    const text = textOfContent(message.content);
    if (message.display !== false) {
      this.bus.emit('message.updated', { sessionID: sessionId, info: projected.info }, directory);
      for (const part of projected.parts) {
        this.bus.emit('message.part.updated', { sessionID: sessionId, part, time: Date.now() }, directory);
      }
    }
    this.#ompPublish(
      hostSession,
      'omp.custom.appended',
      {
        message: {
          wireMessageID: projected.info.id,
          customType: message.customType ?? '',
          attribution: message.attribution,
          timestamp: message.timestamp,
          text,
          ...(message.details !== undefined ? { details: message.details } : {}),
          display: message.display !== false
        }
      },
      { durable: true }
    );
    hostSession.syncedEntryKeys?.add(`${message.role}:${message.customType ?? ''}:${message.timestamp}`);
    return projected.info.id;
  }

  /**
   * Tail-sync: project transcript roles that have no dedicated SDK event
   * (custom injected out-of-band, compaction/branch dividers) so they appear
   * live without a refetch (spec 05 §5.5). Idempotent per (role,type,ts).
   * @returns {{ projected: Array<{wireId: string, role: string}>, lastCompactionId: string | null }}
   */
  /** 尾部同步：投影没有专属 SDK 事件的转录角色（带外注入的 custom、压缩/分支分隔条），让它们无需重取即可 live 呈现（spec 05 §5.5）。按（role、customType、timestamp）幂等。 */
  #tailSyncTranscript(hostSession: HostSession) {
    const session = hostSession.agentSession;
    const out: TailSyncTail = { projected: [], lastCompactionId: null };
    if (!session?.messages) return out;
    const messages = session.messages;
    const pending = [];
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      const message = messages[i];
      if (!message || typeof message !== 'object') continue;
      const role = message.role;
      if (role !== 'custom' && role !== 'hookMessage' && role !== 'compactionSummary' && role !== 'branchSummary' && role !== 'developer') continue;
      // SAFETY: only custom/hook messages carry customType; the read is a
      // presence probe keyed into syncedEntryKeys.
      const customTyped = message as { customType?: string; timestamp?: number };
      const key = `${role}:${customTyped.customType ?? ''}:${message.timestamp}`;
      if (hostSession.syncedEntryKeys?.has(key)) break;
      pending.push(message);
    }
    pending.reverse();
    for (const message of pending) {
      // SAFETY: same presence-probe read as the scan pass above.
      const customTyped = message as { customType?: string; timestamp?: number };
      const key = `${message.role}:${customTyped.customType ?? ''}:${message.timestamp}`;
      hostSession.syncedEntryKeys?.add(key);
      if (message.role === 'developer') {
        if (!textOfContent(message.content).trim()) continue;
        const projected = projectDeveloperMessage(message, {
          sessionID: hostSession.sessionId,
          agent: wireAgentFor(hostSession.currentPersona),
          parentID: hostSession.lastUserWireId || undefined
        });
        this.bus.emit('message.updated', { sessionID: hostSession.sessionId, info: projected.info }, hostSession.directory);
        for (const part of projected.parts) {
          this.bus.emit('message.part.updated', { sessionID: hostSession.sessionId, part, time: Date.now() }, hostSession.directory);
        }
        out.projected.push({ wireId: projected.info.id, role: message.role });
        if (message.attribution === 'user') {
          hostSession.lastUserWireId = projected.info.id;
        }
      } else if (message.role === 'compactionSummary' || message.role === 'branchSummary') {
        const projected = projectDividerMessage(message, {
          sessionID: hostSession.sessionId,
          agent: wireAgentFor(hostSession.currentPersona),
          parentID: hostSession.lastUserWireId || undefined
        });
        this.bus.emit('message.updated', { sessionID: hostSession.sessionId, info: projected.info }, hostSession.directory);
        for (const part of projected.parts) {
          this.bus.emit('message.part.updated', { sessionID: hostSession.sessionId, part, time: Date.now() }, hostSession.directory);
        }
        out.projected.push({ wireId: projected.info.id, role: message.role });
        if (message.role === 'compactionSummary') out.lastCompactionId = projected.info.id;
      } else {
        if (message.display === false || !textOfContent(message.content).trim()) continue;
        const wireId = this.#emitCustomLive(hostSession, message);
        out.projected.push({ wireId, role: message.role });
      }
    }
    return out;
  }

  /** Running async bash/eval job ids across all live sessions — the process
   * ledger keeps those invocations' windows open so job-spawned processes
   * still attribute. A subagent job's ownerId lives in its parent session's
   * registry, but the id alone is all the ledger needs. */
  /** 汇总全部 live 会话中运行中的异步 bash/eval 任务 id——进程台账要保持这些调用的窗口开着，任务派生的进程才能正确归属；台账只需要 id 本身。 */
  #runningAsyncJobIds(): string[] {
    const ids = new Set<string>();
    for (const record of this.#live.snapshot()) {
      if (record.state !== 'live' || !record.payload) continue;
      const manager = record.payload.agentSession?.asyncJobManager;
      if (!manager) continue;
      for (const job of manager.getRunningJobs()) {
        if (job.type === 'bash' || job.type === 'eval') ids.add(job.id);
      }
    }
    return [...ids];
  }

  /** Cancel a managed job backing a killed ledger entry. The manager is a
   * singleton attached to the first session (R12), so fall back to scanning
   * live records when the entry's own session does not hold it. */
  /** 取消被杀台账项背后的受管任务：asyncJobManager 是挂在首个会话上的单例（R12），因此条目自己的会话不持有该任务时，回退扫描全部 live 记录。 */
  #cancelAsyncJob(jobId: string, sessionID: string): void {
    for (const record of this.#live.snapshot()) {
      if (record.sessionId !== sessionID) continue;
      const manager = record.payload?.agentSession?.asyncJobManager;
      if (manager?.getJob(jobId)) {
        manager.cancel(jobId);
        return;
      }
    }
    for (const record of this.#live.snapshot()) {
      const manager = record.payload?.agentSession?.asyncJobManager;
      if (manager?.getJob(jobId)) {
        manager.cancel(jobId);
        return;
      }
    }
  }

  /**
   * Full disposition of the SDK AgentSessionEvent union (spec 05 §5.1/§5.1.1,
   * master D2/D6): every one of the 24 members has an explicit case — wire
   * track, omp track, dual, or a justified intentional-ignore. The trailing
   * default is defense-in-depth only; scripts/check-event-coverage.mjs is
   * the real CI guard against unregistered SDK additions.
   */
  /**
   * SDK AgentSessionEvent 联合的完整分派（spec 05 §5.1/§5.1.1，master
   * D2/D6）：24 个成员每个都有显式分支——wire 轨道、omp 轨道、双轨，或有
   * 理由的故意忽略。末尾 default 只是纵深防御；scripts/check-event-coverage.mjs
   * 才是防止 SDK 新增成员漏配的真正 CI 守卫。
   */
  #handleEngineEvent(hostSession: HostSession, event: AgentSessionEvent) {
    // Late-event guard (plan §3.4): after eviction begins, events may still
    // arrive from in-flight SDK emission; they must land on a still-live
    // record only — an evicting/failed record starts no new work.
    const record = this.#live.byKey(hostSession.key);
    if (!record || (record.state !== 'live' && record.state !== 'materializing') || record.payload !== hostSession) return;
    const { sessionId, directory } = hostSession;
    const session = hostSession.agentSession;
    if (!session) return;
    switch (event.type) {
      case 'message_start': {
        if (event.message?.role === 'user') {
          const pending = hostSession.pendingUserWireId;
          hostSession.pendingUserWireId = null;
          if (pending) {
            const canonicalId = wireMessageId('user', event.message.timestamp, textOfContent(event.message.content));
            hostSession.wireIdEchoes.set(canonicalId, pending);
          }
          return;
        }
        if (event.message?.role === 'developer') {
          // Synthetic prompt (prompt(synthetic:true) yields a developer-role
          // message, agent-session.ts:5597): project immediately and occupy
          // the user turn slot so the following assistant message anchors to
          // it. Mark synced so the tail-sync pass never re-emits it.
          const projected = projectDeveloperMessage(event.message, {
            sessionID: sessionId,
            agent: wireAgentFor(hostSession.currentPersona),
            parentID: hostSession.lastUserWireId || undefined
          });
          this.bus.emit('message.updated', { sessionID: sessionId, info: projected.info }, directory);
          for (const part of projected.parts) {
            this.bus.emit('message.part.updated', { sessionID: sessionId, part, time: Date.now() }, directory);
          }
          hostSession.syncedEntryKeys?.add(`developer::${event.message.timestamp}`);
          if (event.message.attribution === 'user') {
            hostSession.lastUserWireId = projected.info.id;
          }
          return;
        }
        if (event.message?.role !== 'assistant') return;
        hostSession.projector = new StreamProjector({
          sessionID: sessionId,
          directory,
          agent: wireAgentFor(hostSession.currentPersona),
          emit: (type, properties, dir) => this.bus.emit(type, properties, dir),
          sharedFinalToolParts: hostSession.finalToolParts
        });
        hostSession.projector.setParentID(hostSession.lastUserWireId ?? '');
        hostSession.projector.startAssistant(event.message);
        return;
      }
      case 'message_update': {
        const projector = hostSession.projector;
        if (!projector || !projector.current) return;
        const inner = event.assistantMessageEvent;
        if (!inner) return;
        if (inner.type === 'text_delta' && typeof inner.delta === 'string') {
          projector.textDelta(inner.delta);
        } else if (inner.type === 'thinking_delta' && typeof inner.delta === 'string') {
          projector.thinkingDelta(inner.delta);
        } else if (inner.type === 'toolcall_end' && inner.toolCall) {
          projector.toolStarted(inner.toolCall.id, inner.toolCall.name, inner.toolCall.arguments);
        }
        return;
      }
      case 'message_end': {
        if (event.message?.role !== 'assistant' || !hostSession.projector) return;
        const finished = hostSession.projector.finishAssistant(event.message, hostSession.turnToolResults ?? new Map());
        // Stable formula (plan phase 5): normally the persisted message keeps
        // the creation timestamp, so its deterministic id IS the id the
        // projector emitted at message_start and no entry is recorded. The
        // SDK's error-normalization paths (empty/failed stream reset) rewrite
        // the timestamp in place; only then does the cold id drift from the
        // live id, and a single echo entry absorbs it.
        const settledAssistantId = finished ? deterministicWireId(event.message) : null;
        if (finished?.id && settledAssistantId !== finished.id) {
          hostSession.wireIdEchoes.set(settledAssistantId ?? '', finished.id);
        }
        if (finished?.id) {
          hostSession.lastAssistantWireId = finished.id;
          const usage = event.message.usage ?? {};
          this.#ompPublish(
            hostSession,
            'omp.usage.turn',
            {
              messageID: finished.id,
              usage,
              ...(event.message.ttft !== undefined ? { ttftMs: event.message.ttft } : {}),
              ...(event.message.duration !== undefined ? { durationMs: event.message.duration } : {}),
              timestamp: event.message.timestamp ?? Date.now()
            },
            { durable: true }
          );
        }
        return;
      }
      case 'tool_execution_start': {
        hostSession.projector?.toolStarted(event.toolCallId, event.toolName, event.args, {
          ...(event.intent ? { title: event.intent } : {})
        });
        this.processLedger?.onToolStart({
          sessionID: sessionId,
          directory,
          toolCallId: event.toolCallId,
          toolName: event.toolName,
          // SAFETY: SDK declares bash/eval args as a typed bag; the ledger's
          // LedgerToolArgs view only reads optional string fields.
          args: (event.args ?? {}) as LedgerToolArgs
        });
        return;
      }
      case 'tool_execution_update': {
        // Partial results (05 §5.6): running-state append; never terminal —
        // tool_execution_end owns completion. The task tool's partial details
        // carry the per-subagent AgentProgress snapshot the transcript renders
        // live; other tools stay text/asyncState-only until a consumer needs
        // their partial details on the wire.
        const partial = typeof event.partialResult === 'string' ? undefined : event.partialResult;
        const partialText = typeof event.partialResult === 'string' ? event.partialResult : (event.partialResult?.text ?? event.partialResult?.output);
        hostSession.projector?.toolPartial(event.toolCallId, {
          text: partialText,
          asyncState: event.partialResult?.details?.async?.state,
          ...(event.toolName === 'task' && partial?.details !== undefined ? { details: partial.details } : {})
        });
        if (typeof partialText === 'string' && partialText) {
          this.processLedger?.onToolUpdate({ sessionID: sessionId, directory, toolCallId: event.toolCallId, text: partialText });
        }
        return;
      }
      case 'tool_execution_end': {
        // The SDK result is an AgentToolResult {content, details}; normalize
        // once so the transient part and the final finishAssistant projection
        // carry the same text output and structured details (spec 03 §5.4.1).
        const { content, text, details } = normalizeToolExecutionResult(event.result);
        const endInput: LedgerToolEnd = {
          sessionID: sessionId,
          directory,
          toolCallId: event.toolCallId,
          toolName: event.toolName,
          isError: Boolean(event.isError)
        };
        if (text) endInput.output = text;
        // SAFETY: details is the SDK's declared result-details bag; the
        // ledger only reads `details.async.jobId`.
        if (details) endInput.details = details as LedgerToolDetails;
        this.processLedger?.onToolEnd(endInput);
        hostSession.projector?.toolFinished(event.toolCallId, {
          output: text,
          error: event.isError ? text || 'Tool error' : undefined,
          ...(details ? { metadata: { details } } : {})
        });
        const results = hostSession.turnToolResults ?? new Map();
        results.set(event.toolCallId, {
          content,
          ...(details ? { details } : {}),
          isError: Boolean(event.isError),
          timestamp: Date.now()
        });
        hostSession.turnToolResults = results;
        // TUI parity (event-controller.ts:1656-1660): the todo tool result's
        // details.phases is the authoritative full list. todo_reminder's
        // payload carries incomplete items only (todo-tracker.ts:269), so
        // without this mapping the todo panel never sees todo writes and a
        // reminder drops completed items from it.
        // SAFETY: boundary cast — the todo tool's details is the SDK's
        // { phases: TodoPhase[] } marker (tools/todo.ts result details);
        // isTodoPhase re-narrows every element before the projection reads it.
        const todoPhases = (details as { phases?: unknown } | undefined)?.phases;
        if (
          event.toolName === 'todo'
          && !event.isError
          && Array.isArray(todoPhases)
          && todoPhases.every(isTodoPhase)
        ) {
          const todos = todoPhases.flatMap((phase) => phase.tasks.map((task) => ({
            content: task.content,
            status: task.status,
            priority: 'medium',
            // ch10 wire 重合面: the SDK task carries the blocker note; the
            // reminder projection is transient (notice.raised), so this
            // mapping is the only carrier that puts it on the wire.
            ...(typeof task.blocker === 'string' && task.blocker ? { blocker: task.blocker } : {}),
          })));
          this.bus.emit('todo.updated', { sessionID: sessionId, todos }, directory);
        }
        return;
      }
      case 'turn_start':
        // Intentional ignore: message_*/tool_execution_* carry the surface;
        // turn boundaries are a TUI-internal concept (05 §5.1.1).
        return;
      case 'turn_end':
        // Intentional ignore: same reasoning as turn_start.
        return;
      case 'agent_start': {
        hostSession.turnToolResults = new Map();
        hostSession.awaitingAsyncSince = null;
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        return;
      }
      case 'agent_end': {
        const projector = hostSession.projector;
        if (projector?.current) {
          const finished = projector.finishAssistant(
            session.getLastAssistantMessage() ?? {
              content: [],
              timestamp: Date.now(),
              usage: {},
              model: ''
            },
            hostSession.turnToolResults ?? new Map()
          );
          if (finished?.id) hostSession.lastAssistantWireId = finished.id;
        }
        // The settled projector STAYS alive: async-job task updates keep
        // arriving after the turn ends and revive the finalized parts via
        // the session-shared finalToolParts map (see toolPartial). Stray
        // message_update deltas cannot bleed in — they only flow during a
        // live turn, and the next assistant message_start replaces the
        // projector wholesale.
        hostSession.turnToolResults = null;
        this.registry.update(directory, sessionId, { timeUpdated: Date.now() });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
        // Transcript roles without dedicated SDK events (dividers, custom
        // notes) tail-sync here, before the busy/idle decision.
        this.#tailSyncTranscript(hostSession);
        if (event.isTerminal === false) {
          // Async delivery will resume the session (05 §5.7): scheduling
          // pause, not completion — keep busy so the queue gate stays
          // closed and notifications stay suppressed.
          hostSession.awaitingAsyncSince = Date.now();
          this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
          this.#ompPublish(hostSession, 'omp.session.settled', { isTerminal: false }, { durable: false });
          return;
        }
        hostSession.awaitingAsyncSince = null;
        this.bus.emit('session.idle', { sessionID: sessionId }, directory);
        return;
      }
      case 'todo_reminder': {
        // SAFETY: SDK todo rows are the wire todo shape (todo tool contract).
        const todos = ((event.todos ?? []) as Array<{ content?: string; status?: string; blocker?: string; priority?: string }>).map((todo) => ({
          content: todo.content ?? '',
          status: todo.status ?? 'pending',
          priority: todo.priority ?? 'medium',
          ...(typeof todo.blocker === 'string' && todo.blocker ? { blocker: todo.blocker } : {}),
        }));
        // Transient reminder surface only (TUI TodoReminderComponent parity:
        // event-controller.ts presents a reminder, never rewrites the todo
        // panel). The event payload lists incomplete items only
        // (todo-tracker.ts:269), so emitting wire todo.updated here would
        // replace the panel's authoritative full list from the todo tool
        // result mapping (tool_execution_end) and drop completed items.
        this.#ompPublish(
          hostSession,
          'omp.notice.raised',
          {
            level: 'info',
            message: `Unfinished todos (${event.attempt ?? 1}/${event.maxAttempts ?? 1}): ${todos
              .map((todo: { content?: string }) => todo.content)
              .filter(Boolean)
              .join('; ')}`
          },
          { durable: false }
        );
        return;
      }
      case 'todo_auto_clear': {
        this.bus.emit('todo.updated', { sessionID: sessionId, todos: [] }, directory);
        return;
      }
      case 'notice': {
        if (event.level === 'error') console.error('[omp-host]', event.message);
        this.#ompPublish(
          hostSession,
          'omp.notice.raised',
          {
            level: event.level,
            message: event.message,
            ...(event.source ? { source: event.source } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_compaction_start': {
        this.#ompPublish(
          hostSession,
          'omp.compaction.started',
          {
            reason: event.reason,
            action: event.action
          },
          { durable: false }
        );
        return;
      }
      case 'auto_compaction_end': {
        const sync = this.#tailSyncTranscript(hostSession);
        this.#ompPublish(
          hostSession,
          'omp.compaction.ended',
          {
            action: event.action,
            aborted: Boolean(event.aborted),
            willRetry: Boolean(event.willRetry),
            ...(event.skipped !== undefined ? { skipped: event.skipped } : {}),
            ...(event.errorMessage ? { errorMessage: event.errorMessage } : {}),
            ...(event.result?.tokensBefore !== undefined ? { tokensBefore: event.result.tokensBefore } : {}),
            ...(sync.lastCompactionId ? { wireMessageID: sync.lastCompactionId } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_retry_start': {
        // P1 (05 §5.3.2): status + superseded overlay only. Zero wire
        // mutation — message.part.removed stays P2-gated (master R14).
        this.bus.emit(
          'session.status',
          {
            sessionID: sessionId,
            status: {
              type: 'retry',
              attempt: event.attempt,
              message: event.errorMessage,
              next: Date.now() + event.delayMs
            }
          },
          directory
        );
        this.#ompPublish(
          hostSession,
          'omp.retry.started',
          {
            attempt: event.attempt,
            maxAttempts: event.maxAttempts,
            delayMs: event.delayMs,
            errorMessage: event.errorMessage,
            ...(hostSession.lastAssistantWireId ? { supersededMessageID: hostSession.lastAssistantWireId } : {})
          },
          { durable: false }
        );
        return;
      }
      case 'auto_retry_end': {
        this.bus.emit('session.status', { sessionID: sessionId, status: { type: 'busy' } }, directory);
        this.#ompPublish(hostSession, 'omp.retry.ended', {
          success: Boolean(event.success),
          attempt: event.attempt,
          ...(event.finalError ? { finalError: event.finalError } : {}),
          // SAFETY: SDK retry-error updates carry the persisted entry ids.
          retryErrors: ((event.retryErrors ?? []) as Array<{ entryId?: string; persistenceKey?: string; note?: string; retryRecovery?: unknown }>).map((update) => ({
            messageID: this.#retryWireIdFor(hostSession, update),
            note: update.note,
            retryRecovery: update.retryRecovery,
          })),
        }, { durable: true });
        return;
      }
      case 'retry_fallback_applied': {
        // Registry truth sync only; the SDK guarantees a follow-up
        // model_changed which emits the wire session.updated (05 §5.4).
        this.registry.update(directory, sessionId, { model: event.to });
        this.#ompPublish(
          hostSession,
          'omp.fallback.applied',
          {
            from: event.from,
            to: event.to,
            role: event.role
          },
          { durable: true }
        );
        return;
      }
      case 'retry_fallback_succeeded': {
        // Success happened on the fallback model; no registry writeback.
        this.#ompPublish(
          hostSession,
          'omp.fallback.succeeded',
          {
            model: event.model,
            role: event.role
          },
          { durable: true }
        );
        return;
      }
      case 'model_changed': {
        const selector = modelSelector(session.model);
        this.registry.update(directory, sessionId, {
          ...(selector ? { model: selector } : {})
        });
        const info = this.#wireSessionFromLive(hostSession);
        this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);
        this.#ompPublish(hostSession, 'omp.model.changed', {
          // Model omitted when unset: the upstream event is payload-less and
          // the TUI re-reads session.model (invalidate + refetch semantics);
          // a JSON null would fail the UI schema and drop the whole frame.
          ...(session.model
            ? { model: { provider: session.model.provider, id: session.model.id } }
            : {}),
          ...(session.thinkingLevel !== undefined ? { thinkingLevel: session.thinkingLevel } : {}),
        }, { durable: true });
        return;
      }
      case 'ttsr_triggered': {
        this.#ompPublish(
          hostSession,
          'omp.ttsr.triggered',
          {
            // SAFETY: ttsr rules are name-keyed config rows.
            rules: ((event.rules ?? []) as Array<{ name?: string }>).map((rule) => ({ name: rule.name }))
          },
          { durable: false }
        );
        return;
      }
      case 'irc_message': {
        this.#emitCustomLive(hostSession, event.message);
        return;
      }
      case 'thinking_level_changed': {
        // thinkingLevel omitted on clear (SDK contract: ThinkingLevel |
        // undefined; the TUI falls back to Off/inherited) — never JSON null.
        this.#ompPublish(hostSession, 'omp.thinking.changed', {
          ...(event.thinkingLevel !== undefined ? { thinkingLevel: event.thinkingLevel } : {}),
          ...(event.configured !== undefined ? { configured: event.configured } : {}),
          ...(event.resolved !== undefined ? { resolved: event.resolved } : {}),
        }, { durable: true });
        return;
      }
      case 'goal_updated': {
        this.modesDomain?.trackerFor(sessionId, directory)?.applyGoalUpdate?.(event.goal, event.state);
        this.#ompPublish(
          hostSession,
          'omp.goal.updated',
          {
            goal: event.goal ?? null,
            ...(event.state !== undefined ? { state: event.state } : {})
          },
          { durable: true }
        );
        return;
      }
      default: {
        // Defense-in-depth only (05 §5.1): the manifest + CI guard own the
        // real coverage check. Never silently swallow an unknown member.
        // SAFETY: exhaustive switch leaves `never`; the probe only reads .type.
        const unknownEvent = event as { type?: string } | null | undefined;
        console.error(`[omp-host] unhandled AgentSessionEvent type: ${unknownEvent?.type}`);
        this.unknownEventCounts = this.unknownEventCounts ?? new Map();
        // Bounded diagnostic keys (plan §6): unknown types fold into a fixed
        // `other` bucket once the key set is full; total drops are counted.
        const type = unknownEvent?.type ?? 'unknown';
        if (this.unknownEventCounts.size >= OmpHostEngine.#UNKNOWN_EVENT_KEYS_MAX && !this.unknownEventCounts.has(type)) {
          this.unknownEventCounts.set('other', (this.unknownEventCounts.get('other') ?? 0) + 1);
        } else {
          this.unknownEventCounts.set(type, (this.unknownEventCounts.get(type) ?? 0) + 1);
        }
        return;
      }
    }
  }

  /** 目录内各 live 会话的状态快照：inFlight（已接受未派发窗口）、流式、等待异步任一命中即 busy，否则 idle；超过 10 分钟未恢复的 awaitingAsync 视为陈旧并清除，避免永久 busy。 */
  async getSessionStatuses({ directory }: { directory?: string }) {
    await this.#boot();
    const AWAITING_ASYNC_TIMEOUT_MS = 10 * 60 * 1000;
    const now = Date.now();
    const statuses: Record<string, { type: 'busy' } | { type: 'idle' }> = {};
    for (const record of this.#live.snapshot()) {
      const live = record.payload;
      if (!live || record.state !== 'live') continue;
      const id = record.sessionId;
      if (live.directory !== normalizeDirectoryKey(directory)) continue;
      const stale = live.awaitingAsyncSince !== null && now - live.awaitingAsyncSince > AWAITING_ASYNC_TIMEOUT_MS;
      if (stale) live.awaitingAsyncSince = null;
      // inFlight covers the accepted-but-not-yet-dispatching prompt window —
      // the image-describe fallback for text-only models parks there for many
      // seconds while the SDK reports nothing streaming.
      statuses[id] = record.inFlight > 0 || live.agentSession?.isStreaming || live.awaitingAsyncSince !== null ? { type: 'busy' } : { type: 'idle' };
    }
    return statuses;
  }

  /** Structured customType inventory for the omp transcript read (05 §5.2.1). */
  /** omp 转录读取的结构化 customType 清单（05 §5.2.1）：把会话全部可见 custom/hook 消息（display:false 除外）投影为含 wireMessageID、customType、text 等字段的结构化行。 */
  async getCustomMessages({ sessionID, directory }: { sessionID: string; directory?: string }) {
    const context = await this.#transcriptContext(sessionID, directory);
    if (!context) return null;
    const out = [];
    for (const message of context.messages ?? []) {
      if (!message || typeof message !== 'object') continue;
      if (message.role !== 'custom' && message.role !== 'hookMessage') continue;
      if (message.display === false) continue;
      const projected = projectCustomMessage(message, { sessionID });
      out.push({
        wireMessageID: projected.info.id,
        customType: message.customType ?? '',
        timestamp: message.timestamp,
        attribution: message.attribution,
        text: textOfContent(message.content),
        ...(message.details !== undefined ? { details: message.details } : {})
      });
    }
    return out;
  }

  /** 会话级用量遥测：每个 assistant 消息一行——稳定公式 id、时间戳、input/output/cache 读写与合计 token、ttft/时长，全部从冷转录上下文读取。 */
  async getTelemetry({ sessionID, directory }: { sessionID: string; directory?: string }) {
    const context = await this.#transcriptContext(sessionID, directory);
    if (!context) return null;
    // Assistant telemetry ids use the stable formula (plan phase 5) — the
    // same id the streaming projector emitted, no echo resolution needed.
    const out = [];
    for (const message of context.messages ?? []) {
      if (!message || message.role !== 'assistant') continue;
      const usage: UsageInput = message.usage ?? {};
      out.push({
        messageID: deterministicWireId(message),
        timestamp: message.timestamp,
        input: usage.input ?? 0,
        output: usage.output ?? 0,
        cacheRead: usage.cacheRead ?? 0,
        cacheWrite: usage.cacheWrite ?? 0,
        ...(usage.reasoningTokens !== undefined ? { reasoningTokens: usage.reasoningTokens } : {}),
        totalTokens:
          (usage.input ?? 0) + (usage.output ?? 0) + (usage.cacheRead ?? 0) + (usage.cacheWrite ?? 0),
        ...(message.ttft !== undefined ? { ttftMs: message.ttft } : {}),
        ...(message.duration !== undefined ? { durationMs: message.duration } : {})
      });
    }
    return out;
  }

  /**
   * Structured session entries (05 §5.2.1): compaction dividers, branch
   * summaries, model/mode changes, ttsr injections, retry recovery notes.
   */
  /** 结构化会话条目（05 §5.2.1）：压缩分隔、分支摘要、模型/模式变更、ttsr 注入、retry 恢复注记。优先单次流式扫描（plan §7.2），null 才退回冷 manager 全量；retry_recovery 行从转录上下文补充。 */
  async getEntries({ sessionID, directory, kinds }: { sessionID: string; directory?: string; kinds?: string[] }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const wanted = new Set(
      String(kinds ?? '')
        .split(',')
        .map((kind) => kind.trim())
        .filter(Boolean)
    );
    // Structured rows + retry-recovery stream from one scan (plan §7.2);
    // null → manager arm below (legacy versions, blob refs).
    const streamedRows = await readSessionEventRows(
      file.path,
      wanted,
      this.#wireIdResolver(directoryKey, sessionID)
    );
    if (streamedRows) return streamedRows;
    const out = await withColdManager(file.path, async (manager) => {
      const rows: unknown[] = [];
      for (const entry of manager.getEntries() ?? []) {
        const kind = entry.type === 'compaction' ? 'compaction' : entry.type === 'branch_summary' ? 'branch_summary' : entry.type === 'model_change' ? 'model_change' : entry.type === 'mode_change' ? 'mode_change' : entry.type === 'ttsr_injection' ? 'ttsr_injection' : null;
        if (!kind || (wanted.size > 0 && !wanted.has(kind))) continue;
        rows.push({
          kind,
          id: entry.id,
          timestamp: Date.parse(entry.timestamp ?? '') || undefined,
          ...(entry.type === 'compaction'
            ? {
                summary: entry.summary,
                tokensBefore: entry.tokensBefore,
                ...(entry.warning ? { warning: entry.warning } : {})
              }
            : {}),
          ...(entry.type === 'branch_summary' ? { fromId: entry.fromId, summary: entry.summary } : {}),
          ...(entry.type === 'model_change' ? { model: entry.model, ...(entry.role ? { role: entry.role } : {}) } : {}),
          ...(entry.type === 'mode_change' ? { mode: entry.mode, ...(entry.data ? { data: entry.data } : {}) } : {}),
          ...(entry.type === 'ttsr_injection' ? { rules: entry.injectedRules } : {}),
        });
      }
      return rows;
    });
    if (wanted.size === 0 || wanted.has('retry_recovery')) {
      const context = await this.#transcriptContext(sessionID, directory);
      for (const message of context?.messages ?? []) {
        if (!message || message.role !== 'assistant' || !message.retryRecovery) continue;
        out.push({
          kind: 'retry_recovery',
          messageID: deterministicWireId(message),
          timestamp: message.timestamp,
          retryRecovery: message.retryRecovery
        });
      }
    }
    return out;
  }

  /** 打开冷 manager 构建 transcript:true 的会话上下文（含压缩前完整历史）；会话文件未知返回 null。 */
  async #transcriptContext(sessionID: string, directory: string | null | undefined) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    return withColdManager(file.path, (manager) => manager.buildSessionContext({ transcript: true }));
  }

  /**
   * 发送一轮对话：接受派发即报 busy（boot/脏重载/物化/扩展 UI 等待可能耗时
   * 数秒），必要时先做脏转录重载与模型切换；persona 变更会经门逐出并以新
   * persona 重建会话后递归重派发。用户消息先投影上总线，再按 TUI 输入循环
   * 语义派发——streamingBehavior 为 steer（注入运行中的轮）或 followUp
   * （排队）；斜杠 skill 命令改走 skill-prompt custom 消息。finally 归还
   * 派发槽，仅当没有流式/等待异步时补发 session.idle。
   */
  async prompt({
    sessionID,
    directory,
    text,
    model,
    agent,
    images,
    delivery,
    messageID,
  }: {
    sessionID: string;
    directory: string;
    text: string;
    model?: { providerID?: string; modelID?: string };
    agent?: string;
    images?: Array<{ data?: string; mimeType?: string }>;
    delivery?: string;
    messageID?: string;
  }): Promise<ProjectedMessage | null> {
    const directoryKey = normalizeDirectoryKey(directory);
    // Accepted for dispatch: report busy immediately. Everything below —
    // boot, dirty-transcript reload, materialize, the extension-UI init wait,
    // and above all the image-describe fallback inside session.prompt for
    // text-only models — can take many seconds while the session still reads
    // idle, and the UI calls an unanswered user message on an idle session a
    // failed send. The in-flight slot below additionally makes the
    // authoritative /session/status snapshot agree, so a poll mid-window
    // cannot lower the status back to idle; the finally hands the real state
    // back when the call ends without a running turn.
    this.bus.emit('session.status', { sessionID, status: { type: 'busy' } }, directoryKey);
    let hostSession: HostSession | null = null;
    let dispatchRecord: LiveRecord<HostSession> | null = null;
    try {
      await this.#boot();
      // Dual-write gate (plan §8.2): when the transcript changed externally in
      // a way the live mirror cannot absorb (dirty rewrite) and the session is
      // inactive, rebuild the writer from disk — bounded to one bounded retry,
      // never during streaming/retry/compaction/async work, and always through
      // the key's gate so no second writer can appear.
      await this.#reloadIfDirty(sessionID, directoryKey);
      hostSession = await this.#materialize(sessionID, directoryKey);
      if (!hostSession) return null;
      const session = hostSession.agentSession;
      if (!session) return null;
      dispatchRecord = this.#live.byKey(hostSession.key);
      if (dispatchRecord && dispatchRecord.state === 'live') {
        // The synchronous dispatch window pins the record against the idle
        // sweep and reports busy to the status snapshot for as long as the
        // call sits in pre-dispatch work (image describe, extension init).
        this.#live.beginUse(dispatchRecord);
        // A prompt is a user-visible write: refresh the idle TTL. The turn's
        // streaming lifetime is guarded by the SDK activity getters.
        this.#live.touch(dispatchRecord);
      }
      if (hostSession.extensionUiPromise) await hostSession.extensionUiPromise;

      // Model switching: resolve and apply when the requested selector differs.
      if (model && (model.providerID || model.modelID)) {
        const target = this.#resolveModel(model);
        if (target && modelSelector(target) !== modelSelector(session.model)) {
          await session.setModel(target).catch((error) => {
            console.error('[omp-host] model switch failed:', errorText(error));
          });
          this.registry.update(directoryKey, sessionID, {
            model: modelSelector(target)
          });
        }
      }

      const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
      // Persona switch (02 §5.1 D-B3, R2-M3): explicit session-level switch —
      // the wire `agent` parameter and registry meta normalize through
      // personaKeyFor, so the deleted build/plan values and unset all mean
      // "standard" and never trigger a rebuild. A switch to a persona that no
      // longer exists is rejected before any state changes: the session keeps
      // its current persona and the message is not dispatched.
      const nextPersona = personaKeyFor(agent ?? meta?.persona ?? meta?.agent);
      if (nextPersona !== 'standard' && !this.personas.has(nextPersona)) {
        throw new ModeDomainError(404, {
          error: 'persona-not-found',
          name: nextPersona
        });
      }
      if (nextPersona !== hostSession.currentPersona) {
        // The persona shapes the session's system prompt and toolset at
        // construction, so rebuild the AgentSession over the same transcript —
        // through the gate: the old session must finish disposing before the
        // replacement materializes (plan §3.4, no double writer).
        const pinned = hostSession;
        const personaRecord = this.#live.byKey(pinned.key);
        await this.#live.withOperation(sessionKey(directoryKey, sessionID), async () => {
          const record = this.#live.byKey(pinned.key);
          if (record && record.state === 'live' && record.payload === pinned) {
            this.#evictRecord(record, 'persona-rebuild', { emitIdle: false });
          }
        });
        if (personaRecord) await this.#awaitDisposalBounded(personaRecord);
        this.registry.update(directoryKey, sessionID, {
          persona: nextPersona === 'standard' ? undefined : nextPersona,
          agent: undefined
        });
        const rebuilt = await this.#materialize(sessionID, directoryKey);
        if (!rebuilt) return null;
        return this.prompt({
          sessionID,
          directory: directoryKey,
          text,
          model,
          agent: nextPersona,
          images,
          delivery,
          messageID
        });
      }

      const content = [];
      if (text) content.push({ type: 'text', text });
      for (const image of images ?? []) {
        content.push({
          type: 'image',
          data: image.data,
          mimeType: image.mimeType || 'image/png'
        });
      }
      const wireOptions: UserProjectionOptions = {
        sessionID,
        agent: wireAgentFor(nextPersona),
        model: session.model,
        // Exact send-time snapshot: the effective level the turn runs with
        // (explicit pick, else the model's default) rides model.variant.
        thinkingLevel: this.#effectiveThinkingLevel(session),
      };
      if (messageID) wireOptions.wireId = messageID;
      const wire = projectUserMessage(
        {
          role: 'user',
          content: content.length === 1 && content[0].type === 'text' ? content[0].text : content,
          timestamp: Date.now()
        },
        wireOptions
      );
      hostSession.pendingUserWireId = messageID || null;
      hostSession.lastUserWireId = wire.info.id;
      this.bus.emit('message.updated', { sessionID, info: wire.info }, directoryKey);
      for (const part of wire.parts) {
        this.bus.emit('message.part.updated', { sessionID, part, time: Date.now() }, directoryKey);
      }

      if (!meta?.timeCreated) {
        this.registry.update(directoryKey, sessionID, {
          timeCreated: wire.info.time.created
        });
      }
      // Title generation mirrors the TUI: attempted at submission time on every
      // user message, while the turn runs. pi skips internally once the session
      // is named and retries later messages when an attempt failed or the input
      // was too low-signal to title. Slash commands never title in the TUI
      // (commands are host-level there); guard them here too — an unguarded
      // "/compact" once titled the session with the entire compaction summary.
      if (!text.trimStart().startsWith('/')) {
        session.maybeStartTitleGeneration(text);
      }

      const textOnly = content.length === 1 && content[0].type === 'text' ? (content[0].text ?? '') : (text ?? '');
      // SAFETY: filtered blocks are image parts; base64+mime is the wire form.
      const imageContents = content.filter((block) => block.type === 'image') as Array<{ type: 'image'; data: string; mimeType: string }>;
      // Dispatch mirrors the TUI input loop: every submission carries a
      // streaming behavior so a live turn never rejects the prompt. steer
      // injects into the running turn (the TUI's Enter-while-streaming);
      // when idle, and routing through prompt() rather than steer() keeps
      // "/" extension commands working mid-turn (steer() rejects them).
      const streamingBehavior = delivery === 'queue' ? 'followUp' : 'steer';
      // TUI/RPC parity (rpc-mode.ts tryRunRpcSkillCommand): a slash command
      // naming a skill runs as a skill-prompt custom message so the transcript
      // carries the invocation card; plain prompt() executes the command with
      // no card at all.
      if (imageContents.length === 0 && await this.#tryRunSkillCommand(hostSession, textOnly, streamingBehavior)) {
        return wire;
      }
      await session.prompt(textOnly, {
        images: imageContents,
        streamingBehavior
      });
      return wire;
    } finally {
      // Terminal activity (streaming, async work) is read from the SDK
      // getters by the sweeper; the dispatch slot must never outlive the
      // dispatch itself — a lost agent_end must not pin the session forever.
      if (dispatchRecord && dispatchRecord.state === 'live') this.#live.endUse(dispatchRecord);
      // The busy emit above covers the accepted-but-not-yet-dispatching
      // window. When the call ended without a running turn — a thrown
      // dispatch, a dropped prompt, a queued followUp on an idle session —
      // hand the real state back so the session never sticks on busy. Read
      // the CURRENT record's payload: a persona rebuild swaps it mid-call,
      // and materialize may have produced nothing at all. Mirrors the
      // getSessionStatuses predicate (post-endUse inFlight is already 0) so
      // a limbo session awaiting an async ack does not get a spurious idle.
      const current = hostSession ? this.#live.byKey(hostSession.key)?.payload : null;
      if (!current || (!current.agentSession?.isStreaming && current.awaitingAsyncSince === null)) {
        this.bus.emit('session.idle', { sessionID }, directoryKey);
      }
    }
  }

  /**
   * `!` local shell execution (07 §3.2 / GAP-G05): runs the command through
   * the session's own BashRunner — the same primitive the TUI's `!` input
   * uses — so the result persists as a `bashExecution` transcript record.
   * The one divergence is `cd`: the TUI's session-cwd move would re-key a
   * session this host pins to its owning directory, so it refuses outright
   * instead of moving bookkeeping nothing can observe. The wire
   * `session.shell` route stays 501 (OpenCode's model-mediated shell
   * semantics do not exist in omp); the UI's shell-mode composer calls the
   * omp-native route that lands here instead.
   *
   * Events: the running row emits immediately as a user-side synthetic
   * `[omp:bash]` card — the same shape `projectExecutionMessage` produces for
   * the settled record — under a dispatch-time live wire id. At settle the
   * record's canonical id is echo-bridged onto it (wireIdEchoes), so every
   * later projection keeps emitting the row the client already has. A
   * mid-turn dispatch defers its record to the SDK's pendingMessages flush;
   * the echo then registers lazily the first time the record appears in the
   * session's messages (see #wireIdResolver).
   */
  /**
   * `!` 本地 shell 执行（07 §3.2 / GAP-G05）：经会话自己的 BashRunner 运行
   * 命令（与 TUI 的 `!` 输入同一原语），结果持久化为 bashExecution 转录
   * 记录。唯一分歧是 `cd`——TUI 的会话 cwd 迁移会重键本宿主钉死在拥有目录
   * 上的会话，故直接拒绝。运行行以派发时 live wire id 立即发出；结算时把
   * 记录的规范 id 回显桥接到它，后续投影持续发出客户端已有的行；轮中派发
   * 延迟到 pendingMessages flush 后由 #wireIdResolver 懒注册回显。
   */
  async executeBash({
    sessionID,
    directory,
    command,
    excludeFromContext,
  }: {
    sessionID: string;
    directory?: string;
    command: string;
    excludeFromContext?: boolean;
  }): Promise<
    | { status: 'ok'; message: ProjectedMessage; result: { output: string; exitCode?: number; cancelled: boolean; truncated: boolean; timedOut: boolean } }
    | { status: 'refused'; error: string }
    | { status: 'notFound' }
  > {
    const directoryKey = normalizeDirectoryKey(directory);
    // `!cd` is the TUI's session-cwd move: the executor runs it in a
    // persistent shell and the controller then relocates the session file
    // (SessionManager.moveTo). This host pins a session to its owning
    // directory — live records, registry meta, and the UI's directory-scoped
    // lists are all keyed by it — so a silent cwd move would strand the
    // session's bookkeeping. Refuse outright instead of reporting a move
    // that never happened (the TUI itself refuses a deferred `cd`).
    if (isPersistentShellCdCommand(command)) {
      return { status: 'refused', error: '`!cd` moves the session working directory, which is not supported here — the session stays pinned to its project directory.' };
    }
    // Accepted-for-dispatch contract, same as prompt(): report busy up front —
    // boot/materialize can take seconds, and the running card below is the UI's
    // visible "working" state while the command executes. beginUse also makes
    // the authoritative /session/status snapshot agree. The finally hands the
    // real state back when nothing else is running.
    this.bus.emit('session.status', { sessionID, status: { type: 'busy' } }, directoryKey);
    let hostSession: HostSession | null = null;
    let dispatchRecord: LiveRecord<HostSession> | null = null;
    try {
      await this.#boot();
      await this.#reloadIfDirty(sessionID, directoryKey);
      hostSession = await this.#materialize(sessionID, directoryKey);
      if (!hostSession) return { status: 'notFound' };
      const session = hostSession.agentSession;
      if (!session) return { status: 'notFound' };
      dispatchRecord = this.#live.byKey(hostSession.key) ?? null;
      if (dispatchRecord && dispatchRecord.state === 'live') {
        // Pin against the idle sweep for the command's whole lifetime: a
        // long-running `!` must not evict the session under it.
        this.#live.beginUse(dispatchRecord);
        this.#live.touch(dispatchRecord);
      }
      if (hostSession.extensionUiPromise) await hostSession.extensionUiPromise;

      const agent = wireAgentFor(hostSession.currentPersona);
      const started = Date.now();
      // The record's canonical id is unknowable until the SDK mints the record
      // timestamp at completion, so the running card carries a dispatch-time
      // live id; the settle step bridges the canonical id to it.
      const liveId = wireMessageId('custom', started, `[omp:bash] ${command}`);
      let output = '';
      const runningProjection = () => projectExecutionMessage(
        { role: 'bashExecution', command, output, timestamp: started },
        { sessionID, agent, wireId: liveId, status: 'running' }
      );
      const running = runningProjection();
      this.bus.emit('message.updated', { sessionID, info: running.info }, directoryKey);
      this.bus.emit('message.part.updated', { sessionID, part: running.parts[0], time: Date.now() }, directoryKey);

      let result: BashResult;
      try {
        result = await session.executeBash(command, (chunk) => {
          output += chunk;
          // Fresh part per emit: queued bus frames serialize lazily, so a
          // mutated part would smuggle later state into earlier events.
          this.bus.emit('message.part.updated', {
            sessionID,
            part: runningProjection().parts[0],
            time: Date.now()
          }, directoryKey);
        }, { excludeFromContext: excludeFromContext === true, useUserShell: true });
      } catch (error) {
        // Settle the running card as an error so the row does not spin
        // forever, then propagate — the route answers 500.
        const failed = projectExecutionMessage(
          { role: 'bashExecution', command, output: errorText(error), timestamp: started },
          { sessionID, agent, wireId: liveId, status: 'error' }
        );
        this.bus.emit('message.updated', { sessionID, info: failed.info }, directoryKey);
        this.bus.emit('message.part.updated', { sessionID, part: failed.parts[0], time: Date.now() }, directoryKey);
        throw error;
      }

      // The SDK appends the bashExecution record before executeBash resolves —
      // except mid-turn (deferred to the pendingMessages flush at turn end) or
      // after a branch transition (detached destination writes only the old
      // branch's file, never session.messages — the live card below is then
      // the whole visible surface, correctly). The full result fingerprint
      // keeps concurrent same-command `!` runs from claiming each other's
      // record: output/exitCode/cancelled land verbatim (#createMessage).
      const record = [...(session.messages ?? [])].reverse().find(
        (m): m is BashExecutionMessage =>
          m.role === 'bashExecution'
          && m.command === command
          && m.output === result.output
          && m.exitCode === result.exitCode
          && m.cancelled === result.cancelled
          && m.timestamp >= started
      );
      const projected = projectExecutionMessage(
        record ?? {
          role: 'bashExecution',
          command,
          output: result.output,
          exitCode: result.exitCode,
          cancelled: result.cancelled,
          timestamp: started
        },
        { sessionID, agent, wireId: liveId }
      );
      if (record) {
        hostSession.wireIdEchoes.set(executionWireId(record), liveId);
      } else {
        // Deferred append (mid-turn `!`): register the echo lazily when the
        // record lands — #wireIdResolver drains this list on each projection.
        (hostSession.pendingShellEchoes ??= []).push({
          liveId,
          command,
          started,
          output: result.output,
          exitCode: result.exitCode,
          cancelled: result.cancelled
        });
      }
      this.bus.emit('message.updated', { sessionID, info: projected.info }, directoryKey);
      this.bus.emit('message.part.updated', { sessionID, part: projected.parts[0], time: Date.now() }, directoryKey);
      return {
        status: 'ok',
        message: projected,
        result: {
          output: result.output,
          exitCode: result.exitCode,
          cancelled: result.cancelled,
          truncated: Boolean(result.truncated),
          timedOut: Boolean(result.timedOut)
        }
      };
    } finally {
      // Same settlement contract as prompt(): the in-flight slot must not
      // outlive the dispatch, and the busy emit above gets a compensating
      // idle only when nothing else is running — a concurrent turn or a
      // second `!` keeps the session busy.
      if (dispatchRecord && dispatchRecord.state === 'live') this.#live.endUse(dispatchRecord);
      const current = hostSession ? this.#live.byKey(hostSession.key)?.payload : null;
      if (!current || (!current.agentSession?.isStreaming && !current.agentSession?.isBashRunning && current.awaitingAsyncSince === null)) {
        this.bus.emit('session.idle', { sessionID }, directoryKey);
      }
    }
  }

  /**
   * Bounded dirty-reload (plan §8.2): classify the transcript against the
   * live record's materialize-time signature; on `dirty` with every activity
   * guard quiet, evict inside the key's gate and let the caller's
   * #materialize rebuild from disk. Reloads are never attempted while the
   * session streams/retries/compacts/holds async work — those turns keep the
   * steer/queue semantics instead, and "absolute freshness" stays best-effort
   * (plan D6).
   */
  /** 有界脏重载（plan §8.2）：对照物化时签名分类转录外部改动；dirty 且全部活动守卫安静时在键的门内逐出，让调用方的 #materialize 从磁盘重建。流式/重试/压缩/持异步工作期间绝不重载——那些轮保持 steer/queue 语义，绝对新鲜度保持尽力而为（plan D6）。 */
  async #reloadIfDirty(sessionID: string, directoryKey: string): Promise<void> {
    const record = this.#live.get(directoryKey, sessionID);
    if (!record || record.state !== 'live' || !record.payload) return;
    if (record.inFlight > 0 || this.#recordIsActive(record)) return;
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return;
    if (classifyExternalChange(record.payload.fileSignature, file.path) !== 'dirty') return;
    console.warn(`[omp-host] transcript for ${sessionID} changed externally; rebuilding live writer from disk`);
    await this.#live.withOperation(sessionKey(directoryKey, sessionID), async () => {
      const current = this.#live.byKey(sessionKey(directoryKey, sessionID));
      if (!current || current.state !== 'live' || !current.payload) return;
      // Recheck inside the gate: activity may have arrived since selection.
      if (current.inFlight > 0 || this.#recordIsActive(current)) return;
      if (classifyExternalChange(current.payload.fileSignature, file.path) !== 'dirty') return;
      this.#evictRecord(current, 'dual-write-reload');
    });
    const after = this.#live.get(directoryKey, sessionID);
    if (after && after.state === 'evicting') await this.#awaitDisposalBounded(after);
  }

  /**
   * Mirror of rpc-mode's tryRunRpcSkillCommand: when the text is a slash
   * invocation of a known skill and skill commands are enabled, send the
   * skill-prompt custom message (display card, user attribution) instead of a
   * plain prompt. Returns false when the text is not a skill command so the
   * normal dispatch proceeds.
   */
  /** rpc-mode tryRunRpcSkillCommand 的镜像：文本是已知 skill 的斜杠调用且 skill 命令已启用时，改发 skill-prompt custom 消息（展示卡、user 归因）而非普通 prompt；否则返回 false 让正常派发继续。 */
  async #tryRunSkillCommand(hostSession: HostSession, text: string, streamingBehavior: "steer" | "followUp") {
    const session = hostSession.agentSession;
    if (!session?.skillsSettings?.enableSkillCommands) return false;
    const parsed = parseSkillInvocation(text);
    if (!parsed) return false;
    const skill = (session.skills ?? []).find((candidate) => candidate?.name === parsed.name);
    if (!skill) return false;
    const built = await buildSkillPromptMessage(skill, parsed.args, 'user');
    await session.promptCustomMessage(
      {
        customType: SKILL_PROMPT_MESSAGE_TYPE,
        content: built.message,
        display: true,
        details: built.details,
        attribution: 'user'
      },
      { streamingBehavior }
    );
    return true;
  }

  /**
   * Session-scoped model switch without sending a turn (spec 01 GAP-02/
   * GAP-04: prompts omit the model; changing it is an explicit setModel).
   * Same resolution + registry bookkeeping as the prompt-time switch.
   * GAP-06: when the target model matches the session's current model, this
   * degrades to a thinking-level-only change (`setThinkingLevel`) — the
   * in-session thinking slot applies through the same endpoint.
   */
  /** 不发轮的会话级模型切换（spec 01 GAP-02/GAP-04：prompt 不携带模型，换模型是显式 setModel）：与 prompt 时相同的解析与注册表簿记。GAP-06：目标与当前模型相同时退化为仅改思考级别——'inherit' 是 OMPChamber 的清除哨兵，映射为 SDK 的 undefined。 */
  async setSessionModel({ sessionID, directory, model, thinkingLevel }: { sessionID: string; directory?: string; model?: { providerID?: string; modelID?: string }; thinkingLevel?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    if (!model || !(model.providerID || model.modelID)) {
      return { ok: false, error: 'model is required' };
    }
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return { ok: false, error: 'session not found' };
    const session = hostSession.agentSession;
    if (!session) return { ok: false, error: 'session not found' };
    const modelRecord = this.#live.byKey(hostSession.key);
    if (modelRecord && modelRecord.state === 'live') this.#live.touch(modelRecord);
    const target = this.#resolveModel(model);
    if (!target) return { ok: false, error: 'unknown model' };
    if (modelSelector(target) !== modelSelector(session.model)) {
      await session.setModel(target).catch((error) => {
        console.error('[omp-host] model switch failed:', errorText(error));
      });
      this.registry.update(directoryKey, sessionID, {
        model: modelSelector(target)
      });
    }
    if (thinkingLevel !== undefined && typeof session.setThinkingLevel === 'function') {
      // SDK contract: setThinkingLevel returns void (agent-session.d.ts:736)
      // — the change is observed through the thinking_level_changed event,
      // never a return value. 'inherit' is OMPChamber's wire sentinel for
      // clearing the explicit level; the SDK clears via undefined.
      try {
        // SAFETY: wire thinking levels are the SDK ThinkingLevel vocabulary
        // ('low'|'medium'|'high'); 'inherit' is the OMP clear sentinel.
        session.setThinkingLevel(thinkingLevel === 'inherit' ? undefined : (thinkingLevel as Parameters<NonNullable<AgentSession['setThinkingLevel']>>[0]));
      } catch (error) {
        console.error('[omp-host] thinking level switch failed:', errorText(error));
      }
    }
    return {
      ok: true,
      model: modelSelector(session.model) ?? modelSelector(target)
    };
  }

  /**
   * 停止会话：有界等待 AgentSession.abort 的收尾（pi 的 abort 路径没有内置
   * 超时，单个无响应工具会永远挂住 Stop 请求）；正常结算时若引擎层的
   * awaitingAsync limbo 仍 busy，则权威地落定 idle。收尾卡死则强制 dispose
   * （pi 会留下 abortInProgress 且不再接受输入），下一次 prompt 从持久化
   * 转录重建 live 会话。
   */
  async abort({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const live = this.#liveHostAnywhere(directory, sessionID);
    if (!live?.agentSession) return false;
    // AgentSession.abort() delivers the cancellation signal synchronously,
    // then awaits the full turn teardown (post-prompt drain + agent idle).
    // pi caps that drain on its dispose paths but not on abort, so a single
    // signal-blind tool call or never-settling post-prompt task parks the
    // await forever — this route then never answered, the stop request hung,
    // and the session stayed busy until a server restart. Bound the wait; a
    // healthy teardown settles well under a second.
    let timeoutTimer: ReturnType<typeof setTimeout> | undefined;
    const settled = await Promise.race([
      live.agentSession.abort({ reason: 'User aborted' }).then(
        () => true,
        (error) => {
          // The cancellation signal was still delivered; a rejected teardown
          // step must not break the stop contract — but leave a trace.
          console.warn('[omp-host] abort teardown rejected:', errorText(error));
          return true;
        }
      ),
      new Promise((resolve) => {
        timeoutTimer = setTimeout(resolve, this.abortTeardownTimeoutMs);
      }).then(() => false)
    ]);
    clearTimeout(timeoutTimer);
    if (settled) {
      // A settled abort with nothing streaming means the busy state was the
      // engine-level awaiting-async limbo: the turn ended with isTerminal
      // false (async delivery was supposed to resume it) and the resume
      // never came, so pi is idle while the session stays busy — Stop looked
      // dead while a new steer "magically" healed it (agent_start clears
      // awaitingAsyncSince). Stop must be authoritative instead: drop the
      // limbo and settle clients, mirroring the terminal agent_end path. A
      // genuine async resume starts with agent_start, which re-raises busy.
      // Optional chaining: a concurrent delete/dispose may have nulled
      // agentSession while the race was pending.
      if (!live.agentSession?.isStreaming && live.awaitingAsyncSince !== null) {
        live.awaitingAsyncSince = null;
        this.bus.emit('session.idle', { sessionID }, live.directory);
      }
      return true;
    }
    // The teardown is stuck and the session is bricked with it (pi leaves
    // #abortInProgress set and ignores further input). Force-dispose: pi's
    // dispose caps its own drains, the next prompt() rebuilds a live session
    // from the persisted transcript, and the emitted session.idle unsticks
    // every client immediately (module invariant: events carry the session's
    // own directory).
    console.warn(
      `[omp-host] abort teardown did not settle within ${this.abortTeardownTimeoutMs}ms; force-disposing session ${sessionID}`
    );
    // Clients learn the live state ended before any drain; then the bounded
    // disposal starts inside the key's gate and is awaited outside it.
    this.bus.emit('session.idle', { sessionID }, live.directory);
    const record = this.#live.get(live.directory, sessionID);
    if (record && record.state === 'live') {
      await this.#live
        .withOperation(record.key, async () => {
          this.#evictRecord(record, 'abort-force-dispose', { emitIdle: false });
        })
        .catch(() => {});
      await this.#awaitDisposalBounded(record);
    }
    return true;
  }

  /** 触发会话压缩（compact）：会话不存在或未物化返回 false。 */
  async summarize({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession?.agentSession) return false;
    await hostSession.agentSession.compact();
    return true;
  }

  /**
   * 从现有转录 fork 新会话：可选 messageID 作为边界（TUI /branch 语义——
   * 所选用户消息及其后内容离开活动路径，边界以不可见 marker 条目固化）；
   * 无边界则保留完整转录（/fork 语义）。fork 谱系写入 forkParentID（而非
   * wire parentID），继承标题/persona/模型并广播 session.created。
   */
  async fork({ sessionID, directory, messageID }: { sessionID: string; directory: string; messageID?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const file = await this.#findSessionFile(sessionID, directoryKey);
    if (!file) return null;
    const forked = await SessionManager.forkFrom(file.path, directoryKey, this.#sessionDirFor(directoryKey));
    const forkId = forked.getSessionId();
    // Wire contract: an optional messageID bounds the fork. TUI /branch
    // semantics — the selected user message and everything after it leave
    // the active path (the caller restores its text into the composer);
    // without one the fork keeps the whole transcript (TUI /fork semantics).
    if (messageID) {
      const entryId = resolveWireIdToEntryId(forked.getEntries?.() ?? [], messageID, {
        wireIdFor: this.#wireIdResolver(directoryKey, sessionID)
      });
      // Compat: native entry ids pass through unchanged (the same fallback
      // revert uses) before giving up and forking at the leaf.
      const boundary = forked.getEntry?.(entryId ?? messageID);
      if (!boundary) {
        console.warn(`[omp-host] fork boundary ${messageID} not found; forking at the leaf`);
      } else {
        // branch()/resetLeaf() move the leaf in memory only — the loader
        // rebuilds the active path from the last physical entry — so an
        // invisible marker entry appended at the new leaf makes the rewind
        // durable. Empty custom entries project to nothing (dropped by the
        // projection's empty-content rule), so the fork's transcript starts
        // clean at the boundary.
        const parentId = boundary.parentId ?? null;
        if (parentId) forked.branch(parentId);
        else forked.resetLeaf();
        forked.appendCustomEntry('ompchamber.forkBoundary', {
          from: sessionID,
          at: messageID
        });
      }
    }
    const now = Date.now();
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    this.registry.update(directoryKey, forkId, {
      // Fork lineage for the session-tree projection (§5.4). NOT wire
      // `parentID` — that field is subagent parentage, and the shared UI
      // flips sessions carrying it into a read-only subagent composer.
      forkParentID: sessionID,
      title: meta?.title ? `${meta.title} (fork)` : 'Forked session',
      timeCreated: now,
      timeUpdated: now,
      ...(meta?.persona ? { persona: meta.persona } : {}),
      ...(meta?.agent ? { agent: meta.agent } : {}),
      ...(meta?.model ? { model: meta.model } : {})
    });
    await forked.close();
    const session = this.#wireSession(
      {
        id: forkId,
        cwd: directoryKey,
        created: new Date(now),
        modified: new Date(now)
      },
      directoryKey,
      (this.registry.get(directoryKey, forkId) ?? undefined)
    );
    this.bus.emit('session.created', { sessionID: forkId, info: session }, directoryKey);
    return session;
  }

  /**
   * Revert: move the transcript's active branch so `messageID` becomes the
   * last retained message. Records the previous leaf for unrevert.
   */
  /** 回退：移动转录的活动分支使 messageID 成为最后保留的消息；前一个叶子 id 记入注册表供 unrevert 使用。 */
  async revert({ sessionID, directory, messageID }: { sessionID: string; directory?: string; messageID: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession?.agentSession) return null;
    const manager = hostSession.agentSession.sessionManager;
    // The UI sends the wire message id it read from GET messages; branch()
    // wants the engine entry id. Resolve through the same projection the UI
    // saw (native ids pass through unchanged for compat).
    const entryId = resolveWireIdToEntryId(manager.getEntries?.() ?? [], messageID, {
      wireIdFor: this.#wireIdResolver(directoryKey, sessionID)
    });
    manager.branch(entryId ?? messageID);
    const previousLeaf = manager.getLeafId() ?? messageID;
    this.registry.update(directoryKey, sessionID, {
      revert: { messageID, previousLeaf },
      timeUpdated: Date.now()
    });
    const session = this.#wireSessionFromLive(hostSession);
    session.revert = { messageID };
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
  }

  /**
   * Live-session extension commands for one directory (09 §5.4 discovery
   * gap): the headless AvailableCommandsSession has no extension runner, so
   * `pi.registerCommand` commands (e.g. user extensions in
   * ~/.omp/agent/extensions) only exist on materialized sessions. The
   * extension factory runs at session creation, so any live session for the
   * directory is a valid source.
   */
  /** 某目录 live 会话的扩展命令（09 §5.4 发现缺口）：headless AvailableCommandsSession 没有 extension runner，pi.registerCommand 命令只存在于已物化会话；扩展工厂在会话创建时运行，该目录任一 live 会话都是有效来源。 */
  liveCommandsFor(directory: string | null) {
    const directoryKey = normalizeDirectoryKey(directory);
    for (const hostSession of this.#live.snapshot().map((record) => record.payload).filter((payload): payload is HostSession => payload !== null)) {
      if (hostSession.directory !== directoryKey) continue;
      const session = hostSession.agentSession;
      if (!session?.extensionRunner) continue;
      try {
        const commands = getSessionSlashCommands(session) ?? [];
        return Promise.resolve(
          commands.map((command) => ({
            name: command.name,
            ...(typeof command.description === 'string' && command.description ? { description: command.description } : {}),
            source: command.source ?? 'extension'
          }))
        );
      } catch {
        return Promise.resolve([]);
      }
    }
    return Promise.resolve([]);
  }

  /** 撤销回退：用注册表记录的 previousLeaf 恢复活动分支（无记录则 resetLeaf 到物理叶子），清除 revert 元数据并广播 session.updated。 */
  async unrevert({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = await this.#materialize(sessionID, directoryKey);
    if (!hostSession) return null;
    const meta = (this.registry.get(directoryKey, sessionID) ?? undefined);
    const previousLeaf = meta?.revert?.previousLeaf;
    if (!hostSession.agentSession) return null;
    const manager = hostSession.agentSession.sessionManager;
    if (previousLeaf) {
      manager.branch(previousLeaf);
    } else {
      manager.resetLeaf();
    }
    this.registry.update(directoryKey, sessionID, {
      revert: undefined,
      timeUpdated: Date.now()
    });
    const session = this.#wireSessionFromLive(hostSession);
    this.bus.emit('session.updated', { sessionID, info: session }, directoryKey);
    return session;
  }

  /** 读取会话当前 todo 列表：取 SDK TodoPhase 最新阶段的 tasks（SDK 18+ 字段名），缺省补 status/priority。 */
  async getTodos({ sessionID, directory }: { sessionID: string; directory?: string }) {
    await this.#boot();
    const directoryKey = normalizeDirectoryKey(directory);
    const hostSession = this.#liveHostAnywhere(directoryKey, sessionID);
    if (!hostSession?.agentSession) return [];
    const phases = hostSession.agentSession.getTodoPhases();
    const latest = phases[phases.length - 1];
    // SDK TodoPhase carries `tasks`; `items`/`todos` were pre-18 field names
    // that made this read return [] unconditionally. `priority` is not a
    // TodoItem field in the SDK but legacy transcripts may still carry it.
    const todos: Array<{ content: string; status: string; priority?: string }> = latest?.tasks ?? [];
    return todos.map((todo) => ({
      content: todo.content ?? '',
      status: todo.status ?? 'pending',
      priority: todo.priority ?? 'medium'
    }));
  }

  /**
   * Counters-only stream diagnostics (docs/plan.md §9.1, phase 0 slice).
   * Returns sizes and estimates — never transcripts, payloads, paths, or
   * credentials. All "bytes" fields are the buses' declared serialized
   * estimates (UTF-16 units), not JS-heap measures; `process.*Bytes` are
   * Node's own memoryUsage counters. The data-proportional counts are
   * labeled as such (plan §6): they grow with live data by design and are
   * not bounded caches.
   *
   * §9.1 also asks for OS handle and child-process counts. Under Bun,
   * `process._getActiveHandles()`/`getActiveResourcesInfo()` are stubs that
   * always return `[]` — reporting them would masquerade as authoritative
   * zeros, so they are deliberately absent. Child processes are covered by
   * the external sampler scripts/perf/process-tree-sample.mjs instead.
   */
  /** 仅计数器的流诊断（docs/plan.md §9.1 phase 0 切片）：只返回尺寸与估计——绝不返回转录、载荷、路径或凭据。bytes 字段是总线声明的序列化估计（UTF-16 单位）而非 JS 堆测量；process.* 是 Node memoryUsage 计数器；随数据增长的计数显式标注（plan §6）。 */
  getStreamDiagnostics() {
    const memory = process.memoryUsage();
    const now = this.#live.now();
    const records = this.#live.snapshot();
    // Per-record rows let a UI attribute the resident footprint. The payload's
    // fileSignature.size is the transcript-size proxy; null once the payload
    // is detached (evicting/failed — the bytes are still held but no longer
    // attributed to a known file).
    const sessions = records.slice(0, OmpHostEngine.#DIAGNOSTIC_SESSION_ROWS_MAX).map((record) => ({
      id: record.sessionId,
      directory: record.directory,
      state: record.state,
      inFlight: record.inFlight,
      idleMs: Math.max(0, now - record.lastUsedAt),
      transcriptBytes: record.payload?.fileSignature?.size ?? null,
      failure: record.failure ? { reason: record.failure.reason, attempts: record.failure.attempts } : null,
    }));
    return {
      wireBus: this.bus.stats(),
      ompBus: this.ompBus.stats(),
      dataProportional: {
        liveSessions: {
          ...this.#live.stats(),
          sessions,
          truncated: records.length > sessions.length,
        },
        wireIdEchoes: records.reduce((sum, record) => sum + (record.payload?.wireIdEchoes.size ?? 0), 0),
        personas: this.personas.size,
      },
      process: {
        heapUsedBytes: memory.heapUsed,
        externalBytes: memory.external,
        arrayBufferBytes: memory.arrayBuffers,
        rssBytes: memory.rss,
      },
    };
  }

  /**
   * Graceful shutdown (plan §3.4): close the intake, synchronously
   * beginDispose every live record (inside each key's gate), then wait for
   * all disposals under one global deadline. Records whose disposal has not
   * settled by the deadline stay quarantined in the registry — observably
   * failed, still blocking new writers — instead of being cleared and
   * masquerading as released. `sessions.clear()` before these steps was not
   * a shutdown.
   */
  /**
   * 优雅关停（plan §3.4）：关闭 intake，在每个键的门内同步 beginDispose 全部
   * live 记录，再在单一全局截止下等待全部 dispose。截止时仍未结算的记录保持
   * 隔离——可观察的 failed、仍阻止新写方——而不是清掉伪装成已释放。最后
   * 尽力释放各领域与设置存储。
   */
  async shutdown() {
    this.#closing = true;
    clearInterval(this.sweeper);
    const records = this.#live.snapshot();
    let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise((resolve) => {
      deadlineTimer = setTimeout(resolve, this.shutdownDisposeDeadlineMs);
    });
    try {
      await Promise.all(
        records.map((record) =>
          this.#live
            .withOperation(record.key, async () => {
              const current = this.#live.byKey(record.key);
              if (!current || current.state !== 'live') return;
              const disposal = this.#evictRecord(current, 'shutdown', { emitIdle: false });
              // The gate body itself is bounded by the global deadline: a
              // disposal that never settles must not park shutdown (the
              // record simply stays `evicting` — quarantined, observable).
              await Promise.race([disposal, deadline]);
            })
            .catch((error) => {
              console.warn('[omp-host] shutdown dispose failed:', errorText(error));
            }),
        ),
      );
    } finally {
      // An early-settled shutdown must not leave the deadline armed — the
      // live timer would keep the event loop (and the process) alive for
      // the full window after everything already drained.
      clearTimeout(deadlineTimer);
    }
    const quarantined = this.#live.stats();
    if (quarantined.evicting > 0 || quarantined.failed > 0) {
      console.warn(
        `[omp-host] shutdown: ${quarantined.evicting} disposal(s) still draining, ${quarantined.failed} quarantined — restart clears them`,
      );
    }
    try {
      await this.dialogs?.dispose?.('omp-host shutdown');
    } catch {
      // Settle-all is best-effort at shutdown.
    }
    this.uriDomain?.dispose?.();
    this.processLedger?.dispose();
    try {
      await this.settingsStore?.disposeAll?.();
    } catch {
      // Flush is best-effort at shutdown.
    }
  }

  /** SDK model rows (registry-backed); the typed read view for projections. */
  /** SDK 模型行（注册表支撑）；投影消费的类型化读取视图。 */
  availableModels(): RegistryModel[] {
    // SAFETY: SDK Model is a structural superset of RegistryModel
    // (nullable size fields are admitted on the read view).
    return this.#sdkModels() as RegistryModel[];
  }

  /** 模型注册表的原始可用模型列表；注册表未就绪时为空数组。 */
  #sdkModels() {
    return this.modelRegistry?.getAvailable() ?? [];
  }

  /**
   * Reload models from disk (builtin + custom models.yml). Static inputs are
   * mtime-checked inside ModelRegistry.#reloadStaticModels, so a no-op when
   * nothing changed. 'offline' skips network discovery — the provider CRUD
   * domain calls this after writing models.yml so GUI edits are live without
   * a host restart.
   */
  /** 从磁盘重载模型（builtin + 自定义 models.yml）：静态输入在 ModelRegistry 内部做 mtime 检查，无变化即 no-op。'offline' 跳过网络发现——provider CRUD 领域写完 models.yml 后调用它，GUI 编辑无需重启即生效。 */
  async refreshModels() {
    await this.#boot();
    if (!this.modelRegistry) return;
    await this.modelRegistry.refresh('offline');
  }

  /** Public boot barrier for endpoint handlers that need registry state. */
  /** 端点处理器使用的公开 boot 屏障：等待引擎引导（认证/模型/设置）完成。 */
  async ready() {
    await this.#boot();
  }

  /** 目录键到项目 id 的公开访问器（归一化后走 #projectId）。 */
  projectIdFor(directoryKey: string) {
    return this.#projectId(normalizeDirectoryKey(directoryKey));
  }

  /**
   * Move a session to another project directory: relocate the transcript via
   * omp's SessionManager.moveTo and migrate the sidecar metadata. A live
   * session is evicted (awaited, inside the owning key's gate) first — the
   * old cold-path opened a second writable manager while a live writer held
   * the same file (plan §3.4). Ownership transfers cold; the next prompt in
   * the destination materializes fresh.
   */
  /**
   * 把会话移动到另一项目目录：经 omp SessionManager.moveTo 迁移转录并迁移
   * sidecar 元数据。live 会话先在拥有键的门内逐出（有界等待）——旧冷路径
   * 会在 live 写方持有同一文件时打开第二个可写 manager（plan §3.4）。
   * 所有权冷转移；目标目录的下一次 prompt 重新物化。
   */
  async moveSession({ sessionID, destination }: { sessionID: string; destination: string }) {
    await this.#boot();
    const toKey = normalizeDirectoryKey(destination);
    const live = this.#liveHostById(sessionID);
    const fromKey = live ? normalizeDirectoryKey(live.directory) : ((await this.#locateDirectory(sessionID)) ?? null);
    if (!fromKey) return null;
    const key = sessionKey(fromKey, sessionID);
    // Evict → bounded disposal wait → transcript relocation ALL inside the
    // owning key's gate: an interleaved materialize would open a second
    // writer on the file being moved, and a disposal that fails or times
    // out must refuse the move instead of relocating under a live writer
    // (plan §3.4). Ownership transfers cold; the next prompt in the
    // destination materializes fresh.
    await this.#live.withOperation(key, async () => {
      const record = this.#live.get(fromKey, sessionID);
      if (record && record.state === 'live') {
        this.#evictRecord(record, 'move', { emitIdle: true });
      }
      const blocking = this.#live.byKey(key);
      if (blocking) {
        const outcome = await this.#awaitDisposalBounded(blocking);
        if (outcome !== 'disposed') {
          throw new SessionBusyError(
            outcome === 'failed' ? 'session-failed' : 'session-evicting',
            `session ${sessionID} is not movable: disposal ${outcome === 'timeout' ? 'did not settle' : `failed (${blocking.failure?.reason ?? 'unknown'})`}`,
            sessionID,
          );
        }
      }
      const file = await this.#findSessionFile(sessionID, fromKey);
      if (file) {
        // The relocating manager is a cold read of the same file: close +
        // release in finally, same contract as every other cold open.
        await withColdManager(file.path, (manager) => manager.moveTo(toKey, this.#sessionDirFor(toKey)));
      }
      this.registry.move(fromKey, toKey, sessionID);
    });
    this.#maybeReleaseDirectoryState(fromKey);
    const session = await this.getSession({ sessionID, directory: toKey });
    if (session) this.bus.emit('session.updated', { sessionID, info: session }, toKey);
    return session;
  }

  /** 定位会话的拥有目录：live 记录直接读；同 id 在两个目录 live 时返回 null（拒绝猜测）；否则全目录列表查找，未找到返回 null。 */
  async #locateDirectory(sessionID: string) {
    const record = this.#live.bySessionId(sessionID);
    if (record === undefined) return null; // same id live in two directories — refuse to guess
    if (record !== null) return record.directory;
    const byDirectory = await this.listAllSessions({});
    for (const [directory, list] of byDirectory) {
      if (list.some((session: { id: string }) => session.id === sessionID)) return directory;
    }
    return null;
  }

  /** Unique live payload by id (null-ambiguous on two-directory duplicates). */
  /** 按 id 取唯一 live 载荷；同 id 两目录并存（歧义）时返回 null。 */
  #liveHostById(sessionId: string): HostSession | null {
    const record = this.#live.bySessionId(sessionId);
    return record && record.state === 'live' ? record.payload : null;
  }

  /** Test/diagnostics view: live record access by id. */
  /** 测试/诊断视图：按 id（可带目录）访问 live 记录本体。 */
  liveRecord(sessionId: string, directory?: string): LiveRecord<HostSession> | null {
    if (directory) return this.#live.get(directory, sessionId);
    const record = this.#live.bySessionId(sessionId);
    return record === undefined ? null : record;
  }
}
