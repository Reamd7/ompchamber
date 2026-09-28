/**
 * Claude subscription quota.
 *
 * Reports the plan limits Claude Code itself is bound by (rolling session
 * window, weekly windows, model-scoped weekly windows, and paid extra usage)
 * using the OAuth credential Claude Code already holds.
 *
 * @module quota/providers/claude
 */
/**
 * 中文说明：Claude 订阅配额 provider 的入口。
 * 使用 Claude Code 自带的 OAuth 凭据请求 Anthropic 用量端点，汇报会话
 * 滚动窗口、周窗口、按模型的周窗口与付费超额用量；内置 429 退避冷却、
 * 并发请求合并与按凭据指纹隔离的 "最后一次成功数据" 缓存，
 * 以对抗 Anthropic 的激进限流并避免跨账户串数据。
 */

import { createHash } from 'crypto';

import { buildResult } from '../../utils/index.js';
import { loadClaudeCredential } from './auth.js';
import { toClaudeUsage } from './transforms.js';

/** 对外 provider 标识。 */
export const providerId = 'claude';
/** 展示名。 */
export const providerName = 'Claude';
/** OpenCode auth 文件中的凭据匹配别名。 */
export const aliases = ['anthropic', 'claude'];

/** Anthropic OAuth 用量端点。 */
const USAGE_URL = 'https://api.anthropic.com/api/oauth/usage';
/** OAuth 用量接口要求的 anthropic-beta 版本头。 */
const OAUTH_BETA_HEADER = 'oauth-2025-04-20';
/** 收到 429 且未提供可用 retry-after 时的默认冷却时长（5 分钟）。 */
const DEFAULT_COOLDOWN_MS = 5 * 60 * 1000;
/** 冷却时长上限（1 小时），防止服务端返回超长 retry-after 导致长时间不出数。 */
const MAX_COOLDOWN_MS = 60 * 60 * 1000;

/** 最近一次成功的用量数据缓存（按凭据指纹键控），仅用于在 Anthropic 限流期间继续提供旧值。 */
/**
 * Last good payload, kept only to survive Anthropic's aggressive rate limiting.
 * Keyed by credential fingerprint so a second account never sees the first
 * account's numbers.
 *
 * @type {{ fingerprint: string, usage: object, planLabel: string|null }|null}
 */
let cachedUsage = null;
/** 429 退避冷却的截止时间戳（毫秒）；0 表示当前不在冷却期。 */
let cooldownUntil = 0;
/** 进行中的配额请求 Promise，用于合并并发刷新；空闲时为 null。 */
let pendingFetch = null;

/**
 * 计算凭据的 sha256 指纹（access token 与 refresh token 以 \0 分隔拼接）。
 * 用于在账户切换时识别缓存数据是否仍属于当前账户。
 * @param {{accessToken: string, refreshToken?: string}} credential Claude OAuth 凭据
 * @returns {string} 十六进制指纹
 */
const fingerprintOf = (credential) =>
  createHash('sha256').update(`${credential.accessToken}\0${credential.refreshToken ?? ''}`).digest('hex');

/**
 * 从 429 响应的 retry-after 头推算冷却时长：支持秒数与 HTTP 日期两种
 * 格式，解析失败或缺失时退回默认 5 分钟；统一钳制在 1 小时上限内。
 * @param {Response} response 被 429 拒绝的响应
 * @returns {number} 冷却毫秒数
 */
const cooldownFromHeader = (response) => {
  const raw = response.headers.get('retry-after');
  const retryAfter = Number(raw);
  if (Number.isFinite(retryAfter) && retryAfter > 0) {
    return Math.min(retryAfter * 1000, MAX_COOLDOWN_MS);
  }
  const retryAt = raw ? Date.parse(raw) : Number.NaN;
  if (Number.isFinite(retryAt) && retryAt > Date.now()) {
    return Math.min(retryAt - Date.now(), MAX_COOLDOWN_MS);
  }
  return DEFAULT_COOLDOWN_MS;
};

/**
 * 用缓存数据构造成功结果；缓存为空或指纹不匹配（账户已切换）时
 * 返回 null，由调用方决定是否报限流错误。
 * @param {string} fingerprint 当前凭据指纹
 * @param {string|null} planLabel 当前凭据的套餐标签（优先于缓存中保存的值）
 * @returns {object|null}
 */
const cachedResultFor = (fingerprint, planLabel) => {
  if (!cachedUsage || cachedUsage.fingerprint !== fingerprint) return null;
  return buildResult({
    providerId,
    providerName,
    ok: true,
    configured: true,
    usage: cachedUsage.usage,
    planLabel: planLabel ?? cachedUsage.planLabel
  });
};

/**
 * 构造失败结果的快捷方式。
 * @param {string} error 错误文案
 * @param {{configured?: boolean}} [options] configured 默认 true（凭据在但请求失败）
 * @returns {object} buildResult 形状的失败结果
 */
const failure = (error, { configured = true } = {}) =>
  buildResult({ providerId, providerName, ok: false, configured, error });

/** 能从任一凭据源（keychain 等）加载出 Claude OAuth 凭据即视为已配置。 */
export const isConfigured = () => Boolean(loadClaudeCredential());

/**
 * 真正执行配额请求（未做并发合并的版本）。
 * 流程：未配置直接失败；账户切换时清空缓存与冷却；冷却期内直接回放
 * 缓存（无缓存则报限流错误，不再请求）；请求 429 时按 retry-after 进入
 * 冷却并回放缓存；401/403 提示重新登录；成功时经 toClaudeUsage 转换
 * 窗口并更新缓存。
 * @returns {Promise<object>} 统一的配额结果对象
 */
const fetchQuotaUncoalesced = async () => {
  const credential = loadClaudeCredential();
  if (!credential) {
    return failure('Not configured', { configured: false });
  }

  const fingerprint = fingerprintOf(credential);
  if (cachedUsage && cachedUsage.fingerprint !== fingerprint) {
    cachedUsage = null;
    cooldownUntil = 0;
  }

  if (Date.now() < cooldownUntil) {
    return cachedResultFor(fingerprint, credential.planLabel)
      ?? failure('Rate limited. Retrying soon.');
  }

  let response;
  try {
    response = await fetch(USAGE_URL, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${credential.accessToken}`,
        'anthropic-beta': OAUTH_BETA_HEADER
      }
    });
  } catch (error) {
    return failure(error instanceof Error ? error.message : 'Request failed');
  }

  if (response.status === 429) {
    cooldownUntil = Date.now() + cooldownFromHeader(response);
    return cachedResultFor(fingerprint, credential.planLabel)
      ?? failure('Rate limited. Retrying soon.');
  }

  if (response.status === 401 || response.status === 403) {
    return failure('Claude session expired. Open Claude Code to sign in again.');
  }

  if (!response.ok) {
    return failure(`API error: ${response.status}`);
  }

  let payload;
  try {
    payload = await response.json();
  } catch {
    return failure('Unexpected response from Anthropic');
  }

  const { windows, models } = toClaudeUsage(payload);
  const usage = Object.keys(models).length > 0 ? { windows, models } : { windows };
  cachedUsage = { fingerprint, usage, planLabel: credential.planLabel };

  return buildResult({
    providerId,
    providerName,
    ok: true,
    configured: true,
    usage,
    planLabel: credential.planLabel
  });
};

/**
 * 拉取 Claude 配额（带并发合并）：进行中的请求直接复用同一个 Promise，
 * 完成后清空 pendingFetch，以便下一次刷新发起新请求。
 * @returns {Promise<object>} 统一的配额结果对象
 */
export const fetchQuota = () => {
  if (pendingFetch) return pendingFetch;

  const request = fetchQuotaUncoalesced();
  const pending = request.finally(() => {
    if (pendingFetch === pending) pendingFetch = null;
  });
  pendingFetch = pending;
  return pendingFetch;
};

/** 清空用量缓存、冷却截止与进行中请求，供测试在用例间复位模块状态。 */
/** Test seam: clears the rate-limit cache between cases. */
export const resetClaudeQuotaCache = () => {
  cachedUsage = null;
  cooldownUntil = 0;
  pendingFetch = null;
};
