/**
 * 会话目标运行时测试套件：验证“活跃度门控”——静置窗口内父会话重新忙起、
 * 子会话仍在工作时等待下一轮 idle、实时状态读取失败时重试静置窗口，
 * 以及空闲父会话无在跑子会话时的正常审计（终结为 complete 的主路径）。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { createSessionGoalRuntime } from './runtime.js';

/** 测试用父会话 id。 */
const SESSION_ID = 'ses_parent';
/** 测试用子会话 id。 */
const CHILD_ID = 'ses_child';
/** 测试用项目目录。 */
const DIRECTORY = '/workspace';

/** 预置的激活目标 metadata，内嵌在会话响应用。 */
const goal = {
  id: 'goal_1',
  objective: 'Finish the task',
  status: 'active',
  turnsUsed: 1,
  createdAt: 1,
  updatedAt: 1,
};

/** 预置的父会话响应体（metadata 内嵌 goal）。 */
const session = {
  id: SESSION_ID,
  directory: DIRECTORY,
  metadata: { ompchamber: { goal } },
};

/** 构造 JSON Response 的 fetch 替身辅助。 */
const jsonResponse = (body, status = 200) => new Response(JSON.stringify(body), {
  status,
  headers: { 'Content-Type': 'application/json' },
});

/** 从 fetch 入参（字符串或 Request）提取 pathname，便于断言请求序列。 */
const requestPath = (input) => new URL(typeof input === 'string' ? input : input.url).pathname;

/**
 * 用给定 fetch 替身创建运行时（静置窗口 10ms、总开关强制开），注入一条
 * idle 事件并跑完定时器，返回运行时与小模型服务 mock。
 */
const startIdleTick = async (fetchImpl) => {
  const getSmallModelService = vi.fn();
  vi.stubGlobal('fetch', fetchImpl);
  const runtime = createSessionGoalRuntime({
    buildOpenCodeUrl: (pathname) => `http://opencode.test${pathname}`,
    getOpenCodeAuthHeaders: () => ({}),
    getSmallModelService,
    isEnabled: () => true,
    idleQuietMs: 10,
  });
  runtime.processPayload({
    type: 'session.status',
    properties: { sessionID: SESSION_ID, status: { type: 'idle' }, directory: DIRECTORY },
  });
  await vi.runOnlyPendingTimersAsync();
  return { runtime, getSmallModelService };
};

/** 活跃度门控与审计主路径验证（fake timers + 全局 fetch 打桩）。 */
describe('session goal live activity gate', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('waits for the next parent idle when the parent resumed during the quiet window', async () => {
    const paths = [];
    const { runtime, getSmallModelService } = await startIdleTick(vi.fn(async (input) => {
      const pathname = requestPath(input);
      paths.push(pathname);
      if (pathname === `/session/${SESSION_ID}`) return jsonResponse(session);
      if (pathname === '/session/status') return jsonResponse({ [SESSION_ID]: { type: 'busy' } });
      throw new Error(`Unexpected request: ${pathname}`);
    }));

    expect(paths).toEqual([`/session/${SESSION_ID}`, '/session/status']);
    expect(getSmallModelService).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(60_000);
    expect(paths).toHaveLength(2);
    runtime.stop();
  });

  it('waits for the parent result cycle while a direct child is working', async () => {
    const paths = [];
    const { runtime, getSmallModelService } = await startIdleTick(vi.fn(async (input) => {
      const pathname = requestPath(input);
      paths.push(pathname);
      if (pathname === `/session/${SESSION_ID}`) return jsonResponse(session);
      if (pathname === '/session/status') return jsonResponse({ [CHILD_ID]: { type: 'busy' } });
      if (pathname === `/session/${SESSION_ID}/children`) return jsonResponse([{ id: CHILD_ID, parentID: SESSION_ID }]);
      throw new Error(`Unexpected request: ${pathname}`);
    }));

    expect(paths).toEqual([
      `/session/${SESSION_ID}`,
      '/session/status',
      `/session/${SESSION_ID}/children`,
    ]);
    expect(getSmallModelService).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(60_000);
    expect(paths).toHaveLength(3);
    runtime.stop();
  });

  it('retries the quiet window when live status cannot be read', async () => {
    const paths = [];
    const { runtime, getSmallModelService } = await startIdleTick(vi.fn(async (input) => {
      const pathname = requestPath(input);
      paths.push(pathname);
      if (pathname === `/session/${SESSION_ID}`) return jsonResponse(session);
      if (pathname === '/session/status') return jsonResponse({ error: 'unavailable' }, 503);
      throw new Error(`Unexpected request: ${pathname}`);
    }));

    expect(paths).toEqual([`/session/${SESSION_ID}`, '/session/status']);
    expect(getSmallModelService).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(10);
    expect(paths).toEqual([
      `/session/${SESSION_ID}`,
      '/session/status',
      `/session/${SESSION_ID}`,
      '/session/status',
    ]);
    runtime.stop();
  });

  it('audits normally when the idle parent has no working children', async () => {
    const requests = [];
    const fetchImpl = vi.fn(async (input, init = {}) => {
      const pathname = requestPath(input);
      requests.push({ pathname, method: init.method ?? 'GET', body: init.body });
      if (pathname === `/session/${SESSION_ID}` && init.method === 'PATCH') return jsonResponse(session);
      if (pathname === `/session/${SESSION_ID}`) return jsonResponse(session);
      if (pathname === '/session/status') return jsonResponse({});
      if (pathname === `/session/${SESSION_ID}/children`) return jsonResponse([]);
      if (pathname === `/session/${SESSION_ID}/message`) {
        return jsonResponse([{
          info: {
            id: 'msg_assistant',
            sessionID: SESSION_ID,
            role: 'assistant',
            providerID: 'provider',
            modelID: 'model',
            time: { completed: 2 },
            tokens: { input: 1, output: 1, cache: { read: 0 } },
          },
          parts: [{ type: 'text', text: 'The task is verified complete.' }],
        }]);
      }
      throw new Error(`Unexpected request: ${pathname}`);
    });
    const service = {
      generateSmallModelText: vi.fn(async () => ({
        text: '{"verdict":"complete","note":"Task verified complete"}',
        providerID: 'provider',
        modelID: 'model',
      })),
    };
    vi.stubGlobal('fetch', fetchImpl);
    const runtime = createSessionGoalRuntime({
      buildOpenCodeUrl: (pathname) => `http://opencode.test${pathname}`,
      getOpenCodeAuthHeaders: () => ({}),
      getSmallModelService: async () => service,
      isEnabled: () => true,
      idleQuietMs: 10,
    });

    runtime.processPayload({
      type: 'session.status',
      properties: { sessionID: SESSION_ID, status: { type: 'idle' }, directory: DIRECTORY },
    });
    await vi.runOnlyPendingTimersAsync();

    expect(service.generateSmallModelText).toHaveBeenCalledOnce();
    const patch = requests.find((request) => request.pathname === `/session/${SESSION_ID}` && request.method === 'PATCH');
    expect(patch).toBeDefined();
    const writtenGoal = JSON.parse(patch.body).metadata.ompchamber.goal;
    expect(writtenGoal).toMatchObject({
      status: 'complete',
      evaluationProviderID: 'provider',
      evaluationModelID: 'model',
    });
    runtime.stop();
  });
});
