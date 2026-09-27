/**
 * wire-id ↔ session-entry 解析（projection.ts）的测试套件。
 *
 * UI 从 GET messages 拿到的是确定投影出的 wire 消息 id，而
 * SessionManager.branch 需要 session ENTRY id；resolveWireIdToEntryId 负责在
 * 管理器的 entry 列表里逐条比对 type:"message" entry 包裹的 AgentMessage
 * 投影 id。本套件用结构化 fixture 钉住解析、wireIdFor 覆盖与退化输入的行为。
 */
// Wire-id ↔ session-entry resolution (engine revert /undo path, spec 04 GAP-06).
//
// The UI sends the wire message id from GET messages; SessionManager.branch
// wants the session ENTRY id. resolveWireIdToEntryId walks the manager's
// entry list — each `type: "message"` entry wraps the AgentMessage whose
// deterministic projection produced the wire id the UI saw.

import { describe, expect, test } from 'bun:test';
import {
  deterministicWireId,
  resolveWireIdToEntryId,
  wireMessageId,
} from './projection.ts';
import type { AssistantMessageInput, ProjectedContentBlock, TranscriptEntryInput, UserMessageInput } from './projection.ts';

/** 所有 fixture 共用的基准时间戳（毫秒）。 */
const TS = 1_700_000_000_000;

/** fixture 内容块：只用 type 加少量字符串字段的宽松形状。 */
type FixtureBlock = { type: string } & Record<string, string>;
/** fixture 消息：用户或 assistant 消息的最小输入形状。 */
type FixtureMessage = UserMessageInput | AssistantMessageInput;
/** fixture entry：type 为 message 的 transcript 行（含父 id 与时间戳）。 */
type FixtureEntry = { type: 'message'; id: string; parentId: string | null; timestamp: string; message: FixtureMessage };

/** 按角色构造 fixture 消息；assistant 分支要求传入块数组（见调用点）。 */
const inner = (role: 'user' | 'assistant', content: string | FixtureBlock[], timestamp: number): FixtureMessage => {
  if (role === 'user') return { role: 'user', timestamp, content };
  // SAFETY: assistant fixtures always pass block arrays (see call sites).
  return { role: 'assistant', timestamp, content: content as readonly ProjectedContentBlock[] };
};
/** 包装成一条 message entry；parentId 默认 null（根节点）。 */
const entry = (id: string, message: FixtureMessage, parentId: string | null = null): FixtureEntry => ({
  type: 'message',
  id,
  parentId,
  timestamp: new Date(message.timestamp).toISOString(),
  message,
});

/** 用户消息 fixture：一条文本指令。 */
const userMessage = inner('user', [{ type: 'text', text: 'reply with exactly: ok' }], TS);
/** assistant 文本回复 fixture。 */
const assistantText = inner('assistant', [{ type: 'text', text: 'ok' }], TS + 10);
/** assistant 纯工具调用 fixture（无文本块）。 */
const assistantToolOnly = inner('assistant', [{ type: 'toolCall', name: 'bash', callId: 't1' }], TS + 20);

// SAFETY: fixture rows satisfy TranscriptEntryInput structurally (typed
// entry rows + a bare divider row); the label routes them into the resolver.
// 中文补充：fixture 行在结构上满足 TranscriptEntryInput（带类型的
// entry 行 + 一条裸分隔行）；type 标签决定解析器是否处理该行。
const entries: TranscriptEntryInput[] = [
  entry('e1', userMessage),
  entry('e2', assistantText, 'e1'),
  { type: 'thinking_level_change', id: 'e3' },
];

// wire id → entry id 解析契约：文本消息可解析、非 message 行被跳过、
// wireIdFor 覆盖优先、退化输入一律返回 null。
describe('resolveWireIdToEntryId', () => {
  test('resolves user and assistant text wire ids to entry ids', () => {
    const userWire = wireMessageId('user', TS, 'reply with exactly: ok');
    // Stable formula (plan phase 5): assistant ids are content-independent.
    const assistantWire = wireMessageId('assistant', TS + 10, '');
    expect(resolveWireIdToEntryId(entries, userWire)).toBe('e1');
    expect(resolveWireIdToEntryId(entries, assistantWire)).toBe('e2');
  });

  test('non-message entries are skipped; assistant ids ignore content', () => {
    const toolWire = wireMessageId('assistant', TS + 20, '');
    // SAFETY: the e9 row is a message entry; the label routes it to the resolver.
    const e9Row: TranscriptEntryInput = entry('e9', assistantToolOnly);
    expect(resolveWireIdToEntryId([e9Row], toolWire)).toBe('e9');
    expect(resolveWireIdToEntryId(entries, 'e3')).toBeNull();
  });

  test('wireIdFor overrides win (client-echoed user ids)', () => {
    const override = 'msg_echoed-client-id';
    const wireIdFor = (message: { role?: string }) => (message === userMessage ? override : undefined);
    expect(resolveWireIdToEntryId(entries, override, { wireIdFor })).toBe('e1');
    expect(resolveWireIdToEntryId(entries, override)).toBeNull();
  });

  test('degenerate inputs resolve null', () => {
    // SAFETY: userMessage is a user-role message (inner('user', …)).
    expect(resolveWireIdToEntryId([], deterministicWireId(userMessage))).toBeNull();
    expect(resolveWireIdToEntryId(entries, 'msg_nope')).toBeNull();
    expect(resolveWireIdToEntryId(entries, '')).toBeNull();
    expect(resolveWireIdToEntryId(null, 'x')).toBeNull();
  });
});
