/**
 * 浏览器控制结果路由 routes.js 的测试套件。
 *
 * 刻意跑在真实的 express 应用上：本服务的 body 解析按路由挂载，漏挂
 * 的路由注册时不出错、只在客户端真正 POST 时失败——对 agent 表现为无
 * 来由的超时。这里覆盖结果回传、大快照的 body 上限、失败传播与未知
 * id 的宽容处理。
 */
import { describe, expect, it } from 'vitest';
import express from 'express';
import request from 'supertest';

import { createBrowserControlBroker } from './broker.js';
import { registerBrowserControlRoutes } from './routes.js';

/**
 * These run against a real Express app on purpose. This server attaches body
 * parsing per route, so a route that forgets it still *registers* fine and only
 * fails when a client posts to it — which surfaces to the agent as an
 * unexplained timeout, nowhere near the cause.
 */
/**
 * 组装被测应用（中文补充）：记录型 emitRequest 桩 + 真实路由注册；
 * listeners 控制有多少客户端在听。返回 { app, broker, emitted }。
 */
const createApp = ({ listeners = 1 } = {}) => {
  const emitted = [];
  let sequence = 0;
  const broker = createBrowserControlBroker({
    emitRequest: (payload) => {
      emitted.push(payload);
      return listeners;
    },
    createId: () => {
      sequence += 1;
      return `req-${sequence}`;
    },
  });

  const app = express();
  registerBrowserControlRoutes(app, { express, broker });
  return { app, broker, emitted };
};

// 结果回传路由：JSON 解析、成功与失败的传播、超时后的迟到响应与坏请求。
describe('browser control result route', () => {
  it('parses a posted JSON body and resolves the waiting request', async () => {
    const { app, broker, emitted } = createApp();
    const inflight = broker.request('browser.snapshot', {});

    await request(app)
      .post('/api/browser-control/result')
      .send({ requestId: emitted[0].requestId, ok: true, data: { url: 'http://localhost:3000/' } })
      .expect(200, { matched: true });

    expect(await inflight).toEqual({ url: 'http://localhost:3000/' });
  });

  it('accepts a snapshot large enough to carry a real page', async () => {
    const { app, broker, emitted } = createApp();
    const inflight = broker.request('browser.snapshot', {});

    const data = {
      url: 'http://localhost:3000/',
      text: 'x'.repeat(200_000),
      elements: Array.from({ length: 120 }, (_, index) => ({
        selector: `div:nth-of-type(${index})`,
        label: 'y'.repeat(100),
      })),
    };

    await request(app)
      .post('/api/browser-control/result')
      .send({ requestId: emitted[0].requestId, ok: true, data })
      .expect(200, { matched: true });

    const result = await inflight;
    expect(result.text).toHaveLength(200_000);
    expect(result.elements).toHaveLength(120);
  });

  it('propagates a client-reported failure', async () => {
    const { app, broker, emitted } = createApp();
    // Capture the outcome before posting: the rejection lands while the POST is
    // still in flight, and an unattached handler surfaces as an unhandled one.
    const outcome = broker.request('browser.click', { selector: '#nope' })
      .then(() => null, (error) => error);

    await request(app)
      .post('/api/browser-control/result')
      .send({ requestId: emitted[0].requestId, ok: false, error: 'No element matches #nope' })
      .expect(200, { matched: true });

    expect((await outcome)?.message).toBe('No element matches #nope');
  });

  it('reports matched: false for a response that arrived after the timeout', async () => {
    const { app } = createApp();
    await request(app)
      .post('/api/browser-control/result')
      .send({ requestId: 'expired', ok: true, data: {} })
      .expect(200, { matched: false });
  });

  it('rejects a body with no request id', async () => {
    const { app } = createApp();
    await request(app)
      .post('/api/browser-control/result')
      .send({ ok: true })
      .expect(400);
  });

  it('rejects a body that is not an object', async () => {
    const { app } = createApp();
    await request(app)
      .post('/api/browser-control/result')
      .set('Content-Type', 'application/json')
      .send('"just-a-string"')
      .expect(400);
  });
});
