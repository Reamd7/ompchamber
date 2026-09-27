/**
 * realtime-proxy 的集成测试套件（bun:test）。每个用例自起真实的
 * express/http/ws 上游与代理服务器，afterEach 统一关闭，覆盖：SSE/WS 代理
 * URL 构造；SSE 流式转发与安全头透传；未认证 401、origin 受限 403、目标越界
 * 与路径越界 404 的拒绝路径；WS upgrade 的查询参数透传与免密首次连接。
 */
import { afterEach, describe, expect, it } from 'bun:test';
import express from 'express';
import http from 'node:http';
import { WebSocket, WebSocketServer } from 'ws';

import { attachRealtimeProxy, buildRealtimeProxySseUrl, buildRealtimeProxyWsUrl } from './realtime-proxy.js';
import { createUiAuth } from './ui-auth/ui-auth.js';

/** 本套件启动的全部 http server，afterEach 统一关闭以防句柄泄漏。 */
const servers = [];

/** 在 127.0.0.1 随机端口监听 server，登记进 servers 并返回其 http origin。 */
const listen = async (server) => {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  servers.push(server);
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('Expected TCP server address');
  return `http://127.0.0.1:${address.port}`;
};

/** 等待 server 完全关闭的 Promise 封装。 */
const closeServer = async (server) => {
  await new Promise((resolve) => server.close(() => resolve()));
};

/** 起一个挂好实时代理的本地 server：注入固定的 X-Proxy-Auth 头，authToken 为 null 可模拟未认证，originAllowed 可模拟 origin 拒绝。 */
const startProxyServer = async ({ apiBaseUrl, authToken = 'ui-token', originAllowed = true } = {}) => {
  const app = express();
  const server = http.createServer(app);
  const runtime = attachRealtimeProxy({
    app,
    server,
    getDesktopRuntimeConfig: () => ({
      apiBaseUrl,
      requestHeaders: { 'X-Proxy-Auth': 'secret' },
    }),
    getUiAuthController: () => ({
      ensureSessionToken: async () => authToken,
    }),
    isRequestOriginAllowed: async () => originAllowed,
  });
  const origin = await listen(server);
  return { origin, runtime };
};

/** 同 startProxyServer，但认证改用真实 createUiAuth 控制器（覆盖免密首连场景）。 */
const startProxyServerWithAuthController = async ({ apiBaseUrl, uiAuthController, originAllowed = true } = {}) => {
  const app = express();
  const server = http.createServer(app);
  const runtime = attachRealtimeProxy({
    app,
    server,
    getDesktopRuntimeConfig: () => ({
      apiBaseUrl,
      requestHeaders: { 'X-Proxy-Auth': 'secret' },
    }),
    getUiAuthController: () => uiAuthController,
    isRequestOriginAllowed: async () => originAllowed,
  });
  const origin = await listen(server);
  return { origin, runtime };
};

/** 起一个记录请求的 SSE 上游：命中指定 path 时回放两帧 data 事件，其余路径 404。 */
const startSseUpstream = async ({ path = '/api/global/event' } = {}) => {
  const requests = [];
  const server = http.createServer((req, res) => {
    requests.push({ url: req.url, headers: req.headers });
    if (new URL(req.url || '/', 'http://127.0.0.1').pathname !== path) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
    });
    res.write('data: first\n\n');
    res.end('data: second\n\n');
  });
  const origin = await listen(server);
  return { origin, requests };
};

// 关闭本套件登记的所有服务器。
afterEach(async () => {
  while (servers.length > 0) {
    const server = servers.pop();
    await closeServer(server);
  }
});

/** URL 构造器：本地代理地址与 ?url= 查询参数编码规则。 */
describe('realtime proxy URL builders', () => {
  it('builds local SSE proxy URLs with target URL encoded as query data', () => {
    const url = new URL(buildRealtimeProxySseUrl('http://127.0.0.1:57123', 'https://remote.example/api/global/event?x=1'));

    expect(url.origin).toBe('http://127.0.0.1:57123');
    expect(url.pathname).toBe('/api/ompchamber/realtime-proxy/sse');
    expect(url.searchParams.get('url')).toBe('https://remote.example/api/global/event?x=1');
  });

  it('builds local WebSocket proxy URLs with ws protocol', () => {
    const url = new URL(buildRealtimeProxyWsUrl('https://127.0.0.1:57123', 'wss://remote.example/api/global/event/ws'));

    expect(url.protocol).toBe('wss:');
    expect(url.host).toBe('127.0.0.1:57123');
    expect(url.pathname).toBe('/api/ompchamber/realtime-proxy/ws');
    expect(url.searchParams.get('url')).toBe('wss://remote.example/api/global/event/ws');
  });
});

/** 代理主行为：SSE/WS 双向转发、认证与 origin 拦截、目标与路径白名单。 */
describe('realtime proxy', () => {
  it('streams SSE chunks and forwards safe SSE headers with configured runtime headers', async () => {
    const upstream = await startSseUpstream();
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: upstream.origin });

    try {
      const response = await fetch(buildRealtimeProxySseUrl(origin, `${upstream.origin}/api/global/event`), {
        headers: {
          Accept: 'text/event-stream',
          'Last-Event-ID': 'evt-42',
          Origin: 'ompchamber-ui://app',
        },
      });

      expect(response.status).toBe(200);
      expect(await response.text()).toBe('data: first\n\ndata: second\n\n');
      expect(upstream.requests).toHaveLength(1);
      expect(upstream.requests[0].headers.accept).toBe('text/event-stream');
      expect(upstream.requests[0].headers['last-event-id']).toBe('evt-42');
      expect(upstream.requests[0].headers['x-proxy-auth']).toBe('secret');
    } finally {
      runtime.stop();
    }
  });

  it('rejects unauthenticated SSE proxy requests', async () => {
    const upstream = await startSseUpstream();
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: upstream.origin, authToken: null });

    try {
      const response = await fetch(buildRealtimeProxySseUrl(origin, `${upstream.origin}/api/global/event`), {
        headers: { Origin: 'ompchamber-ui://app' },
      });

      expect(response.status).toBe(401);
      expect(upstream.requests).toHaveLength(0);
    } finally {
      runtime.stop();
    }
  });

  it('rejects SSE proxy requests from disallowed origins', async () => {
    const upstream = await startSseUpstream();
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: upstream.origin, originAllowed: false });

    try {
      const response = await fetch(buildRealtimeProxySseUrl(origin, `${upstream.origin}/api/global/event`), {
        headers: { Origin: 'https://evil.example' },
      });

      expect(response.status).toBe(403);
      expect(upstream.requests).toHaveLength(0);
    } finally {
      runtime.stop();
    }
  });

  it('rejects targets outside the active runtime origin', async () => {
    const upstream = await startSseUpstream();
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: 'https://different.example' });

    try {
      const response = await fetch(buildRealtimeProxySseUrl(origin, `${upstream.origin}/api/global/event`), {
        headers: { Origin: 'ompchamber-ui://app' },
      });

      expect(response.status).toBe(404);
      expect(upstream.requests).toHaveLength(0);
    } finally {
      runtime.stop();
    }
  });

  it('rejects targets outside the realtime path allowlist', async () => {
    const upstream = await startSseUpstream({ path: '/api/config/settings' });
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: upstream.origin });

    try {
      const response = await fetch(buildRealtimeProxySseUrl(origin, `${upstream.origin}/api/config/settings`), {
        headers: { Origin: 'ompchamber-ui://app' },
      });

      expect(response.status).toBe(404);
      expect(upstream.requests).toHaveLength(0);
    } finally {
      runtime.stop();
    }
  });

  it('proxies WebSocket upgrades using query params from the raw upgrade request URL', async () => {
    let upstreamRequest = null;
    const upstreamServer = http.createServer();
    const upstreamWs = new WebSocketServer({ server: upstreamServer });
    upstreamWs.on('connection', (socket, request) => {
      upstreamRequest = request;
      socket.on('message', (data, isBinary) => {
        socket.send(isBinary ? data : `echo:${data.toString()}`, { binary: isBinary });
      });
    });
    const upstreamOrigin = await listen(upstreamServer);
    const { origin, runtime } = await startProxyServer({ apiBaseUrl: upstreamOrigin });

    try {
      const target = `${upstreamOrigin.replace(/^http:/, 'ws:')}/api/global/event/ws?lastEventId=evt-1`;
      const client = new WebSocket(buildRealtimeProxyWsUrl(origin, target), {
        headers: { Origin: 'ompchamber-ui://app' },
      });
      await new Promise((resolve, reject) => {
        client.once('open', resolve);
        client.once('error', reject);
      });

      const message = await new Promise((resolve) => {
        client.once('message', (data) => resolve(data.toString()));
        client.send('ping');
      });

      expect(message).toBe('echo:ping');
      expect(upstreamRequest?.url).toBe('/api/global/event/ws?lastEventId=evt-1');
      expect(upstreamRequest?.headers['x-proxy-auth']).toBe('secret');
      client.close();
      upstreamWs.close();
    } finally {
      runtime.stop();
    }
  });

  it('allows first passwordless WebSocket proxy upgrade without an existing cookie', async () => {
    const upstreamServer = http.createServer();
    const upstreamWs = new WebSocketServer({ server: upstreamServer });
    upstreamWs.on('connection', (socket) => {
      socket.send('ready');
    });
    const upstreamOrigin = await listen(upstreamServer);
    const uiAuthController = createUiAuth({ password: '' });
    const { origin, runtime } = await startProxyServerWithAuthController({ apiBaseUrl: upstreamOrigin, uiAuthController });

    try {
      const target = `${upstreamOrigin.replace(/^http:/, 'ws:')}/api/global/event/ws`;
      const client = new WebSocket(buildRealtimeProxyWsUrl(origin, target), {
        headers: { Origin: 'ompchamber-ui://app' },
      });
      const message = await new Promise((resolve, reject) => {
        client.once('message', (data) => resolve(data.toString()));
        client.once('error', reject);
      });

      expect(message).toBe('ready');
      client.close();
      upstreamWs.close();
    } finally {
      runtime.stop();
      uiAuthController.dispose?.();
    }
  });
});
