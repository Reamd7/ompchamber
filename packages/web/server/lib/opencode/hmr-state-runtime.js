/**
 * HMR（热替换）状态运行时：在模块热更新/服务器重启之间保存 OpenCode 进程状态。
 *
 * 把状态挂在注入的 globalThisLike[stateKey] 上，使开发期 HMR 重载模块后
 * 既有进程句柄、端口、工作目录与认证信息不丢失；同时提供状态对象与
 * 运行时对象之间的双向同步（syncStateFromRuntime / restoreRuntimeFromState）。
 */
/**
 * 创建 HMR 状态运行时。
 * @param {object} dependencies - 注入依赖：globalThisLike（状态挂载对象）、
 *   os（homedir 兜底）、processLike（读取环境变量）、stateKey（挂载键名）。
 * @returns 状态创建/读取与认证解析、双向同步的一组纯函数式方法。
 */
export const createHmrStateRuntime = (dependencies) => {
  const {
    globalThisLike,
    os,
    processLike,
    stateKey,
  } = dependencies;

  /**
   * 计算初始 OpenCode 工作目录：优先取 OMPCHAMBER_OPENCODE_CWD 环境变量
   * （去首尾空白），否则回退到用户主目录。
   */
  const getInitialOpenCodeWorkingDirectory = () => {
    const configured = typeof processLike.env.OMPCHAMBER_OPENCODE_CWD === 'string'
      ? processLike.env.OMPCHAMBER_OPENCODE_CWD.trim()
      : '';
    return configured || os.homedir();
  };

  /**
   * 惰性创建并返回 HMR 状态对象。
   * 首次访问时初始化进程句柄、端口、工作目录、关闭标记、信号挂载标记与
   * 认证字段的默认值；后续调用直接复用 globalThisLike[stateKey] 上的既有状态。
   */
  const getOrCreateHmrState = () => {
    if (!globalThisLike[stateKey]) {
      globalThisLike[stateKey] = {
        openCodeProcess: null,
        openCodePort: null,
        openCodeWorkingDirectory: getInitialOpenCodeWorkingDirectory(),
        isShuttingDown: false,
        signalsAttached: false,
        userProvidedOpenCodePassword: undefined,
        openCodeAuthPassword: null,
        openCodeAuthSource: null,
      };
    }
    return globalThisLike[stateKey];
  };

  /**
   * 确保 userProvidedOpenCodePassword 已初始化：仅在该字段仍为 undefined 时
   * 从 OPENCODE_SERVER_PASSWORD 环境变量读取一次；空值归一为 null，使
   * “用户从未设置”与“环境变量后来才出现”可以区分，避免重复读取被覆盖。
   * @param {object} hmrState - HMR 状态对象（就地写入）。
   */
  const ensureUserProvidedOpenCodePassword = (hmrState) => {
    if (typeof hmrState.userProvidedOpenCodePassword !== 'undefined') {
      return;
    }
    const initialPassword = typeof processLike.env.OPENCODE_SERVER_PASSWORD === 'string'
      ? processLike.env.OPENCODE_SERVER_PASSWORD.trim()
      : '';
    hmrState.userProvidedOpenCodePassword = initialPassword || null;
  };

  /**
   * 读取用户显式提供的 OpenCode 密码。
   * @param {object} hmrState - HMR 状态对象。
   * @returns {string|null} 非空密码字符串；未提供或为空时返回 null。
   */
  const getUserProvidedOpenCodePassword = (hmrState) => (
    typeof hmrState.userProvidedOpenCodePassword === 'string' && hmrState.userProvidedOpenCodePassword.length > 0
      ? hmrState.userProvidedOpenCodePassword
      : null
  );

  /**
   * 从 HMR 状态解析当前生效的认证密码与来源。
   * 优先使用状态中已解析的 openCodeAuthPassword / openCodeAuthSource；
   * 缺失时回退到用户环境变量密码（来源标记 'user-env'），两者皆无则为 null。
   * @returns {{ openCodeAuthPassword: string|null, openCodeAuthSource: string|null }}
   */
  const resolveOpenCodeAuthFromState = ({ hmrState, userProvidedOpenCodePassword }) => ({
    openCodeAuthPassword:
      typeof hmrState.openCodeAuthPassword === 'string' && hmrState.openCodeAuthPassword.length > 0
        ? hmrState.openCodeAuthPassword
        : userProvidedOpenCodePassword,
    openCodeAuthSource:
      typeof hmrState.openCodeAuthSource === 'string' && hmrState.openCodeAuthSource.length > 0
        ? hmrState.openCodeAuthSource
        : (userProvidedOpenCodePassword ? 'user-env' : null),
  });

  /**
   * 把运行时对象的最新字段写入 HMR 状态（进程句柄、端口、baseUrl、关闭
   * 标记、信号挂载标记、工作目录与认证信息），供 HMR 重载后恢复。
   */
  const syncStateFromRuntime = (hmrState, runtime) => {
    hmrState.openCodeProcess = runtime.openCodeProcess;
    hmrState.openCodePort = runtime.openCodePort;
    hmrState.openCodeBaseUrl = runtime.openCodeBaseUrl;
    hmrState.isShuttingDown = runtime.isShuttingDown;
    hmrState.signalsAttached = runtime.signalsAttached;
    hmrState.openCodeWorkingDirectory = runtime.openCodeWorkingDirectory;
    hmrState.openCodeAuthPassword = runtime.openCodeAuthPassword;
    hmrState.openCodeAuthSource = runtime.openCodeAuthSource;
  };

  /**
   * 从 HMR 状态重建运行时对象：认证字段先经 resolveOpenCodeAuthFromState
   * 解析（含用户密码回退），openCodeBaseUrl 缺失时归一为 null，其余原样带出。
   */
  const restoreRuntimeFromState = ({ hmrState, userProvidedOpenCodePassword }) => {
    const auth = resolveOpenCodeAuthFromState({ hmrState, userProvidedOpenCodePassword });
    return {
      openCodeProcess: hmrState.openCodeProcess,
      openCodePort: hmrState.openCodePort,
      openCodeBaseUrl: hmrState.openCodeBaseUrl ?? null,
      isShuttingDown: hmrState.isShuttingDown,
      signalsAttached: hmrState.signalsAttached,
      openCodeWorkingDirectory: hmrState.openCodeWorkingDirectory,
      openCodeAuthPassword: auth.openCodeAuthPassword,
      openCodeAuthSource: auth.openCodeAuthSource,
    };
  };

  // 导出：状态创建/读取、用户密码初始化与读取、认证解析、双向同步
  return {
    getOrCreateHmrState,
    ensureUserProvidedOpenCodePassword,
    getUserProvidedOpenCodePassword,
    resolveOpenCodeAuthFromState,
    syncStateFromRuntime,
    restoreRuntimeFromState,
  };
};
