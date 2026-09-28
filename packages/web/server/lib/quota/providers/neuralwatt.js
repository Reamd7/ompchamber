/**
 * NeuralWatt 配额 provider。
 *
 * 查询 /v1/quota 端点，把订阅（kWh 限额）、API key allowance（按周期的
 * 美元限额）与 credits 余额（前两者皆无时的兜底）组装成用量窗口。
 * 请求带 15 秒超时；401 映射为会话过期提示。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp,
  formatMoney,
  asNonEmptyString
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'neuralwatt';
/** 展示名。 */
export const providerName = 'NeuralWatt';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['neuralwatt'];
/** NeuralWatt 配额查询端点。 */
const NEURALWATT_QUOTA_URL = 'https://api.neuralwatt.com/v1/quota';

/** 把 allowance 的周期字符串换算成近似的窗口秒数；未知周期返回 null。 */
// 30d month / 365d year are fixed approximations; real calendars vary but the
// window is for the UI's progress bar label, not billing decisions.
const periodToWindowSeconds = (period) => {
  if (period === 'daily') return 86400;
  if (period === 'weekly') return 604800;
  if (period === 'monthly' || period === 'month') return 30 * 86400;
  if (period === 'yearly' || period === 'year') return 365 * 86400;
  return null;
};

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 NeuralWatt 配额并组装窗口：
 * subscription 存在时以套餐名（缺失时用 plan_limit）为 key 输出订阅窗口，
 * in_overage 时固定 100%；key.allowance 存在时按周期 key 输出限额窗口，
 * 有效上限取 min(limit, credits 余额 + 已花费)，blocked 时固定 100%，
 * key 名附在 valueLabel 上；两者皆无时退回 credits_balance 余额展示。
 * 15 秒超时、401 会话过期、JSON 解析失败分别映射为不同错误文案；
 * 任何窗口都组装不出来时返回 "No quota data in response"。
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

  const timeoutSignal = AbortSignal.timeout(15_000);

  try {
    const response = await fetch(NEURALWATT_QUOTA_URL, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${apiKey}`,
        'Accept-Encoding': 'identity'
      },
      signal: timeoutSignal
    });

    if (!response.ok) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: response.status === 401
          ? 'Session expired — please re-authenticate with NeuralWatt'
          : `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    const subscription = payload?.subscription ?? null;
    const inOverage = Boolean(subscription?.in_overage);
    const allowance = payload?.key?.allowance ?? null;
    const keyName = payload?.key?.name ?? null;
    const creditsRemaining = toNumber(payload?.balance?.credits_remaining_usd);

    const windows = {};

    if (subscription) {
      const kwhIncluded = toNumber(subscription.kwh_included);
      const kwhUsed = toNumber(subscription.kwh_used);
      const plan = asNonEmptyString(subscription.plan);
      // Subscription window title is the plan name; subscription limits reset
      // monthly even on annual billing plans, but the API exposes no kWh window
      // start to derive windowSeconds — pass null rather than fabricating a guess.
      const subKey = plan ?? 'plan_limit';
      const usedPercent = inOverage
        ? 100
        : (kwhIncluded !== null && kwhIncluded > 0 && kwhUsed !== null
            ? Math.max(0, Math.min(100, (kwhUsed / kwhIncluded) * 100))
            : null);
      const subResetAt = toTimestamp(subscription.kwh_reset_date) ?? toTimestamp(subscription.current_period_end);
      windows[subKey] = toUsageWindow({
        usedPercent,
        windowSeconds: null,
        resetAt: subResetAt
      });
    }

    if (allowance) {
      const spent = toNumber(allowance.spent_usd);
      const limit = toNumber(allowance.limit_usd);
      // Credits wallet is reduced by each period's spend before the allowance cap
      // bites, so the real ceiling is min(limit, creditsRemaining + spent).
      const effectiveSpent = spent ?? 0;
      const effectiveLimit = limit !== null && creditsRemaining !== null
        ? Math.min(limit, creditsRemaining + effectiveSpent)
        : (limit ?? creditsRemaining);
      const period = asNonEmptyString(allowance.period);
      const blocked = Boolean(allowance.blocked);
      const usedPercent = blocked
        ? 100
        : (spent !== null && effectiveLimit !== null && effectiveLimit > 0
            ? Math.max(0, Math.min(100, (spent / effectiveLimit) * 100))
            : null);
      // Window title is the localized period label (daily/weekly/monthly); key
      // name is attached via valueLabel for identification (wafer precedent).
      const periodKey = (period === 'daily' || period === 'weekly' || period === 'monthly' || period === 'month')
        ? (period === 'month' ? 'monthly' : period)
        : 'billing_cycle';
      const labelName = asNonEmptyString(keyName);
      const resetAt = toTimestamp(allowance.reset_at);
      const windowSeconds = period ? periodToWindowSeconds(period) : null;
      windows[periodKey] = toUsageWindow({
        usedPercent,
        windowSeconds,
        resetAt,
        ...(labelName ? { valueLabel: labelName } : {})
      });
    } else if (creditsRemaining !== null) {
      windows.credits_balance = toUsageWindow({
        usedPercent: null,
        windowSeconds: null,
        resetAt: null,
        valueLabel: `$${formatMoney(creditsRemaining)}`
      });
    }

    if (Object.keys(windows).length === 0) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: 'No quota data in response'
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
    const isTimeout = error instanceof DOMException && error.name === 'AbortError' && timeoutSignal.aborted;
    const isParseError = error instanceof SyntaxError;
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: true,
      error: isTimeout
        ? 'Request timed out'
        : isParseError
          ? 'Invalid response from provider'
          : (error instanceof Error ? error.message : 'Request failed')
    });
  }
};
