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
  executionWireId,
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
 * Windowed cold transcript reader (docs/plans/omp-host-memory/plan.md §7.2).
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
 * Between the two passes the file may gain appends or be rewritten; the
 * pass-2 reads re-verify each needed record's id+type at its pass-1 byte
 * range, and a final size/mtime comparison rejects ANY mid-read change —
 * including pure appends — so an actively-appended transcript falls back
 * to the manager arm rather than serving a page folded from stale metas.
 * That makes the streamed arm a cold-read optimization only: hot files
 * pay one scan then take the labeled fallback.
 */

/**
 * 模块：冷转录文件的窗口化分页读取器（docs/plans/omp-host-memory/plan.md §7.2）。
 *
 * 背景：冷路径 getMessagesPage 原先经 SessionManager.open 打开整份 JSONL——解析
 * 全部条目、投影全部消息——最后只切出一页。本读取器把驻留内存约束到请求页：
 *
 *   pass 1（流式）：只留每条目的元数据——leaf→root 回溯用的 id/parentId/type/
 *     timestamp；emit 类别、门控标志、wire id 与 flush/anchor 标志（合并展示
 *     序列要用）；全局 toolCall/toolResult 配对 id；按 user 消息折叠的
 *     model/thinking 状态；整条保留的 turn-event 分隔条目与结构化行条目（小
 *     标量）；以及每条目的字节区间（供第二轮定点重读）。
 *   resolve：仅在元数据上回溯 leaf 路径；wire 序列复刻 projectConversation 的
 *     pending-assistant 冲刷顺序；turn-event 分隔条按时间戳拼接（与
 *     #mergeTurnEventDividers 完全一致）；limit/before 选出窗口；窗口扩展为
 *     连续的 fed 区间——向后找最近的 user 锚点（parentID 链）否则最近的
 *     flusher（pending assistant），向前吞并连续的非 flush 条目（tool-result
 *     配对）——使投影结果与全量折叠逐字节一致。
 *   pass 2（重开文件）：只重解析 fed 区间的字节记录；按 buildSessionContext
 *     转录分支的方式产出 AgentMessage，再对照全路径配对集剥离悬空 tool call
 *     （与全量折叠同一判定），投影、拼接分隔条。
 *
 * 窗口化折叠无法复现的一切情况都返回 null——调用方保留 withColdManager 的
 * 全量物化臂（plan §7.1 的带标签回退）：旧版本文件（迁移需要完整条目表）、无
 * id 的 emit 条目（无法选定 fed 区间）、blob:sha256: 引用（要在 open 时经
 * BlobStore 解析，pi-utils 的 blobs 目录不可在此 import）、页压实上的
 * snapcompact 归档（来自 @oh-my-pi/snapcompact，同样不可 import）、fed 区间
 * 超过 MAX_FED_ENTRIES、以及无界 limit（没有分页就没有值得两轮扫描的内存收益）。
 * 单条 JSONL 记录的在途大小以记录本身为界——扫描器至多持有一行 + 一个
 * chunk（plan §7.2 附注）。
 *
 * 两轮之间文件可能被追加或重写；pass-2 读取会在 pass-1 的字节区间上复核每个
 * 所需记录的 id+type，最终再以 size/mtime 比对拒绝任何中途变化——包括纯追
 * 加——因此活跃追加中的转录文件会走 manager 臂，而不是用陈旧元数据折页。这使
 * 流式臂只是冷读优化：热文件付出一次扫描后即取带标签回退。
 */
/** Cap on retained turn-event divider entries per scan. */
/** 单次扫描保留的 turn-event 分隔条目数上限。 */
const MAX_DIVIDER_ENTRIES = 4096;
/** Cap on emit entries loaded with full content around a page. */
/** 页窗口前后以完整内容载入的 emit 条目数上限。 */
const MAX_FED_ENTRIES = 1024;
/** Largest `limit` worth streaming for; larger pages fall back. */
/** 值得流式服务的最大 limit；更大的页直接回退。 */
const MAX_PAGE_LIMIT = 512;
/** Cap on retained structured-row entries per scan (getEntries' five kinds). */
/** 单次扫描保留的结构化行条目数上限（getEntries 的五种 kind）。 */
const MAX_ROW_ENTRIES = 16384;
/** Single JSONL record byte cap (plan §7.2: maxRecords bounds count, not size). */
/** 单条 JSONL 记录的字节上限（plan §7.2：maxRecords 只限条数不限大小）。 */
const MAX_RECORD_BYTES = 64 * 1024 * 1024;

/** 被更新压实取代的旧压实摘要——在线显示用的省略占位文本。 */
const SUPERSEDED_COMPACTION_SUMMARY = '[Superseded compaction summary elided after a newer compaction]';
/** 同上的短摘要占位文本。 */
const SUPERSEDED_COMPACTION_SHORT_SUMMARY = 'Superseded compaction elided';

// -- boundary helpers -----------------------------------------------------------

/** The AgentMessage union as session entries carry it (message entries only). */
/** 会话条目中的 AgentMessage 联合类型（仅 message 条目携带）。 */
type AgentMessage = Extract<SessionEntry, { type: 'message' }>['message'];
/** Assistant arm of the union — retryRecovery field type without importing pi-ai. */
/** 联合中的 assistant 臂——为 retryRecovery 字段类型而不必 import pi-ai。 */
type AssistantArm = Extract<AgentMessage, { role: 'assistant' }>;

/**
 * Runtime string probe without `typeof` (anti-slop): returns the string
 * unchanged, undefined for anything else — including typed-but-wrong
 * runtime values in untrusted JSONL. Generic so callers can probe any
 * declared field type; the tag check does the verification.
 */
/** 不用 typeof 的运行时字符串探针：字符串原样返回，其余（含声明类型与运行时不符的值）返回 undefined。 */
const stringOf = <T,>(value: T): string | undefined =>
  Object.prototype.toString.call(value) === '[object String]' ? String(value) : undefined;

/** 取消息内容的纯文本：字符串直接用，块数组则拼接全部 text 块。 */
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
/** 镜像 projection.ts 的私有解包：剥掉整段包裹的 <tag>...</tag> 外壳。 */
const unwrapFullBodyXml = (text: string): string => {
  const wrapped = text.match(/^<([a-zA-Z-]+)[^>]*>\s*([\s\S]*?)\s*<\/\1>\s*$/);
  return wrapped ? wrapped[2] : text;
};

/** 时间戳归一为毫秒数值：字符串走 Date.parse；数值原样；undefined/对象得 NaN。 */
const timestampMs = (value: string | number | undefined): number => {
  const asString = stringOf(value);
  if (asString !== undefined) return Date.parse(asString);
  return Number(value); // number input → itself; undefined/object → NaN
};

// -- pass-1 metadata -------------------------------------------------------------

/** Folded model/thinking state at a user message's point in the log. */
/** 日志行到某条 user 消息处折叠出的 model/thinking 状态。 */
interface TurnState {
  /** 最近一次 model_change 折叠到的模型 id；无则为 null。 */
  model: string | null;
  /** 最近一次 thinking_level_change 折叠到的档位；置空则为 null。 */
  thinkingLevel: string | null;
}

/** emit 条目的 pass-1 元数据：判定该条目在折叠/合并序列中的全部行为标志。 */
interface EmitMeta {
  /** emit 类别（对应四种会产生消息/展示项的条目类型）。 */
  kind: 'message' | 'custom_message' | 'branch_summary' | 'compaction';
  /** AgentMessage role for message entries; undefined for the others. */
  /** message 条目的 AgentMessage role；其余类别为 undefined。 */
  role: string | undefined;
  /** Entry pushes a message in buildSessionContext (drives fileMessageCount). */
  /** 该条目在 buildSessionContext 中压入一条消息（驱动 fileMessageCount）。 */
  emitsMessage: boolean;
  /** The message yields a wire item in the merged sequence. */
  /** 该消息在合并序列中产出一个 wire 条目。 */
  producesWire: boolean;
  /** Processing it flushes the pending assistant turn. */
  /** 处理它会冲刷 pending 的 assistant 轮次。 */
  flushes: boolean;
  /** Its wire item parks until the next flusher (assistant pairing). */
  /** 其 wire 条目停靠到下一个 flusher 为止（assistant 配对）。 */
  parksAssistant: boolean;
  /** It occupies the user-turn slot (anchors following parentID). */
  /** 它占据 user 轮次槽位（为后续条目锚定 parentID）。 */
  anchorsUser: boolean;
  /** Projected wire id; compactions resolve it after the path is known. */
  /** 投影 wire id；compaction 类在路径确定后再解析。 */
  wireId: string | null;
  /** Numeric ms on the wire item's `time.created`. */
  /** wire 条目 time.created 上的毫秒数值。 */
  created: number;
  /** toolResult's toolCallId. */
  /** toolResult 的 toolCallId。 */
  resultId: string | undefined;
  /** Assistant retry-recovery flag (getEntries rows). */
  /** assistant 的 retry-recovery 标志（getEntries 行用）。 */
  retryRecovery: AssistantArm['retryRecovery'];
  /** Compaction entry's raw summary — needed for the active one's wire id. */
  /** compaction 条目的原始 summary——活动压实的 wire id 需要它。 */
  compactionSummary: string | undefined;
  /** Folded turn state for user messages. */
  /** user 消息折叠出的轮次状态。 */
  turnState: TurnState | undefined;
}

/** 单条条目的 pass-1 元数据：树回溯字段 + 字节区间，不含任何内容。 */
interface EntryMeta {
  /** 条目 id；无 id 的条目无法参与 fed 区间选取（回退路径）。 */
  id: string | undefined;
  /** 父条目 id；根/无父为 null。 */
  parentId: string | null;
  /** 条目类型字符串（原文照录，用于判别与 pass-2 复核）。 */
  type: string;
  /** emit 类条目的行为元数据；非 emit 条目为 undefined。 */
  emit: EmitMeta | undefined;
  /** Byte range [start, end) of this entry's JSONL record, for pass 2 reads. */
  /** 本条目 JSONL 记录的字节区间，供 pass 2 定点重读。 */
  byteStart: number;
  /** 字节区间终点（不含）。 */
  byteEnd: number;
}

/** 一次完整 pass-1 扫描的结果：元数据、保留条目与全部回退标志。 */
interface Scan {
  /** 首个解析条目是否为合法 session 头。 */
  headerValid: boolean;
  /** 文件版本低于 CURRENT_SESSION_VERSION，需要全量迁移（回退）。 */
  needsMigration: boolean;
  /** 某类保留条目超过上限或单条记录超限（回退）。 */
  overCap: boolean;
  /** emit/row 条目中发现 blob:sha256: 引用（回退，需 BlobStore）。 */
  emitBlobRef: boolean;
  /** 解析失败的行数（loader 同样跳过，仅计数不回退）。 */
  malformed: number;
  /** 全部条目（含非 emit）的元数据，按文件顺序。 */
  metas: EntryMeta[];
  /** 整条保留的 turn-event 分隔条目（model_change / mode_change）。 */
  dividerEntries: SessionEntry[];
  /** 整条保留的结构化行条目（getEntries 的五种 kind）。 */
  rowEntries: SessionEntry[];
  /** 头部声明的文件版本号（缺省按 1）。 */
  fileVersion: number;
}

/** 四种 emit 条目类型集合（会产生消息/展示项的条目）。 */
const EMIT_TYPES = new Set(['message', 'custom_message', 'branch_summary', 'compaction']);
/** getEntries 五种结构化行条目类型集合。 */
const ROW_TYPES = new Set(['compaction', 'branch_summary', 'model_change', 'mode_change', 'ttsr_injection']);

/** Custom/hook message wire-id seed: `[omp:<type>] ` + unwrapped text. */
/** custom/hook 消息的 wire-id 种子：`[omp:<type>] ` + 解包后的文本。 */
const customSeedOf = (customType: string | undefined, content: ProjectedContentInput): string => {
  const label = customType ? `[omp:${customType}] ` : '[omp] ';
  return label + unwrapFullBodyXml(textOfContent(content));
};

/**
 * 把一条 emit 类条目分类为 EmitMeta：按条目类型与消息 role 推导
 * producesWire/flushes/parksAssistant/anchorsUser 等标志并解析 wire id，
 * 各分支须与 projectConversation 的折叠语义逐字段一致（含 display 空文本、
 * developer 归因、fileMention 行文案等细节）。compaction 的 wire id 延后到
 * 路径已知时再定（活动/被取代两种 summary）。
 */
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
      // Same fold as projectTranscript: the row lands after the turn it
      // follows (flush the parked assistant first), and the live
      // dispatcher's echo id wins when one was registered (executeBash).
      emit.flushes = true;
      emit.wireId = wireIdFor?.(message) ?? executionWireId(message);
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
/** 按行流式扫描 JSONL 并给出字节偏移，全程至多持有一行 + 一个 chunk（内存有界的核心）。 */
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
/** pass 2：重开文件，只重解析 fed 区间的字节记录并按 id 建索引；任一记录 id/type 复核失败即返回 null（文件中途被改写）。 */
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
/**
 * 流式 pass 1：剥掉可选标题槽首行，校验 session 头（非法头立即终止并回退），
 * 之后逐行折叠 model/thinking 状态、保留分隔条目与结构化行条目、分类 emit 元
 * 数据并记录字节区间——全程不保留任何消息内容。返回 null 表示文件不可读或
 * 首行即全部损坏。
 */
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

/** 仅凭元数据完成的 leaf→root 路径解析结果。 */
interface PathScan {
  /** leaf→root 路径上的 emit 元数据，已反转为展示顺序。 */
  /** Emit metas on the leaf→root path, in display order. */
  emitMetas: { meta: EntryMeta; emit: EmitMeta }[];
  /** 路径上 toolResult 应答的全部 toolCallId（悬空剥离的配对域）。 */
  /** Every toolCallId a path toolResult answers (strip pairing domain). */
  pathResultIds: Set<string>;
  /** 路径上会压入消息的 emit 条目数——与 context.messages.length 对齐。 */
  /** Path emit entries that push a message — context.messages.length parity. */
  fileMessageCount: number;
  /** 路径上最新一条 compaction 的条目 id。 */
  /** Entry id of the latest compaction on the path. */
  activeCompactionId: string | undefined;
}

/**
 * 从最后一条条目沿 parentId 回溯到根（防环），得到路径元数据；
 * 收集 emit 序列、toolResult 配对 id，并在路径已知后为 compaction 补齐
 * wire id（活动压实用真实 summary，被取代的用省略占位）。
 */
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
/** 在连续 emit 区间上复刻 projectConversation 的 wire 顺序（pending assistant 停靠与冲刷）。 */
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

/** 合并序列条目：wire 消息项或分隔条拼接项。 */
interface MergedItem {
  /** 投影 wire id。 */
  wireId: string;
  /** wire 条目的 time.created 毫秒值（分隔条按时间戳插入的依据）。 */
  created: number;
  /** Emit-seq index for message items; -1 for divider splices. */
  emitIndex: number;
  /** 分隔条目本体；消息项为 null。 */
  divider: SessionEntry | null;
}

/** 先取全路径 wire 顺序，再把 turn-event 分隔条按时间戳二分插入——与 #mergeTurnEventDividers 相同的合并规则。 */
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

/**
 * pass 2：把一条完整条目转成 AgentMessage，语义对齐 buildSessionContext 的
 * 转录分支。message 原样返回；custom_message/branch_summary/compaction 经
 * 对应的 create* 工厂重建；compaction.preserveData 存在时返回 'needsManager'
 * （snapcompact 数据需要 manager 臂）；无 summary 等空内容返回 null（跳过）。
 * compaction 按活动/被取代选择真实或省略 summary。
 */
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
/** assistant 内容块类型（联合的单臂成员，供悬空剥离的块级判定）。 */
type AssistantContentBlock = AssistantArm['content'][number];

/**
 * 转录模式的悬空 tool-call 剥离（buildSessionContext 的改写）：路径上没有
 * 配对 toolResult 的 toolCall 块被移除，redactedThinking 块丢弃，thinking
 * 签名清空，轮次打上 strippedToolCalls 计数标记而不是整条消失。就地改写
 * messages 数组。
 */
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

/** getMessagesPage 冷路径的窗口化读取请求。 */
export interface TranscriptPageRequest {
  /** 会话 id（投影 wire id 与分隔条需要）。 */
  sessionID: string;
  /** 会话目录（透传 projectConversation）。 */
  directory?: string;
  /** agent 名（透传 projectConversation）。 */
  agent?: string;
  /** wire id 解析器注入（活动 dispatcher 的 echo id 优先）；缺省用确定性 id。 */
  wireIdFor?: WireIdResolver;
  /** 页大小；非法或超过 MAX_PAGE_LIMIT 时回退 manager 臂。 */
  limit?: number;
  /** 游标：返回该 wire id 之前的页；找不到时返回空页。 */
  before?: string;
}

/** readTranscriptMessagePage 的返回：窗口化的页 + 与全量折叠对齐的辅助数据。 */
export interface TranscriptPageResult {
  /** Parity with buildSessionContext({transcript:true}).messages.length. */
  /** 与 buildSessionContext({transcript:true}).messages.length 对齐的计数。 */
  fileMessageCount: number;
  /** Turn-event divider entries (the live-arm merge needs the same set). */
  /** turn-event 分隔条目全集（live 臂合并需要同一集合）。 */
  dividerEntries: SessionEntry[];
  /** The paginated, divider-merged file arm — already windowed. */
  /** 已分页、已合并分隔条的文件臂结果——窗口化已完成。 */
  page: ProjectedMessagePage;
}

/**
 * Stream `filePath` and return the requested page with retained memory
 * bounded to the page window. Returns null for every labeled fallback; the
 * caller then keeps the withColdManager full-materialization arm.
 */
/**
 * 流式读取 `filePath` 并返回请求的消息页，驻留内存约束在页窗口内。
 * 全部带标签回退（旧版本/无 id emit/blob 引用/超限/中途改写/投影对不上）
 * 一律返回 null，由调用方保留 withColdManager 全量物化臂。
 * before 游标不存在时按约定返回空页（cursor 为 undefined）而非回退。
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
  // 轮次状态查询：按 user 消息的 wire id 反查 pass-1 折叠的 model/thinking 档位。
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
/** 与 directoryExists 同义：任何 stat 失败（ENOENT、非目录）都视为不可用。 */
const directoryUsable = async (dir: string): Promise<boolean> => {
  try {
    return (await fs.promises.stat(dir)).isDirectory();
  } catch {
    return false;
  }
};

/** File signature sampled across the two passes (plan §7.2 rewrite tripwire). */
/** 两轮扫描间采样的文件签名（plan §7.2 的改写绊网）。 */
interface FileSignature {
  /** 文件字节数。 */
  size: number;
  /** mtime 毫秒值。 */
  mtimeMs: number;
}

/** 标量扫描的可变状态盒（避免闭包赋值把绑定收窄为 never）。 */
interface ScalarScanState {
  /** 已解析的 session 头；未见到头时为 null。 */
  header: SessionHeader | null;
}

/** 标题槽扫描状态盒。 */
interface TitleSlotScan {
  /** 首行是否为固定宽度标题槽。 */
  seen: boolean;
  /** 标题槽携带的标题文本（可为空串）。 */
  title: string | undefined;
}

/** 会话列表所需的标量摘要（不打开 manager 即可取得）。 */
export interface SessionScalars {
  /** 头部声明的会话 id。 */
  id: string;
  /** 标题：标题槽覆盖头部的 title；均无则为 undefined。 */
  title: string | undefined;
  /** Recorded header cwd when it still resolves to a directory; else null. */
  /** 头部记录的 cwd 且当前仍是目录；否则 null。 */
  cwd: string | null;
  /** 头部时间戳（ISO 字符串）。 */
  createdIso: string | undefined;
  /** Last non-header record's timestamp ISO; undefined when none. */
  /** 最后一条非头记录的时间戳（ISO）；无记录时为 undefined。 */
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
/**
 * 流式读取 session 头 + 最后一条记录的时间戳，不打开 manager。
 * 文件不可读或没有合法 session 头时返回 null——调用方的 manager 路径随后
 * 复现既有行为（SessionManager.open 会另铸 id 并重写坏文件）。
 * 头部标量不受条目迁移影响，旧版本文件也能流式读取。
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
/** getEntries 的一行——manager 臂按条目 kind 构建的同一 wire 形态；字段按 kind 可选，缺省序列化时省略。 */
export interface SessionEventRow {
  /** 行类别（五种结构化 kind 或 retry_recovery）。 */
  kind: string;
  /** 条目 id。 */
  id?: string;
  /** 时间戳（毫秒）。 */
  timestamp?: number;
  /** 压实/分支摘要文本。 */
  summary?: string;
  /** 压实前 token 数。 */
  tokensBefore?: number;
  /** 压实警告文本。 */
  warning?: string;
  /** 分支摘要的起点条目 id。 */
  fromId?: string;
  /** model_change 后的模型 id。 */
  model?: string;
  /** model_change 的触发角色。 */
  role?: string;
  /** mode_change 后的模式名。 */
  mode?: string;
  /** mode_change 的附加数据。 */
  data?: object;
  /** ttsr_injection 注入的规则列表。 */
  rules?: string[];
  /** retry_recovery 行对应的消息 wire id。 */
  messageID?: string;
  /** retry-recovery 详情（manager 臂同款）。 */
  retryRecovery?: AssistantArm['retryRecovery'];
}

/**
 * getEntries' cold arm: the five structured kinds stream from retained
 * row entries; `retry_recovery` rows come from the path's assistant emit
 * metas (same wireIdFor resolution the manager arm applies). Returns null
 * on the labeled fallbacks (legacy versions need entry-id migration; blob
 * refs resolve through BlobStore).
 */
/**
 * getEntries 的冷路径臂：五种结构化 kind 直接来自扫描保留的行条目；
 * retry_recovery 行由路径上的 assistant emit 元数据合成（wireIdFor 解析与
 * manager 臂一致）。wanted 为空集表示全部 kind。带标签回退（旧版本需条目
 * id 迁移、blob 引用需 BlobStore 等）返回 null。
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
