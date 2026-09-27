/**
 * Ollama Cloud 配额 provider 测试套件。
 *
 * 通过注入自定义 fetchImpl 验证安全与解析约束：重定向（3xx）直接按
 * 认证失败拒绝（不把 Cookie 凭据转发给重定向目标），解析不到用量数据
 * 的成功页面也报错。
 */
import { describe, expect, it } from 'bun:test';
import { fetchOllamaCloudUsage } from './ollama-cloud.js';

/** Ollama Cloud provider 的凭据保护与用量解析失败行为。 */
describe('Ollama Cloud quota provider', () => {
  it('rejects redirects without forwarding credentials', async () => {
    await expect(fetchOllamaCloudUsage({ cookie: 'session=secret' }, async () => new Response('', { status: 302 }))).rejects.toThrow('authentication failed');
  });

  it('rejects successful pages without usage data', async () => {
    await expect(fetchOllamaCloudUsage({ cookie: 'session=secret' }, async () => new Response('<html></html>'))).rejects.toThrow('could not be parsed');
  });
});
