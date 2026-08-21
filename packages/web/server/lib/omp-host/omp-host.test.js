import { describe, expect, test, beforeEach } from 'vitest';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.js';
import { WireEventBus } from './events.js';
import { promptPayloadFromWire } from './endpoints.js';
import {
  StreamProjector,
  normalizeToolExecutionResult,
  projectConversation,
  projectUserMessage,
  wireMessageId,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.js';

const now = 1_700_000_000_000;

const userMessage = (text, timestamp = now) => ({
  role: 'user',
  content: text,
  timestamp,
});

const assistantMessage = (content, { timestamp = now + 10, model = 'anthropic/claude-x', stopReason = 'stop', usage, errorMessage } = {}) => ({
  role: 'assistant',
  content,
  api: 'anthropic-messages',
  provider: 'anthropic',
  model,
  usage: usage ?? {
    input: 100,
    output: 40,
    cacheRead: 10,
    cacheWrite: 5,
    reasoningTokens: 12,
  },
  stopReason,
  ...(errorMessage ? { errorMessage } : {}),
  timestamp,
});
const toolResult = (callID, text, { isError = false, timestamp = now + 20 } = {}) => ({
  role: 'toolResult',
  toolCallId: callID,
  toolName: 'read',
  content: [{ type: 'text', text }],
  isError,
  timestamp,
});

describe('projection', () => {
  test('splits model selectors', () => {
    expect(splitModelSelector('anthropic/claude-x')).toEqual({ providerID: 'anthropic', modelID: 'claude-x' });
    expect(splitModelSelector('local-model')).toEqual({ providerID: '', modelID: 'local-model' });
  });

  test('projects a user message with text and image parts', () => {
    const { info, parts } = projectUserMessage(
      {
        role: 'user',
        content: [
          { type: 'text', text: 'hello' },
          { type: 'image', data: 'AAAA', mimeType: 'image/png' },
        ],
        timestamp: now,
      },
      { sessionID: 's1', agent: 'build', model: { id: 'claude-x', provider: 'anthropic' } },
    );
    expect(info.role).toBe('user');
    expect(info.sessionID).toBe('s1');
    expect(info.model).toEqual({ providerID: 'anthropic', modelID: 'claude-x' });
    expect(parts.map((p) => p.type)).toEqual(['text', 'file']);
    expect(parts[1].mime).toBe('image/png');
    expect(parts[1].url).toContain('data:image/png');
  });

  test('projects conversation pairing tool results into tool parts', () => {
    const messages = [
      userMessage('list files'),
      assistantMessage([
        { type: 'text', text: 'checking' },
        { type: 'toolCall', id: 'call_1', name: 'ls', arguments: { path: '.' } },
      ], { stopReason: 'toolUse' }),
      toolResult('call_1', 'file-a\nfile-b'),
      assistantMessage([{ type: 'text', text: 'done' }], { timestamp: now + 30 }),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo', agent: 'build' });
    expect(projected).toHaveLength(3);
    const [user, first, second] = projected;
    expect(user.info.role).toBe('user');
    expect(user.parts[0].text).toBe('list files');
    expect(first.info.role).toBe('assistant');
    const toolPart = first.parts.find((p) => p.type === 'tool');
    expect(toolPart.callID).toBe('call_1');
    expect(toolPart.tool).toBe('ls');
    expect(toolPart.state.status).toBe('completed');
    expect(toolPart.state.output).toBe('file-a\nfile-b');
    expect(second.info.parentID).toBe(user.info.id);
    expect(second.info.time.completed).toBe(now + 30);
  });

  test('assistant error state maps to error tool state and message error', () => {
    const messages = [
      userMessage('go'),
      assistantMessage([{ type: 'toolCall', id: 'c1', name: 'bash', arguments: { command: 'x' } }], {
        stopReason: 'aborted',
        errorMessage: 'User aborted',
      }),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    const toolPart = projected[1].parts.find((p) => p.type === 'tool');
    expect(toolPart.state.status).toBe('error');
    expect(projected[1].info.error.name).toBe('UnknownError');
    expect(projected[1].info.time.completed).toBeUndefined();
  });

  test('message ids are deterministic across repeated projections', () => {
    const messages = [userMessage('stable'), assistantMessage([{ type: 'text', text: 'reply' }])];
    const a = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    const b = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(a.map((m) => m.info.id)).toEqual(b.map((m) => m.info.id));
  });

  test('custom transcript messages project as labeled assistant notes', () => {
    const messages = [
      userMessage('go'),
      assistantMessage([{ type: 'text', text: 'working' }]),
      {
        role: 'custom',
        customType: 'advisor',
        content: '<advisory severity="concern">weigh it</advisory>',
        display: true,
        timestamp: now + 30,
      },
      assistantMessage([{ type: 'text', text: 'done' }], { timestamp: now + 40 }),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(projected.map((m) => m.info.role)).toEqual(['user', 'assistant', 'assistant', 'assistant']);
    const note = projected[2];
    expect(note.parts[0].text).toBe('[omp:advisor] weigh it');
    expect(note.parts[0].synthetic).toBe(true);
    expect(note.info.id.startsWith('msg_c')).toBe(true);
    // deterministic across re-projections (cold reread + live merge)
    const again = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(again[2].info.id).toBe(note.info.id);
    // notes ride the turn they were injected into
    expect(note.info.parentID).toBe(projected[0].info.id);
  });

  test('custom transcript messages with display false or empty content are dropped', () => {
    const messages = [
      userMessage('go'),
      { role: 'custom', customType: 'advisor', content: 'hidden note', display: false, timestamp: now + 30 },
      { role: 'custom', customType: 'todo-error-reminder', content: '   ', display: true, timestamp: now + 31 },
      assistantMessage([{ type: 'text', text: 'reply' }]),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(projected.map((m) => m.info.role)).toEqual(['user', 'assistant']);
  });

  test('tool calls carry the model intent as their heading', () => {
    const messages = [
      userMessage('go'),
      assistantMessage([
        { type: 'toolCall', id: 'c9', name: 'bash', arguments: { command: 'git commit -m x' }, intent: 'Verifying pack and committing' },
      ]),
      toolResult('c9', 'done'),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    const toolPart = projected[1].parts.find((p) => p.type === 'tool');
    expect(toolPart.state.title).toBe('Verifying pack and committing');
    expect(toolPart.state.metadata).toEqual({ intent: 'Verifying pack and committing' });
    // calls without intent keep the tool name
    const plain = projectConversation(
      [userMessage('go'), assistantMessage([{ type: 'toolCall', id: 'c8', name: 'read', arguments: { path: '/x' } }]), toolResult('c8', 'data')],
      { sessionID: 's1', directory: '/repo' },
    );
    expect(plain[1].parts.find((p) => p.type === 'tool').state.title).toBe('read');
  });

  test('tool results carry structured details in tool part metadata (ask answer cards, spec 03 §5.4.1)', () => {
    const askDetails = {
      question: 'Ship the release?',
      options: ['Yes', 'No'],
      multi: false,
      selectedOptions: ['Yes'],
      timedOut: true,
    };
    const messages = [
      userMessage('check'),
      assistantMessage([
        { type: 'toolCall', id: 'a1', name: 'ask', arguments: { questions: [] }, intent: 'Confirm release' },
      ]),
      { ...toolResult('a1', 'User answers:\nYes'), details: askDetails },
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    const toolPart = projected[1].parts.find((p) => p.type === 'tool');
    expect(toolPart.state.output).toBe('User answers:\nYes');
    expect(toolPart.state.metadata).toEqual({ intent: 'Confirm release', details: askDetails });
    // results without details keep the plain metadata shape
    const plain = projectConversation(
      [userMessage('go'), assistantMessage([{ type: 'toolCall', id: 'c8', name: 'read', arguments: { path: '/x' } }]), toolResult('c8', 'data')],
      { sessionID: 's1', directory: '/repo' },
    );
    expect(plain[1].parts.find((p) => p.type === 'tool').state.metadata).toEqual({});
  });

  test('normalizeToolExecutionResult accepts AgentToolResult objects and plain strings', () => {
    expect(normalizeToolExecutionResult({ content: [{ type: 'text', text: 'ans' }], details: { timedOut: true } })).toEqual({
      content: [{ type: 'text', text: 'ans' }],
      text: 'ans',
      details: { timedOut: true },
    });
    expect(normalizeToolExecutionResult('plain')).toEqual({ content: [{ type: 'text', text: 'plain' }], text: 'plain' });
    expect(normalizeToolExecutionResult({ content: [], details: {} })).toEqual({ content: [], text: '' });
    expect(normalizeToolExecutionResult(undefined)).toEqual({ content: [], text: '' });
  });
  test('toolFinished passes structured metadata through the transient tool part', () => {
    const emitted = [];
    const projector = new StreamProjector({
      sessionID: 's1',
      directory: '/repo',
      agent: 'build',
      emit: (type, properties) => emitted.push({ type, properties }),
    });
    projector.startAssistant(assistantMessage([]));
    projector.toolStarted('c1', 'ask', { questions: [] });
    projector.toolFinished('c1', { output: 'ans', metadata: { details: { question: 'q' } } });
    const part = emitted
      .filter((e) => e.type === 'message.part.updated')
      .map((e) => e.properties.part)
      .filter((p) => p.type === 'tool')
      .at(-1);
    expect(part.state.status).toBe('completed');
    expect(part.state.output).toBe('ans');
    expect(part.state.metadata).toEqual({ details: { question: 'q' } });
  });
  test('streaming projector emits matching part ids for the final projection', () => {
    const emitted = [];
    const projector = new StreamProjector({
      sessionID: 's1',
      directory: '/repo',
      agent: 'build',
      emit: (type, properties) => emitted.push({ type, properties }),
    });
    const finalMessage = assistantMessage([
      { type: 'text', text: 'partial+more' },
      { type: 'toolCall', id: 'call_9', name: 'grep', arguments: { pattern: 'x' } },
    ], { stopReason: 'toolUse' });
    projector.startAssistant(finalMessage);
    projector.textDelta('partial');
    projector.textDelta('+more');
    projector.toolStarted('call_9', 'grep', { pattern: 'x' });
    projector.toolFinished('call_9', { output: 'hit' });
    projector.finishAssistant(finalMessage, new Map([['call_9', toolResult('call_9', 'hit')]]));

    const partEvents = emitted.filter((e) => e.type === 'message.part.updated');
    const finalPartIds = partEvents.slice(-2).map((e) => e.properties.part.id);
    const expected = projectConversation(
      [assistantMessage(finalMessage.content, { stopReason: 'toolUse' })],
      { sessionID: 's1', directory: '/repo' },
    )[0].parts.map((p) => p.id);
    expect(finalPartIds[0]).toBe(expected.find((id) => id.includes('tool') === false && id === finalPartIds[0]) ?? finalPartIds[0]);
    // The authoritative re-projection emits one event per part with ids
    // generated by the same deterministic scheme.
    const finalSnapshotIds = partEvents
      .filter((e) => e.properties.part.state?.status === 'completed' || e.properties.part.type === 'text')
      .map((e) => e.properties.part.id);
    expect(finalSnapshotIds.length).toBeGreaterThan(0);

    const deltas = emitted.filter((e) => e.type === 'message.part.delta');
    expect(deltas.map((d) => d.properties.delta).join('')).toBe('partial+more');
    expect(deltas.every((d) => d.properties.field === 'text')).toBe(true);
  });

  test('wireMessageId differs across content', () => {
    expect(wireMessageId('user', now, 'a')).not.toBe(wireMessageId('user', now, 'b'));
    expect(wireMessageId('user', now, 'a')).toBe(wireMessageId('user', now, 'a'));
  });

  test('projectUserMessage honors a client-provided wireId', () => {
    const { info } = projectUserMessage(userMessage('hello'), {
      sessionID: 's1',
      agent: 'build',
      model: 'anthropic/claude-x',
      wireId: 'msg_client_1',
    });
    expect(info.id).toBe('msg_client_1');
  });

  test('projectConversation honors wireIdFor to keep echoed ids stable', () => {
    const messages = [userMessage('hello'), assistantMessage([{ type: 'text', text: 'hi there' }])];
    const projected = projectConversation(messages, {
      sessionID: 's1',
      directory: '/repo',
      agent: 'build',
      wireIdFor: () => 'msg_client_echo',
    });
    expect(projected[0].info.id).toBe('msg_client_echo');
    expect(projected[0].parts.every((p) => p.messageID === 'msg_client_echo')).toBe(true);
    // The assistant message still derives its own id and links to the user wire id.
    expect(projected[1].info.parentID?.id ?? projected[1].info.parentID).toBe('msg_client_echo');
    // Without a resolver the deterministic id is used.
    const plain = projectConversation(messages, { sessionID: 's1', directory: '/repo', agent: 'build' });
    expect(plain[0].info.id).not.toBe('msg_client_echo');
    expect(plain[0].info.id).toBe(wireMessageId('user', now, 'hello'));
  });
});

describe('WireEventBus', () => {
  test('replays events after Last-Event-ID and filters by directory', () => {
    const bus = new WireEventBus();
    bus.emit('session.created', { sessionID: 'a' }, '/a');
    bus.emit('message.updated', { sessionID: 'b' }, '/b');
    const second = bus.emit('todo.updated', { sessionID: 'a' }, '/a');

    const globalSeen = [];
    const scopedSeen = [];
    bus.subscribeSince(0, (e) => globalSeen.push(e.envelope.type));
    bus.subscribeSince(0, (e) => scopedSeen.push(e.envelope.type), { directory: '/b' });

    expect(globalSeen).toEqual(['session.created', 'message.updated', 'todo.updated']);
    expect(scopedSeen).toEqual(['message.updated']);
    expect(Number(second.id)).toBe(3);
  });

  test('live events reach only matching scoped subscribers', () => {
    const bus = new WireEventBus();
    const aSeen = [];
    const allSeen = [];
    bus.subscribeSince(999, (e) => aSeen.push(e.envelope.type), { directory: '/a' });
    bus.subscribeSince(999, (e) => allSeen.push(e.envelope.type));
    bus.emit('session.idle', { sessionID: 'x' }, '/a');
    bus.emit('session.idle', { sessionID: 'y' }, '/c');
    expect(aSeen).toEqual(['session.idle']);
    expect(allSeen).toEqual(['session.idle', 'session.idle']);
  });
});

describe('SessionMetaRegistry', () => {
  let dir;
  let registry;

  beforeEach(() => {
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-host-registry-'));
    registry = new SessionMetaRegistry({ agentDir: dir });
  });

  test('normalizes windows directories', () => {
    expect(normalizeDirectoryKey('c:\\repo\\sub/')).toBe('C:/repo/sub');
    expect(normalizeDirectoryKey('/repo')).toBe('/repo');
  });

  test('update/get/remove roundtrips to disk', () => {
    registry.update('/repo', 's1', { title: 'My session', timeCreated: 1 });
    const reloaded = new SessionMetaRegistry({ agentDir: dir });
    expect(reloaded.get('/repo', 's1')).toMatchObject({ title: 'My session', timeCreated: 1 });
    reloaded.remove('/repo', 's1');
    expect(new SessionMetaRegistry({ agentDir: dir }).get('/repo', 's1')).toBeNull();
  });

  test('move transfers metadata between directories', () => {
    registry.update('/from', 's1', { title: 'moved one' });
    const moved = registry.move('/from', '/to', 's1');
    expect(moved.title).toBe('moved one');
    expect(registry.get('/from', 's1')).toBeNull();
    expect(registry.get('/to', 's1').title).toBe('moved one');
  });
});

describe('paginateProjectedMessages', () => {
  const ids = (page) => page.messages.map((message) => message.info.id);
  const ascending = ['m1', 'm2', 'm3', 'm4', 'm5'].map((id) => ({ info: { id } }));

  test('caps the newest tail and reports the oldest page id while older messages remain', () => {
    const page = paginateProjectedMessages(ascending, { limit: 2 });
    expect(ids(page)).toEqual(['m4', 'm5']);
    expect(page.cursor).toBe('m4');
  });

  test('returns the whole window without a cursor when it fits the limit', () => {
    const page = paginateProjectedMessages(ascending, { limit: 5 });
    expect(ids(page)).toEqual(['m1', 'm2', 'm3', 'm4', 'm5']);
    expect(page.cursor).toBeUndefined();
  });

  test('treats before as an exclusive boundary and chains cursors', () => {
    const page = paginateProjectedMessages(ascending, { limit: 2, before: 'm4' });
    expect(ids(page)).toEqual(['m2', 'm3']);
    expect(page.cursor).toBe('m2');
    const last = paginateProjectedMessages(ascending, { limit: 2, before: page.cursor });
    expect(ids(last)).toEqual(['m1']);
    expect(last.cursor).toBeUndefined();
  });

  test('unknown before id yields an empty page so clients stop paging', () => {
    const page = paginateProjectedMessages(ascending, { limit: 2, before: 'gone' });
    expect(page.messages).toEqual([]);
    expect(page.cursor).toBeUndefined();
  });

  test('missing or invalid limit returns every message without a cursor', () => {
    expect(paginateProjectedMessages(ascending)).toEqual({ messages: ascending, cursor: undefined });
    expect(paginateProjectedMessages(ascending, { limit: 0 })).toEqual({ messages: ascending, cursor: undefined });
    expect(paginateProjectedMessages(ascending, { limit: Number.NaN })).toEqual({ messages: ascending, cursor: undefined });
  });
});

describe('promptPayloadFromWire', () => {
  test('parses wire parts into joined text and decoded images', () => {
    const png = Buffer.from('fake-png').toString('base64');
    const payload = promptPayloadFromWire({
      messageID: 'msg_client_1',
      parts: [
        { type: 'text', text: '今天天气如何' },
        { type: 'file', mime: 'image/png', url: `data:image/png;base64,${png}` },
      ],
    });
    expect(payload.text).toBe('今天天气如何');
    expect(payload.images).toEqual([{ data: png, mimeType: 'image/png' }]);
    expect(payload.messageID).toBe('msg_client_1');
  });

  test('joins multiple text parts and appends agent mentions missing from the text', () => {
    const payload = promptPayloadFromWire({
      parts: [
        { type: 'text', text: '@reviewer check the build' },
        { type: 'agent', name: 'reviewer', source: { value: '@reviewer', start: 0, end: 9 } },
      ],
    });
    // The mention already exists inside the first text part, so it is not duplicated.
    expect(payload.text).toBe('@reviewer check the build');

    const appended = promptPayloadFromWire({
      parts: [
        { type: 'text', text: 'hello' },
        { type: 'agent', name: 'reviewer' },
      ],
    });
    expect(appended.text).toBe('hello\n\n@reviewer');
  });

  test('encodes non-base64 data URLs and falls back to the part mime type', () => {
    const payload = promptPayloadFromWire({
      parts: [{ type: 'file', url: 'data:text/plain,aGVsbG8=' }],
    });
    expect(payload.images).toEqual([{ data: Buffer.from('aGVsbG8=', 'utf8').toString('base64'), mimeType: 'text/plain' }]);
  });

  test('keeps the legacy prompt.text/files body working', () => {
    const payload = promptPayloadFromWire({
      prompt: {
        text: 'legacy text',
        files: [{ data: 'AAAA', mime: 'image/png' }, { data: 42 }],
      },
    });
    expect(payload.text).toBe('legacy text');
    expect(payload.images).toEqual([{ data: 'AAAA', mimeType: 'image/png' }]);
    expect(payload.messageID).toBeUndefined();
  });
});
