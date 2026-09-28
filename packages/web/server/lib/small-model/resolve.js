/**
 * 小模型解析：调用方未显式指定模型时，依据 auth.json 登录与 models.dev
 * 目录选出要使用的 "provider/model" 组合。解析链镜像 OpenCode 的
 * getSmallModel：设置覆盖 > 配置 small_model > 会话 provider 内挑选 >
 * 按家族优先级扫描全部已认证 provider > Copilot 工具模型兜底。
 */
import { getCatalogProvider } from './catalog.js';

// Mirrors OpenCode's getSmallModel fallback chain:
// 1. `small_model` from the merged config layers ("provider/model").
// 2. GitHub Copilot's hidden utility models when Copilot is logged in.
// 3. Family-priority scan of the authenticated providers' catalog models.
/** 候选模型家族的优先顺序：先 gemini-flash，再 gpt-nano，最后 claude-haiku。 */
const FAMILY_PRIORITY = ['gemini-flash', 'gpt-nano', 'claude-haiku'];
/** GitHub Copilot 未列入目录的内置工具模型，按优先级排列，兜底时取第一个。 */
const COPILOT_UTILITY_MODELS = ['gpt-5.4-nano', 'gpt-4.1', 'gpt-4o', 'gpt-4o-mini'];
// The ChatGPT-plan codex backend only accepts a small allowlist of models
// (nano/API-key models are rejected with 400) — this is its cheapest one.
/** ChatGPT 计划（codex 后端）登录时固定使用的小模型，不做目录扫描。 */
const OPENAI_OAUTH_SMALL_MODEL = 'gpt-5.4-mini';

/** auth.json 中的 provider 别名表：目录 id 映射到历史遗留的登录键名（copilot）。 */
const AUTH_PROVIDER_ALIASES = {
  'github-copilot': ['github-copilot', 'copilot'],
};

/**
 * 按 provider id（含别名）从 auth.json 数据里取出第一条存在的登录条目。
 * @param {Object|null} auth readAuthFile 的返回值。
 * @param {string} providerID provider 标识。
 * @returns {Object|null} 命中的登录条目；所有别名都未命中时返回 null。
 */
export function getAuthEntryForProvider(auth, providerID) {
  const aliases = AUTH_PROVIDER_ALIASES[providerID] || [providerID];
  for (const alias of aliases) {
    const entry = auth?.[alias];
    if (entry && typeof entry === 'object') {
      return entry;
    }
  }
  return null;
}

/**
 * 判断一条登录条目是否携带可用凭证：api 看 key、oauth 看 access/refresh
 * 任一非空、wellknown 看 token；空值或其余形状一律视为不可用。
 * @param {*} entry 待检查的登录条目。
 * @returns {boolean} 是否可用于发起真实请求。
 */
export function isUsableAuthEntry(entry) {
  if (!entry || typeof entry !== 'object') return false;
  if (entry.type === 'api') return typeof entry.key === 'string' && entry.key.length > 0;
  if (entry.type === 'oauth') {
    return (typeof entry.access === 'string' && entry.access.length > 0)
      || (typeof entry.refresh === 'string' && entry.refresh.length > 0);
  }
  if (entry.type === 'wellknown') return typeof entry.token === 'string' && entry.token.length > 0;
  return false;
}

/**
 * 解析 "provider/model" 引用：只在第一个斜杠处切分，model 部分允许继续
 * 包含斜杠（如 openrouter/google/gemini-2.5-flash）。
 * @param {*} value 待解析的字符串。
 * @returns {{providerID: string, modelID: string}|null} 缺 provider 或 model 时返回 null。
 */
export function parseModelRef(value) {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  const slash = trimmed.indexOf('/');
  if (slash <= 0 || slash === trimmed.length - 1) return null;
  return {
    providerID: trimmed.slice(0, slash),
    modelID: trimmed.slice(slash + 1),
  };
}

/**
 * 在一个 provider 的模型表里挑选指定 family 的最新模型（按 release_date 降序）。
 * @param {Object} models 以 model id 为键的模型表。
 * @param {string} family 目标家族名。
 * @returns {Object|null} 最新匹配的模型对象；无匹配时返回 null。
 */
const pickByFamily = (models, family) => {
  const matches = Object.values(models)
    .filter((model) => model && typeof model === 'object' && model.family === family);
  if (matches.length === 0) return null;
  matches.sort((a, b) => String(b.release_date || '').localeCompare(String(a.release_date || '')));
  return matches[0];
};

// Small-model candidates within ONE provider, by family priority. Copilot and
// ChatGPT-plan OpenAI have fixed small models that never appear in the
// catalog; everyone else is scanned through the catalog families.
/**
 * 在单个 provider 内按家族挑出小模型候选。ChatGPT 计划的 openai 与
 * GitHub Copilot 使用固定模型（目录中不存在），其余 provider 依赖目录的
 * family 元数据扫描。
 * @returns {{providerID: string, modelID: string, source: string}|null} source 标明候选的来源。
 */
const pickWithinProvider = (providerID, auth, catalog, family) => {
  if (providerID === 'openai' && auth.openai?.type === 'oauth') {
    return family === 'gpt-nano'
      ? { providerID, modelID: OPENAI_OAUTH_SMALL_MODEL, source: 'codex-small' }
      : null;
  }
  if (providerID === 'github-copilot') {
    return family === 'gpt-nano'
      ? { providerID, modelID: COPILOT_UTILITY_MODELS[0], source: 'copilot-utility' }
      : null;
  }
  const provider = getCatalogProvider(catalog, providerID);
  if (!provider || !provider.models || typeof provider.models !== 'object') return null;
  const model = pickByFamily(provider.models, family);
  return model?.id ? { providerID, modelID: model.id, source: 'family-scan' } : null;
};

/**
 * 解析最终使用的小模型，优先级从高到低：
 * 1. OMPChamber 设置覆盖（source: 'settings'）；
 * 2. OpenCode 配置的 small_model（source: 'config'）；
 * 3. 会话 provider 有可用登录时：其内部家族扫描（'family-scan'），扫不出
 *    则退回会话自身模型（'session-model'），绝不静默切换到别的订阅；
 * 4. 全部已认证 provider 按家族优先级扫描（'family-scan'）；
 * 5. Copilot 工具模型兜底（'copilot-utility'，覆盖 legacy 别名漏扫的情况）。
 * @param {Object} params auth 为 auth.json 数据，catalog 为模型目录，
 *   settingsSmallModel/configSmallModel 为两级显式配置，
 *   preferredProviderID/preferredModelID 描述会话上下文。
 * @returns {{providerID: string, modelID: string, source: string}|null} 无任何可用组合时返回 null。
 */
export function resolveSmallModel({ auth, catalog, settingsSmallModel, configSmallModel, preferredProviderID, preferredModelID }) {
  // OMPChamber's own setting (Settings → Sessions → Small Model override)
  // outranks everything, including the OpenCode config.
  const fromSettings = parseModelRef(settingsSmallModel);
  if (fromSettings) {
    return { ...fromSettings, source: 'settings' };
  }

  const explicit = parseModelRef(configSmallModel);
  if (explicit) {
    return { ...explicit, source: 'config' };
  }

  // Like OpenCode: when the caller has a session context, the utility call
  // stays on the session's provider. Scan its families for a small model,
  // otherwise run on the session's own model — never silently switch to a
  // different provider's subscription.
  const preferred = typeof preferredProviderID === 'string' && preferredProviderID
    ? preferredProviderID
    : null;
  if (preferred && isUsableAuthEntry(getAuthEntryForProvider(auth, preferred))) {
    for (const family of FAMILY_PRIORITY) {
      const match = pickWithinProvider(preferred, auth, catalog, family);
      if (match) return match;
    }
    if (typeof preferredModelID === 'string' && preferredModelID) {
      return { providerID: preferred, modelID: preferredModelID, source: 'session-model' };
    }
  }

  // No session context (or its provider has no usable login): scan all
  // authenticated providers by family priority.
  const authedProviders = Object.keys(auth || {}).filter((providerID) =>
    providerID !== preferred && isUsableAuthEntry(auth[providerID]));

  for (const family of FAMILY_PRIORITY) {
    for (const providerID of authedProviders) {
      const match = pickWithinProvider(providerID, auth, catalog, family);
      if (match) return match;
    }
  }

  // Copilot's utility fallback for legacy auth aliases the loop above missed.
  const copilotEntry = getAuthEntryForProvider(auth, 'github-copilot');
  if (isUsableAuthEntry(copilotEntry)) {
    return {
      providerID: 'github-copilot',
      modelID: COPILOT_UTILITY_MODELS[0],
      source: 'copilot-utility',
    };
  }

  return null;
}
