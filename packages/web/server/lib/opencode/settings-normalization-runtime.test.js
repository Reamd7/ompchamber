/**
 * settings-normalization-runtime 测试套件：围绕符号链接解析验证设置持久化
 * 前的路径规范化——normalizePathForPersistence 的 realpath 解析、异常回退
 * 与 Windows 盘符大小写处理，sanitizeProjects 的路径解析、默认思考等级
 * 配对保留与 realpath 去重，以及 normalizeSettingsPaths 的变更标记。
 * 通过注入假的 os / path / realpathSync 构造纯内存运行时。
 */
import { describe, expect, it } from 'vitest';

import { createSettingsNormalizationRuntime } from './settings-normalization-runtime.js';

/** 用默认依赖构造被测运行时；overrides 可逐项替换（如自定义 realpathSync）。 */
const createTestRuntime = (overrides = {}) => {
  const defaults = {
    os: { homedir: () => '/home/testuser' },
    path: {
      resolve: (...args) => args[args.length - 1],
      sep: '/',
      dirname: (p) => p.split('/').slice(0, -1).join('/') || '/',
    },
    processLike: { platform: 'linux', env: {} },
    realpathSync: (p) => p,
    tunnelBootstrapTtlDefaultMs: 600000,
    tunnelBootstrapTtlMinMs: 60000,
    tunnelBootstrapTtlMaxMs: 3600000,
    tunnelSessionTtlDefaultMs: 86400000,
    tunnelSessionTtlMinMs: 3600000,
    tunnelSessionTtlMaxMs: 604800000,
  };

  return createSettingsNormalizationRuntime({ ...defaults, ...overrides });
};

// 符号链接解析主套件。
describe('settings normalization runtime - symlink resolution', () => {
  // normalizePathForPersistence：realpath 解析、异常回退与 Windows 盘符处理。
  describe('normalizePathForPersistence', () => {
    it('resolves symlinks via realpathSync', () => {
      const runtime = createTestRuntime({
        realpathSync: (p) =>
          p === '/home/user/workplace' ? '/workplace/user' : p,
      });

      const result = runtime.normalizePathForPersistence('/home/user/workplace');
      expect(result).toBe('/workplace/user');
    });

    it('falls back to original path when realpathSync throws', () => {
      const runtime = createTestRuntime({
        realpathSync: () => {
          throw new Error('ENOENT');
        },
      });

      const result = runtime.normalizePathForPersistence('/nonexistent/path');
      expect(result).toBe('/nonexistent/path');
    });

    it('passes through when realpathSync is not provided', () => {
      const runtime = createTestRuntime({ realpathSync: undefined });

      const result = runtime.normalizePathForPersistence('/some/path');
      expect(result).toBe('/some/path');
    });

    it('preserves lowercase colon-prefixed paths on non-Windows platforms', () => {
      const runtime = createTestRuntime({ realpathSync: undefined });

      expect(runtime.normalizePathForPersistence('c:project')).toBe('c:project');
    });

    it('uppercases Windows drive letter before and after realpath resolution', () => {
      const runtime = createTestRuntime({
        processLike: { platform: 'win32', env: {} },
        realpathSync: (p) => {
          // Simulate safeRealpathSync returning a lowercase drive letter
          if (p === 'C:\\Users\\me\\project') return 'c:\\real\\project';
          return p;
        },
      });

      const result = runtime.normalizePathForPersistence('c:\\Users\\me\\project');
      // Drive letter uppercased on input AND after realpath
      expect(result).toBe('C:\\real\\project');
    });
  });

  // sanitizeProjects：项目路径解析、默认思考等级配对与 realpath 去重。
  describe('sanitizeProjects', () => {
    it('resolves symlinks in project paths', () => {
      const runtime = createTestRuntime({
        realpathSync: (p) =>
          p === '/home/user/workplace/MyProject'
            ? '/workplace/user/MyProject'
            : p,
      });

      const projects = [
        { id: 'proj1', path: '/home/user/workplace/MyProject', label: 'MyProject', color: 'primary', addedAt: 1000, lastOpenedAt: 1000 },
      ];

      const result = runtime.sanitizeProjects(projects);
      expect(result[0].path).toBe('/workplace/user/MyProject');
    });

    it('falls back to path.resolve when realpathSync throws', () => {
      const runtime = createTestRuntime({
        realpathSync: () => { throw new Error('ENOENT'); },
        path: { resolve: (p) => '/resolved' + p, sep: '/', dirname: (p) => p.split('/').slice(0, -1).join('/') || '/' },
      });

      const projects = [
        { id: 'proj1', path: '/missing/path', label: 'Missing', color: 'primary', addedAt: 1000, lastOpenedAt: 1000 },
      ];

      const result = runtime.sanitizeProjects(projects);
      expect(result[0].path).toBe('/resolved/missing/path');
    });

    it('keeps a default thinking level next to its model and drops it alone', () => {
      const runtime = createTestRuntime({
        realpathSync: (p) => p,
        path: { resolve: (p) => p, sep: '/', dirname: (p) => p.split('/').slice(0, -1).join('/') || '/' },
      });

      const projects = [
        { id: 'proj1', path: '/a', defaultModel: 'anthropic/claude-opus-5', defaultVariant: 'high' },
        { id: 'proj2', path: '/b', defaultVariant: 'high' },
      ];

      const result = runtime.sanitizeProjects(projects);
      expect(result[0].defaultVariant).toBe('high');
      expect(result[1].defaultVariant).toBe(undefined);
    });

    it('deduplicates projects that resolve to the same realpath', () => {
      const runtime = createTestRuntime({
        realpathSync: (p) => p.startsWith('/symlink') ? '/real/project' : p,
        path: { resolve: (p) => p, sep: '/', dirname: (p) => p.split('/').slice(0, -1).join('/') || '/' },
      });

      const projects = [
        { id: 'proj1', path: '/symlink/a', label: 'A', color: 'primary', addedAt: 1000, lastOpenedAt: 1000 },
        { id: 'proj2', path: '/symlink/b', label: 'B', color: 'keyword', addedAt: 2000, lastOpenedAt: 2000 },
      ];

      const result = runtime.sanitizeProjects(projects);
      expect(result).toHaveLength(1);
      expect(result[0].id).toBe('proj1');
    });
  });

  // normalizeSettingsPaths：lastDirectory 的符号链接解析与 changed 标记。
  describe('normalizeSettingsPaths', () => {
    it('resolves symlinks in lastDirectory', () => {
      const runtime = createTestRuntime({
        realpathSync: (p) =>
          p === '/home/user/workplace/LyraRefactoring'
            ? '/workplace/user/LyraRefactoring'
            : p,
      });

      const result = runtime.normalizeSettingsPaths({
        lastDirectory: '/home/user/workplace/LyraRefactoring',
      });

      expect(result.changed).toBe(true);
      expect(result.settings.lastDirectory).toBe('/workplace/user/LyraRefactoring');
    });

    it('does not flag as changed when path is already canonical', () => {
      const runtime = createTestRuntime();

      const result = runtime.normalizeSettingsPaths({
        lastDirectory: '/real/path',
      });

      expect(result.changed).toBe(false);
    });
  });
});
