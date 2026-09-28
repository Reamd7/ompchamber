// Pure projection from omp agent state to the OpenCode-compatible wire model.
//
// Determinism contract: wire message ids are derived from the omp message
// identity (role + timestamp + content digest), NOT from entry persistence.
// Live-streaming projection and cold re-projection therefore produce the same
// ids for the same conversation, which the UI's session-message loader relies
// on when it merges live events into fetched history.
//
// Part ids are `prt_<messageId>_<seq>` where seq is the part creation index —
// identical between live creation order (events arrive in content order) and
// cold content-array order.
/**
 * 【模块说明】omp agent 状态 → OpenCode 兼容 wire 模型的纯投影（projection）层。
 *
 * 确定性契约：wire 消息 id 由 omp 消息身份（role + 时间戳 + 内容摘要）推导，
 * 与 entry 持久化无关。因此"直播流式投影"与"冷启动重投影"对同一会话产出
 * 完全相同的 id —— UI 的会话消息加载器在把直播事件合并进已拉取的历史时
 * 依赖这一性质。
 *
 * part id 形如 `prt_<messageId>_<seq>`，其中 seq 为 part 的创建序号 ——
 * 直播创建顺序（事件按内容顺序到达）与冷启动 content 数组顺序一致。
 */

import crypto from 'node:crypto';
import type {
  AgentSessionEvent,
  BranchSummaryMessage,
  CompactionSummaryMessage,
  CustomMessage,
  HookMessage,
  SessionEntry,
} from '@oh-my-pi/pi-coding-agent';

// ---------------------------------------------------------------------------
// Input contracts — the fields the projectors actually read off omp messages.
// Content blocks are the flat read-view of the SDK block union (text / image /
// thinking / toolCall); messages are the dispatchable role set of the session
// transcript.
// ---------------------------------------------------------------------------
/**
 * Tool-call arguments exactly as the SDK reports them on the wire
 * (`tool_execution_start.args`: an open string-keyed map). The projectors
 * never read individual argument values — they store the map and re-emit it
 * verbatim into wire tool state, so the SDK's own event type is the owner
 * contract.
 */
/**
 * 工具调用参数的 wire 形态：直接取 SDK `tool_execution_start.args`
 * 的开放字符串键映射。投影器不读取具体参数值，只整包存取并原样
 * 回放进 wire tool state，因此 SDK 自身的事件类型即唯一契约所有者。
 */
export type ToolCallArguments = Extract<AgentSessionEvent, { type: 'tool_execution_start' }>['args'];

 /** One content block as the projectors read it (flat view of the SDK block union). */
/** 投影器读取视角下的单个内容块（SDK block 联合类型的扁平视图）。 */
export interface ProjectedContentBlock {
  /** 块类型标记（'text' | 'thinking' | 'image' | 'toolCall' 等）。 */
  type: string;
  /** text 块的正文。 */
  text?: string;
  /** thinking 块的推理文本。 */
  thinking?: string;
  /** toolCall 块的工具名。 */
  name?: string;
  /** image 块的 base64 数据。 */
  data?: string;
  /** image 块的 MIME 类型。 */
  mimeType?: string;
  /** toolCall 块的调用 id（与 ToolResult 配对的键）。 */
  id?: string;
  /** toolCall 块的参数（对象或字符串化 JSON）。 */
  arguments?: ToolCallArguments | string;
  /** 模型对该次调用陈述的意图（工具行的人类可读标题）。 */
  intent?: string;
}

/** Message content as the projectors accept it: bare text or a block list. */
/** 投影器接受的消息内容形态：纯文本字符串或内容块列表。 */
export type ProjectedContentInput = string | readonly ProjectedContentBlock[];

/** omp usage report fields the usage projection reads (all optional; callers pass `{}` fallbacks). */
/** 用量投影读取的 omp usage 上报字段（全部可选；调用方以空对象兜底）。 */
export interface UsageInput {
  /** 输入 token 数。 */
  input?: number;
  /** 输出 token 数。 */
  output?: number;
  /** 推理 token 数。 */
  reasoningTokens?: number;
  /** 缓存读取 token 数。 */
  cacheRead?: number;
  /** 缓存写入 token 数。 */
  cacheWrite?: number;
  /** SDK Usage.totalTokens — the authoritative final-round-trip window
   * (input+output+cacheRead+cacheWrite + orchestration). Emitted as the wire
   * `tokens.total` so the UI context meter prefers it over summing buckets
   * (OpenCode-wire precedent; TUI computes context from session accounting
   * instead — see docs/plans/omp-host-field-loss/fix-plan.md P7). */
  /** SDK Usage.totalTokens —— 最绔回合窗口的权威值（input+output+cache 读写 + 编排开销）；作为 wire tokens.total 下发，UI 上下文计量优先采用。 */
  totalTokens?: number;
  /** omp reports per-message cost as a number; the SDK usage object carries a cost breakdown. */
  /** 单消息成本：omp 以数值上报；SDK usage 对象则携带成本细分。 */
  cost?: number | { input?: number; output?: number; cacheRead?: number; cacheWrite?: number; total?: number };
}

/** omp UserMessage (role + content + timestamp is the whole projection-relevant shape). */
/** omp UserMessage —— 投影相关字段仅 role + content + timestamp。 */
export interface UserMessageInput {
  /** 角色字面量：'user'。 */
  role: 'user';
  /** 消息内容（文本或块列表）。 */
  content?: ProjectedContentInput;
  /** System-injected (auto-continue etc.); TUI renders dimmed/collapsed. */
  /** 系统注入标记（自动续跑等）；TUI 以暗色/折叠渲染。 */
  synthetic?: boolean;
  /** 创建时间戳（ms）。 */
  timestamp: number;
}

/** omp DeveloperMessage — harness-injected transcript input; projected as
 * synthetic rows (attribution 'user' occupies the user turn slot, anything
 * else rides the current turn as an assistant-side note). */
/** omp DeveloperMessage —— harness 注入的转写输入；投影为合成行（attribution 'user' 占据用户回合槽位，其余作为助手侧附注挂在当前回合）。 */
export interface DeveloperMessageInput {
  /** 角色字面量：'developer'。 */
  role: 'developer';
  /** 注入内容。 */
  content?: ProjectedContentInput;
  /** Injection origin ('user' synthetic prompts, 'agent' mid-turn nudges). */
  /** 注入来源：'user'（合成提示）/ 'agent'（回合中段 nudge）。 */
  attribution?: 'user' | 'agent';
  /** 创建时间戳（ms）。 */
  timestamp: number;
}

/** omp AssistantMessage fields the assistant projector reads. */
/** 助手投影器读取的 omp AssistantMessage 字段。 */
export interface AssistantMessageInput {
  /** 角色字面量：'assistant'（可选）。 */
  role?: 'assistant';
  /** 内容块列表（text / thinking / image / toolCall）。 */
  content?: readonly ProjectedContentBlock[];
  /** 创建时间戳（ms）。 */
  timestamp: number;
  /** 模型选择器字符串（provider/model 形态）。 */
  model?: string;
  /** 提供商 id（优先于从 model 拆分的结果）。 */
  provider?: string;
  /** 本回合的用量上报。 */
  usage?: UsageInput;
  /** 终止原因（'stop' | 'length' | 'toolUse' | 'error' | 'aborted'）。 */
  stopReason?: string;
  /** Tool calls stripped by branch/fork history rewrite (StrippedToolCallsMarker). */
  /** 分支/分叉历史重写剥离的工具调用数（StrippedToolCallsMarker）。 */
  strippedToolCalls?: number;
  /** 回合错误消息（映射为 wire error 字段）。 */
  errorMessage?: string;
}

/** omp ToolResultMessage as the pairing map stores it. */
/** 配对表存储形态的 omp ToolResultMessage。 */
export interface ToolResultMessageInput {
  /** 角色字面量：'toolResult'。 */
  role: 'toolResult';
  /** 对应工具调用的 id（配对键）。 */
  toolCallId: string;
  /** 结果内容块。 */
  content?: readonly ProjectedContentBlock[];
  /** 结构化详情（如 ask 工具的 AskToolDetails）。 */
  details?: unknown;
  /** 工具报错标记。 */
  isError?: boolean;
  /** 结果时间戳（工具 part 的结束时间）。 */
  timestamp?: number;
}

/** `!`/`$` shell-kernel execution message (bash or python role). */
/** `!`/`$` shell 内核执行消息（bash 或 python 角色）。 */
export interface ShellExecutionMessageInput {
  /** 角色字面量：'bashExecution' | 'pythonExecution'。 */
  role: 'bashExecution' | 'pythonExecution';
  /** bash 执行的命令行。 */
  command?: string;
  /** python 执行的代码。 */
  code?: string;
  /** 合并后的输出文本。 */
  output?: string;
  /** 退出码（存在即表示已终止）。 */
  exitCode?: number;
  /** 用户取消标记。 */
  cancelled?: boolean;
  /** `!!`/`$$` marker: the record stays out of model context. */
  /** `!!`/`$$` 标记：该记录不进入模型上下文。 */
  excludeFromContext?: boolean;
  /** 创建时间戳（ms）。 */
  timestamp: number;
}

/** fileMention message files as the projector reads them. */
/** 投影器读取形态的 fileMention 文件项。 */
export interface FileMentionFileInput {
  /** 文件路径。 */
  path?: string;
  /** 行数（展示为 "(N lines)"）。 */
  lineCount?: number;
}

/** omp fileMention message. */
/** omp fileMention 消息。 */
export interface FileMentionMessageInput {
  /** 角色字面量：'fileMention'。 */
  role: 'fileMention';
  /** 被引用的文件列表。 */
  files?: readonly FileMentionFileInput[];
  /** 创建时间戳（ms）。 */
  timestamp: number;
}

/** Dispatchable message set of a session transcript (SDK custom/divider types imported as-is). */
/** 会话转写中可分发的消息集合（SDK custom/divider 类型按原样导入）。 */
export type MessageInput =
  | UserMessageInput
  | DeveloperMessageInput
  | AssistantMessageInput
  | ToolResultMessageInput
  | CustomMessage
  | HookMessage
  | CompactionSummaryMessage
  | BranchSummaryMessage
  | ShellExecutionMessageInput
  | FileMentionMessageInput;

/** Minimal message shape a wire-id derivation needs (role + content + timestamp). */
/** wire id 推导所需的最小消息形态（role + content + timestamp）。 */
export interface WireIdMessageInput {
  /** 消息角色（决定 id 的角色前缀字符）。 */
  role?: string;
  /** 内容（非 assistant 角色时参与摘要种子）。 */
  content?: ProjectedContentInput;
  /** 创建时间戳（base36 编码进 id）。 */
  timestamp: number;
}

/** Resolves a message to a pre-issued wire id (live/cold id bridge); undefined = derive deterministically. */
/** 把消息解析为预签发的 wire id（直播/冷启动 id 桥）；返回 undefined 表示走确定性推导。 */
export type WireIdResolver = (message: WireIdMessageInput) => string | undefined;

// ---------------------------------------------------------------------------
// Output contracts — the OpenCode-compatible wire shapes the engine emits on
// the host bus and the UI's message loader consumes.
// ---------------------------------------------------------------------------

/** `{ providerID, modelID }` selector as the wire model reports a model. */
/** wire 模型上报模型时使用的 `{ providerID, modelID }` 选择器。 */
export interface WireModelSelector {
  /** 提供商 id。 */
  providerID: string;
  /** 模型 id。 */
  modelID: string;
  /** Thinking-level variant snapshot (send-time effort slot). */
  /** 思考档位快照（发送时的 effort 槽位）。 */
  variant?: string;
}

/** Model reference accepted on user-message options: selector string or model object. */
/** 用户消息选项接受的模型引用：选择器字符串或模型对象。 */
export type ModelSelectorInput = string | { provider?: string; id?: string };

/** Wire token totals (per-message projection of an omp usage report). */
/** wire token 汇总（单条消息对 omp usage 上报的投影）。 */
export interface WireTokenTotals {
  /** 输入 token 数。 */
  input: number;
  /** 输出 token 数。 */
  output: number;
  /** 推理 token 数。 */
  reasoning: number;
  /** 缓存读写 token 汇总。 */
  cache: { read: number; write: number };
  /** Final-round-trip window when the SDK reported it; omitted otherwise. */
  /** SDK 上报的最绔回合窗口；否则省略。 */
  total?: number;
}

/** `projectUsage` result. */
/** projectUsage 的返回结构。 */
export interface WireUsageProjection {
  /** token 汇总。 */
  tokens: WireTokenTotals;
  /** 单消息成本（omp 未按消息上报时为 0）。 */
  cost: number;
}

/** Wire message time span. */
/** wire 消息时间区间。 */
export interface WireMessageTime {
  /** 创建时间（ms）。 */
  created: number;
  /** 完成时间；未定稿时省略。 */
  completed?: number;
}

/** Wire part time span. */
/** wire part 时间区间。 */
export interface WirePartTime {
  /** 开始时间（ms）。 */
  start: number;
  /** 结束时间；进行中省略。 */
  end?: number;
}

/** Wire message metadata — the `ompRole`-keyed variant payloads the UI branches on. */
/** wire 消息元数据 —— UI 按 `ompRole` 键分派的变体载荷。 */
export interface WireMessageMetadata {
  /** omp 侧角色标记（'developer' | 'bash' | 'python' | 'file-mention' | 'modelChange' 等）。 */
  ompRole?: string;
  /** Count of tool calls stripped by branch/fork history rewrite (SDK
   * StrippedToolCallsMarker); the UI renders an elided-activity line. */
  /** 分支/分叉重写剥离的工具调用数；UI 据此渲染省略活动行。 */
  ompStrippedToolCalls?: number;
  /** `!`/`$` 执行卡的命令。 */
  command?: string;
  /** `!`/`$` 执行的退出码。 */
  exitCode?: number;
  /** `!`/`$` 执行的取消标记。 */
  cancelled?: boolean;
  /** `!!`/`$$` dispatch marker: the execution record is excluded from model context. */
  /** `!!`/`$$` 派发标记：执行记录不进入模型上下文。 */
  excludeFromContext?: boolean;
  /** 压缩前的 token 数（compactionSummary）。 */
  tokensBefore?: number;
  /** 压缩警告（compactionSummary）。 */
  warning?: string;
  /** 分支来源消息 id（branchSummary）。 */
  fromId?: string;
  /** fileMention 的文件列表投影。 */
  files?: Array<{ path?: string; lines?: number }>;
  /** Divider attribution (05 §5.5): the model selector a model_change divider
   * echoes; the mode value a mode_change divider carries. */
  /** 分隔行署名（05 §5.5）：model_change 回显的模型选择器。 */
  model?: string;
  /** mode_change 分隔行携带的模式值。 */
  mode?: string;
  /** model_change 的角色标签（署名归因）。 */
  role?: string;
  /** 解析模型命中回退（fallback）标记。 */
  fallback?: boolean;
}

/** Wire tool-part state (running/completed/error snapshot of one tool call). */
/** wire 工具 part 元数据（单次工具调用快照的附件字段）。 */
export interface WireToolMetadata {
  /** 模型陈述的调用意图（工具行标题）。 */
  intent?: string;
  /** 结构化工具详情（如 AskToolDetails）。 */
  details?: unknown;
  /** 异步任务状态（'running' | 'completed' | 'failed' 等）。 */
  asyncState?: string;
}

/** Wire tool-part state. */
/** wire 工具 part 状态。 */
export interface WireToolState {
  /** 运行状态：'running' | 'completed' | 'error'。 */
  status: 'running' | 'completed' | 'error';
  /** 解析后的调用参数映射。 */
  input: ToolCallArguments;
  /** 文本输出。 */
  output?: string;
  /** 出错时的错误文本。 */
  error?: string;
  /** 行标题（intent 优先，回退工具名）。 */
  title?: string;
  /** 元数据（intent / details / asyncState）。 */
  metadata?: WireToolMetadata;
  /** 起止时间。 */
  time: WirePartTime;
}

/** One wire message part (flat view: step-start / text / reasoning / tool / file). */
/** 单个 wire 消息 part（扁平视图：step-start / text / reasoning / tool / file）。 */
export interface WireMessagePart {
  /** part id（prt_ 前缀 + 消息 id 片段 + 序号）。 */
  id: string;
  /** 所属会话 id。 */
  sessionID: string;
  /** 所属 wire 消息 id。 */
  messageID: string;
  /** part 类型（'step-start' | 'text' | 'reasoning' | 'tool' | 'file'）。 */
  type: string;
  /** 文本/推理内容。 */
  text?: string;
  /** 起止时间。 */
  time?: WirePartTime;
  /** 合成 part 标记（不进模型上下文的展示行）。 */
  synthetic?: boolean;
  /** file part 的 MIME 类型。 */
  mime?: string;
  /** file part 的 data URL（base64 图片）。 */
  url?: string;
  /** tool part 的调用 id。 */
  callID?: string;
  /** 工具名。 */
  tool?: string;
  /** tool part 的状态快照。 */
  state?: WireToolState;
  /** `!` local-execution card payload (OpenCode user-shell row shape): the
   * UI's shell card reads these fields on a `type: 'text'` part and never
   * renders `text` itself. `status` runs 'running' while output streams and
   * settles to 'completed' | 'error' | 'cancelled'. */
  /** `!` 本地执行卡载荷（OpenCode 用户 shell 行形态）：`text` part 携带时，UI 的 shell 卡读取这些字段而不渲染 text 本身。 */
  shellAction?: {
    command?: string;
    output?: string;
    status?: string;
  };
}

/** Wire message info (flat view: user and assistant variants share the core fields). */
/** wire 消息信息（扁平视图：user 与 assistant 变体共享核心字段）。 */
export interface WireMessageInfo {
  /** wire 消息 id（msg_ 前缀）。 */
  id: string;
  /** 所属会话 id。 */
  sessionID: string;
  /** 角色：'user' | 'assistant'。 */
  role: 'user' | 'assistant';
  /** 创建/完成时间。 */
  time: WireMessageTime;
  /** agent 标识（默认 'build'）。 */
  agent: string;
  /** 模型选择器快照。 */
  model?: WireModelSelector;
  /** 父消息 id（assistant 锚定其用户提示）。 */
  parentID?: string;
  /** 变体载荷（ompRole 键）。 */
  metadata?: WireMessageMetadata;
  /** 扁平模型 id 字段。 */
  modelID?: string;
  /** 扁平提供商 id 字段。 */
  providerID?: string;
  /** 模式（与 agent 同值）。 */
  mode?: string;
  /** 工作目录（cwd/root）。 */
  path?: { cwd: string; root: string };
  /** 单消息成本。 */
  cost?: number;
  /** token 汇总。 */
  tokens?: WireTokenTotals;
  /** Terminal stop reason ('stop' | 'length' | 'toolUse' | 'error' | 'aborted');
   * present only on settled assistant messages — its presence is the wire
   * "step closed" signal (ChatMessage open-step check). */
  /** 终止原因；仅定稿的 assistant 消息携带 —— 其存在即 wire 的"回合已关闭"信号。 */
  finish?: string;
  /** Compaction/branch divider marker: OpenCode's turn-summary picker skips
   * summary messages when choosing a turn's answer text. */
  /** 压缩/分支分隔标记：OpenCode 的回合摘要选择器跳过 summary 消息。 */
  summary?: boolean;
  /** 回合错误（name + message）。 */
  error?: { name: string; data: { message: string } };
}

/** One projected message: wire info plus its ordered parts. */
/** 单条投影结果：wire 信息 + 有序 parts。 */
export interface ProjectedMessage {
  /** 消息信息。 */
  info: WireMessageInfo;
  /** 有序 part 列表。 */
  parts: WireMessagePart[];
}

/** Tool result as the assistant projector's pairing map stores it. */
/** 助手投影器配对表存储形态的工具结果。 */
export interface ProjectedToolResult {
  /** 结果内容块。 */
  content?: readonly ProjectedContentBlock[];
  /** 结构化详情。 */
  details?: unknown;
  /** 报错标记。 */
  isError?: boolean;
  /** 结果时间戳。 */
  timestamp?: number;
}

/** `normalizeToolExecutionResult` result. */
/** normalizeToolExecutionResult 的返回结构。 */
export interface NormalizedToolResult {
  /** 内容块（字符串输入会包装为 text 块）。 */
  content: readonly ProjectedContentBlock[];
  /** 内容块拼接出的纯文本。 */
  text: string;
  /** 存在且非空时的结构化详情。 */
  details?: unknown;
}

/** Tool execution end result: SDK AgentToolResult `{content, details}` or a plain string. */
/** tool_execution_end 的 result：SDK AgentToolResult 的 `{content, details}` 或纯字符串。 */
export type ToolExecutionResultInput = { content?: readonly ProjectedContentBlock[]; details?: unknown } | string | null | undefined;

/** Transcript entry as wire-id resolution walks it (only `type: "message"` entries carry a message). */
/** wire id 反查遍历的转写 entry 形态（仅 `type: "message"` entry 携带消息）。 */
export interface TranscriptEntryInput {
  /** entry 类型（'message' | 'model_change' 等）。 */
  type: string;
  /** entry 持久化 id（返回值）。 */
  id?: string;
  /** 携带的 AgentMessage（message entry）。 */
  message?: MessageInput;
}

/** `resolveWireIdToEntryId` options. */
/** resolveWireIdToEntryId 的选项。 */
export interface WireIdResolveOptions {
  /** 预签发 id 解析器（覆盖确定性推导）。 */
  wireIdFor?: WireIdResolver;
}

/** Message-history paging options (OpenCode `limit`/`before` contract). */
/** 消息历史分页选项（OpenCode 的 `limit`/`before` 契约）。 */
export interface PaginationOptions {
  /** 页大小上限（取最新尾部）。 */
  limit?: number;
  /** 排他边界消息 id（取其之前）。 */
  before?: string;
}

/** `paginateProjectedMessages` result. */
/** paginateProjectedMessages 的返回结构。 */
export interface ProjectedMessagePage {
  /** 当前页消息（升序）。 */
  messages: readonly ProjectedMessage[];
  /** 还有更早消息时的下一页游标（本页最旧 id）。 */
  cursor?: string;
}

/** Options shared by the synthetic user-side projectors (execution, file-mention). */
/** 合成用户侧投影器（执行、文件提及）共享的选项。 */
export interface BasicProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** agent 标识。 */
  agent?: string;
}

/** Options for the custom/hook message projector. */
/** custom/hook 消息投影器的选项。 */
export interface CustomProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** agent 标识。 */
  agent?: string;
  /** 挂靠的父（用户）消息 id。 */
  parentID?: string;
}

/** Options for the divider (compactionSummary / branchSummary) projector. */
/** 分隔行（compactionSummary / branchSummary）投影器的选项。 */
export interface DividerProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** agent 标识。 */
  agent?: string;
  /** 挂靠的父（用户）消息 id。 */
  parentID?: string;
}

/** Options for the user-message projector. */
/** 用户消息投影器的选项。 */
export interface UserProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** agent 标识。 */
  agent?: string;
  /** 发送时模型（选择器字符串或对象）。 */
  model?: ModelSelectorInput;
  /** Send-time thinking level; rides wire `model.variant` when present. */
  /** 发送时思考档位；存在时随 wire model.variant 下发。 */
  thinkingLevel?: string;
  /** 预签发 wire id（直播/冷启动桥）。 */
  wireId?: string;
}

/** Options for the assistant-message projector. */
/** 助手消息投影器的选项。 */
export interface AssistantProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** agent 标识。 */
  agent?: string;
  /** 工作目录（wire path.cwd/root）。 */
  directory?: string;
  /** 父消息 id。 */
  parentID?: string;
  /** 预签发 wire id。 */
  wireId?: string;
}

/** Options for the full-conversation projector. */
/** 全会话投影器的选项。 */
export interface ConversationProjectionOptions {
  /** 会话 id。 */
  sessionID: string;
  /** 工作目录。 */
  directory?: string;
  /** agent 标识。 */
  agent?: string;
  /** 全局默认模型（无回合快照时使用）。 */
  model?: ModelSelectorInput;
  /** 逐消息的 id 解析器。 */
  wireIdFor?: WireIdResolver;
  /** Per-user-message send-time model/thinking snapshot (engine-built). */
  /** 引擎构建的逐用户消息发送时模型/思考快照。 */
  turnStateFor?: (message: WireIdMessageInput) => { model?: string; thinkingLevel?: string } | null | undefined;
  /** 预签发 wire id。 */
  wireId?: string;
}

/** `message.updated` bus payload emitted by the streaming projector. */
/** 流式投影器发出的 message.updated 总线载荷。 */
export interface WireMessageUpdatedProperties {
  /** 会话 id。 */
  sessionID: string;
  /** 更新后的消息信息全量。 */
  info: WireMessageInfo;
}

/** `message.part.updated` bus payload emitted by the streaming projector. */
/** 流式投影器发出的 message.part.updated 总线载荷。 */
export interface WirePartUpdatedProperties {
  /** 会话 id。 */
  sessionID: string;
  /** 更新后的 part 全量。 */
  part: WireMessagePart;
  /** 事件时间（ms）。 */
  time?: number;
}

/** `message.part.delta` bus payload emitted by the streaming projector. */
/** 流式投影器发出的 message.part.delta 总线载荷。 */
export interface WirePartDeltaProperties {
  /** 会话 id。 */
  sessionID: string;
  /** 所属消息 id。 */
  messageID: string;
  /** 目标 part id。 */
  partID: string;
  /** 增量字段（当前仅 'text'）。 */
  field: 'text';
  /** 追加的文本增量。 */
  delta: string;
}

/** Sink the streaming projector emits wire events through (the host bus). */
/** 流式投影器发出 wire 事件的汇（host 总线）。 */
export type StreamProjectorEmit = (
  type: 'message.updated' | 'message.part.updated' | 'message.part.delta',
  properties: WireMessageUpdatedProperties | WirePartUpdatedProperties | WirePartDeltaProperties,
  directory?: string,
) => void;

/** StreamProjector constructor options. */
/** StreamProjector 构造选项。 */
export interface StreamProjectorOptions {
  /** 会话 id。 */
  sessionID: string;
  /** 工作目录（事件作用域 + wire path）。 */
  directory?: string;
  /** agent 标识。 */
  agent?: string;
  /** 事件汇回调。 */
  emit: StreamProjectorEmit;
  /**
   * Cross-generation store of finalized tool parts (callID → coordinates).
   * The engine creates one projector per assistant turn; async-job task
   * updates outlive the owning turn and must revive parts recorded by a
   * previous generation, so the map lives on the host session and every
   * projector shares it.
   */
  /** 跨代共享的已定稿工具 part 表（callID → 坐标）：引擎每个助手回合新建一个投影器，而异步任务更新可能晚于所属回合存活、需复活先前代记录的 part，故该表挂在 host 会话上由所有投影器共享。 */
  sharedFinalToolParts?: Map<string, { id: string; messageID: string }>;
}

// ---------------------------------------------------------------------------
// Wire-id derivation
// ---------------------------------------------------------------------------

/** base36 编码字母表（0-9 + a-z）。 */
const BASE36 = '0123456789abcdefghijklmnopqrstuvwxyz';

/** 把非负整数编码为 base36 字符串，左侧补零至 pad 位（默认 8）。 */
const toBase36 = (value: number, pad = 8) => {
  let out = '';
  let n = value;
  do {
    out = BASE36[n % 36] + out;
    n = Math.floor(n / 36);
  } while (n > 0);
  return out.padStart(pad, '0');
};

/** 取文本 SHA-256 摘要的前 4 个 hex 字符，作为 wire id 的内容区分段。 */
const contentDigest = (text?: string | null) => {
  const hash = crypto.createHash('sha256').update(String(text ?? '')).digest('hex');
  return hash.slice(0, 4);
};

/** 按 provider/model 形态的选择器字符串拆分为 { providerID, modelID }；无分隔符时 providerID 为空串。 */
export const splitModelSelector = (modelId?: string | null): WireModelSelector => {
  const separator = String(modelId ?? '').indexOf('/');
  if (separator === -1) return { providerID: '', modelID: String(modelId ?? '') };
  return {
    providerID: String(modelId).slice(0, separator),
    modelID: String(modelId).slice(separator + 1)
  };
};

/** 由角色字符（a/c/u）+ base36 时间戳 + 4 位内容摘要拼出确定性的 `msg_` wire id。 */
export const wireMessageId = (role: string | null | undefined, timestamp: number, seedText?: string | null) => {
  const roleChar = role === 'assistant' ? 'a' : role === 'custom' ? 'c' : 'u';
  return `msg_${roleChar}${toBase36(timestamp)}${contentDigest(seedText)}`;
};

/**
 * Deterministic wire id of one engine message — the exact derivation the
 * user/assistant projectors use. Assistant ids are content-independent
 * (docs/plan.md phase 5 stable formula): the SDK mints the message
 * timestamp once at creation and never mutates it, so seeding from the
 * empty string makes the streaming projector's message_start id and the
 * cold projection of the persisted message identical by construction — no
 * bridging map is needed for assistant messages. User ids keep the content
 * digest: user messages arrive complete and queued prompts can share a
 * timestamp, so content is the discriminator there.
 */
/**
 * 单条引擎消息的确定性 wire id —— 与 user/assistant 投影器使用的推导
 * 完全一致。assistant id 与内容无关（docs/plan.md phase 5 稳定公式）：
 * SDK 在创建时一次性铸造消息时间戳且不再改动，以空串作种子使流式投影器
 * message_start 的 id 与持久化消息的冷投影按构造即相同 —— assistant
 * 消息因此无需桥接表。user id 保留内容摘要：用户消息完整到达且排队的
 * 提示可能共享时间戳，内容才是区分依据。
 */
export const deterministicWireId = (message: WireIdMessageInput) => {
  const seed = message.role === 'assistant' ? '' : textOfContent(message.content);
  return wireMessageId(message.role, message.timestamp, seed);
};

/**
 * Resolve a wire message id (what the UI reads from GET messages) back to
 * the session ENTRY id (what SessionManager.branch expects). Walks the
 * manager's entry list: each `type: "message"` entry wraps the AgentMessage
 * whose deterministic projection the UI saw. Returns null when no entry
 * projects to that id — callers decide whether to pass the raw id through
 * (compat: native entry ids already worked).
 */
/**
 * 把 wire 消息 id（UI 从 GET messages 读到的）反解回会话 entry id
 * （SessionManager.branch 期望的）。遍历管理器的 entry 列表：每个
 * `type: "message"` entry 包裹着 UI 所见确定性投影对应的 AgentMessage。
 * 没有 entry 投影到该 id 时返回 null —— 由调用方决定是否透传原始 id
 * （兼容：原生 entry id 一直可用）。
 */
export const resolveWireIdToEntryId = (
  entries: readonly TranscriptEntryInput[] | null | undefined,
  wireId: string | null | undefined,
  { wireIdFor }: WireIdResolveOptions = {},
): string | null => {
  if (!Array.isArray(entries) || typeof wireId !== 'string' || !wireId) return null;
  for (const entry of entries) {
    if (entry?.type !== 'message' || !entry.message) continue;
    const message = entry.message;
    if (message.role !== 'user' && message.role !== 'assistant') continue;
    const projected = wireIdFor?.(message) ?? deterministicWireId(message);
    if (projected === wireId) return entry.id ?? null;
  }
  return null;
};
/**
 * Page a chronologically ascending wire-message list by the OpenCode
 * message-history contract: `limit` caps the newest tail, `before` is an
 * exclusive message-id boundary, and the returned cursor is the oldest id of
 * the page when older messages remain. An unknown `before` id yields an empty
 * page so clients stop paging instead of looping over stale cursors.
 */
/**
 * 按时间升序的 wire 消息列表执行 OpenCode 消息历史分页契约：`limit`
 * 截取最新的尾部，`before` 是排他的消息 id 边界，还有更早消息时返回的
 * cursor 是本页最旧的 id。未知的 `before` id 产生空页，让客户端停止
 * 翻页而不是在过期 cursor 上循环。
 */
export const paginateProjectedMessages = (
  messages: readonly ProjectedMessage[],
  { limit, before }: PaginationOptions = {},
): ProjectedMessagePage => {
  let windowed = messages;
  if (before) {
    const boundary = messages.findIndex((message) => message?.info?.id === before);
    windowed = boundary === -1 ? [] : messages.slice(0, boundary);
  }
  const pageLimit = typeof limit === 'number' && Number.isFinite(limit) && limit > 0 ? Math.floor(limit) : undefined;
  if (pageLimit === undefined || windowed.length <= pageLimit) {
    return { messages: windowed, cursor: undefined };
  }
  const page = windowed.slice(-pageLimit);
  return { messages: page, cursor: page[0]?.info?.id };
};

/**
 * 把 omp usage 上报投影为 wire `{ tokens, cost }`。缺省字段一律落 0；
 * totalTokens 仅在 SDK 上报时透传为 tokens.total；omp 经由 usage 报告
 * 而非按消息上报成本，单消息 cost 为 0，会话聚合由 usage 报告提供。
 */
export const projectUsage = (usage?: UsageInput | null): WireUsageProjection => {
  const u: UsageInput = usage ?? {};
  return {
    tokens: {
      input: u.input ?? 0,
      output: u.output ?? 0,
      reasoning: u.reasoningTokens ?? 0,
      cache: {
        read: u.cacheRead ?? 0,
        write: u.cacheWrite ?? 0,
      },
      ...(u.totalTokens !== undefined ? { total: u.totalTokens } : {}),
    },
    // omp reports cost through usage reports rather than per-message totals;
    // per-message cost is surfaced as zero and session aggregates come from
    // usage reports when available.
    cost: typeof u.cost === 'number' ? u.cost : 0
  };
};

/** 提取内容的纯文本：字符串直接返回，块列表拼接全部 text 块，其余为空串。 */
const textOfContent = (content?: ProjectedContentInput | null) => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((block) => block && block.type === 'text')
    .map((block) => block.text)
    .join('');
};

/** True when a tool result carries structured, non-empty details. */
/** 工具结果携带非空结构化 details 时为 true。 */
const hasDetails = (result: { details?: unknown }) =>
  result.details !== null &&
  result.details !== undefined &&
  typeof result.details === 'object' &&
  Object.keys(result.details).length > 0;

/**
 * Normalize a tool_execution_end `result` into the transcript
 * ToolResultMessage shape. The SDK passes an AgentToolResult
 * `{content, details}` object; plain strings also occur (older emitters,
 * tests). Returns the content blocks, their concatenated text, and the
 * structured details when present.
 */
/**
 * 把 tool_execution_end 的 `result` 归一为转写 ToolResultMessage 形态。
 * SDK 传 AgentToolResult 的 `{content, details}` 对象；纯字符串也会出现
 * （旧发射方、测试）。返回内容块、其拼接文本，以及存在时的结构化详情。
 */
export const normalizeToolExecutionResult = (result?: ToolExecutionResultInput): NormalizedToolResult => {
  if (result !== null && typeof result === 'object') {
    const content = Array.isArray(result.content) ? result.content : [];
    return {
      content,
      text: textOfContent(content),
      ...(hasDetails(result) ? { details: result.details } : {})
    };
  }
  const text = typeof result === 'string' ? result : '';
  return { content: text ? [{ type: 'text', text }] : [], text };
};

/** 过滤出内容中的 image 块（非数组输入返回空数组）。 */
const imageBlocks = (content?: ProjectedContentInput | null): ProjectedContentBlock[] =>
  Array.isArray(content) ? content.filter((block) => block && block.type === 'image') : [];

/** 生成 part id：`prt_` + 消息 id 去 `msg_` 前缀 + `_` + 创建序号。 */
const partId = (messageWireId: string, seq: number) => `prt_${messageWireId.slice(4)}_${seq}`;

/**
 * Coerce one tool-arguments value into the wire input map: object maps pass
 * through, stringified JSON is parsed, and anything else degrades to
 * `{ input: value }` (or `{}`) so tool state always carries an object.
 */
/**
 * 把单个工具参数值强制归一为 wire 输入映射：对象映射直接透传，字符串化
 * JSON 尝试解析，其余退化为 `{ input: value }`（或 `{}`），保证 tool
 * state 始终携带对象。
 */
const safeJson = (value: ToolCallArguments | string | undefined): ToolCallArguments => {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    return value;
  }
  if (typeof value === 'string') {
    try {
      return JSON.parse(value);
    } catch {
      return { input: value };
    }
  }
  return {};
};

/**
 * Project one omp UserMessage into wire `{ info, parts }`.
 * `model` is the send-time model (SDK user messages carry none — the engine
 * passes the live session model for fresh sends, the turn-state stamper for
 * replays); `thinkingLevel`, when known, rides `model.variant` as the wire
 * contract's effort slot so each message carries its exact turn snapshot.
 */
/**
 * 把单条 omp UserMessage 投影为 wire `{ info, parts }`。
 * `model` 是发送时的模型（SDK 用户消息不携带 —— 引擎对新发送传入会话
 * 实时模型，对重放传入回合状态戳）；已知 `thinkingLevel` 时随
 * `model.variant` 下发（wire 契约的 effort 槽位），使每条消息携带其精确
 * 的回合快照。
 */
export const projectUserMessage = (
  message: UserMessageInput,
  { sessionID, agent, model, thinkingLevel, wireId }: UserProjectionOptions,
): ProjectedMessage => {
  const id = wireId ?? wireMessageId('user', message.timestamp, textOfContent(message.content));
  const text = textOfContent(message.content);
  const parts: WireMessagePart[] = [];
  let seq = 0;
  if (text.length > 0) {
    parts.push({
      id: partId(id, seq++),
      sessionID,
      messageID: id,
      type: 'text',
      text,
      ...(message.synthetic ? { synthetic: true } : {}),
    });
  }
  for (const image of imageBlocks(message.content)) {
    parts.push({
      id: partId(id, seq++),
      sessionID,
      messageID: id,
      type: 'file',
      mime: image.mimeType || 'image/png',
      url: `data:${image.mimeType || 'image/png'};base64,${image.data}`
    });
  }
  const selector = model
    ? typeof model === 'string'
      ? splitModelSelector(model)
      : { providerID: model.provider ?? '', modelID: model.id ?? '' }
    : { providerID: '', modelID: '' };
  const info: WireMessageInfo = {
    id,
    sessionID,
    role: 'user',
    time: { created: message.timestamp },
    agent: agent ?? 'build',
    model: {
      providerID: selector.providerID,
      modelID: selector.modelID,
      ...(typeof thinkingLevel === 'string' && thinkingLevel.length > 0 ? { variant: thinkingLevel } : {})
    }
  };
  return { info, parts };
};


/** Transcript entry subset the turn-state stamper reads (SDK SessionEntry). */
/** 回合状态戳器读取的转写 entry 子集（SDK SessionEntry）。 */
type SessionEntryLike = {
  /** entry 类型。 */
  type: string;
  /** model_change 携带的模型。 */
  model?: unknown;
  /** thinking_level_change 携带的档位。 */
  thinkingLevel?: unknown;
  /** message entry 携带的消息。 */
  message?: WireIdMessageInput | null;
};

/**
 * Fold the transcript's `model_change` / `thinking_level_change` entries
 * into a per-user-message turn-state resolver: every user entry is stamped
 * with the model and thinking level in effect at its point in the log —
 * the exact snapshot the turn ran with. `wireIdFor` mirrors the projector's
 * id derivation so overridden ids join; entries before the first change
 * fall back to the caller's seed (passed separately by the engine).
 */
/**
 * 把转写中的 `model_change` / `thinking_level_change` entry 折叠为逐
 * 用户消息的回合状态解析器：每个用户 entry 都被盖上其在日志位置处生效
 * 的模型与思考档位 —— 即该回合实际运行的快照。`wireIdFor` 镜像投影器
 * 的 id 推导，使被覆盖的 id 也能入表；首个变更之前的 entry 回退到调用方
 * 的种子值（由引擎另行传入）。
 */
export const buildTurnStateStamper = (
  entries: readonly SessionEntryLike[] | null | undefined,
  { wireIdFor }: { wireIdFor?: WireIdResolver } = {},
) => {
  const stateByWireId = new Map();
  let model = null;
  let thinkingLevel = null;
  for (const entry of entries ?? []) {
    if (!entry || typeof entry !== 'object') continue;
    if (entry.type === 'model_change' && typeof entry.model === 'string' && entry.model.length > 0) {
      model = entry.model;
    } else if (entry.type === 'thinking_level_change') {
      thinkingLevel = typeof entry.thinkingLevel === 'string' && entry.thinkingLevel.length > 0 ? entry.thinkingLevel : null;
    } else if (entry.type === 'message' && entry.message?.role === 'user') {
      const key = wireIdFor?.(entry.message) ?? deterministicWireId(entry.message);
      stateByWireId.set(key, { model, thinkingLevel });
    }
  }
  return (message: WireIdMessageInput | null | undefined) => (message?.role === 'user' ? (stateByWireId.get(wireIdFor?.(message) ?? deterministicWireId(message)) ?? null) : null);
};

/**
 * Project one omp transcript `custom_message` (advisor nudges, todo reminders,
 * late LSP diagnostics, ...) into a labeled assistant-side wire message so the
 * note stays visible in history without fragmenting the user's turns. The
 * `[omp:<type>]` prefix marks the text as harness-injected rather than model
 * output. Entries the engine marked `display: false`, and empty ones, are
 * dropped.
 */
/** 剥掉恰好被单一 XML 标签整段包裹的外壳（如 system-reminder 包裹），返回内部正文；不匹配时原样返回。 */
const unwrapFullBodyXml = (text: string): string => {
  const wrapped = text.match(/^<([a-zA-Z-]+)[^>]*>\s*([\s\S]*?)\s*<\/\1>\s*$/);
  return wrapped ? wrapped[2] : text;
};

/**
 * 把单条 omp 转写 `custom_message`（advisor 提示、todo 提醒、迟到的 LSP
 * 诊断等）投影为带标签的助手侧 wire 消息，使附注在历史中可见且不打碎
 * 用户的回合。`[omp:<type>]` 前缀标记该文本为 harness 注入而非模型输出。
 * 引擎标记 `display: false` 的 entry 与空 entry 会被丢弃。
 */
export const projectCustomMessage = (
  message: CustomMessage | HookMessage,
  { sessionID, agent, parentID }: CustomProjectionOptions,
): ProjectedMessage => {
  const text = textOfContent(message.content);
  const label = message.customType ? `[omp:${message.customType}] ` : '[omp] ';
  const body = unwrapFullBodyXml(text);
  const id = wireMessageId('custom', message.timestamp, label + body);
  return {
    info: {
      id,
      sessionID,
      role: 'assistant',
      // The turn model only renders assistant messages whose parentID resolves
      // to a user message, so notes ride the turn they were injected into.
      ...(parentID ? { parentID } : {}),
      time: { created: message.timestamp, completed: message.timestamp },
      agent: agent ?? 'build',
      model: { providerID: '', modelID: '' }
    },
    parts: [
      {
        id: partId(id, 0),
        sessionID,
        messageID: id,
        type: 'text',
        text: label + body,
        synthetic: true,
        time: { start: message.timestamp }
      }
    ]
  };
};

/** 构造 developer 消息的合成 text part（标签前缀 + 正文，带 synthetic 标记）。 */
const developerTextPart = (
  id: string,
  sessionID: string,
  label: string,
  text: string,
  timestamp: number,
): WireMessagePart => ({
  id: partId(id, 0),
  sessionID,
  messageID: id,
  type: 'text',
  text: label + text,
  synthetic: true,
  time: { start: timestamp }
});

/**
 * Project a `developer` role message (harness-injected transcript input) into
 * the wire model. The TUI renders both attributions as collapsed synthetic
 * rows at the user position (chat-transcript-builder.ts `#appendChatMessage`,
 * user/developer case); the wire shape keeps the turn structure intact:
 * - attribution 'user' (synthetic prompts, todo-command reminders,
 *   image-bearing custom-message conversions): user-side synthetic message
 *   occupying the user turn slot, so the following assistant message anchors
 *   to it exactly like a real prompt.
 * - attribution 'agent' (mid-turn injections: turn-recovery reminders,
 *   auto-continue, side-channel nudges): assistant-side `[omp:developer]`
 *   note riding the current turn (projectCustomMessage's shape), so it never
 *   splits the turn it was injected into.
 */
/**
 * 把 `developer` 角色消息（harness 注入的转写输入）投影为 wire 模型。
 * TUI 把两种 attribution 都渲染为用户位置的折叠合成行
 * （chat-transcript-builder.ts 的 #appendChatMessage，user/developer 分支）；
 * wire 形态保持回合结构完整：
 * - attribution 'user'（合成提示、todo 命令提醒、带图的 custom-message
 *   转换）：占据用户回合槽位的用户侧合成消息，后续 assistant 消息像
 *   真实提示一样精确锚定到它。
 * - attribution 'agent'（回合中段注入：回合恢复提醒、自动续跑、侧信道
 *   nudge）：挂靠当前回合的助手侧 `[omp:developer]` 附注
 *   （projectCustomMessage 的形态），绝不拆分被注入的回合。
 */
export const projectDeveloperMessage = (
  message: DeveloperMessageInput,
  { sessionID, agent, parentID }: { sessionID: string; agent?: string; parentID?: string },
): ProjectedMessage => {
  const text = textOfContent(message.content);
  const label = '[omp:developer] ';
  const userSlot = message.attribution === 'user';
  // Reminder injections wrap their body in <system-reminder>...</system-reminder>;
  // the UI's synthetic-part filter drops parts containing that tag, so the part
  // text carries the unwrapped body. The id seed stays on the raw text so
  // already-persisted notes keep their wire ids across this unwrap.
  const body = unwrapFullBodyXml(text);
  const id = wireMessageId(userSlot ? 'user' : 'custom', message.timestamp, label + text);
  if (userSlot) {
    return {
      info: {
        id,
        sessionID,
        role: 'user',
        time: { created: message.timestamp },
        agent: agent ?? 'build',
        model: { providerID: '', modelID: '' },
        metadata: { ompRole: 'developer' }
      },
      parts: [developerTextPart(id, sessionID, label, body, message.timestamp)]
    };
  }
  return {
    info: {
      id,
      sessionID,
      role: 'assistant',
      ...(parentID ? { parentID } : {}),
      time: { created: message.timestamp, completed: message.timestamp },
      agent: agent ?? 'build',
      model: { providerID: '', modelID: '' },
      metadata: { ompRole: 'developer' }
    },
    parts: [developerTextPart(id, sessionID, label, body, message.timestamp)]
  };
};

/**
 * Project a transcript turn-state entry (model_change / mode_change) into
 * the same slim-divider wire shape as compaction summaries, so the timeline
 * shows where the session's model or mode changed. Standalone (no parentID):
 * turn pairing never anchors on it. Entries without a role tag are the
 * session's init/restore bookkeeping, not user-visible switches — skipped.
 * Timestamps arrive as ISO strings (transcript persistence) or numeric ms.
 */
/** Minimal divider-row contract: the fields the projectors read off a
 * turn-event entry (SDK SessionEntry satisfies it structurally). */
/** 分隔行最小契约：投影器从 turn-event entry 读取的字段（SDK SessionEntry 结构性满足）。 */
export interface DividerEntryInput {
  /** entry 类型（'model_change' | 'mode_change'）。 */
  type: string;
  /** 时间戳（ISO 字符串或数值毫秒）。 */
  timestamp?: string | number;
  /** 变更后的模型选择器。 */
  model?: string;
  /** 触发者角色标签（署名归因，非档位）。 */
  role?: string;
  /** 解析模型命中回退（fallback）标记。 */
  resolvedModelIsFallback?: boolean;
  /** 思考档位值。 */
  thinkingLevel?: string;
  /** 模式值。 */
  mode?: string;
}

/**
 * 把转写回合状态 entry（model_change / mode_change）投影为与压缩摘要
 * 相同的纤细分隔行 wire 形态，使时间轴显示会话在何处切换了模型或模式。
 * 独立成段（无 parentID）：回合配对绝不锚定于它。无角色标签的 entry 是
 * 会话的 init/restore 簿记而非用户可见的切换 —— 跳过。时间戳可为 ISO
 * 字符串（转写持久化）或数值毫秒。
 */
export const projectTurnEventDivider = (
  entry: SessionEntry | DividerEntryInput,
  { sessionID }: { sessionID: string },
): ProjectedMessage | null => {
  if (!entry || typeof entry !== 'object') return null;
  // Transcript entries persist ISO string timestamps (getEntries Date.parses
  // them); tolerate numeric ms too. Wire time.created is numeric ms.
  const rawTimestamp: unknown = entry.timestamp;
  const timestamp = typeof rawTimestamp === 'number' ? rawTimestamp : typeof rawTimestamp === 'string' && rawTimestamp.length > 0 ? Date.parse(rawTimestamp) : Number.NaN;
  if (!Number.isFinite(timestamp)) return null;
  if (entry.type === 'model_change') {
    if (typeof entry.model !== 'string' || entry.model.length === 0) return null;
    if (typeof entry.role !== 'string' || entry.role.length === 0) return null;
    // Body stays the model selector alone — the role tag is attribution, not
    // a variant/level; rendering it inline read like a thinking level next to
    // the message snapshots. It rides metadata for the expanded detail.
    const body = entry.model;
    const id = wireMessageId('custom', timestamp, `[omp:modelChange] ${body}`);
    return {
      info: {
        id,
        sessionID,
        role: 'assistant',
        time: { created: timestamp, completed: timestamp },
        agent: 'build',
        model: { providerID: '', modelID: '' },
        metadata: {
          ompRole: 'modelChange',
          model: entry.model,
          ...(typeof entry.role === 'string' ? { role: entry.role } : {}),
          ...(entry.resolvedModelIsFallback === true ? { fallback: true } : {})
        }
      },
      parts: [
        {
          id: partId(id, 0),
          sessionID,
          messageID: id,
          type: 'text',
          text: `[omp:modelChange] ${body}`,
          synthetic: true,
          time: { start: timestamp }
        }
      ]
    };
  }
  if (entry.type === 'mode_change') {
    if (typeof entry.mode !== 'string' || entry.mode.length === 0) return null;
    const id = wireMessageId('custom', timestamp, `[omp:modeChange] ${entry.mode}`);
    return {
      info: {
        id,
        sessionID,
        role: 'assistant',
        time: { created: timestamp, completed: timestamp },
        agent: 'build',
        model: { providerID: '', modelID: '' },
        metadata: { ompRole: 'modeChange', mode: entry.mode }
      },
      parts: [
        {
          id: partId(id, 0),
          sessionID,
          messageID: id,
          type: 'text',
          text: `[omp:modeChange] ${entry.mode}`,
          synthetic: true,
          time: { start: timestamp }
        }
      ]
    };
  }
  return null;
};
/**
 * Project a transcript divider role (compactionSummary / branchSummary) into
 * a synthetic assistant-side wire message (spec 05 §5.5, GAP-E04 P1).
 * Rendered as a collapsible slim divider; the `[omp:<role>]` prefix is the
 * UI's tier-classification contract (05 §5.8.1).
 */
/**
 * 把转写分隔角色（compactionSummary / branchSummary）投影为合成助手侧
 * wire 消息（spec 05 §5.5，GAP-E04 P1）。渲染为可折叠的纤细分隔行；
 * `[omp:<role>]` 前缀是 UI 的层级分类契约（05 §5.8.1）。
 */
export const projectDividerMessage = (
  message: CompactionSummaryMessage | BranchSummaryMessage,
  { sessionID, agent, parentID }: DividerProjectionOptions,
): ProjectedMessage => {
  const summary = String(message.summary ?? '');
  const role = message.role === 'branchSummary' ? 'branchSummary' : 'compactionSummary';
  const label = `[omp:${role}] `;
  const id = wireMessageId('custom', message.timestamp, label + summary);
  return {
    info: {
      id,
      sessionID,
      role: 'assistant',
      ...(parentID ? { parentID } : {}),
      time: { created: message.timestamp, completed: message.timestamp },
      summary: true,
      agent: agent ?? 'build',
      model: { providerID: '', modelID: '' },
      metadata: {
        ompRole: role,
        ...(message.role === 'compactionSummary'
          ? {
              tokensBefore: message.tokensBefore,
              ...(message.warning ? { warning: message.warning } : {})
            }
          : { fromId: message.fromId })
      }
    },
    parts: [
      {
        id: partId(id, 0),
        sessionID,
        messageID: id,
        type: 'text',
        text: label + summary,
        synthetic: true,
        time: { start: message.timestamp }
      }
    ]
  };
};

/** Options for the `!`/`$` execution-row projector. */
/** `!`/`$` 执行行投影器的选项。 */
export interface ExecutionProjectionOptions extends BasicProjectionOptions {
  /** Live-issued wire id. A `!` dispatch emits its running card before the
   * transcript record exists (the SDK mints the record timestamp at
   * completion), so the engine mints this id at dispatch and registers it on
   * `wireIdEchoes` at settle — the phase-5 echo bridge then makes every later
   * cold projection emit the same row id. */
  /** 直播签发的 wire id。`!` 派发在转写记录存在之前就发出运行中卡片（SDK 在完成时才铸造记录时间戳），因此引擎在派发时铸造该 id 并在结算时登记到 wireIdEchoes —— phase-5 回声桥使之后的每次冷投影发出相同的行 id。 */
  wireId?: string;
  /** Live-run status override: persisted records are always terminal, so a
   * running dispatch reports 'running' while output streams. */
  /** 直播运行状态覆盖：持久化记录总是终态，运行中的派发在输出流式期间上报 'running'。 */
  status?: string;
}

/** Canonical cold wire id for a `!`/`$` execution record — the formula the
 * live dispatcher's echo bridge and every cold projection must agree on. */
/** `!`/`$` 执行记录的规范冷 wire id —— 直播派发器的回声桥与所有冷投影必须达成一致的公式。 */
export const executionWireId = (message: ShellExecutionMessageInput): string => {
  const kind = message.role === 'pythonExecution' ? 'python' : 'bash';
  const command = kind === 'python' ? (message.code ?? '') : (message.command ?? '');
  const output = String(message.output ?? '');
  const cancelled = message.cancelled ? ' (cancelled)' : '';
  const exit = message.exitCode !== undefined ? ` [exit ${message.exitCode}]` : '';
  return wireMessageId('custom', message.timestamp, `[omp:${kind}] ` + command + output + exit + cancelled);
};

/**
 * Project a `!`/`$` shell-kernel execution role into a user-side synthetic
 * message (spec 05 §5.10, GAP-E14). Standalone segment (parentID='') so
 * turn pairing never anchors on it; the `[omp:bash]`/`[omp:python]` prefix
 * routes the UI to the execution-card renderer, and the part's `shellAction`
 * payload (command/output/status) feeds its existing shell card.
 */
/**
 * 把 `!`/`$` shell 内核执行角色投影为用户侧合成消息（spec 05 §5.10，
 * GAP-E14）。独立成段（parentID=''）使回合配对绝不锚定于它；
 * `[omp:bash]`/`[omp:python]` 前缀把 UI 路由到执行卡渲染器，part 的
 * `shellAction` 载荷（command/output/status）喂给现有的 shell 卡。
 */
export const projectExecutionMessage = (
  message: ShellExecutionMessageInput,
  { sessionID, agent, wireId, status }: ExecutionProjectionOptions,
): ProjectedMessage => {
  const kind = message.role === 'pythonExecution' ? 'python' : 'bash';
  const command = kind === 'python' ? (message.code ?? '') : (message.command ?? '');
  const label = `[omp:${kind}] `;
  const output = String(message.output ?? '');
  const cancelled = message.cancelled ? ' (cancelled)' : '';
  const exit = message.exitCode !== undefined ? ` [exit ${message.exitCode}]` : '';
  const id = wireId ?? executionWireId(message);
  const shellStatus = status
    ?? (message.cancelled
      ? 'cancelled'
      : message.exitCode !== undefined && message.exitCode !== 0
        ? 'error'
        : 'completed');
  return {
    info: {
      id,
      sessionID,
      role: 'user',
      time: { created: message.timestamp },
      agent: agent ?? 'build',
      model: { providerID: '', modelID: '' },
      metadata: {
        ompRole: kind,
        command,
        exitCode: message.exitCode,
        cancelled: Boolean(message.cancelled),
        excludeFromContext: Boolean(message.excludeFromContext)
      }
    },
    parts: [
      {
        id: partId(id, 0),
        sessionID,
        messageID: id,
        type: 'text',
        text: `${label}$ ${command}${exit}${cancelled}${output ? `\n${output}` : ''}`,
        synthetic: true,
        time: { start: message.timestamp },
        shellAction: { command, output, status: shellStatus }
      }
    ]
  };
};

/**
 * Project a fileMention role into a user-side synthetic message — one
 * `└ Read <path> (N lines)` line per file (TUI messages.ts:294-302).
 */
/** 把 fileMention 角色投影为用户侧合成消息 —— 每个文件一行 `└ Read <path> (N lines)`（TUI messages.ts:294-302）。 */
export const projectFileMentionMessage = (
  message: FileMentionMessageInput,
  { sessionID, agent }: BasicProjectionOptions,
): ProjectedMessage => {
  const files = Array.isArray(message.files) ? message.files : [];
  const lines = files.map((file) => `└ Read ${file.path ?? '(unknown)'}${file.lineCount !== undefined ? ` (${file.lineCount} lines)` : ''}`).join('\n');
  const label = '[omp:file-mention] ';
  const id = wireMessageId('custom', message.timestamp, label + lines);
  return {
    info: {
      id,
      sessionID,
      role: 'user',
      time: { created: message.timestamp },
      agent: agent ?? 'build',
      model: { providerID: '', modelID: '' },
      metadata: {
        ompRole: 'file-mention',
        files: files.map((file) => ({
          path: file.path,
          lines: file.lineCount
        }))
      }
    },
    parts: [
      {
        id: partId(id, 0),
        sessionID,
        messageID: id,
        type: 'text',
        text: label + lines,
        synthetic: true,
        time: { start: message.timestamp }
      }
    ]
  };
};

/**
 * Project one omp AssistantMessage (with its paired ToolResultMessages) into
 * wire `{ info, parts }`. Tool results are matched by toolCallId; unpaired
 * calls are rendered in their last observed state.
 */
/**
 * 把单条 omp AssistantMessage（连同其配对的 ToolResultMessages）投影为
 * wire `{ info, parts }`。工具结果按 toolCallId 配对；未配对的调用按其
 * 最后观测到的状态渲染。
 */
export const projectAssistantMessage = (
  message: AssistantMessageInput,
  toolResults: Map<string, ProjectedToolResult>,
  { sessionID, agent, directory, parentID, wireId }: AssistantProjectionOptions,
): ProjectedMessage => {
  // Stable formula (plan phase 5): content-independent, so this cold arm
  // matches the streaming projector's start id without a bridging map.
  const id = wireId ?? deterministicWireId(message);
  const selector = {
    providerID: message.provider ?? splitModelSelector(message.model ?? '').providerID,
    modelID: message.model ?? ''
  };
  const { tokens, cost } = projectUsage(message.usage);

  const parts: WireMessagePart[] = [];
  let seq = 0;
  // 局部入队助手：统一走一处以便日后在 push 点插桩。
  const pushPart = (part: WireMessagePart) => parts.push(part);

  pushPart({
    id: partId(id, seq++),
    sessionID,
    messageID: id,
    type: 'step-start'
  });

  for (const block of message.content ?? []) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'text') {
      pushPart({
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'text',
        text: block.text,
        time: { start: message.timestamp }
      });
    } else if (block.type === 'thinking') {
      pushPart({
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'reasoning',
        text: block.thinking ?? '',
        time: { start: message.timestamp, end: message.timestamp }
      });
    } else if (block.type === 'image' && typeof block.data === 'string') {
      // Assistant-attached images project as wire file parts in content
      // order (TUI assistant-message.ts:815-818 renders them interleaved);
      // the user-message projector uses the identical shape.
      const mime = block.mimeType || 'image/png';
      pushPart({
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'file',
        mime,
        url: `data:${mime};base64,${block.data}`,
        time: { start: message.timestamp },
      });
    } else if (block.type === 'toolCall') {
      const callBlockId = typeof block.id === 'string' ? block.id : undefined;
      const result = callBlockId !== undefined ? toolResults.get(callBlockId) : undefined;
      const input = safeJson(block.arguments);
      // The model states its reason for each call in `intent`; it is the
      // human-readable heading for the tool row (the raw name/command stays
      // available through state fallbacks).
      const intent = typeof block.intent === 'string' && block.intent.trim() ? block.intent.trim() : null;
      const base = {
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'tool',
        callID: block.id,
        tool: block.name
      };
      if (!result) {
        pushPart({
          ...base,
          state: {
            status: message.stopReason === 'aborted' ? 'error' : 'completed',
            input,
            ...(message.stopReason === 'aborted'
              ? {
                  error: 'Aborted',
                  time: { start: message.timestamp, end: message.timestamp }
                }
              : {
                  output: '',
                  title: intent ?? block.name,
                  metadata: intent ? { intent } : {},
                  time: { start: message.timestamp, end: message.timestamp }
                })
          }
        });
      } else if (result.isError) {
        pushPart({
          ...base,
          state: {
            status: 'error',
            input,
            error: textOfContent(result.content) || 'Tool error',
            time: {
              start: message.timestamp,
              end: result.timestamp ?? message.timestamp
            }
          }
        });
      } else {
        // Structured tool details (the ask tool's AskToolDetails, spec 03
        // §5.4.1) ride in metadata.details so tool-specific transcript cards
        // can render without parsing the output text.
        pushPart({
          ...base,
          state: {
            status: 'completed',
            input,
            output: textOfContent(result.content),
            title: intent ?? block.name,
            metadata: {
              ...(intent ? { intent } : {}),
              ...(hasDetails(result) ? { details: result.details } : {})
            },
            time: {
              start: message.timestamp,
              end: result.timestamp ?? message.timestamp
            }
          }
        });
      }
    }
  }

  const completedAt = message.stopReason === 'error' || message.stopReason === 'aborted' ? undefined : message.timestamp;

  const info: WireMessageInfo = {
    id,
    sessionID,
    role: 'assistant',
    time: {
      created: message.timestamp,
      ...(completedAt !== undefined ? { completed: completedAt } : {})
    },
    ...(message.errorMessage
      ? {
          error: {
            name: 'UnknownError',
            data: { message: message.errorMessage }
          }
        }
      : {}),
    // Branch/fork history rewrite strips unpaired tool calls and stamps the
    // count (SDK session-context.ts StrippedToolCallsMarker); the UI renders
    // an elided-activity line from it (TUI StrippedToolCallsPlaceholder).
    ...(typeof message.strippedToolCalls === 'number' && message.strippedToolCalls > 0 ? { metadata: { ompStrippedToolCalls: message.strippedToolCalls } } : {}),
    parentID: parentID ?? '',
    modelID: selector.modelID,
    providerID: selector.providerID,
    mode: agent ?? 'build',
    agent: agent ?? 'build',
    path: { cwd: directory ?? '', root: directory ?? '' },
    ...(message.stopReason !== undefined ? { finish: message.stopReason } : {}),
    cost,
    tokens
  };
  return { info, parts };
};

/**
 * Project a full omp message list into wire `{info, parts}[]` pairs.
 * `messages` is the AgentMessage[] of a session (messages getter or rebuilt
 * context). ToolResultMessages are paired into the preceding assistant
 * message's tool parts.
 */
/**
 * 把完整的 omp 消息列表投影为 wire `{info, parts}[]` 对。`messages` 是
 * 会话的 AgentMessage[]（messages getter 或重建的上下文）。
 * ToolResultMessage 会被配对进前一条 assistant 消息的工具 part。
 */
export const projectConversation = (
  messages: readonly MessageInput[] | null | undefined,
  options: ConversationProjectionOptions,
): ProjectedMessage[] => {
  const out: ProjectedMessage[] = [];
  let lastUserWireId = '';
  let pendingAssistant: AssistantMessageInput | null = null;
  let pendingResults: Map<string, ProjectedToolResult> = new Map();

  // 冲刷挂起的 assistant 消息：连同已配对的工具结果一起投影入列。
  const flushAssistant = () => {
    if (!pendingAssistant) return;
    const wireId = options?.wireIdFor?.(pendingAssistant);
    out.push(
      projectAssistantMessage(pendingAssistant, pendingResults, {
        ...options,
        ...(wireId ? { wireId } : {}),
        parentID: lastUserWireId
      })
    );
    pendingAssistant = null;
    pendingResults = new Map();
  };

  for (const message of messages ?? []) {
    if (!message || typeof message !== 'object') continue;
    if (message.role === 'user') {
      flushAssistant();
      const wireId = options?.wireIdFor?.(message);
      // Turn-state snapshot (model + thinking as of this message in the
      // transcript log) overrides the projection-wide model default.
      const turnState = options?.turnStateFor?.(message);
      const projected = projectUserMessage(message, {
        ...options,
        ...(turnState?.model ? { model: turnState.model } : {}),
        ...(turnState?.thinkingLevel ? { thinkingLevel: turnState.thinkingLevel } : {}),
        ...(wireId ? { wireId } : {})
      });
      lastUserWireId = projected.info.id;
      out.push(projected);
    } else if (message.role === 'assistant') {
      flushAssistant();
      pendingAssistant = message;
    } else if (message.role === 'custom' || message.role === 'hookMessage') {
      if (message.display === false) continue;
      if (!textOfContent(message.content).trim()) continue;
      flushAssistant();
      out.push(
        projectCustomMessage(message, {
          ...options,
          parentID: lastUserWireId || undefined
        })
      );
    } else if (message.role === 'compactionSummary' || message.role === 'branchSummary') {
      flushAssistant();
      out.push(
        projectDividerMessage(message, {
          ...options,
          parentID: lastUserWireId || undefined
        })
      );
    } else if (message.role === 'bashExecution' || message.role === 'pythonExecution') {
      // Standalone user-side segment: never anchors turn pairing (05 §5.10).
      // Flush first so the row renders after the turn it follows in
      // transcript order, and honor the live dispatcher's echo id so a
      // re-projection reconciles onto the row the dispatch already emitted.
      flushAssistant();
      out.push(
        projectExecutionMessage(message, {
          ...options,
          wireId: options?.wireIdFor?.(message)
        })
      );
    } else if (message.role === 'fileMention') {
      out.push(projectFileMentionMessage(message, options));
    } else if (message.role === 'toolResult') {
      if (!pendingAssistant) continue;
      pendingResults.set(message.toolCallId, message);
    } else if (message.role === 'developer') {
      // TUI parity: developer-role transcript messages render in the TUI as
      // collapsed synthetic rows. Empty ones stay invisible there too.
      if (!textOfContent(message.content).trim()) continue;
      flushAssistant();
      const projected = projectDeveloperMessage(message, {
        ...options,
        parentID: lastUserWireId || undefined
      });
      if (message.attribution === 'user') {
        // A synthetic prompt occupies the user turn slot: the following
        // assistant message anchors to it like a real prompt.
        lastUserWireId = projected.info.id;
      }
      out.push(projected);
    }
  }
  flushAssistant();
  return out;
};

/**
 * Streaming projector: consumes omp AgentSessionEvents for one assistant turn
 * and emits wire events through the provided sink. Produces the same shapes as
 * `projectAssistantMessage` for the final state.
 */
/**
 * 流式投影器：消费单个助手回合的 omp AgentSessionEvents，并通过给定
 * 的事件汇发出 wire 事件。终态产出与 `projectAssistantMessage` 相同的
 * 形态。
 */
export class StreamProjector {
  /** 会话 id。 */
  declare sessionID: string;
  /** 工作目录（事件作用域 + wire path 字段）。 */
  declare directory: string | undefined;
  /** agent 标识（默认 'build'）。 */
  declare agent: string | undefined;
  /** wire 事件汇（host 总线）。 */
  declare emit: StreamProjectorEmit;
  /** 当前回合的 wire 消息信息；回合之外为 null。 */
  declare current: WireMessageInfo | null;
  /** part 序号发生器（part id 的 seq 段）。 */
  declare seq: number;
  /** 当前文本 part id（惰性创建）。 */
  declare textPartId: string | null;
  /** 已发出的文本增量总长度。 */
  declare textLength: number;
  /** 当前推理 part id（惰性创建）。 */
  declare reasoningPartId: string | null;
  /** 已发出的推理增量总长度。 */
  declare reasoningLength: number;
  /** Finalized tool parts by callID — async-job updates revive them (toolPartial). */
  /** 已定稿工具 part 表（callID → 坐标）—— 异步任务更新据此复活（toolPartial）。 */
  declare finalToolParts: Map<string, { id: string; messageID: string; toolName: string }>;
  /** callID → 工具 part id。 */
  declare toolPartIds: Map<string, string>;
  /** callID → 工具名。 */
  declare toolNames: Map<string, string>;
  /** callID → 解析后的调用参数。 */
  declare toolInputs: Map<string, ToolCallArguments>;
  /** callID → 开始时间。 */
  declare toolStartTimes: Map<string, number>;
  /** callID → 累积的部分输出文本。 */
  declare toolPartialText: Map<string, string>;
  /** callID → 累积的部分元数据。 */
  declare toolPartialMeta: Map<string, WireToolMetadata>;
  /** 当前回合锚定的父消息 id（空串 = 独立段）。 */
  declare parentID: string;

  /** 以会话坐标与事件汇构造投影器，并重置全部回合内状态（引擎每个助手回合新建一个实例）。 */
  constructor({ sessionID, directory, agent, emit, sharedFinalToolParts }: StreamProjectorOptions) {
    this.sessionID = sessionID;
    this.directory = directory;
    this.agent = agent;
    this.emit = emit;
    this.current = null;
    this.seq = 0;
    this.textPartId = null;
    this.textLength = 0;
    this.reasoningPartId = null;
    this.reasoningLength = 0;
    this.toolPartIds = new Map();
    this.toolNames = new Map();
    this.toolStartTimes = new Map();
    this.toolPartialText = new Map();
    this.toolPartialMeta = new Map();
    this.finalToolParts = sharedFinalToolParts ?? new Map();
    this.parentID = '';
  }

  /** 设置当前回合锚定的父消息 id（undefined 归一为空串）。 */
  setParentID(parentID?: string) {
    this.parentID = parentID ?? '';
  }

  // SAFETY: part ids are only minted between startAssistant and the
  // settle path, so `current` is set at every call site.
  /** 读取当前消息 id；part id 仅在 startAssistant 与 settle 路径之间铸造，故 current 在每个调用点必然已就位。 */
  #currentId(): string {
    // SAFETY: parts are only minted between startAssistant and settle, so
    // `current` is set at every call site.
    return (this.current as { id: string }).id;
  }

  /** 以当前消息 id 与自增 seq 铸造下一个 part id。 */
  #newPartId() {
    return partId(this.#currentId(), this.seq++);
  }

  /** 通过事件汇发出 message.part.updated（附带当前时间戳）。 */
  #emitPartUpdated(part: WireMessagePart) {
    this.emit('message.part.updated', { sessionID: this.sessionID, part, time: Date.now() }, this.directory);
  }

  /** Returns the wire message info for the started assistant message. */
  /** 开启一条 assistant 消息：铸造稳定 id、发出 message.updated 与首个 step-start part；返回该 wire 消息信息。 */
  startAssistant(message: AssistantMessageInput): WireMessageInfo {
    // Stable formula (plan phase 5): seed is content-independent so the
    // start-time id equals every later cold re-projection of the settled
    // message (the SDK never mutates the creation timestamp).
    this.current = {
      id: deterministicWireId(message),
      sessionID: this.sessionID,
      role: 'assistant',
      time: { created: message.timestamp },
      parentID: this.parentID,
      modelID: message.model ?? '',
      providerID: message.provider ?? '',
      mode: this.agent ?? 'build',
      agent: this.agent ?? 'build',
      path: { cwd: this.directory ?? '', root: this.directory ?? '' },
      cost: 0,
      tokens: {
        input: 0,
        output: 0,
        reasoning: 0,
        cache: { read: 0, write: 0 }
      }
    };
    this.seq = 0;
    this.textPartId = null;
    this.textLength = 0;
    this.reasoningPartId = null;
    this.reasoningLength = 0;
    this.toolPartIds = new Map();
    this.toolInputs = new Map();
    this.toolStartTimes = new Map();
    this.toolPartialText = new Map();
    this.toolPartialMeta = new Map();
    this.emit('message.updated', { sessionID: this.sessionID, info: this.current }, this.directory);
    const stepStartId = this.#newPartId();
    this.#emitPartUpdated({
      id: stepStartId,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'step-start'
    });
    return this.current;
  }

  /** 惰性创建文本 part（首个文本增量时先发出空 text part）并返回其 id。 */
  #ensureTextPart() {
    if (this.textPartId) return this.textPartId;
    this.textPartId = this.#newPartId();
    this.#emitPartUpdated({
      id: this.textPartId,
      sessionID: this.sessionID,
      messageID: this.#currentId(),
      type: 'text',
      text: '',
      time: { start: Date.now() }
    });
    return this.textPartId;
  }

  /** 追加一段文本增量：确保 text part 存在并发出 message.part.delta。 */
  textDelta(delta: string) {
    if (!this.current) return;
    const partIdNow = this.#ensureTextPart();
    this.textLength += delta.length;
    this.emit(
      'message.part.delta',
      {
        sessionID: this.sessionID,
        messageID: this.current.id,
        partID: partIdNow,
        field: 'text',
        delta
      },
      this.directory
    );
  }

  /** 惰性创建推理 part（首个推理增量时先发出空 reasoning part）并返回其 id。 */
  #ensureReasoningPart() {
    if (this.reasoningPartId) return this.reasoningPartId;
    this.reasoningPartId = this.#newPartId();
    this.#emitPartUpdated({
      id: this.reasoningPartId,
      sessionID: this.sessionID,
      messageID: this.#currentId(),
      type: 'reasoning',
      text: '',
      time: { start: Date.now() }
    });
    return this.reasoningPartId;
  }

  /** 追加一段推理（thinking）增量：确保 reasoning part 存在并发出 message.part.delta。 */
  thinkingDelta(delta: string) {
    if (!this.current) return;
    const partIdNow = this.#ensureReasoningPart();
    this.reasoningLength += delta.length;
    this.emit(
      'message.part.delta',
      {
        sessionID: this.sessionID,
        messageID: this.current.id,
        partID: partIdNow,
        field: 'text',
        delta
      },
      this.directory
    );
  }

  /**
   * tool_execution_start / streaming tool calls. `input` is the parsed args.
   */
  /** tool_execution_start / 流式工具调用。`input` 为解析后的参数映射；同一 callID 重复开始会被忽略。 */
  toolStarted(
    callID: string,
    toolName: string,
    input?: ToolCallArguments,
    { title }: { title?: string } = {},
  ) {
    if (!this.current) return;
    if (this.toolPartIds.has(callID)) return;
    const id = this.#newPartId();
    this.toolPartIds.set(callID, id);
    this.toolNames.set(callID, toolName);
    this.toolInputs.set(callID, input ?? {});
    this.toolPartialText.delete(callID);
    this.toolPartialMeta.delete(callID);
    this.toolStartTimes.set(callID, Date.now());
    this.#emitPartUpdated({
      id,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'tool',
      callID,
      tool: toolName,
      state: {
        status: 'running',
        input: input ?? {},
        ...(title ? { title } : {}),
        time: { start: Date.now() }
      }
    });
  }

  /**
   * tool_execution_update: append partial output to a running tool part
   * (spec 05 §5.6). Never sets a terminal state — tool_execution_end owns
   * completion (TUI parity: partial async snapshots are only terminal for
   * parked background blocks, which the engine cannot reliably replicate, so
   * it stays conservative).
   */
  /**
   * tool_execution_update：向运行中的工具 part 追加部分输出（spec 05
   * §5.6）。绝不设置终态 —— 完成归 tool_execution_end 所有（TUI 对齐：
   * 部分异步快照仅对停靠的后台块是终态，引擎无法可靠复刻，故保持保守）。
   */
  toolPartial(
    callID: string,
    { text, asyncState, details }: { text?: string; asyncState?: string; details?: unknown } = {},
  ) {
    if (!this.current) return;
    let id = this.toolPartIds.get(callID);
    let messageID = this.current.id;
    let revived = false;
    let recordedToolName = '';
    if (!id) {
      // Async-job task updates keep arriving after the owning turn settled
      // (the tool call returned a spawn snapshot while the background job
      // runs on). Revive the finalized part — recorded at toolFinished in the
      // session-shared map — so the transcript card keeps refreshing instead
      // of freezing at the spawn snapshot. Scoped to structured snapshots
      // (task tool details); plain text updates for dead calls stay dropped.
      // The job's asyncState settles the revived part when it lands.
      const finalized = this.finalToolParts.get(callID);
      if (!finalized || details === undefined) return;
      id = finalized.id;
      messageID = finalized.messageID;
      revived = true;
      recordedToolName = finalized.toolName;
    }
    const reviveStatus = (state: string | undefined): 'running' | 'completed' | 'error' =>
      state === 'completed' ? 'completed' : state === 'failed' ? 'error' : 'running';
    const toolName = revived ? recordedToolName : (this.toolNames.get(callID) ?? '');
    const startedAt = this.toolStartTimes.get(callID) ?? Date.now();
    if (typeof text === 'string' && text.length > 0) {
      const acc = this.toolPartialText.get(callID) ?? '';
      this.toolPartialText.set(callID, acc + text);
    }
    const output = this.toolPartialText.get(callID) ?? '';
    const priorMeta = this.toolPartialMeta.get(callID);
    // Structured live snapshots (the task tool's per-subagent AgentProgress[])
    // replace prior details wholesale — every update carries a full snapshot.
    let metadata: WireToolMetadata | undefined = priorMeta;
    if (asyncState || details !== undefined) {
      metadata = { ...(priorMeta ?? {}) };
      if (asyncState) metadata.asyncState = asyncState;
      if (details !== undefined) metadata.details = details;
      this.toolPartialMeta.set(callID, metadata);
    }
    this.#emitPartUpdated({
      id,
      sessionID: this.sessionID,
      messageID,
      type: 'tool',
      callID,
      tool: toolName,
      state: {
        status: revived ? reviveStatus(asyncState) : 'running',
        input: this.toolInputs.get(callID) ?? {},
        ...(output ? { output } : {}),
        ...(metadata ? { metadata } : {}),
        time: { start: startedAt }
      }
    });
  }

  /** 工具调用终结：发出 completed/error 终态 part，并把坐标登记进跨代共享表（finalToolParts）供异步任务更新复活。 */
  toolFinished(
    callID: string,
    { output, error, metadata }: { output?: string; error?: string; metadata?: WireToolMetadata } = {},
  ) {
    if (!this.current) return;
    const id = this.toolPartIds.get(callID);
    if (!id) return;
    const toolName = this.toolNames.get(callID) ?? '';
    const startedAt = this.toolStartTimes.get(callID) ?? Date.now();
    this.finalToolParts.set(callID, { id, messageID: this.current.id, toolName });
    if (this.finalToolParts.size > 64) {
      // Bounded: async-job windows span at most a few turns; drop the oldest
      // entries so a long session cannot grow the map indefinitely.
      const oldest = this.finalToolParts.keys().next().value;
      if (typeof oldest === 'string') this.finalToolParts.delete(oldest);
    }
    this.#emitPartUpdated({
      id,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'tool',
      callID,
      tool: toolName,
      state: error
        ? {
            status: 'error',
            input: this.toolInputs.get(callID) ?? {},
            error,
            time: { start: startedAt, end: Date.now() }
          }
        : {
            status: 'completed',
            input: this.toolInputs.get(callID) ?? {},
            output: output ?? '',
            title: toolName,
            metadata: metadata ?? {},
            time: { start: startedAt, end: Date.now() }
          }
    });
  }

  /**
   * Finalize the assistant message. `message` is the settled omp
   * AssistantMessage; tool names/results are re-projected from its content and
   * the provided results so the final part states are authoritative.
   */
  /**
   * 收束（finalize）assistant 消息。`message` 为已定稿的 omp
   * AssistantMessage；工具名与结果从其内容和所给结果重新投影，
   * 使最终 part 状态成为权威。
   */
  finishAssistant(message: AssistantMessageInput, toolResults: Map<string, ProjectedToolResult>): WireMessageInfo | null {
    if (!this.current) return this.current;
    const { info, parts } = projectAssistantMessage(message, toolResults, {
      sessionID: this.sessionID,
      directory: this.directory,
      agent: this.agent,
      parentID: this.parentID,
      wireId: this.current.id
    });
    this.current = info;
    this.emit('message.updated', { sessionID: this.sessionID, info }, this.directory);
    for (const part of parts) {
      this.#emitPartUpdated(part);
    }
    return info;
  }
}
