/**
 * opencode plugin 路由（plugin-routes.js）的集成测试套件。
 *
 * 用 supertest 驱动真实 express app + 真实 plugins.js 实现：测试跑在
 * 临时目录沙箱里，OPENCODE_CONFIG 指向临时 user 配置以隔离真实环境。
 * 覆盖面：插件条目与源码文件的完整 CRUD（含 409/404/400 错误路径）、
 * npm registry 查询的各种结果分类（有更新/无更新/版本缺失/包缺失/
 * spec 畸形/网络失败/按包名去重/限流），以及延迟重启响应字段。
 * registry 相关用例通过注入 mock getNpmInfo 隔离真实网络。
 */
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, mock, test } from 'bun:test';
import express from 'express';
import fs from 'fs';
import os from 'os';
import path from 'path';
import request from 'supertest';

import { registerPluginRoutes } from './plugin-routes.js';

// 当前用例的项目临时目录（beforeEach 重建）。
let projectDir;
// user 作用域配置文件路径（OPENCODE_CONFIG 指向，beforeAll 设置）。
let userConfigPath;
// 整个套件的临时根目录（beforeAll 创建、afterAll 删除）。
let rootDir;
// 真实 plugins.js 模块命名空间（beforeAll 动态导入）。
let plugins;
// 注入路由的引擎刷新 mock（beforeEach 重建，断言“未被调用”）。
let refreshOpenCodeAfterConfigChange;
// 当前用例使用的 express app（helper 构建）。
let app;
// 需要在 afterEach 恢复权限的文件列表（chmod 0 用例登记）。
let cleanupPaths;

/** 以 root 身份运行时跳过用例的 test 包装：root 总能读到 chmod 0 的文件，“不可读路径”用例会失效。 */
const testUnlessRoot = typeof process.getuid === 'function' && process.getuid() === 0 ? test.skip : test;

/** 读取并 JSON.parse 指定文件，供断言直接检查落盘内容。 */
function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

/**
 * 构建挂好 plugin 路由的测试 express app：项目目录固定指向临时
 * projectDir，插件实现取自真实 plugins.js 模块，clientReloadDelayMs
 * 压到 25ms 加速测试；overrides 可覆盖任意依赖（如 mock getNpmInfo）。
 */
function createApp(overrides = {}) {
  const testApp = express();
  testApp.use(express.json());
  registerPluginRoutes(testApp, {
    resolveOptionalProjectDirectory: async () => ({ directory: projectDir, error: null }),
    refreshOpenCodeAfterConfigChange,
    clientReloadDelayMs: 25,
    listPluginEntries: plugins.listPluginEntries,
    getPluginEntry: plugins.getPluginEntry,
    createPluginEntry: plugins.createPluginEntry,
    updatePluginEntry: plugins.updatePluginEntry,
    deletePluginEntry: plugins.deletePluginEntry,
    listPluginDirFiles: plugins.listPluginDirFiles,
    readPluginDirFile: plugins.readPluginDirFile,
    writePluginDirFile: plugins.writePluginDirFile,
    deletePluginDirFile: plugins.deletePluginDirFile,
    encodePluginId: plugins.encodePluginId,
    decodePluginId: plugins.decodePluginId,
    ...overrides,
  });
  return testApp;
}

/** 用指定 getNpmInfo mock 构建 registry 测试 app 并设为全局 app。 */
function createRegistryApp(getNpmInfo) {
  app = createApp({ getNpmInfo });
  return app;
}

/** 辅助：POST 创建一个 user 作用域插件条目（默认 spec 为 “a”），预期 200。 */
async function createEntry(spec = 'a') {
  return request(app)
    .post('/api/config/plugins/entry')
    .send({ spec, scope: 'user' })
    .expect(200);
}

/** 辅助：POST 写入一个 user 作用域插件源码文件（默认 test.js），预期 200。 */
async function createFile(fileName = 'test.js', content = '//x') {
  return request(app)
    .post('/api/config/plugins/file')
    .send({ fileName, content, scope: 'user' })
    .expect(200);
}

/** 插件路由集成测试：registry 查询分类、条目/文件 CRUD 与错误码映射。 */
describe('opencode plugin routes', () => {
  // 建立测试根目录，把 OPENCODE_CONFIG 指向临时 user 配置并加载真实 plugins 模块。
  beforeAll(async () => {
    rootDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-plugin-routes-'));
    userConfigPath = path.join(rootDir, 'user-opencode.json');
    process.env.OPENCODE_CONFIG = userConfigPath;
    plugins = await import('./plugins.js');
  });

  // 每个用例获得独立的 projectDir、干净的 user 配置与 plugins 目录、新的 mock 与 app。
  beforeEach(() => {
    projectDir = fs.mkdtempSync(path.join(rootDir, 'project-'));
    fs.rmSync(userConfigPath, { force: true });
    fs.rmSync(path.join(rootDir, 'plugins'), { recursive: true, force: true });
    refreshOpenCodeAfterConfigChange = mock(async () => undefined);
    cleanupPaths = [];
    app = createApp();
  });

  // 恢复用例中 chmod 0 的文件权限，避免 afterAll 清理根目录时失败。
  afterEach(() => {
    for (const target of cleanupPaths) {
      try {
        fs.chmodSync(target, 0o600);
      } catch {
      }
    }
  });

  // 删除整个测试根目录并还原 OPENCODE_CONFIG 环境变量。
  afterAll(() => {
    fs.rmSync(rootDir, { recursive: true, force: true });
    delete process.env.OPENCODE_CONFIG;
  });

  test('GET /api/config/plugins empty returns entries and files arrays', async () => {
    const response = await request(app).get('/api/config/plugins').expect(200);

    expect(response.body).toEqual({ entries: [], files: [] });
  });

  test('GET /registry with empty specs returns empty results', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } }));
    createRegistryApp(getNpmInfo);

    const response = await request(app).get('/api/config/plugins/registry?specs=').expect(200);

    expect(response.body).toEqual({ results: [] });
    expect(getNpmInfo).not.toHaveBeenCalled();
  });

  test('GET /registry reports update for exact npm version behind latest', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '2.0.0', versions: ['1.0.0', '2.0.0'], distTags: { latest: '2.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo@1.0.0').expect(200);

    expect(response.body.results[0]).toMatchObject({
      kind: 'npm-ok',
      spec: 'foo@1.0.0',
      name: 'foo',
      currentVersion: '1.0.0',
      latestVersion: '2.0.0',
      hasUpdate: true,
    });
  });

  test('GET /registry reports no update when exact npm version matches latest', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo@1.0.0').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-ok', hasUpdate: false, latestVersion: '1.0.0', currentVersion: '1.0.0' });
  });

  test('GET /registry reports missing exact npm version', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '2.0.0', versions: ['1.0.0', '2.0.0'], distTags: { latest: '2.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo@99.99.99').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-missing-version', name: 'foo', currentVersion: '99.99.99', latestVersion: '2.0.0' });
  });

  test('GET /registry reports missing npm package', async () => {
    createRegistryApp(mock(async () => ({ ok: false, status: 404, error: 'Package not found' })));

    const response = await request(app).get('/api/config/plugins/registry?specs=nonexistent@1.0.0').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-missing-package', spec: 'nonexistent@1.0.0', name: 'nonexistent', error: 'Package not found' });
  });

  test('GET /registry reports malformed npm spec', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } }));
    createRegistryApp(getNpmInfo);

    const response = await request(app).get('/api/config/plugins/registry?specs=%40%40malformed').expect(200);

    expect(response.body.results[0]).toEqual({ kind: 'npm-malformed', spec: '@@malformed', error: 'Spec syntax is malformed' });
    expect(getNpmInfo).not.toHaveBeenCalled();
  });

  test('GET /registry reports existing path plugin ok', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } })));
    const tmpFile = path.join(fs.mkdtempSync(path.join(rootDir, 'plugin-path-')), 'plugin.js');
    fs.writeFileSync(tmpFile, '// plugin', 'utf8');

    const response = await request(app).get(`/api/config/plugins/registry?specs=${encodeURIComponent(tmpFile)}`).expect(200);

    expect(response.body.results[0]).toEqual({ kind: 'path-ok', spec: tmpFile, absolutePath: tmpFile });
  });

  test('GET /registry reports missing path plugin', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=%2Fnonexistent%2F__path%2Fxyz.js').expect(200);

    expect(response.body.results[0]).toEqual({ kind: 'path-missing', spec: '/nonexistent/__path/xyz.js', absolutePath: '/nonexistent/__path/xyz.js' });
  });

  test('GET /registry treats Windows absolute paths as local paths', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } }));
    createRegistryApp(getNpmInfo);

    const windowsPath = 'C:\\Users\\me\\plugin.js';
    const response = await request(app)
      .get(`/api/config/plugins/registry?specs=${encodeURIComponent(windowsPath)}`)
      .expect(200);

    expect(response.body.results[0]).toEqual({ kind: 'path-missing', spec: windowsPath, absolutePath: windowsPath });
    expect(getNpmInfo).not.toHaveBeenCalled();
  });

  testUnlessRoot('GET /registry reports unreadable path plugin', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } })));
    const tmpFile = path.join(fs.mkdtempSync(path.join(rootDir, 'plugin-unreadable-')), 'plugin.js');
    fs.writeFileSync(tmpFile, '// plugin', 'utf8');
    cleanupPaths.push(tmpFile);
    fs.chmodSync(tmpFile, 0);

    const response = await request(app).get(`/api/config/plugins/registry?specs=${encodeURIComponent(tmpFile)}`).expect(200);

    expect(response.body.results[0]).toEqual({ kind: 'path-unreadable', spec: tmpFile, absolutePath: tmpFile });
  });

  test('GET /registry reports npm network failure without failing route', async () => {
    createRegistryApp(mock(async () => ({ ok: false, status: 'network', error: 'socket closed' })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo@1.0.0').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-network', spec: 'foo@1.0.0', error: 'socket closed' });
  });

  test('GET /registry deduplicates npm package lookups by name', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '3.0.0', versions: ['1', '2', '3'], distTags: { latest: '3.0.0' } }));
    createRegistryApp(getNpmInfo);

    await request(app).get('/api/config/plugins/registry?specs=foo@1,foo@2,foo@3').expect(200);

    expect(getNpmInfo).toHaveBeenCalledTimes(1);
    expect(getNpmInfo).toHaveBeenCalledWith('foo', { forceRefresh: false });
  });

  test('GET /registry forwards refresh true to npm lookup', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } }));
    createRegistryApp(getNpmInfo);

    await request(app).get('/api/config/plugins/registry?specs=foo&refresh=true').expect(200);

    expect(getNpmInfo).toHaveBeenCalledWith('foo', { forceRefresh: true });
  });

  test('GET /registry rejects more than 100 unique specs', async () => {
    const specs = Array.from({ length: 101 }, (_, index) => `pkg-${index}`).join(',');

    const response = await request(app).get(`/api/config/plugins/registry?specs=${specs}`).expect(400);

    expect(response.body).toEqual({ error: 'too many specs' });
  });

  test('GET /registry reports bare npm name with null current version', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '2.0.0', versions: ['1.0.0', '2.0.0'], distTags: { latest: '2.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-ok', spec: 'foo', name: 'foo', currentVersion: null, hasUpdate: false });
  });

  test('GET /registry accepts non-exact npm range without missing-version noise', async () => {
    createRegistryApp(mock(async () => ({ ok: true, latest: '2.0.0', versions: ['1.0.0', '2.0.0'], distTags: { latest: '2.0.0' } })));

    const response = await request(app).get('/api/config/plugins/registry?specs=foo@%5E1.0').expect(200);

    expect(response.body.results[0]).toMatchObject({ kind: 'npm-ok', spec: 'foo@^1.0', name: 'foo', currentVersion: '^1.0', hasUpdate: false });
  });

  test('GET /registry supports scoped npm package specs', async () => {
    const getNpmInfo = mock(async () => ({ ok: true, latest: '1.0.0', versions: ['1.0.0'], distTags: { latest: '1.0.0' } }));
    createRegistryApp(getNpmInfo);

    const response = await request(app).get('/api/config/plugins/registry?specs=%40scope%2Ffoo%401.0.0').expect(200);

    expect(getNpmInfo).toHaveBeenCalledWith('@scope/foo', { forceRefresh: false });
    expect(response.body.results[0]).toMatchObject({ kind: 'npm-ok', spec: '@scope/foo@1.0.0', name: '@scope/foo' });
  });

  test('POST /entry creates entry and defers restart', async () => {
    const response = await createEntry('a');

    expect(response.body).toMatchObject({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin entry created. Restart the engine to apply.',
    });
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('GET after POST returns created entry', async () => {
    await createEntry('a');

    const response = await request(app).get('/api/config/plugins').expect(200);

    expect(response.body.entries).toEqual([expect.objectContaining({ spec: 'a', scope: 'user' })]);
  });

  test('POST duplicate entry returns 409', async () => {
    await createEntry('a');

    const response = await request(app)
      .post('/api/config/plugins/entry')
      .send({ spec: 'a', scope: 'user' })
      .expect(409);

    expect(response.body.error).toContain('already exists');
  });

  test('PATCH /entry/:id updates entry in same array index', async () => {
    await createEntry('a');
    const before = await request(app).get('/api/config/plugins').expect(200);
    const id = before.body.entries[0].id;

    const response = await request(app)
      .patch(`/api/config/plugins/entry/${encodeURIComponent(id)}`)
      .send({ spec: 'b' })
      .expect(200);

    expect(response.body.success).toBe(true);
    expect(response.body).toMatchObject({
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin entry updated. Restart the engine to apply.',
    });
    const after = await request(app).get('/api/config/plugins').expect(200);
    expect(after.body.entries[0]).toEqual(expect.objectContaining({ spec: 'b', scope: 'user' }));
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('DELETE /entry/:id removes entry and prunes plugin key', async () => {
    await createEntry('a');
    const listed = await request(app).get('/api/config/plugins').expect(200);
    const id = listed.body.entries[0].id;

    const response = await request(app).delete(`/api/config/plugins/entry/${encodeURIComponent(id)}`).expect(200);

    const after = await request(app).get('/api/config/plugins').expect(200);
    expect(after.body.entries).toEqual([]);
    expect(readJson(userConfigPath).plugin).toBeUndefined();
    expect(response.body).toMatchObject({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin entry deleted. Restart the engine to apply.',
    });
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('POST /file writes plugin dir file', async () => {
    const response = await createFile('test.js', '//x');

    expect(response.body).toMatchObject({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin file created. Restart the engine to apply.',
    });
    expect(fs.readFileSync(path.join(rootDir, 'plugins', 'test.js'), 'utf8')).toBe('//x');
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('POST duplicate file returns 409', async () => {
    await createFile('test.js', '//x');

    const response = await request(app)
      .post('/api/config/plugins/file')
      .send({ fileName: 'test.js', content: '//again', scope: 'user' })
      .expect(409);

    expect(response.body.error).toContain('already exists');
  });

  test('PUT /file/:id updates file content', async () => {
    await createFile('test.js', '//x');
    const listed = await request(app).get('/api/config/plugins').expect(200);
    const id = listed.body.files[0].id;

    const response = await request(app)
      .put(`/api/config/plugins/file/${encodeURIComponent(id)}`)
      .send({ content: '//y' })
      .expect(200);

    expect(fs.readFileSync(path.join(rootDir, 'plugins', 'test.js'), 'utf8')).toBe('//y');
    expect(response.body).toMatchObject({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin file updated. Restart the engine to apply.',
    });
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('DELETE /file/:id unlinks file', async () => {
    await createFile('test.js', '//x');
    const listed = await request(app).get('/api/config/plugins').expect(200);
    const id = listed.body.files[0].id;

    const response = await request(app).delete(`/api/config/plugins/file/${encodeURIComponent(id)}`).expect(200);

    expect(fs.existsSync(path.join(rootDir, 'plugins', 'test.js'))).toBe(false);
    expect(response.body).toMatchObject({
      success: true,
      requiresReload: false,
      requiresRestart: true,
      restartDeferred: true,
      message: 'Plugin file deleted. Restart the engine to apply.',
    });
    expect(refreshOpenCodeAfterConfigChange).not.toHaveBeenCalled();
  });

  test('PATCH unknown entry id returns 404', async () => {
    const id = plugins.encodePluginId('config', 'user:missing');

    const response = await request(app)
      .patch(`/api/config/plugins/entry/${encodeURIComponent(id)}`)
      .send({ spec: 'b' })
      .expect(404);

    expect(response.body.error).toContain('not found');
  });

  test('POST invalid fileName returns 400', async () => {
    const response = await request(app)
      .post('/api/config/plugins/file')
      .send({ fileName: '../escape.js', content: '//x', scope: 'user' })
      .expect(400);

    expect(response.body.error).toContain('Plugin file name');
  });
});
