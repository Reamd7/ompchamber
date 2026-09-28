/**
 * 隧道领域类型与归一化工具模块。
 *
 * 集中定义隧道提供商（provider）、模式（mode）与意图（intent）的常量字面量、
 * TunnelServiceError 业务异常，以及启动请求的归一化与校验函数。
 * 所有归一化函数都将非法输入收敛到安全默认值（默认提供商 cloudflare、默认模式 quick），
 * 供 routes.js、index.js（服务层）与各 providers 适配器共用。
 */
import os from 'os';
import path from 'path';

/** 提供商常量：Cloudflare（依赖 cloudflared CLI）。 */
export const TUNNEL_PROVIDER_CLOUDFLARE = 'cloudflare';
/** 提供商常量：ngrok（依赖 ngrok CLI）。 */
export const TUNNEL_PROVIDER_NGROK = 'ngrok';

/** 模式常量：快速隧道（临时公网地址，无需账号/token）。 */
export const TUNNEL_MODE_QUICK = 'quick';
/** 模式常量：托管远端隧道（凭 token + hostname 接入已存在的 Cloudflare Tunnel）。 */
export const TUNNEL_MODE_MANAGED_REMOTE = 'managed-remote';
/** 模式常量：托管本地隧道（由本地 config.yml 配置文件驱动）。 */
export const TUNNEL_MODE_MANAGED_LOCAL = 'managed-local';

/** 意图常量：临时公网暴露（quick 模式对应的意图）。 */
export const TUNNEL_INTENT_EPHEMERAL_PUBLIC = 'ephemeral-public';
/** 意图常量：持久公网暴露（managed-remote / managed-local 对应的意图）。 */
export const TUNNEL_INTENT_PERSISTENT_PUBLIC = 'persistent-public';
/** 意图常量：私有网络。当前保留值，没有任何模式声明使用它。 */
const TUNNEL_INTENT_PRIVATE_NETWORK = 'private-network';

/** 全部受支持的隧道意图集合，normalizeTunnelIntent 与校验函数据此过滤。 */
const SUPPORTED_TUNNEL_INTENTS = new Set([
  TUNNEL_INTENT_EPHEMERAL_PUBLIC,
  TUNNEL_INTENT_PERSISTENT_PUBLIC,
  TUNNEL_INTENT_PRIVATE_NETWORK,
]);

/** 全部受支持的隧道模式集合，isSupportedTunnelMode 据此判定。 */
const SUPPORTED_TUNNEL_MODES = new Set([
  TUNNEL_MODE_QUICK,
  TUNNEL_MODE_MANAGED_REMOTE,
  TUNNEL_MODE_MANAGED_LOCAL,
]);

/**
 * 隧道业务异常：除 message 外携带稳定的机器可读 code
 * （validation_error / provider_unsupported / mode_unsupported / missing_dependency / startup_failed），
 * 路由层依据 code 将其映射为对应的 HTTP 状态码。
 */
export class TunnelServiceError extends Error {
  /**
   * @param {string} code 稳定错误码（供路由层映射 HTTP 状态）
   * @param {string} message 人类可读的错误信息
   * @param {*|null} details 可选的附加上下文，默认 null
   */
  constructor(code, message, details = null) {
    super(message);
    this.name = 'TunnelServiceError';
    this.code = code;
    this.details = details;
  }
}

/** 全部受支持的提供商 id 集合，normalizeTunnelProvider 据此过滤非法值。 */
const SUPPORTED_TUNNEL_PROVIDERS = new Set([
  TUNNEL_PROVIDER_CLOUDFLARE,
  TUNNEL_PROVIDER_NGROK,
]);

/**
 * 按平台返回对应的 path API（win32 用 path.win32，其余平台用默认 path），
 * 使路径计算可在非 Windows 环境下模拟 Windows 语义（供测试注入 platform）。
 */
const getPathApiForPlatform = (platform) => (platform === 'win32' ? path.win32 : path);

/**
 * 判断 candidatePath 解析后是否位于 directoryPath 目录之内（含等于目录本身）。
 * Windows 下忽略大小写差异；以目录分隔符结尾的前缀判定边界，避免同级目录因
 * 字符串前缀相同而被误判为包含。
 * @param {string} candidatePath 待检查的路径
 * @param {string} directoryPath 目标目录
 * @param {string} platform 平台标识，默认 process.platform
 * @returns {boolean} 任一入参非字符串或不在目录内时返回 false
 */
export function isPathWithinDirectory(candidatePath, directoryPath, platform = process.platform) {
  if (typeof candidatePath !== 'string' || typeof directoryPath !== 'string') {
    return false;
  }

  const pathApi = getPathApiForPlatform(platform);
  const resolvedCandidate = pathApi.resolve(candidatePath);
  const resolvedDirectory = pathApi.resolve(directoryPath);
  const comparableCandidate = platform === 'win32' ? resolvedCandidate.toLowerCase() : resolvedCandidate;
  const comparableDirectory = platform === 'win32' ? resolvedDirectory.toLowerCase() : resolvedDirectory;
  const directoryPrefix = comparableDirectory.endsWith(pathApi.sep)
    ? comparableDirectory
    : `${comparableDirectory}${pathApi.sep}`;

  return comparableCandidate === comparableDirectory || comparableCandidate.startsWith(directoryPrefix);
}

/**
 * 解析隧道配置文件路径为绝对路径：支持 ~ 与 ~/、~\ 前缀展开到 home，
 * 其余交给 path.resolve 处理。
 * 解析结果必须落在 home 目录内，否则抛出 TunnelServiceError('validation_error')，
 * 防止请求指向任意路径（如系统目录）造成越权读写。
 * @param {string} value 用户提供的路径字符串
 * @param {string} home 用于展开 ~ 的主目录，默认 os.homedir()
 * @param {string} platform 平台标识，默认 process.platform
 * @returns {string} 归一化后的绝对路径
 * @throws {TunnelServiceError} 路径逸出 home 目录时抛出（code: validation_error）
 */
export function resolveTunnelConfigPath(value, home = os.homedir(), platform = process.platform) {
  const pathApi = getPathApiForPlatform(platform);
  let resolved;
  if (value === '~') {
    resolved = home;
  } else if (value.startsWith('~/') || value.startsWith('~\\')) {
    resolved = pathApi.join(home, value.slice(2));
  } else {
    resolved = pathApi.resolve(value);
  }

  if (!isPathWithinDirectory(resolved, home, platform)) {
    throw new TunnelServiceError(
      'validation_error',
      `Config path must be within the home directory (${home}). Got: ${resolved}`
    );
  }
  return resolved;
}

/**
 * 归一化提供商 id：去首尾空白并转小写；非字符串、为空或不受支持时
 * 一律回退为 TUNNEL_PROVIDER_CLOUDFLARE。
 * @returns {string} 合法的提供商 id
 */
export function normalizeTunnelProvider(value) {
  if (typeof value !== 'string') {
    return TUNNEL_PROVIDER_CLOUDFLARE;
  }
  const provider = value.trim().toLowerCase();
  if (!provider || !SUPPORTED_TUNNEL_PROVIDERS.has(provider)) {
    return TUNNEL_PROVIDER_CLOUDFLARE;
  }
  return provider;
}

/**
 * 归一化隧道模式：去首尾空白并转小写；仅识别三种合法模式字面量，
 * 非法或缺失值一律回退为 TUNNEL_MODE_QUICK。
 * @returns {string} 合法的模式字面量
 */
export function normalizeTunnelMode(value) {
  if (typeof value !== 'string') {
    return TUNNEL_MODE_QUICK;
  }
  const mode = value.trim().toLowerCase();
  if (!mode) {
    return TUNNEL_MODE_QUICK;
  }
  if (mode === TUNNEL_MODE_QUICK) {
    return TUNNEL_MODE_QUICK;
  }
  if (mode === TUNNEL_MODE_MANAGED_REMOTE) {
    return TUNNEL_MODE_MANAGED_REMOTE;
  }
  if (mode === TUNNEL_MODE_MANAGED_LOCAL) {
    return TUNNEL_MODE_MANAGED_LOCAL;
  }
  return TUNNEL_MODE_QUICK;
}

/**
 * 归一化隧道意图：非字符串、为空或不在受支持集合内时返回 undefined
 * （交由 modeIntentFallback 按模式推导默认意图）。
 * @returns {string|undefined} 合法意图或 undefined
 */
function normalizeTunnelIntent(value) {
  if (typeof value !== 'string') {
    return undefined;
  }
  const intent = value.trim().toLowerCase();
  if (!intent || !SUPPORTED_TUNNEL_INTENTS.has(intent)) {
    return undefined;
  }
  return intent;
}

/**
 * 依据模式推导默认意图：quick → ephemeral-public；
 * managed-remote / managed-local → persistent-public；未知模式返回 undefined。
 */
function modeIntentFallback(mode) {
  if (mode === TUNNEL_MODE_QUICK) {
    return TUNNEL_INTENT_EPHEMERAL_PUBLIC;
  }
  if (mode === TUNNEL_MODE_MANAGED_REMOTE || mode === TUNNEL_MODE_MANAGED_LOCAL) {
    return TUNNEL_INTENT_PERSISTENT_PUBLIC;
  }
  return undefined;
}

/**
 * 面向启动请求的模式归一化：仅当输入为字符串且等于三种合法模式字面量之一时
 * 原样返回，其余情况（含缺失）回退为 TUNNEL_MODE_QUICK。
 */
function normalizeTunnelModeForRequest(value) {
  if (typeof value === 'string') {
    const mode = value.trim().toLowerCase();
    if (mode === TUNNEL_MODE_QUICK || mode === TUNNEL_MODE_MANAGED_REMOTE || mode === TUNNEL_MODE_MANAGED_LOCAL) {
      return mode;
    }
  }
  return TUNNEL_MODE_QUICK;
}

/**
 * 归一化可选路径字段，保留三态语义：
 * null 或空字符串 → null（显式清除）；非字符串（含 undefined）→ undefined（未提供）；
 * 其余 trim 后经 resolveTunnelConfigPath 校验并返回绝对路径。
 * @param {*} value 待归一化的路径值
 * @returns {string|null|undefined} 绝对路径 / 显式置空 / 未提供
 * @throws {TunnelServiceError} 路径逸出 home 目录时由 resolveTunnelConfigPath 抛出
 */
export function normalizeOptionalPath(value) {
  if (value === null) {
    return null;
  }
  if (typeof value !== 'string') {
    return undefined;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }
  return resolveTunnelConfigPath(trimmed);
}

/**
 * 判断给定 mode 是否属于受支持的模式字面量（不做归一化，需传入已归一化的值）。
 * @returns {boolean}
 */
export function isSupportedTunnelMode(mode) {
  return SUPPORTED_TUNNEL_MODES.has(mode);
}

/**
 * 将用户输入与默认值合并归一化为标准启动请求。
 * provider/mode 非法回退默认；intent 缺失时按模式回退；configPath 遵循
 * normalizeOptionalPath 的三态语义（input 自带 configPath 属性时优先于 defaults）；
 * token/hostname 做 trim（hostname 额外转小写）处理。
 * @param {object} input 请求输入
 * @param {object} defaults 默认值来源（如磁盘设置中的存量配置）
 * @returns {{provider: string, mode: string, intent: string|undefined, configPath: string|null|undefined, token: string, hostname: string}}
 */
export function normalizeTunnelStartRequest(input = {}, defaults = {}) {
  const provider = normalizeTunnelProvider(input.provider ?? defaults.provider);
  const mode = normalizeTunnelModeForRequest(input.mode ?? defaults.mode);
  const explicitIntent = normalizeTunnelIntent(input.intent ?? defaults.intent);
  const intent = explicitIntent ?? modeIntentFallback(mode);
  const configPathValue = Object.prototype.hasOwnProperty.call(input, 'configPath')
    ? input.configPath
    : defaults.configPath;
  const configPath = normalizeOptionalPath(configPathValue);

  const token = typeof (input.token ?? defaults.token) === 'string'
    ? (input.token ?? defaults.token).trim()
    : '';

  const hostname = typeof (input.hostname ?? defaults.hostname) === 'string'
    ? (input.hostname ?? defaults.hostname).trim().toLowerCase()
    : '';

  return {
    provider,
    mode,
    intent,
    configPath,
    token,
    hostname,
  };
}

/**
 * 校验归一化后的启动请求是否满足提供商能力（capabilities）约束：
 * 请求必须是对象且 provider/mode 存在；capabilities.provider 必须与请求匹配；
 * 模式必须在该提供商 capabilities.modes 中声明；显式 intent 必须与模式声明的
 * intent 一致；模式 requires 中声明的 token/hostname/configPath 必须已提供。
 * @param {object} request normalizeTunnelStartRequest 的产物
 * @param {object} capabilities 提供商能力描述（provider / modes[].key / intent / requires）
 * @returns {void} 校验通过无返回值
 * @throws {TunnelServiceError} code 为 validation_error / provider_unsupported / mode_unsupported 之一
 */
export function validateTunnelStartRequest(request, capabilities) {
  if (!request || typeof request !== 'object') {
    throw new TunnelServiceError('validation_error', 'Tunnel start request must be an object');
  }

  if (!request.provider) {
    throw new TunnelServiceError('validation_error', 'Tunnel provider is required');
  }

  if (!isSupportedTunnelMode(request.mode)) {
    throw new TunnelServiceError('mode_unsupported', `Unsupported tunnel mode: ${request.mode}`);
  }

  if (!capabilities || capabilities.provider !== request.provider) {
    throw new TunnelServiceError('provider_unsupported', `Unsupported tunnel provider: ${request.provider}`);
  }

  if (!Array.isArray(capabilities.modes)) {
    throw new TunnelServiceError('mode_unsupported', `Provider '${request.provider}' does not declare tunnel modes`);
  }

  const modeDescriptor = capabilities.modes.find((entry) => entry?.key === request.mode);
  if (!modeDescriptor) {
    throw new TunnelServiceError('mode_unsupported', `Provider '${request.provider}' does not support mode '${request.mode}'`);
  }

  if (typeof request.intent === 'string' && request.intent.length > 0) {
    if (!SUPPORTED_TUNNEL_INTENTS.has(request.intent)) {
      throw new TunnelServiceError('validation_error', `Unsupported tunnel intent: ${request.intent}`);
    }
    if (modeDescriptor.intent !== request.intent) {
      throw new TunnelServiceError(
        'validation_error',
        `Tunnel intent '${request.intent}' does not match mode '${request.mode}' (expected '${modeDescriptor.intent}')`
      );
    }
  }

  const requiredFields = Array.isArray(modeDescriptor.requires) ? modeDescriptor.requires : [];

  if (requiredFields.includes('token')) {
    if (!request.token) {
      throw new TunnelServiceError('validation_error', 'Managed remote tunnel token is required');
    }
  }

  if (requiredFields.includes('hostname')) {
    if (!request.hostname) {
      throw new TunnelServiceError('validation_error', 'Managed remote tunnel hostname is required');
    }
  }

  if (requiredFields.includes('configPath')) {
    if (request.configPath === undefined || request.configPath === null || request.configPath === '') {
      throw new TunnelServiceError('validation_error', `Mode '${request.mode}' requires a configPath`);
    }
  }
}
