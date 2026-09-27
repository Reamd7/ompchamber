/**
 * 会话文件夹快照路由测试套件：验证持久化安全——并发保存使用唯一临时
 * 文件、rename 失败时清理临时文件、磁盘文件缺失不被当作权威空快照、
 * 磁盘状态畸形 / 结构非法时返回 500 而非清空浏览器端有效状态，以及
 * updatedAt 单调性（旧快照与同版本异数据晚到都被忽略）。
 */
import { describe, expect, it, vi } from 'vitest';
import path from 'path';

import { registerSessionFoldersRoutes } from './routes.js';

/** 用 Map 模拟 express app，只登记 get / post 注册的处理器，便于按方法与路径直接调用。 */
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
    },
    getRoute(method, routePath) {
      return routes.get(`${method} ${routePath}`);
    },
  };
};

/** 最小 mock res：记录 status / json 调用，通过 getter 暴露 statusCode 与 body。 */
const createMockResponse = () => {
  let statusCode = 200;
  let body = null;

  return {
    status(code) {
      statusCode = code;
      return this;
    },
    json(payload) {
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

/** 总是抛 ENOENT 错误的 readFile 替身，模拟磁盘上还没有快照文件。 */
const missingFile = async () => {
  const error = new Error('missing');
  error.code = 'ENOENT';
  throw error;
};

/** 构造一份结构合法的快照载荷，仅 updatedAt 可变。 */
const folderPayload = (updatedAt) => ({
  version: 1,
  foldersMap: {},
  collapsedFolderIds: [],
  updatedAt,
});

/** 快照读写、原子落盘与损坏 / 并发场景的行为验证。 */
describe('session folders routes', () => {
  it('uses unique temp files for concurrent saves', async () => {
    const { app, getRoute } = createRouteRegistry();
    const tempPaths = [];
    const fsPromises = {
      readFile: vi.fn(missingFile),
      mkdir: vi.fn(async () => {}),
      writeFile: vi.fn(async (tempPath) => {
        tempPaths.push(tempPath);
        await new Promise((resolve) => setTimeout(resolve, 0));
      }),
      rename: vi.fn(async () => {}),
    };

    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const handler = getRoute('POST', '/api/session-folders');

    await Promise.all([
      handler({ body: folderPayload(1) }, createMockResponse()),
      handler({ body: folderPayload(2) }, createMockResponse()),
    ]);

    expect(tempPaths).toHaveLength(2);
    expect(new Set(tempPaths).size).toBe(2);
    expect(tempPaths.every((tempPath) => tempPath.includes('sessions-directories.json.tmp-'))).toBe(true);
  });

  it('removes the temp file when rename fails', async () => {
    const { app, getRoute } = createRouteRegistry();
    const fsPromises = {
      readFile: vi.fn(missingFile),
      mkdir: vi.fn(async () => {}),
      writeFile: vi.fn(async () => {}),
      rename: vi.fn(async () => {
        throw new Error('rename failed');
      }),
      unlink: vi.fn(async () => {}),
    };

    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const handler = getRoute('POST', '/api/session-folders');
    const response = createMockResponse();

    await handler({ body: folderPayload(1) }, response);

    expect(response.statusCode).toBe(500);
    expect(fsPromises.unlink).toHaveBeenCalledWith(expect.stringContaining('sessions-directories.json.tmp-'));
  });

  it('does not present a missing disk file as an authoritative empty snapshot', async () => {
    const { app, getRoute } = createRouteRegistry();
    const fsPromises = { readFile: vi.fn(missingFile) };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    await getRoute('GET', '/api/session-folders')({}, response);

    expect(response.statusCode).toBe(200);
    expect(response.body).toEqual({ version: 1, exists: false });
  });

  it('rejects malformed disk state instead of clearing valid browser state', async () => {
    const { app, getRoute } = createRouteRegistry();
    const fsPromises = { readFile: vi.fn(async () => '{broken') };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    await getRoute('GET', '/api/session-folders')({}, response);

    expect(response.statusCode).toBe(500);
  });

  it('rejects structurally invalid folder entries from disk', async () => {
    const { app, getRoute } = createRouteRegistry();
    const malformedPayload = {
      ...folderPayload(10),
      foldersMap: { project: [{ id: 'folder', name: 'Folder', sessionIds: 'session-1', createdAt: 1 }] },
    };
    const fsPromises = { readFile: vi.fn(async () => JSON.stringify(malformedPayload)) };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    await getRoute('GET', '/api/session-folders')({}, response);

    expect(response.statusCode).toBe(500);
    expect(response.body).toEqual({ error: 'Stored session folders have an invalid shape' });
  });

  it('keeps the newest folder snapshot when an older write arrives later', async () => {
    const { app, getRoute } = createRouteRegistry();
    let persisted = JSON.stringify(folderPayload(20));
    const fsPromises = {
      readFile: vi.fn(async () => persisted),
      mkdir: vi.fn(async () => {}),
      writeFile: vi.fn(async (_tempPath, value) => {
        persisted = value;
      }),
      rename: vi.fn(async () => {}),
      unlink: vi.fn(async () => {}),
    };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    await getRoute('POST', '/api/session-folders')({ body: folderPayload(10) }, response);

    expect(response.body).toEqual({ success: true, ignored: true });
    expect(fsPromises.writeFile).not.toHaveBeenCalled();
  });

  it('keeps the existing folder snapshot when a duplicate revision has different data', async () => {
    const { app, getRoute } = createRouteRegistry();
    const persistedPayload = { ...folderPayload(20), foldersMap: { existing: [] } };
    const fsPromises = {
      readFile: vi.fn(async () => JSON.stringify(persistedPayload)),
      mkdir: vi.fn(async () => {}),
      writeFile: vi.fn(async () => {}),
      rename: vi.fn(async () => {}),
      unlink: vi.fn(async () => {}),
    };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    const duplicateRevision = { ...folderPayload(20), foldersMap: { replacement: [] } };
    await getRoute('POST', '/api/session-folders')({ body: duplicateRevision }, response);

    expect(response.body).toEqual({ success: true, ignored: true });
    expect(fsPromises.writeFile).not.toHaveBeenCalled();
  });

  it('allows a valid snapshot to repair structurally invalid prior state', async () => {
    const { app, getRoute } = createRouteRegistry();
    const fsPromises = {
      readFile: vi.fn(async () => JSON.stringify({ version: 1, updatedAt: 999, foldersMap: null })),
      mkdir: vi.fn(async () => {}),
      writeFile: vi.fn(async () => {}),
      rename: vi.fn(async () => {}),
      unlink: vi.fn(async () => {}),
    };
    registerSessionFoldersRoutes(app, {
      fsPromises,
      path,
      ompchamberDataDir: '/tmp/ompchamber-test',
    });

    const response = createMockResponse();
    await getRoute('POST', '/api/session-folders')({ body: folderPayload(10) }, response);

    expect(response.body).toEqual({ success: true });
    expect(fsPromises.writeFile).toHaveBeenCalledTimes(1);
  });
});
