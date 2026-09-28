/**
 * Claude 凭据发现测试套件：通过 mock `security` 子进程、fs 与 OpenCode
 * auth.json，验证 loadClaudeCredential 的来源优先级（Keychain > 凭据文件 >
 * OpenCode > 环境变量）、跨平台分支（darwin/linux）以及“blob 只含无关
 * MCP token 时返回 null”的过滤行为。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// mock 的 child_process.execFileSync：默认抛错模拟“Keychain 无条目”，
// 各用例按需改为返回 Claude Code 凭据 blob。
const execFileSync = vi.fn();
// 内存文件系统：path -> 文件内容，供 mock 的 fs.existsSync/readFileSync 使用。
const files = new Map();
// mock 的 OpenCode readAuthFile()，默认返回空对象（未登录）。
const openCodeAuth = vi.fn(() => ({}));

vi.mock('child_process', () => ({ execFileSync: (...args) => execFileSync(...args) }));

vi.mock('fs', () => {
  const fs = {
    existsSync: (filePath) => files.has(filePath),
    readFileSync: (filePath) => {
      if (!files.has(filePath)) throw new Error('ENOENT');
      return files.get(filePath);
    },
  };
  return { ...fs, default: fs };
});

vi.mock('../../../opencode/auth.js', () => ({ readAuthFile: () => openCodeAuth() }));

import { loadClaudeCredential } from './auth.js';

// 构造 Claude Code 凭据 blob 的 JSON 字符串：混入无关的 MCP token，
// 确保解析只读取 claudeAiOauth 条目。
const claudeCodeBlob = (accessToken) => JSON.stringify({
  mcpOAuth: { 'linear|abc': { accessToken: 'unrelated-mcp-token' } },
  claudeAiOauth: {
    accessToken,
    refreshToken: `${accessToken}-refresh`,
    expiresAt: 1786735755912,
    subscriptionType: 'max',
  },
});

// 临时把 process.platform 改为指定值执行 run()，结束后恢复原描述符，
// 用于覆盖 darwin/linux 分支。
const withPlatform = (platform, run) => {
  const original = Object.getOwnPropertyDescriptor(process, 'platform');
  Object.defineProperty(process, 'platform', { value: platform, configurable: true });
  try {
    return run();
  } finally {
    Object.defineProperty(process, 'platform', original);
  }
};

// 每个用例前重置所有 mock 与环境变量，保证来源优先级从干净状态开始。
beforeEach(() => {
  files.clear();
  execFileSync.mockReset();
  execFileSync.mockImplementation(() => { throw new Error('no keychain entry'); });
  openCodeAuth.mockReturnValue({});
  delete process.env.CLAUDE_CONFIG_DIR;
  delete process.env.CLAUDE_CODE_OAUTH_TOKEN;
});

// 清理用例中设置的 CLAUDE_* 环境变量，避免泄漏到其它测试文件。
afterEach(() => {
  delete process.env.CLAUDE_CONFIG_DIR;
  delete process.env.CLAUDE_CODE_OAUTH_TOKEN;
});

// 主 describe：按来源优先级逐级验证凭据发现与降级行为。
describe('Claude credential discovery', () => {
  it('prefers the macOS Keychain over a stale credentials file', () => {
    execFileSync.mockReturnValue(claudeCodeBlob('keychain-token'));
    files.set(`${process.env.HOME}/.claude/.credentials.json`, claudeCodeBlob('file-token'));

    const credential = withPlatform('darwin', loadClaudeCredential);

    expect(credential.accessToken).toBe('keychain-token');
    expect(credential.refreshToken).toBe('keychain-token-refresh');
    expect(credential.planLabel).toBe('max');
    expect(credential.source).toBe('keychain');
  });

  it('reads the credentials file on Linux, where there is no Keychain', () => {
    files.set(`${process.env.HOME}/.claude/.credentials.json`, claudeCodeBlob('file-token'));

    const credential = withPlatform('linux', loadClaudeCredential);

    expect(execFileSync).not.toHaveBeenCalled();
    expect(credential.accessToken).toBe('file-token');
    expect(credential.source).toBe('credentials-file');
  });

  it('honours CLAUDE_CONFIG_DIR when locating the credentials file', () => {
    process.env.CLAUDE_CONFIG_DIR = '/tmp/claude-home';
    files.set('/tmp/claude-home/.credentials.json', claudeCodeBlob('custom-dir-token'));

    expect(withPlatform('linux', loadClaudeCredential).accessToken).toBe('custom-dir-token');
  });

  it('falls back to the OpenCode auth entry when Claude Code is not signed in', () => {
    openCodeAuth.mockReturnValue({ anthropic: { access: 'opencode-token', refresh: 'opencode-refresh', expires: 1786735755912 } });

    const credential = withPlatform('linux', loadClaudeCredential);

    expect(credential.accessToken).toBe('opencode-token');
    expect(credential.source).toBe('opencode-auth');
    expect(credential.planLabel).toBeNull();
  });

  it('falls back to CLAUDE_CODE_OAUTH_TOKEN last, without a refresh token', () => {
    process.env.CLAUDE_CODE_OAUTH_TOKEN = 'env-token';

    const credential = withPlatform('linux', loadClaudeCredential);

    expect(credential.accessToken).toBe('env-token');
    expect(credential.refreshToken).toBeNull();
    expect(credential.source).toBe('env');
  });

  it('ignores a Keychain blob that only holds unrelated MCP tokens', () => {
    execFileSync.mockReturnValue(JSON.stringify({ mcpOAuth: { 'linear|abc': { accessToken: 'unrelated' } } }));

    expect(withPlatform('darwin', loadClaudeCredential)).toBeNull();
  });

  it('returns null when every source is empty', () => {
    expect(withPlatform('darwin', loadClaudeCredential)).toBeNull();
  });
});
