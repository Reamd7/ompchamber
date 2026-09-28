/**
 * Claude 订阅配额 provider 测试套件。
 *
 * mock 掉凭据加载并 stub 全局 fetch，验证：未配置分支、窗口与套餐标签
 * 的正常返回、并发请求合并、429 冷却期间回放缓存且不再请求 Anthropic、
 * 账户切换后不串用旧缓存，以及 401 与网络失败的错误文案。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/** loadClaudeCredential 的 mock 句柄：按用例返回不同凭据或 null。 */
const credential = vi.fn();

vi.mock('./auth.js', () => ({
  loadClaudeCredential: () => credential(),
}));

import { fetchQuota, isConfigured, resetClaudeQuotaCache } from './index.js';

/** 标准测试凭据：max 套餐、keychain 来源、token 互不相同（供指纹区分）。 */
const CREDENTIAL = {
  accessToken: 'access-a',
  refreshToken: 'refresh-a',
  expiresAt: Date.now() + 3_600_000,
  planLabel: 'max',
  source: 'keychain',
};

/** 文档化的 Anthropic 用量载荷：会话窗口 5%、周窗口 4%。 */
const PAYLOAD = {
  limits: [
    { kind: 'session', percent: 5, resets_at: '2026-08-14T19:10:00Z', scope: null },
    { kind: 'weekly_all', percent: 4, resets_at: '2026-08-20T15:00:00Z', scope: null },
  ],
};

/** 构造 200 JSON 成功响应替身（带空 Headers）。 */
const jsonResponse = (body) => ({
  ok: true,
  status: 200,
  headers: new Headers(),
  json: async () => body,
});

/** 构造指定状态码（可带响应头）的错误响应替身。 */
const errorResponse = (status, headers = {}) => ({
  ok: false,
  status,
  headers: new Headers(headers),
  json: async () => ({}),
});

beforeEach(() => {
  resetClaudeQuotaCache();
  credential.mockReturnValue(CREDENTIAL);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/** Claude provider 的缓存、限流冷却与凭据切换行为。 */
describe('Claude quota provider', () => {
  it('reports not configured when no credential source has a token', async () => {
    credential.mockReturnValue(null);

    expect(isConfigured()).toBe(false);
    const result = await fetchQuota();
    expect(result.configured).toBe(false);
    expect(result.ok).toBe(false);
    expect(result.usage).toBeNull();
  });

  it('returns windows and the plan label from the credential', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(PAYLOAD)));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.planLabel).toBe('max');
    expect(result.usage.windows['5h'].usedPercent).toBe(5);
  });

  it('coalesces concurrent refreshes into one Anthropic request', async () => {
    let resolveResponse;
    const fetchMock = vi.fn().mockReturnValue(new Promise((resolve) => {
      resolveResponse = resolve;
    }));
    vi.stubGlobal('fetch', fetchMock);

    const first = fetchQuota();
    const second = fetchQuota();
    resolveResponse(jsonResponse(PAYLOAD));

    const [firstResult, secondResult] = await Promise.all([first, second]);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(firstResult.ok).toBe(true);
    expect(secondResult.ok).toBe(true);
  });

  it('keeps serving the last good values while Anthropic rate limits', async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse(PAYLOAD))
      .mockResolvedValueOnce(errorResponse(429, { 'retry-after': '120' }));
    vi.stubGlobal('fetch', fetchMock);

    await fetchQuota();
    const rateLimited = await fetchQuota();

    expect(rateLimited.ok).toBe(true);
    expect(rateLimited.usage.windows['5h'].usedPercent).toBe(5);
  });

  it('does not call Anthropic again while the rate-limit cooldown is active', async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse(PAYLOAD))
      .mockResolvedValueOnce(errorResponse(429));
    vi.stubGlobal('fetch', fetchMock);

    await fetchQuota();
    await fetchQuota();
    await fetchQuota();

    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('never shows one account cached values after the credential changes', async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse(PAYLOAD))
      .mockResolvedValueOnce(errorResponse(429));
    vi.stubGlobal('fetch', fetchMock);

    await fetchQuota();
    credential.mockReturnValue({ ...CREDENTIAL, accessToken: 'access-b', refreshToken: 'refresh-b', planLabel: null });
    const afterSwitch = await fetchQuota();

    expect(afterSwitch.ok).toBe(false);
    expect(afterSwitch.usage).toBeNull();
  });

  it('explains an expired session instead of reporting a bare 401', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(errorResponse(401)));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.configured).toBe(true);
    expect(result.error).toContain('Claude Code');
  });

  it('surfaces a network failure instead of an empty successful result', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('socket hang up')));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.usage).toBeNull();
    expect(result.error).toBe('socket hang up');
  });
});
