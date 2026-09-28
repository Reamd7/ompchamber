/**
 * 自定义 LLM provider 配置的数据层。
 *
 * 面向 OpenCode 分层配置中的 provider 键（同时兼容 providers 别名），提供
 * 自定义 provider 的校验（validateCustomProviderConfig）、创建/更新
 * （upsertProviderConfig）、删除（removeProviderConfig）以及各配置层的
 * 存在性探测（getProviderSources）。只操作 opencode.json 中的非敏感字段：
 * API key 等凭据始终保存在 auth.json，经由 OpenCode auth API 管理。
 */
import {
  CONFIG_FILE,
  readConfigLayers,
  isPlainObject,
  getConfigForPath,
  writeConfig,
} from './shared.js';

/** 自定义 provider ID 的合法格式：小写字母或数字开头，仅含小写字母、数字、连字符与下划线。 */
const PROVIDER_ID_PATTERN = /^[a-z0-9][a-z0-9-_]*$/;
/** 自定义 provider 的 baseURL 必须以 http:// 或 https:// 开头。 */
const BASE_URL_PATTERN = /^https?:\/\//;
/** 默认的 AI adapter npm 包（OpenAI 兼容协议）。 */
const OPENAI_COMPATIBLE_NPM = '@ai-sdk/openai-compatible';
/** 允许自定义 provider 使用的 adapter npm 包白名单。 */
const CUSTOM_PROVIDER_NPM_PACKAGES = new Set([
  OPENAI_COMPATIBLE_NPM,
  '@ai-sdk/openai',
  '@ai-sdk/anthropic',
]);

/**
 * 探测某个 provider ID 在各配置层（user/project/custom）中是否存在，
 * 每层同时兼容 provider 与 providers 两种键名。返回 auth（恒为不存在）、
 * user、project、custom 四个来源的 exists 标志与文件路径，供前端判断编辑入口。
 */
function getProviderSources(providerId, workingDirectory) {
  const layers = readConfigLayers(workingDirectory);
  const { userConfig, projectConfig, customConfig, paths } = layers;

  const customProviders = isPlainObject(customConfig?.provider) ? customConfig.provider : {};
  const customProvidersAlias = isPlainObject(customConfig?.providers) ? customConfig.providers : {};
  const projectProviders = isPlainObject(projectConfig?.provider) ? projectConfig.provider : {};
  const projectProvidersAlias = isPlainObject(projectConfig?.providers) ? projectConfig.providers : {};
  const userProviders = isPlainObject(userConfig?.provider) ? userConfig.provider : {};
  const userProvidersAlias = isPlainObject(userConfig?.providers) ? userConfig.providers : {};

  const customExists =
    Object.prototype.hasOwnProperty.call(customProviders, providerId) ||
    Object.prototype.hasOwnProperty.call(customProvidersAlias, providerId);
  const projectExists =
    Object.prototype.hasOwnProperty.call(projectProviders, providerId) ||
    Object.prototype.hasOwnProperty.call(projectProvidersAlias, providerId);
  const userExists =
    Object.prototype.hasOwnProperty.call(userProviders, providerId) ||
    Object.prototype.hasOwnProperty.call(userProvidersAlias, providerId);

  return {
    sources: {
      auth: { exists: false },
      user: { exists: userExists, path: paths.userPath },
      project: { exists: projectExists, path: paths.projectPath || null },
      custom: { exists: customExists, path: paths.customPath }
    }
  };
}

/**
 * Validate a custom provider config payload before persistence.
 * Returns { ok: true, value } or { ok: false, error }.
 *
 * Credentials: either config.env contains a variable name, or hasStoredAuth is true
 * (auth.json already has a key — typically after auth.set, or when editing).
 * 中文补充：逐项校验 providerId 格式、config 形状、显示名称、npm 适配包
 * 白名单、options.baseURL（必须 http/https 开头）、models（至少一个且每个
 * 模型有 name）、凭据（env 数组或 hasStoredAuth 二选一）与可选的
 * options.headers（键值均为非空字符串）。通过时返回去除多余字段、统一
 * trim 的规范化配置；失败时返回错误消息 —— 本函数不抛异常、不写盘。
 */
function validateCustomProviderConfig(providerId, config, options = {}) {
  if (!providerId || typeof providerId !== 'string' || !PROVIDER_ID_PATTERN.test(providerId)) {
    return { ok: false, error: 'Provider ID must match /^[a-z0-9][a-z0-9-_]*$/' };
  }

  if (!isPlainObject(config)) {
    return { ok: false, error: 'Provider config must be an object' };
  }

  const name = typeof config.name === 'string' ? config.name.trim() : '';
  if (!name) {
    return { ok: false, error: 'Provider name is required' };
  }

  const npm = typeof config.npm === 'string' ? config.npm.trim() : OPENAI_COMPATIBLE_NPM;
  if (!CUSTOM_PROVIDER_NPM_PACKAGES.has(npm)) {
    return { ok: false, error: 'Custom providers must use @ai-sdk/openai-compatible, @ai-sdk/openai, or @ai-sdk/anthropic' };
  }

  const optionsBlock = isPlainObject(config.options) ? config.options : null;
  if (!optionsBlock) {
    return { ok: false, error: 'Provider options are required' };
  }

  const baseURL = typeof optionsBlock.baseURL === 'string' ? optionsBlock.baseURL.trim() : '';
  if (!baseURL) {
    return { ok: false, error: 'Base URL is required' };
  }
  if (!BASE_URL_PATTERN.test(baseURL)) {
    return { ok: false, error: 'Base URL must start with http:// or https://' };
  }

  const models = isPlainObject(config.models) ? config.models : null;
  if (!models || Object.keys(models).length === 0) {
    return { ok: false, error: 'At least one model is required' };
  }

  const normalizedModels = {};
  for (const [modelId, modelValue] of Object.entries(models)) {
    const trimmedId = typeof modelId === 'string' ? modelId.trim() : '';
    if (!trimmedId) {
      return { ok: false, error: 'Model id is required' };
    }
    if (!isPlainObject(modelValue)) {
      return { ok: false, error: `Model "${trimmedId}" must be an object` };
    }
    const modelName = typeof modelValue.name === 'string' ? modelValue.name.trim() : '';
    if (!modelName) {
      return { ok: false, error: `Model "${trimmedId}" requires a name` };
    }
    normalizedModels[trimmedId] = { name: modelName };
  }

  const normalized = {
    npm,
    name,
    options: {
      baseURL,
    },
    models: normalizedModels,
  };

  let env = [];
  if (Array.isArray(config.env)) {
    env = config.env
      .filter((entry) => typeof entry === 'string' && entry.trim().length > 0)
      .map((entry) => entry.trim());
    if (env.length > 0) {
      normalized.env = env;
    }
  }

  const hasStoredAuth = Boolean(options.hasStoredAuth);
  if (env.length === 0 && !hasStoredAuth) {
    return {
      ok: false,
      error: 'API key or {env:VAR} credentials are required',
    };
  }

  if (isPlainObject(optionsBlock.headers)) {
    const headers = {};
    for (const [headerKey, headerValue] of Object.entries(optionsBlock.headers)) {
      if (typeof headerKey !== 'string' || !headerKey.trim()) {
        continue;
      }
      if (typeof headerValue !== 'string' || !headerValue.trim()) {
        return { ok: false, error: `Header "${headerKey}" requires a non-empty value` };
      }
      headers[headerKey.trim()] = headerValue.trim();
    }
    if (Object.keys(headers).length > 0) {
      normalized.options.headers = headers;
    }
  }

  return { ok: true, value: { providerId, config: normalized } };
}

/**
 * Persist (create or update) a custom provider block in OpenCode user/project/custom config.
 * Does not write secrets — API keys remain in auth.json via the OpenCode auth API.
 * 中文补充：先完整校验，失败时抛出带 statusCode=400 的 Error；再按 scope
 * 选定写入目标（user=用户层，project=项目层，custom=OPENCODE_CONFIG 指定的
 * 自定义层），将规范化配置写入目标层的 provider 键，并从 disabled_providers
 * 中移除该 ID（保证新写入的 provider 立即可用）。返回 providerId、落盘路径
 * 与规范化后的配置。
 */
function upsertProviderConfig(providerId, config, workingDirectory, scope = 'user', options = {}) {
  const validated = validateCustomProviderConfig(providerId, config, options);
  if (!validated.ok) {
    const error = new Error(validated.error);
    error.statusCode = 400;
    throw error;
  }

  const layers = readConfigLayers(workingDirectory);
  let targetPath = layers.paths.userPath;

  if (scope === 'project') {
    if (!workingDirectory) {
      throw new Error('Working directory is required for project scope');
    }
    targetPath = layers.paths.projectPath || targetPath;
  } else if (scope === 'custom') {
    if (!layers.paths.customPath) {
      throw new Error('Custom config path (OPENCODE_CONFIG) is not set');
    }
    targetPath = layers.paths.customPath;
  } else if (scope !== 'user') {
    throw new Error('Invalid scope');
  }

  const targetConfig = getConfigForPath(layers, targetPath);
  const providerConfig = isPlainObject(targetConfig.provider) ? { ...targetConfig.provider } : {};
  providerConfig[validated.value.providerId] = validated.value.config;
  targetConfig.provider = providerConfig;

  if (Array.isArray(targetConfig.disabled_providers)) {
    targetConfig.disabled_providers = targetConfig.disabled_providers.filter(
      (entry) => entry !== validated.value.providerId,
    );
  }

  const writePath = targetPath || CONFIG_FILE;
  writeConfig(targetConfig, writePath);

  return {
    providerId: validated.value.providerId,
    path: writePath,
    config: validated.value.config,
  };
}

/**
 * 从指定 scope（user/project/custom）的配置层中删除一个自定义 provider，
 * 同时处理 provider 与 providers 两种键名；对应键删空后连带移除该键。
 * 目标层不存在该条目时返回 false（不写盘），删除成功返回 true。
 */
function removeProviderConfig(providerId, workingDirectory, scope = 'user') {
  if (!providerId || typeof providerId !== 'string') {
    throw new Error('Provider ID is required');
  }

  const layers = readConfigLayers(workingDirectory);
  let targetPath = layers.paths.userPath;

  if (scope === 'project') {
    if (!workingDirectory) {
      throw new Error('Working directory is required for project scope');
    }
    targetPath = layers.paths.projectPath || targetPath;
  } else if (scope === 'custom') {
    if (!layers.paths.customPath) {
      return false;
    }
    targetPath = layers.paths.customPath;
  }

  const targetConfig = getConfigForPath(layers, targetPath);
  const providerConfig = isPlainObject(targetConfig.provider) ? targetConfig.provider : {};
  const providersConfig = isPlainObject(targetConfig.providers) ? targetConfig.providers : {};
  const removedProvider = Object.prototype.hasOwnProperty.call(providerConfig, providerId);
  const removedProviders = Object.prototype.hasOwnProperty.call(providersConfig, providerId);

  if (!removedProvider && !removedProviders) {
    return false;
  }

  if (removedProvider) {
    delete providerConfig[providerId];
    if (Object.keys(providerConfig).length === 0) {
      delete targetConfig.provider;
    } else {
      targetConfig.provider = providerConfig;
    }
  }

  if (removedProviders) {
    delete providersConfig[providerId];
    if (Object.keys(providersConfig).length === 0) {
      delete targetConfig.providers;
    } else {
      targetConfig.providers = providersConfig;
    }
  }

  writeConfig(targetConfig, targetPath || CONFIG_FILE);
  console.log(`Removed provider ${providerId} from config: ${targetPath}`);
  return true;
}

// 对外导出：provider 校验、增删与各配置层来源探测。
export {
  getProviderSources,
  removeProviderConfig,
  upsertProviderConfig,
  validateCustomProviderConfig,
};
