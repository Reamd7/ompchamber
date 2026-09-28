/**
 * 本地 SSE 路由的单元测试（bun:test）。
 *
 * 不启动真实 server：用自制的路由注册表与 req/res mock 直接调用
 * registerNotificationRoutes / registerScheduledTaskRoutes 注册的处理函数，
 * 断言 SSE 响应头对 nginx 反代安全（no-buffering、no-transform）、
 * 心跳定时器与客户端连接集合的生命周期（ready 事件、心跳写入、
 * 连接关闭后自动移除）。
 */
import { describe, expect, it, vi } from 'bun:test';

import { NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS, registerNotificationRoutes } from './lib/notifications/routes.js';
import { registerScheduledTaskRoutes } from './lib/scheduled-tasks/routes.js';

/**
 * 构造一个最小 express 替身：把 get/post/put/patch/delete 注册的
 * handler 存入 Map，getRoute(method, path) 供测试直接取出调用。
 */
const createRouteRegistry = () => {
  const routes = new Map();

  return {
    app: {
      get(path, handler) {
        routes.set(`GET ${path}`, handler);
      },
      post(path, handler) {
        routes.set(`POST ${path}`, handler);
      },
      put(path, handler) {
        routes.set(`PUT ${path}`, handler);
      },
      patch(path, handler) {
        routes.set(`PATCH ${path}`, handler);
      },
      delete(path, handler) {
        routes.set(`DELETE ${path}`, handler);
      },
    },
    getRoute(method, path) {
      return routes.get(`${method} ${path}`);
    },
  };
};

/**
 * 构造最小 IncomingMessage mock：headers 为空对象，on/emit 维护一个
 * 事件监听表，用于模拟请求 'close' 等事件以触发 SSE 清理逻辑。
 */
const createMockRequest = () => {
  const listeners = new Map();

  return {
    headers: {},
    on(event, handler) {
      listeners.set(event, handler);
      return this;
    },
    emit(event) {
      const handler = listeners.get(event);
      if (typeof handler === 'function') {
        handler();
      }
    },
  };
};

/**
 * 构造最小 ServerResponse mock：记录 setHeader 的 header（小写键）、
 * write 累积的 body、status/json 调用、flushHeaders/flush 次数，
 * 并以 getter 暴露给断言；on/emit 用于模拟 'error'/'close' 清理路径。
 */
const createMockResponse = () => {
  const headers = new Map();
  const listeners = new Map();
  let statusCode = 200;
  let body = '';
  let flushed = false;
  let bodyFlushCount = 0;

  return {
    on(event, handler) {
      listeners.set(event, handler);
      return this;
    },
    emit(event) {
      const handler = listeners.get(event);
      if (typeof handler === 'function') {
        handler();
      }
    },
    setHeader(name, value) {
      headers.set(name.toLowerCase(), value);
    },
    getHeader(name) {
      return headers.get(name.toLowerCase());
    },
    flushHeaders() {
      flushed = true;
    },
    flush() {
      bodyFlushCount += 1;
    },
    write(chunk) {
      body += String(chunk);
      return true;
    },
    status(code) {
      statusCode = code;
      return this;
    },
    json(payload) {
      body += JSON.stringify(payload);
      return this;
    },
    get statusCode() {
      return statusCode;
    },
    get body() {
      return body;
    },
    get flushed() {
      return flushed;
    },
    get bodyFlushCount() {
      return bodyFlushCount;
    },
  };
};

/** 验证本地 SSE 端点（通知流、OMPChamber 事件流）的响应头与连接生命周期。 */
describe('local SSE routes', () => {
  it('serves notification SSE with nginx-safe headers', async () => {
    vi.useFakeTimers();
    const { app, getRoute } = createRouteRegistry();
    const clients = new Set();

    try {
      registerNotificationRoutes(app, {
        uiAuthController: {
          ensureSessionToken: async () => 'ui-token',
        },
        getUiSessionTokenFromRequest: () => 'ui-token',
        getUiNotificationClients: () => clients,
        writeSseEvent(res, payload) {
          res.write(`data: ${JSON.stringify(payload)}\n\n`);
        },
      });

      const handler = getRoute('GET', '/api/notifications/stream');
      const req = createMockRequest();
      const res = createMockResponse();

      await handler(req, res);

      expect(res.statusCode).toBe(200);
      expect(res.getHeader('content-type')).toContain('text/event-stream');
      expect(res.getHeader('cache-control')).toBe('no-cache, no-transform');
      expect(res.getHeader('connection')).toBe('keep-alive');
      expect(res.getHeader('x-accel-buffering')).toBe('no');
      expect(res.flushed).toBe(true);
      expect(res.body).toContain('ompchamber:notification-stream-ready');
      expect(clients.has(res)).toBe(true);
      expect(vi.getTimerCount()).toBe(1);
      expect(res.bodyFlushCount).toBe(1);

      vi.advanceTimersByTime(NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS);
      expect(res.body).toContain(':heartbeat\n\n');
      expect(res.bodyFlushCount).toBe(2);

      res.emit('error');
      expect(clients.has(res)).toBe(false);
      expect(vi.getTimerCount()).toBe(0);

      const bodyAfterClose = res.body;
      vi.advanceTimersByTime(NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS);
      expect(res.body).toBe(bodyAfterClose);
    } finally {
      vi.useRealTimers();
    }
  });

  it('serves OMPChamber SSE with nginx-safe headers', () => {
    const { app, getRoute } = createRouteRegistry();
    const clients = new Set();

    registerScheduledTaskRoutes(app, {
      getOMPChamberEventClients: () => clients,
      writeSseEvent(res, payload) {
        res.write(`data: ${JSON.stringify(payload)}\n\n`);
      },
    });

    const handler = getRoute('GET', '/api/ompchamber/events');
    const req = createMockRequest();
    const res = createMockResponse();

    handler(req, res);

    expect(res.statusCode).toBe(200);
    expect(res.getHeader('content-type')).toContain('text/event-stream');
    expect(res.getHeader('cache-control')).toBe('no-cache, no-transform');
    expect(res.getHeader('connection')).toBe('keep-alive');
    expect(res.getHeader('x-accel-buffering')).toBe('no');
    expect(res.flushed).toBe(true);
    expect(res.body).toContain('ompchamber:event-stream-ready');
    expect(clients.has(res)).toBe(true);

    req.emit('close');
    expect(clients.has(res)).toBe(false);
  });
});
