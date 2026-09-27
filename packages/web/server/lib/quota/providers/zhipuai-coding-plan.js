/**
 * Zhipu AI Coding Plan quota fetch
 *
 * API: https://open.bigmodel.cn/api/monitor/usage/quota/limit
 *
 * Response limits:
 * - TOKENS_LIMIT: Token usage (5-hour rolling window)
 * - TIME_LIMIT: MCP tools usage (monthly window)
 *
 * @typedef {Object} TokensLimit
 * @property {string} type - 'TOKENS_LIMIT'
 * @property {number} [unit]
 * @property {number} [number]
 * @property {number} [nextResetTime]
 * @property {number} [percentage]
 *
 * @typedef {Object} McpToolsTimeLimit
 * @property {string} type - 'TIME_LIMIT'
 * @property {number} [unit]
 * @property {number} [number]
 * @property {number} [usage]
 * @property {number} [currentValue]
 * @property {number} [remaining]
 * @property {number} [percentage]
 * @property {number} [nextResetTime]
 * @property {Array<{modelCode: string, usage: number}>} [usageDetails]
 */
/**
 * 中文说明：智谱 AI Coding Plan（bigmodel.cn）配额 provider。
 * 端点返回两类限额：TOKENS_LIMIT（Token 用量，5 小时滚动窗口）与
 * TIME_LIMIT（MCP 工具用量，按月窗口），分别映射为 Tokens 与
 * MCP Tools 两个窗口；API key 支持从 auth 文件或配置层解析。
 */
import { readAuthFile } from '../../opencode/auth.js';
import { readConfigLayers } from '../../opencode/shared.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  resolveWindowSeconds,
  normalizeTimestamp
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'zhipuai-coding-plan';
/** 展示名。 */
export const providerName = 'Zhipu AI Coding Plan';
/** OpenCode auth 文件与配置层中的凭据匹配别名。 */
const aliases = ['zhipuai-coding-plan', 'zhipuai', 'zhipu'];

/**
 * 解析智谱 API key：优先取 OpenCode auth 文件中任一别名下的 key/token，
 * 均缺失时回退读取配置层 mergedConfig.provider[alias].options.apiKey；
 * 配置读取失败按未配置处理。
 * @returns {string|null} 可用的 API key，无则返回 null
 */
function getApiKey() {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  const apiKeyFromAuth = entry?.key ?? entry?.token;

  if (apiKeyFromAuth) {
    return apiKeyFromAuth;
  }

  try {
    const { mergedConfig } = readConfigLayers();

    for (const alias of aliases) {
      const providerConfig = mergedConfig?.provider?.[alias];
      if (providerConfig?.options?.apiKey) {
        return providerConfig.options.apiKey;
      }
    }
  } catch {
    // Ignore config read errors; the provider will be treated as not configured.
  }

  return null;
}

/** 只要能解析出 API key（auth 文件或配置层任一来源）即视为已配置。 */
export const isConfigured = () => {
  return Boolean(getApiKey());
};

/**
 * 拉取智谱 Coding Plan 配额：TOKENS_LIMIT 映射为 Tokens 窗口（时长由
 * resolveWindowSeconds 推导），TIME_LIMIT 映射为固定 30 天的 MCP Tools
 * 窗口；未配置、HTTP 错误或异常均返回结构化失败结果。
 */
export const fetchQuota = async () => {
  const apiKey = getApiKey();

  if (!apiKey) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  try {
    const response = await fetch('https://open.bigmodel.cn/api/monitor/usage/quota/limit', {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${apiKey}`,
        'Content-Type': 'application/json'
      }
    });

    if (!response.ok) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    const limits = Array.isArray(payload?.data?.limits) ? payload.data.limits : [];

    const tokensLimit = limits.find((limit) => limit?.type === 'TOKENS_LIMIT');
    const mcpToolsTimeLimit = limits.find((limit) => limit?.type === 'TIME_LIMIT');

    const windows = {};

    // Handle TOKENS_LIMIT (5-hour window for token usage)
    if (tokensLimit) {
      const windowSeconds = resolveWindowSeconds(tokensLimit);
      const resetAt = tokensLimit?.nextResetTime ? normalizeTimestamp(tokensLimit.nextResetTime) : null;
      const usedPercent = typeof tokensLimit?.percentage === 'number' ? tokensLimit.percentage : null;

      windows['Tokens'] = toUsageWindow({
        usedPercent,
        windowSeconds,
        resetAt
      });
    }

    // Handle TIME_LIMIT (MCP tools monthly window)
    if (mcpToolsTimeLimit) {
      // TIME_LIMIT unit=5 means 1 month (30 days)
      const monthSeconds = 30 * 24 * 60 * 60;
      const resetAt = mcpToolsTimeLimit?.nextResetTime ? normalizeTimestamp(mcpToolsTimeLimit.nextResetTime) : null;
      const usedPercent = typeof mcpToolsTimeLimit?.percentage === 'number' ? mcpToolsTimeLimit.percentage : null;

      windows['MCP Tools'] = toUsageWindow({
        usedPercent,
        windowSeconds: monthSeconds,
        resetAt
      });
    }

    return buildResult({
      providerId,
      providerName,
      ok: true,
      configured: true,
      usage: { windows }
    });
  } catch (error) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: true,
      error: error instanceof Error ? error.message : 'Request failed'
    });
  }
};
