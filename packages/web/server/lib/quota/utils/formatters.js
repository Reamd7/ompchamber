/**
 * 配额工具 - 格式化函数：重置时间的本地化展示、剩余秒数计算、统一用量
 * 窗口（usage window）对象的构造，以及 provider 结果与时长单位的换算。
 * 全部为纯函数，输入非法时返回 null 而不是抛错。
 */
/**
 * 把重置时间戳格式化为本地化字符串：今天只显示时刻（如 `下午3:05`），
 * 其它日期显示“月 日 星期 时刻”。无效时间戳返回 null。
 * @param {number|string} timestamp 可被 Date 解析的时间戳
 * @returns {string|null}
 */
export const formatResetTime = (timestamp) => {
  try {
    const resetDate = new Date(timestamp);
    if (!Number.isFinite(resetDate.getTime())) {
      return null;
    }

    const now = new Date();
    const isToday = resetDate.toDateString() === now.toDateString();
    
    if (isToday) {
      return resetDate.toLocaleTimeString(undefined, {
        hour: 'numeric',
        minute: '2-digit'
      });
    }
    
    return resetDate.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      weekday: 'short',
      hour: 'numeric',
      minute: '2-digit'
    });
  } catch {
    return null;
  }
};

/** 判断重置时间戳是否存在（非 null/undefined/空串），供派生字段判空。 */
const hasResetTimestamp = (resetAt) => resetAt !== null && resetAt !== undefined && resetAt !== '';

/**
 * 计算距重置还有多少秒：已过期归为 0；时间戳缺失或无法解析返回 null。
 * @param {number|string|null} resetAt 重置时间
 * @returns {number|null}
 */
export const calculateResetAfterSeconds = (resetAt) => {
  if (!hasResetTimestamp(resetAt)) return null;
  const resetAtTime = new Date(resetAt).getTime();
  if (!Number.isFinite(resetAtTime)) return null;
  const delta = Math.floor((resetAtTime - Date.now()) / 1000);
  return delta < 0 ? 0 : delta;
};

/**
 * 构造统一的用量窗口对象，是所有 provider 共享的窗口形状。自动派生
 * remainingPercent（仅当 usedPercent 为有限数字，且下限钳到 0）、
 * resetAfterSeconds 与本地化的重置时间；valueLabel 仅在提供时携带。
 * @param {{ usedPercent?: number, windowSeconds?: number|null, resetAt?: number|string|null, valueLabel?: string }} fields
 * @returns {object} 含 usedPercent/remainingPercent/windowSeconds/resetAfterSeconds/
 *   resetAt/resetAtFormatted/resetAfterFormatted（及可选 valueLabel）的窗口
 */
export const toUsageWindow = ({ usedPercent, windowSeconds, resetAt, valueLabel }) => {
  const resetAfterSeconds = calculateResetAfterSeconds(resetAt);
  const resetFormatted = hasResetTimestamp(resetAt) ? formatResetTime(resetAt) : null;
  const hasFiniteUsedPercent = typeof usedPercent === 'number' && Number.isFinite(usedPercent);
  return {
    usedPercent,
    remainingPercent: hasFiniteUsedPercent ? Math.max(0, 100 - usedPercent) : null,
    windowSeconds: windowSeconds ?? null,
    resetAfterSeconds,
    resetAt,
    resetAtFormatted: resetFormatted,
    resetAfterFormatted: resetFormatted,
    ...(valueLabel ? { valueLabel } : {})
  };
};

/**
 * 构造 provider 抓取结果的标准外壳：附上 fetchedAt 抓取时刻；error 与
 * planLabel 仅在非空时携带，usage 缺省归一为 null。
 * @param {{ providerId: string, providerName: string, ok: boolean, configured: boolean, usage?: object|null, error?: string, planLabel?: string }} result
 * @returns {object}
 */
export const buildResult = ({ providerId, providerName, ok, configured, usage, error, planLabel }) => ({
  providerId,
  providerName,
  ok,
  configured,
  usage: usage ?? null,
  ...(error ? { error } : {}),
  ...(planLabel ? { planLabel } : {}),
  fetchedAt: Date.now()
});

/**
 * 把 Google API 的时长枚举单位转换为短标签：分钟 `m`、小时 `h`、天 `d`；
 * 缺失或未知单位返回 `'limit'`。
 * @param {number} duration 时长数值
 * @param {string} unit `TIME_UNIT_MINUTE` / `TIME_UNIT_HOUR` / `TIME_UNIT_DAY`
 * @returns {string}
 */
export const durationToLabel = (duration, unit) => {
  if (!duration || !unit) return 'limit';
  if (unit === 'TIME_UNIT_MINUTE') return `${duration}m`;
  if (unit === 'TIME_UNIT_HOUR') return `${duration}h`;
  if (unit === 'TIME_UNIT_DAY') return `${duration}d`;
  return 'limit';
};

/**
 * 把 Google API 的时长枚举单位换算为秒；缺失或未知单位返回 null。
 * @param {number} duration 时长数值
 * @param {string} unit `TIME_UNIT_MINUTE` / `TIME_UNIT_HOUR` / `TIME_UNIT_DAY`
 * @returns {number|null}
 */
export const durationToSeconds = (duration, unit) => {
  if (!duration || !unit) return null;
  if (unit === 'TIME_UNIT_MINUTE') return duration * 60;
  if (unit === 'TIME_UNIT_HOUR') return duration * 3600;
  if (unit === 'TIME_UNIT_DAY') return duration * 86400;
  return null;
};

/**
 * 把金额格式化为两位小数的字符串（如 `'2.50'`）；非有限数字返回 null。
 * @param {unknown} value 金额数值
 * @returns {string|null}
 */
export const formatMoney = (value) => {
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  return value.toFixed(2);
};
