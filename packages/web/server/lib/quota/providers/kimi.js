/**
 * Kimi for Coding 配额 provider。
 *
 * 请求 api.kimi.com 的用量端点，把周用量块（usage）与各限流条目
 * （limits[]，按窗口时长命名，5 小时窗口额外加 "Rate Limit" 前缀）
 * 组装成窗口表。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp,
  durationToLabel,
  durationToSeconds
} from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'kimi-for-coding';
/** 展示名。 */
export const providerName = 'Kimi for Coding';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['kimi-for-coding', 'kimi'];

/** 由 total 与 used/remaining（二者取其一，used 优先）计算 [0,100] 的已用百分比；total 缺失返回 null。 */
// Kimi's weekly `usage` block reports `used`; its rate-limit `limits[].detail`
// blocks report `remaining` instead. Neither field is guaranteed present, so
// derive usedPercent from whichever one the API actually returned.
const computeUsedPercent = (total, used, remaining) => {
  if (!total) return null;
  if (used !== null) {
    return Math.max(0, Math.min(100, (used / total) * 100));
  }
  if (remaining !== null) {
    return Math.max(0, Math.min(100, 100 - (remaining / total) * 100));
  }
  return null;
};

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 Kimi 配额：usage 块输出 weekly 窗口；limits 数组逐条按
 * durationToLabel 生成的时长标签输出各限流窗口（5 小时窗口加
 * "Rate Limit" 前缀以便与周用量区分）；未配置、HTTP 错误或异常
 * 均返回结构化失败结果。
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
    const response = await fetch('https://api.kimi.com/coding/v1/usages', {
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
    const usage = payload?.usage ?? null;
    if (usage) {
      const limit = toNumber(usage.limit);
      const used = toNumber(usage.used);
      const remaining = toNumber(usage.remaining);
      const usedPercent = computeUsedPercent(limit, used, remaining);
      windows.weekly = toUsageWindow({
        usedPercent,
        windowSeconds: null,
        resetAt: toTimestamp(usage.resetTime)
      });
    }

    const limits = Array.isArray(payload?.limits) ? payload.limits : [];
    for (const limit of limits) {
      const window = limit?.window;
      const detail = limit?.detail;
      const rawLabel = durationToLabel(window?.duration, window?.timeUnit);
      const windowSeconds = durationToSeconds(window?.duration, window?.timeUnit);
      const label = windowSeconds === 5 * 60 * 60 ? `Rate Limit (${rawLabel})` : rawLabel;
      const total = toNumber(detail?.limit);
      const used = toNumber(detail?.used);
      const remaining = toNumber(detail?.remaining);
      const usedPercent = computeUsedPercent(total, used, remaining);
      windows[label] = toUsageWindow({
        usedPercent,
        windowSeconds,
        resetAt: toTimestamp(detail?.resetTime)
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
