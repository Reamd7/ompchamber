/**
 * Cloudflare 隧道提供商适配器。
 *
 * 基于 cloudflare-tunnel.js 的底层能力实现提供商协议（registry 要求的
 * start/stop/checkAvailability/resolvePublicUrl 及可选的 diagnose/getMetadata）：
 * 声明 quick / managed-remote / managed-local 三种模式的能力，诊断时检查
 * cloudflared 安装与 Cloudflare API 可达性，并按模式分发启动到底层实现。
 */
import {
  checkCloudflareApiReachability,
  checkCloudflaredAvailable,
  inspectManagedLocalCloudflareConfig,
  normalizeCloudflareTunnelHostname,
  startCloudflareManagedLocalTunnel,
  startCloudflareManagedRemoteTunnel,
  startCloudflareQuickTunnel,
} from '../../cloudflare-tunnel.js';

import {
  TUNNEL_INTENT_EPHEMERAL_PUBLIC,
  TUNNEL_INTENT_PERSISTENT_PUBLIC,
  TUNNEL_MODE_MANAGED_LOCAL,
  TUNNEL_MODE_MANAGED_REMOTE,
  TUNNEL_MODE_QUICK,
  TUNNEL_PROVIDER_CLOUDFLARE,
  TunnelServiceError,
} from '../types.js';
import { getTunnelDependencyInstallInfo } from '../install-help.js';

/**
 * Cloudflare 提供商能力声明：默认 quick 模式。modes 数组描述三种模式的
 * intent、必填字段（managed-remote 需要 token + hostname）、支持的能力项
 * （sessionTTL / customDomain / configFile）与稳定性等级，供注册表、
 * doctor 诊断与 validateTunnelStartRequest 校验使用。
 */
export const cloudflareTunnelProviderCapabilities = {
  provider: TUNNEL_PROVIDER_CLOUDFLARE,
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
      stability: 'ga',
    },
    {
      key: TUNNEL_MODE_MANAGED_REMOTE,
      label: 'Managed Remote Tunnel',
      intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
      requires: ['token', 'hostname'],
      supports: ['customDomain', 'sessionTTL'],
      stability: 'ga',
    },
    {
      key: TUNNEL_MODE_MANAGED_LOCAL,
      label: 'Managed Local Tunnel',
      intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
      requires: [],
      supports: ['configFile', 'customDomain', 'sessionTTL'],
      stability: 'ga',
    },
  ],
};

/**
 * 创建 Cloudflare 隧道提供商实例。
 * @returns {object} 提供商对象：id 为 'cloudflare'，含 capabilities、
 *   checkAvailability、diagnose、start、stop、resolvePublicUrl、getMetadata。
 */
export function createCloudflareTunnelProvider() {
    /**
     * 校验托管远端 token 的形态：非字符串、为空或包含空白字符均判失败，
     * 返回 { ok, detail } 供诊断结果展示。
     * @param {*} value 待校验的 token
     * @returns {{ok: boolean, detail: string}}
     */
  const validateTokenShape = (value) => {
    if (typeof value !== 'string') {
      return { ok: false, detail: 'Managed remote token is missing.' };
    }
    const trimmed = value.trim();
    if (!trimmed) {
      return { ok: false, detail: 'Managed remote token is missing.' };
    }
    if (/\s/.test(trimmed)) {
      return { ok: false, detail: 'Managed remote token has whitespace; provide the raw token value.' };
    }
    return { ok: true, detail: 'Managed remote token looks valid.' };
  };

    /**
     * 汇总检查项为 summary：统计 fail/warn 数量，无 fail 即 ready。
     */
  const createModeSummary = (checks) => {
    const failures = checks.filter((entry) => entry.status === 'fail').length;
    const warnings = checks.filter((entry) => entry.status === 'warn').length;
    return {
      ready: failures === 0,
      failures,
      warnings,
    };
  };

    /**
     * 组装单个模式的诊断结果（mode/checks/summary/ready/blockers）；
     * blockers 剔除 startup_readiness 项，仅保留用户可自行处理的阻塞原因。
     */
  const describeMode = ({ mode, checks }) => {
    const summary = createModeSummary(checks);
    const blockers = checks
      .filter((entry) => entry.status === 'fail' && entry.id !== 'startup_readiness')
      .map((entry) => entry.detail || entry.label || entry.id);
    return {
      mode,
      checks,
      summary,
      ready: summary.ready,
      blockers,
    };
  };

  return {
    id: TUNNEL_PROVIDER_CLOUDFLARE,
    capabilities: cloudflareTunnelProviderCapabilities,
    /**
     * 检查 cloudflared 依赖是否安装；无论成败都并入安装指引信息
     * （installCommand/installUrl/message），供 UI 直接提示用户。
     */
    checkAvailability: async () => {
      const result = await checkCloudflaredAvailable();
      if (result.available) {
        return {
          ...result,
          ...getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE),
        };
      }
      const installInfo = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE);
      return {
        ...result,
        ...installInfo,
      };
    },
    /**
     * 提供商级诊断：检查 cloudflared 安装与 Cloudflare API 可达性得到 providerChecks，
     * 再为三种模式分别生成检查项——quick（依赖 + 边缘网络可达）、managed-remote
     * （hostname 与 token 形态；未显式传入且存在已保存预设时可豁免缺失项）、
     * managed-local（配置文件检查）。request.mode 非空时仅返回该模式的结果。
     * @param {object} request 诊断输入（mode/configPath/hostname/token/
     *   tokenProvided/hostnameProvided/hasSavedManagedRemoteProfile）
     * @returns {Promise<{providerChecks: Array, modes: Array}>}
     */
    diagnose: async (request = {}) => {
      const dependency = await checkCloudflaredAvailable();
      const network = await checkCloudflareApiReachability();
      const installInfo = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE);

      const providerChecks = [
        {
          id: 'dependency',
          label: 'cloudflared installed',
          status: dependency.available ? 'pass' : 'fail',
          detail: dependency.available
            ? (dependency.version || dependency.path || 'cloudflared available')
            : installInfo.message,
        },
        {
          id: 'network',
          label: 'Cloudflare API reachable',
          status: network.reachable ? 'pass' : 'fail',
          detail: network.reachable
            ? (network.status ? `HTTP ${network.status}` : 'Reachable')
            : (network.error || 'Could not reach api.trycloudflare.com'),
        },
      ];

      const startupReady = dependency.available && network.reachable;
      const startupDetail = startupReady
        ? 'Provider dependency and network checks passed.'
        : 'Resolve provider checks before starting tunnels.';

      const quickChecks = [
        {
          id: 'startup_readiness',
          label: 'Provider startup readiness',
          status: startupReady ? 'pass' : 'fail',
          detail: startupDetail,
        },
        {
          id: 'quick_mode_prerequisites',
          label: 'Quick tunnel prerequisites',
          status: network.reachable ? 'pass' : 'fail',
          detail: network.reachable
            ? 'Cloudflare edge is reachable for quick tunnels.'
            : 'Cloudflare edge is not reachable for quick tunnels.',
        },
      ];

      const managedLocalInspection = inspectManagedLocalCloudflareConfig({
        configPath: request.configPath,
        hostname: request.hostname,
      });
      const managedLocalChecks = [
        {
          id: 'startup_readiness',
          label: 'Provider startup readiness',
          status: startupReady ? 'pass' : 'fail',
          detail: startupDetail,
        },
        {
          id: 'managed_local_config',
          label: 'Managed local config',
          status: managedLocalInspection.ok ? 'pass' : 'fail',
          detail: managedLocalInspection.ok
            ? `${managedLocalInspection.effectiveConfigPath}${managedLocalInspection.resolvedHostname ? ` (${managedLocalInspection.resolvedHostname})` : ''}`
            : managedLocalInspection.error,
        },
      ];

      const normalizedHost = normalizeCloudflareTunnelHostname(request.hostname);
      const hostnameMissing = !normalizedHost;
      const remoteTokenValidation = validateTokenShape(request.token);
      const tokenMissing = typeof request.token !== 'string' || request.token.trim().length === 0;
      const hasSavedManagedRemoteProfile = request.hasSavedManagedRemoteProfile === true;
      const tokenProvided = request.tokenProvided === true;
      const hostnameProvided = request.hostnameProvided === true;
      const hasExplicitManagedRemoteInput = tokenProvided || hostnameProvided;
      const canUseSavedProfileForHostname = !hasExplicitManagedRemoteInput && hostnameMissing && hasSavedManagedRemoteProfile;
      const canUseSavedProfileForToken = !hasExplicitManagedRemoteInput && tokenMissing && hasSavedManagedRemoteProfile;
      const savedProfileReadyDetail = 'at least one saved profile present';
      const managedRemoteChecks = [
        {
          id: 'startup_readiness',
          label: 'Provider startup readiness',
          status: startupReady ? 'pass' : 'fail',
          detail: startupDetail,
        },
        {
          id: 'managed_remote_hostname',
          label: 'Managed remote hostname',
          status: normalizedHost || canUseSavedProfileForHostname ? 'pass' : 'fail',
          detail: normalizedHost
            ? normalizedHost
            : canUseSavedProfileForHostname
              ? savedProfileReadyDetail
              : 'Managed remote hostname is required (use --hostname).',
        },
        {
          id: 'managed_remote_token',
          label: 'Managed remote token',
          status: remoteTokenValidation.ok || canUseSavedProfileForToken ? 'pass' : 'fail',
          detail: canUseSavedProfileForToken
            ? savedProfileReadyDetail
            : remoteTokenValidation.detail,
        },
      ];

      const allModes = [
        describeMode({ mode: TUNNEL_MODE_QUICK, checks: quickChecks }),
        describeMode({ mode: TUNNEL_MODE_MANAGED_REMOTE, checks: managedRemoteChecks }),
        describeMode({ mode: TUNNEL_MODE_MANAGED_LOCAL, checks: managedLocalChecks }),
      ];

      const modeFilter = typeof request.mode === 'string' && request.mode.trim().length > 0
        ? request.mode.trim().toLowerCase()
        : null;
      const modes = modeFilter ? allModes.filter((entry) => entry.mode === modeFilter) : allModes;

      return {
        providerChecks,
        modes,
      };
    },
    /**
     * 按模式分发启动：managed-remote 用 token + hostname；managed-local 用
     * configPath（hostname 可选）；quick 需要 context.originUrl（本地服务地址），
     * 缺失时抛 validation_error，否则以 originUrl + activePort 启动临时隧道。
     * @param {object} request 归一化后的启动请求
     * @param {object} context 服务层注入的上下文（activePort/originUrl 等）
     * @throws {TunnelServiceError} quick 模式缺 originUrl 时抛 validation_error
     */
    start: async (request, context = {}) => {
      if (request.mode === TUNNEL_MODE_MANAGED_REMOTE) {
        return startCloudflareManagedRemoteTunnel({
          token: request.token,
          hostname: request.hostname,
        });
      }

      if (request.mode === TUNNEL_MODE_MANAGED_LOCAL) {
        return startCloudflareManagedLocalTunnel({
          configPath: request.configPath,
          hostname: request.hostname,
        });
      }

      if (!context.originUrl) {
        throw new TunnelServiceError('validation_error', 'originUrl is required for quick tunnel mode');
      }

      return startCloudflareQuickTunnel({
        originUrl: context.originUrl,
        port: context.activePort,
      });
    },
    /** 停止给定控制器（委托控制器自身的 stop，无额外清理）。 */
    stop: (controller) => {
      controller?.stop?.();
    },
    /** 从控制器解析公网地址；控制器缺失或未提供值时返回 null。 */
    resolvePublicUrl: (controller) => controller?.getPublicUrl?.() ?? null,
    /**
     * 返回控制器元数据：生效的配置文件路径与解析出的 hostname
     * （managed-local 诊断展示用），无值时各项为 null。
     */
    getMetadata: (controller) => ({
      configPath: controller?.getEffectiveConfigPath?.() ?? null,
      resolvedHostname: controller?.getResolvedHostname?.() ?? null,
    }),
  };
}
