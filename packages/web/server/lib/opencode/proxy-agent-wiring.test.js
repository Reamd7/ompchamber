/**
 * OpenCode API 代理的 keep-alive agent 装配测试：mock 掉
 * http-proxy-middleware，断言每个代理都持有 keepAlive 的 agent、API 与
 * OAuth 代理共享同一实例、agent 按协议（scheme）记忆化，并验证生产时序
 * （代理注册时引擎端口未知 → bootstrap 之后才解析出 https 目标）下的
 * 惰性解析行为。
 */
import http from 'node:http';
import https from 'node:https';

import { beforeEach, describe, expect, it, vi } from 'vitest';

/** 提升到 vi.mock 之前创建的 mock 函数，供 mock 工厂引用。 */
const { createProxyMiddlewareMock } = vi.hoisted(() => ({
  createProxyMiddlewareMock: vi.fn(),
}));

// 用桩替换 http-proxy-middleware：仅捕获 createProxyMiddleware 的调用参数。
vi.mock('http-proxy-middleware', () => ({
  createProxyMiddleware: createProxyMiddlewareMock,
}));

/** 在 mock 生效后动态加载被测模块。 */
const { registerOpenCodeProxy } = await import('./proxy.js');

/** 构造只实现 get / set 的极简 express app 桩（其余方法为 no-op）。 */
const createStubApp = () => {
  const settings = new Map();
  const noop = () => {};

  return {
    get: (...args) => (args.length === 1 ? settings.get(args[0]) : undefined),
    set: (key, value) => {
      settings.set(key, value);
    },
    use: noop,
    post: noop,
    put: noop,
    patch: noop,
    delete: noop,
    all: noop,
  };
};

/**
 * 中文补充：state 故意做成可变对象，以便测试模拟生产时序——代理注册
 * 先于 OpenCode bootstrap，端口与 base URL 在那之后才可解析。
 */
/**
 * `state` is intentionally mutable so a test can model the production ordering:
 * the proxy is registered before OpenCode bootstraps, so the port/base URL only
 * become resolvable afterwards.
 */
const createStubDeps = (state) => ({
  fs: { promises: { realpath: async (value) => value } },
  os: {},
  path: {},
  OPEN_CODE_READY_GRACE_MS: 0,
  LONG_REQUEST_TIMEOUT_MS: 1_000,
  getRuntime: () => ({ openCodePort: state.port, openCodeBaseUrl: state.baseUrl }),
  getOpenCodeAuthHeaders: () => ({}),
  // Mirrors network-runtime.js: throws until the port is known.
  buildOpenCodeUrl: (pathname) => {
    if (!state.port) {
      throw new Error('OpenCode port is not available');
    }
    return `${state.baseUrl}${pathname}`;
  },
  ensureOpenCodeApiPrefix: (pathname) => pathname,
});

/** 已完成 bootstrap 的引擎状态（本地 http 目标）。 */
const managedState = () => ({ port: 49303, baseUrl: 'http://127.0.0.1:49303' });
/** 注册时尚未 bootstrap 的冷启动状态（端口未知）。 */
const coldState = () => ({ port: null, baseUrl: null });

/** 从 mock 调用参数中提取每次代理创建实际使用的 agent。 */
const agentsFromCalls = () => createProxyMiddlewareMock.mock.calls.map(([options]) => options.agent);

// keep-alive agent 装配主套件。
describe('OpenCode API proxy agent wiring', () => {
  // 每个用例前重置 mock，并让中间件桩直接放行请求。
  beforeEach(() => {
    createProxyMiddlewareMock.mockReset();
    createProxyMiddlewareMock.mockImplementation(() => (_req, _res, next) => next?.());
  });

  it('constructs every proxy with a keep-alive agent', () => {
    registerOpenCodeProxy(createStubApp(), createStubDeps(managedState()));

    expect(createProxyMiddlewareMock).toHaveBeenCalled();

    for (const agent of agentsFromCalls()) {
      // Without an explicit agent, http-proxy falls back to `agent: false`,
      // which forces `Connection: close` and burns one ephemeral port per
      // request. See createOpenCodeProxyAgent in ./proxy.js.
      expect(agent).toBeTruthy();
      expect(agent.options?.keepAlive).toBe(true);
    }
  });

  it('shares one agent instance across the API and OAuth proxies', () => {
    registerOpenCodeProxy(createStubApp(), createStubDeps(managedState()));

    const agents = agentsFromCalls();

    expect(agents.length).toBeGreaterThan(1);
    expect(agents.every(Boolean)).toBe(true);
    expect(new Set(agents).size).toBe(1);
  });

  it('memoizes the agent per scheme rather than allocating one per resolution', () => {
    registerOpenCodeProxy(createStubApp(), createStubDeps(managedState()));

    const [options] = createProxyMiddlewareMock.mock.calls[0];

    expect(options.agent).toBe(options.agent);
  });

  // Production ordering: startup-pipeline-runtime.js calls setupProxy() before
  // bootstrapOpenCodeAtStartup(), so at registration the port is null,
  // buildOpenCodeUrl throws, and resolveProxyTarget() falls back to the http
  // loopback default. An external https server configured via OPENCODE_HOST is
  // only visible after bootstrap, so the agent must be resolved lazily.
  it('resolves an https agent after bootstrap even though registration ran cold', () => {
    const state = coldState();
    registerOpenCodeProxy(createStubApp(), createStubDeps(state));

    // Cold: nothing resolvable yet, so the http fallback target applies.
    for (const agent of agentsFromCalls()) {
      expect(agent).not.toBeInstanceOf(https.Agent);
    }

    // Bootstrap completes against an external https server.
    state.baseUrl = 'https://opencode.example.com:4096';

    for (const agent of agentsFromCalls()) {
      expect(agent).toBeInstanceOf(https.Agent);
      // Asserted on the live resolver path, not just the exported factory:
      // the https branch is the one a mutation could silently strip.
      expect(agent.options?.keepAlive).toBe(true);
      expect(agent.options?.maxFreeSockets).toBe(256);
    }
  });

  it('keeps a plain http agent when bootstrap resolves an http target', () => {
    const state = coldState();
    registerOpenCodeProxy(createStubApp(), createStubDeps(state));

    Object.assign(state, managedState());

    for (const agent of agentsFromCalls()) {
      // https.Agent extends http.Agent, so the negative assertion is load-bearing.
      expect(agent).toBeInstanceOf(http.Agent);
      expect(agent).not.toBeInstanceOf(https.Agent);
    }
  });

  it('derives an https agent when the target is already https at registration', () => {
    registerOpenCodeProxy(
      createStubApp(),
      createStubDeps({ port: 4096, baseUrl: 'https://opencode.example.com:4096' }),
    );

    const agents = agentsFromCalls();

    expect(agents.length).toBeGreaterThan(0);
    for (const agent of agents) {
      expect(agent).toBeInstanceOf(https.Agent);
    }
  });
});
