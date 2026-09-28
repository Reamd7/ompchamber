/**
 * Claude CLI 登录状态探测。
 *
 * 通过同步执行 `claude auth status --json` 判断本机 Claude Code CLI 是否
 * 已登录。调用前会剔除父进程注入的 API key/OAuth 环境变量（它们会让
 * CLI 绕过真实登录态，导致误报已连接）；桌面进程 PATH 找不到 claude 时，
 * 再通过登录 shell（或 Windows 的 where）定位一次。
 */
import { spawnSync } from 'node:child_process';

/**
 * 同步执行 `claude auth status --json` 并返回子进程结果。
 * 超时 6 秒；windowsHide 避免 Windows 上弹出控制台窗口；env 为净化后的环境。
 * @param {Function} spawnSyncFn - 可注入的同步执行器（测试替身）。
 * @param {string} command - claude 可执行文件（或其绝对路径）。
 * @param {object} env - 传给子进程的环境变量。
 */
const readStatus = (spawnSyncFn, command, env) => spawnSyncFn(command, ['auth', 'status', '--json'], {
  encoding: 'utf8',
  timeout: 6000,
  env,
  windowsHide: true,
});

/**
 * 当直接执行 claude 失败时，从登录 shell 环境定位 claude 可执行文件路径。
 * Windows 用 `where claude` 取第一条结果；类 Unix 用 $SHELL -lic
 * 'command -v claude'（交互式登录 shell 以加载用户 rc 文件中的 PATH）。
 * @returns {string|null} 解析到的可执行文件路径，失败返回 null。
 */
const resolveFromLoginShell = (spawnSyncFn, env, platform) => {
  if (platform === 'win32') {
    const result = spawnSyncFn('where', ['claude'], {
      encoding: 'utf8',
      timeout: 6000,
      env,
      windowsHide: true,
    });
    return `${result.stdout || ''}`.split(/\r?\n/).map((line) => line.trim()).find(Boolean) || null;
  }

  const shell = env.SHELL || '/bin/zsh';
  const result = spawnSyncFn(shell, ['-lic', 'command -v claude'], {
    encoding: 'utf8',
    timeout: 6000,
    env,
    windowsHide: true,
  });
  return `${result.stdout || ''}`.trim().split(/\s+/).pop() || null;
};

/**
 * 探测 Claude CLI 的权威登录状态。
 * 先剔除 ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN / CLAUDE_CODE_OAUTH_TOKEN，
 * 防止环境变量伪装登录；直接调用无输出且报错时经登录 shell 解析后重试。
 * 输出为空返回 reason 'empty-status'；解析出的 loggedIn 决定 connected。
 * @param {object} [options] - 可注入 spawnSyncFn、env、platform 便于测试。
 * @returns {{ connected: boolean, reason: string }} reason 为 'logged-in'、
 *   'logged-out'、'empty-status' 或异常消息文本。
 */
export const getClaudeCliAuthStatus = ({
  spawnSyncFn = spawnSync,
  env = process.env,
  platform = process.platform,
} = {}) => {
  const childEnv = { ...env };
  delete childEnv.ANTHROPIC_API_KEY;
  delete childEnv.ANTHROPIC_AUTH_TOKEN;
  delete childEnv.CLAUDE_CODE_OAUTH_TOKEN;

  try {
    let result = readStatus(spawnSyncFn, 'claude', childEnv);
    if (!`${result.stdout || ''}`.trim() && result.error) {
      const resolved = resolveFromLoginShell(spawnSyncFn, childEnv, platform);
      if (resolved) result = readStatus(spawnSyncFn, resolved, childEnv);
    }
    const output = `${result.stdout || ''}`.trim();
    if (!output) return { connected: false, reason: 'empty-status' };
    const payload = JSON.parse(output);
    return {
      connected: payload?.loggedIn === true,
      reason: payload?.loggedIn === true ? 'logged-in' : 'logged-out',
    };
  } catch (error) {
    return {
      connected: false,
      reason: error instanceof Error ? error.message : String(error),
    };
  }
};
