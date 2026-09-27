/**
 * 通知模板运行时测试套件。
 *
 * 覆盖 zen 模型相关接口在 provider 下线后的兼容行为（空列表、保留存量值），
 * 以及通知正文提取规则：只认带 type: 'text' 的片段、排除 reasoning
 * （思维链）片段，包括直接从 payload 提取与经 fetch 拉取助手消息两条路径。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

import { createNotificationTemplateRuntime } from './template-runtime.js';

/** 备份全局 fetch，afterEach 中恢复，防止用例内的 stub 泄漏。 */
const originalFetch = globalThis.fetch;

/**
 * 以最小依赖创建被测运行时：设置可注入、URL 构建与请求头为直通桩。
 * persistSettings 与 git 解析仅满足构造参数形状，本套件不触及。
 */
const createRuntime = (settings = {}) => createNotificationTemplateRuntime({
  readSettingsFromDisk: async () => settings,
  persistSettings: vi.fn(async () => {}),
  buildOpenCodeUrl: (path) => path,
  getOpenCodeAuthHeaders: () => ({}),
  resolveGitBinaryForSpawn: () => 'git',
});

/** zen 模型接口在 provider 退役后的兼容行为。 */
describe('notification template runtime zen models', () => {
  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it('returns no selectable zen models after provider retirement', async () => {
    const runtime = createRuntime();
    const models = await runtime.fetchFreeZenModels();

    expect(models).toEqual([]);
  });

  it('preserves stored zen model value for compatibility without validation', async () => {
    const runtime = createRuntime({ zenModel: 'trinity-large-preview-free' });

    await expect(runtime.resolveZenModel()).resolves.toBe('trinity-large-preview-free');
  });
});

/** 通知正文的消息文本提取规则（payload 直取与 fetch 拉取两条路径）。 */
describe('notification template message extraction', () => {
  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it('excludes reasoning parts from payload message text', () => {
    const runtime = createRuntime();

    expect(runtime.extractLastMessageText({
      properties: {
        info: {
          parts: [
            { type: 'reasoning', text: 'private chain of thought' },
            { type: 'text', text: 'final answer' },
          ],
        },
      },
    })).toBe('final answer');
  });

  it('ignores untyped parts even when they contain text', () => {
    const runtime = createRuntime();

    expect(runtime.extractLastMessageText({
      properties: {
        info: {
          parts: [
            { text: 'untyped text' },
            { content: 'untyped content' },
            { type: 'text', text: 'typed final answer' },
          ],
        },
      },
    })).toBe('typed final answer');
  });

  it('excludes reasoning parts when fetching assistant messages', async () => {
    const runtime = createRuntime();
    globalThis.fetch = vi.fn(async () => new Response(JSON.stringify([
      {
        info: { id: 'msg-1', role: 'assistant', finish: 'stop' },
        parts: [
          { type: 'reasoning', text: 'private chain of thought' },
          { type: 'text', text: 'final answer' },
        ],
      },
    ])));

    await expect(runtime.fetchLastAssistantMessageText('session-1', 'msg-1')).resolves.toBe('final answer');
  });
});
