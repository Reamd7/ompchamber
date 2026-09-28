/**
 * Google Provider - Transforms
 *
 * Data transformation functions for Google quota responses.
 * @module quota/providers/google/transforms
 */
/**
 * Google 配额响应的数据转换：解析复合 refresh token、为不同来源
 * （gemini / antigravity）解析窗口语义，并把 quota bucket / 模型数据
 * 整形为统一的 `{ "<source>/<model>": { windows } }` 结构。所有输入均按
 * “缺字段即跳过”处理，不抛错。
 */

import {
  asNonEmptyString,
  toNumber,
  toTimestamp,
  toUsageWindow
} from '../../utils/index.js';

/** antigravity 5 小时窗口的时长（秒）。 */
const GOOGLE_FIVE_HOUR_WINDOW_SECONDS = 5 * 60 * 60;
/** gemini 等来源的每日窗口时长（秒）。 */
const GOOGLE_DAILY_WINDOW_SECONDS = 24 * 60 * 60;

/**
 * 解析 Google 复合 refresh token。OpenCode 侧把 token 存成
 * `token|projectId|managedProjectId` 的管道分隔格式；本函数拆出三段并
 * 逐段做空值归一化（空段 -> null）。
 * @param {unknown} rawRefreshToken 原始 refresh 字符串
 * @returns {{ refreshToken: string|null, projectId: string|null, managedProjectId: string|null }}
 *   非字符串或空串时三者均为 null
 */
export const parseGoogleRefreshToken = (rawRefreshToken) => {
  const refreshToken = asNonEmptyString(rawRefreshToken);
  if (!refreshToken) {
    return { refreshToken: null, projectId: null, managedProjectId: null };
  }

  const [rawToken = '', rawProject = '', rawManagedProject = ''] = refreshToken.split('|');
  return {
    refreshToken: asNonEmptyString(rawToken),
    projectId: asNonEmptyString(rawProject),
    managedProjectId: asNonEmptyString(rawManagedProject)
  };
};

/**
 * 按来源推断配额窗口的标签与时长：gemini 是每日窗口；antigravity 需要依据
 * 距重置的剩余时间区分——超过 10 小时按每日窗口计，否则按 5 小时窗口计
 * （无重置时间时保守按每日处理）。未知来源同样按每日处理。
 * @param {string} sourceId 来源标识（'gemini' | 'antigravity'）
 * @param {number|null} resetAt 重置时间戳（毫秒），可空
 * @returns {{ label: string, seconds: number }} 窗口键名与窗口秒数
 */
const resolveGoogleWindow = (sourceId, resetAt) => {
  if (sourceId === 'gemini') {
    return { label: 'daily', seconds: GOOGLE_DAILY_WINDOW_SECONDS };
  }

  if (sourceId === 'antigravity') {
    const remainingSeconds = typeof resetAt === 'number'
      ? Math.max(0, Math.round((resetAt - Date.now()) / 1000))
      : null;

    if (remainingSeconds !== null && remainingSeconds > 10 * 60 * 60) {
      return { label: 'daily', seconds: GOOGLE_DAILY_WINDOW_SECONDS };
    }

    return { label: '5h', seconds: GOOGLE_FIVE_HOUR_WINDOW_SECONDS };
  }

  return { label: 'daily', seconds: GOOGLE_DAILY_WINDOW_SECONDS };
};

/**
 * 把 retrieveUserQuota 返回的单个 quota bucket 转成模型条目。模型名会补上
 * `<sourceId>/` 前缀避免跨来源重名；remainingFraction（0~1）换算为
 * usedPercent = 100 - 剩余百分比（下限 0）。
 * @param {object} bucket 原始 bucket（需含 modelId，否则返回 null）
 * @param {string} sourceId 来源标识
 * @returns {{ [scopedName: string]: { windows: Record<string, object> } } | null}
 */
export const transformQuotaBucket = (bucket, sourceId) => {
  const modelId = asNonEmptyString(bucket?.modelId);
  if (!modelId) {
    return null;
  }

  const scopedName = modelId.startsWith(`${sourceId}/`)
    ? modelId
    : `${sourceId}/${modelId}`;

  const remainingFraction = toNumber(bucket?.remainingFraction);
  const remainingPercent = remainingFraction !== null
    ? Math.round(remainingFraction * 100)
    : null;
  const usedPercent = remainingPercent !== null ? Math.max(0, 100 - remainingPercent) : null;
  const resetAt = toTimestamp(bucket?.resetTime);
  const window = resolveGoogleWindow(sourceId, resetAt);

  return {
    [scopedName]: {
      windows: {
        [window.label]: toUsageWindow({
          usedPercent,
          windowSeconds: window.seconds,
          resetAt
        })
      }
    }
  };
};

/**
 * 把 fetchAvailableModels 返回的单个模型条目（含 quotaInfo.remainingFraction /
 * resetTime）转成与 transformQuotaBucket 相同的模型条目结构；resetTime 经
 * Date.parse 归一化为毫秒时间戳。
 * @param {string} modelName 模型名（可能已带 sourceId 前缀）
 * @param {object} modelData 原始模型数据
 * @param {string} sourceId 来源标识
 * @returns {{ [scopedName: string]: { windows: Record<string, object> } }}
 */
export const transformModelData = (modelName, modelData, sourceId) => {
  const scopedName = modelName.startsWith(`${sourceId}/`)
    ? modelName
    : `${sourceId}/${modelName}`;

  const remainingFraction = modelData?.quotaInfo?.remainingFraction;
  const remainingPercent = typeof remainingFraction === 'number'
    ? Math.round(remainingFraction * 100)
    : null;
  const usedPercent = remainingPercent !== null ? Math.max(0, 100 - remainingPercent) : null;
  const resetAt = modelData?.quotaInfo?.resetTime
    ? new Date(modelData.quotaInfo.resetTime).getTime()
    : null;
  const window = resolveGoogleWindow(sourceId, resetAt);

  return {
    [scopedName]: {
      windows: {
        [window.label]: toUsageWindow({
          usedPercent,
          windowSeconds: window.seconds,
          resetAt
        })
      }
    }
  };
};
