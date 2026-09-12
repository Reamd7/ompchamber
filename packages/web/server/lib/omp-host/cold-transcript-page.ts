import fs from 'node:fs';
import { parseTitleSlotLine } from '@oh-my-pi/pi-coding-agent/session/session-title-slot';
import {
  CURRENT_SESSION_VERSION,
  type FileEntry,
  type SessionEntry,
  type SessionHeader,
} from '@oh-my-pi/pi-coding-agent/session/session-entries';
import {
  createBranchSummaryMessage,
  createCompactionSummaryMessage,
  createCustomMessage,
  isCustomMessageContent,
  normalizeCustomMessagePayload,
} from '@oh-my-pi/pi-coding-agent/session/messages';
import {
  deterministicWireId,
  projectConversation,
  projectTurnEventDivider,
  wireMessageId,
} from './projection.ts';
import type {
  ConversationProjectionOptions,
  ProjectedContentInput,
  ProjectedMessage,
  ProjectedMessagePage,
  WireIdMessageInput,
  WireIdResolver,
} from './projection.ts';

/**
 * Windowed cold transcript reader (docs/PLAN.md §7.2).
 *
 * The cold `getMessagesPage` used to `SessionManager.open` the whole JSONL —
 * parse every entry, project every message — then slice one page at the end.
 * This reader bounds retained memory to the requested page instead:
 *
 *   pass 1 (stream): per-entry metadata only — id/parentId/type/timestamp
 *     for the leaf→root walk; emit kind, gates, wire ids, and flush/anchor
 *     flags for the merged display sequence; global toolCall/toolResult
 *     pairing ids; folded model/thinking state per user message; the
 *     turn-event divider and structured-row entries retained whole (small
 *     scalars); and each entry's byte range for the targeted re-read.
 *   resolve: the leaf path is walked on metadata alone; the wire sequence
 *     reproduces projectConversation's pending-assistant flush order; the
 *     turn-event dividers splice in by timestamp exactly as
 *     #mergeTurnEventDividers does; `limit`/`before` select the window; and
 *     the window expands to a contiguous fed range — nearest user-anchor
 *     backwards (parentID chain) then nearest flusher (pending-assistant
 *     state), consecutive non-flushing entries forwards (tool-result
 *     pairing) — so the projected page is identical to the full fold.
 *   pass 2 (re-open): re-parse exactly the fed range's byte records; emit
 *     AgentMessages exactly as buildSessionContext's transcript branch does,
 *     strip dangling tool calls against the path-global pairing set (the
 *     same verdict a full fold gives), project, splice dividers.
 *
 * Anything the windowed fold cannot reproduce returns null — the caller
 * keeps the `withColdManager` full-materialization arm (plan §7.1 labeled
 * fallback): legacy file versions (migrations need the full entry list),
 * emit entries without ids (fed-range selection impossible), `blob:sha256:`
 * references (they resolve through BlobStore at open; the blobs dir is a
 * pi-utils lookup not importable here), snapcompact archives on page
 * compactions (blocks come from @oh-my-pi/snapcompact, likewise not
 * importable), fed ranges beyond MAX_FED_ENTRIES, and unbounded `limit`
 * (no page ⇒ no retained-memory win to pay two scans for). A single JSONL
 * record's in-flight size stays bounded by the record itself — the scanner
 * holds ≤ one line + one chunk (plan §7.2 caveat).
 *
 * Between the two passes the file may gain appends; the page is computed
 * from the pass-1 snapshot and pass 2 selects by byte range + id check, so
 * a concurrent append yields a self-consistent page at the earlier read
 * point — the same contract `SessionManager.open` gives (it reads once). A
 * needed record whose id/type no longer matches means a mid-read rewrite —
 * fallback.
 */

/** Cap on retained turn-event divider entries per scan. */
const MAX_DIVIDER_ENTRIES = 4096;
/** Cap on emit entries loaded with full content around a page. */
const MAX_FED_ENTRIES = 1024;
/** Largest `limit` worth streaming for; larger pages fall back. */
const MAX_PAGE_LIMIT = 512;
/** Cap on retained structured-row entries per scan (getEntries' five kinds). */
const MAX_ROW_ENTRIES = 16384;
/** Single JSONL record byte cap (plan §7.2: maxRecords bounds count, not size). */
const MAX_RECORD_BYTES = 64 * 1024 * 1024;

const SUPERSEDED_COMPACTION_SUMMARY = '[Superseded compaction summary elided after a newer compaction]';
const SUPERSEDED_COMPACTION_SHORT_SUMMARY = 'Superseded compaction elided';

// -- boundary helpers -----------------------------------------------------------

/** The AgentMessage union as session entries carry it (message entries only). */
type AgentMessage = Extract<SessionEntry, { type: 'message' }>['message'];
/** Assistant arm of the union — retryRecovery field type without importing pi-ai. */
type AssistantArm = Extract<AgentMessage, { role: 'assistant' }>;

/**
 * Runtime string probe without `typeof` (anti-slop): returns the string
 * unchanged, undefined for anything else — including typed-but-wrong
 * runtime values in untrusted JSONL. Generic so callers can probe any
 * declared field type; the tag check does the verification.
 */
const stringOf = <T,>(value: T): string | undefined =>
  Object.prototype.toString.call(value) === '[object String]' ? String(value) : undefined;

const textOfContent = (content: ProjectedContentInput | null | undefined): string => {
  const asString = stringOf(content);
  if (asString !== undefined) return asString;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && b.type === 'text')
    .map((b) => b.text)
    .join('');
};

/** projection.ts's private unwrap, mirrored: full-body `<tag>...</tag>` wrappers. */
const unwrapFullBodyXml = (text: string): string => {
  const wrapped = text.match(/^<([a-zA-Z-]+)[^>]*>\s*([\s\S]*?)\s*<\/\1>\s*$/);
  return wrapped ? wrapped[2] : text;
};

const timestampMs = (value: string | number | undefined): number => {
  const asString = stringOf(value);
  if (asString !== undefined) return Date.parse(asString);
  return Number(value); // number input → itself; undefined/object → NaN
};

// -- pass-1 metadata -------------------------------------------------------------

/** Folded model/thinking state at a user message's point in the log. */
interface TurnState {
  model: string | null;
  thinkingLevel: string | null;
}

interface EmitMeta {
  kind: 'message' | 'custom_message' | 'branch_summary' | 'compaction';
  /** AgentMessage role for message entries; undefined for the others. */
  role: string | undefined;
  /** Entry pushes a message in buildSessionContext (drives fileMessageCount). */
  emitsMessage: boolean;
  /** The message yields a wire item in the merged sequence. */
  producesWire: boolean;
  /** Processing it flushes the pending assistant turn. */
  flushes: boolean;
  /** Its wire item parks until the next flusher (assistant pairing). */
  parksAssistant: boolean;
  /** It occupies the user-turn slot (anchors following parentID). */
  anchorsUser: boolean;
  /** Projected wire id; compactions resolve it after the path is known. */
  wireId: string | null;
  /** Numeric ms on the wire item's `time.created`. */
  created: number;
  /** toolResult's toolCallId. */
  resultId: string | undefined;
  /** Assistant retry-recovery flag (getEntries rows). */
  retryRecovery: AssistantArm['retryRecovery'];
  /** Compaction entry's raw summary — needed for the active one's wire id. */
  compactionSummary: string | undefined;
  /** Folded turn state for user messages. */
  turnState: TurnState | undefined;
}

interface EntryMeta {
  id: string | undefined;
  parentId: string | null;
  type: string;
  emit: EmitMeta | undefined;
  /** Byte range [start, end) of this entry's JSONL record, for pass 2 reads. */
  byteStart: number;
  byteEnd: number;
}

interface Scan {
  headerValid: boolean;
  needsMigration: boolean;
  overCap: boolean;
  emitBlobRef: boolean;
  malformed: number;
  metas: EntryMeta[];
  dividerEntries: SessionEntry[];
  rowEntries: SessionEntry[];
  fileVersion: number;
}

const EMIT_TYPES = new Set(['message', 'custom_message', 'branch_summary', 'compaction']);
const ROW_TYPES = new Set(['compaction', 'branch_summary', 'model_change', 'mode_change', 'ttsr_injection']);

/** Custom/hook message wire-id seed: `[omp:<type>] ` + unwrapped text. */
const customSeedOf = (customType: string | undefined, content: ProjectedContentInput): string => {
  const label = customType ? `[omp:${customType}] ` : '[omp] ';
  return label + unwrapFullBodyXml(textOfContent(content));
};

const classifyEmit = (
  entry: SessionEntry,
  fold: TurnState,
  wireIdFor: WireIdResolver | undefined,
  created: number,
): EmitMeta => {
  const emit: EmitMeta = {
    kind: 'message',
    role: undefined,
    emitsMessage: false,
    producesWire: false,
    flushes: false,
    parksAssistant: false,
    anchorsUser: false,
    wireId: null,
    created,
    resultId: undefined,
    retryRecovery: undefined,
    compactionSummary: undefined,
    turnState: undefined,
  };

  if (entry.type === 'message') {
    const message = entry.message;
    emit.emitsMessage = true;
    // Raw message timestamp — the projected item's time.created is exactly
    // this; NaN ordering quirks must match the reference fold verbatim.
    emit.created = timestampMs(message.timestamp);
    emit.role = stringOf(message.role);
    if (message.role === 'user' || message.role === 'assistant') {
      emit.producesWire = true;
      emit.flushes = true;
      emit.parksAssistant = message.role === 'assistant';
      emit.anchorsUser = message.role === 'user';
      emit.wireId = wireIdFor?.(message) ?? deterministicWireId(message);
      if (message.role === 'user') {
        emit.turnState = { model: fold.model, thinkingLevel: fold.thinkingLevel };
      } else {
        emit.retryRecovery = message.retryRecovery;
      }
    } else if (message.role === 'toolResult') {
      emit.resultId = message.toolCallId;
    } else if (message.role === 'custom' || message.role === 'hookMessage') {
      const text = textOfContent(message.content);
      if (message.display !== false && text.trim()) {
        emit.producesWire = true;
        emit.flushes = true;
        emit.wireId = wireMessageId('custom', emit.created, customSeedOf(message.customType, message.content));
      }
    } else if (message.role === 'developer') {
      const text = textOfContent(message.content);
      if (text.trim()) {
        emit.producesWire = true;
        emit.flushes = true;
        emit.anchorsUser = message.attribution === 'user';
        emit.wireId = wireMessageId(emit.anchorsUser ? 'user' : 'custom', emit.created, `[omp:developer] ${text}`);
      }
    } else if (message.role === 'bashExecution' || message.role === 'pythonExecution') {
      emit.producesWire = true;
      const kind = message.role === 'pythonExecution' ? 'python' : 'bash';
      const command = message.role === 'pythonExecution' ? message.code : message.command;
      const output = String(message.output ?? '');
      const cancelled = message.cancelled ? ' (cancelled)' : '';
      const exit = message.exitCode !== undefined ? ` [exit ${message.exitCode}]` : '';
      emit.wireId = wireMessageId('custom', emit.created, `[omp:${kind}] ` + command + output + exit + cancelled);
    } else if (message.role === 'fileMention') {
      emit.producesWire = true;
      const files = Array.isArray(message.files) ? message.files : [];
      const lines = files
        .map((f) => `└ Read ${f.path ?? '(unknown)'}${f.lineCount !== undefined ? ` (${f.lineCount} lines)` : ''}`)
        .join('\n');
      emit.wireId = wireMessageId('custom', emit.created, `[omp:file-mention] ` + lines);
    } else if (message.role === 'compactionSummary' || message.role === 'branchSummary') {
      emit.producesWire = true;
      emit.flushes = true;
      const summary = String(message.summary ?? '');
      const roleTag = message.role === 'branchSummary' ? 'branchSummary' : 'compactionSummary';
      emit.wireId = wireMessageId('custom', emit.created, `[omp:${roleTag}] ` + summary);
    }
    // Other roles emit a message but no wire item and never flush.
    return emit;
  }

  if (entry.type === 'custom_message') {
    emit.kind = 'custom_message';
    if (isCustomMessageContent(entry.content)) {
      emit.emitsMessage = true;
      const normalized = normalizeCustomMessagePayload(entry);
      const text = textOfContent(normalized.content);
      if (normalized.display !== false && text.trim()) {
        emit.producesWire = true;
        emit.flushes = true;
        emit.wireId = wireMessageId('custom', emit.created, `[omp:${normalized.customType}] ` + unwrapFullBodyXml(text));
      }
    }
    return emit;
  }

  if (entry.type === 'branch_summary') {
    emit.kind = 'branch_summary';
    if (entry.summary) {
      emit.emitsMessage = true;
      emit.producesWire = true;
      emit.flushes = true;
      emit.wireId = wireMessageId('custom', emit.created, `[omp:branchSummary] ${entry.summary}`);
    }
    return emit;
  }

  if (entry.type === 'compaction') {
    emit.kind = 'compaction';
    emit.emitsMessage = true;
    emit.producesWire = true;
    emit.flushes = true;
    emit.compactionSummary = entry.summary;
    // wireId assigned post-path: superseded compactions carry the elision
    // summary, so the id depends on which path compaction is latest.
    return emit;
  }
  return emit;
};

/**
 * Stream a JSONL file line-by-line with byte offsets, holding ≤ one line +
 * one chunk. Returns false when the file can't be opened/read. visit(line,
 * byteStart, byteEnd) may return false to stop early. byteEnd includes the
 * newline; a trailing unterminated record is visited without one.
 */
const scanJsonlLines = async (
  filePath: string,
  visit: (line: string, byteStart: number, byteEnd: number) => void | false,
  maxLineBytes?: number,
): Promise<boolean> => {
  const decoder = new TextDecoder();
  let buffer: Buffer<ArrayBufferLike> = Buffer.alloc(0);
  let offset = 0;
  try {
    for await (const chunk of fs.createReadStream(filePath)) {
      // SAFETY: fs.createReadStream without an encoding option yields Buffer
      // chunks by contract.
      const part = chunk as Buffer<ArrayBufferLike>;
      buffer = buffer.length === 0 ? part : Buffer.concat([buffer, part]);
      let search = 0;
      for (;;) {
        const nl = buffer.indexOf(0x0a, search);
        if (nl === -1) {
          // Unterminated runaway line — the remainder buffer itself is the
          // bound plan §7.2 asks for, so abort before it grows further.
          if (maxLineBytes !== undefined && buffer.length - search > maxLineBytes) return false;
          break;
        }
        if (visit(decoder.decode(buffer.subarray(search, nl)), offset + search, offset + nl + 1) === false) {
          return true;
        }
        search = nl + 1;
      }
      buffer = buffer.subarray(search);
      offset += search;
    }
  } catch {
    return false;
  }
  if (buffer.length > 0) visit(decoder.decode(buffer), offset, offset + buffer.length);
  return true;
};

/**
 * Pass 2: reopen the file and re-parse exactly the fed range's byte records.
 * Any range whose parsed id/type no longer matches the pass-1 meta means the
 * file was rewritten mid-read — the caller falls back to the manager arm.
 */
const readEntriesAtRanges = async (
  filePath: string,
  metas: readonly EntryMeta[],
): Promise<Map<string, SessionEntry> | null> => {
  if (metas.length === 0) return new Map();
  let handle: fs.promises.FileHandle;
  try {
    handle = await fs.promises.open(filePath, 'r');
  } catch {
    return null;
  }
  try {
    const retained = new Map<string, SessionEntry>();
    for (const meta of metas) {
      const id = meta.id;
      if (id === undefined) return null;
      const length = meta.byteEnd - meta.byteStart;
      const out = Buffer.alloc(length);
      const { bytesRead } = await handle.read(out, 0, length, meta.byteStart);
      if (bytesRead !== length) return null;
      let entry: SessionEntry;
      try {
        // SAFETY: raw JSONL boundary — identity is re-verified against the
        // pass-1 meta (id + type) immediately below before use.
        entry = JSON.parse(out.toString('utf8').trim()) as SessionEntry;
      } catch {
        return null;
      }
      if (entry.id !== id || entry.type !== meta.type) return null;
      retained.set(id, entry);
    }
    return retained;
  } finally {
    await handle.close();
  }
};

/** Stream pass 1: header + per-entry metadata + byte ranges; retains no content. */
const scanTranscript = async (
  filePath: string,
  wireIdFor: WireIdResolver | undefined,
): Promise<Scan | null> => {
  const scan: Scan = {
    headerValid: false,
    needsMigration: false,
    overCap: false,
    emitBlobRef: false,
    malformed: 0,
    metas: [],
    dividerEntries: [],
    rowEntries: [],
    fileVersion: CURRENT_SESSION_VERSION,
  };
  const fold: TurnState = { model: null, thinkingLevel: null };
  let sawFirstLine = false;
  let sawFirstEntry = false;
  const ok = await scanJsonlLines(filePath, (line, byteStart, byteEnd) => {
    const trimmed = line.trim();
    if (!sawFirstLine) {
      sawFirstLine = true;
      // The optional fixed-width title slot is a physical first line that is
      // not an entry (loader semantics): peel it before parsing; a non-slot
      // first line is a real record left for the parser.
      if (trimmed && parseTitleSlotLine(trimmed)) return;
    }
    if (!trimmed) return;
    if (byteEnd - byteStart > MAX_RECORD_BYTES) {
      // One oversized record → labeled fallback (the scanner's buffer cap
      // aborts earlier when the line has no terminator at all).
      scan.overCap = true;
      return false;
    }
    let entry: FileEntry;
    try {
      // SAFETY: JSON.parse is the raw boundary; fields the scan relies on
      // (type/id/parentId) are re-checked with stringOf below, and every
      // emit/row field read goes through union narrowing after a `type`
      // match — garbage shapes degrade to fallback, never wrong output.
      entry = JSON.parse(trimmed) as FileEntry;
    } catch {
      scan.malformed++;
      return;
    }
    if (!sawFirstEntry) {
      sawFirstEntry = true;
      // loadWithKnownSize yields [] unless the first parsed entry is a valid
      // session header — same gate here, then the scan stops.
      if (!(entry.type === 'session' && stringOf(entry.id) !== undefined)) return false;
      scan.headerValid = true;
      scan.fileVersion = entry.version !== undefined && Number.isFinite(entry.version) ? entry.version : 1;
      if (scan.fileVersion < CURRENT_SESSION_VERSION) {
        scan.needsMigration = true;
        return false;
      }
      return; // the header is not a tree entry (getEntries() drops it)
    }
    // 'session'-typed lines past the header stay in the tree: they never
    // emit or divide, but a later entry may parent onto them — the reference
    // path walk includes them via byId, so metas must too.
    const meta: EntryMeta = {
      id: 'id' in entry ? stringOf(entry.id) : undefined,
      parentId: 'parentId' in entry ? (stringOf(entry.parentId) ?? null) : null,
      type: stringOf(entry.type) ?? '',
      emit: undefined,
      byteStart,
      byteEnd,
    };
    // Raw-line blob precheck: `blob:sha256:` references only ever appear as
    // serialized JSON strings; a substring hit on an emit/row record ⇒ the
    // manager arm's BlobStore resolution is required → labeled fallback.
    if ((EMIT_TYPES.has(meta.type) || ROW_TYPES.has(meta.type)) && trimmed.includes('blob:sha256:')) {
      scan.emitBlobRef = true;
    }
    if (entry.type === 'model_change') {
      const model = stringOf(entry.model);
      if (model !== undefined && model.length > 0) fold.model = model;
    } else if (entry.type === 'thinking_level_change') {
      const level = stringOf(entry.thinkingLevel);
      fold.thinkingLevel = level !== undefined && level.length > 0 ? level : null;
    }
    // Direct discriminated comparisons narrow the union (Set.has cannot).
    if (entry.type === 'model_change' || entry.type === 'mode_change') {
      if (scan.dividerEntries.length >= MAX_DIVIDER_ENTRIES) scan.overCap = true;
      else scan.dividerEntries.push(entry);
    }
    if (
      entry.type === 'compaction' ||
      entry.type === 'branch_summary' ||
      entry.type === 'model_change' ||
      entry.type === 'mode_change' ||
      entry.type === 'ttsr_injection'
    ) {
      if (scan.rowEntries.length >= MAX_ROW_ENTRIES) scan.overCap = true;
      else scan.rowEntries.push(entry);
    }
    if (
      entry.type === 'message' ||
      entry.type === 'custom_message' ||
      entry.type === 'branch_summary' ||
      entry.type === 'compaction'
    ) {
      meta.emit = classifyEmit(entry, fold, wireIdFor, timestampMs(entry.timestamp));
    }
    scan.metas.push(meta);
  }, MAX_RECORD_BYTES);
  if (!ok || !sawFirstEntry) return null; // unreadable / empty / all-malformed
  return scan;
};

// -- path + merged sequence -------------------------------------------------------

interface PathScan {
  /** Emit metas on the leaf→root path, in display order. */
  emitMetas: { meta: EntryMeta; emit: EmitMeta }[];
  /** Every toolCallId a path toolResult answers (strip pairing domain). */
  pathResultIds: Set<string>;
  /** Path emit entries that push a message — context.messages.length parity. */
  fileMessageCount: number;
  /** Entry id of the latest compaction on the path. */
  activeCompactionId: string | undefined;
}

const resolvePath = (scan: Scan): PathScan => {
  const byId = new Map<string, EntryMeta>();
  for (const meta of scan.metas) if (meta.id !== undefined) byId.set(meta.id, meta);
  const leaf = scan.metas[scan.metas.length - 1];
  const pathMetas: EntryMeta[] = [];
  if (leaf) {
    const seen = new Set<string>();
    let cursor: EntryMeta | undefined = leaf;
    while (cursor && (cursor.id === undefined || !seen.has(cursor.id))) {
      if (cursor.id !== undefined) seen.add(cursor.id);
      pathMetas.push(cursor);
      cursor = cursor.parentId !== null ? byId.get(cursor.parentId) : undefined;
    }
    pathMetas.reverse();
  }
  const emitMetas: { meta: EntryMeta; emit: EmitMeta }[] = [];
  const pathResultIds = new Set<string>();
  let activeCompactionId: string | undefined;
  for (const meta of pathMetas) {
    if (meta.type === 'compaction') activeCompactionId = meta.id;
    if (!meta.emit) continue;
    emitMetas.push({ meta, emit: meta.emit });
    if (meta.emit.resultId !== undefined) pathResultIds.add(meta.emit.resultId);
  }
  for (const { meta, emit } of emitMetas) {
    if (emit.kind !== 'compaction' || emit.wireId !== null) continue;
    const isActive = meta.id === activeCompactionId;
    // The pass-1 meta retains the compaction's summary, so the wire id is
    // the real summary seed for the active one and the elision marker for
    // superseded ones — matching projectDividerMessage's `label + summary`.
    const summary = isActive ? String(emit.compactionSummary ?? '') : SUPERSEDED_COMPACTION_SUMMARY;
    emit.wireId = wireMessageId('custom', emit.created, `[omp:compactionSummary] ` + summary);
  }
  const fileMessageCount = emitMetas.filter((e) => e.emit.emitsMessage).length;
  return { emitMetas, pathResultIds, fileMessageCount, activeCompactionId };
};

/** projectConversation wire order over a contiguous emit range. */
const wireOrderOf = (emitMetas: readonly { emit: EmitMeta }[], start: number, end: number): number[] => {
  const order: number[] = [];
  let pending = -1;
  for (let i = start; i < end; i++) {
    const emit = emitMetas[i].emit;
    if (emit.flushes && pending >= 0) {
      order.push(pending);
      pending = -1;
    }
    if (!emit.producesWire) continue;
    if (emit.parksAssistant) pending = i;
    else order.push(i);
  }
  if (pending >= 0) order.push(pending);
  return order;
};

interface MergedItem {
  wireId: string;
  created: number;
  /** Emit-seq index for message items; -1 for divider splices. */
  emitIndex: number;
  divider: SessionEntry | null;
}

const buildMergedSequence = (emitMetas: readonly { emit: EmitMeta }[], dividerEntries: readonly SessionEntry[], sessionID: string): MergedItem[] => {
  const seq: MergedItem[] = wireOrderOf(emitMetas, 0, emitMetas.length).map((emitIndex) => {
    const emit = emitMetas[emitIndex].emit;
    return { wireId: emit.wireId ?? '', created: emit.created, emitIndex, divider: null };
  });
  for (const entry of dividerEntries) {
    const wire = projectTurnEventDivider(entry, { sessionID });
    if (!wire) continue;
    const created = wire.info.time?.created ?? 0;
    const at = seq.findIndex((item) => item.created >= created);
    seq.splice(at === -1 ? seq.length : at, 0, { wireId: wire.info.id, created, emitIndex: -1, divider: entry });
  }
  return seq;
};

// -- pass-2 emit + page assembly ---------------------------------------------------

const emitAgentMessage = (
  entry: SessionEntry,
  activeCompactionId: string | undefined,
): AgentMessage | 'needsManager' | null => {
  if (entry.type === 'message') return entry.message;
  if (entry.type === 'custom_message') {
    if (!isCustomMessageContent(entry.content)) return null;
    const normalized = normalizeCustomMessagePayload(entry);
    const attribution = entry.attribution === undefined ? undefined : normalized.attribution;
    return createCustomMessage(
      normalized.customType,
      normalized.content,
      normalized.display,
      normalized.details,
      entry.timestamp,
      attribution,
    );
  }
  if (entry.type === 'branch_summary') {
    if (!entry.summary) return null;
    return createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp);
  }
  if (entry.type === 'compaction') {
    if (entry.preserveData !== undefined) return 'needsManager';
    const active = entry.id === activeCompactionId;
    return createCompactionSummaryMessage(
      active ? entry.summary : SUPERSEDED_COMPACTION_SUMMARY,
      entry.tokensBefore,
      entry.timestamp,
      // warning/method/tokensAfter ride both active and superseded (reference
      // conditions only shortSummary + blocks on `active`).
      {
        shortSummary: active ? entry.shortSummary : SUPERSEDED_COMPACTION_SHORT_SUMMARY,
        warning: entry.warning,
        method: entry.method,
        tokensAfter: entry.tokensAfter,
      },
    );
  }
  return null;
};

/**
 * Transcript-mode dangling-tool-call strip (buildSessionContext's rewrite):
 * calls without a paired toolResult on the path are removed, redactedThinking
 * blocks drop, thinking signatures clear, and the turn is marked
 * strippedToolCalls instead of vanishing.
 */
type AssistantContentBlock = AssistantArm['content'][number];

const stripDanglingToolCalls = (messages: AgentMessage[], paired: ReadonlySet<string>): void => {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (message.role !== 'assistant') continue;
    const content = message.content;
    const isDanglingCall = (block: AssistantContentBlock): boolean =>
      block.type === 'toolCall' && !paired.has(block.id);
    let stripped = 0;
    for (const block of content) if (isDanglingCall(block)) stripped++;
    if (stripped === 0) continue;
    const normalized = content
      .filter((block) => !isDanglingCall(block) && block.type !== 'redactedThinking')
      .map((block) =>
        block.type === 'thinking' && block.thinkingSignature
          ? { ...block, thinkingSignature: undefined }
          : block,
      );
    // SAFETY: the rewrite preserves every assistant field; the marker field
    // mirrors the reference's `AgentMessage & StrippedToolCallsMarker` stamp.
    messages[i] = { ...message, content: normalized, strippedToolCalls: stripped } as AgentMessage;
  }
};

export interface TranscriptPageRequest {
  sessionID: string;
  directory?: string;
  agent?: string;
  wireIdFor?: WireIdResolver;
  limit?: number;
  before?: string;
}

export interface TranscriptPageResult {
  /** Parity with buildSessionContext({transcript:true}).messages.length. */
  fileMessageCount: number;
  /** Turn-event divider entries (the live-arm merge needs the same set). */
  dividerEntries: SessionEntry[];
  /** The paginated, divider-merged file arm — already windowed. */
  page: ProjectedMessagePage;
}

/**
 * Stream `filePath` and return the requested page with retained memory
 * bounded to the page window. Returns null for every labeled fallback; the
 * caller then keeps the withColdManager full-materialization arm.
 */
export const readTranscriptMessagePage = async (
  filePath: string,
  request: TranscriptPageRequest,
): Promise<TranscriptPageResult | null> => {
  const { limit, before, sessionID } = request;
  if (limit === undefined || !Number.isFinite(limit) || limit <= 0 || Math.floor(limit) > MAX_PAGE_LIMIT) {
    return null;
  }
  const pageLimit = Math.floor(limit);

  // Plan §7.2 signature: a size/mtime change across the two passes means an
  // external rewrite may have moved earlier records the meta pass already
  // folded into path/pagination — drop the result, fall back (the manager
  // arm is effectively the bounded retry the plan allows).
  let startSig: FileSignature;
  try {
    const stat = await fs.promises.stat(filePath);
    startSig = { size: stat.size, mtimeMs: stat.mtimeMs };
  } catch {
    return null;
  }

  const scan = await scanTranscript(filePath, request.wireIdFor);
  if (!scan || !scan.headerValid || scan.needsMigration || scan.overCap || scan.emitBlobRef) return null;
  const pathScan = resolvePath(scan);
  // Active-compaction wire ids need the summary text — a page containing one
  // means a compaction sits inside the window; resolve lazily in pass 2.
  const merged = buildMergedSequence(pathScan.emitMetas, scan.dividerEntries, sessionID);

  // paginateProjectedMessages over the merged sequence.
  let windowEnd = merged.length;
  if (before) {
    const boundary = merged.findIndex((item) => item.wireId === before);
    if (boundary === -1) {
      return { fileMessageCount: pathScan.fileMessageCount, dividerEntries: scan.dividerEntries, page: { messages: [], cursor: undefined } };
    }
    windowEnd = boundary;
  }
  const pageStart = Math.max(0, windowEnd - pageLimit);
  const pageItems = merged.slice(pageStart, windowEnd);
  const cursor = windowEnd > pageLimit ? pageItems[0]?.wireId : undefined;

  // Fed range: the emit entries whose content the page's projection needs —
  // nearest user anchor backwards (parentID), else nearest flusher (pending
  // assistant), else the first page emit; consecutive non-flushers forwards
  // (toolResult pairing for the page's trailing assistant turn).
  const emitMetas = pathScan.emitMetas;
  const pageEmitIndexes = pageItems.filter((i) => i.emitIndex >= 0).map((i) => i.emitIndex);
  const firstPageEmit = pageEmitIndexes[0] ?? emitMetas.length;
  let fedStart = firstPageEmit;
  for (let i = firstPageEmit - 1; i >= 0; i--) {
    if (emitMetas[i].emit.anchorsUser) { fedStart = i; break; }
    if (emitMetas[i].emit.flushes && fedStart === firstPageEmit) fedStart = i;
  }
  let fedEnd = firstPageEmit === emitMetas.length ? firstPageEmit : (pageEmitIndexes[pageEmitIndexes.length - 1] ?? firstPageEmit) + 1;
  while (fedEnd < emitMetas.length && !emitMetas[fedEnd].emit.flushes) fedEnd++;
  if (pageEmitIndexes.length === 0) { fedStart = 0; fedEnd = 0; }
  if (fedEnd - fedStart > MAX_FED_ENTRIES) return null;

  const neededMetas: EntryMeta[] = [];
  for (let i = fedStart; i < fedEnd; i++) {
    const meta = emitMetas[i].meta;
    if (meta.id === undefined) return null;
    neededMetas.push(meta);
  }

  const retained = await readEntriesAtRanges(filePath, neededMetas);
  if (!retained) return null;
  try {
    const endStat = await fs.promises.stat(filePath);
    if (endStat.size !== startSig.size || endStat.mtimeMs !== startSig.mtimeMs) return null;
  } catch {
    return null;
  }

  const fedMessages: AgentMessage[] = [];
  for (let i = fedStart; i < fedEnd; i++) {
    const meta = emitMetas[i].meta;
    const entry = meta.id !== undefined ? retained.get(meta.id) : undefined;
    if (!entry) return null;
    const emitted = emitAgentMessage(entry, pathScan.activeCompactionId);
    if (emitted === 'needsManager') return null;
    if (emitted === null) continue;
    fedMessages.push(emitted);
  }
  stripDanglingToolCalls(fedMessages, pathScan.pathResultIds);

  const turnStateByWireId = new Map<string, TurnState>();
  for (const { emit } of emitMetas) {
    if (emit.turnState && emit.wireId) turnStateByWireId.set(emit.wireId, emit.turnState);
  }
  const turnStateFor = (message: WireIdMessageInput | null | undefined) => {
    if (message?.role !== 'user') return null;
    const state = turnStateByWireId.get(request.wireIdFor?.(message) ?? deterministicWireId(message));
    if (!state) return null;
    return { model: state.model ?? undefined, thinkingLevel: state.thinkingLevel ?? undefined };
  };

  const projected = projectConversation(fedMessages, {
    sessionID,
    directory: request.directory,
    agent: request.agent,
    wireIdFor: request.wireIdFor,
    turnStateFor,
  } satisfies ConversationProjectionOptions);

  // Fed-range wire order maps emit indexes → projected output ranks.
  const fedWireOrder = wireOrderOf(emitMetas, fedStart, fedEnd);
  if (fedWireOrder.length !== projected.length) return null;
  const rankByEmitIndex = new Map<number, number>();
  fedWireOrder.forEach((emitIndex, rank) => rankByEmitIndex.set(emitIndex, rank));

  const messages: ProjectedMessage[] = [];
  for (const item of pageItems) {
    if (item.divider) {
      const wire = projectTurnEventDivider(item.divider, { sessionID });
      if (wire) messages.push(wire);
      continue;
    }
    const rank = rankByEmitIndex.get(item.emitIndex);
    if (rank === undefined || !projected[rank]) return null;
    messages.push(projected[rank]);
  }
  return {
    fileMessageCount: pathScan.fileMessageCount,
    dividerEntries: scan.dividerEntries,
    page: { messages, cursor },
  };
};

// -- scalar + row readers -----------------------------------------------------------

/** directoryExists parity: any stat failure (ENOENT, non-dir) → false. */
const directoryUsable = async (dir: string): Promise<boolean> => {
  try {
    return (await fs.promises.stat(dir)).isDirectory();
  } catch {
    return false;
  }
};

/** File signature sampled across the two passes (plan §7.2 rewrite tripwire). */
interface FileSignature {
  size: number;
  mtimeMs: number;
}

interface ScalarScanState {
  header: SessionHeader | null;
}

interface TitleSlotScan {
  seen: boolean;
  title: string | undefined;
}

export interface SessionScalars {
  id: string;
  title: string | undefined;
  /** Recorded header cwd when it still resolves to a directory; else null. */
  cwd: string | null;
  createdIso: string | undefined;
  /** Last non-header record's timestamp ISO; undefined when none. */
  modifiedIso: string | undefined;
}

/**
 * Stream the session header + last-record timestamp without opening a
 * manager. Returns null when the file is unreadable or has no valid session
 * header — the caller's manager path then reproduces the existing behavior
 * (`SessionManager.open` mints a fresh id and rewrites invalid files).
 * Header scalars are unaffected by entry migrations, so legacy versions
 * stream fine.
 */
export const readSessionScalars = async (filePath: string): Promise<SessionScalars | null> => {
  // Boxed named contracts: the visitor closure's assignment would otherwise
  // narrow the bindings to never.
  const state: ScalarScanState = { header: null };
  const slotState: TitleSlotScan = { seen: false, title: undefined };
  let lastIso: string | undefined;
  let sawFirstLine = false;
  const ok = await scanJsonlLines(filePath, (line) => {
    const trimmed = line.trim();
    if (!sawFirstLine) {
      sawFirstLine = true;
      // The physical first line may be the fixed-width title slot — peel it;
      // its title overrides the header's (loader applyTitleSlot semantics).
      if (trimmed) {
        const slot = parseTitleSlotLine(trimmed);
        if (slot) {
          slotState.seen = true;
          slotState.title = slot.title;
          return;
        }
      }
    }
    if (!trimmed) return;
    let entry: FileEntry;
    try {
      // SAFETY: raw JSONL boundary — the header gate re-checks type/id, and
      // every field read below goes through stringOf (runtime-verified).
      entry = JSON.parse(trimmed) as FileEntry;
    } catch {
      return; // malformed record — loader skips it too
    }
    if (!state.header) {
      if (!(entry.type === 'session' && stringOf(entry.id) !== undefined)) return false;
      state.header = entry;
      return;
    }
    // Every later record — including a stray second 'session' line — counts
    // for `modified` (the manager's getEntries() drops only the header).
    const ts = 'timestamp' in entry ? stringOf(entry.timestamp) : undefined;
    if (ts !== undefined) lastIso = ts;
  });
  const header = state.header;
  if (!ok || !header) return null;
  const recorded = stringOf(header.cwd);
  const usable = recorded !== undefined && recorded.length > 0 && (await directoryUsable(recorded));
  return {
    id: header.id,
    title: slotState.seen ? (slotState.title && slotState.title.length > 0 ? slotState.title : undefined) : header.title,
    cwd: usable ? (recorded ?? null) : null,
    createdIso: stringOf(header.timestamp),
    modifiedIso: lastIso,
  };
};

/**
 * One getEntries row — the wire shape the manager arm builds per entry
 * kind. Fields are per-kind optional; absent fields serialize as omitted.
 */
export interface SessionEventRow {
  kind: string;
  id?: string;
  timestamp?: number;
  summary?: string;
  tokensBefore?: number;
  warning?: string;
  fromId?: string;
  model?: string;
  role?: string;
  mode?: string;
  data?: object;
  rules?: string[];
  messageID?: string;
  retryRecovery?: AssistantArm['retryRecovery'];
}

/**
 * getEntries' cold arm: the five structured kinds stream from retained
 * row entries; `retry_recovery` rows come from the path's assistant emit
 * metas (same wireIdFor resolution the manager arm applies). Returns null
 * on the labeled fallbacks (legacy versions need entry-id migration; blob
 * refs resolve through BlobStore).
 */
export const readSessionEventRows = async (
  filePath: string,
  wanted: ReadonlySet<string>,
  wireIdFor: WireIdResolver | undefined,
): Promise<SessionEventRow[] | null> => {
  const scan = await scanTranscript(filePath, wireIdFor);
  if (!scan || !scan.headerValid || scan.needsMigration || scan.overCap || scan.emitBlobRef) return null;
  const rows: SessionEventRow[] = [];
  for (const entry of scan.rowEntries) {
    if (wanted.size > 0 && !wanted.has(entry.type)) continue;
    const row: SessionEventRow = {
      kind: entry.type,
      id: entry.id,
      timestamp: Date.parse(entry.timestamp ?? '') || undefined,
    };
    if (entry.type === 'compaction') {
      row.summary = entry.summary;
      row.tokensBefore = entry.tokensBefore;
      if (entry.warning) row.warning = entry.warning;
    } else if (entry.type === 'branch_summary') {
      row.fromId = entry.fromId;
      row.summary = entry.summary;
    } else if (entry.type === 'model_change') {
      row.model = entry.model;
      if (entry.role) row.role = entry.role;
    } else if (entry.type === 'mode_change') {
      row.mode = entry.mode;
      if (entry.data) row.data = entry.data;
    } else if (entry.type === 'ttsr_injection') {
      row.rules = entry.injectedRules;
    }
    rows.push(row);
  }
  if (wanted.size === 0 || wanted.has('retry_recovery')) {
    const pathScan = resolvePath(scan);
    for (const { emit } of pathScan.emitMetas) {
      if (emit.role !== 'assistant' || !emit.retryRecovery) continue;
      rows.push({
        kind: 'retry_recovery',
        messageID: emit.wireId ?? undefined,
        timestamp: emit.created,
        retryRecovery: emit.retryRecovery,
      });
    }
  }
  return rows;
};
