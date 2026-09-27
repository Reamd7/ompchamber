/**
 * LiveSession 生命周期矩阵测试（docs/plan.md §3.2-3.4/§4、验收 §10.1
 * 「live 生命周期」）：目录键、逐出状态机、等待式 disposal、隔离墓碑、
 * 活动守卫与 delete/move/shutdown 串行化。引擎注入假单调时钟与 agent
 * 工厂；SessionManager 以 mock.module 打桩（接缝同 omp-host.engine.test），
 * 引擎值在 mock.module 之后动态加载，以绑定桩化的 SDK 面。
 */
import { describe, expect, mock, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import type { OmpHostEngine } from './engine.ts';

// Live-session lifecycle matrix (docs/plan.md §3.2-3.4/§4, acceptance §10.1
// "live 生命周期"): directory keys, eviction state machine, awaited
// disposal, quarantine tombstones, activity guards, delete/move/shutdown
// serialization. The engine gets a fake monotonic clock and an injected
// agent factory; SessionManager is module-mocked like omp-host.engine.test.
// The engine VALUE loads dynamically after mock.module so it binds the
// mocked SDK surface (same seam as omp-host.engine.test.ts).
// 真实 SDK 面：mock 展开在其上，仅替换被桩成员。
const realSdk = await import('@oh-my-pi/pi-coding-agent');
// 真实 runtime-init 面：仅替换 initializeExtensions。
const realRuntimeInit = await import('@oh-my-pi/pi-coding-agent/modes/runtime-init');

// Per-harness state the top-level module mocks close over (registered before
// any engine import so bun's mock.module applies to engine.ts's bindings).
/** 顶层模块 mock 闭包引用的逐 harness 状态（在任何 engine import 之前
 * 登记好，bun 的 mock.module 才能命中 engine.ts 的绑定）。 */
interface LifecycleMockState {
  // 当前 harness 的 agent 根目录（getDefaultSessionDir 用）。
  agentDir: string;
  // SessionManager.list 返回的假会话文件清单。
  files: Array<{ path: string; cwd: string }>;
}
/** 顶层 mock 状态单例（createHarness 每次整体重置）。 */
const mockState: LifecycleMockState = {
  agentDir: '',
  files: [],
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
        return {
          getSessionId: () => path.basename(file, '.jsonl'),
          onSessionNameChanged: () => () => {},
          getHeader: (): null => null,
          getEntries: () => [],
          buildSessionContext: () => ({ messages: [] }),
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
          .map((file) => ({ id: path.basename(file.path, '.jsonl'), path: file.path }));
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
mock.module('@oh-my-pi/pi-coding-agent/modes/runtime-init', () => ({
  ...realRuntimeInit,
  initializeExtensions: async () => {},
}));

/** 引擎所需会话替身的鸭子类型：本套件断言触及的全部状态与行为面。 */
interface FixtureSession {
  // 名字/artifacts 目录查询与改名回调。
  sessionManager: {
    getSessionName: () => string;
    getArtifactsDir: () => string;
    onSessionNameChanged: (cb: () => void) => (() => void) | undefined;
  };
  // 当前模型（provider/id）。
  model: { provider: string; id: string };
  // 会话消息列表（本套件不填充内容）。
  messages: unknown[];
  // 是否正在流式生成。
  isStreaming: boolean;
  // 是否正在中止。
  isAborting: boolean;
  // 是否正在自动重试。
  isRetrying: boolean;
  // 是否正在压缩上下文。
  isCompacting: boolean;
  // 是否正在生成 handoff。
  isGeneratingHandoff: boolean;
  // 是否有 bash 执行中。
  isBashRunning: boolean;
  // 是否有 eval 执行中。
  isEvalRunning: boolean;
  // 是否有待决 bash 消息。
  hasPendingBashMessages: boolean;
  // 是否有待决 Python 消息。
  hasPendingPythonMessages: boolean;
  // prompt 之后是否仍有未完工作。
  hasPostPromptWork: boolean;
  // 排队消息数。
  queuedMessageCount: number;
  // 事件订阅（替身返回空注销函数）。
  subscribe: () => () => void;
  // 异步工作是否未清空（disposal 等待依据）。
  hasPendingAsyncWork: () => boolean;
  // 开始销毁的标记回调。
  beginDispose: () => void;
  // 等待式销毁（可带 drain 超时）。
  dispose: (options?: { drainTimeoutMs?: number }) => Promise<void>;
  // 提交一条 prompt。
  prompt: () => Promise<boolean>;
  // 中止当前生成。
  abort: () => Promise<void>;
  // 标题生成触发钩子。
  maybeStartTitleGeneration: () => void;
}

/** 构造一个默认全静默的会话替身（全部状态字段取安全缺省值）。 */
const makeFixtureSession = (id: string, dir: string): FixtureSession => ({
  sessionManager: {
    getSessionName: () => `name-${id}`,
    getArtifactsDir: () => path.join(dir, id),
    onSessionNameChanged: () => () => {},
  },
  model: { provider: 'p1', id: 'm1' },
  messages: [],
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
  subscribe: () => () => {},
  hasPendingAsyncWork: () => false,
  beginDispose: () => {},
  dispose: async () => {},
  prompt: async () => true,
  abort: async () => {},
  maybeStartTitleGeneration: () => {},
});

/** 单个 harness 的测试操作面：引擎、假时钟、替身会话表与真实落盘路径。 */
interface Harness {
  // 被测引擎实例。
  engine: OmpHostEngine;
  // 可手动推进的时间源（now 毫秒）。
  clock: { now: number };
  // 按 sessionID 索引的替身会话。
  sessions: Map<string, FixtureSession>;
  // 临时 agent 根目录。
  agentDir: string;
  // 实际写盘的 .jsonl 路径（清理与断言用）。
  realPaths: string[];
}

/** 组装隔离 harness：临时 agentDir、假会话文件、假时钟，以及注入 agent
 * 工厂与短真实时钟界限的引擎；sessionOverrides 可按 id 覆写替身行为。 */
const createHarness = async ({
  sessionOverrides,
  files: extraFiles,
}: {
  sessionOverrides?: Record<string, Partial<FixtureSession>>;
  files?: Array<{ id: string; cwd: string }>;
} = {}): Promise<Harness> => {
  const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-lifecycle-'));
  const clock = { now: 0 };
  const sessions = new Map<string, FixtureSession>();
  const realPaths: string[] = [];
  mockState.agentDir = agentDir;
  mockState.files = [];

  for (const file of extraFiles ?? [{ id: 's1', cwd: '/repo' }]) {
    const filePath = path.join(agentDir, 'sessions', `${file.id}.jsonl`);
    mockState.files.push({ path: filePath, cwd: file.cwd });
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(filePath, '');
    realPaths.push(filePath);
  }

  // Dynamic after the module mocks above (see header comment).
  const { OmpHostEngine: Engine } = await import('./engine.ts');
  const engine = new Engine({
    agentDir,
    now: () => clock.now,
    // Small real-clock bounds: the hung-disposal tests exercise bounded
    // waits that must complete well inside the default test timeout.
    evictDrainTimeoutMs: 150,
    shutdownDisposeDeadlineMs: 200,
    // SAFETY: the engine option expects the SDK factory signature; the
    // harness supplies a duck-typed double the engine only calls.
    createAgentSession: (async (options: { cwd: string; sessionManager: { getSessionId?: () => string } }) => {
      const id = options.sessionManager.getSessionId?.() ?? 's1';
      const fixture = { ...makeFixtureSession(id, options.cwd), ...(sessionOverrides?.[id] ?? {}) };
      sessions.set(id, fixture);
      return {
        // SAFETY: fixture is a FixtureSession double, not the SDK session.
        session: fixture as never,
        setToolUIContext: () => {},
      };
    }) as never,
  });

  return { engine, clock, sessions, agentDir, realPaths };
};

/** 等待真实毫秒让逐出/disposal 的异步清场落定（注册表无公开完成 Promise）。 */
const settle = (ms = 20) => new Promise((resolve) => setTimeout(resolve, ms));

/** LiveSessionRegistry 生命周期（plan §3.2-3.4）：空闲 TTL 逐出、UI 租约
 * 与各类活动信号的逐出否决、disposal 时序与失败隔离墓碑、目录隔离，以及
 * delete/move/shutdown 的串行化语义。 */
describe('LiveSessionRegistry lifecycle (plan §3.2-3.4)', () => {
  test('idle TTL evicts a single live session — no live-count gate', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.clock.now += 29 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.clock.now += 2 * 60_000; // past the 30-minute TTL
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('UI lease holders and pending dialogs veto eviction', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    h.engine.dialogs.leases.release({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.engine.dialogs.registry.register({
      directory: '/repo',
      sessionId: 's1',
      kind: 'approval',
      payload: { approval: { prompt: 'ok?' } },
    });
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('lease heartbeats refresh the idle TTL — eviction counts from the last acquire (plan §4.2)', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });

    // Stay attached far past the TTL: every acquire (attach or heartbeat
    // renew) refreshes lastUsedAt, so the window restarts while viewed.
    h.clock.now += 45 * 60_000;
    h.engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.clock.now += 10_000;
    h.engine.dialogs.leases.acquire({ directory: '/repo', sessionId: 's1', clientId: 'c1' });

    // The holder leaves 25 min after the last heartbeat: still inside the
    // renewed window (pre-fix this evicted — lastUsedAt stayed at the
    // pre-lease timestamp, so the next sweep tick reclaimed it).
    h.engine.dialogs.leases.release({ directory: '/repo', sessionId: 's1', clientId: 'c1' });
    h.clock.now += 25 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    // Past 30 min since the last acquire the session is reclaimed.
    h.clock.now += 6 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('every SDK activity signal vetoes eviction', async () => {
    const flags: Array<keyof FixtureSession> = [
      'isStreaming',
      'isAborting',
      'isRetrying',
      'isCompacting',
      'isGeneratingHandoff',
      'isBashRunning',
      'isEvalRunning',
      'hasPendingBashMessages',
      'hasPendingPythonMessages',
      'hasPostPromptWork',
    ];
    for (const flag of flags) {
      // SAFETY: `flag` is a keyof FixtureSession whose value is boolean.
      const h = await createHarness({ sessionOverrides: { s1: { [flag]: true } as Partial<FixtureSession> } });
      await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
      h.clock.now += 31 * 60_000;
      h.engine.sweepIdleSessionsNow();
      await settle(10);
      expect(h.engine.liveRecord('s1')?.state).toBe('live');
    }
    const asyncWork = await createHarness({ sessionOverrides: { s1: { hasPendingAsyncWork: () => true } } });
    await asyncWork.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    asyncWork.clock.now += 31 * 60_000;
    asyncWork.engine.sweepIdleSessionsNow();
    await settle(10);
    expect(asyncWork.engine.liveRecord('s1')?.state).toBe('live');

    const queued = await createHarness({ sessionOverrides: { s1: { queuedMessageCount: 2 } } });
    await queued.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    queued.clock.now += 31 * 60_000;
    queued.engine.sweepIdleSessionsNow();
    await settle(10);
    expect(queued.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('in-flight operations veto eviction', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const record = h.engine.liveRecord('s1');
    if (!record) throw new Error('missing record');
    record.inFlight = 1;
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle();
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('eviction order: beginDispose → host teardown → session.idle → dispose settled', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          beginDispose: () => order.push('beginDispose'),
          subscribe: () => {
            order.push('subscribed');
            return () => order.push('unsubscribed');
          },
          dispose: async () => {
            order.push('dispose-start');
            await new Promise((resolve) => setTimeout(resolve, 15));
            order.push('dispose-done');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(80);

    expect(order.indexOf('beginDispose')).toBeLessThan(order.indexOf('dispose-start'));
    expect(order.indexOf('beginDispose')).toBeLessThan(order.indexOf('unsubscribed'));
    const idleSeen = h.engine.bus.replay.some((entry) => entry.envelope.type === 'session.idle');
    expect(idleSeen).toBe(true);
    expect(order.indexOf('dispose-done')).toBeGreaterThan(order.indexOf('unsubscribed'));
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('failed disposal quarantines: tombstone blocks rematerialization, observable in stats', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            throw new Error('drain exploded');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);

    const record = h.engine.liveRecord('s1');
    expect(record?.state).toBe('failed');
    expect(record?.failure?.reason).toContain('drain exploded');
    const diagnostics = h.engine.getStreamDiagnostics();
    expect(diagnostics.dataProportional.liveSessions.failed).toBe(1);
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'again' })).rejects.toThrow(/quarantined/);
  });

  test('never-settling disposal keeps the record evicting; materialize rejects, bounded', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: () => new Promise(() => {}),
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');

    // A new writer is rejected with the retryable busy error after the
    // bounded wait — never a second AgentSession over the same file.
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'again' })).rejects.toThrow(/evicting/);
    expect(h.sessions.size).toBe(1);
  });

  test('materialization failure releases every installed resource', async () => {
    const unsubscribed: string[] = [];
    const disposed: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          subscribe: () => () => {
            unsubscribed.push('s1');
          },
          dispose: async () => {
            disposed.push('s1');
          },
          sessionManager: {
            getSessionName: () => 'n',
            getArtifactsDir: () => '/a',
            onSessionNameChanged: () => {
              throw new Error('name callback registration exploded');
            },
          },
        },
      },
    });
    await expect(h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' })).rejects.toThrow(
      /name callback registration exploded/,
    );
    await settle(20);

    expect(unsubscribed).toEqual(['s1']);
    expect(disposed).toEqual(['s1']);
    expect(h.engine.liveRecord('s1')).toBeNull();
    // Setup failure is recoverable: no quarantine tombstone remains.
    expect(h.engine.getStreamDiagnostics().dataProportional.liveSessions.total).toBe(0);
  });

  test('wire sessions carry live state; a settled eviction emits a cold session.updated', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });

    const listed = await h.engine.listSessions({ directory: '/repo' });
    const wire = listed.find((session) => session.id === 's1');
    expect(wire?.live).toBe('live');
    expect(wire?.transcriptBytes).toBe(0);

    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(40);
    expect(h.engine.liveRecord('s1')).toBeNull();

    // The post-eviction session.updated replaces the stored record wholesale
    // — `live` absent marks the session cold for every client holding it.
    const updates = h.engine.bus.replay.filter(
      (entry) => entry.envelope.type === 'session.updated' && entry.envelope.properties?.info?.id === 's1',
    );
    const last = updates.at(-1)?.envelope.properties?.info;
    expect(last?.live).toBeUndefined();
  });

  test('stream diagnostics reports one bounded row per non-cold record', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });

    const diagnostics = h.engine.getStreamDiagnostics();
    const rows = diagnostics.dataProportional.liveSessions.sessions;
    expect(diagnostics.dataProportional.liveSessions.truncated).toBe(false);
    const row = rows.find((entry) => entry.id === 's1');
    expect(row?.state).toBe('live');
    expect(row?.directory).toBe('/repo');
    expect(row?.transcriptBytes).toBe(0);
    expect(row?.idleMs).toBe(0);

    h.engine.sweepIdleSessionsNow();
    expect(h.engine.getStreamDiagnostics().dataProportional.liveSessions.sessions).toHaveLength(1);
  });

  test('releaseSession evicts an idle resident, refuses active, no-ops cold', async () => {
    const h = await createHarness();
    // Cold session: nothing resident → idempotent no-op.
    expect(await h.engine.releaseSession({ sessionID: 's1', directory: '/repo' })).toBe('cold');

    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(await h.engine.releaseSession({ sessionID: 's1', directory: '/repo' })).toBe('released');
    await settle(40);
    expect(h.engine.liveRecord('s1')).toBeNull();

    // Active session: the release refuses rather than killing a live turn.
    const busy = await createHarness({ sessionOverrides: { s1: { isStreaming: true } } });
    await busy.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(await busy.engine.releaseSession({ sessionID: 's1', directory: '/repo' })).toBe('active');
    expect(busy.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('a throwing host unsubscribe cannot strand an evicting record', async () => {
    const disposed: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          subscribe: () => () => {
            throw new Error('unsub exploded');
          },
          dispose: async () => {
            disposed.push('s1');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(50);
    // A host-side teardown failure must not abort disposal: the SDK writer
    // still drained and the record finished evicting instead of stranding.
    expect(disposed).toEqual(['s1']);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('deleteSession refuses with a retryable busy error while disposal hangs', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: { dispose: () => new Promise(() => {}) },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    // evictDrainTimeoutMs 150 → the bounded wait times out inside the gate;
    // the transcript is never removed under a still-live writer.
    await expect(h.engine.deleteSession({ sessionID: 's1', directory: '/repo' })).rejects.toThrow(/not deletable/);
    expect(fs.existsSync(realPath)).toBe(true);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');
  });

  test('deleteSession refuses a quarantined record instead of unlinking under it', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            throw new Error('drain exploded');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('failed');

    const [realPath] = h.realPaths;
    await expect(h.engine.deleteSession({ sessionID: 's1', directory: '/repo' })).rejects.toThrow(/not deletable/);
    expect(fs.existsSync(realPath)).toBe(true);
  });

  test('moveSession refuses with a retryable busy error while disposal hangs', async () => {
    const h = await createHarness({
      sessionOverrides: {
        s1: { dispose: () => new Promise(() => {}) },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    await expect(h.engine.moveSession({ sessionID: 's1', destination: '/elsewhere' })).rejects.toThrow(/not movable/);
    expect(fs.existsSync(realPath)).toBe(true);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');
  });

  test('deleteSession awaits disposal before touching the transcript file', async () => {
    const h = await createHarness();
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    const [realPath] = h.realPaths;
    // Replace the fixture's dispose AFTER materialization (overrides map to
    // the same object the engine holds).
    const session = h.sessions.get('s1');
    if (!session) throw new Error('s1 missing');
    session.dispose = async () => {
      if (!fs.existsSync(realPath)) throw new Error('file removed before disposal settled');
      await new Promise((resolve) => setTimeout(resolve, 15));
    };
    await h.engine.deleteSession({ sessionID: 's1', directory: '/repo' });
    expect(fs.existsSync(realPath)).toBe(false);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('same session id in two directories is two isolated records', async () => {
    const h = await createHarness({ files: [{ id: 'same', cwd: '/a' }, { id: 'same', cwd: '/b' }] });
    await h.engine.prompt({ sessionID: 'same', directory: '/a', text: 'in a' });
    await h.engine.prompt({ sessionID: 'same', directory: '/b', text: 'in b' });
    const recordA = h.engine.liveRecord('same', '/a');
    const recordB = h.engine.liveRecord('same', '/b');
    expect(recordA?.state).toBe('live');
    expect(recordB?.state).toBe('live');
    expect(recordA).not.toBe(recordB);
    expect(recordA?.directory).toBe('/a');
    expect(recordB?.directory).toBe('/b');
    // An id-only lookup refuses the ambiguous pair instead of guessing.
    expect(h.engine.liveRecord('same')).toBeNull();
  });

  test('moveSession evicts the live writer before relocating the file', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            order.push('disposed');
          },
        },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    await h.engine.moveSession({ sessionID: 's1', destination: '/elsewhere' });
    expect(order).toEqual(['disposed']);
    expect(h.engine.liveRecord('s1')).toBeNull();
  });

  test('shutdown: beginDispose on every live record; hanging disposal quarantines within the deadline', async () => {
    const order: string[] = [];
    const h = await createHarness({
      files: [{ id: 'a1', cwd: '/repo' }, { id: 'a2', cwd: '/repo' }],
      sessionOverrides: {
        a1: {
          beginDispose: () => order.push('a1-begin'),
          dispose: async () => {},
        },
        a2: {
          dispose: () => new Promise(() => {}),
        },
      },
    });
    await h.engine.prompt({ sessionID: 'a1', directory: '/repo', text: 'warm' });
    await h.engine.prompt({ sessionID: 'a2', directory: '/repo', text: 'warm' });

    const startedAt = Date.now();
    await h.engine.shutdown();
    expect(Date.now() - startedAt).toBeLessThan(20_000);
    expect(order).toContain('a1-begin');
    // The settled record is gone; the hanging one is quarantined observably.
    expect(h.engine.liveRecord('a1')).toBeNull();
    expect(h.engine.liveRecord('a2')?.state).toBe('evicting');
    await expect(h.engine.prompt({ sessionID: 'a1', directory: '/repo', text: 'post-shutdown' })).rejects.toThrow(
      /shutting down/,
    );
  });

  test('external transcript rewrite evicts the stale live writer before the next prompt (plan §8.2)', async () => {
    const order: string[] = [];
    const h = await createHarness({
      sessionOverrides: {
        s1: {
          dispose: async () => {
            order.push('disposed');
          },
        },
      },
    });
    // Non-empty transcript so materialize records a tail entry id.
    fs.writeFileSync(h.realPaths[0], '{"id":"e1"}\n{"id":"e2"}\n');
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    expect(h.engine.liveRecord('s1')?.state).toBe('live');

    // External writer shrinks the transcript — classifyExternalChange: dirty.
    fs.writeFileSync(h.realPaths[0], '{"id":"e1"}\n');

    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'after-rewrite' });
    // The stale writer was disposed and a fresh one materialized through the
    // gate — never two writers over the same file.
    expect(order).toEqual(['disposed']);
    expect(h.engine.liveRecord('s1')?.state).toBe('live');
  });

  test('eviction drops the record-scoped wireIdEchoes; live sessions keep theirs (plan D5/phase 5)', async () => {
    const h = await createHarness({
      files: [{ id: 's1', cwd: '/repo' }, { id: 's2', cwd: '/repo' }],
      sessionOverrides: {
        // s2 stays active through the sweep so its echoes must survive.
        s2: { isStreaming: true },
      },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    await h.engine.prompt({ sessionID: 's2', directory: '/repo', text: 'warm' });
    // Echo entries ride the live record (compact mapping): each session's
    // map dies with its own record — no process-wide accumulation.
    h.engine.liveRecord('s1')?.payload?.wireIdEchoes.set('cold-a', 'live-a');
    h.engine.liveRecord('s2')?.payload?.wireIdEchoes.set('cold-b', 'live-b');
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')).toBeNull();
    expect(h.engine.liveRecord('s2')?.state).toBe('live');
    expect(h.engine.liveRecord('s2')?.payload?.wireIdEchoes.get('cold-b')).toBe('live-b');
    // Aggregate diagnostics observe the surviving session's entries only.
    expect(h.engine.getStreamDiagnostics().dataProportional.wireIdEchoes).toBe(1);
  });

  test('HTTP boundary: prompt against an evicting record answers 409 SessionBusyError (plan §3.4)', async () => {
    const h = await createHarness({
      sessionOverrides: { s1: { dispose: () => new Promise(() => {}) } },
    });
    await h.engine.prompt({ sessionID: 's1', directory: '/repo', text: 'warm' });
    h.clock.now += 31 * 60_000;
    h.engine.sweepIdleSessionsNow();
    await settle(30);
    expect(h.engine.liveRecord('s1')?.state).toBe('evicting');

    const { startOmpHost } = await import('./host.ts');
    const host = await startOmpHost({ engine: h.engine, port: 0 });
    try {
      const response = await fetch(`${host.baseUrl}/session/s1/prompt_async`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ directory: '/repo', parts: [{ type: 'text', text: 'again' }] }),
      });
      expect(response.status).toBe(409);
      // SAFETY: response.json() yields unknown; the 409 wire shape is the
      // contract under test.
      const payload = (await response.json()) as { _tag?: string };
      expect(payload._tag).toBe('SessionBusyError');
    } finally {
      await host.close();
    }
  });
});
