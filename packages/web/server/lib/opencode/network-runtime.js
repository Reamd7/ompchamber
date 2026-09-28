/**
 * OpenCode 网络运行时：维护与（本地或远程托管的）OpenCode 引擎之间的
 * URL 构造与健康检查。持有可变的 state（openCodePort、openCodeBaseUrl、
 * API 前缀探测状态等）：端口未知时 buildOpenCodeUrl 会抛错，调用方
 * （proxy、routes 等）依赖这一失败信号区分“引擎尚未启动”；
 * waitForReady 轮询引擎的 /global/health 端点直到其报告 healthy。
 */

/**
 * 创建 OpenCode 网络运行时。
 *
 * @param {object} deps
 * @param {object} deps.state 可变的引擎状态对象（openCodePort / openCodeBaseUrl / API 前缀字段）
 * @param {Function} deps.getOpenCodeAuthHeaders 返回请求引擎所需的认证头
 * @param {string} [deps.configuredOpenCodeHostname] 配置的绑定主机名（默认 127.0.0.1）
 * @returns {{ waitForReady: Function, normalizeApiPrefix: Function, setDetectedOpenCodeApiPrefix: Function,
 *   buildOpenCodeUrl: Function, ensureOpenCodeApiPrefix: Function, scheduleOpenCodeApiDetection: Function }}
 */
export const createOpenCodeNetworkRuntime = (deps) => {
  const {
    state,
    getOpenCodeAuthHeaders,
    configuredOpenCodeHostname = '127.0.0.1',
  } = deps;

  /**
   * 把配置的主机名规范成可连接形式：空值回退 127.0.0.1；通配地址
   * （0.0.0.0 / ::）改连本机回环；裸 IPv6 地址补上方括号；已带方括号
   * 的原样保留。
   */
  const resolveConnectHostname = () => {
    const raw = typeof configuredOpenCodeHostname === 'string' ? configuredOpenCodeHostname.trim() : '';
    const hostname = raw || '127.0.0.1';
    if (hostname === '0.0.0.0' || hostname === '::' || hostname === '[::]') {
      return '127.0.0.1';
    }
    if (hostname.startsWith('[') && hostname.endsWith(']')) {
      return hostname;
    }
    return hostname.includes(':') ? `[${hostname}]` : hostname;
  };

  /**
   * 规范化 OpenCode API 的路径前缀：空串或 "/" 归一为空串；传入完整
   * URL 时取其 pathname 再递归处理（解析失败返回空串）；其余情况确保
   * 以 "/" 开头并去掉末尾 "/"。
   */
  const normalizeApiPrefix = (prefix) => {
    if (!prefix) {
      return '';
    }

    if (prefix.includes('://')) {
      try {
        const parsed = new URL(prefix);
        return normalizeApiPrefix(parsed.pathname);
      } catch {
        return '';
      }
    }

    const trimmed = prefix.trim();
    if (!trimmed || trimmed === '/') {
      return '';
    }
    const withLeading = trimmed.startsWith('/') ? trimmed : `/${trimmed}`;
    return withLeading.endsWith('/') ? withLeading.slice(0, -1) : withLeading;
  };

  /**
   * 轮询引擎健康端点直到就绪或超时。
   *
   * 每轮请求 <url>/global/health（单次 3 秒超时、携带认证头），响应为
   * ok 且 body.healthy === true 即返回 true；否则间隔 100ms 重试，直到
   * 总超时（默认 10 秒）耗尽返回 false。网络异常视为未就绪并继续轮询。
   *
   * @param {string} url 引擎基地址
   * @param {number} [timeoutMs] 总超时毫秒数，默认 10000
   * @returns {Promise<boolean>} 是否在超时内观测到 healthy
   */
  const waitForReady = async (url, timeoutMs = 10000) => {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      let timeout = null;
      try {
        const controller = new AbortController();
        timeout = setTimeout(() => controller.abort(), 3000);
        const response = await fetch(`${url.replace(/\/+$/, '')}/global/health`, {
          method: 'GET',
          headers: {
            Accept: 'application/json',
            ...getOpenCodeAuthHeaders(),
          },
          signal: controller.signal,
        });
        clearTimeout(timeout);
        timeout = null;

        if (response.ok) {
          const body = await response.json().catch(() => null);
          if (body?.healthy === true) {
            return true;
          }
        }
      } catch {
      } finally {
        if (timeout) {
          clearTimeout(timeout);
        }
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    return false;
  };

  /**
   * 标记 API 前缀探测完成：前缀固定为空串（引擎直接挂在根路径），同时
   * 清理可能存在的探测定时器，避免重复调度。
   */
  const setDetectedOpenCodeApiPrefix = () => {
    state.openCodeApiPrefix = '';
    state.openCodeApiPrefixDetected = true;
    if (state.openCodeApiDetectionTimer) {
      clearTimeout(state.openCodeApiDetectionTimer);
      state.openCodeApiDetectionTimer = null;
    }
  };

  /**
   * 构造指向 OpenCode 引擎的完整 URL：base + 前缀 + 规范化路径。base
   * 优先取 state.openCodeBaseUrl（外部引擎地址），否则用连接主机名 +
   * 端口拼出本地地址；路径确保以 "/" 开头。端口未知时抛错（见模块说明）。
   */
  const buildOpenCodeUrl = (path, prefixOverride) => {
    if (!state.openCodePort) {
      throw new Error('OpenCode port is not available');
    }
    const normalizedPath = path.startsWith('/') ? path : `/${path}`;
    const prefix = normalizeApiPrefix(prefixOverride !== undefined ? prefixOverride : '');
    const fullPath = `${prefix}${normalizedPath}`;
    const base = state.openCodeBaseUrl ?? `http://${resolveConnectHostname()}:${state.openCodePort}`;
    return `${base}${fullPath}`;
  };

  /**
   * 立即执行一次 API 前缀“探测”：当前实现无真实探测逻辑，直接置空前缀
   * 并标记已探测，恒返回 true。
   */
  const detectOpenCodeApiPrefix = () => {
    state.openCodeApiPrefixDetected = true;
    state.openCodeApiPrefix = '';
    return true;
  };

  /** 确保 API 前缀已确定（未探测则触发探测）；当前等价于 detectOpenCodeApiPrefix。 */
  const ensureOpenCodeApiPrefix = () => detectOpenCodeApiPrefix();

  /**
   * 调度一次异步的 API 前缀探测。当前实现为空操作（前缀探测已被简化），
   * 保留导出以兼容既有调用方签名。
   */
  const scheduleOpenCodeApiDetection = () => {
    return;
  };

  return {
    waitForReady,
    normalizeApiPrefix,
    setDetectedOpenCodeApiPrefix,
    buildOpenCodeUrl,
    ensureOpenCodeApiPrefix,
    scheduleOpenCodeApiDetection,
  };
};
