/**
 * Ollama Cloud 配额 provider。
 *
 * Ollama Cloud 无公开 JSON API，本 provider 使用托管凭据存储中的 Cookie
 * 请求 ollama.com/settings 页面，再从返回的 HTML 中用正则解析
 * Session/Weekly 用量百分比与 Premium 用量，组装成窗口表。
 */
import { buildResult, toUsageWindow, toNumber } from '../utils/index.js';
import { readManagedCredential } from '../credentials/providers.js';

/** 对外 provider 标识。 */
export const providerId = 'ollama-cloud';
/** 展示名。 */
export const providerName = 'Ollama Cloud';
/** 托管凭据存储中的凭据匹配别名。 */
const aliases = ['ollama-cloud', 'ollamacloud'];

/**
 * 从 ollama.com/settings 页面 HTML 中解析用量窗口。
 * 依次匹配 "Session usage ... N%"、"Weekly usage ... N%" 与
 * "Premium used/total" 三种文案，命中即写入对应窗口：session/weekly
 * 只有百分比；premium 额外换算百分比并给出 "used / total" 文案。
 * @param {string} html 页面 HTML 文本
 * @returns {Object<string, object>} 窗口标签 -> 窗口对象；未命中的项不写入
 */
export const parseOllamaSettingsHtml = (html) => {
  const windows = {};
  const sessionMatch = html.match(/Session\s+usage[^0-9]*([0-9.]+)%/i);
  if (sessionMatch) {
    windows.session = toUsageWindow({
      usedPercent: toNumber(sessionMatch[1]),
      windowSeconds: null,
      resetAt: null
    });
  }
  const weeklyMatch = html.match(/Weekly\s+usage[^0-9]*([0-9.]+)%/i);
  if (weeklyMatch) {
    windows.weekly = toUsageWindow({
      usedPercent: toNumber(weeklyMatch[1]),
      windowSeconds: null,
      resetAt: null
    });
  }
  const premiumMatch = html.match(/Premium[^0-9]*([0-9]+)\s*\/\s*([0-9]+)/i);
  if (premiumMatch) {
    const used = toNumber(premiumMatch[1]);
    const total = toNumber(premiumMatch[2]);
    const usedPercent = total && used !== null ? Math.min(100, (used / total) * 100) : null;
    windows.premium = toUsageWindow({
      usedPercent,
      windowSeconds: null,
      resetAt: null,
      valueLabel: `${used ?? 0} / ${total ?? 0}`
    });
  }
  return windows;
};

/** Ollama Cloud 无 API key，凭托管凭据存储中是否存有 Cookie 判断是否已配置。 */
export const isConfigured = () => {
  return Boolean(readManagedCredential(providerId));
};

/**
 * 带 Cookie 请求 ollama.com/settings 并解析用量窗口。
 * redirect:'manual' 下收到 3xx、或 401/403 都视为认证失败（避免把凭据
 * 转发给登录重定向目标）；页面解析不出任何窗口时也抛错。
 * @param {{cookie: string}} credential 托管 Cookie 凭据
 * @param {Function} [fetchImpl] 可注入的 fetch 实现（测试用）
 * @returns {Promise<Object<string, object>>} 窗口表
 * @throws {Error} 认证失败、HTTP 错误或用量无法解析时
 */
export const fetchOllamaCloudUsage = async (credential, fetchImpl = fetch) => {
  const response = await fetchImpl('https://ollama.com/settings', {
    method: 'GET',
    headers: { Cookie: credential.cookie, 'User-Agent': 'OMPChamber quota provider' },
    redirect: 'manual',
    signal: AbortSignal.timeout(15_000),
  });
  if (response.status === 401 || response.status === 403 || (response.status >= 300 && response.status < 400)) {
    throw new Error('Ollama Cloud authentication failed');
  }
  if (!response.ok) throw new Error(`Ollama Cloud returned HTTP ${response.status}`);
  const windows = parseOllamaSettingsHtml(await response.text());
  if (Object.keys(windows).length === 0) throw new Error('Ollama Cloud usage data could not be parsed');
  return windows;
};

/**
 * 拉取 Ollama Cloud 配额：从托管凭据存储读取 Cookie，未配置返回
 * configured:false；实际请求与解析交由 fetchOllamaCloudUsage，
 * 异常统一转换为 ok:false 的结构化结果。
 */
export const fetchQuota = async () => {
  const credential = readManagedCredential(providerId);

  if (!credential) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  try {
    const windows = await fetchOllamaCloudUsage(credential);

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
