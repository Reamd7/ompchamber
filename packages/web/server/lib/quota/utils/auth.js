/**
 * 配额工具 - 凭据文件与鉴权条目解析：JSON 配置文件的宽容读取（缺失/
 * 损坏返回 null 不抛错）、antigravity 账号文件路径、以及 auth.json 条目
 * 的别名匹配与形态归一化（字符串 token 或对象）。供各 provider 的
 * auth 模块复用。
 */
import fs from 'fs';
import path from 'path';
import os from 'os';

/** OpenCode 配置目录（~/.config/opencode）。 */
const OPENCODE_CONFIG_DIR = path.join(os.homedir(), '.config', 'opencode');
/** OpenCode 数据目录（~/.local/share/opencode）。 */
const OPENCODE_DATA_DIR = path.join(os.homedir(), '.local', 'share', 'opencode');

/**
 * antigravity-accounts.json 的候选路径：配置目录与数据目录各一份，
 * 按序探测，先命中者生效。
 */
export const ANTIGRAVITY_ACCOUNTS_PATHS = [
  path.join(OPENCODE_CONFIG_DIR, 'antigravity-accounts.json'),
  path.join(OPENCODE_DATA_DIR, 'antigravity-accounts.json')
];

/**
 * 宽容地读取并解析 JSON 文件：文件不存在、内容为空或解析失败均返回
 * null（解析失败额外 console.warn 一条日志），绝不抛错。
 * @param {string} filePath 绝对路径
 * @returns {any|null} 解析后的 JSON 值
 */
export const readJsonFile = (filePath) => {
  if (!fs.existsSync(filePath)) {
    return null;
  }
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const trimmed = raw.trim();
    if (!trimmed) return null;
    return JSON.parse(trimmed);
  } catch (error) {
    console.warn(`Failed to read JSON file: ${filePath}`, error);
    return null;
  }
};

/**
 * 按别名顺序在 auth 对象中查找第一个存在的条目。
 * @param {object} auth readAuthFile() 的结果
 * @param {string[]} aliases 别名列表（按优先级）
 * @returns {any|null} 命中的条目；全部未命中返回 null
 */
export const getAuthEntry = (auth, aliases) => {
  for (const alias of aliases) {
    if (auth[alias]) {
      return auth[alias];
    }
  }
  return null;
};

/**
 * 把 auth 条目归一化为对象：字符串条目包装为 `{ token }`，对象原样
 * 透传，其余（null/数字等）返回 null。
 * @param {any} entry 原始条目
 * @returns {object|null}
 */
export const normalizeAuthEntry = (entry) => {
  if (!entry) return null;
  if (typeof entry === 'string') {
    return { token: entry };
  }
  if (typeof entry === 'object') {
    return entry;
  }
  return null;
};
