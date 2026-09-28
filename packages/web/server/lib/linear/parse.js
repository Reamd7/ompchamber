/**
 * Linear 模块共用的轻量值校验与规范化工具。
 *
 * 提供字符串/普通对象判断，以及 trim 后非空字符串、有限数值、环境变量的读取；
 * 所有函数对非法输入都给出安全兜底值，避免各模块重复编写防御性逻辑。
 */
/**
 * 判断值是否为字符串（基于 Object.prototype.toString，能正确识别跨 realm 的字符串）。
 * @param {unknown} value 待判断的值
 * @returns {boolean}
 */
export function isString(value) {
  return Object.prototype.toString.call(value) === '[object String]';
}

/**
 * 判断值是否为普通对象：排除 null、数组以及原型不是 Object.prototype 的对象
 * （如类实例、Map 等）。
 * @param {unknown} value 待判断的值
 * @returns {boolean}
 */
export function isPlainObject(value) {
  if (value == null || Array.isArray(value)) {
    return false;
  }
  return Object.getPrototypeOf(value) === Object.prototype;
}

/**
 * 读取 trim 后的非空字符串；值不是字符串或 trim 后为空时返回空字符串。
 * @param {unknown} value 输入值
 * @returns {string} 规范化后的字符串（可能为空串）
 */
export function readTrimmedString(value) {
  return isString(value) && value.trim() ? value.trim() : '';
}

/**
 * 读取有限数值；值为 NaN、Infinity 或非数值时返回 null。
 * @param {unknown} value 输入值
 * @returns {number | null}
 */
export function readFiniteNumber(value) {
  return Number.isFinite(value) ? value : null;
}

/**
 * 读取并 trim 环境变量；变量不存在或值为空白时返回空字符串。
 * @param {string} name 环境变量名
 * @returns {string}
 */
export function readEnv(name) {
  const raw = process.env[name];
  return raw ? raw.trim() : '';
}
