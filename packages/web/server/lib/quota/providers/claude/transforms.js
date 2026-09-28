/**
 * Shapes Anthropic's OAuth usage payload into quota windows.
 *
 * The payload carries the same limit twice: a legacy set of named fields
 * (`five_hour`, `seven_day`, ...) and a newer `limits` array. Only the array
 * reports model-scoped limits, and Anthropic ships new limit kinds there under
 * rotating internal code names, so limits are read from the array by `kind` and
 * fall back to the legacy fields when the array is missing.
 *
 * @module quota/providers/claude/transforms
 */
/**
 * 将 Anthropic OAuth 用量接口的响应负载转换为统一的配额窗口结构。
 *
 * 负载中新旧两套字段并存：旧版具名字段（`five_hour`、`seven_day` 等）与
 * 新版 `limits` 数组。只有数组能表达模型级（model-scoped）限额，且
 * Anthropic 会以轮换的内部代号在其中新增限额种类，因此优先按 `kind`
 * 读取数组；数组缺失时回退到旧版具名字段。任何畸形输入都归一化为
 * `{ windows: {}, models: {} }`，绝不上抛异常。
 */

import { asObject, asNonEmptyString, toNumber, toTimestamp, toUsageWindow, formatMoney } from '../../utils/index.js';

/** 会话（5 小时）滚动窗口在结果 `windows` 中的键名。 */
const SESSION_WINDOW = '5h';
/** 周（7 天）滚动窗口在结果 `windows` 中的键名。 */
const WEEKLY_WINDOW = '7d';
/** 额外用量（付费溢出额度）窗口的键名；本质是月度消费上限而非滚动窗口。 */
const EXTRA_USAGE_WINDOW = 'extra_usage';

// Consumers rank limits by how soon they run out, so each window carries its
// duration. Extra usage is a monthly spend cap, not a rolling window.
/** 5 小时窗口的时长（秒），供消费方按“最快耗尽”排序。 */
const SESSION_WINDOW_SECONDS = 5 * 60 * 60;
/** 7 天窗口的时长（秒）。 */
const WEEKLY_WINDOW_SECONDS = 7 * 24 * 60 * 60;

/** 把任意值宽容地当作数组处理：非数组返回空数组，避免上层逐一判型。 */
const asArray = (value) => (Array.isArray(value) ? value : []);

/**
 * 解析 Anthropic 的“最小货币单位”表示（如 `{ amount_minor: 10000, exponent: 2 }`）。
 * @param {unknown} value 原始金额对象
 * @returns {number|null} 换算后的主单位金额；缺少 `amount_minor` 时返回 null
 */
/** Money in Anthropic's minor-unit form, e.g. `{ amount_minor: 10000, exponent: 2 }`. */
const toAmount = (value) => {
  const money = asObject(value);
  const minor = toNumber(money?.amount_minor);
  if (minor === null) return null;
  const exponent = toNumber(money?.exponent) ?? 2;
  return minor / 10 ** exponent;
};

/**
 * 把已用/上限金额格式化为展示标签，如 `$2.50 / $100.00`。
 * @param {number|null} used 已用金额（主单位）
 * @param {number|null} limit 上限金额（主单位），缺失时只显示已用部分
 * @param {string|null} currency 货币代码；USD（或缺失）用 `$` 前缀，其余用 `代码 + 空格`
 * @returns {string|null} 已用金额无法格式化时返回 null（整个窗口随之省略）
 */
const formatSpendLabel = (used, limit, currency) => {
  const usedLabel = formatMoney(used);
  if (usedLabel === null) return null;
  const prefix = currency === 'USD' || !currency ? '$' : `${currency} `;
  const limitLabel = formatMoney(limit);
  return limitLabel === null ? `${prefix}${usedLabel}` : `${prefix}${usedLabel} / ${prefix}${limitLabel}`;
};

/**
 * 向目标对象写入一个用量窗口；percent 与 valueLabel 均缺失时静默跳过，
 * 保证不会产出永远为空的进度条。
 * @param {Record<string, object>} target 承载窗口的对象（账号级 `windows` 或模型级窗口表）
 * @param {string} key 窗口键名（如 `'5h'`、`'extra_usage'`）
 * @param {{ percent: number|null, resetAt: number|null, valueLabel?: string|null, windowSeconds?: number|null }} fields
 */
const addWindow = (target, key, { percent, resetAt, valueLabel, windowSeconds = null }) => {
  if (percent === null && !valueLabel) return;
  target[key] = toUsageWindow({ usedPercent: percent, windowSeconds, resetAt, valueLabel });
};

/**
 * 消费新版 `limits` 数组：按 `kind` 分发——`session` 写入 5h 窗口，
 * `weekly_all` 写入 7d 窗口，`weekly_scoped` 按 `scope.model.display_name`
 * 写入对应模型的 7d 窗口。未知 kind 一律忽略，不为新限额种类臆造窗口。
 * @param {unknown[]} limits 原始 limits 数组
 * @param {Record<string, object>} windows 输出：账号级窗口表（就地修改）
 * @param {Record<string, object>} models 输出：模型名到 `{ windows }` 的映射（就地修改）
 */
const applyLimitsArray = (limits, windows, models) => {
  for (const entry of limits) {
    const limit = asObject(entry);
    if (!limit) continue;
    const percent = toNumber(limit.percent);
    const resetAt = toTimestamp(limit.resets_at);
    const modelName = asNonEmptyString(asObject(asObject(limit.scope)?.model)?.display_name);

    if (limit.kind === 'session') {
      addWindow(windows, SESSION_WINDOW, { percent, resetAt, windowSeconds: SESSION_WINDOW_SECONDS });
      continue;
    }
    if (limit.kind === 'weekly_all') {
      addWindow(windows, WEEKLY_WINDOW, { percent, resetAt, windowSeconds: WEEKLY_WINDOW_SECONDS });
      continue;
    }
    if (limit.kind === 'weekly_scoped' && modelName) {
      const modelWindows = {};
      addWindow(modelWindows, WEEKLY_WINDOW, { percent, resetAt, windowSeconds: WEEKLY_WINDOW_SECONDS });
      if (Object.keys(modelWindows).length > 0) models[modelName] = { windows: modelWindows };
    }
  }
};

/**
 * 旧版兜底：limits 数组缺失时从具名字段 `five_hour` / `seven_day` 读取
 * `utilization` 与 `resets_at`，写入 5h / 7d 窗口。此路径没有模型级信息。
 * @param {object} payload 原始负载
 * @param {Record<string, object>} windows 输出：账号级窗口表（就地修改）
 */
const applyLegacyFields = (payload, windows) => {
  const fiveHour = asObject(payload.five_hour);
  const sevenDay = asObject(payload.seven_day);
  if (fiveHour) {
    addWindow(windows, SESSION_WINDOW, {
      percent: toNumber(fiveHour.utilization),
      resetAt: toTimestamp(fiveHour.resets_at),
      windowSeconds: SESSION_WINDOW_SECONDS
    });
  }
  if (sevenDay) {
    addWindow(windows, WEEKLY_WINDOW, {
      percent: toNumber(sevenDay.utilization),
      resetAt: toTimestamp(sevenDay.resets_at),
      windowSeconds: WEEKLY_WINDOW_SECONDS
    });
  }
};

/**
 * 额外用量（extra usage）是超出套餐限额后的付费溢出部分。只有账号启用后
 * 才有意义；未启用时该区块会渲染成一条永远为空的进度条，因此直接省略。
 */
/**
 * Extra usage is the paid overflow beyond plan limits. It is only meaningful
 * once the account has it enabled; a disabled block would render as a permanent
 * empty bar.
 */
const applyExtraUsage = (payload, windows) => {
  const spend = asObject(payload.spend);
  if (!spend || spend.enabled !== true) return;
  const used = toAmount(spend.used);
  const limit = toAmount(spend.limit);
  addWindow(windows, EXTRA_USAGE_WINDOW, {
    percent: toNumber(spend.percent),
    resetAt: null,
    valueLabel: formatSpendLabel(used, limit, asNonEmptyString(asObject(spend.used)?.currency))
  });
};

/**
 * 入口转换函数：把 OAuth usage 端点的 JSON 负载整形为统一用量结构。
 * 畸形负载（null、非对象、limits 非数组）不抛错，返回空结构。
 * @param {unknown} rawPayload OAuth usage 端点解析后的 JSON 负载
 * @returns {{ windows: Record<string, object>, models: Record<string, object> }}
 *   windows：账号级窗口（'5h'/'7d'/'extra_usage'）；models：模型名 -> { windows }
 */
/**
 * @param {unknown} rawPayload Parsed JSON body of the OAuth usage endpoint.
 * @returns {{ windows: Record<string, object>, models: Record<string, object> }}
 */
export const toClaudeUsage = (rawPayload) => {
  const payload = asObject(rawPayload) ?? {};
  const windows = {};
  const models = {};

  const limits = asArray(payload.limits);
  if (limits.length > 0) {
    applyLimitsArray(limits, windows, models);
  } else {
    applyLegacyFields(payload, windows);
  }

  applyExtraUsage(payload, windows);

  return { windows, models };
};
