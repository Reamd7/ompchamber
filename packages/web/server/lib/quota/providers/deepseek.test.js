/**
 * DeepSeek 配额 provider 测试套件。
 *
 * 用 vi.mock 替换 auth 文件读取并 stub 全局 fetch，验证 USD/CNY 余额的
 * 选择与格式化、字符串/数字余额的解析，以及 401/403、超时、解析失败、
 * 空数据等错误路径的文案映射。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../opencode/auth.js', () => ({
  readAuthFile: () => ({ deepseek: { key: 'test-token' } }),
}));

import { fetchQuota } from './deepseek.js';

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

/** 官方文档示例的余额响应载荷，作为各用例构造变体的公共基底。 */
// Documented payload shape from https://api.deepseek.com/user/balance
const DOCUMENTED_PAYLOAD = {
  is_available: true,
  balance_infos: [
    {
      currency: 'USD',
      total_balance: '7.54',
      granted_balance: '0.00',
      topped_up_balance: '7.54'
    }
  ]
};

/** DeepSeek provider 的余额窗口组装与错误映射行为。 */
describe('DeepSeek quota provider', () => {
  it('builds credits_balance window from documented USD payload (string balance)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse(DOCUMENTED_PAYLOAD)));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.providerId).toBe('deepseek');

    const window = result.usage.windows.credits_balance;
    expect(window).toBeDefined();
    expect(window.valueLabel).toBe('$7.54');
    expect(window.usedPercent).toBeNull();
    expect(window.windowSeconds).toBeNull();
    expect(window.resetAt).toBeNull();
  });

  it('falls back to CNY entry when no USD entry is present', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      is_available: true,
      balance_infos: [
        { currency: 'CNY', total_balance: '100.00', granted_balance: '0.00', topped_up_balance: '100.00' }
      ]
    })));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.credits_balance.valueLabel).toBe('¥100.00');
  });

  it('prefers the USD entry when both USD and CNY are present', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      is_available: true,
      balance_infos: [
        { currency: 'CNY', total_balance: '100.00', granted_balance: '0.00', topped_up_balance: '100.00' },
        { currency: 'USD', total_balance: '3.55', granted_balance: '0.00', topped_up_balance: '3.55' }
      ]
    })));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.credits_balance.valueLabel).toBe('$3.55');
  });

  it('tolerates a numeric total_balance', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      is_available: true,
      balance_infos: [{ currency: 'USD', total_balance: 12.5, granted_balance: 0, topped_up_balance: 12.5 }]
    })));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.credits_balance.valueLabel).toBe('$12.50');
  });

  it('maps 401 to session-expired error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 401, json: async () => ({}) }));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.error).toBe('Session expired — please re-authenticate with DeepSeek');
  });

  it('maps 403 to session-expired error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 403, json: async () => ({}) }));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.error).toBe('Session expired — please re-authenticate with DeepSeek');
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

  it('reports a normalized timeout error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new DOMException('The operation timed out.', 'TimeoutError')));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.error).toBe('Request timed out');
  });

  it('returns no-quota-data on a 200 payload with no usable balance', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      is_available: true,
      balance_infos: [{ currency: 'USD', total_balance: '', granted_balance: '0.00', topped_up_balance: '0.00' }]
    })));

    const result = await fetchQuota();

    expect(result.ok).toBe(false);
    expect(result.configured).toBe(true);
    expect(result.error).toBe('No quota data in response');
    expect(result.usage).toBeNull();
  });

  it('keeps a literal zero balance as a valid valueLabel', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(mockResponse({
      is_available: true,
      balance_infos: [{ currency: 'USD', total_balance: '0.00', granted_balance: '0.00', topped_up_balance: '0.00' }]
    })));

    const result = await fetchQuota();

    expect(result.ok).toBe(true);
    expect(result.usage.windows.credits_balance.valueLabel).toBe('$0.00');
  });
});
