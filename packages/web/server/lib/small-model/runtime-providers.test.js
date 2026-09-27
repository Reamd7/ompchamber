/**
 * 运行时 provider 快照（runtime-providers.js）测试套件：/provider 载荷的
 * 归一化（插件凭证、zen 哨兵剔除、模型端点回退）、快照的单飞与 TTL
 * 缓存，以及 OpenCode 不可达或未接线时的回退语义。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  ZEN_ANONYMOUS_API_KEY,
  configureOpenCodeRuntimeProviders,
  getRuntimeProvider,
  getRuntimeProviderSnapshot,
  resetOpenCodeRuntimeProviders,
} from './runtime-providers.js';

/** 构造 /provider 响应载荷的工厂：三个典型 provider——插件配置注册、zen 匿名、auth.json 凭证。 */
const providerPayload = (overrides = {}) => ({
  all: [
    {
      id: 'llmapi',
      source: 'config',
      options: { apiKey: 'plugin-key', baseURL: 'https://api.llmapi.ai/v1/' },
      models: { 'claude-opus-4-8': { api: { id: 'claude-opus-4-8', url: '', npm: '@ai-sdk/anthropic' } } },
    },
    {
      id: 'opencode',
      source: 'custom',
      options: { apiKey: ZEN_ANONYMOUS_API_KEY },
      models: { 'free-model': { api: { id: 'free-model', url: 'https://opencode.ai/zen/v1', npm: '@ai-sdk/openai-compatible' } } },
    },
    {
      id: 'zai-coding-plan',
      source: 'api',
      key: 'auth-json-key',
      options: {},
      models: { 'glm-5': { api: { id: 'glm-5', url: 'https://api.z.ai/api/coding/paas/v4', npm: '@ai-sdk/openai-compatible' } } },
    },
  ],
  connected: ['llmapi', 'opencode', 'zai-coding-plan'],
  ...overrides,
});

// 快照获取与缓存行为：mock 全局 fetch，验证归一化结果、并发去重与失败回退。
describe('OpenCode runtime provider snapshot', () => {
  let fetchMock;

  beforeEach(() => {
    fetchMock = vi.fn(async () => new Response(JSON.stringify(providerPayload()), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }));
    vi.stubGlobal('fetch', fetchMock);
    configureOpenCodeRuntimeProviders({
      buildOpenCodeUrl: (pathname) => `http://127.0.0.1:4096${pathname}`,
      getOpenCodeAuthHeaders: () => ({ Authorization: 'Basic test' }),
    });
  });

  afterEach(() => {
    configureOpenCodeRuntimeProviders(null);
    resetOpenCodeRuntimeProviders();
    vi.unstubAllGlobals();
  });

  it('reports the credential and endpoint a plugin registered at runtime', async () => {
    const provider = await getRuntimeProvider('llmapi');

    expect(provider).toMatchObject({ apiKey: 'plugin-key', baseURL: 'https://api.llmapi.ai/v1' });
    expect(fetchMock.mock.calls[0][0]).toBe('http://127.0.0.1:4096/provider');
    expect(fetchMock.mock.calls[0][1].headers).toMatchObject({ Authorization: 'Basic test' });
  });

  it('refuses the zen sentinel as a credential', async () => {
    const provider = await getRuntimeProvider('opencode');

    expect(provider.apiKey).toBeNull();
    expect(provider.anonymousZen).toBe(true);
    // The endpoint is still reported; only the credential is withheld.
    expect(provider.baseURL).toBe('https://opencode.ai/zen/v1');
  });

  it('falls back to the model endpoint when the provider carries no baseURL', async () => {
    expect((await getRuntimeProvider('zai-coding-plan')).baseURL).toBe('https://api.z.ai/api/coding/paas/v4');
  });

  it('serves one snapshot to concurrent callers instead of refetching', async () => {
    await Promise.all([getRuntimeProvider('llmapi'), getRuntimeProvider('opencode'), getRuntimeProvider('zai-coding-plan')]);

    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('answers "unknown" rather than "no providers" when OpenCode is unreachable', async () => {
    resetOpenCodeRuntimeProviders();
    fetchMock.mockRejectedValue(new Error('connection refused'));

    expect(await getRuntimeProviderSnapshot()).toBeNull();
  });

  it('keeps the previous snapshot when a later refresh fails', async () => {
    await getRuntimeProviderSnapshot();
    fetchMock.mockRejectedValue(new Error('connection refused'));

    // Past the snapshot TTL, so the next read genuinely attempts a refresh.
    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 60_000);
    const refreshed = await getRuntimeProviderSnapshot();
    vi.useRealTimers();

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(refreshed.providers.has('llmapi')).toBe(true);
  });

  it('stays on file-based resolution until it is configured', async () => {
    configureOpenCodeRuntimeProviders(null);

    expect(await getRuntimeProvider('llmapi')).toBeNull();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
