/**
 * server-utils-runtime.js 的 PATH 构建逻辑单元测试（vitest）。
 *
 * 覆盖 buildManagedOpenCodePath（用户自定义 PATH 优先、进程 PATH 最小时以
 * 登录 shell PATH 为主合并、Windows 包管理器目录发现）与 buildAugmentedPath
 * （保持用户 PATH 顺序、最小 PATH 时的回退）。afterEach 统一清理临时目录并
 * 恢复 PATH 环境变量。
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import { createServerUtilsRuntime } from './server-utils-runtime.js';

// 用例运行前捕获的原始 PATH，afterEach 中恢复。
const originalPath = process.env.PATH;
// afterEach 待清理的临时目录列表。
const tempDirs = [];

/** 创建带前缀的临时目录并登记到 tempDirs 以便统一清理。 */
const createTempDir = (prefix) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  tempDirs.push(dir);
  return dir;
};

// 清理全部临时目录并恢复 PATH 环境变量。
afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }

  if (originalPath === undefined) {
    delete process.env.PATH;
    return;
  }

  process.env.PATH = originalPath;
});

/**
 * 构造注入桩依赖的服务端工具运行时：getLoginShellPath 由参数提供，
 * processLike 可模拟平台（linux/win32）与自定义环境变量；其余回调均为空实现，
 * 仅 PATH 构建逻辑真实运行。
 */
const createRuntime = (loginShellPath, processLike = { platform: 'linux', env: process.env }) => createServerUtilsRuntime({
  fs,
  os,
  path,
  process: processLike,
  openCodeReadyGraceMs: 0,
  longRequestTimeoutMs: 0,
  getRuntime: () => ({}),
  getOpenCodeAuthHeaders: () => ({}),
  buildOpenCodeUrl: (route) => route,
  ensureOpenCodeApiPrefix: () => {},
  getUiNotificationClients: () => new Set(),
  getOpenCodePort: () => null,
  setOpenCodePortState: () => {},
  syncToHmrState: () => {},
  markOpenCodeNotReady: () => {},
  setOpenCodeNotReadySince: () => {},
  clearLastOpenCodeError: () => {},
  getLoginShellPath: () => loginShellPath,
});

// 套件：托管 OpenCode PATH 与增强 PATH 的构建规则（优先级、合并顺序与平台差异）。
describe('server utils runtime', () => {
  it('prefers shell PATH for managed OpenCode before appending process-only entries', () => {
    const home = os.homedir();
    const currentPath = [
      path.join(home, '.opencode', 'bin'),
      path.join(home, '.bun', 'bin'),
      path.join(home, 'Library', 'pnpm'),
      '/opt/homebrew/bin',
      '/usr/bin',
    ].join(path.delimiter);
    process.env.PATH = currentPath;

    const runtime = createRuntime([
      path.join(home, '.opencode', 'bin'),
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
      path.join(home, '.cargo', 'bin'),
    ].join(path.delimiter));

    expect(runtime.buildManagedOpenCodePath()).toBe([
      path.join(home, '.opencode', 'bin'),
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
      path.join(home, '.cargo', 'bin'),
      path.join(home, 'Library', 'pnpm'),
    ].join(path.delimiter));
  });

  it('uses login shell PATH for managed OpenCode when process PATH is minimal', () => {
    const home = os.homedir();
    const loginShellPath = [
      path.join(home, '.opencode', 'bin'),
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
    ].join(path.delimiter);
    process.env.PATH = ['/usr/local/bin', '/usr/bin', '/bin'].join(path.delimiter);

    const runtime = createRuntime(loginShellPath);

    // Should prefer login shell PATH but merge in any process entries not already present.
    expect(runtime.buildManagedOpenCodePath()).toBe([
      path.join(home, '.opencode', 'bin'),
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
      '/usr/local/bin',
      '/bin',
    ].join(path.delimiter));
  });

  it('adds existing Windows package-manager directories to managed OpenCode PATH', () => {
    const root = createTempDir('ompchamber-win-path-');
    const systemDir = path.join(root, 'System32');
    const appData = path.join(root, 'Roaming');
    const programFiles = path.join(root, 'Program Files');
    const localAppData = path.join(root, 'Local');
    const programData = path.join(root, 'ProgramData');
    const userProfile = path.join(root, 'User');

    const npmBin = path.join(appData, 'npm');
    const nodeBin = path.join(programFiles, 'nodejs');
    const pnpmHome = path.join(localAppData, 'pnpm');
    const yarnBin = path.join(localAppData, 'Yarn', 'bin');
    const chocoBin = path.join(programData, 'chocolatey', 'bin');

    for (const dir of [systemDir, npmBin, nodeBin, pnpmHome, yarnBin, chocoBin]) {
      fs.mkdirSync(dir, { recursive: true });
    }

    const runtime = createRuntime(null, {
      platform: 'win32',
      env: {
        PATH: systemDir,
        APPDATA: appData,
        ProgramFiles: programFiles,
        LOCALAPPDATA: localAppData,
        ProgramData: programData,
        USERPROFILE: userProfile,
      },
    });

    expect(runtime.buildManagedOpenCodePath()).toBe([
      systemDir,
      npmBin,
      nodeBin,
      pnpmHome,
      yarnBin,
      chocoBin,
    ].join(path.delimiter));
  });

  it('preserves user-configured process PATH order before appending shell-only entries', () => {
    const home = os.homedir();
    process.env.PATH = [
      path.join(home, '.bun', 'bin'),
      path.join(home, 'Library', 'pnpm'),
      '/opt/homebrew/bin',
      '/usr/bin',
    ].join(path.delimiter);

    const runtime = createRuntime([
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      path.join(home, '.cargo', 'bin'),
      '/usr/bin',
    ].join(path.delimiter));

    expect(runtime.buildAugmentedPath()).toBe([
      path.join(home, '.bun', 'bin'),
      path.join(home, 'Library', 'pnpm'),
      '/opt/homebrew/bin',
      '/usr/bin',
      path.join(home, '.cargo', 'bin'),
    ].join(path.delimiter));
  });

  it('prefers login shell PATH when current process PATH is minimal', () => {
    const home = os.homedir();
    process.env.PATH = ['/usr/local/bin', '/usr/bin', '/bin'].join(path.delimiter);

    const runtime = createRuntime([
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
    ].join(path.delimiter));

    expect(runtime.buildAugmentedPath()).toBe([
      path.join(home, '.bun', 'bin'),
      '/opt/homebrew/bin',
      '/usr/bin',
      '/usr/local/bin',
      '/bin',
    ].join(path.delimiter));
  });
});
