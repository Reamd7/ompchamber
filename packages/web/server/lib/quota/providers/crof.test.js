/**
 * CrofAI 配额 provider 测试套件。
 *
 * 用 vi.mock 替换 auth 文件读取并 stub 全局 fetch，验证 credits 余额的
 * 展示（数字、数字字符串、缺失字段）以及 401、5xx、JSON 解析失败等
 * 错误路径的文案映射。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../opencode/auth.js', () => ({
  readAuthFile: () => ({ crof: { key: 'test-token' } }),
}));

import { fetchQuota } from './crof.js';

afterEach(() => {
  vi.unstubAllGlobals();
});

/** 构造最小可用的 fetch Response 替身：默认 200 + JSON 体，可被 init 覆盖。 */
const mockResponse = (body, init = {}) => ({
  ok: true,
  status: 200,
  json: async () => body,
  ...init,
});

/** CrofAI provider 的余额展示与错误映射行为。 */
describe('Crof quota provider', () => {
  it('reports credits balance as valueLabel with null percent', async () => {
    // Documented /usage_api/ response from https://crof.ai/docs.md
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(
      mockResponse({ usable_requests: 450, credits: 12.3456 }),
    ));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.providerId).toBe('crof');
    expect(result.usage.windows.credits.usedPercent).toBeNull();
    expect(result.usage.windows.credits.valueLabel).toBe('$12.35');
    expect(result.usage.windows.credits.windowSeconds).toBeNull();
    expect(result.usage.windows.credits.resetAt).toBeNull();
  });

  it('tolerates missing credits field', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(
      mockResponse({ usable_requests: 0 }),
    ));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.credits.valueLabel).toBeUndefined();
    expect(result.usage.windows.credits.usedPercent).toBeNull();
  });

  it('parses numeric-string credits', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(
      mockResponse({ credits: '99.5' }),
    ));

    const result = await fetchQuota();

    expect(result.usage.windows.credits.valueLabel).toBe('$99.50');
  });

  it('maps 401 to session-expired error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: false,
      status: 401,
      json: async () => ({}),
    }));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.configured).toBe(true);
    expect(result.error).toBe('Session expired — please re-authenticate with CrofAI');
  });

  it('surfaces non-401 API errors with status', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: false,
      status: 503,
      json: async () => ({}),
    }));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.error).toBe('API error: 503');
  });

  it('reports invalid-response on JSON parse failure', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => { throw new SyntaxError('Unexpected token'); },
    }));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.error).toBe('Invalid response from provider');
  });
});
