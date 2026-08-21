import { describe, test, expect, mock, afterAll } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const agentDir = mkdtempSync(path.join(tmpdir(), 'omp-dispositions-'));
const sessionDir = path.join(agentDir, 'sessions');

const sessionFiles = [{ id: 's1', path: path.join(sessionDir, 's1.jsonl') }];
const listenersBySession = new Map();

const makeFakeSession = (id) => ({
  model: { provider: 'p1', id: 'current-model' },
  isStreaming: false,
  messages: [],
  thinkingLevel: 'high',
  subscribe(listener) {
    listenersBySession.set(id, listener);
    return () => listenersBySession.delete(id);
  },
  sessionManager: { onSessionNameChanged: () => {}, getSessionName: () => undefined },
  getLastAssistantMessage: () => null,
  setModel: async () => ({}),
  maybeStartTitleGeneration: () => {},
  prompt: async () => true,
});

const realSdk = await import('@oh-my-pi/pi-coding-agent');
mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {},
  ModelRegistry: class {
    constructor() {}
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'm1' }];
    }
  },
  SessionManager: Object.assign(
    class {},
    {
      async open(file) {
        return { getSessionId: () => path.basename(file, '.jsonl'), onSessionNameChanged: () => {} };
      },
      async list() {
        return sessionFiles;
      },
      getDefaultSessionDir: () => sessionDir,
    },
  ),
  createAgentSession: async (options) => {
    const id = options.sessionManager?.getSessionId?.() ?? 's1';
    return { session: makeFakeSession(id) };
  },
  Settings: class extends realSdk.Settings {
    static async init() {
      return {
        getCwd: () => 'C:/stub-boot',
        cloneForCwd: async () => ({ getCwd: () => 'C:/stub-boot' }),
      };
    }
  },
  VERSION: 'test',
  discoverAuthStorage: () => ({}),
  BUILTIN_TOOLS: [],
}));

const { OmpHostEngine } = await import('./engine.js');

afterAll(() => {
  rmSync(agentDir, { recursive: true, force: true, maxRetries: 5 });
});

const harness = async () => {
  const engine = new OmpHostEngine({ agentDir });
  const wire = [];
  const omp = [];
  engine.bus.subscribeSince(0, (entry) => wire.push(entry.envelope));
  engine.ompBus.subscribeSince(0, (entry) => omp.push(entry.envelope));
  await engine.prompt({ sessionID: 's1', directory: '/repo', text: 'seed' });
  const emit = (event) => listenersBySession.get('s1')(event);
  return {
    engine,
    emit,
    wire,
    omp,
    wireOf: (type) => wire.filter((e) => e.type === type),
    ompOf: (type) => omp.filter((e) => e.type === type),
  };
};

describe('SDK event dispositions (spec 05 §5.1, master D6-R6)', () => {
  test('auto_retry_start emits wire retry status + omp overlay, zero wire mutation', async () => {
    const h = await harness();
    h.emit({ type: 'agent_start' });
    h.emit({ type: 'message_start', message: { role: 'assistant', content: [], timestamp: 1 } });
    h.emit({
      type: 'message_end',
      message: { role: 'assistant', content: [{ type: 'text', text: 'boom' }], model: 'p1/m1', timestamp: 2, usage: { input: 1, output: 1 } },
    });
    h.emit({ type: 'auto_retry_start', attempt: 2, maxAttempts: 5, delayMs: 1500, errorMessage: 'upstream 500' });

    const status = h.wireOf('session.status').at(-1);
    expect(status.properties.status.type).toBe('retry');
    expect(status.properties.status.attempt).toBe(2);
    expect(status.properties.status.message).toBe('upstream 500');
    expect(typeof status.properties.status.next).toBe('number');

    const started = h.ompOf('omp.retry.started').at(-1);
    expect(started.payload.errorMessage).toBe('upstream 500');
    expect(started.payload.maxAttempts).toBe(5);
    // Superseded overlay points at the just-settled assistant message.
    expect(typeof started.payload.supersededMessageID).toBe('string');
    expect(h.wireOf('message.part.removed')).toHaveLength(0); // P1: producer stays zero (master R14)
  });

  test('auto_retry_end emits wire busy + durable omp.retry.ended with recovery notes', async () => {
    const h = await harness();
    h.emit({
      type: 'auto_retry_end',
      success: false,
      attempt: 3,
      finalError: 'gave up',
      retryErrors: [{ entryId: 'e1', persistenceKey: 'p1', note: 'credential', retryRecovery: { status: 'recovered' } }],
    });
    expect(h.wireOf('session.status').at(-1).properties.status.type).toBe('busy');
    const ended = h.ompOf('omp.retry.ended').at(-1);
    expect(ended.payload.success).toBe(false);
    expect(ended.payload.retryErrors[0]).toEqual({
      messageID: 'p1',
      note: 'credential',
      retryRecovery: { status: 'recovered' },
    });
    expect(engine => engine).toBeTruthy();
    // durable → replayable
    const replayed = [];
    h.engine.ompBus.subscribeSince(0, (entry) => replayed.push(entry.envelope.type));
    expect(replayed).toContain('omp.retry.ended');
  });

  test('agent_end isTerminal=false keeps busy; terminal settles idle', async () => {
    const h = await harness();
    h.emit({ type: 'agent_start' });
    h.emit({ type: 'agent_end', isTerminal: false, messages: [] });
    expect(h.wireOf('session.idle')).toHaveLength(0);
    expect(h.wireOf('session.status').at(-1).properties.status.type).toBe('busy');
    expect(h.ompOf('omp.session.settled').at(-1).payload).toEqual({ isTerminal: false });
    h.emit({ type: 'agent_end', isTerminal: true, messages: [] });
    expect(h.wireOf('session.idle')).toHaveLength(1);
    // Status snapshot must not downgrade while awaiting async delivery.
    const statuses = await h.engine.getSessionStatuses({ directory: '/repo' });
    expect(statuses.s1.type).toBe('idle');
  });

  test('model_changed syncs registry + wire truth + omp event; fallback applies without wire double-emit', async () => {
    const h = await harness();
    const before = h.wireOf('session.updated').length;
    h.emit({ type: 'retry_fallback_applied', from: 'p1/m1', to: 'p1/fallback', role: 'default' });
    expect(h.wireOf('session.updated')).toHaveLength(before); // no wire emit; model_changed owns it
    expect(h.ompOf('omp.fallback.applied').at(-1).payload).toEqual({ from: 'p1/m1', to: 'p1/fallback', role: 'default' });

    h.engine.sessions.get('s1').agentSession.model = { provider: 'p1', id: 'fallback' };
    h.emit({ type: 'model_changed' });
    expect(h.wireOf('session.updated').length).toBe(before + 1);
    const changed = h.ompOf('omp.model.changed').at(-1);
    expect(changed.payload.model).toEqual({ provider: 'p1', id: 'fallback' });
    expect(changed.payload.thinkingLevel).toBe('high');
    expect(h.engine.registry.get('/repo', 's1').model).toBe('p1/fallback');

    h.emit({ type: 'retry_fallback_succeeded', model: 'p1/m1', role: 'default' });
    expect(h.ompOf('omp.fallback.succeeded').at(-1).payload).toEqual({ model: 'p1/m1', role: 'default' });
  });

  test('notice keeps console.error for error level and emits omp toast channel', async () => {
    const h = await harness();
    const errors = [];
    const original = console.error;
    console.error = (...args) => errors.push(args.join(' '));
    try {
      h.emit({ type: 'notice', level: 'error', message: 'boom' });
      h.emit({ type: 'notice', level: 'info', message: 'fyi', source: 'hub' });
    } finally {
      console.error = original;
    }
    expect(errors.length).toBe(1);
    const raised = h.ompOf('omp.notice.raised');
    expect(raised).toHaveLength(2);
    expect(raised[0].payload).toEqual({ level: 'error', message: 'boom' });
    expect(raised[1].payload).toEqual({ level: 'info', message: 'fyi', source: 'hub' });
  });

  test('todo_auto_clear emits an empty todo.updated', async () => {
    const h = await harness();
    h.emit({ type: 'todo_auto_clear' });
    const todos = h.wireOf('todo.updated');
    expect(todos.at(-1).properties.todos).toEqual([]);
  });

  test('irc_message projects a wire card and structured omp payload; display:false stays cardless', async () => {
    const h = await harness();
    h.emit({
      type: 'irc_message',
      message: {
        role: 'custom',
        customType: 'irc:incoming',
        content: [{ type: 'text', text: '<irc> hello there' }],
        display: true,
        details: { channel: '#ops' },
        timestamp: 5,
      },
    });
    expect(h.wireOf('message.updated').length).toBeGreaterThan(0);
    const appended = h.ompOf('omp.custom.appended').at(-1);
    expect(appended.payload.message.customType).toBe('irc:incoming');
    expect(appended.payload.message.display).toBe(true);
    expect(appended.payload.message.details).toEqual({ channel: '#ops' });
    const wireBefore = h.wireOf('message.updated').length;

    h.emit({
      type: 'irc_message',
      message: {
        role: 'custom',
        customType: 'ultrathink-notice',
        content: [{ type: 'text', text: 'hidden prelude' }],
        display: false,
        timestamp: 6,
      },
    });
    expect(h.wireOf('message.updated')).toHaveLength(wireBefore); // no card for hidden types (T3)
    expect(h.ompOf('omp.custom.appended').at(-1).payload.message.display).toBe(false);
  });

  test('thinking/goal/ttsr/compaction dispositions emit their registered omp events', async () => {
    const h = await harness();
    h.emit({ type: 'thinking_level_changed', thinkingLevel: 'medium', configured: 'auto', resolved: 'high' });
    h.emit({ type: 'goal_updated', goal: { text: 'ship it' }, state: { active: true } });
    h.emit({ type: 'ttsr_triggered', rules: [{ name: 'no-secrets' }] });
    h.emit({ type: 'auto_compaction_start', reason: 'threshold', action: 'context-full' });
    h.emit({ type: 'auto_compaction_end', action: 'context-full', result: undefined, aborted: false, willRetry: false });

    expect(h.ompOf('omp.thinking.changed').at(-1).payload).toEqual({
      thinkingLevel: 'medium', configured: 'auto', resolved: 'high',
    });
    expect(h.ompOf('omp.goal.updated').at(-1).payload).toEqual({
      goal: { text: 'ship it' }, state: { active: true },
    });
    expect(h.ompOf('omp.ttsr.triggered').at(-1).payload).toEqual({ rules: [{ name: 'no-secrets' }] });
    expect(h.ompOf('omp.compaction.started').at(-1).payload).toEqual({ reason: 'threshold', action: 'context-full' });
    const ended = h.ompOf('omp.compaction.ended').at(-1).payload;
    expect(ended.action).toBe('context-full');
    expect(ended.aborted).toBe(false);
  });

  test('message_end publishes per-turn usage telemetry', async () => {
    const h = await harness();
    h.emit({ type: 'message_start', message: { role: 'assistant', content: [], timestamp: 10 } });
    h.emit({
      type: 'message_end',
      message: {
        role: 'assistant',
        content: [{ type: 'text', text: 'answer' }],
        model: 'p1/m1',
        timestamp: 11,
        usage: { input: 120, output: 60, cacheRead: 10, cacheWrite: 5 },
        ttft: 250,
        duration: 1800,
      },
    });
    const usage = h.ompOf('omp.usage.turn').at(-1);
    expect(usage.payload.usage.input).toBe(120);
    expect(usage.payload.ttftMs).toBe(250);
    expect(usage.payload.durationMs).toBe(1800);
    expect(typeof usage.payload.messageID).toBe('string');
  });

  test('tool_execution_update appends partial output without a terminal state', async () => {
    const h = await harness();
    h.emit({ type: 'message_start', message: { role: 'assistant', content: [], timestamp: 20 } });
    h.emit({ type: 'tool_execution_start', toolCallId: 'c1', toolName: 'longjob', args: {} });
    h.emit({ type: 'tool_execution_update', toolCallId: 'c1', toolName: 'longjob', args: {}, partialResult: { text: 'half', details: { async: { state: 'running' } } } });
    const parts = h.wireOf('message.part.updated');
    const toolPart = parts.map((e) => e.properties.part).filter((p) => p.type === 'tool').at(-1);
    expect(toolPart.state.status).toBe('running');
    expect(toolPart.state.output).toBe('half');
    expect(toolPart.state.metadata.asyncState).toBe('running');
    h.emit({ type: 'tool_execution_end', toolCallId: 'c1', toolName: 'longjob', result: 'done', isError: false });
    const settled = h.wireOf('message.part.updated').map((e) => e.properties.part).filter((p) => p.type === 'tool').at(-1);
    expect(settled.state.status).toBe('completed');
  });

  test('tool_execution_end normalizes SDK AgentToolResult so ask details reach the transcript', async () => {
    const h = await harness();
    h.emit({ type: 'message_start', message: { role: 'assistant', content: [], timestamp: 30 } });
    h.emit({ type: 'tool_execution_start', toolCallId: 'a1', toolName: 'ask', args: { questions: [] } });
    const askDetails = { question: 'Ship it?', options: ['Yes', 'No'], multi: false, selectedOptions: ['Yes'], timedOut: true };
    h.emit({
      type: 'tool_execution_end',
      toolCallId: 'a1',
      toolName: 'ask',
      result: { content: [{ type: 'text', text: 'User answers:\nYes' }], details: askDetails },
      isError: false,
    });
    const toolParts = () => h.wireOf('message.part.updated').map((e) => e.properties.part).filter((p) => p.type === 'tool');
    const transient = toolParts().at(-1);
    expect(transient.state.status).toBe('completed');
    expect(transient.state.output).toBe('User answers:\nYes');
    expect(transient.state.metadata.details).toEqual(askDetails);

    h.emit({
      type: 'message_end',
      message: {
        role: 'assistant',
        content: [{ type: 'toolCall', id: 'a1', name: 'ask', arguments: { questions: [] } }],
        model: 'p1/m1',
        timestamp: 31,
        usage: {},
      },
    });
    const final = toolParts().at(-1);
    expect(final.state.output).toBe('User answers:\nYes');
    expect(final.state.metadata.details).toEqual(askDetails);
  });

  test('turn_start/turn_end are explicit intentional ignores; unknown members fail loudly', async () => {
    const h = await harness();
    h.emit({ type: 'turn_start' });
    h.emit({ type: 'turn_end', message: { role: 'assistant', content: [] }, toolResults: [] });
    const errors = [];
    const original = console.error;
    console.error = (...args) => errors.push(args.join(' '));
    try {
      h.emit({ type: 'something_new' });
    } finally {
      console.error = original;
    }
    expect(errors.join('\n')).toMatch(/unhandled AgentSessionEvent type: something_new/);
    expect(h.engine.unknownEventCounts.get('something_new')).toBe(1);
  });

  test('tail-sync projects unannounced dividers on terminal agent_end', async () => {
    const h = await harness();
    const live = h.engine.sessions.get('s1');
    live.agentSession.messages.push(
      { role: 'user', content: 'hi', timestamp: 30 },
      { role: 'assistant', content: [{ type: 'text', text: 'ok' }], model: 'p1/m1', timestamp: 31 },
      { role: 'compactionSummary', summary: 'post-turn compaction', tokensBefore: 5000, timestamp: 32 },
    );
    const before = h.wireOf('message.updated').length;
    h.emit({ type: 'agent_start' });
    h.emit({ type: 'agent_end', messages: [] });
    const wireMessages = h.wireOf('message.updated').slice(before);
    const texts = wireMessages.map((e) => e.properties.info);
    expect(texts.some((info) => info?.metadata?.ompRole === 'compactionSummary')).toBe(true);
    // Idempotent: a second agent_end does not re-emit the divider.
    const count = h.wireOf('message.updated').length;
    h.emit({ type: 'agent_start' });
    h.emit({ type: 'agent_end', messages: [] });
    expect(h.wireOf('message.updated').length).toBe(count);
  });
});
