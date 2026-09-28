/**
 * 隧道提供商注册表模块。
 *
 * 提供提供商的注册（含必需方法与重复 id 校验）、按 id 查询、列举、能力快照列举
 * 与封印（seal，封印后禁止再注册）能力，是 tunnelService 与路由层访问提供商的
 * 唯一入口。注册表创建后可通过 seal 固化，防止运行期被动态篡改。
 */

/** 提供商必须实现的方法名列表，注册时逐一校验其类型为函数。 */
const REQUIRED_PROVIDER_METHODS = ['start', 'stop', 'checkAvailability', 'resolvePublicUrl'];

/**
 * 创建提供商注册表实例。
 * @param {Array} initialProviders 初始提供商列表（逐个走 register 的完整校验）
 * @returns {{register: Function, get: Function, list: Function, listCapabilities: Function, seal: Function}}
 */
export function createTunnelProviderRegistry(initialProviders = []) {
  const providers = new Map();
  let sealed = false;

  /**
   * 注册一个提供商：校验未封印、id 为非空字符串、REQUIRED_PROVIDER_METHODS 全部
   * 实现、id（trim + 小写归一）未重复，全部通过后入表。
   * @param {object} provider 提供商对象
   * @returns {object} 入参 provider 本身，便于链式调用
   * @throws {Error} 已封印 / id 非法 / 缺少必需方法 / 重复注册
   */
  const register = (provider) => {
    if (sealed) {
      throw new Error('Tunnel provider registry is sealed; no further registrations allowed');
    }
    if (!provider || typeof provider.id !== 'string' || provider.id.trim().length === 0) {
      throw new Error('Tunnel provider must define a non-empty id');
    }
    for (const method of REQUIRED_PROVIDER_METHODS) {
      if (typeof provider[method] !== 'function') {
        throw new Error(`Tunnel provider '${provider.id}' must implement ${method}()`);
      }
    }
    const key = provider.id.trim().toLowerCase();
    if (providers.has(key)) {
      throw new Error(`Tunnel provider '${key}' is already registered`);
    }
    providers.set(key, provider);
    return provider;
  };

  /**
   * 按 id 查找提供商（id 会 trim 并转小写后匹配）；id 非法或未注册返回 null。
   * @param {string} providerId 提供商 id
   * @returns {object|null}
   */
  const get = (providerId) => {
    if (typeof providerId !== 'string' || providerId.trim().length === 0) {
      return null;
    }
    return providers.get(providerId.trim().toLowerCase()) ?? null;
  };

  /** 列出全部已注册提供商（返回新数组，遍历顺序即注册顺序）。 */
  const list = () => Array.from(providers.values());

  /** 列出各提供商 capabilities 的浅拷贝数组，供 /tunnel/providers 等只读展示使用。 */
  const listCapabilities = () => list().map((provider) => ({ ...provider.capabilities }));

  // 注册初始提供商列表（逐一经过 register 的校验，非法项直接抛错）。
  for (const provider of initialProviders) {
    register(provider);
  }

  /** 封印注册表：此后任何 register 调用都会抛错，用于启动完成后固化提供商集合。 */
  const seal = () => { sealed = true; };

  // 注册表公开 API。
  return {
    register,
    get,
    list,
    listCapabilities,
    seal,
  };
}
