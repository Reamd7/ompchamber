/**
 * OpenCode Go 配额 provider。
 *
 * 请求 opencode.ai 的 Go 用量 API，读取 rolling/weekly/monthly 三个
 * 用量窗口的百分比与重置时间；API key 取自 OpenCode auth 文件，
 * 拉取时顺带清理历史版本遗留在托管凭据存储中的文件。
 */
import { readAuthFile } from '../../opencode/auth.js';
import { deleteLegacyOpenCodeGoCredential } from '../credentials/store.js';
import { buildResult, getAuthEntry, normalizeAuthEntry, toUsageWindow } from '../utils/index.js';

/** 对外 provider 标识。 */
export const providerId = 'opencode-go';
/** 展示名。 */
export const providerName = 'OpenCode Go';
/** OpenCode auth 文件中的凭据匹配别名（导出供测试与凭据清理使用）。 */
export const aliases = ['opencode-go'];

/** 输出窗口 key 与 API usage 字段名的映射（rolling 即 5 小时滚动窗口）。 */
const windowsByApiKey = {
  '5h': 'rolling',
  weekly: 'weekly',
  monthly: 'monthly',
};

/**
 * 解析 Go 用量 API 的响应载荷为窗口表。
 * 只接受 percent 为有限数字且 resetsAt 为可解析日期字符串的条目，
 * 百分比钳制到 [0,100]；payload 缺失或 usage 为空时返回空对象。
 * @param {object|null} payload API 的 JSON 响应
 * @returns {Object<string, object>} '5h' / weekly / monthly -> 窗口对象
 */
export const parseOpenCodeGoUsage = (payload) => {
  const usage = payload && typeof payload === 'object' ? payload.usage : null;
  if (!usage || typeof usage !== 'object') return {};
  const windows = {};
  for (const [key, apiKey] of Object.entries(windowsByApiKey)) {
    const entry = usage[apiKey];
    if (!entry || typeof entry !== 'object') continue;
    const usedPercent = entry.percent;
    const resetAt = entry.resetsAt;
    if (typeof usedPercent !== 'number' || !Number.isFinite(usedPercent)) continue;
    if (typeof resetAt !== 'string' || !Number.isFinite(new Date(resetAt).getTime())) continue;
    windows[key] = toUsageWindow({
      usedPercent: Math.min(100, Math.max(0, usedPercent)),
      resetAt,
      windowSeconds: null,
    });
  }
  return windows;
};

/**
 * 以 bearer key 请求 OpenCode Go 用量 API 并解析窗口。
 * 401/403 抛认证失败（错误信息不含密钥本身）；响应解析不出任何
 * 窗口时抛错。
 * @param {string} apiKey OpenCode Go API key
 * @param {Function} [fetchImpl] 可注入的 fetch 实现（测试用）
 * @returns {Promise<Object<string, object>>} 窗口表
 * @throws {Error} 认证失败、HTTP 错误或用量无法解析时
 */
export const fetchOpenCodeGoUsage = async (apiKey, fetchImpl = fetch) => {
  const response = await fetchImpl('https://opencode.ai/zen/go/v1/usage', {
    headers: {
      Accept: 'application/json',
      Authorization: `Bearer ${apiKey}`,
      'User-Agent': 'OMPChamber quota provider',
    },
    signal: AbortSignal.timeout(15_000),
  });
  if (response.status === 401 || response.status === 403) {
    throw new Error('OpenCode Go authentication failed');
  }
  if (!response.ok) throw new Error(`OpenCode Go usage API returned HTTP ${response.status}`);
  const windows = parseOpenCodeGoUsage(await response.json().catch(() => null));
  if (Object.keys(windows).length === 0) throw new Error('OpenCode Go usage data could not be parsed');
  return windows;
};

/** 从 OpenCode auth 文件解析 opencode-go 别名下的 key/token，缺失返回 null。 */
const getApiKey = () => {
  const entry = normalizeAuthEntry(getAuthEntry(readAuthFile(), aliases));
  return entry?.key ?? entry?.token ?? null;
};

/** 能从 auth 文件解析出 API key 即视为已配置。 */
export const isConfigured = () => Boolean(getApiKey());

/**
 * 拉取 OpenCode Go 配额：先清理遗留凭据文件（老版本把 key 存在托管
 * 存储），再从 auth 文件读取 key 请求用量 API；未配置返回
 * configured:false，任何异常统一转换为 ok:false 的结构化结果。
 */
export const fetchQuota = async () => {
  try {
    deleteLegacyOpenCodeGoCredential();
    const apiKey = getApiKey();
    if (!apiKey) return buildResult({ providerId, providerName, ok: false, configured: false, error: 'Not configured' });
    const windows = await fetchOpenCodeGoUsage(apiKey);
    return buildResult({ providerId, providerName, ok: true, configured: true, usage: { windows } });
  } catch (error) {
    return buildResult({ providerId, providerName, ok: false, configured: true, error: error instanceof Error ? error.message : 'Request failed' });
  }
};
