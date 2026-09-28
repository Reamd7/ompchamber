/**
 * OpenCode 认证状态运行时：集中管理 Basic auth 密码的归一化、生成、轮换与请求头构造。
 *
 * 密码来源分三类：用户环境变量（user-env）、托管实例生成（generated）、
 * 主动轮换（rotated）。写入密码会同步 process.env.OPENCODE_SERVER_PASSWORD
 * 并通过 syncToHmrState 持久到 HMR 状态，保证子进程与热重载后一致。
 */
/**
 * 创建 OpenCode 认证状态运行时。
 * @param {object} dependencies - 注入依赖：crypto（随机密码）、process
 *   （环境变量）、密码/来源的读写器、getUserProvidedPassword（用户显式
 *   密码）与 syncToHmrState（同步回调）。
 * @returns 认证请求头构造、连接安全性判断与密码确保/轮换方法。
 */
export const createOpenCodeAuthStateRuntime = (dependencies) => {
  const {
    crypto,
    process,
    getAuthPassword,
    setAuthPassword,
    getAuthSource,
    setAuthSource,
    getUserProvidedPassword,
    syncToHmrState,
  } = dependencies;

  /** 归一化密码：非字符串返回空串，否则去首尾空白。 */
  const normalizeOpenCodePassword = (value) => {
    if (typeof value !== 'string') {
      return '';
    }
    return value.trim();
  };

  /** 密码有效性：必须是去空白后长度大于 0 的字符串。 */
  const isValidOpenCodePassword = (password) => typeof password === 'string' && password.trim().length > 0;

  /**
   * 生成 URL 安全的强随机密码：32 字节随机数经 base64 编码后，把 +、/
   * 替换为 -、_ 并去掉尾部 = 填充，可直接用于 URL 与环境变量。
   */
  const generateSecureOpenCodePassword = () =>
    crypto
      .randomBytes(32)
      .toString('base64')
      .replace(/\+/g, '-')
      .replace(/\//g, '_')
      .replace(/=+$/g, '');

  /**
   * 写入认证状态（密码 + 来源）。
   * 密码无效时清空内存状态、删除 OPENCODE_SERVER_PASSWORD 环境变量并同步
   * HMR，返回 null；有效时写入内存状态与环境变量并同步 HMR，返回归一化密码。
   * @returns {string|null} 最终生效的密码。
   */
  const setOpenCodeAuthState = (password, source) => {
    const normalized = normalizeOpenCodePassword(password);
    if (!isValidOpenCodePassword(normalized)) {
      setAuthPassword(null);
      setAuthSource(null);
      delete process.env.OPENCODE_SERVER_PASSWORD;
      syncToHmrState();
      return null;
    }

    setAuthPassword(normalized);
    setAuthSource(source);
    process.env.OPENCODE_SERVER_PASSWORD = normalized;
    syncToHmrState();
    return normalized;
  };

  /**
   * 构造访问 OpenCode 服务所需的 Basic auth 请求头。
   * 密码缺失时返回空对象（不带认证）；用户名取 OPENCODE_SERVER_USERNAME
   * （默认 opencode），以 `username:password` 的 base64 编码生成 Authorization 头。
   * @returns {{ Authorization?: string }}
   */
  const getOpenCodeAuthHeaders = () => {
    const password = normalizeOpenCodePassword(getAuthPassword() || process.env.OPENCODE_SERVER_PASSWORD || '');

    if (!password) {
      return {};
    }

    const username = process.env.OPENCODE_SERVER_USERNAME?.trim() || 'opencode';
    const credentials = Buffer.from(`${username}:${password}`).toString('base64');
    return { Authorization: `Basic ${credentials}` };
  };

  /** 判断当前连接是否已启用认证（即请求头中存在 Authorization 字段）。 */
  const isOpenCodeConnectionSecure = () => Object.prototype.hasOwnProperty.call(getOpenCodeAuthHeaders(), 'Authorization');

  /**
   * 确保托管本地 OpenCode 实例始终有可用密码。
   * 决策顺序：用户显式密码有效 → 直接采用（来源 user-env）；rotateManaged
   * 为 true → 无条件轮换新密码（来源 rotated）；既有密码有效 → 沿用（来源
   * 缺省时补 generated）；否则生成新密码（来源 generated）。生成/轮换各打一条日志。
   * @param {object} [options] - rotateManaged 为 true 时强制轮换。
   * @returns {Promise<string>} 最终生效的密码。
   */
  const ensureLocalOpenCodeServerPassword = async ({ rotateManaged = false } = {}) => {
    const userProvidedPassword = getUserProvidedPassword();
    if (isValidOpenCodePassword(userProvidedPassword)) {
      return setOpenCodeAuthState(userProvidedPassword, 'user-env');
    }

    if (rotateManaged) {
      const rotatedPassword = setOpenCodeAuthState(generateSecureOpenCodePassword(), 'rotated');
      console.log('Rotated secure password for managed local OpenCode instance');
      return rotatedPassword;
    }

    const currentPassword = getAuthPassword();
    const currentSource = getAuthSource();
    if (isValidOpenCodePassword(currentPassword)) {
      return setOpenCodeAuthState(currentPassword, currentSource || 'generated');
    }

    const generatedPassword = setOpenCodeAuthState(generateSecureOpenCodePassword(), 'generated');
    console.log('Generated secure password for managed local OpenCode instance');
    return generatedPassword;
  };

  // 导出：认证请求头构造、连接安全性判断、密码确保/轮换
  return {
    getOpenCodeAuthHeaders,
    isOpenCodeConnectionSecure,
    ensureLocalOpenCodeServerPassword,
  };
};
