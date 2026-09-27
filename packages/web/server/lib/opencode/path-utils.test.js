/**
 * path-utils 模块的单元测试。
 *
 * 覆盖 pathLooksUserConfigured 的启发式判定：空/非字符串与最小系统
 * PATH 判为 false，家目录路径、Homebrew/pkg/snap 等包管理器前缀、
 * .cargo/.nvm/.pyenv 等 dotfile 工具链目录以及 Windows 风格路径判为
 * true；以及 mergePathValues 的保序去重合并语义。
 */
import path from 'node:path';
import os from 'node:os';
import { describe, expect, it } from 'vitest';

import { pathLooksUserConfigured, mergePathValues } from './path-utils.js';

/** 当前用户主目录，用于构造“像用户配置过”的 PATH 段。 */
const home = os.homedir();
/** 当前平台的 PATH 分隔符（POSIX 为 ':'，Windows 为 ';'）。 */
const delim = path.delimiter;

// PATH 是否“由用户配置”的启发式判定
describe('pathLooksUserConfigured', () => {
  it('returns false for empty or non-string values', () => {
    expect(pathLooksUserConfigured('', home, delim)).toBe(false);
    expect(pathLooksUserConfigured(null, home, delim)).toBe(false);
    expect(pathLooksUserConfigured(undefined, home, delim)).toBe(false);
    expect(pathLooksUserConfigured(42, home, delim)).toBe(false);
  });

  it('returns false for minimal system PATH', () => {
    expect(pathLooksUserConfigured('/usr/local/bin:/usr/bin:/bin', home, delim)).toBe(false);
  });

  it('detects paths under home directory', () => {
    expect(pathLooksUserConfigured(`${home}/.bun/bin:/usr/bin`, home, delim)).toBe(true);
    expect(pathLooksUserConfigured(`${home}/.local/bin:/usr/bin`, home, delim)).toBe(true);
  });

  it('detects home directory itself', () => {
    expect(pathLooksUserConfigured(`${home}:/usr/bin`, home, delim)).toBe(true);
  });

  it('detects well-known package manager prefixes', () => {
    expect(pathLooksUserConfigured('/opt/homebrew/bin:/usr/bin', home, delim)).toBe(true);
    expect(pathLooksUserConfigured('/opt/pkg/bin:/usr/bin', home, delim)).toBe(true);
    expect(pathLooksUserConfigured('/snap/bin:/usr/bin', home, delim)).toBe(true);
  });

  it('detects well-known dot-directory basenames', () => {
    expect(pathLooksUserConfigured('/some/path/.cargo/bin:/usr/bin', home, delim)).toBe(true);
    expect(pathLooksUserConfigured('/some/path/.nvm/versions/node/v20/bin:/usr/bin', home, delim)).toBe(true);
    expect(pathLooksUserConfigured('/some/path/.pyenv/shims:/usr/bin', home, delim)).toBe(true);
    expect(pathLooksUserConfigured('/some/path/.opencode/bin:/usr/bin', home, delim)).toBe(true);
  });

  it('detects Windows home and toolchain paths', () => {
    const windowsHome = 'C:\\Users\\agent';
    expect(pathLooksUserConfigured('C:\\Users\\agent\\.bun\\bin;C:\\Windows\\System32', windowsHome, ';')).toBe(true);
    expect(pathLooksUserConfigured('C:\\tools\\.cargo\\bin;C:\\Windows\\System32', windowsHome, ';')).toBe(true);
  });
});

// 两段 PATH 值的保序去重合并
describe('mergePathValues', () => {
  it('returns empty string for empty inputs', () => {
    expect(mergePathValues('', '', delim)).toBe('');
  });

  it('returns primary when fallback is empty', () => {
    expect(mergePathValues('/a:/b', '', delim)).toBe('/a:/b');
  });

  it('returns fallback when primary is empty', () => {
    expect(mergePathValues('', '/a:/b', delim)).toBe('/a:/b');
  });

  it('deduplicates segments, preserving primary order', () => {
    expect(mergePathValues('/a:/b:/c', '/b:/d:/a', delim)).toBe('/a:/b:/c:/d');
  });

  it('appends all fallback segments when no overlap', () => {
    expect(mergePathValues('/a:/b', '/c:/d', delim)).toBe('/a:/b:/c:/d');
  });
});
