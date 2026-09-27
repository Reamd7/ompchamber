/**
 * walkthrough 路由（routes.js）的 HTTP 集成测试：起真实 express 服务并用
 * 真实 fetch 访问。存在意义是复现"断线检查误伤健康请求"的 bug——service
 * 与 store 的单测都正确，问题只出现在真实 HTTP 层。覆盖未中断请求的正常
 * 回答、刷新后重连重挂、无 directory 的前置 400、语言参数透传（含非字符串
 * 忽略），以及经专用端点取消而非依赖断线。
 */
import express from 'express';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { registerWalkthroughRoutes } from './routes.js';

// These run over real HTTP on purpose. The bug this file exists for was
// invisible to unit tests: the service and the store were both correct, and the
// response was dropped by a disconnect check that misread a healthy request.

/** 测试用来源：working-tree 全部改动。 */
const SOURCE = { kind: 'working-tree', scope: 'all' };

/** 路由行为：正常回答、断线后重挂、入参校验、语言透传与显式取消。 */
describe('walkthrough routes', () => {
  let server;
  let base;
  let releaseJob;
  /** 当前挂起的生成任务 promise（由 releaseJob 手动放行），null 表示空闲。 */
  let job;

  /** 最近一次传给 service 的参数，用于断言路由透传了什么。 */
  let lastArgs;

  /** walkthrough service 桩：记录参数、可挂起的生成、可查询的取消。 */
  const service = {
    async getWalkthrough(args) {
      lastArgs = args;
      return { walkthrough: null, hunks: [], hunkCount: 0, generating: Boolean(job) };
    },
    async generateWalkthrough(args) {
      lastArgs = args;
      if (job) return job;
      job = new Promise((resolve) => {
        releaseJob = () => resolve({ walkthrough: { title: 'DONE' }, hunks: [], hunkCount: 1 });
      }).finally(() => { job = null; });
      return job;
    },
    async cancelWalkthroughGeneration() {
      return { cancelled: Boolean(job) };
    },
  };

  /** 发起一次 generate POST；可选 signal 用于模拟客户端断开。 */
  const generate = (signal) => fetch(`${base}/api/walkthrough/generate`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ directory: '/repo', source: SOURCE }),
    signal,
  });

  beforeEach(async () => {
    job = null;
    releaseJob = undefined;
    lastArgs = undefined;
    const app = express();
    app.use(express.json());
    registerWalkthroughRoutes(app, { getWalkthroughService: async () => service });
    server = app.listen(0);
    await new Promise((resolve) => server.once('listening', resolve));
    base = `http://127.0.0.1:${server.address().port}`;
  });

  afterEach(async () => {
    await new Promise((resolve) => server.close(resolve));
  });

  it('answers a generation request that nobody interrupted', async () => {
    const pending = generate();
    await new Promise((resolve) => setTimeout(resolve, 20));
    releaseJob();

    const body = await (await pending).json();

    expect(body.walkthrough).toEqual({ title: 'DONE' });
  });

  it('delivers the result to a client that reconnected after a refresh', async () => {
    const controller = new AbortController();
    generate(controller.signal).catch(() => {});
    await new Promise((resolve) => setTimeout(resolve, 20));
    controller.abort();
    await new Promise((resolve) => setTimeout(resolve, 20));

    // The reloaded page sees work in progress and re-attaches to it.
    const read = await (await fetch(
      `${base}/api/walkthrough?directory=/repo&source=${encodeURIComponent(JSON.stringify(SOURCE))}`,
    )).json();
    expect(read.generating).toBe(true);

    const reattached = generate();
    await new Promise((resolve) => setTimeout(resolve, 20));
    releaseJob();

    const body = await (await reattached).json();
    expect(body.walkthrough).toEqual({ title: 'DONE' });
  });

  it('rejects a request without a directory before touching the service', async () => {
    const response = await fetch(`${base}/api/walkthrough/generate`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: SOURCE }),
    });

    expect(response.status).toBe(400);
    expect(job).toBeNull();
  });

  // The language belongs to the request, not to a setting, so both the read
  // and the generation have to carry it: readiness is computed from a prompt
  // that contains the language instruction.
  it('carries the requested language into the service', async () => {
    await fetch(
      `${base}/api/walkthrough?directory=/repo&language=uk&source=${encodeURIComponent(JSON.stringify(SOURCE))}`,
    );
    expect(lastArgs.language).toBe('uk');

    const pending = fetch(`${base}/api/walkthrough/generate`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ directory: '/repo', source: SOURCE, language: 'ja' }),
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    releaseJob();
    await pending;

    expect(lastArgs.language).toBe('ja');
  });

  it('ignores a language that is not a string', async () => {
    await fetch(
      `${base}/api/walkthrough?directory=/repo&language[]=uk&source=${encodeURIComponent(JSON.stringify(SOURCE))}`,
    );

    expect(lastArgs.language).toBeUndefined();
  });

  it('cancels through its own endpoint rather than a dropped connection', async () => {
    generate().catch(() => {});
    await new Promise((resolve) => setTimeout(resolve, 20));

    const response = await fetch(`${base}/api/walkthrough/cancel`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ directory: '/repo', source: SOURCE }),
    });

    expect(await response.json()).toEqual({ cancelled: true });
    releaseJob();
  });
});
