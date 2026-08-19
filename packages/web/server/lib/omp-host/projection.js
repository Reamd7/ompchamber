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

const BASE36 = '0123456789abcdefghijklmnopqrstuvwxyz';

const toBase36 = (value, pad = 8) => {
  let out = '';
  let n = value;
  do {
    out = BASE36[n % 36] + out;
    n = Math.floor(n / 36);
  } while (n > 0);
  return out.padStart(pad, '0');
};

const contentDigest = (text) => {
  const hash = crypto.createHash('sha256').update(String(text ?? '')).digest('hex');
  return hash.slice(0, 4);
};

export const splitModelSelector = (modelId) => {
  const separator = String(modelId ?? '').indexOf('/');
  if (separator === -1) return { providerID: '', modelID: String(modelId ?? '') };
  return {
    providerID: String(modelId).slice(0, separator),
    modelID: String(modelId).slice(separator + 1),
  };
};

export const wireMessageId = (role, timestamp, seedText) => {
  const roleChar = role === 'assistant' ? 'a' : role === 'custom' ? 'c' : 'u';
  return `msg_${roleChar}${toBase36(timestamp)}${contentDigest(seedText)}`;
};

/**
 * Page a chronologically ascending wire-message list by the OpenCode
 * message-history contract: `limit` caps the newest tail, `before` is an
 * exclusive message-id boundary, and the returned cursor is the oldest id of
 * the page when older messages remain. An unknown `before` id yields an empty
 * page so clients stop paging instead of looping over stale cursors.
 */
export const paginateProjectedMessages = (messages, { limit, before } = {}) => {
  let windowed = messages;
  if (before) {
    const boundary = messages.findIndex((message) => message?.info?.id === before);
    windowed = boundary === -1 ? [] : messages.slice(0, boundary);
  }
  const pageLimit = typeof limit === 'number' && Number.isFinite(limit) && limit > 0
    ? Math.floor(limit)
    : undefined;
  if (pageLimit === undefined || windowed.length <= pageLimit) {
    return { messages: windowed, cursor: undefined };
  }
  const page = windowed.slice(-pageLimit);
  return { messages: page, cursor: page[0]?.info?.id };
};

export const projectUsage = (usage) => {
  const u = usage ?? {};
  return {
    tokens: {
      input: u.input ?? 0,
      output: u.output ?? 0,
      reasoning: u.reasoningTokens ?? 0,
      cache: {
        read: u.cacheRead ?? 0,
        write: u.cacheWrite ?? 0,
      },
    },
    // omp reports cost through usage reports rather than per-message totals;
    // per-message cost is surfaced as zero and session aggregates come from
    // usage reports when available.
    cost: typeof u.cost === 'number' ? u.cost : 0,
  };
};

const textOfContent = (content) => {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((block) => block && block.type === 'text')
    .map((block) => block.text)
    .join('');
};

const imageBlocks = (content) =>
  Array.isArray(content) ? content.filter((block) => block && block.type === 'image') : [];

const partId = (messageWireId, seq) => `prt_${messageWireId.slice(4)}_${seq}`;

const safeJson = (value) => {
  if (value && typeof value === 'object' && !Array.isArray(value)) return value;
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
 */
export const projectUserMessage = (message, { sessionID, agent, model, wireId }) => {
  const id = wireId ?? wireMessageId('user', message.timestamp, textOfContent(message.content));
  const text = textOfContent(message.content);
  const parts = [];
  let seq = 0;
  if (text.length > 0) {
    parts.push({ id: partId(id, seq++), sessionID, messageID: id, type: 'text', text });
  }
  for (const image of imageBlocks(message.content)) {
    parts.push({
      id: partId(id, seq++),
      sessionID,
      messageID: id,
      type: 'file',
      mime: image.mimeType || 'image/png',
      url: `data:${image.mimeType || 'image/png'};base64,${image.data}`,
    });
  }
  const selector = model
    ? typeof model === 'string'
      ? splitModelSelector(model)
      : { providerID: model.provider ?? '', modelID: model.id ?? '' }
    : { providerID: '', modelID: '' };
  const info = {
    id,
    sessionID,
    role: 'user',
    time: { created: message.timestamp },
    agent: agent ?? 'build',
    model: { providerID: selector.providerID, modelID: selector.modelID },
  };
  return { info, parts };
};

/**
 * Project one omp transcript `custom_message` (advisor nudges, todo reminders,
 * late LSP diagnostics, ...) into a labeled assistant-side wire message so the
 * note stays visible in history without fragmenting the user's turns. The
 * `[omp:<type>]` prefix marks the text as harness-injected rather than model
 * output. Entries the engine marked `display: false`, and empty ones, are
 * dropped.
 */
export const projectCustomMessage = (message, { sessionID, agent, parentID }) => {
  const text = textOfContent(message.content);
  const label = message.customType ? `[omp:${message.customType}] ` : '[omp] ';
  const wrapped = text.match(/^<([a-zA-Z-]+)[^>]*>\s*([\s\S]*?)\s*<\/\1>\s*$/);
  const body = wrapped ? wrapped[2] : text;
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
      model: { providerID: '', modelID: '' },
    },
    parts: [
      {
        id: partId(id, 0),
        sessionID,
        messageID: id,
        type: 'text',
        text: label + body,
        synthetic: true,
        time: { start: message.timestamp },
      },
    ],
  };
};

/**
 * Project one omp AssistantMessage (with its paired ToolResultMessages) into
 * wire `{ info, parts }`. Tool results are matched by toolCallId; unpaired
 * calls are rendered in their last observed state.
 */
export const projectAssistantMessage = (
  message,
  toolResults,
  { sessionID, agent, directory, parentID, wireId },
) => {
  const seed = textOfContent(message.content) || (message.content?.[0]?.name ?? '');
  const id = wireId ?? wireMessageId('assistant', message.timestamp, seed);
  const selector = { providerID: message.provider ?? splitModelSelector(message.model ?? '').providerID, modelID: message.model ?? '' };
  const { tokens, cost } = projectUsage(message.usage);

  const parts = [];
  let seq = 0;
  const pushPart = (part) => parts.push(part);

  pushPart({
    id: partId(id, seq++),
    sessionID,
    messageID: id,
    type: 'step-start',
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
        time: { start: message.timestamp },
      });
    } else if (block.type === 'thinking') {
      pushPart({
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'reasoning',
        text: block.thinking ?? '',
        time: { start: message.timestamp, end: message.timestamp },
      });
    } else if (block.type === 'toolCall') {
      const result = toolResults.get(block.id);
      const input = safeJson(block.arguments);
      // The model states its reason for each call in `intent`; it is the
      // human-readable heading for the tool row (the raw name/command stays
      // available through state fallbacks).
      const intent = typeof block.intent === 'string' && block.intent.trim()
        ? block.intent.trim()
        : null;
      const base = {
        id: partId(id, seq++),
        sessionID,
        messageID: id,
        type: 'tool',
        callID: block.id,
        tool: block.name,
      };
      if (!result) {
        pushPart({
          ...base,
          state: {
            status: message.stopReason === 'aborted' ? 'error' : 'completed',
            input,
            ...(message.stopReason === 'aborted'
              ? { error: 'Aborted', time: { start: message.timestamp, end: message.timestamp } }
              : {
                  output: '',
                  title: intent ?? block.name,
                  metadata: intent ? { intent } : {},
                  time: { start: message.timestamp, end: message.timestamp },
                }),
          },
        });
      } else if (result.isError) {
        pushPart({
          ...base,
          state: {
            status: 'error',
            input,
            error: textOfContent(result.content) || 'Tool error',
            time: { start: message.timestamp, end: result.timestamp ?? message.timestamp },
          },
        });
      } else {
        pushPart({
          ...base,
          state: {
            status: 'completed',
            input,
            output: textOfContent(result.content),
            title: intent ?? block.name,
            metadata: intent ? { intent } : {},
            time: { start: message.timestamp, end: result.timestamp ?? message.timestamp },
          },
        });
      }
    }
  }

  const completedAt = message.stopReason === 'error' || message.stopReason === 'aborted'
    ? undefined
    : message.timestamp;

  const info = {
    id,
    sessionID,
    role: 'assistant',
    time: { created: message.timestamp, ...(completedAt !== undefined ? { completed: completedAt } : {}) },
    ...(message.errorMessage
      ? {
          error: {
            name: 'UnknownError',
            data: { message: message.errorMessage },
          },
        }
      : {}),
    parentID: parentID ?? '',
    modelID: selector.modelID,
    providerID: selector.providerID,
    mode: agent ?? 'build',
    agent: agent ?? 'build',
    path: { cwd: directory ?? '', root: directory ?? '' },
    cost,
    tokens,
  };
  return { info, parts };
};

/**
 * Project a full omp message list into wire `{info, parts}[]` pairs.
 * `messages` is the AgentMessage[] of a session (messages getter or rebuilt
 * context). ToolResultMessages are paired into the preceding assistant
 * message's tool parts.
 */
export const projectConversation = (messages, options) => {
  const out = [];
  let lastUserWireId = '';
  let pendingAssistant = null;
  let pendingResults = new Map();

  const flushAssistant = () => {
    if (!pendingAssistant) return;
    const wireId = options?.wireIdFor?.(pendingAssistant);
    out.push(projectAssistantMessage(pendingAssistant, pendingResults, {
      ...options,
      ...(wireId ? { wireId } : {}),
      parentID: lastUserWireId,
    }));
    pendingAssistant = null;
    pendingResults = new Map();
  };

  for (const message of messages ?? []) {
    if (!message || typeof message !== 'object') continue;
    if (message.role === 'user') {
      flushAssistant();
      const wireId = options?.wireIdFor?.(message);
      const projected = projectUserMessage(message, wireId ? { ...options, wireId } : options);
      lastUserWireId = projected.info.id;
      out.push(projected);
    } else if (message.role === 'assistant') {
      flushAssistant();
      pendingAssistant = message;
    } else if (message.role === 'custom') {
      if (message.display === false) continue;
      if (!textOfContent(message.content).trim()) continue;
      flushAssistant();
      out.push(projectCustomMessage(message, { ...options, parentID: lastUserWireId || undefined }));
    } else if (message.role === 'toolResult') {
      if (!pendingAssistant) continue;
      pendingResults.set(message.toolCallId, message);
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
  constructor({ sessionID, directory, agent, emit }) {
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
    this.parentID = '';
  }

  setParentID(parentID) {
    this.parentID = parentID ?? '';
  }

  #newPartId() {
    return partId(this.current.id, this.seq++);
  }

  #emitPartUpdated(part) {
    this.emit('message.part.updated', { sessionID: this.sessionID, part, time: Date.now() }, this.directory);
  }

  /** Returns the wire message info for the started assistant message. */
  startAssistant(message) {
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
      tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
    };
    this.seq = 0;
    this.textPartId = null;
    this.textLength = 0;
    this.reasoningPartId = null;
    this.reasoningLength = 0;
    this.toolPartIds = new Map();
    this.toolNames = new Map();
    this.toolInputs = new Map();
    this.toolStartTimes = new Map();
    this.emit('message.updated', { sessionID: this.sessionID, info: this.current }, this.directory);
    const stepStartId = this.#newPartId();
    this.#emitPartUpdated({ id: stepStartId, sessionID: this.sessionID, messageID: this.current.id, type: 'step-start' });
    return this.current;
  }

  #ensureTextPart() {
    if (this.textPartId) return this.textPartId;
    this.textPartId = this.#newPartId();
    this.#emitPartUpdated({
      id: this.textPartId,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'text',
      text: '',
      time: { start: Date.now() },
    });
    return this.textPartId;
  }

  textDelta(delta) {
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
        delta,
      },
      this.directory,
    );
  }

  #ensureReasoningPart() {
    if (this.reasoningPartId) return this.reasoningPartId;
    this.reasoningPartId = this.#newPartId();
    this.#emitPartUpdated({
      id: this.reasoningPartId,
      sessionID: this.sessionID,
      messageID: this.current.id,
      type: 'reasoning',
      text: '',
      time: { start: Date.now() },
    });
    return this.reasoningPartId;
  }

  thinkingDelta(delta) {
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
        delta,
      },
      this.directory,
    );
  }

  /**
   * tool_execution_start / streaming tool calls. `input` is the parsed args.
   */
  toolStarted(callID, toolName, input, { title } = {}) {
    if (!this.current) return;
    if (this.toolPartIds.has(callID)) return;
    const id = this.#newPartId();
    this.toolPartIds.set(callID, id);
    this.toolNames.set(callID, toolName);
    this.toolInputs.set(callID, input ?? {});
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
        time: { start: Date.now() },
      },
    });
  }

  toolFinished(callID, { output, error } = {}) {
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
            time: { start: startedAt, end: Date.now() },
          }
        : {
            status: 'completed',
            input: this.toolInputs.get(callID) ?? {},
            output: output ?? '',
            title: toolName,
            metadata: {},
            time: { start: startedAt, end: Date.now() },
          },
    });
  }

  /**
   * Finalize the assistant message. `message` is the settled omp
   * AssistantMessage; tool names/results are re-projected from its content and
   * the provided results so the final part states are authoritative.
   */
  finishAssistant(message, toolResults) {
    if (!this.current) return this.current;
    const { info, parts } = projectAssistantMessage(message, toolResults, {
      sessionID: this.sessionID,
      directory: this.directory,
      agent: this.agent,
      parentID: this.parentID,
      wireId: this.current.id,
    });
    this.current = info;
    this.emit('message.updated', { sessionID: this.sessionID, info }, this.directory);
    for (const part of parts) {
      this.#emitPartUpdated(part);
    }
    return info;
  }
}
