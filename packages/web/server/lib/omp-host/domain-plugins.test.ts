/**
 * OMP plugin 域测试：不透明 id 的编解码、capability 门控（关闭时不触
 * list 源）、manifest 扩展投影（加载态与项目覆盖权限）、目录作用域的
 * 列表透传，以及 revealCommand 的三平台文件管理器定位命令。
 */
import { describe, expect, test } from 'bun:test';
import fs from 'node:fs';
import path from 'node:path';
import {
  revealCommand,
  decodePluginId,
  encodePluginId,
  projectExtension,
  projectPlugin,
  registerPluginsDomainRoutes,
} from './domain-plugins.ts';
import type { PluginListResult } from './domain-plugins.ts';

/** 构造捕获 (method, pattern) → handler 的路由表替身。 */
const makeRoute = () => {
  const handlers = new Map<string, unknown>();
  const route = (method: string, pattern: string, handler: import('./domain-plugins.ts').DomainRouteHandler) => handlers.set(`${method} ${pattern}`, handler);
  return { handlers, route };
};

/** 按 URL（可带 init）构造最小 Request。 */
const request = (url: string, init?: RequestInit) => new Request(url, init);

/** 插件域主套件：id 编解码、501 门控、投影与目录作用域列表。 */
describe('OMP plugin domain', () => {
  test('uses opaque ids that preserve plugin kind, scope, and name', () => {
    const id = encodePluginId('npm', 'user', '@scope/example');
    expect(decodePluginId(id)).toEqual({ kind: 'npm', scope: 'user', name: '@scope/example' });
  });


  test('capability off does not call the list source', async () => {
    const { handlers, route } = makeRoute();
    let calls = 0;
    registerPluginsDomainRoutes(route, {
      features: { 'plugins.v1': false },
      list: async () => {
        calls += 1;
        // Capability gate answers before the list source runs; the typed
        // empty result is never read (asserted below: calls === 0).
        return { plugins: [], extensions: [] };
      },
    });
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const response = await (handlers.get('GET /omp/plugins') as (request: Request) => Promise<Response>)(request('http://host/omp/plugins'));
    expect(response.status).toBe(501);
    expect(await response.json()).toEqual({ error: 'plugins.v1-unavailable' });
    expect(calls).toBe(0);
  });
  test('projects manifest extension entries with loaded state and project override permissions', () => {
    const plugin = {
      name: 'example',
      version: '1.0.0',
      scope: 'project' as const,
      enabled: true,
      manifest: { extensions: ['extensions/index.ts'] },
    };
    const projected = projectPlugin(plugin);
    expect(projected).toMatchObject({ kind: 'npm', scope: 'project', editable: true });
    expect(projected.permissions).toEqual({ toggle: true, features: true, settings: true, uninstall: false });
    const entry = projectExtension({
      filePath: '/repo/.omp/plugins/example/extensions/index.ts',
      scope: 'project',
      source: 'plugin-manifest',
      editable: false,
      pluginId: projected.id,
      pluginName: projected.name,
      declaredEntry: 'extensions/index.ts',
      loaded: true,
    });
    expect(entry).toMatchObject({ source: 'plugin-manifest', editable: false, loaded: true, pluginName: 'example' });
  });
  test('lists OMP plugins under the explicit directory scope', async () => {
    const { handlers, route } = makeRoute();
    const directories: string[] = [];
    registerPluginsDomainRoutes(route, {
      features: { 'plugins.v1': true },
      list: async (directory) => {
        directories.push(directory);
        // SAFETY: the route serializes deps.list's result verbatim, so the
        // fake models exactly the asserted wire subset — bridge it to the
        // ProjectedPlugin contract the route type expects.
        return {
          plugins: [{ id: 'plugin-id', kind: 'npm', scope: 'user', name: 'example', version: '1.0.0', enabled: true }] as PluginListResult['plugins'],
          extensions: [],
        };
      },
    });
    // SAFETY: test fixture narrowing — the asserted shape is the harness contract this test reads.
    const response = await (handlers.get('GET /omp/plugins') as (request: Request) => Promise<Response>)(
      request('http://host/omp/plugins?directory=%2Frepo'),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      plugins: [{ id: 'plugin-id', kind: 'npm', scope: 'user', name: 'example', version: '1.0.0', enabled: true }],
      extensions: [],
    });
    expect(directories).toEqual(['/repo']);
  });
});

/** revealCommand：darwin/linux/win32 三平台的「在文件管理器中显示」。 */
describe('revealCommand', () => {
  test('darwin uses open -R for selection', () => {
    expect(revealCommand('darwin', '/repo/plug/index.ts')).toEqual({
      command: 'open',
      args: ['-R', '/repo/plug/index.ts'],
    });
  });

  test('linux opens parent directory only', () => {
    expect(revealCommand('linux', __filename)).toEqual({
      command: 'xdg-open',
      args: [import.meta.dir],
    });
  });

  test('win32 selects files via explorer /select,', () => {
    const file = path.join(__dirname, 'fixtures', 'stub.ts');
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, '');
    expect(revealCommand('win32', file)).toEqual({
      command: 'explorer',
      args: [`/select,${file}`],
    });
    expect(revealCommand('win32', import.meta.dir)).toEqual({
      command: 'explorer',
      args: [import.meta.dir],
    });
  });
});
