/**
 * ngrok 隧道提供商适配器。
 *
 * 基于 ngrok-tunnel.js 的底层能力实现提供商协议：能力声明（仅 quick 模式、beta
 * 稳定性）、依赖/authtoken/网络三项诊断（diagnose）、quick 隧道启动、停止与
 * 公网地址解析。与 cloudflare 适配器保持相同的对象协议，可互换注册。
 */
import {
  checkNgrokApiReachability,
  checkNgrokAuthtokenConfigured,
  checkNgrokAvailable,
  startNgrokQuickTunnel,
} from '../../ngrok-tunnel.js';

import {
  TUNNEL_INTENT_EPHEMERAL_PUBLIC,
  TUNNEL_MODE_QUICK,
  TUNNEL_PROVIDER_NGROK,
  TunnelServiceError,
} from '../types.js';
import { getTunnelDependencyInstallInfo } from '../install-help.js';

/**
 * ngrok 提供商能力声明：默认 quick 模式，无必填字段，支持 sessionTTL，
 * 稳定性标记为 beta（与 cloudflare 的 ga 相区分）。
 */
export const ngrokTunnelProviderCapabilities = {
  provider: TUNNEL_PROVIDER_NGROK,
  defaults: {
    mode: TUNNEL_MODE_QUICK,
    optionDefaults: {},
  },
  modes: [
    {
      key: TUNNEL_MODE_QUICK,
      label: 'Quick Tunnel',
      intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
      requires: [],
      supports: ['sessionTTL'],
      stability: 'beta',
    },
  ],
};

/**
 * 创建 ngrok 隧道提供商实例。
 * @returns {object} 提供商对象：id 为 'ngrok'，含 capabilities、checkAvailability、
 *   diagnose、start、stop、resolvePublicUrl、getMetadata（恒为 null）。
 */
export function createNgrokTunnelProvider() {
  return {
    id: TUNNEL_PROVIDER_NGROK,
    capabilities: ngrokTunnelProviderCapabilities,
    /**
     * 检查 ngrok 依赖是否安装；无论成败都并入安装指引信息
     * （installCommand/installUrl/message），供 UI 直接提示用户。
     */
    checkAvailability: async () => {
      const result = await checkNgrokAvailable();
      if (result.available) {
        return {
          ...result,
          ...getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_NGROK),
        };
      }
      const installInfo = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_NGROK);
      return {
        ...result,
        ...installInfo,
      };
    },
    /**
     * 提供商级诊断：检查 ngrok 安装、authtoken 配置与 ngrok API 可达性三项
     * providerChecks；三者全部通过才判定 quick 模式就绪，否则在 blockers 中
     * 给出待处理提示（不启动隧道）。
     * @returns {Promise<{providerChecks: Array, modes: Array}>}
     */
    diagnose: async () => {
      const dependency = await checkNgrokAvailable();
      const authtoken = await checkNgrokAuthtokenConfigured(dependency.path);
      const network = await checkNgrokApiReachability();
      const installInfo = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_NGROK);
      const startupReady = dependency.available && authtoken.configured && network.reachable;
      const providerChecks = [
        {
          id: 'dependency',
          label: 'ngrok installed',
          status: dependency.available ? 'pass' : 'fail',
          detail: dependency.available
            ? (dependency.version || dependency.path || 'ngrok available')
            : installInfo.message,
        },
        {
          id: 'authtoken',
          label: 'ngrok authtoken configured',
          status: authtoken.configured ? 'pass' : 'fail',
          detail: authtoken.configured
            ? authtoken.detail
            : (authtoken.detail || 'Run: ngrok config add-authtoken <your-ngrok-token>'),
        },
        {
          id: 'network',
          label: 'ngrok API reachable',
          status: network.reachable ? 'pass' : 'fail',
          detail: network.reachable
            ? (network.status ? `HTTP ${network.status}` : 'Reachable')
            : (network.error || 'Could not reach api.ngrok.com'),
        },
      ];

      return {
        providerChecks,
        modes: [
          {
            mode: TUNNEL_MODE_QUICK,
            checks: [
              {
                id: 'startup_readiness',
                label: 'Provider startup readiness',
                status: startupReady ? 'pass' : 'fail',
                detail: startupReady
                  ? 'Provider dependency, auth, and network checks passed.'
                  : 'Resolve provider checks before starting tunnels.',
              },
            ],
            summary: {
              ready: startupReady,
              failures: startupReady ? 0 : 1,
              warnings: 0,
            },
            ready: startupReady,
            blockers: startupReady ? [] : ['Resolve provider checks before starting tunnels.'],
          },
        ],
      };
    },
    /**
     * 启动 quick 隧道：仅支持 quick 模式，其他模式抛 mode_unsupported；
     * 以 context.activePort 作为本地转发目标端口。
     * @param {object} request 归一化后的启动请求
     * @param {object} context 服务层注入的上下文（activePort）
     * @throws {TunnelServiceError} 模式不是 quick 时抛 mode_unsupported
     */
    start: async (request, context = {}) => {
      if (request.mode !== TUNNEL_MODE_QUICK) {
        throw new TunnelServiceError('mode_unsupported', `Ngrok only supports '${TUNNEL_MODE_QUICK}' mode right now`);
      }
      return startNgrokQuickTunnel({ port: context.activePort });
    },
    /** 停止给定控制器（委托控制器自身的 stop，无额外清理）。 */
    stop: (controller) => {
      controller?.stop?.();
    },
    /** 从控制器解析公网地址；控制器缺失或未提供值时返回 null。 */
    resolvePublicUrl: (controller) => controller?.getPublicUrl?.() ?? null,
    /** ngrok 不提供额外元数据，恒返回 null。 */
    getMetadata: () => null,
  };
}
