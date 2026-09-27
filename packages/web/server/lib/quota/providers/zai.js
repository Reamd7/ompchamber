/**
 * z.ai Coding Plan 配额 provider。
 *
 * 请求 api.z.ai 的配额限额端点，把 TOKENS_LIMIT 与改名后的 CREDIT_LIMIT
 * 条目（字段语义相同）按时长映射为 5h/weekly 等窗口，TIME_LIMIT 条目
 * 映射为 MCP Tools 月窗口，并把套餐等级 data.level 作为 planLabel 透出。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  resolveWindowSeconds,
  resolveWindowLabel,
  normalizeTimestamp
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'zai-coding-plan';
/** 展示名。 */
export const providerName = 'z.ai';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['zai-coding-plan', 'zai', 'z.ai'];

/** 把积分数量格式化为紧凑展示：千位以内用 toLocaleString，超过则缩写为 "12k" 风格。 */
// CREDIT_LIMIT entries carry `usage` (total credits), `currentValue` (consumed),
// and `remaining`; TOKENS_LIMIT entries only carry a percentage.
const formatCreditAmount = (value) => {
  if (value < 1000) return value.toLocaleString('en-US');
  return `${Math.round(value / 100) / 10}k`;
};

/**
 * 由 CREDIT_LIMIT 条目的 currentValue（已用）与 usage（总量）生成
 * "65 / 12k credits" 风格的 valueLabel；任一字段缺失返回 null。
 * @param {object|null} limit 限额条目
 * @returns {string|null}
 */
const formatCreditValueLabel = (limit) => {
  const used = toNumber(limit?.currentValue);
  const total = toNumber(limit?.usage);
  if (used === null || total === null) return null;
  return `${formatCreditAmount(used)} / ${formatCreditAmount(total)} credits`;
};

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 z.ai 配额：遍历 TOKENS_LIMIT/CREDIT_LIMIT 条目，按 resolveWindowSeconds
 * 推导的时长生成窗口（百分比直取 percentage 字段，积分明细放入 valueLabel）；
 * TIME_LIMIT 条目生成固定 30 天的 MCP Tools 窗口；未配置、HTTP 错误或
 * 异常均返回结构化失败结果。
 */
export const fetchQuota = async () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  const apiKey = entry?.key ?? entry?.token;

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
    const response = await fetch('https://api.z.ai/api/monitor/usage/quota/limit', {
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
    const windows = {};
    // The API renamed TOKENS_LIMIT to CREDIT_LIMIT; field semantics stayed the same,
    // so both limit types map to the same windows.
    for (const limit of limits.filter((entry) => entry?.type === 'TOKENS_LIMIT' || entry?.type === 'CREDIT_LIMIT')) {
      const windowSeconds = resolveWindowSeconds(limit);
      const windowLabel = resolveWindowLabel(windowSeconds);
      const resetAt = limit?.nextResetTime ? normalizeTimestamp(limit.nextResetTime) : null;
      const usedPercent = typeof limit?.percentage === 'number' ? limit.percentage : null;
      const creditValueLabel = formatCreditValueLabel(limit);

      windows[windowLabel] = toUsageWindow({
        usedPercent,
        windowSeconds,
        resetAt,
        valueLabel: creditValueLabel
      });
    }

    const mcpToolsTimeLimit = limits.find((limit) => limit?.type === 'TIME_LIMIT');
    if (mcpToolsTimeLimit) {
      windows['MCP Tools'] = toUsageWindow({
        usedPercent: typeof mcpToolsTimeLimit.percentage === 'number' ? mcpToolsTimeLimit.percentage : null,
        windowSeconds: 30 * 24 * 60 * 60,
        resetAt: mcpToolsTimeLimit.nextResetTime ? normalizeTimestamp(mcpToolsTimeLimit.nextResetTime) : null
      });
    }

    return buildResult({
      providerId,
      providerName,
      ok: true,
      configured: true,
      usage: { windows },
      planLabel: typeof payload?.data?.level === 'string' && payload.data.level ? payload.data.level : null
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
