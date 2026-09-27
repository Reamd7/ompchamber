/**
 * 配额 provider 注册表测试套件。
 *
 * 验证 google 模块按注册表约定的形状导出成员、listConfiguredQuotaProviders
 * 在注册表完整导入时不抛错，以及 fetchQuotaForProvider 对同一 provider
 * 并发请求的合并与完成后重新放行。
 */
import { describe, expect, it } from 'vitest';

import * as google from './google/index.js';
import { fetchQuotaForProvider, listConfiguredQuotaProviders } from './index.js';

/** 注册表的成员聚合、已配置列表与并发合并行为。 */
describe('quota provider registry', () => {
  it('exposes google provider configuration helpers through the provider module', () => {
    expect(google.providerId).toBe('google');
    expect(google.providerName).toBe('Google');
    expect(typeof google.isConfigured).toBe('function');
    expect(typeof google.resolveGoogleAuthSources).toBe('function');
  });

  it('can list configured providers without missing provider exports', () => {
    expect(() => listConfiguredQuotaProviders()).not.toThrow();
  });

  it('coalesces concurrent refreshes by provider ID', async () => {
    const first = fetchQuotaForProvider('unsupported-test-provider');
    const second = fetchQuotaForProvider('unsupported-test-provider');

    expect(first).toBe(second);
    await first;
    expect(fetchQuotaForProvider('unsupported-test-provider')).not.toBe(first);
  });
});
