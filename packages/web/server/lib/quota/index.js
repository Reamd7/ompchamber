/**
 * Quota module
 *
 * Provides quota usage tracking for various AI provider services.
 * @module quota
 */
/**
 * quota 模块汇总出口（中文说明）：统一再导出各 provider 的配额查询函数
 * （Claude、OpenAI、Google、Cursor、Kimi、MiniMax 等），供路由与调用方
 * 按需引入，避免耦合 providers/ 目录的内部文件布局。
 */

export {
  listConfiguredQuotaProviders,
  fetchQuotaForProvider,
  fetchClaudeQuota,
  fetchOpenaiQuota,
  fetchGoogleQuota,
  fetchCodexQuota,
  fetchCursorQuota,
  fetchDeepseekQuota,
  fetchCopilotQuota,
  fetchCopilotAddonQuota,
  fetchKimiQuota,
  fetchOpenRouterQuota,
  fetchZaiQuota,
  fetchNanoGptQuota,
  fetchMinimaxCodingPlanQuota,
  fetchMinimaxCnCodingPlanQuota,
  fetchOllamaCloudQuota,
  fetchZhipuaiQuota,
  fetchWaferQuota
} from './providers/index.js';
