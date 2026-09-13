import { afterAll, describe, expect, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { loadEntriesFromFile } from '@oh-my-pi/pi-coding-agent/session/session-loader';
import { buildSessionContext } from '@oh-my-pi/pi-coding-agent/session/session-context';
import { migrateSessionEntries } from '@oh-my-pi/pi-coding-agent/session/session-migrations';
import { serializeTitleSlot } from '@oh-my-pi/pi-coding-agent/session/session-title-slot';
import type { SessionEntry } from '@oh-my-pi/pi-coding-agent/session/session-entries';
import {
  buildTurnStateStamper,
  paginateProjectedMessages,
  projectConversation,
  projectTurnEventDivider,
} from './projection.ts';
import type { ProjectedMessage } from './projection.ts';
import { readSessionEventRows, readSessionScalars, readTranscriptMessagePage } from './cold-transcript-page.ts';

// Parity matrix for the windowed cold reader (docs/PLAN.md §7.2/§7.3): the
// streamed page must equal the full-materialization arm — real JSONL files,
// real SDK loader + buildSessionContext as the reference. Both arms run the
// same projection + divider-merge + pagination math the engine applies.

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-coldpage-'));
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }));

const iso = (ms: number) => new Date(ms).toISOString();
let seq = 0;
const writeTranscript = (lines: unknown[]): string => {
  const file = path.join(dir, `t${seq++}.jsonl`);
  fs.writeFileSync(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n');
  return file;
};

/** JSONL field values the fixtures write — concrete union, no `unknown`. */
type JsonlValue = string | number | boolean | null | readonly JsonlValue[] | { [key: string]: JsonlValue };

const header = (id = 's1', extra: { [key: string]: JsonlValue } = {}) => ({
  type: 'session',
  version: 3,
  id,
  timestamp: iso(1_000),
  cwd: dir,
  ...extra,
});
const entry = (type: string, id: string, parentId: string | null, ts: number, extra: { [key: string]: JsonlValue } = {}) => ({
  type,
  id,
  parentId,
  timestamp: iso(ts),
  ...extra,
});
const userMsg = (ts: number, text: string) => ({
  role: 'user',
  content: [{ type: 'text', text }],
  timestamp: ts,
});
const assistantMsg = (ts: number, text: string, extra: { [key: string]: JsonlValue } = {}) => ({
  role: 'assistant',
  content: [{ type: 'text', text }],
  provider: 'p1',
  model: 'm1',
  timestamp: ts,
  stopReason: 'stop',
  ...extra,
});
const toolCallMsg = (ts: number, callId: string, name = 'bash') => ({
  role: 'assistant',
  content: [{ type: 'toolCall', id: callId, name, arguments: {} }],
  provider: 'p1',
  model: 'm1',
  timestamp: ts,
  stopReason: 'toolUse',
});
const toolResultMsg = (ts: number, callId: string, text = 'result-out') => ({
  role: 'toolResult',
  toolCallId: callId,
  content: [{ type: 'text', text }],
  timestamp: ts,
});
const msgEntry = (id: string, parentId: string | null, ts: number, message: JsonlValue) =>
  entry('message', id, parentId, ts, { message });

/** Reference arm: exactly the engine's cold file arm over a real load. */
const referencePage = async (filePath: string, { limit, before }: { limit?: number; before?: string } = {}) => {
  const loaded = await loadEntriesFromFile(filePath);
  // Migrate BEFORE filtering the header — migrateToCurrentVersion reads the
  // version off the session entry and would otherwise treat the file as v1
  // (id/parentId re-linking mutates the tree).
  migrateSessionEntries(loaded);
  const entries = loaded.filter((e): e is SessionEntry => e.type !== 'session');
  const context = buildSessionContext(entries, undefined, undefined, { transcript: true });
  const turnStateFor = buildTurnStateStamper(entries, {});
  const projected = projectConversation(context.messages, {
    sessionID: 's1',
    agent: 'build',
    turnStateFor,
  });
  const merged = [...projected];
  for (const e of entries) {
    const wire = projectTurnEventDivider(e, { sessionID: 's1' });
    if (!wire) continue;
    const at = merged.findIndex((i) => (i.info.time?.created ?? 0) >= (wire.info.time?.created ?? 0));
    merged.splice(at === -1 ? merged.length : at, 0, wire);
  }
  return { page: paginateProjectedMessages(merged, { limit, before }), count: context.messages.length };
};

const streamed = (filePath: string, opts: { limit?: number; before?: string } = {}) =>
  readTranscriptMessagePage(filePath, { sessionID: 's1', agent: 'build', limit: opts.limit, before: opts.before });

const expectParity = async (filePath: string, opts: { limit?: number; before?: string } = {}) => {
  const got = await streamed(filePath, opts);
  const ref = await referencePage(filePath, opts);
  expect(got).not.toBeNull();
  expect(got?.fileMessageCount).toBe(ref.count);
  expect(got?.page.cursor).toBe(ref.page.cursor);
  expect(got?.page.messages).toEqual([...ref.page.messages]);
  return got?.page;
};

describe('cold transcript page parity', () => {
  test('linear turns: tail page + cursor walk', async () => {
    const lines: unknown[] = [header()];
    let parent: string | null = null;
    for (let i = 0; i < 30; i++) {
      lines.push(msgEntry(`u${i}`, parent, 2_000 + i * 100, userMsg(2_000 + i * 100, `question ${i}`)));
      lines.push(msgEntry(`a${i}`, `u${i}`, 2_050 + i * 100, assistantMsg(2_050 + i * 100, `answer ${i}`)));
      parent = `a${i}`;
    }
    const file = writeTranscript(lines);
    const page1 = await expectParity(file, { limit: 5 });
    expect(page1?.messages).toHaveLength(5);
    expect(page1?.cursor).toBeTruthy();
    const page2 = await expectParity(file, { limit: 5, before: page1?.cursor });
    expect(page2?.messages).toHaveLength(5);
    const page3 = await expectParity(file, { limit: 5, before: page2?.cursor });
    expect(page3?.messages).toHaveLength(5);
  });

  test('model/mode/thinking changes: dividers + turn-state stamps', async () => {
    const file = writeTranscript([
      header(),
      entry('model_change', 'mc1', null, 1_500, { model: 'p1/m1', role: 'default' }),
      msgEntry('u1', 'mc1', 2_000, userMsg(2_000, 'first')),
      msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'one')),
      entry('thinking_level_change', 'tl1', 'a1', 2_200, { thinkingLevel: 'high' }),
      entry('mode_change', 'md1', 'tl1', 2_300, { mode: 'plan' }),
      msgEntry('u2', 'md1', 3_000, userMsg(3_000, 'second')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'two')),
    ]);
    const page = await expectParity(file, { limit: 10 });
    const kinds = page?.messages.map((m) => m.info.metadata?.ompRole ?? m.info.role);
    expect(kinds).toEqual(['modelChange', 'user', 'assistant', 'modeChange', 'user', 'assistant']);
    // Turn-state: u2 must carry the folded model/thinking snapshot.
    const u2 = page?.messages.find((m) => m.info.role === 'user' && m.info.time.created === 3_000);
    expect(u2?.info.model?.variant).toBe('high');
  });

  test('compaction pair: superseded elision + active summary', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'old q')),
      msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'old a')),
      entry('compaction', 'c1', 'a1', 2_500, { summary: 'first compaction', shortSummary: 'c1', firstKeptEntryId: 'u2', tokensBefore: 4000 }),
      msgEntry('u2', 'c1', 3_000, userMsg(3_000, 'mid q')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'mid a')),
      entry('compaction', 'c2', 'a2', 3_500, { summary: 'latest compaction', shortSummary: 'c2', firstKeptEntryId: 'u3', tokensBefore: 6000 }),
      msgEntry('u3', 'c2', 4_000, userMsg(4_000, 'new q')),
      msgEntry('a3', 'u3', 4_100, assistantMsg(4_100, 'new a')),
    ]);
    const page = await expectParity(file, { limit: 20 });
    const summaries = page?.messages.filter((m) => m.info.metadata?.ompRole === 'compactionSummary');
    expect(summaries).toHaveLength(2);
    const texts = summaries?.map((m) => m.parts[0]?.text) ?? [];
    expect(texts[0]).toContain('Superseded compaction');
    expect(texts[1]).toContain('latest compaction');
  });

  test('branch_summary + custom_message + gated customs', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
      msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'a')),
      entry('branch_summary', 'b1', 'a1', 2_200, { summary: 'branched away', fromId: 'a1' }),
      entry('custom_message', 'cm1', 'b1', 2_300, { customType: 'advisor', content: 'review note', display: true }),
      entry('custom_message', 'cm2', 'cm1', 2_400, { customType: 'hidden', content: 'invisible', display: false }),
      entry('custom_message', 'cm3', 'cm2', 2_500, { customType: 'broken', content: { not: 'string-or-array' }, display: true }),
      msgEntry('u2', 'cm3', 3_000, userMsg(3_000, 'q2')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'a2')),
    ]);
    const page = await expectParity(file, { limit: 20 });
    const ids = page?.messages.map((m) => m.info.metadata?.ompRole ?? m.info.role);
    expect(ids).toEqual(['user', 'assistant', 'branchSummary', 'assistant', 'user', 'assistant']);
    // cm3's invalid content emits nothing; cm2's display:false stays out.
    expect(page?.messages.some((m) => m.parts[0]?.text?.includes('invisible'))).toBe(false);
    expect(page?.messages.some((m) => m.parts[0]?.text?.includes('string-or-array'))).toBe(false);
  });

  test('branched path: off-path entries never emit', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'shared root')),
      msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'branch A answer')),
      msgEntry('u2', 'u1', 2_200, userMsg(2_200, 'branch B question')),
      msgEntry('a2', 'u2', 2_300, assistantMsg(2_300, 'branch B answer')),
    ]);
    // Leaf = a2 → path u1→u2→a2; a1 is off-path.
    const page = await expectParity(file, { limit: 20 });
    const hasText = (needle: string) => page?.messages.some((m) => m.parts.some((p) => p.text?.includes(needle)));
    expect(hasText('branch A answer')).toBe(false);
    expect(hasText('branch B answer')).toBe(true);
  });

  test('title-slot first line is peeled, not an entry', async () => {
    const slot = serializeTitleSlot({ title: 'Slot Title', source: 'auto', updatedAt: '2025-01-01T00:00:00Z' });
    const file = path.join(dir, `t${seq++}.jsonl`);
    fs.writeFileSync(
      file,
      slot +
        [
          JSON.stringify(header('s-slot')),
          JSON.stringify(msgEntry('u1', null, 2_000, userMsg(2_000, 'slotted q'))),
          JSON.stringify(msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'slotted a'))),
        ].join('\n') +
        '\n',
    );
    await expectParity(file, { limit: 10 });
    const scalars = await readSessionScalars(file);
    expect(scalars?.title).toBe('Slot Title');
    expect(scalars?.id).toBe('s-slot');
  });

  test('malformed record mid-file is skipped identically', async () => {
    const lines = [
      JSON.stringify(header()),
      JSON.stringify(msgEntry('u1', null, 2_000, userMsg(2_000, 'before'))),
      '{not valid json',
      JSON.stringify(msgEntry('a1', 'u1', 2_100, assistantMsg(2_100, 'after'))),
    ];
    const file = path.join(dir, `t${seq++}.jsonl`);
    fs.writeFileSync(file, lines.join('\n') + '\n');
    await expectParity(file, { limit: 10 });
  });

  test('tool call/result split across the page boundary still pairs', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'run it')),
      msgEntry('a1', 'u1', 2_100, toolCallMsg(2_100, 'call-1')),
      msgEntry('r1', 'a1', 2_200, toolResultMsg(2_200, 'call-1')),
      msgEntry('u2', 'r1', 3_000, userMsg(3_000, 'next')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'done')),
    ]);
    // limit=3 → page = [toolResult-folded a1, u2, a2]; a1's pairing is forward.
    const page = await expectParity(file, { limit: 3 });
    const a1 = page?.messages.find((m) => m.info.time.created === 2_100);
    expect(a1?.info.metadata?.ompStrippedToolCalls).toBeUndefined();
    const toolParts = a1?.parts.filter((p) => p.type === 'tool') ?? [];
    expect(toolParts.length).toBeGreaterThan(0);
    expect(toolParts[0]?.state?.status).toBe('completed');
  });

  test('dangling tool call keeps the strippedToolCalls marker', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'go')),
      msgEntry('a1', 'u1', 2_100, toolCallMsg(2_100, 'call-orphan')),
      msgEntry('u2', 'a1', 3_000, userMsg(3_000, 'after')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'tail')),
    ]);
    const page = await expectParity(file, { limit: 4 });
    const a1 = page?.messages.find((m) => m.info.time.created === 2_100);
    expect(a1?.info.metadata?.ompStrippedToolCalls).toBe(1);
  });

  test('before cursor on divider and custom wire ids', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
      entry('mode_change', 'md1', 'u1', 2_050, { mode: 'plan' }),
      msgEntry('a1', 'md1', 2_100, assistantMsg(2_100, 'a')),
      entry('custom_message', 'cm1', 'a1', 2_150, { customType: 'advisor', content: 'note text', display: true }),
      msgEntry('u2', 'cm1', 3_000, userMsg(3_000, 'q2')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'a2')),
    ]);
    const ref = await referencePage(file, { limit: 20 });
    const divider = ref.page.messages.find((m) => m.info.metadata?.ompRole === 'modeChange');
    expect(divider).toBeTruthy();
    await expectParity(file, { limit: 3, before: divider?.info.id });
    const custom = ref.page.messages.find((m) => m.parts[0]?.text?.includes('note text'));
    expect(custom).toBeTruthy();
    await expectParity(file, { limit: 3, before: custom?.info.id });
    // Unknown cursor → empty page, both arms.
    await expectParity(file, { limit: 3, before: 'msg_zzzzzzzzzzzz' });
  });

  test('developer messages anchor and attribute identically', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
      msgEntry('dev1', 'u1', 2_050, {
        role: 'developer',
        content: [{ type: 'text', text: 'synthetic prompt' }],
        attribution: 'user',
        timestamp: 2_050,
      }),
      msgEntry('a1', 'dev1', 2_100, assistantMsg(2_100, 'a')),
      msgEntry('dev2', 'a1', 2_150, {
        role: 'developer',
        content: [{ type: 'text', text: 'mid-turn note' }],
        attribution: 'agent',
        timestamp: 2_150,
      }),
      msgEntry('u2', 'dev2', 3_000, userMsg(3_000, 'q2')),
      msgEntry('a2', 'u2', 3_100, assistantMsg(3_100, 'a2')),
    ]);
    await expectParity(file, { limit: 20 });
  });

  test('fallbacks: legacy version, blob refs, unbounded limit', async () => {
    const legacy = writeTranscript([
      { type: 'session', version: 2, id: 's1', timestamp: iso(1_000), cwd: dir },
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
    ]);
    expect(await streamed(legacy, { limit: 5 })).toBeNull();

    const blobbed = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'blob:sha256:abc123')),
    ]);
    expect(await streamed(blobbed, { limit: 5 })).toBeNull();

    const normal = writeTranscript([header(), msgEntry('u1', null, 2_000, userMsg(2_000, 'q'))]);
    expect(await streamed(normal, {})).toBeNull(); // no limit → fallback
  });
});

describe('cold scalar + event-row readers', () => {
  test('readSessionScalars matches the header + tail fields', async () => {
    const file = writeTranscript([
      header('s-scalar', { title: 'My Session' }),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
      entry('label', 'lb1', 'u1', 2_500, { targetId: 'u1', label: 'mark' }),
      msgEntry('a1', 'lb1', 3_000, assistantMsg(3_000, 'a')),
    ]);
    const scalars = await readSessionScalars(file);
    expect(scalars).not.toBeNull();
    expect(scalars?.id).toBe('s-scalar');
    expect(scalars?.title).toBe('My Session');
    expect(scalars?.cwd).toBe(dir);
    expect(scalars?.createdIso).toBe(iso(1_000));
    expect(scalars?.modifiedIso).toBe(iso(3_000));
  });

  test('readSessionScalars falls back on missing/invalid header', async () => {
    const bad = path.join(dir, `t${seq++}.jsonl`);
    fs.writeFileSync(bad, JSON.stringify({ type: 'message', id: 'x' }) + '\n');
    expect(await readSessionScalars(bad)).toBeNull();
  });

  test('readSessionEventRows matches the manager-arm row shape', async () => {
    const file = writeTranscript([
      header(),
      msgEntry('u1', null, 2_000, userMsg(2_000, 'q')),
      entry('model_change', 'mc1', 'u1', 2_050, { model: 'p1/m1', role: 'default' }),
      msgEntry('a1', 'mc1', 2_100, { ...assistantMsg(2_100, 'a'), retryRecovery: { attempt: 2 } }),
      entry('mode_change', 'md1', 'a1', 2_150, { mode: 'plan', data: { plan: 'x' } }),
      entry('ttsr_injection', 'tt1', 'md1', 2_200, { injectedRules: ['rule-a'] }),
      entry('compaction', 'c1', 'tt1', 2_300, { summary: 'sum', tokensBefore: 42 }),
      entry('branch_summary', 'b1', 'c1', 2_400, { summary: 'bs', fromId: 'a1' }),
    ]);
    const rows = await readSessionEventRows(file, new Set(), undefined);
    expect(rows).not.toBeNull();
    const kinds = rows?.map((r) => r.kind);
    expect(kinds).toEqual(['model_change', 'mode_change', 'ttsr_injection', 'compaction', 'branch_summary', 'retry_recovery']);
    const retry = rows?.find((r) => r.kind === 'retry_recovery');
    expect(retry?.messageID).toMatch(/^msg_a/);
    // kinds filter keeps only the wanted rows (no retry_recovery unless asked).
    const filtered = await readSessionEventRows(file, new Set(['model_change']), undefined);
    expect(filtered?.map((r) => r.kind)).toEqual(['model_change']);
  });
});
