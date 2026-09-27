/**
 * Quota Providers Registry
 *
 * Implements quota fetching for various AI providers using a registry pattern.
 * @module quota/providers
 */
/**
 * 配额 provider 注册表（中文说明）。
 *
 * 汇总 lib/quota/providers 下各 AI 服务商的配额查询模块，以 registry 统一
 * 暴露 providerId / providerName / isConfigured / fetchQuota 四个成员，
 * 并提供三类能力：列出已配置的 provider、按 id 拉取配额（含并发请求合并）、
 * 以及一组按 provider 命名的转发导出（兼容按名调用的既有调用方）。
 */

import { buildResult } from '../utils/index.js';

import * as claude from './claude/index.js';
import * as codex from './codex.js';
import * as copilot from './copilot.js';
import * as crof from './crof.js';
import * as cursor from './cursor.js';
import * as deepseek from './deepseek.js';
import * as google from './google/index.js';
import * as kimi from './kimi.js';
import * as nanogpt from './nanogpt.js';
import * as openai from './openai.js';
import * as openrouter from './openrouter.js';
import * as zai from './zai.js';
import * as zhipuaiCodingPlan from './zhipuai-coding-plan.js';
import * as minimaxCodingPlan from './minimax-coding-plan.js';
import * as minimaxCnCodingPlan from './minimax-cn-coding-plan.js';
import * as neuralwatt from './neuralwatt.js';
import * as ollamaCloud from './ollama-cloud.js';
import * as wafer from './wafer.js';
import * as opencodeGo from './opencode-go.js';
import * as xai from './xai.js';

/**
 * provider 注册表：key 为对外暴露的 provider 标识（同时是
 * fetchQuotaForProvider 的 providerId 入参），value 从对应模块的导出聚合，
 * 以抹平各模块的导出差异（如 google 的 fetchGoogleQuota、
 * copilot 的 fetchQuotaAddon）。
 */
const registry = {
  claude: {
    providerId: claude.providerId,
    providerName: claude.providerName,
    isConfigured: claude.isConfigured,
    fetchQuota: claude.fetchQuota
  },
  codex: {
    providerId: codex.providerId,
    providerName: codex.providerName,
    isConfigured: codex.isConfigured,
    fetchQuota: codex.fetchQuota
  },
  crof: {
    providerId: crof.providerId,
    providerName: crof.providerName,
    isConfigured: crof.isConfigured,
    fetchQuota: crof.fetchQuota
  },
  cursor: {
    providerId: cursor.providerId,
    providerName: cursor.providerName,
    isConfigured: cursor.isConfigured,
    fetchQuota: cursor.fetchQuota
  },
  deepseek: {
    providerId: deepseek.providerId,
    providerName: deepseek.providerName,
    isConfigured: deepseek.isConfigured,
    fetchQuota: deepseek.fetchQuota
  },
  google: {
    providerId: google.providerId,
    providerName: google.providerName,
    isConfigured: google.isConfigured,
    fetchQuota: google.fetchGoogleQuota
  },
  'zai-coding-plan': {
    providerId: zai.providerId,
    providerName: zai.providerName,
    isConfigured: zai.isConfigured,
    fetchQuota: zai.fetchQuota
  },
  'zhipuai-coding-plan': {
    providerId: zhipuaiCodingPlan.providerId,
    providerName: zhipuaiCodingPlan.providerName,
    isConfigured: zhipuaiCodingPlan.isConfigured,
    fetchQuota: zhipuaiCodingPlan.fetchQuota
  },
  'kimi-for-coding': {
    providerId: kimi.providerId,
    providerName: kimi.providerName,
    isConfigured: kimi.isConfigured,
    fetchQuota: kimi.fetchQuota
  },
  openrouter: {
    providerId: openrouter.providerId,
    providerName: openrouter.providerName,
    isConfigured: openrouter.isConfigured,
    fetchQuota: openrouter.fetchQuota
  },
  'nano-gpt': {
    providerId: nanogpt.providerId,
    providerName: nanogpt.providerName,
    isConfigured: nanogpt.isConfigured,
    fetchQuota: nanogpt.fetchQuota
  },
  'github-copilot': {
    providerId: copilot.providerId,
    providerName: copilot.providerName,
    isConfigured: copilot.isConfigured,
    fetchQuota: copilot.fetchQuota
  },
  'github-copilot-addon': {
    providerId: copilot.providerIdAddon,
    providerName: copilot.providerNameAddon,
    isConfigured: copilot.isConfigured,
    fetchQuota: copilot.fetchQuotaAddon
  },
  'minimax-coding-plan': {
    providerId: minimaxCodingPlan.providerId,
    providerName: minimaxCodingPlan.providerName,
    isConfigured: minimaxCodingPlan.isConfigured,
    fetchQuota: minimaxCodingPlan.fetchQuota
  },
  'minimax-cn-coding-plan': {
    providerId: minimaxCnCodingPlan.providerId,
    providerName: minimaxCnCodingPlan.providerName,
    isConfigured: minimaxCnCodingPlan.isConfigured,
    fetchQuota: minimaxCnCodingPlan.fetchQuota
  },
  'ollama-cloud': {
    providerId: ollamaCloud.providerId,
    providerName: ollamaCloud.providerName,
    isConfigured: ollamaCloud.isConfigured,
    fetchQuota: ollamaCloud.fetchQuota
  },
  wafer: {
    providerId: wafer.providerId,
    providerName: wafer.providerName,
    isConfigured: wafer.isConfigured,
    fetchQuota: wafer.fetchQuota
  },
  'opencode-go': {
    providerId: opencodeGo.providerId,
    providerName: opencodeGo.providerName,
    isConfigured: opencodeGo.isConfigured,
    fetchQuota: opencodeGo.fetchQuota
  },
  neuralwatt: {
    providerId: neuralwatt.providerId,
    providerName: neuralwatt.providerName,
    isConfigured: neuralwatt.isConfigured,
    fetchQuota: neuralwatt.fetchQuota
  },
  xai: {
    providerId: xai.providerId,
    providerName: xai.providerName,
    isConfigured: xai.isConfigured,
    fetchQuota: xai.fetchQuota
  }
};

/**
 * 进行中的配额请求合并表：providerId -> 进行中的 Promise。
 * 同一 provider 的并发请求共享同一个 Promise；请求结束（无论成败）后
 * 从表中移除，保证后续刷新会发起真正的新请求。
 */
const pendingFetches = new Map();


/**
 * 列出当前已配置（凭据可用）的所有 provider 标识。
 * 逐个调用注册表内 provider 的 isConfigured()；单个 provider 抛错时
 * 忽略该错误（避免拖垮整个列表接口），只收集返回 true 的 id。
 * @returns {string[]} 已配置的 provider id 数组
 */
export const listConfiguredQuotaProviders = () => {
  const configured = [];

  for (const [id, provider] of Object.entries(registry)) {
    try {
      if (provider.isConfigured()) {
        configured.push(id);
      }
    } catch {
      // Ignore provider-specific config errors in list API.
    }
  }

  return configured;
};

/**
 * 真正执行配额请求（未做并发合并的版本）。
 * providerId 未注册时返回 configured:false 的失败结果；provider 自身
 * 抛出异常时在此捕获并转换为 ok:false 的结构化结果，保证调用方
 * 永远拿到 buildResult 形状的数据而不是异常。
 * @param {string} providerId 注册表中的 provider 标识
 * @returns {Promise<object>} 统一的配额结果对象
 */
const fetchQuotaForProviderUncoalesced = async (providerId) => {
  const provider = registry[providerId];

  if (!provider) {
    return buildResult({
      providerId,
      providerName: providerId,
      ok: false,
      configured: false,
      error: 'Unsupported provider'
    });
  }

  try {
    return await provider.fetchQuota();
  } catch (error) {
    return buildResult({
      providerId: provider.providerId,
      providerName: provider.providerName,
      ok: false,
      configured: true,
      error: error instanceof Error ? error.message : 'Request failed'
    });
  }
};

/**
 * 按 providerId 拉取配额，并对同一 provider 的并发请求做合并。
 * 已有进行中的请求时直接复用该 Promise；否则发起请求并在完成后
 * 从 pendingFetches 清除自己（仅当仍是同一个 Promise 时才清除，
 * 防止竞态下误删后来者的记录）。
 * @param {string} providerId 注册表中的 provider 标识
 * @returns {Promise<object>} 统一的配额结果对象
 */
export const fetchQuotaForProvider = (providerId) => {
  const existing = pendingFetches.get(providerId);
  if (existing) return existing;

  const pending = fetchQuotaForProviderUncoalesced(providerId).finally(() => {
    if (pendingFetches.get(providerId) === pending) pendingFetches.delete(providerId);
  });
  pendingFetches.set(providerId, pending);
  return pending;
};

/** 转发导出：Claude 订阅配额查询，直接复用 claude 模块的 fetchQuota。 */
export const fetchClaudeQuota = claude.fetchQuota;
/** 转发导出：OpenAI（ChatGPT 后端）限流窗口查询，复用 openai 模块的 fetchQuota。 */
export const fetchOpenaiQuota = openai.fetchQuota;
/** 转发导出：Google 配额查询，复用 google 模块的 fetchGoogleQuota。 */
export const fetchGoogleQuota = google.fetchGoogleQuota;
/** 转发导出：Codex（ChatGPT 订阅）配额查询，复用 codex 模块的 fetchQuota。 */
export const fetchCodexQuota = codex.fetchQuota;
/** 转发导出：Cursor 配额查询，复用 cursor 模块的 fetchQuota。 */
export const fetchCursorQuota = cursor.fetchQuota;
/** 转发导出：DeepSeek 余额查询，复用 deepseek 模块的 fetchQuota。 */
export const fetchDeepseekQuota = deepseek.fetchQuota;
/** 转发导出：GitHub Copilot 主订阅配额查询，复用 copilot 模块的 fetchQuota。 */
export const fetchCopilotQuota = copilot.fetchQuota;
/** 转发导出：GitHub Copilot 附加订阅（add-on）配额查询，复用 copilot 模块的 fetchQuotaAddon。 */
export const fetchCopilotAddonQuota = copilot.fetchQuotaAddon;
/** 转发导出：Kimi for Coding 配额查询，复用 kimi 模块的 fetchQuota。 */
export const fetchKimiQuota = kimi.fetchQuota;
/** 转发导出：OpenRouter 积分查询，复用 openrouter 模块的 fetchQuota。 */
export const fetchOpenRouterQuota = openrouter.fetchQuota;
/** 转发导出：z.ai Coding Plan 配额查询，复用 zai 模块的 fetchQuota。 */
export const fetchZaiQuota = zai.fetchQuota;
/** 智谱 Coding Plan 配额查询的内部引用，供下方 fetchZhipuaiQuota 历史别名复用。 */
const fetchZhipuaiCodingPlanQuota = zhipuaiCodingPlan.fetchQuota;
/** 转发导出：NanoGPT 配额查询，复用 nanogpt 模块的 fetchQuota。 */
export const fetchNanoGptQuota = nanogpt.fetchQuota;
/** 转发导出：MiniMax Coding Plan 国际站（minimax.io）配额查询。 */
export const fetchMinimaxCodingPlanQuota = minimaxCodingPlan.fetchQuota;
/** 转发导出：MiniMax Coding Plan 国内站（minimaxi.com）配额查询。 */
export const fetchMinimaxCnCodingPlanQuota = minimaxCnCodingPlan.fetchQuota;
/** 转发导出：Ollama Cloud 配额查询，复用 ollama-cloud 模块的 fetchQuota。 */
export const fetchOllamaCloudQuota = ollamaCloud.fetchQuota;
/** 转发导出：Wafer.ai 配额查询，复用 wafer 模块的 fetchQuota。 */
export const fetchWaferQuota = wafer.fetchQuota;
/** 智谱配额查询的历史别名：指向 zhipuai-coding-plan 的 fetchQuota，保持旧调用方可用。 */
export const fetchZhipuaiQuota = zhipuaiCodingPlan.fetchQuota;
