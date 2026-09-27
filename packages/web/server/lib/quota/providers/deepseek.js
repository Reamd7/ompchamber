/**
 * DeepSeek 配额 provider。
 *
 * 请求 api.deepseek.com 的余额端点，把账户总余额格式化为
 * credits_balance 窗口的 valueLabel（优先 USD、其次 CNY，带对应货币符号）；
 * 不提供百分比用量，仅展示余额。
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
export const providerId = 'deepseek';
/** 展示名。 */
export const providerName = 'DeepSeek';
/** OpenCode auth 文件中的凭据匹配别名。 */
const aliases = ['deepseek'];
/** DeepSeek 余额查询端点。 */
const DEEPSEEK_QUOTA_URL = 'https://api.deepseek.com/user/balance';

/** 读取 auth 文件，存在 key 或 token 即视为已配置。 */
export const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.key || entry?.token);
};

/**
 * 拉取 DeepSeek 余额：从 balance_infos 中优先选 USD 条目、其次 CNY；
 * total_balance 为字符串或数字均可解析，空字符串视为无数据；
 * 401/403 映射为会话过期提示，15 秒超时与 JSON 解析失败有独立错误文案。
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
    const response = await fetch(DEEPSEEK_QUOTA_URL, {
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
        error: response.status === 401 || response.status === 403
          ? 'Session expired — please re-authenticate with DeepSeek'
          : `API error: ${response.status}`
      });
    }

    const payload = await response.json();
    const balanceInfos = Array.isArray(payload?.balance_infos) ? payload.balance_infos : [];
    const balanceInfo = balanceInfos.find((info) => info?.currency === 'USD')
      ?? balanceInfos.find((info) => info?.currency === 'CNY')
      ?? null;
    const rawBalance = balanceInfo?.total_balance;
    const totalBalance = (typeof rawBalance === 'number' || (typeof rawBalance === 'string' && rawBalance.trim() !== ''))
      ? toNumber(rawBalance)
      : null;

    if (totalBalance === null) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: 'No quota data in response'
      });
    }

    const isCny = balanceInfo?.currency === 'CNY';
    const symbol = isCny ? '¥' : '$';
    const valueLabel = `${symbol}${formatMoney(totalBalance)}`;

    const windows = {
      credits_balance: toUsageWindow({
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
    const isTimeout = error instanceof DOMException && (
      error.name === 'TimeoutError' || (error.name === 'AbortError' && timeoutSignal.aborted)
    );
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
