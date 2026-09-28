/**
 * project-directory-runtime.js 的单元测试套件：目录校验（validateDirectoryPath）
 * 与项目目录解析（resolveProjectDirectory / resolveOptionalProjectDirectory）。
 * 通过注入伪造的 fsPromises 与设置读取函数，覆盖 symlink 归一、URI 编码头域、
 * 多级回退顺序以及各类错误分支，不触碰真实文件系统。
 */
import { describe, expect, it } from 'vitest';

import { createProjectDirectoryRuntime } from './project-directory-runtime.js';

/**
 * 以默认桩依赖加 overrides 构造 createProjectDirectoryRuntime 测试实例。
 * 默认桩：stat 恒返回目录、realpath 原样透传、设置读取返回空对象、项目列表原样透传。
 * @param {object} [overrides] 需覆盖的依赖项（如 fsPromises、getReadSettingsFromDiskMigrated）
 * @returns {object} 项目目录运行时实例
 */
const createTestRuntime = (overrides = {}) => {
  const defaults = {
    fsPromises: {
      stat: async () => ({ isDirectory: () => true }),
      realpath: async (p) => p,
    },
    path: {
      resolve: (p) => p,
    },
    normalizeDirectoryPath: (value) => value,
    readSettingsFromDiskMigrated: async () => ({}),
    getReadSettingsFromDiskMigrated: () => async () => ({}),
    sanitizeProjects: (input) => input,
  };

  return createProjectDirectoryRuntime({ ...defaults, ...overrides });
};

/** 项目目录运行时：目录校验与三级来源解析（header / query / 设置）的行为测试。 */
describe('project directory runtime', () => {
  /** validateDirectoryPath：realpath 归一与空值、非目录、不存在、无权限等错误分支。 */
  describe('validateDirectoryPath', () => {
    it('returns resolved real path for a valid directory', async () => {
      const runtime = createTestRuntime();
      const result = await runtime.validateDirectoryPath('/home/user/project');

      expect(result).toEqual({ ok: true, directory: '/home/user/project', requestedDirectory: '/home/user/project' });
    });

    it('resolves symlinks via fsPromises.realpath', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/path/to/project',
        },
      });

      const result = await runtime.validateDirectoryPath('/symlink/path/to/project');

      expect(result).toEqual({ ok: true, directory: '/real/path/to/project', requestedDirectory: '/symlink/path/to/project' });
    });

    it('returns error when candidate is empty', async () => {
      const runtime = createTestRuntime();
      const result = await runtime.validateDirectoryPath('');

      expect(result).toEqual({ ok: false, error: 'Directory parameter is required' });
    });

    it('returns error when candidate is not a string', async () => {
      const runtime = createTestRuntime();
      const result = await runtime.validateDirectoryPath(null);

      expect(result).toEqual({ ok: false, error: 'Directory parameter is required' });
    });

    it('returns error when path is not a directory', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => false }),
          realpath: async (p) => p,
        },
      });

      const result = await runtime.validateDirectoryPath('/some/file.txt');

      expect(result).toEqual({ ok: false, error: 'Specified path is not a directory' });
    });

    it('returns error when path does not exist', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => { throw { code: 'ENOENT' }; },
          realpath: async (p) => p,
        },
      });

      const result = await runtime.validateDirectoryPath('/nonexistent');

      expect(result).toEqual({ ok: false, error: 'Directory not found' });
    });

    it('returns error when access is denied', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => { throw { code: 'EACCES' }; },
          realpath: async (p) => p,
        },
      });

      const result = await runtime.validateDirectoryPath('/restricted');

      expect(result).toEqual({ ok: false, error: 'Access to directory denied' });
    });

    it('returns error when realpath fails after stat succeeds', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => { throw { code: 'ENOENT' }; },
        },
      });

      const result = await runtime.validateDirectoryPath('/deleted-after-stat');

      expect(result).toEqual({ ok: false, error: 'Directory not found' });
    });
  });

  /** resolveProjectDirectory：目录头域（含 URI 编码标记）、query 参数与设置中 lastDirectory / 活跃项目的优先级及 symlink 解析。 */
  describe('resolveProjectDirectory', () => {
    it('resolves symlinks in x-opencode-directory header', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/workspace/project',
        },
      });

      const req = {
        get: (header) => header === 'x-opencode-directory' ? '/home/user/workspace/project' : null,
        query: {},
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(result).toEqual({
        directory: '/real/workspace/project',
        requestedDirectory: '/home/user/workspace/project',
        error: null,
      });
    });

    it('decodes marked x-opencode-directory header values', async () => {
      const pathWithUnicode = '/home/user/测试项目';
      let validatedPath = null;
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async (p) => {
            validatedPath = p;
            return { isDirectory: () => true };
          },
          realpath: async (p) => p,
        },
      });

      const req = {
        get: (header) => {
          if (header === 'x-opencode-directory') return encodeURIComponent(pathWithUnicode);
          if (header === 'x-opencode-directory-encoding') return 'uri';
          return null;
        },
        query: {},
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(validatedPath).toBe(pathWithUnicode);
      expect(result).toEqual({ directory: pathWithUnicode, requestedDirectory: pathWithUnicode, error: null });
    });

    it('preserves raw percent sequences without directory encoding marker', async () => {
      const rawPath = '/home/user/foo%20bar';
      let validatedPath = null;
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async (p) => {
            validatedPath = p;
            return { isDirectory: () => true };
          },
          realpath: async (p) => p,
        },
      });

      const req = {
        get: (header) => header === 'x-opencode-directory' ? rawPath : null,
        query: {},
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(validatedPath).toBe(rawPath);
      expect(result).toEqual({ directory: rawPath, requestedDirectory: rawPath, error: null });
    });

    it('falls back to query directory when an unmarked encoded header is invalid', async () => {
      const validPath = '/home/user/workspace/project';
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async (p) => {
            if (p === validPath) return { isDirectory: () => true };
            throw { code: 'ENOENT' };
          },
          realpath: async (p) => p,
        },
      });

      const req = {
        get: (header) => header === 'x-opencode-directory' ? encodeURIComponent(validPath) : null,
        query: { directory: validPath },
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(result).toEqual({ directory: validPath, requestedDirectory: validPath, error: null });
    });

    it('resolves symlinks in query directory parameter', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/workspace/project',
        },
      });

      const req = {
        get: () => null,
        query: { directory: '/home/user/workspace/project' },
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(result).toEqual({
        directory: '/real/workspace/project',
        requestedDirectory: '/home/user/workspace/project',
        error: null,
      });
    });

    it('resolves symlinks in lastDirectory from settings', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/workspace/project',
        },
        getReadSettingsFromDiskMigrated: () => async () => ({
          lastDirectory: '/home/user/workspace/project',
        }),
      });

      const req = {
        get: () => null,
        query: {},
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(result).toEqual({
        directory: '/real/workspace/project',
        requestedDirectory: '/home/user/workspace/project',
        error: null,
      });
    });

    it('resolves symlinks in active project path from settings', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/workspace/project',
        },
        getReadSettingsFromDiskMigrated: () => async () => ({
          projects: [{ id: 'proj-1', path: '/home/user/workspace/project' }],
          activeProjectId: 'proj-1',
        }),
        sanitizeProjects: (input) => input,
      });

      const req = {
        get: () => null,
        query: {},
      };

      const result = await runtime.resolveProjectDirectory(req);

      expect(result).toEqual({
        directory: '/real/workspace/project',
        requestedDirectory: '/home/user/workspace/project',
        error: null,
      });
    });
  });

  /** resolveOptionalProjectDirectory：未显式指定目录时返回 null 而非报错，指定后行为与必选解析一致。 */
  describe('resolveOptionalProjectDirectory', () => {
    it('returns null directory when no directory is requested', async () => {
      const runtime = createTestRuntime();

      const req = {
        get: () => null,
        query: {},
      };

      const result = await runtime.resolveOptionalProjectDirectory(req);

      expect(result).toEqual({ directory: null, requestedDirectory: null, error: null });
    });

    it('resolves symlinks when directory is provided', async () => {
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async () => ({ isDirectory: () => true }),
          realpath: async () => '/real/workspace/project',
        },
      });

      const req = {
        get: (header) => header === 'x-opencode-directory' ? '/symlink/workspace/project' : null,
        query: {},
      };

      const result = await runtime.resolveOptionalProjectDirectory(req);

      expect(result).toEqual({
        directory: '/real/workspace/project',
        requestedDirectory: '/symlink/workspace/project',
        error: null,
      });
    });

    it('preserves raw percent sequences without directory encoding marker', async () => {
      const rawPath = '/optional/foo%25bar';
      let validatedPath = null;
      const runtime = createTestRuntime({
        fsPromises: {
          stat: async (p) => {
            validatedPath = p;
            return { isDirectory: () => true };
          },
          realpath: async (p) => p,
        },
      });

      const req = {
        get: (header) => header === 'x-opencode-directory' ? rawPath : null,
        query: {},
      };

      const result = await runtime.resolveOptionalProjectDirectory(req);

      expect(validatedPath).toBe(rawPath);
      expect(result).toEqual({ directory: rawPath, requestedDirectory: rawPath, error: null });
    });
  });
});
