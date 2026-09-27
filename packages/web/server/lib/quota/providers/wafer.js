/**
 * Wafer.ai 配额 provider。
 *
 * 请求 pass.wafer.ai 的配额端点，读取当前计费周期的剩余请求数、
 * 包含上限与超额请求数，组装成单个用量窗口；请求带 15 秒超时。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp,
  resolveWindowLabel,
  asNonEmptyString
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'wafer';
/** 展示名。 */
export const providerName = 'Wafer.ai';
/** OpenCode auth 文件中的凭据匹配别名（覆盖多种拼写形式）。 */
const aliases = ['wafer', 'wafer-ai', 'wafer_ai', 'wafer.ai'];

/** Wafer.ai 配额查询端点。 */
const WAFER_QUOTA_URL = 'https://pass.wafer.ai/v1/inference/quota';
/** 端点未返回窗口起止时间时的兜底窗口时长：5 小时。 */
const WAFER_WINDOW_SECONDS = 5 * 3600;

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 Wafer.ai 配额：窗口时长优先由 window_start/window_end 推导，
 * 缺失时按 5 小时兜底；存在超额请求时不钳制百分比（允许超过 100）；
 * valueLabel 拼接套餐档位、剩余量与超额数量；响应中没有任何配额字段时
 * 返回 "No quota data in response"。15 秒超时与 JSON 解析失败有独立文案。
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
    const response = await fetch(WAFER_QUOTA_URL, {
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
        error: `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    const remaining = toNumber(payload?.remaining_included_requests);
    const limit = toNumber(payload?.included_request_limit);
    const overage = toNumber(payload?.overage_request_count);
    const usedPercentRaw = toNumber(payload?.current_period_used_percent);
    const windowStart = toTimestamp(payload?.window_start);
    const windowEnd = toTimestamp(payload?.window_end);
    const planTier = asNonEmptyString(payload?.plan_tier);

    if (remaining === null && limit === null && overage === null && usedPercentRaw === null) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: 'No quota data in response'
      });
    }

    const hasOverage = overage !== null && overage > 0;
    const usedPercent = hasOverage
      ? Math.max(0, usedPercentRaw ?? 0)
      : Math.max(0, Math.min(100, usedPercentRaw ?? 0));

    const windowSeconds = windowStart !== null && windowEnd !== null
      ? Math.round((windowEnd - windowStart) / 1000)
      : WAFER_WINDOW_SECONDS;
    const windowLabel = resolveWindowLabel(windowSeconds);

    let valueLabel = null;
    if (remaining !== null && limit !== null) {
      const parts = [];
      if (planTier) parts.push(planTier);
      parts.push(`${remaining} / ${limit} left`);
      if (hasOverage) parts.push(`+${overage} overage`);
      valueLabel = parts.join(' · ');
    }

    const windows = {};
    windows[windowLabel] = toUsageWindow({
      usedPercent,
      windowSeconds,
      resetAt: windowEnd,
      valueLabel
    });

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
