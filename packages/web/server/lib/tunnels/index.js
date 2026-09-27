/**
 * 隧道服务（TunnelService）模块。
 *
 * 在提供商注册表（registry.js）之上实现与具体提供商无关的隧道生命周期编排：
 * 依赖可用性检查、启动（含互斥锁、请求归一化与能力校验、旧隧道替换、
 * 依赖缺失/启动失败的错误包装）、停止、公网地址与提供商元数据查询。
 * 活动隧道控制器通过注入的 getController/setController 存取，便于宿主管理。
 */
import {
  TUNNEL_MODE_QUICK,
  TUNNEL_PROVIDER_CLOUDFLARE,
  TunnelServiceError,
  normalizeTunnelStartRequest,
  validateTunnelStartRequest,
} from './types.js';
import { getTunnelDependencyInstallInfo } from './install-help.js';

/**
 * 创建隧道服务实例。
 * @param {object} options registry 提供商注册表（必需，缺失即抛错）；
 *   getController/setController 活动控制器的存取器；getActivePort 本地服务端口
 *   （用于推导 quick 隧道的 originUrl）；onQuickTunnelWarning quick 模式警告回调。
 * @returns {{start: Function, stop: Function, checkAvailability: Function, getPublicUrl: Function, getProviderMetadata: Function, resolveActiveMode: Function, resolveActiveProvider: Function}}
 * @throws {Error} 未提供 registry 时抛出
 */
export function createTunnelService({
  registry,
  getController,
  setController,
  getActivePort,
  onQuickTunnelWarning,
}) {
  if (!registry) {
    throw new Error('Tunnel service requires a provider registry');
  }

  /**
   * 返回当前活动控制器上的模式；无控制器或模式不是字符串时返回 null。
   * @returns {string|null}
   */
  const resolveActiveMode = () => {
    const controller = getController();
    if (!controller || typeof controller.mode !== 'string') {
      return null;
    }
    return controller.mode;
  };

  /**
   * 返回当前活动控制器上的提供商 id；无控制器或值不是字符串时返回 null。
   * @returns {string|null}
   */
  const resolveActiveProvider = () => {
    const controller = getController();
    if (!controller || typeof controller.provider !== 'string') {
      return null;
    }
    return controller.provider;
  };

  /**
   * 停止当前活动隧道：优先委托该提供商的 stop（可做专属清理），
   * 提供商未提供 stop 时退回控制器自身的 stop；随后将控制器置空。
   * @returns {boolean} 是否确有隧道被停止（无活动隧道返回 false）
   */
  const stop = () => {
    const controller = getController();
    if (!controller) {
      return false;
    }

    const providerId = typeof controller.provider === 'string' ? controller.provider : '';
    const provider = providerId ? registry.get(providerId) : null;
    if (provider?.stop) {
      provider.stop(controller);
    } else {
      controller.stop?.();
    }
    setController(null);
    return true;
  };

  /**
   * 检查指定提供商 CLI 依赖的可用性（版本、路径、安装指引等），直接透传提供商结果。
   * @param {string} providerId 提供商 id
   * @throws {TunnelServiceError} provider_unsupported：注册表中不存在该提供商
   */
  const checkAvailability = async (providerId) => {
    const provider = registry.get(providerId);
    if (!provider) {
      throw new TunnelServiceError('provider_unsupported', `Unsupported tunnel provider: ${providerId}`);
    }
    const result = await provider.checkAvailability();
    return result;
  };

  // Mutex to prevent concurrent tunnel starts from orphaning child processes.
  // 启动互斥锁：防止并发启动隧道导致旧子进程被遗留成孤儿。
  let startLock = Promise.resolve();

  /**
   * 启动（或复用）隧道。流程：排队获取互斥锁 → 归一化请求 → 查找提供商并校验其
   * capabilities → 若当前已有公网地址但模式/提供商发生变化则先 stop 旧隧道 →
   * 依赖不可用时抛 missing_dependency（cloudflare 附加安装指引文案）→ 调用
   * provider.start（TunnelServiceError 原样透传，其余异常包装为 startup_failed）→
   * 启动后解析公网地址（拿不到则回滚 stop 并抛 startup_failed）→ quick 模式触发
   * onQuickTunnelWarning 回调。锁在 finally 中释放，异常不会造成死锁。
   * @param {object} rawRequest 原始启动请求（provider/mode/intent/token/hostname/configPath）
   * @param {object} options 透传给 provider.start 的附加选项
   * @returns {Promise<{publicUrl: string, request: object, activeMode: string, provider: string, providerMetadata: object|null}>}
   * @throws {TunnelServiceError} provider_unsupported / mode_unsupported /
   *   validation_error / missing_dependency / startup_failed
   */
  const start = async (rawRequest, options = {}) => {
    let releaseLock;
    const lockPromise = new Promise((resolve) => { releaseLock = resolve; });
    const previousLock = startLock;
    startLock = lockPromise;

    await previousLock;

    try {
      const request = normalizeTunnelStartRequest(rawRequest);
      const provider = registry.get(request.provider);

      if (!provider) {
        throw new TunnelServiceError('provider_unsupported', `Unsupported tunnel provider: ${request.provider}`);
      }

      validateTunnelStartRequest(request, provider.capabilities);

      let publicUrl = provider.resolvePublicUrl(getController());
      const activeMode = resolveActiveMode();
      const activeProvider = resolveActiveProvider();

      if (publicUrl && (activeMode !== request.mode || activeProvider !== request.provider)) {
        stop();
        publicUrl = null;
      }

      if (!publicUrl) {
        const availability = await provider.checkAvailability();
        if (!availability?.available) {
          const missingDependencyMessage = typeof availability?.message === 'string' && availability.message.trim().length > 0
            ? availability.message
            : (request.provider === TUNNEL_PROVIDER_CLOUDFLARE
              ? getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE).message
              : `Required dependency for provider '${request.provider}' is missing`);
          throw new TunnelServiceError('missing_dependency', missingDependencyMessage);
        }

        const activePort = Number.isFinite(getActivePort?.()) ? getActivePort() : null;
        const originUrl = activePort !== null ? `http://127.0.0.1:${activePort}` : undefined;

        let controller;
        try {
          controller = await provider.start(request, {
            activePort,
            originUrl,
            ...options,
          });
        } catch (error) {
          if (error instanceof TunnelServiceError) {
            throw error;
          }
          const message = error instanceof Error && error.message.trim().length > 0
            ? error.message
            : 'Failed to start tunnel';
          throw new TunnelServiceError('startup_failed', message);
        }
        controller.provider = request.provider;
        setController(controller);

        publicUrl = provider.resolvePublicUrl(controller);
        if (!publicUrl) {
          stop();
          throw new TunnelServiceError('startup_failed', 'Tunnel started but no public URL was assigned');
        }

        if (request.mode === TUNNEL_MODE_QUICK) {
          onQuickTunnelWarning?.();
        }
      }

      return {
        publicUrl,
        request,
        activeMode: request.mode,
        provider: request.provider,
        providerMetadata: provider.getMetadata?.(getController()) ?? null,
      };
    } finally {
      releaseLock();
    }
  };

  /**
   * 返回当前活动隧道的公网地址；无控制器返回 null；
   * 控制器的提供商未注册时退回控制器自带的 getPublicUrl。
   * @returns {string|null}
   */
  const getPublicUrl = () => {
    const controller = getController();
    if (!controller) {
      return null;
    }
    const provider = registry.get(controller.provider);
    if (!provider) {
      return controller.getPublicUrl?.() ?? null;
    }
    return provider.resolvePublicUrl(controller);
  };

  /**
   * 返回当前活动隧道的提供商元数据（如生效配置路径、解析出的 hostname）；
   * 无活动隧道或提供商未实现 getMetadata 时返回 null。
   * @returns {object|null}
   */
  const getProviderMetadata = () => {
    const controller = getController();
    if (!controller) {
      return null;
    }
    const provider = registry.get(controller.provider);
    return provider?.getMetadata?.(controller) ?? null;
  };

  // 服务公开 API：生命周期与状态查询。
  return {
    start,
    stop,
    checkAvailability,
    getPublicUrl,
    getProviderMetadata,
    resolveActiveMode,
    resolveActiveProvider,
  };
}
