/**
 * 配额工具 - 类型转换函数：把各 provider 原始响应中类型不定的字段宽容地
 * 归一化为对象/非空字符串/有限数字/毫秒时间戳，以及 Z.ai 限额的窗口
 * 秒数与标签推导。约定：无法归一化时一律返回 null，绝不抛错。
 */
/**
 * 把任意值归一化为对象：非 null 的对象原样返回，其余（含数组以外的基础
 * 类型、null/undefined）返回 null。
 * @param {unknown} value
 * @returns {object|null}
 */
export const asObject = (value) => (value && typeof value === 'object' ? value : null);

/**
 * 归一化为去除首尾空格后的非空字符串；非字符串或空白串返回 null。
 * @param {unknown} value
 * @returns {string|null}
 */
export const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed ? trimmed : null;
};

/**
 * 归一化为有限数字：数字直接透传；字符串经 Number 解析；其余返回 null。
 * NaN/Infinity 一律视为无效。
 * @param {unknown} value
 * @returns {number|null}
 */
export const toNumber = (value) => {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === 'string') {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
};

/**
 * 归一化为毫秒级时间戳：数字按秒级（< 1e12）自动乘 1000；字符串经
 * Date.parse 解析；解析失败或类型不符返回 null。
 * @param {unknown} value
 * @returns {number|null}
 */
export const toTimestamp = (value) => {
  if (!value) return null;
  if (typeof value === 'number') {
    return value < 1_000_000_000_000 ? value * 1000 : value;
  }
  if (typeof value === 'string') {
    const parsed = Date.parse(value);
    return Number.isNaN(parsed) ? null : parsed;
  }
  return null;
};

/**
 * 只处理数字的时间戳归一化：秒级（< 1e12）乘 1000 转毫秒，其余原样
 * 返回；非数字返回 null（不做字符串解析）。
 * @param {unknown} value
 * @returns {number|null}
 */
export const normalizeTimestamp = (value) => {
  if (typeof value !== 'number') return null;
  return value < 1_000_000_000_000 ? value * 1000 : value;
};

/**
 * Z.ai 限额的窗口时长映射：`limit.unit` 是令牌桶档位编号（3 = 1 小时、
 * 6 = 1 天），配合 `limit.number` 倍数得到窗口秒数。
 */
const ZAI_TOKEN_WINDOW_SECONDS = {
  3: 60 * 60,
  6: 7 * 24 * 60 * 60
};

/**
 * 由 Z.ai 的 `{ number, unit }` 限额推导窗口秒数；unit 不在映射表或
 * number 缺失时返回 null。
 * @param {{ number?: number, unit?: number } | null} limit
 * @returns {number|null}
 */
export const resolveWindowSeconds = (limit) => {
  if (!limit || !limit.number) return null;
  const unitSeconds = ZAI_TOKEN_WINDOW_SECONDS[limit.unit];
  if (!unitSeconds) return null;
  return unitSeconds * limit.number;
};

/**
 * 把窗口秒数转为展示标签：整天的按天（7 天特判为 `weekly`），整小时的
 * 按 `Nh`，其余按 `Ns`；空值回退 `'tokens'`。
 * @param {number|null} windowSeconds
 * @returns {string}
 */
export const resolveWindowLabel = (windowSeconds) => {
  if (!windowSeconds) return 'tokens';
  if (windowSeconds % 86400 === 0) {
    const days = windowSeconds / 86400;
    return days === 7 ? 'weekly' : `${days}d`;
  }
  if (windowSeconds % 3600 === 0) {
    return `${windowSeconds / 3600}h`;
  }
  return `${windowSeconds}s`;
};
