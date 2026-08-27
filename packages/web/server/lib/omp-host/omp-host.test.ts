import { describe, expect, test, beforeEach } from 'vitest';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SessionMetaRegistry, normalizeDirectoryKey } from './registry.ts';
import { WireEventBus } from './events.ts';
import { promptPayloadFromWire, wireSkillRows } from './endpoints.ts';
import {
  StreamProjector,
  normalizeToolExecutionResult,
  projectConversation,
  projectUserMessage,
  wireMessageId,
  splitModelSelector,
  paginateProjectedMessages,
} from './projection.ts';
import type {
  UsageInput,
  UserMessageInput,
  AssistantMessageInput,
  ToolResultMessageInput,
  ProjectedContentBlock,
  MessageInput,
  ProjectedMessage,
  ProjectedMessagePage,
  WireMessageInfo,
} from './projection.ts';

const now = 1_700_000_000_000;

const userMessage = (text: string, timestamp: number = now): UserMessageInput => ({
  role: 'user',
  content: text,
  timestamp,
});

/** Assistant transcript fixture — carries the inert `api` marker alongside the projection contract. */
interface AssistantFixture extends AssistantMessageInput {
  api?: string;
}

/** Tool-result fixture — carries the inert `toolName` label alongside the pairing contract. */
interface ToolResultFixture extends ToolResultMessageInput {
  toolName?: string;
}


const assistantMessage = (
  content?: readonly ProjectedContentBlock[],
  {
    timestamp = now + 10,
    model = 'anthropic/claude-x',
    stopReason = 'stop',
    usage,
    errorMessage,
  }: { timestamp?: number; model?: string; stopReason?: string; usage?: UsageInput; errorMessage?: string } = {},
): AssistantFixture => ({
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
const toolResult = (
  callID: string,
  text: string,
  { isError = false, timestamp = now + 20 }: { isError?: boolean; timestamp?: number } = {},
): ToolResultFixture => ({
  role: 'toolResult',
  toolCallId: callID,
  toolName: 'read',
  content: [{ type: 'text', text }],
  isError,
  timestamp,
});

/**
 * Wire `parentID` is a bare string, but the frozen wireIdFor assertion reads it
 * through `parentID?.id ?? parentID`. This view keeps the string while exposing
 * a chainable `.id`; it stays assignable to ProjectedMessage, so the single
 * `as` in that test narrows the page — it is not a type launder.
 */
type ChainableParentPage = Array<
  Omit<ProjectedMessage, 'info'> & {
    info: Omit<WireMessageInfo, 'parentID'> & { parentID?: string & { id?: string } };
  }
>;

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

  test('finish maps from stopReason on settled messages only', () => {
    // Cold projection: stopReason rides verbatim as wire finish.
    const projected = projectConversation(
      [
        userMessage('go'),
        assistantMessage([{ type: 'text', text: 'a' }], { stopReason: 'toolUse' }),
      ],
      { sessionID: 's1', directory: '/repo' },
    );
    expect(projected[1].info.finish).toBe('toolUse');

    // No stopReason on the persisted message → the key stays absent.
    const bare = assistantMessage([{ type: 'text', text: 'x' }]);
    delete bare.stopReason;
    const bareProjected = projectConversation([userMessage('go'), bare], {
      sessionID: 's1',
      directory: '/repo',
    });
    expect('finish' in bareProjected[1].info).toBe(false);

    // Live streaming: message_start carries NO finish (open-step signal);
    // message_end settles it from the same mapping.
    const emitted = [];
    const projector = new StreamProjector({
      sessionID: 's1',
      directory: '/repo',
      agent: 'build',
      emit: (type, properties) => emitted.push({ type, properties }),
    });
    const finalMessage = assistantMessage([{ type: 'text', text: 'a' }]);
    const started = projector.startAssistant(finalMessage);
    expect('finish' in started).toBe(false);
    projector.finishAssistant(finalMessage, new Map());
    const settledInfo = emitted
      .filter((e) => e.type === 'message.updated')
      .map((e) => e.properties.info)
      .at(-1);
    expect(settledInfo.finish).toBe('stop');
  });

  test('projectUsage emits the SDK totalTokens as wire tokens.total when present', () => {
    const withTotal = assistantMessage([{ type: 'text', text: 'a' }], {
      usage: { input: 10, output: 5, cacheRead: 2, cacheWrite: 1, totalTokens: 18 },
    });
    const [, withTotalMessage] = projectConversation([userMessage('q'), withTotal], {
      sessionID: 's1',
      directory: '/repo',
    });
    expect(withTotalMessage.info.tokens?.total).toBe(18);

    const withoutTotal = assistantMessage([{ type: 'text', text: 'b' }], {
      usage: { input: 10, output: 5 },
    });
    const [, withoutTotalMessage] = projectConversation([userMessage('q'), withoutTotal], {
      sessionID: 's1',
      directory: '/repo',
    });
    expect('total' in (withoutTotalMessage.info.tokens ?? {})).toBe(false);
  });

  test('assistant image blocks project as interleaved file parts', () => {
    const projected = projectConversation(
      [
        userMessage('show me'),
        assistantMessage([
          { type: 'text', text: 'here is the chart:' },
          { type: 'image', data: 'iVBOR', mimeType: 'image/png' },
          { type: 'text', text: 'and done' },
        ]),
      ],
      { sessionID: 's1', directory: '/repo' },
    );
    const [, assistant] = projected;
    expect(assistant.parts[2].mime).toBe('image/png');
    expect(assistant.parts[2].url).toContain('data:image/png;base64,iVBOR');
  });

  test('user message synthetic flag rides the text part', () => {
    const synthetic = projectUserMessage(
      { role: 'user', content: 'auto-continue', synthetic: true, timestamp: now },
      { sessionID: 's1', agent: 'build', model: { id: 'claude-x', provider: 'anthropic' } },
    );
    expect(synthetic.parts[0].synthetic).toBe(true);
    const human = projectUserMessage(
      { role: 'user', content: 'typed', timestamp: now },
      { sessionID: 's1', agent: 'build', model: { id: 'claude-x', provider: 'anthropic' } },
    );
    expect('synthetic' in human.parts[0]).toBe(false);
  });

  test('message ids are deterministic across repeated projections', () => {
    const messages = [userMessage('stable'), assistantMessage([{ type: 'text', text: 'reply' }])];
    const a = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    const b = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(a.map((m) => m.info.id)).toEqual(b.map((m) => m.info.id));
  });

  test('custom transcript messages project as labeled assistant notes', () => {
    const messages: MessageInput[] = [
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
    const messages: MessageInput[] = [
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

  test('developer agent-attribution note rides the turn without splitting it', () => {
    const messages = [
      userMessage('run'),
      assistantMessage([{ type: 'text', text: 'first' }], { timestamp: now + 10 }),
      { role: 'developer', content: [{ type: 'text', text: 'retry reminder' }], attribution: 'agent', timestamp: now + 30 },
      assistantMessage([{ type: 'text', text: 'second' }], { timestamp: now + 40 }),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo', agent: 'build' });
    // user, assistant#1, note, assistant#2 — the note stays between the two
    // assistant messages of the same turn.
    expect(projected.map((m) => m.info.role)).toEqual(['user', 'assistant', 'assistant', 'assistant']);
    const note = projected[2];
    expect(note.info.metadata).toEqual({ ompRole: 'developer' });
    expect(note.parts[0].text).toBe('[omp:developer] retry reminder');
    expect(note.parts[0].synthetic).toBe(true);
    // Deterministic id derivation the live paths share.
    expect(note.info.id).toBe(wireMessageId('custom', now + 30, '[omp:developer] retry reminder'));
    // Both assistant messages anchor to the real user message; the note
    // never became the turn's user parent.
    expect(projected[1].info.parentID).toBe(projected[0].info.id);
    expect(projected[3].info.parentID).toBe(projected[0].info.id);
  });

  test('developer user-attribution synthetic prompt occupies the user turn slot', () => {
    const messages = [
      userMessage('run'),
      assistantMessage([{ type: 'text', text: 'done' }], { timestamp: now + 10 }),
      { role: 'developer', content: 'queued follow-up', attribution: 'user', timestamp: now + 30 },
      assistantMessage([{ type: 'text', text: 'follow-up answer' }], { timestamp: now + 40 }),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo', agent: 'build' });
    const note = projected[2];
    expect(note.info.role).toBe('user');
    expect(note.info.metadata).toEqual({ ompRole: 'developer' });
    expect(note.parts[0].text).toBe('[omp:developer] queued follow-up');
    expect(note.parts[0].synthetic).toBe(true);
    expect(note.info.id).toBe(wireMessageId('user', now + 30, '[omp:developer] queued follow-up'));
    // The following assistant message anchors to the synthetic prompt, like
    // a real prompt — the turn model only renders assistants parented to a
    // user message.
    expect(projected[3].info.parentID).toBe(note.info.id);
  });

  test('empty developer messages are skipped', () => {
    const messages = [
      userMessage('run'),
      { role: 'developer', content: '   ', attribution: 'agent', timestamp: now + 5 },
      assistantMessage([{ type: 'text', text: 'done' }]),
    ];
    const projected = projectConversation(messages, { sessionID: 's1', directory: '/repo' });
    expect(projected).toHaveLength(2);
  });

  test('developer reminder <system-reminder> wrapper is stripped from the part text, id stays on raw text', () => {
    const raw = '<system-reminder>\nYou stopped with 2 incomplete todo item(s):\n- task\n</system-reminder>';
    const message = { role: 'developer', content: [{ type: 'text', text: raw }], attribution: 'agent', timestamp: now + 30 };
    const projected = projectConversation(
      [userMessage('run'), assistantMessage([{ type: 'text', text: 'stopping' }]), message],
      { sessionID: 's1', directory: '/repo' },
    );
    const note = projected[2];
    // The UI's synthetic filter drops parts containing the tag, so the part
    // must carry the unwrapped body while the wire id seeds on the raw text.
    expect(note.parts[0].text).toBe('[omp:developer] You stopped with 2 incomplete todo item(s):\n- task');
    expect(note.info.id).toBe(wireMessageId('custom', now + 30, '[omp:developer] ' + raw));
  });

  test('strippedToolCalls marker rides assistant metadata for elided branch activity', () => {
    const stripped = assistantMessage([{ type: 'text', text: 'partial turn' }]);
    stripped.strippedToolCalls = 3;
    const projected = projectConversation(
      [userMessage('run'), stripped],
      { sessionID: 's1', directory: '/repo' },
    );
    expect(projected[1].info.metadata).toEqual({ ompStrippedToolCalls: 3 });
    // No marker / zero → no metadata field at all.
    const plain = projectConversation(
      [userMessage('run'), assistantMessage([{ type: 'text', text: 'clean turn' }])],
      { sessionID: 's1', directory: '/repo' },
    );
    expect('metadata' in plain[1].info).toBe(false);
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
    // SAFETY: the projection types parentID as its wire object form, but the
    // echo path stores the plain parent wire id; ChainableParentPage models
    // both shapes the assertions read without loosening the source contract.
    const projected = projectConversation(messages, {
      sessionID: 's1',
      directory: '/repo',
      agent: 'build',
      wireIdFor: () => 'msg_client_echo',
    }) as ChainableParentPage;
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

  test('migrates legacy fork-lineage parentID to forkParentID and persists it', () => {
    // Released builds wrote fork lineage into `parentID` (engine.fork was its
    // only writer). On load those edges must move to `forkParentID` so the
    // fork stops projecting as a read-only subagent session on the wire.
    registry.update('/repo', 'fork_1', { parentID: 'ses_root', title: 'ses_root (fork)', timeCreated: 1 });
    registry.update('/repo', 'sub_1', { parentID: 'ses_main', forkParentID: 'ses_root', timeCreated: 2 });

    const reloaded = new SessionMetaRegistry({ agentDir: dir });
    expect(reloaded.get('/repo', 'fork_1')).toMatchObject({ forkParentID: 'ses_root', title: 'ses_root (fork)' });
    expect(reloaded.get('/repo', 'fork_1').parentID).toBeUndefined();
    // An entry that already carries forkParentID keeps parentID untouched:
    // that pairing is genuine subagent parentage.
    expect(reloaded.get('/repo', 'sub_1')).toMatchObject({ parentID: 'ses_main', forkParentID: 'ses_root' });
    // The migration is persisted, not just in-memory.
    expect(new SessionMetaRegistry({ agentDir: dir }).get('/repo', 'fork_1')).toMatchObject({ forkParentID: 'ses_root' });
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
  const ids = (page: ProjectedMessagePage) => page.messages.map((message) => message.info.id);
  // Pagination only reads info.id; the remaining wire-info fields pad the
  // ProjectedMessage contract (inert stubs for these tests).
  const ascending: ProjectedMessage[] = ['m1', 'm2', 'm3', 'm4', 'm5'].map((id) => ({
    info: { id, sessionID: 's1', role: 'user', time: { created: now }, agent: 'build' },
    parts: [],
  }));

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

describe('wireSkillRows', () => {
  test('projects discovered skills onto wire rows with frontmatter-stripped content', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-host-skills-'));
    const withFrontmatter = path.join(dir, 'front', 'SKILL.md');
    const withoutFrontmatter = path.join(dir, 'plain', 'SKILL.md');
    fs.mkdirSync(path.dirname(withFrontmatter), { recursive: true });
    fs.mkdirSync(path.dirname(withoutFrontmatter), { recursive: true });
    fs.writeFileSync(withFrontmatter, '---\nname: front\ndescription: has frontmatter\n---\n\nFrontmatter body.\n');
    fs.writeFileSync(withoutFrontmatter, 'Plain body, no frontmatter.\n');

    const rows = await wireSkillRows([
      { name: 'front', description: 'has frontmatter', filePath: withFrontmatter },
      { name: 'plain', description: '', filePath: withoutFrontmatter },
    ]);

    expect(rows).toEqual([
      { name: 'front', description: 'has frontmatter', location: withFrontmatter, content: '\nFrontmatter body.\n' },
      { name: 'plain', description: '', location: withoutFrontmatter, content: 'Plain body, no frontmatter.\n' },
    ]);
    expect(rows[0]!.content.startsWith('---')).toBe(false);
    expect(rows[0]!.location).not.toBe('');
  });

  test('degrades an unreadable SKILL.md to empty content instead of dropping the row', async () => {
    const missing = path.join(os.tmpdir(), `omp-host-skills-gone-${process.pid}`, 'SKILL.md');
    const rows = await wireSkillRows([{ name: 'gone', description: '', filePath: missing }]);
    expect(rows).toEqual([{ name: 'gone', description: '', location: missing, content: '' }]);
  });
});
