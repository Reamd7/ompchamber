/**
 * OMPChamber 会话路由（routes.js）的测试套件。
 *
 * 用 supertest 驱动真实 express app + vi.mock 掉的引擎客户端/git/worktree，
 * 并按 URL 桩化 globalThis.fetch，覆盖：会话创建（含非 ASCII 目录 header、
 * 局部 JSON 解析、session-created 事件）、缺省模型/agent 解析、goal 元数
 * 据先于派发、worktree 建立与引导等待、send/fork 的选择复用与基线、
 * 无效输入在任何 OpenCode 副作用之前被拒，以及部分结果（partial）报告。
 */
import express from 'express';
import request from 'supertest';
import { beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

/** createWorktree 桩：固定返回 side-task worktree 的信息。 */
const createWorktreeMock = vi.fn(async () => ({
  head: 'abc123',
  name: 'side-task',
  branch: 'ompchamber/side-task',
  path: '/repo/worktrees/side-task',
}));
/** getWorktreeBootstrapStatus 桩：默认立即就绪（setup-ready）。 */
const getWorktreeBootstrapStatusMock = vi.fn(async () => ({
  status: 'ready',
  phase: 'setup-ready',
  error: null,
  updatedAt: Date.now(),
}));
/** 引擎端 session.create 桩：固定返回 ses_123。 */
const sessionCreateMock = vi.fn(async () => ({ data: { id: 'ses_123' } }));
/** 引擎端 session.fork 桩：固定返回带标题的 ses_fork。 */
const sessionForkMock = vi.fn(async () => ({ data: { id: 'ses_fork', title: 'Forked session' } }));
/** 引擎端 session.messages 桩：默认由 beforeEach 换成"记录派发"行为。 */
const sessionMessagesMock = vi.fn(async () => ({ data: [] }));

// 用例预置的既有消息（setSessionMessages 写入）。
let existingSessionMessages = [];
// 已"落盘"的派发 user 消息序号，用于生成递增 id。
let dispatchedUserMessageSeq = 0;

// The service confirms a prompt landed by watching for a new user message, so
// the default mock behaves like OpenCode recording each dispatched prompt.
// 中文补充：服务靠观察新的 user 消息确认 prompt 落盘，故默认桩模拟
// OpenCode 把每次派发的 prompt 都记录进会话。
const setSessionMessages = (messages) => {
  existingSessionMessages = messages;
};

/**
 * session.messages 的默认实现：在预置消息之后追加一条新的 user 消息，
 * 模拟一次 prompt 派发被 OpenCode 记录（waitForPromptLanded 的证据源）。
 */
const recordedSessionMessages = async () => {
  dispatchedUserMessageSeq += 1;
  return {
    data: [
      ...existingSessionMessages,
      {
        info: {
          id: `msg_dispatched_${dispatchedUserMessageSeq}`,
          role: 'user',
          time: { created: 1000 + dispatchedUserMessageSeq },
        },
      },
    ],
  };
};

// Selection inputs are fetched whenever a request names a model, agent, or
// variant, so every prompt-dispatching fetch mock must answer them.
// 中文补充：只要请求点名了 model、agent 或 variant 就会拉取选择输入，
// 因此每个会派发 prompt 的 fetch 桩都必须应答这三个端点。
const selectionInputResponse = (url) => {
  const text = String(url);
  if (text.includes('/config/providers')) {
    return {
      ok: true,
      json: async () => ({
        providers: [
          { id: 'openai', models: [{ id: 'gpt-5.5', variants: { high: {} } }] },
          { id: 'anthropic', models: [{ id: 'claude-sonnet-5', variants: { high: {} } }] },
        ],
      }),
    };
  }
  if (text.includes('/agent')) {
    return { ok: true, json: async () => [{ name: 'build', mode: 'primary' }, { name: 'plan', mode: 'primary' }] };
  }
  if (text.includes('/config')) return { ok: true, json: async () => ({}) };
  return null;
};
/** 引擎端 session.command 桩：slash 命令派发成功路径。 */
const sessionCommandMock = vi.fn(async () => ({ data: {} }));
/** 引擎端 command.list 桩：默认没有任何命令。 */
const commandListMock = vi.fn(async () => ({ data: [] }));
// 经 globalThis 暴露给 vi.mock 工厂（工厂在模块作用域执行，拿不到本地变量）。
globalThis.__ompchamberCreateWorktreeMock = createWorktreeMock;
// 同上：worktree 引导状态查询桩的 globalThis 桥。
globalThis.__ompchamberGetWorktreeBootstrapStatusMock = getWorktreeBootstrapStatusMock;

// 被测路由注册函数；beforeAll 里动态 import 以配合 vi.mock 的提升时机。
let registerOMPChamberSessionRoutes;

// 引擎客户端整桩替换：session.create/fork/messages/command 与 command.list。
vi.mock('../opencode/local-engine-client.js', () => ({
  createLocalEngineClient: () => ({
    session: {
      create: sessionCreateMock,
      fork: sessionForkMock,
      messages: sessionMessagesMock,
      command: sessionCommandMock,
    },
    command: {
      list: commandListMock,
    },
  }),
}));

// git 模块桩：转发到 globalThis 上的两个 mock，便于用例改写实现。
vi.mock('../git/index.js', () => ({
  createWorktree: (...args) => globalThis.__ompchamberCreateWorktreeMock(...args),
  getWorktreeBootstrapStatus: (...args) => globalThis.__ompchamberGetWorktreeBootstrapStatusMock(...args),
}));

/**
 * 构造注册了会话路由的 express app；overrides 替换任意依赖，options.
 * globalJson=false 时不挂全局 JSON 中间件（验证路由自带 body 解析）。
 * calls 供用例记录副作用。
 */
const createApp = (overrides = {}, options = {}) => {
  const app = express();
  if (options.globalJson !== false) {
    app.use(express.json());
  }
  const calls = [];
  registerOMPChamberSessionRoutes(app, {
    readSettingsFromDiskMigrated: async () => ({ projects: [{ id: 'proj_1', path: '/repo/app' }] }),
    sanitizeProjects: (projects) => projects,
    validateDirectoryPath: async (directory) => ({ ok: true, directory }),
    buildOpenCodeUrl: (route) => `http://opencode.test${route}`,
    getOpenCodeAuthHeaders: () => ({ Authorization: 'Bearer test' }),
    waitForOpenCodeReady: vi.fn(async () => undefined),
    ...overrides,
  });
  return { app, calls };
};

// 会话路由契约：创建/发送/分叉端到端行为，见各用例断言。
describe('ompchamber session routes', () => {
  // 动态导入被测模块，确保 vi.mock 已生效。
  beforeAll(async () => {
    ({ registerOMPChamberSessionRoutes } = await import('./routes.js'));
  });

  // 每个用例前重置全部桩并恢复默认实现（消息桩重新模拟"记录派发"）。
  beforeEach(() => {
    createWorktreeMock.mockClear();
    getWorktreeBootstrapStatusMock.mockClear();
    getWorktreeBootstrapStatusMock.mockImplementation(async () => ({
      status: 'ready',
      phase: 'setup-ready',
      error: null,
      updatedAt: Date.now(),
    }));
    sessionCreateMock.mockClear();
    sessionForkMock.mockClear();
    existingSessionMessages = [];
    dispatchedUserMessageSeq = 0;
    sessionMessagesMock.mockReset();
    sessionMessagesMock.mockImplementation(recordedSessionMessages);
    sessionCommandMock.mockReset();
    sessionCommandMock.mockResolvedValue({ data: {} });
    commandListMock.mockReset();
    commandListMock.mockResolvedValue({ data: [] });
  });

  it('creates a session for a directory', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async () => ({ ok: true, json: async () => ({ id: 'ses_123' }) }));
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', title: 'Side task' })
        .expect(200);

      expect(response.body.sessionId).toBeTruthy();
      expect(response.body.sessionId).toBe('ses_123');
      expect(response.body.directory).toBe('/repo/app');
      expect(response.body.promptDispatched).toBe(false);
      expect(globalThis.fetch).toHaveBeenCalledWith(
        'http://opencode.test/session?directory=%2Frepo%2Fapp',
        expect.objectContaining({
          method: 'POST',
          body: JSON.stringify({ directory: '/repo/app', title: 'Side task' }),
        }),
      );
      expect(sessionCreateMock).not.toHaveBeenCalled();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('percent-encodes the directory header for non-ASCII checkout paths', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async () => ({ ok: true, json: async () => ({ id: 'ses_123' }) }));
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/home/user/Masaüstü/projeler', title: 'Side task' })
        .expect(200);

      expect(globalThis.fetch).toHaveBeenCalledWith(
        expect.any(String),
        expect.objectContaining({
          headers: expect.objectContaining({
            'x-opencode-directory': encodeURIComponent('/home/user/Masaüstü/projeler'),
          }),
        }),
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('parses JSON body without global middleware', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async () => ({ ok: true, json: async () => ({ id: 'ses_123' }) }));
    try {
      const { app } = createApp({}, { globalJson: false });
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app' })
        .expect(200);

      expect(response.body.sessionId).toBe('ses_123');
      expect(response.body.directory).toBe('/repo/app');
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('emits a session-created event after creating a session', async () => {
    const originalFetch = globalThis.fetch;
    const emitSessionCreatedEvent = vi.fn();
    globalThis.fetch = vi.fn(async () => ({ ok: true, json: async () => ({ id: 'ses_123' }) }));
    try {
      const { app } = createApp({ emitSessionCreatedEvent });
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', title: 'Side task' })
        .expect(200);

      expect(emitSessionCreatedEvent).toHaveBeenCalledWith(expect.objectContaining({
        sessionID: 'ses_123',
        directory: '/repo/app',
        title: 'Side task',
        promptDispatched: false,
        dispatchedAsCommand: false,
      }));
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('resolves default model and agent when prompt omits them', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => {
      const text = String(url);
      if (text.includes('/prompt_async')) {
        return { ok: true, text: async () => '' };
      }
      if (text.includes('/config/providers')) {
        return { ok: true, json: async () => ({ providers: [{ id: 'openai', models: { 'gpt-5.5': { id: 'gpt-5.5' } } }] }) };
      }
      if (text.includes('/agent')) {
        return { ok: true, json: async () => [{ name: 'build', mode: 'primary' }] };
      }
      if (text.includes('/config')) {
        return { ok: true, json: async () => ({}) };
      }
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    globalThis.fetch = fetchMock;
    const { app } = createApp({
      readSettingsFromDiskMigrated: async () => ({
        defaultModel: 'openai/gpt-5.5',
        defaultAgent: 'build',
        projects: [{ id: 'proj_1', path: '/repo/app' }],
      }),
    });
    try {
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run this' })
        .expect(200);

      expect(response.body.model).toEqual({ providerID: 'openai', modelID: 'gpt-5.5' });
      expect(response.body.agent).toBe('build');
      expect(fetchMock).toHaveBeenCalledWith(
        'http://opencode.test/config/providers?directory=%2Frepo%2Fapp',
        expect.any(Object),
      );
      const promptCall = fetchMock.mock.calls.find(([url]) => String(url).includes('/prompt_async'));
      expect(JSON.parse(promptCall?.[1]?.body)).toMatchObject({
        model: { providerID: 'openai', modelID: 'gpt-5.5' },
        agent: 'build',
      });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('dispatches an initial prompt when model is provided', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => {
      if (String(url).includes('/prompt_async')) {
        return { ok: true, text: async () => '' };
      }
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run this', model: 'openai/gpt-5.5' })
        .expect(200);

      expect(response.body.sessionId).toBe('ses_123');
      expect(response.body.promptDispatched).toBe(true);
      expect(fetchMock).toHaveBeenCalledWith(
        'http://opencode.test/session/ses_123/prompt_async?directory=%2Frepo%2Fapp',
        expect.objectContaining({ method: 'POST' }),
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('creates goal metadata before dispatching the initial goal prompt', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => {
      if (String(url).includes('/prompt_async')) return { ok: true, text: async () => '' };
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    const createSessionGoal = vi.fn(async () => undefined);
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp({ createSessionGoal });
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({
          directory: '/repo/app',
          prompt: 'Finish and verify the migration',
          model: 'openai/gpt-5.5',
          goal: true,
          goalTokenBudget: 200000,
        })
        .expect(200);

      const promptCall = fetchMock.mock.calls.find(([url]) => String(url).includes('/prompt_async'));
      const promptPayload = JSON.parse(promptCall[1].body);
      expect(createSessionGoal).toHaveBeenCalledWith(expect.objectContaining({
        sessionID: 'ses_123',
        directory: '/repo/app',
        objective: 'Finish and verify the migration',
        tokenBudget: 200000,
        providerID: 'openai',
        modelID: 'gpt-5.5',
      }));
      expect(createSessionGoal.mock.invocationCallOrder[0]).toBeLessThan(fetchMock.mock.invocationCallOrder.at(-1));
      expect(promptPayload.parts).toEqual([
        { type: 'text', text: 'Finish and verify the migration' },
        expect.objectContaining({ type: 'text', synthetic: true }),
      ]);
      expect(response.body).toMatchObject({ goalEnabled: true, goalTokenBudget: 200000, promptDispatched: true });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('rejects invalid goal requests before creating a session', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn();
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', goal: true })
        .expect(400, { error: 'prompt is required when goal is enabled' });
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run', goalTokenBudget: 200000 })
        .expect(400, { error: 'goalTokenBudget requires goal' });
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run', goal: true, goalTokenBudget: 999 })
        .expect(400, { error: 'goalTokenBudget must be an integer from 1000 to 100000000' });
      expect(fetchMock).not.toHaveBeenCalled();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('creates a worktree before creating a session', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async (url) => {
      if (String(url).includes('/prompt_async')) {
        return { ok: true, text: async () => '' };
      }
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({
          directory: '/repo/app',
          worktree: { name: 'side-task', branchName: 'ompchamber/side-task', startRef: 'main' },
          setUpstream: false,
          prompt: 'Run this',
          model: 'openai/gpt-5.5',
        })
        .expect(200);

      expect(createWorktreeMock).toHaveBeenCalledWith('/repo/app', {
        mode: 'new',
        name: 'side-task',
        branchName: 'ompchamber/side-task',
        startRef: 'main',
        setUpstream: false,
      });
      expect(response.body.directory).toBe('/repo/worktrees/side-task');
      expect(response.body.worktree.path).toBe('/repo/worktrees/side-task');
      expect(globalThis.fetch).toHaveBeenCalledWith(
        'http://opencode.test/session/ses_123/prompt_async?directory=%2Frepo%2Fworktrees%2Fside-task',
        expect.objectContaining({ method: 'POST' }),
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('waits for the worktree bootstrap to complete before creating the session', async () => {
    const statuses = [
      { status: 'pending', phase: 'directory-created', error: null, updatedAt: 1 },
      { status: 'pending', phase: 'git-ready', error: null, updatedAt: 2 },
      { status: 'ready', phase: 'setup-ready', error: null, updatedAt: 3 },
    ];
    getWorktreeBootstrapStatusMock.mockImplementation(async () => statuses.shift() || statuses[statuses.length - 1]);
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async (url) => {
      if (String(url).includes('/prompt_async')) {
        return { ok: true, text: async () => '' };
      }
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({
          directory: '/repo/app',
          worktree: { name: 'side-task' },
          prompt: 'Run this',
          model: 'openai/gpt-5.5',
        })
        .expect(200);

      expect(response.body.promptDispatched).toBe(true);
      const sessionCreateCalls = globalThis.fetch.mock.calls.filter(([url]) => String(url).includes('/session?directory'));
      const promptCalls = globalThis.fetch.mock.calls.filter(([url]) => String(url).includes('/prompt_async'));
      expect(sessionCreateCalls.length).toBeGreaterThanOrEqual(1);
      expect(promptCalls.length).toBeGreaterThanOrEqual(1);
      const createIndex = globalThis.fetch.mock.calls.indexOf(sessionCreateCalls[0]);
      const promptIndex = globalThis.fetch.mock.calls.indexOf(promptCalls[0]);
      expect(getWorktreeBootstrapStatusMock).toHaveBeenCalled();
      expect(createIndex).toBeGreaterThan(-1);
      expect(promptIndex).toBeGreaterThan(createIndex);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('fails the create when the worktree bootstrap failed', async () => {
    getWorktreeBootstrapStatusMock.mockImplementation(async () => ({
      status: 'failed',
      phase: 'directory-created',
      error: 'branch already exists',
      updatedAt: Date.now(),
    }));
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async (url) => ({ ok: true, json: async () => ({ id: 'ses_123' }) }));
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({
          directory: '/repo/app',
          worktree: { name: 'side-task' },
          prompt: 'Run this',
          model: 'openai/gpt-5.5',
        })
        .expect(500, { error: 'Worktree bootstrap failed: branch already exists' });
      const promptCalls = globalThis.fetch.mock.calls.filter(([url]) => String(url).includes('/prompt_async'));
      expect(promptCalls.length).toBe(0);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('sends a goal prompt to an existing session after creating goal metadata', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => selectionInputResponse(url) || { ok: true, text: async () => '' });
    const createSessionGoal = vi.fn(async () => undefined);
    globalThis.fetch = fetchMock;
    try {
      setSessionMessages([{ info: { id: 'msg_before', role: 'assistant', time: { created: 10, completed: 20 } } }]);
      const { app } = createApp({ createSessionGoal });
      const response = await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({
          directory: '/repo/app',
          prompt: 'Apply and verify the review feedback',
          model: 'openai/gpt-5.5',
          agent: 'build',
          variant: 'high',
          goal: true,
          goalTokenBudget: 200000,
        })
        .expect(200);

      expect(response.body).toMatchObject({
        action: 'send',
        sessionId: 'ses_source',
        directory: '/repo/app',
        promptDispatched: true,
        goalEnabled: true,
        baselineAssistantMessageId: 'msg_before',
      });
      expect(createSessionGoal).toHaveBeenCalledWith(expect.objectContaining({
        sessionID: 'ses_source',
        directory: '/repo/app',
        objective: 'Apply and verify the review feedback',
      }));
      const promptCall = fetchMock.mock.calls.find(([url]) => String(url).includes('/prompt_async'));
      expect(promptCall?.[0]).toBe('http://opencode.test/session/ses_source/prompt_async?directory=%2Frepo%2Fapp');
      expect(createSessionGoal.mock.invocationCallOrder[0]).toBeLessThan(fetchMock.mock.invocationCallOrder.at(-1));
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('uses the expanded slash-command template as the goal objective before command dispatch', async () => {
    const originalFetch = globalThis.fetch;
    const createSessionGoal = vi.fn(async () => undefined);
    commandListMock.mockResolvedValue({
      data: [{
        name: 'issue--to-pr',
        template: 'Take $ARGUMENTS from issue through a verified pull request. Confirm the PR covers $ARGUMENTS.',
      }],
    });
    globalThis.fetch = vi.fn(async (url) => selectionInputResponse(url));
    try {
      const { app } = createApp({ createSessionGoal });
      const response = await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({
          directory: '/repo/app',
          prompt: '/issue--to-pr LIN-123',
          model: 'openai/gpt-5.5',
          agent: 'build',
          goal: true,
        })
        .expect(200);

      expect(createSessionGoal).toHaveBeenCalledWith(expect.objectContaining({
        objective: 'Take LIN-123 from issue through a verified pull request. Confirm the PR covers LIN-123.',
      }));
      expect(sessionCommandMock).toHaveBeenCalledWith(expect.objectContaining({
        command: 'issue--to-pr',
        arguments: 'LIN-123',
      }));
      expect(createSessionGoal.mock.invocationCallOrder[0]).toBeLessThan(sessionCommandMock.mock.invocationCallOrder[0]);
      expect(response.body).toMatchObject({ goalEnabled: true, dispatchedAsCommand: true });
      expect(globalThis.fetch.mock.calls.some(([url]) => String(url).includes('/prompt_async'))).toBe(false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('reuses the previous session selection when send omits model, agent, and variant', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => selectionInputResponse(url) || { ok: true, text: async () => '' });
    globalThis.fetch = fetchMock;
    try {
      setSessionMessages([
          {
            info: {
              id: 'msg_user',
              role: 'user',
              agent: 'plan',
              model: { providerID: 'anthropic', modelID: 'claude-sonnet-5', variant: 'high' },
              time: { created: 5 },
            },
          },
          { info: { id: 'msg_before', role: 'assistant', time: { created: 10, completed: 20 } } },
      ]);
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({ directory: '/repo/app', prompt: 'Continue where you left off' })
        .expect(200);

      expect(response.body).toMatchObject({
        action: 'send',
        sessionId: 'ses_source',
        model: { providerID: 'anthropic', modelID: 'claude-sonnet-5' },
        agent: 'plan',
        variant: 'high',
        promptDispatched: true,
      });
      const promptCall = fetchMock.mock.calls.find(([url]) => String(url).includes('/prompt_async'));
      const promptBody = JSON.parse(promptCall[1].body);
      expect(promptBody).toMatchObject({
        model: { providerID: 'anthropic', modelID: 'claude-sonnet-5' },
        agent: 'plan',
        variant: 'high',
      });
      // The default-selection inputs (config/providers/agents) must not be consulted.
      expect(fetchMock.mock.calls.every(([url]) => String(url).includes('/prompt_async'))).toBe(true);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('forks from a message, dispatches the prompt, and emits the new session', async () => {
    const originalFetch = globalThis.fetch;
    const emitSessionCreatedEvent = vi.fn();
    globalThis.fetch = vi.fn(async (url) => selectionInputResponse(url) || { ok: true, text: async () => '' });
    try {
      const { app } = createApp({ emitSessionCreatedEvent });
      const response = await request(app)
        .post('/api/ompchamber/sessions/ses_source/fork')
        .send({
          directory: '/repo/app',
          messageId: 'msg_branch_point',
          prompt: 'Try the alternative implementation',
          model: 'openai/gpt-5.5',
          agent: 'build',
          variant: 'high',
        })
        .expect(200);

      expect(sessionForkMock).toHaveBeenCalledWith({
        sessionID: 'ses_source',
        directory: '/repo/app',
        messageID: 'msg_branch_point',
      });
      expect(response.body).toMatchObject({
        action: 'fork',
        sourceSessionId: 'ses_source',
        sessionId: 'ses_fork',
        directory: '/repo/app',
        promptDispatched: true,
      });
      expect(sessionMessagesMock).toHaveBeenCalledWith({
        sessionID: 'ses_fork',
        directory: '/repo/app',
        limit: 100,
      });
      expect(globalThis.fetch).toHaveBeenCalledWith(
        'http://opencode.test/session/ses_fork/prompt_async?directory=%2Frepo%2Fapp',
        expect.objectContaining({ method: 'POST' }),
      );
      expect(emitSessionCreatedEvent).toHaveBeenCalledWith(expect.objectContaining({
        sessionID: 'ses_fork',
        sourceSessionID: 'ses_source',
        directory: '/repo/app',
        promptDispatched: true,
      }));
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('rejects send and fork requests without a prompt before calling OpenCode', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn();
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({ directory: '/repo/app' })
        .expect(400, { error: 'prompt is required' });
      await request(app)
        .post('/api/ompchamber/sessions/ses_source/fork')
        .send({ directory: '/repo/app' })
        .expect(400, { error: 'prompt is required' });
      expect(fetchMock).not.toHaveBeenCalled();
      expect(sessionForkMock).not.toHaveBeenCalled();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('reports the forked session when prompt dispatch fails', async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(async (url) => selectionInputResponse(url) || { ok: false, status: 500, text: async () => 'dispatch failed' });
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions/ses_source/fork')
        .send({
          directory: '/repo/app',
          prompt: 'Try another approach',
          model: 'openai/gpt-5.5',
          agent: 'build',
          variant: 'high',
        })
        .expect(500);

      expect(response.body).toMatchObject({
        partial: true,
        partialAction: 'fork-created',
        sessionId: 'ses_fork',
        directory: '/repo/app',
      });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('does not apply a default variant to an explicitly requested model', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => {
      const text = String(url);
      if (text.includes('/prompt_async')) return { ok: true, text: async () => '' };
      if (text.includes('/config/providers')) {
        return {
          ok: true,
          json: async () => ({
            providers: [
              { id: 'openai', models: { requested: { id: 'requested' }, default: { id: 'default', variants: { high: {} } } } },
            ],
          }),
        };
      }
      if (text.includes('/agent')) return { ok: true, json: async () => [{ name: 'build', mode: 'primary' }] };
      if (text.includes('/config')) return { ok: true, json: async () => ({}) };
      return { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp({
        readSettingsFromDiskMigrated: async () => ({
          defaultModel: 'openai/default',
          defaultVariant: 'high',
          projects: [{ id: 'proj_1', path: '/repo/app' }],
        }),
      });
      await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({ directory: '/repo/app', prompt: 'Continue', model: 'openai/requested', agent: 'build' })
        .expect(200);

      const promptCall = fetchMock.mock.calls.find(([url]) => String(url).includes('/prompt_async'));
      expect(JSON.parse(promptCall[1].body)).not.toHaveProperty('variant');
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('rejects an unknown agent before creating a session or worktree', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => selectionInputResponse(url) || { ok: true, json: async () => ({ id: 'ses_123' }) });
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({
          directory: '/repo/app',
          prompt: 'Run this',
          agent: 'not-an-agent',
          worktree: { name: 'side-task' },
        })
        .expect(400, { error: "Unknown agent 'not-an-agent' for /repo/app" });

      expect(createWorktreeMock).not.toHaveBeenCalled();
      expect(fetchMock.mock.calls.some(([url]) => String(url) === 'http://opencode.test/session?directory=%2Frepo%2Fapp')).toBe(false);
      expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/prompt_async'))).toBe(false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('rejects an unknown model and an unknown variant before dispatching', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => selectionInputResponse(url) || { ok: true, json: async () => ({ id: 'ses_123' }) });
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run this', model: 'openai/gpt-nope' })
        .expect(400, { error: "Unknown model 'openai/gpt-nope' for /repo/app" });
      await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run this', model: 'openai/gpt-5.5', variant: 'ultra' })
        .expect(400, { error: "Unknown variant 'ultra' for model 'openai/gpt-5.5'" });

      expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/prompt_async'))).toBe(false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('reports promptDispatched false when the accepted prompt never reaches the session', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => {
      if (String(url).includes('/prompt_async')) return { ok: true, text: async () => '' };
      return selectionInputResponse(url) || { ok: true, json: async () => ({ id: 'ses_123' }) };
    });
    globalThis.fetch = fetchMock;
    sessionMessagesMock.mockResolvedValue({ data: [] });
    try {
      const { app } = createApp();
      const response = await request(app)
        .post('/api/ompchamber/sessions')
        .send({ directory: '/repo/app', prompt: 'Run this', model: 'openai/gpt-5.5' })
        .expect(200);

      expect(response.body.sessionId).toBe('ses_123');
      expect(response.body.promptDispatched).toBe(false);
      expect(response.body.promptError).toBeTruthy();
    } finally {
      globalThis.fetch = originalFetch;
    }
  }, 20_000);

  it('does not retry a failed slash command as a normal prompt', async () => {
    const originalFetch = globalThis.fetch;
    const fetchMock = vi.fn(async (url) => selectionInputResponse(url));
    commandListMock.mockResolvedValue({ data: [{ name: 'review' }] });
    sessionCommandMock.mockRejectedValue(new Error('command response failed'));
    globalThis.fetch = fetchMock;
    try {
      const { app } = createApp();
      await request(app)
        .post('/api/ompchamber/sessions/ses_source/send')
        .send({
          directory: '/repo/app',
          prompt: '/review fix this',
          model: 'openai/gpt-5.5',
          agent: 'build',
          variant: 'high',
        })
        .expect(500);

      expect(sessionCommandMock).toHaveBeenCalledTimes(1);
      expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/prompt_async'))).toBe(false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
