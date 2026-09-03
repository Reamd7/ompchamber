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
export type ToolCallArguments = Extract<AgentSessionEvent, { type: 'tool_execution_start' }>['args'];

 /** One content block as the projectors read it (flat view of the SDK block union). */
export interface ProjectedContentBlock {
  type: string;
  text?: string;
  thinking?: string;
  name?: string;
  data?: string;
  mimeType?: string;
  id?: string;
  arguments?: ToolCallArguments | string;
  intent?: string;
}

/** Message content as the projectors accept it: bare text or a block list. */
export type ProjectedContentInput = string | readonly ProjectedContentBlock[];

/** omp usage report fields the usage projection reads (all optional; callers pass `{}` fallbacks). */
export interface UsageInput {
  input?: number;
  output?: number;
  reasoningTokens?: number;
  cacheRead?: number;
  cacheWrite?: number;
  /** SDK Usage.totalTokens — the authoritative final-round-trip window
   * (input+output+cacheRead+cacheWrite + orchestration). Emitted as the wire
   * `tokens.total` so the UI context meter prefers it over summing buckets
   * (OpenCode-wire precedent; TUI computes context from session accounting
   * instead — see docs/omp-host-field-loss-fix-plan.md P7). */
  totalTokens?: number;
  /** omp reports per-message cost as a number; the SDK usage object carries a cost breakdown. */
  cost?: number | { input?: number; output?: number; cacheRead?: number; cacheWrite?: number; total?: number };
}

/** omp UserMessage (role + content + timestamp is the whole projection-relevant shape). */
export interface UserMessageInput {
  role: 'user';
  content?: ProjectedContentInput;
  /** System-injected (auto-continue etc.); TUI renders dimmed/collapsed. */
  synthetic?: boolean;
  timestamp: number;
}

/** omp DeveloperMessage — harness-injected transcript input; projected as
 * synthetic rows (attribution 'user' occupies the user turn slot, anything
 * else rides the current turn as an assistant-side note). */
export interface DeveloperMessageInput {
  role: 'developer';
  content?: ProjectedContentInput;
  /** Injection origin ('user' synthetic prompts, 'agent' mid-turn nudges). */
  attribution?: 'user' | 'agent';
  timestamp: number;
}

/** omp AssistantMessage fields the assistant projector reads. */
export interface AssistantMessageInput {
  role?: 'assistant';
  content?: readonly ProjectedContentBlock[];
  timestamp: number;
  model?: string;
  provider?: string;
  usage?: UsageInput;
  stopReason?: string;
  /** Tool calls stripped by branch/fork history rewrite (StrippedToolCallsMarker). */
  strippedToolCalls?: number;
  errorMessage?: string;
}

/** omp ToolResultMessage as the pairing map stores it. */
export interface ToolResultMessageInput {
  role: 'toolResult';
  toolCallId: string;
  content?: readonly ProjectedContentBlock[];
  details?: unknown;
  isError?: boolean;
  timestamp?: number;
}

/** `!`/`$` shell-kernel execution message (bash or python role). */
export interface ShellExecutionMessageInput {
  role: 'bashExecution' | 'pythonExecution';
  command?: string;
  code?: string;
  output?: string;
  exitCode?: number;
  cancelled?: boolean;
  timestamp: number;
}

/** fileMention message files as the projector reads them. */
export interface FileMentionFileInput {
  path?: string;
  lineCount?: number;
}

/** omp fileMention message. */
export interface FileMentionMessageInput {
  role: 'fileMention';
  files?: readonly FileMentionFileInput[];
  timestamp: number;
}

/** Dispatchable message set of a session transcript (SDK custom/divider types imported as-is). */
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
export interface WireIdMessageInput {
  role?: string;
  content?: ProjectedContentInput;
  timestamp: number;
}

/** Resolves a message to a pre-issued wire id (live/cold id bridge); undefined = derive deterministically. */
export type WireIdResolver = (message: WireIdMessageInput) => string | undefined;

// ---------------------------------------------------------------------------
// Output contracts — the OpenCode-compatible wire shapes the engine emits on
// the host bus and the UI's message loader consumes.
// ---------------------------------------------------------------------------

/** `{ providerID, modelID }` selector as the wire model reports a model. */
export interface WireModelSelector {
  providerID: string;
  modelID: string;
  /** Thinking-level variant snapshot (send-time effort slot). */
  variant?: string;
}

/** Model reference accepted on user-message options: selector string or model object. */
export type ModelSelectorInput = string | { provider?: string; id?: string };

/** Wire token totals (per-message projection of an omp usage report). */
export interface WireTokenTotals {
  input: number;
  output: number;
  reasoning: number;
  cache: { read: number; write: number };
  /** Final-round-trip window when the SDK reported it; omitted otherwise. */
  total?: number;
}

/** `projectUsage` result. */
export interface WireUsageProjection {
  tokens: WireTokenTotals;
  cost: number;
}

/** Wire message time span. */
export interface WireMessageTime {
  created: number;
  completed?: number;
}

/** Wire part time span. */
export interface WirePartTime {
  start: number;
  end?: number;
}

/** Wire message metadata — the `ompRole`-keyed variant payloads the UI branches on. */
export interface WireMessageMetadata {
  ompRole?: string;
  /** Count of tool calls stripped by branch/fork history rewrite (SDK
   * StrippedToolCallsMarker); the UI renders an elided-activity line. */
  ompStrippedToolCalls?: number;
  command?: string;
  exitCode?: number;
  cancelled?: boolean;
  tokensBefore?: number;
  warning?: string;
  fromId?: string;
  files?: Array<{ path?: string; lines?: number }>;
  /** Divider attribution (05 §5.5): the model selector a model_change divider
   * echoes; the mode value a mode_change divider carries. */
  model?: string;
  mode?: string;
  role?: string;
  fallback?: boolean;
}

/** Wire tool-part state (running/completed/error snapshot of one tool call). */
export interface WireToolMetadata {
  intent?: string;
  details?: unknown;
  asyncState?: string;
}

/** Wire tool-part state. */
export interface WireToolState {
  status: 'running' | 'completed' | 'error';
  input: ToolCallArguments;
  output?: string;
  error?: string;
  title?: string;
  metadata?: WireToolMetadata;
  time: WirePartTime;
}

/** One wire message part (flat view: step-start / text / reasoning / tool / file). */
export interface WireMessagePart {
  id: string;
  sessionID: string;
  messageID: string;
  type: string;
  text?: string;
  time?: WirePartTime;
  synthetic?: boolean;
  mime?: string;
  url?: string;
  callID?: string;
  tool?: string;
  state?: WireToolState;
}

/** Wire message info (flat view: user and assistant variants share the core fields). */
export interface WireMessageInfo {
  id: string;
  sessionID: string;
  role: 'user' | 'assistant';
  time: WireMessageTime;
  agent: string;
  model?: WireModelSelector;
  parentID?: string;
  metadata?: WireMessageMetadata;
  modelID?: string;
  providerID?: string;
  mode?: string;
  path?: { cwd: string; root: string };
  cost?: number;
  tokens?: WireTokenTotals;
  /** Terminal stop reason ('stop' | 'length' | 'toolUse' | 'error' | 'aborted');
   * present only on settled assistant messages — its presence is the wire
   * "step closed" signal (ChatMessage open-step check). */
  finish?: string;
  /** Compaction/branch divider marker: OpenCode's turn-summary picker skips
   * summary messages when choosing a turn's answer text. */
  summary?: boolean;
  error?: { name: string; data: { message: string } };
}

/** One projected message: wire info plus its ordered parts. */
export interface ProjectedMessage {
  info: WireMessageInfo;
  parts: WireMessagePart[];
}

/** Tool result as the assistant projector's pairing map stores it. */
export interface ProjectedToolResult {
  content?: readonly ProjectedContentBlock[];
  details?: unknown;
  isError?: boolean;
  timestamp?: number;
}

/** `normalizeToolExecutionResult` result. */
export interface NormalizedToolResult {
  content: readonly ProjectedContentBlock[];
  text: string;
  details?: unknown;
}

/** Tool execution end result: SDK AgentToolResult `{content, details}` or a plain string. */
export type ToolExecutionResultInput = { content?: readonly ProjectedContentBlock[]; details?: unknown } | string | null | undefined;

/** Transcript entry as wire-id resolution walks it (only `type: "message"` entries carry a message). */
export interface TranscriptEntryInput {
  type: string;
  id?: string;
  message?: MessageInput;
}

/** `resolveWireIdToEntryId` options. */
export interface WireIdResolveOptions {
  wireIdFor?: WireIdResolver;
}

/** Message-history paging options (OpenCode `limit`/`before` contract). */
export interface PaginationOptions {
  limit?: number;
  before?: string;
}

/** `paginateProjectedMessages` result. */
export interface ProjectedMessagePage {
  messages: readonly ProjectedMessage[];
  cursor?: string;
}

/** Options shared by the synthetic user-side projectors (execution, file-mention). */
export interface BasicProjectionOptions {
  sessionID: string;
  agent?: string;
}

/** Options for the custom/hook message projector. */
export interface CustomProjectionOptions {
  sessionID: string;
  agent?: string;
  parentID?: string;
}

/** Options for the divider (compactionSummary / branchSummary) projector. */
export interface DividerProjectionOptions {
  sessionID: string;
  agent?: string;
  parentID?: string;
}

/** Options for the user-message projector. */
export interface UserProjectionOptions {
  sessionID: string;
  agent?: string;
  model?: ModelSelectorInput;
  /** Send-time thinking level; rides wire `model.variant` when present. */
  thinkingLevel?: string;
  wireId?: string;
}

/** Options for the assistant-message projector. */
export interface AssistantProjectionOptions {
  sessionID: string;
  agent?: string;
  directory?: string;
  parentID?: string;
  wireId?: string;
}

/** Options for the full-conversation projector. */
export interface ConversationProjectionOptions {
  sessionID: string;
  directory?: string;
  agent?: string;
  model?: ModelSelectorInput;
  wireIdFor?: WireIdResolver;
  /** Per-user-message send-time model/thinking snapshot (engine-built). */
  turnStateFor?: (message: WireIdMessageInput) => { model?: string; thinkingLevel?: string } | null | undefined;
  wireId?: string;
}

/** `message.updated` bus payload emitted by the streaming projector. */
export interface WireMessageUpdatedProperties {
  sessionID: string;
  info: WireMessageInfo;
}

/** `message.part.updated` bus payload emitted by the streaming projector. */
export interface WirePartUpdatedProperties {
  sessionID: string;
  part: WireMessagePart;
  time?: number;
}

/** `message.part.delta` bus payload emitted by the streaming projector. */
export interface WirePartDeltaProperties {
  sessionID: string;
  messageID: string;
  partID: string;
  field: 'text';
  delta: string;
}

/** Sink the streaming projector emits wire events through (the host bus). */
export type StreamProjectorEmit = (
  type: 'message.updated' | 'message.part.updated' | 'message.part.delta',
  properties: WireMessageUpdatedProperties | WirePartUpdatedProperties | WirePartDeltaProperties,
  directory?: string,
) => void;

/** StreamProjector constructor options. */
export interface StreamProjectorOptions {
  sessionID: string;
  directory?: string;
  agent?: string;
  emit: StreamProjectorEmit;
}

// ---------------------------------------------------------------------------
// Wire-id derivation
// ---------------------------------------------------------------------------

const BASE36 = '0123456789abcdefghijklmnopqrstuvwxyz';

const toBase36 = (value: number, pad = 8) => {
  let out = '';
  let n = value;
  do {
    out = BASE36[n % 36] + out;
    n = Math.floor(n / 36);
  } while (n > 0);
  return out.padStart(pad, '0');
};

const contentDigest = (text?: string | null) => {
  const hash = crypto.createHash('sha256').update(String(text ?? '')).digest('hex');
  return hash.slice(0, 4);
};

export const splitModelSelector = (modelId?: string | null): WireModelSelector => {
  const separator = String(modelId ?? '').indexOf('/');
  if (separator === -1) return { providerID: '', modelID: String(modelId ?? '') };
  return {
    providerID: String(modelId).slice(0, separator),
    modelID: String(modelId).slice(separator + 1)
  };
};

export const wireMessageId = (role: string | null | undefined, timestamp: number, seedText?: string | null) => {
  const roleChar = role === 'assistant' ? 'a' : role === 'custom' ? 'c' : 'u';
  return `msg_${roleChar}${toBase36(timestamp)}${contentDigest(seedText)}`;
};

/**
 * Deterministic wire id of one engine message — the exact derivation the
 * user/assistant projectors use (assistant messages with no text seed from
 * the first content block's name).
 */
export const deterministicWireId = (message: WireIdMessageInput) => {
  const seed = message.role === 'assistant' && !textOfContent(message.content)
    ? ((Array.isArray(message.content) ? message.content[0]?.name : undefined) ?? '')
    : textOfContent(message.content);
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

const textOfContent = (content?: ProjectedContentInput | null) => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((block) => block && block.type === 'text')
    .map((block) => block.text)
    .join('');
};

/** True when a tool result carries structured, non-empty details. */
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

const imageBlocks = (content?: ProjectedContentInput | null): ProjectedContentBlock[] =>
  Array.isArray(content) ? content.filter((block) => block && block.type === 'image') : [];

const partId = (messageWireId: string, seq: number) => `prt_${messageWireId.slice(4)}_${seq}`;

/**
 * Coerce one tool-arguments value into the wire input map: object maps pass
 * through, stringified JSON is parsed, and anything else degrades to
 * `{ input: value }` (or `{}`) so tool state always carries an object.
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
type SessionEntryLike = {
  type: string;
  model?: unknown;
  thinkingLevel?: unknown;
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
const unwrapFullBodyXml = (text: string): string => {
  const wrapped = text.match(/^<([a-zA-Z-]+)[^>]*>\s*([\s\S]*?)\s*<\/\1>\s*$/);
  return wrapped ? wrapped[2] : text;
};

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
export interface DividerEntryInput {
  type: string;
  timestamp?: string | number;
  model?: string;
  role?: string;
  resolvedModelIsFallback?: boolean;
  thinkingLevel?: string;
  mode?: string;
}

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

/**
 * Project a `!`/`$` shell-kernel execution role into a user-side synthetic
 * message (spec 05 §5.10, GAP-E14). Standalone segment (parentID='') so
 * turn pairing never anchors on it; the `[omp:bash]`/`[omp:python]` prefix
 * routes the UI to the execution-card renderer (shellAction classification).
 */
export const projectExecutionMessage = (
  message: ShellExecutionMessageInput,
  { sessionID, agent }: BasicProjectionOptions,
): ProjectedMessage => {
  const kind = message.role === 'pythonExecution' ? 'python' : 'bash';
  const command = kind === 'python' ? (message.code ?? '') : (message.command ?? '');
  const label = `[omp:${kind}] `;
  const output = String(message.output ?? '');
  const cancelled = message.cancelled ? ' (cancelled)' : '';
  const exit = message.exitCode !== undefined ? ` [exit ${message.exitCode}]` : '';
  const id = wireMessageId('custom', message.timestamp, label + command + output + exit + cancelled);
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
        cancelled: Boolean(message.cancelled)
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
        time: { start: message.timestamp }
      }
    ]
  };
};

/**
 * Project a fileMention role into a user-side synthetic message — one
 * `└ Read <path> (N lines)` line per file (TUI messages.ts:294-302).
 */
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
export const projectAssistantMessage = (
  message: AssistantMessageInput,
  toolResults: Map<string, ProjectedToolResult>,
  { sessionID, agent, directory, parentID, wireId }: AssistantProjectionOptions,
): ProjectedMessage => {
  const seed = textOfContent(message.content) || (message.content?.[0]?.name ?? '');
  const id = wireId ?? wireMessageId('assistant', message.timestamp, seed);
  const selector = {
    providerID: message.provider ?? splitModelSelector(message.model ?? '').providerID,
    modelID: message.model ?? ''
  };
  const { tokens, cost } = projectUsage(message.usage);

  const parts: WireMessagePart[] = [];
  let seq = 0;
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
export const projectConversation = (
  messages: readonly MessageInput[] | null | undefined,
  options: ConversationProjectionOptions,
): ProjectedMessage[] => {
  const out: ProjectedMessage[] = [];
  let lastUserWireId = '';
  let pendingAssistant: AssistantMessageInput | null = null;
  let pendingResults: Map<string, ProjectedToolResult> = new Map();

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
      out.push(projectExecutionMessage(message, options));
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
export class StreamProjector {
  declare sessionID: string;
  declare directory: string | undefined;
  declare agent: string | undefined;
  declare emit: StreamProjectorEmit;
  declare current: WireMessageInfo | null;
  declare seq: number;
  declare textPartId: string | null;
  declare textLength: number;
  declare reasoningPartId: string | null;
  declare reasoningLength: number;
  declare toolPartIds: Map<string, string>;
  declare toolNames: Map<string, string>;
  declare toolInputs: Map<string, ToolCallArguments>;
  declare toolStartTimes: Map<string, number>;
  declare toolPartialText: Map<string, string>;
  declare toolPartialMeta: Map<string, WireToolMetadata>;
  declare parentID: string;

  constructor({ sessionID, directory, agent, emit }: StreamProjectorOptions) {
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
    this.toolInputs = new Map();
    this.toolStartTimes = new Map();
    this.toolPartialText = new Map();
    this.toolPartialMeta = new Map();
    this.parentID = '';
  }

  setParentID(parentID?: string) {
    this.parentID = parentID ?? '';
  }

  // SAFETY: part ids are only minted between startAssistant and the
  // settle path, so `current` is set at every call site.
  #currentId(): string {
    // SAFETY: parts are only minted between startAssistant and settle, so
    // `current` is set at every call site.
    return (this.current as { id: string }).id;
  }

  #newPartId() {
    return partId(this.#currentId(), this.seq++);
  }

  #emitPartUpdated(part: WireMessagePart) {
    this.emit('message.part.updated', { sessionID: this.sessionID, part, time: Date.now() }, this.directory);
  }

  /** Returns the wire message info for the started assistant message. */
  startAssistant(message: AssistantMessageInput): WireMessageInfo {
    const seed = textOfContent(message.content) || (message.content?.[0]?.name ?? '');
    this.current = {
      id: wireMessageId('assistant', message.timestamp, seed),
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
  toolPartial(callID: string, { text, asyncState }: { text?: string; asyncState?: string } = {}) {
    if (!this.current) return;
    const id = this.toolPartIds.get(callID);
    if (!id) return;
    const toolName = this.toolNames.get(callID) ?? '';
    const startedAt = this.toolStartTimes.get(callID) ?? Date.now();
    if (typeof text === 'string' && text.length > 0) {
      const acc = this.toolPartialText.get(callID) ?? '';
      this.toolPartialText.set(callID, acc + text);
    }
    const output = this.toolPartialText.get(callID) ?? '';
    const priorMeta = this.toolPartialMeta.get(callID);
    const metadata: WireToolMetadata | undefined = asyncState ? { ...(priorMeta ?? {}), asyncState } : priorMeta;
    if (metadata !== undefined) this.toolPartialMeta.set(callID, metadata);
    this.#emitPartUpdated({
      id,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'tool',
      callID,
      tool: toolName,
      state: {
        status: 'running',
        input: this.toolInputs.get(callID) ?? {},
        ...(output ? { output } : {}),
        ...(metadata ? { metadata } : {}),
        time: { start: startedAt }
      }
    });
  }

  toolFinished(
    callID: string,
    { output, error, metadata }: { output?: string; error?: string; metadata?: WireToolMetadata } = {},
  ) {
    if (!this.current) return;
    const id = this.toolPartIds.get(callID);
    if (!id) return;
    const toolName = this.toolNames.get(callID) ?? '';
    const startedAt = this.toolStartTimes.get(callID) ?? Date.now();
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
