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

const TS = 1_700_000_000_000;

type FixtureBlock = { type: string } & Record<string, string>;
type FixtureMessage = UserMessageInput | AssistantMessageInput;
type FixtureEntry = { type: 'message'; id: string; parentId: string | null; timestamp: string; message: FixtureMessage };

const inner = (role: 'user' | 'assistant', content: string | FixtureBlock[], timestamp: number): FixtureMessage => {
  if (role === 'user') return { role: 'user', timestamp, content };
  // SAFETY: assistant fixtures always pass block arrays (see call sites).
  return { role: 'assistant', timestamp, content: content as readonly ProjectedContentBlock[] };
};
const entry = (id: string, message: FixtureMessage, parentId: string | null = null): FixtureEntry => ({
  type: 'message',
  id,
  parentId,
  timestamp: new Date(message.timestamp).toISOString(),
  message,
});

const userMessage = inner('user', [{ type: 'text', text: 'reply with exactly: ok' }], TS);
const assistantText = inner('assistant', [{ type: 'text', text: 'ok' }], TS + 10);
const assistantToolOnly = inner('assistant', [{ type: 'toolCall', name: 'bash', callId: 't1' }], TS + 20);

// SAFETY: fixture rows satisfy TranscriptEntryInput structurally (typed
// entry rows + a bare divider row); the label routes them into the resolver.
const entries: TranscriptEntryInput[] = [
  entry('e1', userMessage),
  entry('e2', assistantText, 'e1'),
  { type: 'thinking_level_change', id: 'e3' },
];

describe('resolveWireIdToEntryId', () => {
  test('resolves user and assistant text wire ids to entry ids', () => {
    const userWire = wireMessageId('user', TS, 'reply with exactly: ok');
    const assistantWire = wireMessageId('assistant', TS + 10, 'ok');
    expect(resolveWireIdToEntryId(entries, userWire)).toBe('e1');
    expect(resolveWireIdToEntryId(entries, assistantWire)).toBe('e2');
  });

  test('non-message entries are skipped; assistant-without-text seeds from block name', () => {
    const toolWire = wireMessageId('assistant', TS + 20, 'bash');
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
