/**
 * startup-pipeline-runtime 模块的单元测试。
 *
 * 验证启动管线的阶段顺序：监听（并拿到实际端口）→ 把端口发布给隧道
 * 上下文 → 调度 OpenCode API 探测 → 最后才引导托管 OpenCode，确保
 * 引导期间对外已有可用的监听端口。
 */
import { describe, expect, it, vi } from 'vitest';

import { createStartupPipelineRuntime } from './startup-pipeline-runtime.js';

// 启动管线各阶段的执行顺序
describe('startup pipeline runtime', () => {
  it('publishes the listening port before bootstrapping managed OpenCode', async () => {
    const order = [];
    const runtime = createStartupPipelineRuntime({
      createTerminalRuntime: () => ({}),
      createDictationRuntime: () => ({}),
      createMessageStreamWsRuntime: () => ({}),
      createServerStartupRuntime: () => ({
        resolveBindHost: () => '127.0.0.1',
        startListeningAndMaybeTunnel: async () => {
          order.push('listen');
          return { activePort: 3901 };
        },
        attachProcessHandlers: vi.fn(),
      }),
    });

    await runtime.run({
      app: {},
      setupProxy: vi.fn(),
      staticRoutesRuntime: { registerStaticRoutes: vi.fn() },
      apiOnly: false,
      tunnelRuntimeContext: {
        setActivePort: (port) => order.push(`port:${port}`),
      },
      scheduleOpenCodeApiDetection: () => order.push('detect'),
      bootstrapOpenCodeAtStartup: () => order.push('bootstrap'),
      process: {},
      crypto: {},
      server: {},
      attachSignals: false,
    });

    expect(order).toEqual(['listen', 'port:3901', 'detect', 'bootstrap']);
  });
});
