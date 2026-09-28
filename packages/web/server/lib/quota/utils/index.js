/**
 * Quota Utilities
 *
 * Shared utility functions for quota calculations and formatting.
 * @module quota/utils
 */
/**
 * 配额工具桶（barrel）：统一出口，聚合鉴权/文件解析（auth.js）、类型
 * 转换（transformers.js）与格式化（formatters.js），供各 provider 通过
 * `../../utils/index.js` 一次性引入。
 */

// 凭据文件读取与 auth 条目解析。
export * from './auth.js';
// 原始字段到对象/字符串/数字/时间戳的宽容归一化。
export * from './transformers.js';
// 用量窗口构造、时间/金额/时长格式化与 provider 结果外壳。
export * from './formatters.js';
