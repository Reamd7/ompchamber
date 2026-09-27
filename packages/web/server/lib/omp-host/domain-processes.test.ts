// domain-processes tests — PLAN-session-process-monitor.md §验证.
//
// Contracts under test: capability gating (501 while the feature or the
// ledger is unavailable), snapshot/output/kill route shapes, param decoding,
// and error mapping (not-found / forbidden / unsupported action).
/**
 * domain-processes 路由测试，对应
 * PLAN-session-process-monitor.md §验证：capability 门控（feature 或
 * ledger 不可用时 501）、snapshot/output/kill 路由形状、参数解码与
 * 错误映射（not-found / forbidden / 不支持的动作）。
 */

import { describe, expect, test } from 'bun:test';
import { createProcessDomain } from './domain-processes.ts';
import { ProcessLedger } from './process-ledger.ts';
import type { ProcInfo, ProcessPlatform } from './process-platform.ts';
import type { UriRoute, UriRouteContext } from './domain-uri.ts';

/** 测试基准目录。 */
const DIR = '/repo';
/** 测试会话 id。 */
const SES = 'ses_1';

/** 全空的进程平台桩：枚举无进程、terminate 恒成功，路由层无需真实 OS。 */
const platform: ProcessPlatform = {
  enumerateTree: () => Promise.resolve([] satisfies ProcInfo[]),
  sampleStats: () => Promise.resolve(new Map()),
  cwdOf: () => null,
  isAlive: () => true,
  terminate: () => Promise.resolve(true),
};

/** 构造关闭定时器的台账并注入一个打开的 bash 窗口（toolCall c1）。 */
const makeLedger = () => {
  const ledger = new ProcessLedger({
    platform,
    setIntervalFn: () => ({}),
    clearIntervalFn: () => {},
  });
  ledger.onToolStart({ sessionID: SES, directory: DIR, toolCallId: 'c1', toolName: 'bash', args: { command: 'sleep 5' } });
  return ledger;
};

/** 挂载后 handler 的调用签名（与 UriRoute 的 handler 一致）。 */
type Handler = (request: Request, ctx?: UriRouteContext) => Response | Promise<Response>;

/** 用给定 ledger/features 挂载进程域，返回 (method path) → handler 表。 */
const mount = (deps: { ledger: ProcessLedger | null; features?: Record<string, boolean> }) => {
  const routes = new Map<string, Handler>();
  const route: UriRoute = (method, path, handler) => routes.set(`${method} ${path}`, handler);
  createProcessDomain({ features: () => deps.features ?? { 'processes.v1': true }, ledger: () => deps.ledger }).mount(route);
  return routes;
};

/** 以 GET 调用挂载表中指定路径的 handler。 */
const get = (routes: Map<string, Handler>, path: string, ctx?: UriRouteContext) => {
  const handler = routes.get(`GET ${path}`)!;
  return handler(new Request(`http://x${path}`), ctx);
};

/** 构造只带 url 的路由上下文（GET 路由只读查询参数）。 */
const urlCtx = (url: string): UriRouteContext => ({ params: {}, url: new URL(url) });

/** 主套件：门控、参数校验、快照/输出/kill 的状态码与响应体形状。 */
describe('domain-processes routes', () => {
  test('feature off answers 501', async () => {
    const routes = mount({ ledger: makeLedger(), features: { 'processes.v1': false } });
    const res = await get(routes, '/omp/processes', urlCtx('http://x/omp/processes?directory=%2Frepo'));
    expect(res.status).toBe(501);
  });

  test('unresolved ledger answers 501 even with the feature on', async () => {
    const routes = mount({ ledger: null });
    const res = await get(routes, '/omp/processes', urlCtx('http://x/omp/processes?directory=%2Frepo'));
    expect(res.status).toBe(501);
  });

  test('snapshot requires a directory', async () => {
    const routes = mount({ ledger: makeLedger() });
    const res = await get(routes, '/omp/processes', urlCtx('http://x/omp/processes'));
    expect(res.status).toBe(400);
  });

  test('snapshot returns the ledger entries for the directory', async () => {
    const ledger = makeLedger();
    const routes = mount({ ledger });
    const res = await get(routes, '/omp/processes', urlCtx('http://x/omp/processes?directory=%2Frepo'));
    expect(res.status).toBe(200);
    const body = await res.json();
    // SAFETY: test fixture — the response shape is this module's own contract.
    const snap = body as { revision: number; entries: unknown[] };
    expect(snap.entries).toHaveLength(0); // no processes observed yet
    expect(Number.isFinite(snap.revision)).toBe(true);
  });

  test('output returns the captured tail', async () => {
    const ledger = makeLedger();
    ledger.onToolUpdate({ sessionID: SES, directory: DIR, toolCallId: 'c1', text: 'hello\n' });
    const routes = mount({ ledger });
    const res = await get(
      routes,
      '/omp/processes/output',
      urlCtx(`http://x/omp/processes/output?directory=${encodeURIComponent(DIR)}&key=${encodeURIComponent(`${SES} c1`)}`),
    );
    expect(res.status).toBe(200);
    // SAFETY: test fixture — response shape is the module's own contract.
    expect((await res.json() as { output: string }).output).toBe('hello\n');
  });

  test('output 404s for an unknown key', async () => {
    const routes = mount({ ledger: makeLedger() });
    const res = await get(
      routes,
      '/omp/processes/output',
      urlCtx(`http://x/omp/processes/output?directory=${encodeURIComponent(DIR)}&key=nope`),
    );
    expect(res.status).toBe(404);
  });

  test('kill rejects a non-kill body', async () => {
    const routes = mount({ ledger: makeLedger() });
    const handler = routes.get('POST /omp/processes/{sessionID}/{key}')!;
    const res = await handler(
      new Request('http://x/omp/processes/ses_1/x', { method: 'POST', body: JSON.stringify({ kind: 'restart' }) }),
      { params: { sessionID: SES, key: 'x' } },
    );
    expect(res.status).toBe(400);
  });

  test('kill maps not-found to 404', async () => {
    const routes = mount({ ledger: makeLedger() });
    const handler = routes.get('POST /omp/processes/{sessionID}/{key}')!;
    const res = await handler(
      new Request('http://x/omp/processes/ses_1/x', { method: 'POST', body: JSON.stringify({ kind: 'kill' }) }),
      { params: { sessionID: SES, key: 'nope' } },
    );
    expect(res.status).toBe(404);
  });
});
