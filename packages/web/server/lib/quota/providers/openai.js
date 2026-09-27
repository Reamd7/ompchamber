/**
 * OpenAI（ChatGPT 后端）配额 provider。
 *
 * 用 OpenCode auth 中的 access token 请求 chatgpt.com 的 wham/usage 端点，
 * 读取 primary/secondary 两个限流窗口并固定映射为 5h 与 weekly 窗口。
 * 注意与 codex.js 共享同一组 auth 别名，但此处窗口命名是固定标签、
 * 且不读取 credits/spend_control。
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

/** 对外 provider 标识（模块内部使用，不导出）。 */
const providerId = 'openai';
/** 展示名（模块内部使用，不导出）。 */
const providerName = 'OpenAI';
/** OpenCode auth 文件中的凭据匹配别名（与 codex.js 共享）。 */
const aliases = ['openai', 'codex', 'chatgpt'];

/** 读取 auth 文件，存在 OAuth access token 或 API token 即视为已配置。 */
const isConfigured = () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  return Boolean(entry?.access || entry?.token);
};

/**
 * 拉取 OpenAI 限流窗口：primary_window 固定映射为 '5h'、
 * secondary_window 固定映射为 'weekly'；reset_at 由秒级时间戳换算为毫秒；
 * 未配置、HTTP 错误或异常均返回结构化失败结果。
 */
export const fetchQuota = async () => {
  const auth = readAuthFile();
  const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
  const accessToken = entry?.access ?? entry?.token;

  if (!accessToken) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  try {
    const response = await fetch('https://chatgpt.com/backend-api/wham/usage', {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${accessToken}`,
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
    const primary = payload?.rate_limit?.primary_window ?? null;
    const secondary = payload?.rate_limit?.secondary_window ?? null;

    const windows = {};
    if (primary) {
      windows['5h'] = toUsageWindow({
        usedPercent: primary.used_percent ?? null,
        windowSeconds: primary.limit_window_seconds ?? null,
        resetAt: primary.reset_at ? primary.reset_at * 1000 : null
      });
    }
    if (secondary) {
      windows['weekly'] = toUsageWindow({
        usedPercent: secondary.used_percent ?? null,
        windowSeconds: secondary.limit_window_seconds ?? null,
        resetAt: secondary.reset_at ? secondary.reset_at * 1000 : null
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
