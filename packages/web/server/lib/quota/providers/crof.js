/**
 * CrofAI 配额 provider。
 *
 * 请求 crof.ai 的 usage_api，把返回的 credits 余额格式化为 credits 窗口的
 * valueLabel；不提供百分比用量，仅展示余额。
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
export const providerId = 'crof';
/** 展示名。 */
export const providerName = 'CrofAI';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['crof'];
/** CrofAI 用量查询端点。 */
const CROF_USAGE_URL = 'https://crof.ai/usage_api/';

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 CrofAI 余额：credits 字段缺失时窗口仍成功返回、只是没有
 * valueLabel；401 映射为会话过期提示，15 秒超时与 JSON 解析失败
 * 有独立错误文案。
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
    const response = await fetch(CROF_USAGE_URL, {
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
          ? 'Session expired — please re-authenticate with CrofAI'
          : `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    const credits = toNumber(payload?.credits);
    const valueLabel = credits !== null ? `$${formatMoney(credits)}` : null;

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
