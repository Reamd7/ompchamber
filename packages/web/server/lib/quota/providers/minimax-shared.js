/**
 * MiniMax Coding Plan 配额 provider 的共享实现（工厂）。
 *
 * 国际站 minimax-coding-plan 与国内站 minimax-cn-coding-plan 的配额协议
 * 完全一致、仅 API 域名不同，因此由 createMiniMaxCodingPlanProvider 统一
 * 承担凭据读取、双端点回退（token 套餐优先、coding 套餐兜底）、用量百分比
 * 与窗口时长的计算；两个站点模块只传入各自的标识、别名与端点 URL。
 */
import { readAuthFile } from '../../opencode/auth.js';
import {
  getAuthEntry,
  normalizeAuthEntry,
  buildResult,
  toUsageWindow,
  toNumber,
  toTimestamp,
} from '../utils/index.js';

/** MiniMax 窗口状态码 3：该窗口对当前套餐档位不适用（如无周限制的老套餐）。 */
// Status 3 indicates the window is not applicable for the current plan tier.
const WINDOW_STATUS_INACTIVE = 3;

/** 通用文本类套餐在 model_remains 中的 model_name 取值（小写匹配）。 */
const TEXT_MODELS = ['general', 'chat', 'text'];

/**
 * 从 model_remains 数组中挑选最能代表整体套餐用量的模型条目。
 * 优先级：1) 有剩余额度且名为 minimax-m 开头的模型；2) 名为
 * general/chat/text 的通用条目；3) 带剩余百分比的条目；4) 兜底返回首条。
 * @param {Array<object|null>} modelRemains API 返回的各模型剩余量列表
 * @returns {object|null} 选中的模型条目；数组为空或非数组时返回 null
 */
const pickChatModel = (modelRemains) => {
  if (!Array.isArray(modelRemains) || modelRemains.length === 0) return null;

  const m3Candidate = modelRemains.find(
    (m) => m?.model_name && /^minimax-m/i.test(m.model_name) && toNumber(m.current_interval_total_count) > 0
  );
  if (m3Candidate) return m3Candidate;

  const textCandidate = modelRemains.find(
    (m) => m?.model_name && TEXT_MODELS.includes(m.model_name.toLowerCase())
  );
  if (textCandidate) return textCandidate;

  const percentCandidate = modelRemains.find(
    (m) => typeof m?.current_interval_remaining_percent === 'number'
  );
  if (percentCandidate) return percentCandidate;

  return modelRemains[0];
};

/**
 * 判断响应是否携带可用的配额数据：base_resp.status_code 非 0 视为业务失败，
 * model_remains 必须是非空数组。
 * @param {object|null} payload 接口返回的 JSON 载荷
 * @returns {boolean}
 */
const isUsablePayload = (payload) => {
  const baseResp = payload?.base_resp;
  if (baseResp && baseResp.status_code !== 0) return false;
  const rems = payload?.model_remains;
  return Array.isArray(rems) && rems.length > 0;
};

/**
 * 以 bearer token 请求 MiniMax 配额端点；网络异常、非 2xx 或载荷不可用
 * 统一归一化为 null，由调用方决定回退策略。
 * @param {string} url 配额端点地址
 * @param {string} apiKey API 密钥
 * @returns {Promise<object|null>} 可用的配额载荷；任何失败返回 null
 */
const fetchEndpoint = async (url, apiKey) => {
  try {
    const response = await fetch(url, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${apiKey}`,
        'Content-Type': 'application/json',
      },
    });
    if (!response.ok) return null;
    const payload = await response.json();
    if (!isUsablePayload(payload)) return null;
    return payload;
  } catch {
    return null;
  }
};

/**
 * 把任意值安全转换为 [0,100] 区间的百分比数字；无法转换时返回 null。
 * @param {*} value 待转换的值
 * @returns {number|null}
 */
const coercePercent = (value) => {
  const n = toNumber(value);
  return n !== null ? Math.max(0, Math.min(100, n)) : null;
};

/**
 * Check if a window (interval or weekly) is active for the current plan.
 * Status 3 means the window is not applicable (e.g. legacy plans without weekly limits).
 * When the status field is absent, default to active.
 */
/**
 * 判断窗口（interval 或 weekly）对当前套餐是否生效：
 * status 等于 3 表示该窗口不适用（如无周限制的老套餐），
 * 字段缺失时默认视为生效。
 */
const isWindowActive = (status) => {
  const n = toNumber(status);
  return n === null || n !== WINDOW_STATUS_INACTIVE;
};

/**
 * Calculate window duration in seconds from API timestamps or remains_time.
 * MiniMax API returns remains_time in milliseconds (confirmed via live API testing:
 * 9664502 ms = 2.68h in a 5h window, consistent with remaining_percent).
 */
/**
 * 计算窗口总时长（秒）：优先用 resetAt - startAt（毫秒时间戳差），
 * 否则退回 remains_time（毫秒）；两者都不可用时返回 null。
 */
const calculateWindowSeconds = (startAt, resetAt, remainsTimeMs) => {
  if (startAt && resetAt && resetAt > startAt) {
    return Math.floor((resetAt - startAt) / 1000);
  }
  if (remainsTimeMs && remainsTimeMs > 0) {
    return Math.floor(remainsTimeMs / 1000);
  }
  return null;
};

/**
 * 从单个模型条目计算 interval（5h）与 weekly 两个窗口的用量指标。
 * usedPercent 优先由 API 的剩余百分比反推；缺失时用 counts 计算，
 * token 套餐的字段语义反转（usage 字段实际表示剩余量，需用 total - usage）。
 * @param {object} model pickChatModel 选中的模型条目
 * @param {boolean} isTokenPlan 是否命中 token 套餐端点
 * @returns {{intervalUsedPercent:number|null, intervalWindowSeconds:number|null, intervalResetAt:number|null, weeklyUsedPercent:number|null, weeklyWindowSeconds:number|null, weeklyResetAt:number|null}}
 */
const calculateUsage = (model, isTokenPlan) => {
  const intervalTotal = toNumber(model.current_interval_total_count);
  const intervalUsageRaw = toNumber(model.current_interval_usage_count);
  const intervalStartAt = toTimestamp(model.start_time);
  const intervalResetAt = toTimestamp(model.end_time);
  const intervalRemainsTime = toNumber(model.remains_time);
  const intervalRemainingPercent = coercePercent(model.current_interval_remaining_percent);

  const weeklyTotal = toNumber(model.current_weekly_total_count);
  const weeklyUsageRaw = toNumber(model.current_weekly_usage_count);
  const weeklyStartAt = toTimestamp(model.weekly_start_time);
  const weeklyResetAt = toTimestamp(model.weekly_end_time);
  const weeklyRemainsTime = toNumber(model.weekly_remains_time);
  const weeklyRemainingPercent = coercePercent(model.current_weekly_remaining_percent);

  let intervalUsedPercent = null;
  if (intervalRemainingPercent !== null) {
    intervalUsedPercent = 100 - intervalRemainingPercent;
  } else if (intervalTotal > 0 && intervalUsageRaw !== null) {
    const intervalUsed = isTokenPlan
      ? Math.max(0, intervalTotal - intervalUsageRaw)
      : intervalUsageRaw;
    intervalUsedPercent = Math.max(0, Math.min(100, (intervalUsed / intervalTotal) * 100));
  }

  let weeklyUsedPercent = null;
  if (weeklyRemainingPercent !== null) {
    weeklyUsedPercent = 100 - weeklyRemainingPercent;
  } else if (weeklyTotal > 0 && weeklyUsageRaw !== null) {
    const weeklyUsed = isTokenPlan
      ? Math.max(0, weeklyTotal - weeklyUsageRaw)
      : weeklyUsageRaw;
    weeklyUsedPercent = Math.max(0, Math.min(100, (weeklyUsed / weeklyTotal) * 100));
  }

  const intervalWindowSeconds = calculateWindowSeconds(intervalStartAt, intervalResetAt, intervalRemainsTime);
  const weeklyWindowSeconds = calculateWindowSeconds(weeklyStartAt, weeklyResetAt, weeklyRemainsTime);

  return {
    intervalUsedPercent,
    intervalWindowSeconds,
    intervalResetAt,
    weeklyUsedPercent,
    weeklyWindowSeconds,
    weeklyResetAt,
  };
};

/**
 * 创建一个 MiniMax Coding Plan 配额 provider（站点无关的工厂）。
 * @param {object} options
 * @param {string} options.providerId 对外 provider 标识
 * @param {string} options.providerName 展示名（含站点域名）
 * @param {string[]} options.aliases OpenCode auth 文件中的凭据匹配别名
 * @param {string} options.tokenPlanUrl token 套餐剩余量端点（优先请求）
 * @param {string} options.codingPlanUrl coding 套餐剩余量端点（兜底）
 * @returns {{providerId:string, providerName:string, aliases:string[], isConfigured:Function, fetchQuota:Function}}
 */
export const createMiniMaxCodingPlanProvider = ({ providerId, providerName, aliases, tokenPlanUrl, codingPlanUrl }) => {
  /** 读取 OpenCode auth 文件，存在 key 或 token 即视为已配置。 */
  const isConfigured = () => {
    const auth = readAuthFile();
    const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
    return Boolean(entry?.key || entry?.token);
  };

  /**
   * 拉取配额：先请求 token 套餐端点，失败再回退 coding 套餐端点；
   * 组装 5h 窗口与（套餐支持时的）weekly 窗口。未配置、两个端点均无
   * 可用数据、无模型条目或请求异常时，分别返回带对应错误信息的
   * ok:false 结构化结果。
   */
  const fetchQuota = async () => {
    const auth = readAuthFile();
    const entry = normalizeAuthEntry(getAuthEntry(auth, aliases));
    const apiKey = entry?.key ?? entry?.token;

    if (!apiKey) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: false,
        error: 'Not configured',
      });
    }

    try {
      let payload = await fetchEndpoint(tokenPlanUrl, apiKey);
      let isTokenPlan = true;

      if (!payload) {
        payload = await fetchEndpoint(codingPlanUrl, apiKey);
        isTokenPlan = false;
      }

      if (!payload) {
        return buildResult({
          providerId,
          providerName,
          ok: false,
          configured: true,
          error: 'API returned no usable quota data',
        });
      }

      const model = pickChatModel(payload.model_remains);
      if (!model) {
        return buildResult({
          providerId,
          providerName,
          ok: false,
          configured: true,
          error: 'No model quota data available',
        });
      }

      const {
        intervalUsedPercent,
        intervalWindowSeconds,
        intervalResetAt,
        weeklyUsedPercent,
        weeklyWindowSeconds,
        weeklyResetAt,
      } = calculateUsage(model, isTokenPlan);

      const windows = {
        '5h': toUsageWindow({
          usedPercent: intervalUsedPercent,
          windowSeconds: intervalWindowSeconds,
          resetAt: intervalResetAt,
        }),
      };

      // Only include the weekly window when the plan tier supports it.
      // Status 3 = not applicable (e.g. legacy Coding Plan without weekly limits).
      const weeklyActive = isWindowActive(model.current_weekly_status);
      const hasWeeklyData =
        weeklyActive &&
        (coercePercent(model.current_weekly_remaining_percent) !== null ||
          toNumber(model.current_weekly_total_count) > 0);

      if (hasWeeklyData) {
        windows.weekly = toUsageWindow({
          usedPercent: weeklyUsedPercent,
          windowSeconds: weeklyWindowSeconds,
          resetAt: weeklyResetAt,
        });
      }

      return buildResult({
        providerId,
        providerName,
        ok: true,
        configured: true,
        usage: { windows },
      });
    } catch (error) {
      return buildResult({
        providerId,
        providerName,
        ok: false,
        configured: true,
        error: error instanceof Error ? error.message : 'Request failed',
      });
    }
  };

  return {
    providerId,
    providerName,
    aliases,
    isConfigured,
    fetchQuota,
  };
};
