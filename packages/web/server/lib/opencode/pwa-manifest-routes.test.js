/**
 * PWA manifest 路由测试：验证 /manifest.webmanifest 快捷方式（shortcuts）
 * 的作用域过滤——目录作用域的 manifest 不回退到无关的全局会话；根作用域
 * 的 manifest 会包含子会话快捷方式。通过替换 globalThis.fetch 模拟
 * OpenCode 的 /session 列表，用手写 response 对象捕获输出。
 */
import { describe, expect, it } from 'vitest';
import { registerPwaManifestRoute } from './pwa-manifest-routes.js';

/** 极简 response 桩：记录 header / type / body 并支持链式调用。 */
const createResponse = () => ({
  headers: new Map(),
  contentType: '',
  body: '',
  setHeader(name, value) {
    this.headers.set(name, value);
    return this;
  },
  type(value) {
    this.contentType = value;
    return this;
  },
  send(value) {
    this.body = value;
    return this;
  },
});

// PWA manifest 路由主套件。
describe('PWA manifest route', () => {
  it('does not fall back to unrelated global session shortcuts for scoped manifests', async () => {
    const routes = new Map();
    const app = {
      get(route, handler) {
        routes.set(route, handler);
      },
    };
    const originalFetch = globalThis.fetch;
    const fetchCalls = [];
    globalThis.fetch = async (url) => {
      fetchCalls.push(String(url));
      const sessions = String(url).includes('?directory=')
        ? []
        : [
            {
              id: 'other-session',
              title: 'Other project',
              directory: '/workspace/other',
              time: { updated: 2 },
            },
          ];
      return {
        ok: true,
        json: async () => sessions,
      };
    };

    try {
      registerPwaManifestRoute(app, {
        process: { platform: 'darwin' },
        resolveProjectDirectory: async () => ({ directory: '/workspace/app' }),
        buildOpenCodeUrl: (route) => route,
        getOpenCodeAuthHeaders: () => ({}),
        readSettingsFromDiskMigrated: async () => ({}),
        normalizePwaAppName: (value, fallback) => typeof value === 'string' && value.trim() ? value.trim() : fallback,
        normalizePwaOrientation: (value, fallback) => typeof value === 'string' && value.trim() ? value.trim() : fallback,
      });

      const handler = routes.get('/manifest.webmanifest');
      const res = createResponse();
      await handler({ query: {} }, res);

      const manifest = JSON.parse(res.body);
      expect(fetchCalls).toHaveLength(2);
      expect(manifest.shortcuts).toEqual([
        {
          name: 'Appearance Settings',
          short_name: 'Settings',
          description: 'Open appearance settings',
          url: '/?settings=appearance',
          icons: [{ src: '/pwa-192.png', sizes: '192x192', type: 'image/png' }],
        },
      ]);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('includes child session shortcuts for root-scoped manifests', async () => {
    const routes = new Map();
    const app = {
      get(route, handler) {
        routes.set(route, handler);
      },
    };
    const originalFetch = globalThis.fetch;
    const fetchCalls = [];
    globalThis.fetch = async (url) => {
      fetchCalls.push(String(url));
      return {
        ok: true,
        json: async () => [
          {
            id: 'root-child',
            title: 'Root child',
            directory: '/workspace/app',
            time: { updated: 2 },
          },
        ],
      };
    };

    try {
      registerPwaManifestRoute(app, {
        process: { platform: 'darwin' },
        resolveProjectDirectory: async () => ({ directory: '/' }),
        buildOpenCodeUrl: (route) => route,
        getOpenCodeAuthHeaders: () => ({}),
        readSettingsFromDiskMigrated: async () => ({}),
        normalizePwaAppName: (value, fallback) => typeof value === 'string' && value.trim() ? value.trim() : fallback,
        normalizePwaOrientation: (value, fallback) => typeof value === 'string' && value.trim() ? value.trim() : fallback,
      });

      const handler = routes.get('/manifest.webmanifest');
      const res = createResponse();
      await handler({ query: {} }, res);

      const manifest = JSON.parse(res.body);
      expect(fetchCalls).toEqual(['/session?directory=%2F']);
      expect(manifest.shortcuts).toContainEqual({
        name: 'Root child',
        short_name: 'Root child',
        description: 'Open recent session',
        url: '/?session=root-child',
        icons: [{ src: '/pwa-192.png', sizes: '192x192', type: 'image/png' }],
      });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
