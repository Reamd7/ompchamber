/**
 * executable-search 模块测试套件。
 *
 * 覆盖可执行文件搜索的 Windows 特有行为：搜索目录解析（自动追加 WindowsApps
 * 别名目录、Path 大小写变体读取）、PATH 查找（Store 应用别名 + PATHEXT 扩展名
 * 展开）、启动目标解析（stat 失败时回退原始命令名并注入净化 PATH），以及
 * 环境副本中 PATH/Path/path 三变体的一致性。
 */
import { describe, expect, it } from 'bun:test';

import {
  createExecutableSearchEnv,
  findExecutableOnPath,
  getExecutableSearchDirectories,
  resolveExecutableLaunchTarget,
} from './executable-search.js';

/** 搜索目录解析：验证 Windows 平台自动追加 WindowsApps 目录与 Path 变体读取。 */
describe('getExecutableSearchDirectories', () => {
  it('adds the WindowsApps app-alias directory on Windows', () => {
    const directories = getExecutableSearchDirectories({
      platform: 'win32',
      env: {
        PATH: 'C:\\Tools',
        LOCALAPPDATA: 'C:\\Users\\Ada\\AppData\\Local',
      },
    });

    expect(directories).toContain('C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps');
  });

  it('reads Windows Path casing when PATH is not present', () => {
    const directories = getExecutableSearchDirectories({
      platform: 'win32',
      env: {
        Path: 'C:\\Tools;C:\\MoreTools',
        LOCALAPPDATA: 'C:\\Users\\Ada\\AppData\\Local',
      },
    });

    expect(directories[0]).toBe('C:\\Tools');
    expect(directories[1]).toBe('C:\\MoreTools');
  });
});

/** PATH 查找：验证即使 PATH 未包含 WindowsApps 也能借注入目录找到 Store 应用别名。 */
describe('findExecutableOnPath', () => {
  it('finds Windows Store app execution aliases even when PATH omits WindowsApps', () => {
    const aliasPath = 'C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps\\ngrok.exe';
    const fsLike = {
      statSync: (candidate) => {
        if (candidate === aliasPath) {
          return { isFile: () => true };
        }
        throw new Error('not found');
      },
      accessSync: () => {},
    };

    const resolved = findExecutableOnPath('ngrok', {
      platform: 'win32',
      env: {
        PATH: 'C:\\Tools',
        LOCALAPPDATA: 'C:\\Users\\Ada\\AppData\\Local',
        PATHEXT: '.EXE;.CMD',
      },
      fsLike,
    });

    expect(resolved).toBe(aliasPath);
  });
});

/** 启动目标：验证 stat 全部失败（EACCES）时回退原始命令名，且 PATH 含 WindowsApps。 */
describe('resolveExecutableLaunchTarget', () => {
  it('returns a Windows launch target with WindowsApps on PATH when stat lookup fails', () => {
    const target = resolveExecutableLaunchTarget('ngrok', {
      platform: 'win32',
      env: {
        PATH: 'C:\\Windows\\System32',
        LOCALAPPDATA: 'C:\\Users\\Ada\\AppData\\Local',
      },
      fsLike: {
        statSync: () => { throw new Error('EACCES'); },
        accessSync: () => {},
      },
    });

    expect(target?.command).toBe('ngrok');
    expect(target?.env.Path).toContain('C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps');
  });
});

/** 环境副本：验证 Windows 下 PATH/Path/path 三个大小写变体保持同步。 */
describe('createExecutableSearchEnv', () => {
  it('keeps Windows PATH variants in sync', () => {
    const env = createExecutableSearchEnv({
      platform: 'win32',
      env: {
        PATH: 'C:\\Windows\\System32',
        LOCALAPPDATA: 'C:\\Users\\Ada\\AppData\\Local',
      },
    });

    expect(env.PATH).toBe(env.Path);
    expect(env.path).toBe(env.Path);
  });
});
