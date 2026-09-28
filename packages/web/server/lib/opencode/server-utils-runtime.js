/**
 * 服务端工具运行时工厂。
 *
 * 聚合 OpenCode 端口状态管理（setOpenCodePort/waitForOpenCodePort）、PATH
 * 构建（登录 shell PATH 合并与 Windows 工具链目录发现）、SSE data 帧解析、
 * agents/providers/models 快照拉取，以及 OpenCode 反向代理挂载（setupProxy）。
 * 全部依赖经 dependencies 注入。
 */
import { registerOpenCodeProxy } from './proxy.js';
import { pathLooksUserConfigured, mergePathValues } from './path-utils.js';

/**
 * 创建服务端工具运行时：dependencies 注入 fs/os/path/process、就绪宽限与
 * 长请求超时、OpenCode 端口与就绪状态的存取回调、URL/鉴权头构造、代理参数、
 * UI 通知客户端与登录 shell PATH 等；返回端口管理、PATH 构建、SSE 解析、
 * 快照拉取与代理挂载方法。
 */
export const createServerUtilsRuntime = (dependencies) => {
  const {
    fs,
    os,
    path,
    process,
    openCodeReadyGraceMs,
    longRequestTimeoutMs,
    getRuntime,
    getOpenCodeAuthHeaders,
    buildOpenCodeUrl,
    ensureOpenCodeApiPrefix,
    getUpstreamStallTimeoutMs,
    getUiNotificationClients,
    getOpenCodePort,
    setOpenCodePortState,
    syncToHmrState,
    markOpenCodeNotReady,
    setOpenCodeNotReadySince,
    clearLastOpenCodeError,
    getLoginShellPath,
  } = dependencies;

  /**
   * 记录探测到的 OpenCode 端口：非法或非正数直接忽略；端口变化（或首次从
   * null 变为已知）时更新状态、同步 HMR 并打日志，端口变化还会标记 OpenCode
   * 未就绪并重置未就绪起始时间；最后清除上一次的 OpenCode 错误记录。
   */
  const setOpenCodePort = (port) => {
    if (!Number.isFinite(port) || port <= 0) {
      return;
    }

    const numericPort = Math.trunc(port);
    const currentPort = getOpenCodePort();
    const portChanged = currentPort !== numericPort;

    if (portChanged || currentPort === null) {
      setOpenCodePortState(numericPort);
      syncToHmrState();
      console.log(`Detected OpenCode port: ${numericPort}`);

      if (portChanged) {
        markOpenCodeNotReady();
      }
      setOpenCodeNotReadySince(Date.now());
    }

    clearLastOpenCodeError();
  };

  /**
   * 轮询等待 OpenCode 端口就绪（默认最多 15 秒、每 50ms 检查一次）：
   * 已知端口立即返回，超时抛出 Error。
   */
  const waitForOpenCodePort = async (timeoutMs = 15000) => {
    if (getOpenCodePort() !== null) {
      return getOpenCodePort();
    }

    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      if (getOpenCodePort() !== null) {
        return getOpenCodePort();
      }
    }

    throw new Error('Timed out waiting for OpenCode port');
  };

  /** 大小写不敏感地读取环境变量：先精确匹配键名，再按小写比较；未命中返回空串。 */
  const getEnvValue = (name) => {
    const env = process.env || {};
    if (typeof env[name] === 'string') return env[name];
    const key = Object.keys(env).find((candidate) => candidate.toLowerCase() === name.toLowerCase());
    return key && typeof env[key] === 'string' ? env[key] : '';
  };

  /**
   * 收集 Windows 上各包管理器与运行时的可执行目录（npm、nodejs、pnpm、bun、
   * volta、yarn、scoop、chocolatey、WindowsApps、~/.opencode/bin 等）：依据
   * 环境变量推断根路径，去重并仅保留真实存在的目录，用 path.delimiter 拼接；
   * 非 Windows 平台返回空串。
   */
  const buildWindowsManagedToolchainPath = () => {
    if (process.platform !== 'win32') return '';

    const home = os.homedir();
    const userProfile = getEnvValue('USERPROFILE') || home;
    const appData = getEnvValue('APPDATA') || (userProfile ? path.join(userProfile, 'AppData', 'Roaming') : '');
    const localAppData = getEnvValue('LOCALAPPDATA') || (userProfile ? path.join(userProfile, 'AppData', 'Local') : '');
    const programFiles = getEnvValue('ProgramFiles') || 'C:\\Program Files';
    const programFilesX86 = getEnvValue('ProgramFiles(x86)');
    const programData = getEnvValue('ProgramData') || 'C:\\ProgramData';
    const bunInstall = getEnvValue('BUN_INSTALL');
    const voltaHome = getEnvValue('VOLTA_HOME');
    const scoop = getEnvValue('SCOOP');
    const scoopGlobal = getEnvValue('SCOOP_GLOBAL');

    const candidates = [
      path.join(appData, 'npm'),
      path.join(programFiles, 'nodejs'),
      programFilesX86 ? path.join(programFilesX86, 'nodejs') : '',
      path.join(localAppData, 'Programs', 'nodejs'),
      getEnvValue('PNPM_HOME'),
      path.join(localAppData, 'pnpm'),
      bunInstall ? path.join(bunInstall, 'bin') : '',
      path.join(userProfile, '.bun', 'bin'),
      voltaHome ? path.join(voltaHome, 'bin') : '',
      path.join(localAppData, 'Volta', 'bin'),
      path.join(localAppData, 'Yarn', 'bin'),
      path.join(localAppData, 'Yarn', 'Data', 'global', 'node_modules', '.bin'),
      scoop ? path.join(scoop, 'shims') : '',
      path.join(userProfile, 'scoop', 'shims'),
      scoopGlobal ? path.join(scoopGlobal, 'shims') : '',
      path.join(programData, 'chocolatey', 'bin'),
      path.join(localAppData, 'Microsoft', 'WindowsApps'),
      path.join(userProfile, '.opencode', 'bin'),
      path.join(userProfile, '.local', 'bin'),
    ];

    const seen = new Set();
    const existing = [];
    for (const candidate of candidates) {
      const trimmed = typeof candidate === 'string' ? candidate.trim() : '';
      if (!trimmed) continue;
      const normalized = trimmed.toLowerCase();
      if (seen.has(normalized)) continue;
      seen.add(normalized);
      try {
        if (fs.existsSync(trimmed)) existing.push(trimmed);
      } catch {
      }
    }

    return existing.join(path.delimiter);
  };

  /**
   * 构建增强版 PATH：若当前进程 PATH 看起来是用户自定义的（pathLooksUserConfigured）
   * 则以它为主、登录 shell PATH 为补充，反之亦然；经 mergePathValues 按序合并，
   * 保证主 PATH 的顺序优先、补充条目只追加不重复。
   */
  const buildAugmentedPath = () => {
    const currentPath = getEnvValue('PATH');
    const loginShellPath = getLoginShellPath();
    const home = os.homedir();
    const currentPathLooksUserConfigured = pathLooksUserConfigured(currentPath, home, path.delimiter);
    const primaryPath = currentPathLooksUserConfigured ? currentPath : loginShellPath;
    const fallbackPath = currentPathLooksUserConfigured ? loginShellPath : currentPath;

    return mergePathValues(primaryPath, fallbackPath, path.delimiter);
  };

  /**
   * 构建托管 OpenCode 子进程的 PATH：以登录 shell PATH 为主合并当前进程 PATH，
   * 再并入 Windows 工具链目录（buildWindowsManagedToolchainPath，非 Windows 为空）。
   */
  const buildManagedOpenCodePath = () => {
    const currentPath = getEnvValue('PATH');
    const loginShellPath = getLoginShellPath();
    const basePath = mergePathValues(loginShellPath || '', currentPath, path.delimiter);

    return mergePathValues(basePath, buildWindowsManagedToolchainPath(), path.delimiter);
  };

  /**
   * 解析一段 SSE 块的 data: 负载：合并多行 data 后 JSON.parse；若结果为带
   * object 类型 payload 字段的包装对象则解包返回 payload，否则返回解析值本身。
   * 块为空、没有 data 行、负载为空或解析失败均返回 null。
   */
  const parseSseDataPayload = (block) => {
    if (!block || typeof block !== 'string') {
      return null;
    }
    const dataLines = block
      .split('\n')
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.slice(5).replace(/^\s/, ''));

    if (dataLines.length === 0) {
      return null;
    }

    const payloadText = dataLines.join('\n').trim();
    if (!payloadText) {
      return null;
    }

    try {
      const parsed = JSON.parse(payloadText);
      if (
        parsed &&
        typeof parsed === 'object' &&
        typeof parsed.payload === 'object' &&
        parsed.payload !== null
      ) {
        return parsed.payload;
      }
      return parsed;
    } catch {
      return null;
    }
  };

  /**
   * 从 OpenCode 拉取一个数组型快照接口：端口未知、HTTP 非 2xx 或响应不是
   * 数组时抛出带 invalidMessage 上下文的 Error。
   */
  const fetchArraySnapshot = async (route, invalidMessage) => {
    if (!getOpenCodePort()) {
      throw new Error('OpenCode port is not available');
    }

    const response = await fetch(buildOpenCodeUrl(route), {
      method: 'GET',
      headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
    });

    if (!response.ok) {
      throw new Error(`Failed to fetch ${invalidMessage} (status ${response.status})`);
    }

    const payload = await response.json().catch(() => null);
    if (!Array.isArray(payload)) {
      throw new Error(`Invalid ${invalidMessage} payload from OpenCode`);
    }
    return payload;
  };

  /** 拉取 OpenCode GET /agent 的 agents 快照数组。 */
  const fetchAgentsSnapshot = () => fetchArraySnapshot('/agent', 'agents snapshot');
  /** 拉取 OpenCode GET /provider 的 providers 快照数组。 */
  const fetchProvidersSnapshot = () => fetchArraySnapshot('/provider', 'providers snapshot');
  /** 拉取 OpenCode GET /model 的 models 快照数组。 */
  const fetchModelsSnapshot = () => fetchArraySnapshot('/model', 'models snapshot');

  /**
   * 在 express app 上挂载 OpenCode 反向代理（registerOpenCodeProxy），
   * 透传就绪宽限、长请求超时、SSE 上游停滞超时与 UI 通知客户端集合等依赖。
   */
  const setupProxy = (app) => {
    registerOpenCodeProxy(app, {
      fs,
      os,
      path,
      OPEN_CODE_READY_GRACE_MS: openCodeReadyGraceMs,
      LONG_REQUEST_TIMEOUT_MS: longRequestTimeoutMs,
      getRuntime,
      getOpenCodeAuthHeaders,
      buildOpenCodeUrl,
      ensureOpenCodeApiPrefix,
      getSseUpstreamStallTimeoutMs: getUpstreamStallTimeoutMs,
      getUiNotificationClients,
    });
  };

  return {
    setOpenCodePort,
    waitForOpenCodePort,
    buildAugmentedPath,
    buildManagedOpenCodePath,
    parseSseDataPayload,
    fetchAgentsSnapshot,
    fetchProvidersSnapshot,
    fetchModelsSnapshot,
    setupProxy,
  };
};
