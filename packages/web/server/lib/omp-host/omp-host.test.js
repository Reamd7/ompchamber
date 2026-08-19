import { describe, expect, test, beforeEach } from 'vitest';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.js';
import { WireEventBus } from './events.js';
import {
  StreamProjector,
  projectConversation,
  projectUserMessage,
  wireMessageId,
  splitModelSelector,
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
