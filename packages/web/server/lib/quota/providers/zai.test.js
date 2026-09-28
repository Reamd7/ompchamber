/**
 * z.ai Coding Plan 配额 provider 测试套件。
 *
 * 用 vi.mock 替换 auth 文件读取并 stub 全局 fetch，验证 TOKENS_LIMIT 与
 * TIME_LIMIT 条目到 5h/weekly/MCP Tools 窗口的映射，以及 CREDIT_LIMIT
 * 条目的积分 valueLabel 与套餐等级 planLabel 的透出。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../opencode/auth.js', () => ({
  readAuthFile: () => ({ 'zai-coding-plan': { key: 'test-token' } }),
}));

import { fetchQuota } from './zai.js';

afterEach(() => {
  vi.unstubAllGlobals();
});

/** 构造最小可用的 fetch Response 替身：默认 200 + JSON 体。 */
const mockResponse = (body) => ({
  ok: true,
  status: 200,
  json: async () => body,
});

/** z.ai provider 的限额条目映射与 planLabel 透出行为。 */
describe('Z.ai quota provider', () => {
  it('surfaces 5-hour, weekly, and MCP quota windows', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      data: {
        limits: [
          { type: 'TOKENS_LIMIT', unit: 3, number: 5, percentage: 0 },
          { type: 'TOKENS_LIMIT', unit: 6, number: 1, percentage: 100, nextResetTime: 1785659659993 },
          { type: 'TIME_LIMIT', unit: 5, number: 1, percentage: 0, nextResetTime: 1787128459979 },
        ],
      },
    })));

    const result = await fetchQuota();
    const windows = result.usage.windows;

    expect(result.ok).toBe(true);
    expect(windows['5h']).toMatchObject({
      usedPercent: 0,
      remainingPercent: 100,
      windowSeconds: 5 * 60 * 60,
      resetAt: null,
    });
    expect(windows.weekly).toMatchObject({
      usedPercent: 100,
      remainingPercent: 0,
      windowSeconds: 7 * 24 * 60 * 60,
      resetAt: 1785659659993,
    });
    expect(windows['MCP Tools']).toMatchObject({
      usedPercent: 0,
      remainingPercent: 100,
      windowSeconds: 30 * 24 * 60 * 60,
      resetAt: 1787128459979,
    });
  });

  it('maps CREDIT_LIMIT entries to windows with credit value labels and plan level', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      code: 200,
      data: {
        limits: [
          { type: 'CREDIT_LIMIT', unit: 3, number: 5, usage: 12000, currentValue: 65, remaining: 11934, percentage: 1, nextResetTime: 1787257978907 },
          { type: 'CREDIT_LIMIT', unit: 6, number: 1, usage: 60000, currentValue: 65, remaining: 59934, percentage: 1, nextResetTime: 1787844668997 },
        ],
        level: 'pro',
      },
    })));

    const result = await fetchQuota();
    const windows = result.usage.windows;

    expect(result.ok).toBe(true);
    expect(result.planLabel).toBe('pro');
    expect(windows['5h']).toMatchObject({
      usedPercent: 1,
      remainingPercent: 99,
      windowSeconds: 5 * 60 * 60,
      resetAt: 1787257978907,
      valueLabel: '65 / 12k credits',
    });
    expect(windows.weekly).toMatchObject({
      usedPercent: 1,
      remainingPercent: 99,
      windowSeconds: 7 * 24 * 60 * 60,
      resetAt: 1787844668997,
      valueLabel: '65 / 60k credits',
    });
  });
});
