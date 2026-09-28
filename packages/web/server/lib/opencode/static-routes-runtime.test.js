/**
 * static-routes-runtime 的 API-only 模式回退路由测试。
 *
 * 覆盖：浏览器 UI 路由返回包含 connect-url 引导命令的 HTML 回退页；
 * JSON 客户端得到 { ok, mode: 'api-only' } 说明；API、认证与健康检查
 * 路由不被回退逻辑拦截。依赖全部使用最小 stub（无磁盘 UI 资源场景）。
 */
import { describe, expect, it } from 'bun:test';
import express from 'express';
import request from 'supertest';
import { createStaticRoutesRuntime } from './static-routes-runtime.js';

/** 以最小 stub 依赖构造被测的静态路由运行时（existsSync 恒为 false）。 */
const createRuntime = () => createStaticRoutesRuntime({
  fs: { existsSync: () => false },
  path: { join: (...parts) => parts.join('/'), resolve: (value) => value, sep: '/' },
  process: { env: {} },
  __dirname: '/server',
  express,
  resolveProjectDirectory: () => '',
  buildOpenCodeUrl: () => '',
  getOpenCodeAuthHeaders: () => ({}),
  readSettingsFromDiskMigrated: async () => ({}),
  normalizePwaAppName: (value) => value,
  normalizePwaOrientation: (value) => value,
});

// API-only 模式下回退路由的内容与旁路行为
describe('static routes runtime', () => {
  it('returns API-only HTML fallback for browser UI routes', async () => {
    const app = express();
    createRuntime().registerApiOnlyFallbackRoutes(app);

    const response = await request(app).get('/sessions/abc').set('Accept', 'text/html');

    expect(response.status).toBe(200);
    expect(response.text).toContain('OMPChamber is running in headless mode');
    expect(response.text).toContain('Open it from the OMPChamber desktop or mobile app');
    expect(response.text).toContain('ompchamber connect-url --help');
    expect(response.text).toContain('Copy command');
  });

  it('returns API-only info JSON for JSON clients', async () => {
    const app = express();
    createRuntime().registerApiOnlyFallbackRoutes(app);

    const response = await request(app).get('/sessions/abc').set('Accept', 'application/json');

    expect(response.status).toBe(200);
    expect(response.body).toEqual({
      ok: true,
      mode: 'api-only',
      message: 'OMPChamber is running in API-only mode',
    });
  });

  it('does not intercept API, auth, or health routes in API-only mode', async () => {
    const app = express();
    createRuntime().registerApiOnlyFallbackRoutes(app);

    const api = await request(app).get('/api/version');
    const auth = await request(app).get('/auth/session');
    const health = await request(app).get('/health');

    expect(api.body).not.toEqual({ ok: true, mode: 'api-only', message: 'OMPChamber is running in API-only mode' });
    expect(auth.body).not.toEqual({ ok: true, mode: 'api-only', message: 'OMPChamber is running in API-only mode' });
    expect(health.body).not.toEqual({ ok: true, mode: 'api-only', message: 'OMPChamber is running in API-only mode' });
  });
});
