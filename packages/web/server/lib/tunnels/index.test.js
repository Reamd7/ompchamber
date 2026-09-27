/**
 * 隧道服务（createTunnelService）测试套件。
 *
 * 覆盖服务编排的两个核心行为：提供商启动异常向路由调用方透传
 * （普通 Error 被包装为 TunnelServiceError/startup_failed 且保留原始消息），
 * 以及切换提供商时对活动隧道的替换（先停旧隧道再启动新提供商的隧道）。
 */
import { describe, expect, it } from 'bun:test';

import { createTunnelService } from './index.js';
import {
  TUNNEL_INTENT_EPHEMERAL_PUBLIC,
  TUNNEL_MODE_QUICK,
  TUNNEL_PROVIDER_CLOUDFLARE,
  TUNNEL_PROVIDER_NGROK,
} from './types.js';

/** 构造满足服务协议的最小提供商桩：capabilities 声明 quick 模式，
 *  start/stop/resolvePublicUrl 可按用例定制，默认实现均可用。 */
const createProvider = ({ provider, start, stop, resolvePublicUrl }) => ({
  id: provider,
  capabilities: {
    provider,
    modes: [{ key: TUNNEL_MODE_QUICK, intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC }],
  },
  checkAvailability: async () => ({ available: true }),
  start,
  stop,
  resolvePublicUrl: resolvePublicUrl || ((controller) => controller?.getPublicUrl?.() ?? null),
});

/** 构造以普通对象为底的注册表桩（仅需 get 方法）。 */
const createRegistry = (providers) => ({
  get: (providerId) => providers[providerId] ?? null,
});

/** 服务编排行为：启动错误透传与跨提供商替换。 */
describe('createTunnelService', () => {
  it('returns provider startup errors to route callers', async () => {
    let controller = null;
    const provider = createProvider({
      provider: TUNNEL_PROVIDER_NGROK,
      start: async () => {
        throw new Error('ngrok authtoken is not configured');
      },
    });
    const service = createTunnelService({
      registry: createRegistry({ [TUNNEL_PROVIDER_NGROK]: provider }),
      getController: () => controller,
      setController: (next) => { controller = next; },
      getActivePort: () => 3000,
    });

    try {
      await service.start({ provider: TUNNEL_PROVIDER_NGROK, mode: TUNNEL_MODE_QUICK });
      throw new Error('Expected service.start to fail');
    } catch (error) {
      expect(error.name).toBe('TunnelServiceError');
      expect(error.code).toBe('startup_failed');
      expect(error.message).toBe('ngrok authtoken is not configured');
    }
  });

  it('replaces an active quick tunnel when the provider changes', async () => {
    let stopped = false;
    let ngrokStarted = false;
    let controller = {
      provider: TUNNEL_PROVIDER_CLOUDFLARE,
      mode: TUNNEL_MODE_QUICK,
      stop: () => { stopped = true; },
      getPublicUrl: () => 'https://cloudflare.example',
    };
    const cloudflareProvider = createProvider({
      provider: TUNNEL_PROVIDER_CLOUDFLARE,
      start: async () => controller,
    });
    const ngrokProvider = createProvider({
      provider: TUNNEL_PROVIDER_NGROK,
      start: async () => {
        ngrokStarted = true;
        return {
          mode: TUNNEL_MODE_QUICK,
          getPublicUrl: () => 'https://demo.ngrok-free.app',
        };
      },
    });
    const service = createTunnelService({
      registry: createRegistry({
        [TUNNEL_PROVIDER_CLOUDFLARE]: cloudflareProvider,
        [TUNNEL_PROVIDER_NGROK]: ngrokProvider,
      }),
      getController: () => controller,
      setController: (next) => { controller = next; },
      getActivePort: () => 3000,
    });

    const result = await service.start({ provider: TUNNEL_PROVIDER_NGROK, mode: TUNNEL_MODE_QUICK });

    expect(stopped).toBe(true);
    expect(ngrokStarted).toBe(true);
    expect(result.provider).toBe(TUNNEL_PROVIDER_NGROK);
    expect(result.publicUrl).toBe('https://demo.ngrok-free.app');
  });
});
