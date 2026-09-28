/**
 * GitHub Copilot 配额 provider 测试套件。
 *
 * 用 vi.mock 替换 auth 文件读取并 stub 全局 fetch，以 it.each 同时驱动
 * 主订阅与 add-on 两个入口，验证只暴露 premium_interactions 窗口、
 * unlimited 套餐的展示以及 entitlement 不可用时对 percent_remaining 的回退。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../opencode/auth.js', () => ({
  readAuthFile: () => ({ 'github-copilot': { access: 'test-token' } }),
}));

import { fetchQuota, fetchQuotaAddon } from './copilot.js';

afterEach(() => {
  vi.unstubAllGlobals();
});

/** 文档化的配额响应载荷：chat/completions 快照应被忽略，仅 premium_interactions 生效。 */
const payload = {
  quota_reset_date: '2026-09-01T00:00:00Z',
  quota_snapshots: {
    chat: { entitlement: 100, remaining: 80 },
    completions: { entitlement: 1000, remaining: 900 },
    premium_interactions: { entitlement: 300, remaining: 225 },
  },
};

/** 构造 fetch Response 替身：默认返回上方 payload，可传入自定义 body。 */
const mockResponse = (body = payload) => ({
  ok: true,
  status: 200,
  json: async () => body,
});

/** 主订阅与 add-on 订阅共用的配额换算行为。 */
describe('GitHub Copilot quota provider', () => {
  it.each([
    ['primary provider', fetchQuota],
    ['add-on provider', fetchQuotaAddon],
  ])('exposes only premium interactions for the %s', async (_name, fetchProviderQuota) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse()));

    const result = await fetchProviderQuota();

    expect(result.ok).toBe(true);
    expect(Object.keys(result.usage.windows)).toEqual(['premium_interactions']);
    expect(result.usage.windows.premium_interactions.usedPercent).toBe(25);
    expect(result.usage.windows.premium_interactions.valueLabel).toBe('225 / 300 left');
  });

  it.each([
    ['primary provider', fetchQuota],
    ['add-on provider', fetchQuotaAddon],
  ])('reports unlimited plans without a percent for the %s', async (_name, fetchProviderQuota) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      quota_reset_date: '2026-09-01T00:00:00Z',
      quota_snapshots: {
        premium_interactions: { unlimited: true, entitlement: -1, remaining: -1 },
      },
    })));

    const result = await fetchProviderQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.premium_interactions.usedPercent).toBeNull();
    expect(result.usage.windows.premium_interactions.valueLabel).toBe('Unlimited');
  });

  it.each([
    ['primary provider', fetchQuota],
    ['add-on provider', fetchQuotaAddon],
  ])('falls back to percent_remaining when entitlement is unusable for the %s', async (_name, fetchProviderQuota) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      quota_reset_date: '2026-09-01T00:00:00Z',
      quota_snapshots: {
        premium_interactions: { entitlement: 0, remaining: 0, percent_remaining: 75.5 },
      },
    })));

    const result = await fetchProviderQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.premium_interactions.usedPercent).toBeCloseTo(24.5);
    expect(result.usage.windows.premium_interactions.valueLabel ?? null).toBeNull();
  });
});
