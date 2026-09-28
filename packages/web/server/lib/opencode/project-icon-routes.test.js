/**
 * 项目图标路由测试：当项目元数据指向的图标文件缺失时，GET
 * /api/projects/:projectId/icon 应回退到磁盘上实际存在的文件并按其扩展
 * 名推导 MIME（元数据写 png、实际是 jpg 时返回 image/jpeg）。
 */
import { describe, expect, it, vi } from 'vitest';
import crypto from 'crypto';
import path from 'path';

import { registerProjectIconRoutes } from './project-icon-routes.js';

/** 构造记录型 app 桩：按 "METHOD /path" 保存路由处理器，便于直接调用。 */
const createRouteRegistry = () => {
  const routes = new Map();

  return {
    app: {
      get(routePath, handler) {
        routes.set(`GET ${routePath}`, handler);
      },
      post(routePath, handler) {
        routes.set(`POST ${routePath}`, handler);
      },
      put(routePath, handler) {
        routes.set(`PUT ${routePath}`, handler);
      },
      delete(routePath, handler) {
        routes.set(`DELETE ${routePath}`, handler);
      },
    },
    getRoute(method, routePath) {
      return routes.get(`${method} ${routePath}`);
    },
  };
};

/** 构造链式 response 桩：记录状态码、header（键统一小写）与响应体。 */
const createMockResponse = () => {
  const headers = new Map();
  let statusCode = 200;
  let body = null;

  return {
    setHeader(name, value) {
      headers.set(name.toLowerCase(), value);
    },
    getHeader(name) {
      return headers.get(name.toLowerCase());
    },
    status(code) {
      statusCode = code;
      return this;
    },
    json(payload) {
      body = payload;
      return this;
    },
    send(payload) {
      body = payload;
      return this;
    },
    get statusCode() {
      return statusCode;
    },
    get body() {
      return body;
    },
  };
};

// 项目图标路由主套件。
describe('project icon routes', () => {
  it('uses fallback file extension MIME when metadata points to a missing icon', async () => {
    const { app, getRoute } = createRouteRegistry();
    const jpgBytes = Buffer.from('jpg-bytes');
    const enoent = Object.assign(new Error('missing'), { code: 'ENOENT' });
    const fsPromises = {
      readFile: vi.fn(async (iconPath) => {
        if (iconPath.endsWith('.jpg')) {
          return jpgBytes;
        }
        throw enoent;
      }),
    };

    registerProjectIconRoutes(app, {
      fsPromises,
      path,
      crypto,
      ompchamberDataDir: '/tmp/ompchamber-test',
      sanitizeProjects: (projects) => projects,
      readSettingsFromDiskMigrated: async () => ({
        projects: [{
          id: 'proj-1',
          path: '/repo',
          iconImage: { mime: 'image/png', updatedAt: 1, source: 'custom' },
        }],
      }),
      persistSettings: async () => ({}),
      createFsSearchRuntime: () => ({ searchFilesystemFiles: async () => [] }),
      spawn: vi.fn(),
      resolveGitBinaryForSpawn: vi.fn(),
    });

    const res = createMockResponse();
    await getRoute('GET', '/api/projects/:projectId/icon')({
      params: { projectId: 'proj-1' },
      query: {},
    }, res);

    expect(res.statusCode).toBe(200);
    expect(res.getHeader('Content-Type')).toBe('image/jpeg');
    expect(res.body).toBe(jpgBytes);
  });
});
