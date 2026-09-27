/**
 * gh CLI 凭据读取模块。
 *
 * 通过同步执行 `gh auth token` 获取本机 gh CLI 已登录的 token，作为
 * OMPChamber 自身 OAuth token 之外的备选凭证；结果缓存 30 秒，避免每次
 * GitHub 调用都起一个子进程。任何失败（未安装 gh、未登录、超时）均返回
 * null，不抛出。
 */
import { execFileSync } from 'child_process';

/** token 缓存有效期：30 秒，期间重复调用不再起子进程。 */
const CACHE_TTL_MS = 30_000;
/** 缓存的 token 值；null 表示上次查询失败或尚无缓存。 */
let cachedToken = null;
/** 缓存写入时间戳（ms epoch）。 */
let cachedAt = 0;
/** 是否已缓存过一次结果；用于区分"缓存了 null"与"从未查询"。 */
let hasCachedToken = false;

/**
 * 同步执行 `gh auth token` 读取 gh CLI 的登录 token：5 秒超时、隐藏
 * Windows 子进程窗口。任何异常（命令不存在、非零退出、空输出）都返回
 * null。
 */
function fetchGhCliToken() {
  try {
    const token = execFileSync('gh', ['auth', 'token'], {
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'pipe'],
      timeout: 5000,
      windowsHide: true,
    }).trim();
    return token || null;
  } catch {
    return null;
  }
}

/**
 * 读取 gh CLI token，带 30 秒结果缓存（失败结果同样缓存，避免反复起
 * 子进程探测）。需要立即刷新时先调用 clearGhCliTokenCache。
 */
export function getGhCliToken() {
  const now = Date.now();
  if (hasCachedToken && now - cachedAt < CACHE_TTL_MS) {
    return cachedToken;
  }
  const token = fetchGhCliToken();
  cachedToken = token;
  cachedAt = now;
  hasCachedToken = true;
  return token;
}

/** 清空 token 缓存，使下一次 getGhCliToken 重新执行 gh 命令。 */
export function clearGhCliTokenCache() {
  cachedToken = null;
  cachedAt = 0;
  hasCachedToken = false;
}
