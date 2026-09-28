/**
 * 隧道装配运行时（tunnel wiring runtime）。
 *
 * 把隧道服务层（createTunnelService）与隧道 HTTP 路由层
 * （createTunnelRoutesRuntime）组装成一个整体：初始化时在 Express app 上
 * 注册隧道相关路由，并返回携带服务句柄与活动端口读写器的装配结果。
 *
 * 采用依赖注入风格：crypto、URL、provider 注册表、磁盘配置读取、各种
 * normalize 函数与常量全部由调用方通过 dependencies 传入，便于测试替换。
 */
import { printTunnelWarning } from '../cloudflare-tunnel.js';
import { createTunnelService } from '../tunnels/index.js';
import { createTunnelRoutesRuntime } from '../tunnels/routes.js';

/**
 * 创建隧道装配运行时。
 * @param {object} dependencies - 全部外部依赖（tunnelProviderRegistry、
 *   tunnelAuthController、设置/托管隧道配置的磁盘读取、normalize 函数、
 *   模式常量以及活动 controller 与运行时状态的存取器）。
 * @returns {{ initialize: Function }} 只暴露 initialize；后者完成服务与
 *   路由的创建并返回装配句柄。
 */
export const createTunnelWiringRuntime = (dependencies) => {
  const {
    crypto,
    URL,
    tunnelProviderRegistry,
    tunnelAuthController,
    readSettingsFromDiskMigrated,
    readManagedRemoteTunnelConfigFromDisk,
    normalizeTunnelProvider,
    normalizeTunnelMode,
    normalizeOptionalPath,
    normalizeManagedRemoteTunnelHostname,
    normalizeTunnelBootstrapTtlMs,
    normalizeTunnelSessionTtlMs,
    isSupportedTunnelMode,
    upsertManagedRemoteTunnelToken,
    resolveManagedRemoteTunnelToken,
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_LOCAL,
    TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_PROVIDER_CLOUDFLARE,
    TunnelServiceError,
    getActiveTunnelController,
    setActiveTunnelController,
    getRuntimeManagedRemoteTunnelHostname,
    setRuntimeManagedRemoteTunnelHostname,
    getRuntimeManagedRemoteTunnelToken,
    setRuntimeManagedRemoteTunnelToken,
  } = dependencies;

  /**
   * 在给定 Express app 上装配隧道能力。
   *
   * 先创建隧道服务（管理活动 controller、读取活动端口、输出 quick tunnel
   * 警告），再创建隧道路由运行时并把路由挂到 app 上；活动端口以闭包变量
   * 保存，监听端口变化后可通过返回的 setActivePort 更新。
   * @param {object} app - Express 应用实例。
   * @param {number} initialPort - 当前监听端口，作为活动端口的初始值。
   * @returns 携带 tunnelService、startTunnelWithNormalizedRequest 代理与
   *   活动端口读写器的装配句柄。
   */
  const initialize = (app, initialPort) => {
    let activePort = initialPort;

    // 隧道服务：统一管理活动 tunnel controller 的创建与停止、端口读取和
    // quick tunnel 警告回调（printTunnelWarning）。
    const tunnelService = createTunnelService({
      registry: tunnelProviderRegistry,
      getController: getActiveTunnelController,
      setController: setActiveTunnelController,
      getActivePort: () => activePort,
      onQuickTunnelWarning: () => {
        printTunnelWarning();
      },
    });

    // 隧道路由运行时：接收全部隧道依赖，负责在 app 上注册隧道 HTTP 路由。
    const tunnelRoutesRuntime = createTunnelRoutesRuntime({
      crypto,
      URL,
      tunnelService,
      tunnelProviderRegistry,
      tunnelAuthController,
      readSettingsFromDiskMigrated,
      readManagedRemoteTunnelConfigFromDisk,
      normalizeTunnelProvider,
      normalizeTunnelMode,
      normalizeOptionalPath,
      normalizeManagedRemoteTunnelHostname,
      normalizeTunnelBootstrapTtlMs,
      normalizeTunnelSessionTtlMs,
      isSupportedTunnelMode,
      upsertManagedRemoteTunnelToken,
      resolveManagedRemoteTunnelToken,
      TUNNEL_MODE_QUICK,
      TUNNEL_MODE_MANAGED_LOCAL,
      TUNNEL_MODE_MANAGED_REMOTE,
      TUNNEL_PROVIDER_CLOUDFLARE,
      TunnelServiceError,
      getActivePort: () => activePort,
      getRuntimeManagedRemoteTunnelHostname,
      setRuntimeManagedRemoteTunnelHostname,
      getRuntimeManagedRemoteTunnelToken,
      setRuntimeManagedRemoteTunnelToken,
      getActiveTunnelController,
      setActiveTunnelController,
    });

    tunnelRoutesRuntime.registerRoutes(app);

    return {
      tunnelService,
      // 透传路由运行时的规范化启动入口，保持调用方与路由层解耦
      startTunnelWithNormalizedRequest: (...args) => tunnelRoutesRuntime.startTunnelWithNormalizedRequest(...args),
      // 读取当前活动端口（供健康检查、URL 构建等使用）
      getActivePort: () => activePort,
      // 更新活动端口（监听端口变化后同步给隧道层）
      setActivePort: (value) => {
        activePort = value;
      },
    };
  };

  return {
    initialize,
  };
};
