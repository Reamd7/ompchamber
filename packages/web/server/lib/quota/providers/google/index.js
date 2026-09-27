/**
 * Google 配额提供方入口：组装鉴权来源解析（auth.js）、API 调用（api.js）
 * 与响应转换（transforms.js），对每个可用来源刷新 access token、抓取
 * quota buckets 与模型列表，合并成统一的 provider 结果。账号级 windows
 * 恒为空——Google 的限额全部落在具体模型上。
 */
import { buildResult } from '../../utils/index.js';
import {
  resolveGoogleAuthSources,
  resolveGoogleOAuthClient,
  DEFAULT_PROJECT_ID
} from './auth.js';
import { transformQuotaBucket, transformModelData } from './transforms.js';
import {
  refreshGoogleAccessToken,
  fetchGoogleQuotaBuckets,
  fetchGoogleModels
} from './api.js';

// 透传鉴权来源解析，供配额注册表探测 provider 是否已配置。
export { resolveGoogleAuthSources } from './auth.js';

/** provider 的稳定标识（配额注册表键）。 */
export const providerId = 'google';
/** provider 的展示名称。 */
export const providerName = 'Google';
/** 该 provider 在 OpenCode auth.json 中匹配的条目别名。 */
export const aliases = ['google', 'google.oauth'];

/**
 * 是否已配置：存在任一可用鉴权来源（Gemini CLI 或 Antigravity）即视为已配置。
 * @returns {boolean}
 */
export const isConfigured = () => resolveGoogleAuthSources().length > 0;

/**
 * 抓取 Google 配额。逐个来源处理：access token 缺失或过期时先用该来源的
 * OAuth client 刷新；gemini 来源额外拉取 retrieveUserQuota 的 buckets，
 * 所有来源都拉取 fetchAvailableModels；模型结果按 `<source>/<model>` 合并。
 * 单个来源失败记入 sourceErrors 并继续；全部来源都没有产出任何模型时
 * 整体判为失败并返回首个来源错误，否则成功返回合并后的 models。
 * @returns {Promise<object>} buildResult 结构的 provider 结果
 */
export const fetchGoogleQuota = async () => {
  const authSources = resolveGoogleAuthSources();
  if (!authSources.length) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: false,
      error: 'Not configured'
    });
  }

  const models = {};
  const sourceErrors = [];

  for (const source of authSources) {
    const now = Date.now();
    let accessToken = source.accessToken;

    if (!accessToken || (typeof source.expires === 'number' && source.expires <= now)) {
      if (!source.refreshToken) {
        sourceErrors.push(`${source.sourceLabel}: Missing refresh token`);
        continue;
      }
      const { clientId, clientSecret } = resolveGoogleOAuthClient(source.sourceId);
      accessToken = await refreshGoogleAccessToken(source.refreshToken, clientId, clientSecret);
    }

    if (!accessToken) {
      sourceErrors.push(`${source.sourceLabel}: Failed to refresh OAuth token`);
      continue;
    }

    const projectId = source.projectId ?? DEFAULT_PROJECT_ID;
    let mergedAnyModel = false;

    if (source.sourceId === 'gemini') {
      const quotaPayload = await fetchGoogleQuotaBuckets(accessToken, projectId);
      const buckets = Array.isArray(quotaPayload?.buckets) ? quotaPayload.buckets : [];

      for (const bucket of buckets) {
        const transformed = transformQuotaBucket(bucket, source.sourceId);
        if (transformed) {
          Object.assign(models, transformed);
          mergedAnyModel = true;
        }
      }
    }

    const payload = await fetchGoogleModels(accessToken, projectId);
    if (payload) {
      for (const [modelName, modelData] of Object.entries(payload.models ?? {})) {
        const transformed = transformModelData(modelName, modelData, source.sourceId);
        Object.assign(models, transformed);
        mergedAnyModel = true;
      }
    }

    if (!mergedAnyModel) {
      sourceErrors.push(`${source.sourceLabel}: Failed to fetch models`);
    }
  }

  if (!Object.keys(models).length) {
    return buildResult({
      providerId,
      providerName,
      ok: false,
      configured: true,
      error: sourceErrors[0] ?? 'Failed to fetch models'
    });
  }

  return buildResult({
    providerId,
    providerName,
    ok: true,
    configured: true,
    usage: {
      windows: {},
      models: Object.keys(models).length ? models : undefined
    }
  });
};
