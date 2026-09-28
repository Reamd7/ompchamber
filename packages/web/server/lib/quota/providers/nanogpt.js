/**
 * NanoGPT 配额 provider。
 *
 * 请求 nano-gpt.com 的订阅用量端点，读取 daily 与 monthly 两个用量块；
 * 已用百分比优先取 API 给出的 percentUsed（0~1 小数），否则由
 * used/limit 计算。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp
} from '../utils/index.js';

/** daily 窗口的固定时长：24 小时。 */
const NANO_GPT_DAILY_WINDOW_SECONDS = 86400;

/** 对外 provider 标识。 */
export const providerId = 'nano-gpt';
/** 展示名。 */
export const providerName = 'NanoGPT';
/** OpenCode auth 文件中的凭据匹配别名（覆盖多种拼写形式）。 */
const aliases = ['nano-gpt', 'nanogpt', 'nano_gpt'];

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 NanoGPT 配额：分别组装 daily（固定 24 小时）与 monthly 窗口；
 * 订阅 state 非 active 时在两个窗口的 valueLabel 中附上状态文案；
 * monthly 的重置时间缺失时回退 period.currentPeriodEnd；
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
    const response = await fetch('https://nano-gpt.com/api/subscription/v1/usage', {
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
    const windows = {};
    const period = payload?.period ?? null;
    const daily = payload?.daily ?? null;
    const monthly = payload?.monthly ?? null;
    const state = payload?.state ?? 'active';

    if (daily) {
      let usedPercent = null;
      const percentUsed = daily?.percentUsed;
      if (typeof percentUsed === 'number') {
        usedPercent = Math.max(0, Math.min(100, percentUsed * 100));
      } else {
        const used = toNumber(daily?.used);
        const limit = toNumber(daily?.limit ?? daily?.limits?.daily);
        if (used !== null && limit !== null && limit > 0) {
          usedPercent = Math.max(0, Math.min(100, (used / limit) * 100));
        }
      }
      const resetAt = toTimestamp(daily?.resetAt);
      const valueLabel = state !== 'active' ? `(${state})` : null;
      windows['daily'] = toUsageWindow({
        usedPercent,
        windowSeconds: NANO_GPT_DAILY_WINDOW_SECONDS,
        resetAt,
        valueLabel
      });
    }

    if (monthly) {
      let usedPercent = null;
      const percentUsed = monthly?.percentUsed;
      if (typeof percentUsed === 'number') {
        usedPercent = Math.max(0, Math.min(100, percentUsed * 100));
      } else {
        const used = toNumber(monthly?.used);
        const limit = toNumber(monthly?.limit ?? monthly?.limits?.monthly);
        if (used !== null && limit !== null && limit > 0) {
          usedPercent = Math.max(0, Math.min(100, (used / limit) * 100));
        }
      }
      const resetAt = toTimestamp(monthly?.resetAt ?? period?.currentPeriodEnd);
      const valueLabel = state !== 'active' ? `(${state})` : null;
      windows['monthly'] = toUsageWindow({
        usedPercent,
        windowSeconds: null,
        resetAt,
        valueLabel
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
