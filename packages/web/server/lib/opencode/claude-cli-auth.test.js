/**
 * claude-cli-auth 模块的单元测试。
 *
 * 覆盖：以净化后的环境执行 `claude auth status --json` 得到权威登录态
 * （并断言凭证环境变量确实被剔除）；CLI 登出时不被 OpenCode 的过期
 * 标记误导；桌面 PATH 找不到 claude 时经登录 shell（zsh -lic）定位后重试。
 */
import { describe, expect, test } from 'bun:test';

import { getClaudeCliAuthStatus } from './claude-cli-auth.js';

// getClaudeCliAuthStatus 的探测、环境净化与登录 shell 回退链路
describe('getClaudeCliAuthStatus', () => {
  test('reports the authoritative Claude CLI login state', () => {
    let invocation = null;
    const status = getClaudeCliAuthStatus({
      env: {
        PATH: '/usr/bin',
        CLAUDE_CODE_OAUTH_TOKEN: 'must-not-leak',
      },
      spawnSyncFn(command, args, options) {
        invocation = { command, args, options };
        return { stdout: JSON.stringify({ loggedIn: true, authMethod: 'oauth' }) };
      },
    });

    expect(status).toEqual({ connected: true, reason: 'logged-in' });
    expect(invocation.command).toBe('claude');
    expect(invocation.args).toEqual(['auth', 'status', '--json']);
    expect(invocation.options.env.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
  });

  test('ignores a stale OpenCode marker when the CLI is logged out', () => {
    const status = getClaudeCliAuthStatus({
      spawnSyncFn: () => ({ stdout: JSON.stringify({ loggedIn: false }) }),
    });

    expect(status).toEqual({ connected: false, reason: 'logged-out' });
  });

  test('finds Claude through a login shell when a desktop PATH cannot', () => {
    const invocations = [];
    const status = getClaudeCliAuthStatus({
      env: { HOME: '/Users/test', PATH: '/usr/bin:/bin', SHELL: '/bin/zsh' },
      platform: 'darwin',
      spawnSyncFn(command, args, options) {
        invocations.push({ command, args, options });
        if (command === 'claude') return { stdout: '', error: new Error('spawnSync claude ENOENT') };
        if (command === '/bin/zsh') return { stdout: '/Users/test/.local/bin/claude\n' };
        return { stdout: JSON.stringify({ loggedIn: true, authMethod: 'claude.ai' }) };
      },
    });

    expect(status).toEqual({ connected: true, reason: 'logged-in' });
    expect(invocations.map(({ command }) => command)).toEqual([
      'claude',
      '/bin/zsh',
      '/Users/test/.local/bin/claude',
    ]);
    expect(invocations[1].args).toEqual(['-lic', 'command -v claude']);
  });
});
