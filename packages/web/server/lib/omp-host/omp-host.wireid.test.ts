/**
 * Wire-ID 契约矩阵测试（docs/plan.md phase 5 验收）：长会话 id 稳定性、
 * 跨目录同 sessionID、冷/热/重连 id 一致与多客户端乐观回显。稳定公式使
 * 已定稿 assistant 消息的 live id 与任何冷重投影按构造相等，常规路径
 * assistant 回显映射保持为空；只有客户端回显的 user id 与 SDK 偶发的时间
 * 戳改写漂移会留下记录。SDK 与 engine 模块在 mock.module 登记之后动态
 * 加载（接缝同 omp-host.engine.test.ts）。
 */
import { describe, expect, mock, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { OmpHostEngine } from './engine.ts';
import { deterministicWireId, wireMessageId } from './projection.ts';

// Wire-ID contract matrix (docs/plan.md phase 5 acceptance): long-session
// stability, cross-directory same sessionID, cold/live/reconnect id
// agreement, and multi-client optimistic echoes. The stable formula makes a
// settled assistant message's live id identical to every cold re-projection
// by construction, so the echo map stays empty for assistant messages on the
// normal path; only client-echoed user ids and the SDK's rare
// timestamp-rewrite drift record entries.
//
// The SDK and engine modules load dynamically AFTER mock.module registration
// below — a static import would bind engine.ts to the real SDK graph before
// the mocks exist (same seam as omp-host.engine.test.ts).
// 真实 SDK 面：mock 展开在其上，仅替换被桩成员。
const realSdk = await import('@oh-my-pi/pi-coding-agent');
// 真实 runtime-init 面：仅替换 initializeExtensions。
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');

/** Fixture transcript 消息——投影函数只读的字段子集。 */
/** Fixture transcript message — the subset the projections read. */
interface FixtureMessage {
  // 消息角色。
  role: string;
  // 毫秒时间戳（wire id 公式的输入之一）。
  timestamp: number;
  // 内容部件数组（文本/工具调用等）。
  content: Array<{ type: string; text?: string; name?: string }>;
  // 可选模型 id。
  model?: string;
  // 可选 provider id。
  provider?: string;
  // 可选用量（input/output）。
  usage?: { input: number; output: number };
  // 可选停止原因。
  stopReason?: string;
}

/** 本套件发出的 AgentSessionEvent 成员——harness 契约。 */
/** The AgentSessionEvent members these tests emit — the harness contract. */
interface FixtureEvent {
  // 事件类型。
  type: string;
  // 可选：事件携带的消息。
  message?: FixtureMessage;
  // 可选：流式增量事件。
  assistantMessageEvent?: { type: string; delta: string };
}

/** mock 会话文件记录：wire id、磁盘路径与所属 cwd。 */
interface SessionFile {
  // wire 会话 id（可与磁盘文件名不同）。
  id: string;
  // .jsonl 磁盘路径。
  path: string;
  // 会话工作目录（list 的过滤键）。
  cwd: string;
}

/** 顶层模块 mock 闭包引用的 harness 状态。 */
interface WireIdMockState {
  // 当前 agent 根目录。
  agentDir: string;
  // 已登记的会话文件。
  files: SessionFile[];
  // 按文件路径持久化的 transcript 模型（buildSessionContext 分支）。
  /** Per-file persisted transcript model (buildSessionContext arm). */
  transcript: Map<string, FixtureMessage[]>;
}

/** 顶层 mock 状态单例（createHarness 每次整体重置）。 */
const mockState: WireIdMockState = {
  agentDir: '',
  files: [],
  transcript: new Map(),
};


mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async () => {},
}));

mock.module('@oh-my-pi/pi-coding-agent', () => ({
  ...realSdk,
  AgentRegistry: class {},
  ModelRegistry: class {
    async refresh() {}
    getAvailable() {
      return [{ provider: 'p1', id: 'm1' }];
    }
  },
  SessionManager: Object.assign(
    class {},
    {
      async open(file: string) {
        const record = mockState.files.find((candidate) => candidate.path === file);
        const id = record?.id ?? path.basename(file, '.jsonl');
        return {
          getSessionId: () => id,
          onSessionNameChanged: () => () => {},
          getHeader: (): null => null,
          getEntries: () => [],
          buildSessionContext: () => ({ messages: mockState.transcript.get(file) ?? [] }),
          getCwd: () => undefined,
          getSessionName: () => undefined,
          getArtifactsDir: () => file.slice(0, -'.jsonl'.length),
          close: async () => {},
          moveTo: async () => {},
        };
      },
      async list(cwd?: string) {
        return mockState.files
          .filter((file) => !cwd || file.cwd === cwd)
          .map((file) => ({ id: file.id, path: file.path }));
      },
      getDefaultSessionDir: () => path.join(mockState.agentDir, 'sessions'),
      async forkFrom() {
        throw new Error('not needed');
      },
      createEmptySessionFile: () => 'unused',
    },
  ),
  createAgentSession: async () => {
    throw new Error('injected factory must be used');
  },
}));

/** 断言读取的 wire 行最小形状：事件类型 + info.id/role。 */
interface WireEventRow {
  // wire 事件类型。
  type: string;
  // 投影消息 info 的可断言子集。
  info: { id?: string; role?: string } | undefined;
}

/** 测试操作面：引擎、假时钟、事件注入、transcript 建模与 wire 行捕获。 */
interface Harness {
  // 被测引擎实例。
  engine: OmpHostEngine;
  // 可手动推进的时间源（now 毫秒）。
  clock: { now: number };
  // 向某目录的 live 订阅注入一条 SDK AgentSessionEvent。
  /** Emit an SDK AgentSessionEvent into one directory's live subscription. */
  emit: (directory: string, event: FixtureEvent) => void;
  // 建模持久化 transcript：JSONL 文件、live 列表与冷上下文三处同步。
  /** Model the persisted transcript: JSONL file + live list + cold context. */
  persist: (directory: string, messages: FixtureMessage[]) => void;
  // 捕获到的 wire 事件行。
  wireRows: () => WireEventRow[];
}

/** 逐出 disposal 经微任务加注入的 drain 预算落定；与 lifecycle 套件同款
 * 短真实等待（注册表无公开完成 Promise）。 */
/** Eviction disposal settles through microtasks plus the injected drain
 * budget; the module's lifecycle tests use the same short real wait because
 * the registry exposes no public completion promise. */
const settle = (ms = 20) => {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
};

/** 组装隔离 harness：临时 agentDir、按 cwd 键控的 live 事件处理器/消息
 * 列表、注入 agent 工厂的引擎，以及捕获全部 wire 行的订阅。 */
const createHarness = async ({
  files: extraFiles,
}: {
  files?: Array<{ id: string; cwd: string }>;
} = {}): Promise<Harness> => {
  const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-wireid-'));
  const clock = { now: 0 };
  // Handlers/live lists key by cwd: cross-directory fixtures share the
  // sessionID, so the session id alone cannot route events.
  const handlers = new Map<string, Array<(event: FixtureEvent) => void>>();
  const liveLists = new Map<string, FixtureMessage[]>();
  mockState.agentDir = agentDir;
  mockState.files = [];
  mockState.transcript = new Map();

  for (const file of extraFiles ?? [{ id: 's1', cwd: '/repo' }]) {
    // Cross-directory fixtures share the sessionID: the on-disk name stays
    // unique while `list` reports the wire id.
    const filePath = path.join(agentDir, 'sessions', `${file.cwd.replace(/[^a-z0-9]/gi, '_')}_${file.id}.jsonl`);
    mockState.files.push({ id: file.id, path: filePath, cwd: file.cwd });
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(filePath, '');
  }

  const { OmpHostEngine: Engine } = await import('./engine.ts');
  const engine = new Engine({
    agentDir,
    now: () => clock.now,
    // SAFETY: the engine option expects the SDK factory signature; the
    // harness supplies a duck-typed double the engine only calls.
    createAgentSession: (async (options: { cwd: string; sessionManager: { getSessionId?: () => string } }) => {
      const id = options.sessionManager.getSessionId?.() ?? 's1';
      const dir = options.cwd ?? '/repo';
      const record = mockState.files.find((file) => file.id === id && file.cwd === dir)
        ?? mockState.files.find((file) => file.id === id);
      const messages: FixtureMessage[] = [];
      liveLists.set(record?.cwd ?? dir, messages);
      return {
        // SAFETY: fixture is a duck-typed double, not the SDK session.
        session: {
          model: { provider: 'p1', id: 'm1' },
          messages,
          isStreaming: false,
          isAborting: false,
          isRetrying: false,
          isCompacting: false,
          isGeneratingHandoff: false,
          isBashRunning: false,
          isEvalRunning: false,
          hasPendingBashMessages: false,
          hasPendingPythonMessages: false,
          hasPostPromptWork: false,
          queuedMessageCount: 0,
          hasPendingAsyncWork: () => false,
          beginDispose: () => {},
          dispose: async () => {},
          prompt: mock(async () => true),
          abort: async () => {},
          maybeStartTitleGeneration: () => {},
          subscribe: (handler: (event: FixtureEvent) => void) => {
            const key = record?.cwd ?? dir;
            const list = handlers.get(key) ?? [];
            list.push(handler);
            handlers.set(key, list);
            return () => {
              const current = handlers.get(key) ?? [];
              const at = current.indexOf(handler);
              if (at >= 0) current.splice(at, 1);
            };
          },
          sessionManager: {
            getSessionName: () => `name-${id}`,
            getArtifactsDir: () => path.join(agentDir, id),
            onSessionNameChanged: () => () => {},
          },
        } as never,
        setToolUIContext: () => {},
      };
    }) as never,
  });

  const rows: WireEventRow[] = [];
  engine.bus.subscribeSince(0, (entry) => {
    // The bus envelope is the generic BusEnvelope; `message.updated`'s info
    // member is the only field these assertions read.
    // SAFETY: harness-owned read of the wire bus envelope; every row the
    // engine emits for these tests carries `properties.info` or nothing.
    const envelope = entry.envelope as { type: string; properties: { info?: { id?: string; role?: string } } };
    rows.push({ type: envelope.type, info: envelope.properties?.info });
  });

  return {
    engine,
    clock,
    emit: (directory, event) => {
      for (const handler of handlers.get(directory) ?? []) handler(event);
    },
    persist: (directory, messages) => {
      const file = mockState.files.find((candidate) => candidate.cwd === directory);
      if (!file) return;
      mockState.transcript.set(file.path, messages);
      const live = liveLists.get(directory);
      if (live) live.splice(0, live.length, ...messages);
      // The windowed cold arm parses the real JSONL file, so model the
      // persisted transcript there too (session header + message entries,
      // the same shape omp-host.coldpage.test fixtures write).
      const lines = [
        { type: 'session', version: 3, id: file.id, timestamp: new Date(1_000).toISOString(), cwd: agentDir },
        ...messages.map((message, index) => ({
          type: 'message',
          id: `e${index + 1}`,
          parentId: null,
          timestamp: new Date(message.timestamp).toISOString(),
          message,
        })),
      ];
      fs.writeFileSync(file.path, `${lines.map((line) => JSON.stringify(line)).join('\n')}\n`);
    },
    wireRows: () => rows,
  };
};

/** 取最近一条流式 assistant 消息的 wire id（无则空串）。 */
const lastStreamedAssistantId = (h: Harness) => {
  const rows = h.wireRows().filter((row) => row.type === 'message.updated' && row.info?.role === 'assistant');
  return rows.at(-1)?.info?.id ?? '';
};

/** 拉取某目录的会话消息页并按角色过滤。 */
const roleRows = async (h: Harness, role: string, directory = '/repo') => {
  const page = await h.engine.getMessagesPage({ sessionID: 's1', directory });
  return (page?.messages ?? []).filter((message) => message.info.role === role);
};

/** wire id 契约（docs/plan.md phase 5）：长会话冷热 id 一致、跨目录同
 * sessionID 互不串扰、逐出/重连后 id 稳定，以及乐观回显与漂移记录边界。 */
describe('wire id contract (docs/plan.md phase 5)', () => {
  test('long session: settled assistant ids equal cold re-projection; only user prompts record echoes', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'go', messageID: 'msg_client_1' });
    h.emit('/repo', { type: 'message_start', message: { role: 'user', content: [{ type: 'text', text: 'go' }], timestamp: 1_000 } });

    const settled: FixtureMessage[] = [];
    for (let turn = 0; turn < 50; turn += 1) {
      const ts = 2_000 + turn;
      h.emit('/repo', { type: 'message_start', message: { role: 'assistant', content: [], timestamp: ts } });
      h.emit('/repo', { type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: `answer ${turn}` } });
      const final: FixtureMessage = {
        role: 'assistant',
        content: [{ type: 'text', text: `answer ${turn} with more text` }],
        provider: 'p1',
        model: 'm1',
        timestamp: ts,
        usage: { input: 1, output: 1 },
        stopReason: 'stop',
      };
      settled.push(final);
      h.emit('/repo', { type: 'message_end', message: final });
    }
    h.persist('/repo', settled);

    const echoes = h.engine.liveRecord('s1', '/repo')?.payload?.wireIdEchoes;
    expect(echoes).toBeDefined();
    // One client-echoed user entry; ZERO assistant entries on the normal
    // path — the stable formula removes the per-turn assistant bridge.
    expect(echoes?.size).toBe(1);
    for (const live of echoes?.values() ?? []) expect(live).toBe('msg_client_1');

    // Every streamed assistant id equals the deterministic cold id.
    const projected = await roleRows(h, 'assistant');
    expect(projected.length).toBe(50);
    for (const message of projected) {
      const match = settled.find((row) => row.timestamp === message.info.time.created);
      expect(match).toBeDefined();
      if (!match) continue;
      expect(message.info.id).toBe(deterministicWireId(match));
    }
  });

  test('cross-directory same sessionID: echoes resolve only inside their own directory', async () => {
    const h = await createHarness({ files: [{ id: 's1', cwd: '/repoA' }, { id: 's1', cwd: '/repoB' }] });
    await h.engine.prompt({ sessionID: 's1', directory: '/repoA', text: 'from A', messageID: 'msg_client_A' });
    await h.engine.prompt({ sessionID: 's1', directory: '/repoB', text: 'from B', messageID: 'msg_client_B' });
    // Only A's user message_start arrived, so only A holds an echo entry.
    h.emit('/repoA', { type: 'message_start', message: { role: 'user', content: [{ type: 'text', text: 'from A' }], timestamp: 5_000 } });
    h.persist('/repoA', [{ role: 'user', timestamp: 5_000, content: [{ type: 'text', text: 'from A' }] }]);
    h.persist('/repoB', [{ role: 'user', timestamp: 6_000, content: [{ type: 'text', text: 'from B' }] }]);

    const userA = await roleRows(h, 'user', '/repoA');
    expect(userA.at(0)?.info.id).toBe('msg_client_A');

    const userB = await roleRows(h, 'user', '/repoB');
    // B never echoed: canonical formula id, and never A's client id.
    expect(userB.at(0)?.info.id).toBe(wireMessageId('user', 6_000, 'from B'));
    expect(userB.at(0)?.info.id).not.toBe('msg_client_A');
  });

  test('reconnect/evict: assistant ids stay identical from the cold arm; user echoes fall back to canonical', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'go', messageID: 'msg_client_1' });
    h.emit('/repo', { type: 'message_start', message: { role: 'user', content: [{ type: 'text', text: 'go' }], timestamp: 1_000 } });
    h.emit('/repo', { type: 'message_start', message: { role: 'assistant', content: [], timestamp: 2_000 } });
    const final: FixtureMessage = {
      role: 'assistant',
      content: [{ type: 'text', text: 'done' }],
      provider: 'p1',
      model: 'm1',
      timestamp: 2_000,
      usage: { input: 1, output: 1 },
      stopReason: 'stop',
    };
    h.emit('/repo', { type: 'message_end', message: final });
    const liveAssistantId = lastStreamedAssistantId(h);
    expect(liveAssistantId).toBe(deterministicWireId(final));
    h.persist('/repo', [
      { role: 'user', timestamp: 1_000, content: [{ type: 'text', text: 'go' }] },
      final,
    ]);

    // Idle eviction drops the record (and its echo entries with it).
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1', '/repo')).toBeNull();

    // Cold arm: the assistant id is STILL the streamed id (stable formula);
    // the user echo is gone, so the canonical formula id returns.
    const coldAssistant = await roleRows(h, 'assistant');
    expect(coldAssistant.at(0)?.info.id).toBe(liveAssistantId);
    const coldUser = await roleRows(h, 'user');
    expect(coldUser.at(0)?.info.id).toBe(wireMessageId('user', 1_000, 'go'));
  });

  test('multi-client optimistic prompts each keep their own echo entry', async () => {
    const h = await createHarness();
    const clients = ['msg_client_one', 'msg_client_two'];
    for (const [index, client] of clients.entries()) {
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: `ask ${index}`, messageID: client });
      h.emit('/repo', { type: 'message_start', message: { role: 'user', content: [{ type: 'text', text: `ask ${index}` }], timestamp: 10_000 + index } });
    }
    const echoes = h.engine.liveRecord('s1', '/repo')?.payload?.wireIdEchoes;
    expect(echoes?.size).toBe(2);
    expect(echoes?.get(wireMessageId('user', 10_000, 'ask 0'))).toBe('msg_client_one');
    expect(echoes?.get(wireMessageId('user', 10_001, 'ask 1'))).toBe('msg_client_two');
    expect(h.engine.getStreamDiagnostics().dataProportional.wireIdEchoes).toBe(2);
  });

  test('SDK timestamp-rewrite drift records exactly one assistant echo resolving cold to live', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'go' });
    h.emit('/repo', { type: 'message_start', message: { role: 'user', content: [{ type: 'text', text: 'go' }], timestamp: 1_000 } });
    h.emit('/repo', { type: 'message_start', message: { role: 'assistant', content: [], timestamp: 2_000 } });
    const liveId = lastStreamedAssistantId(h);
    // The SDK's error-normalization reset rewrote the timestamp in place.
    const rewritten: FixtureMessage = {
      role: 'assistant',
      content: [],
      provider: 'p1',
      model: 'm1',
      timestamp: 9_000,
      usage: { input: 0, output: 0 },
      stopReason: 'stop',
    };
    h.emit('/repo', { type: 'message_end', message: rewritten });
    h.persist('/repo', [rewritten]);

    const echoes = h.engine.liveRecord('s1', '/repo')?.payload?.wireIdEchoes;
    // No user echo (no messageID), one drift entry: cold(9_000) → live(2_000).
    expect(echoes?.size).toBe(1);
    expect(echoes?.get(deterministicWireId(rewritten))).toBe(liveId);

    // The page projection still shows the id the client already saw.
    const projected = await roleRows(h, 'assistant');
    expect(projected.at(0)?.info.id).toBe(liveId);
  });
});
