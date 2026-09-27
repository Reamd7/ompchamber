/**
 * OpenRouter 配额 provider。
 *
 * 请求 openrouter.ai 的 credits 端点，把总充值与总消耗换算为剩余额度，
 * 仅以 valueLabel（"$X left · $Y spent"）展示，不提供百分比用量。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  formatMoney
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'openrouter';
/** 展示名。 */
export const providerName = 'OpenRouter';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['openrouter'];

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 OpenRouter 积分：剩余额度 = max(0, total_credits - total_usage)；
 * total_credits 与 total_usage 缺任一时不出 valueLabel；
 * 未配置、HTTP 错误或异常均返回结构化失败结果。
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
    const response = await fetch('https://openrouter.ai/api/v1/credits', {
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
    const credits = payload?.data ?? {};
    const totalCredits = toNumber(credits.total_credits);
    const totalUsage = toNumber(credits.total_usage);
    const remaining = totalCredits !== null && totalUsage !== null
      ? Math.max(0, totalCredits - totalUsage)
      : null;
    let valueLabel = null;
    if (remaining !== null && totalUsage !== null) {
      valueLabel = `$${formatMoney(remaining)} left · $${formatMoney(totalUsage)} spent`;
    }

    const windows = {
      credits: toUsageWindow({
        usedPercent: null,
        windowSeconds: null,
        resetAt: null,
        valueLabel
      })
    };

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
