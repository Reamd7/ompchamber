import { describe, expect, test } from 'bun:test';
import { registerEndpoints } from './endpoints.ts';
import { OmpEventBus, WireEventBus } from './events.ts';
import type { OmpHostEngine } from './engine.ts';
import type { RouteHandler } from './endpoints.ts';

// `POST /omp/sessions/{id}/bash` — the OpenChamber-owned `!` local-execution
// route (07 §3.2). The handler validates the request, forwards to
// engine.executeBash, and maps the discriminated outcome; the engine's own
// behavior (card events, echo bridging, deferred records) is covered in
// omp-host.engine.test.ts.

// SAFETY: endpoint tests install only the members the mounted routes call;
// the double is a duck-typed partial, not a full engine.
const asEngineDouble = <T,>(double: T): OmpHostEngine => double as OmpHostEngine;

type ExecuteBashInput = { sessionID: string; directory?: string; command: string; excludeFromContext?: boolean };
type ExecuteBashOutcome =
  | { status: 'ok'; message: { info: { id: string } }; result: { output: string; exitCode?: number; cancelled: boolean; truncated: boolean; timedOut: boolean } }
  | { status: 'refused'; error: string }
  | { status: 'notFound' };

const mountBashRoute = (executeBash: (input: ExecuteBashInput) => Promise<ExecuteBashOutcome>): RouteHandler => {
  const routes: Array<{ method: string; pattern: string; handler: RouteHandler }> = [];
  const route = (method: string, pattern: string, handler: RouteHandler) => routes.push({ method, pattern, handler });
  registerEndpoints(
    route,
    asEngineDouble({
      bus: new WireEventBus({ capacity: 8, maxBytes: 64 * 1024, maxEventBytes: 512 * 1024 }),
      ompBus: new OmpEventBus({ capacity: 8, maxBytes: 64 * 1024, maxEventBytes: 512 * 1024 }),
      executeBash,
      dialogs: { mount: () => {} },
      modesDomain: {},
      uriDomain: { mount: () => {} },
      processDomain: { mount: () => {} },
      settingsStoreReady: async () => null,
      settingsStore: null,
      customAgents: new Map(),
      ready: async () => {},
      availableModels: () => [],
      getStreamDiagnostics: () => ({}),
    }),
    { version: 'test' },
  );
  const entry = routes.find((r) => r.method === 'POST' && r.pattern === '/omp/sessions/{id}/bash');
  if (!entry) throw new Error('route not mounted: POST /omp/sessions/{id}/bash');
  return entry.handler;
};

const bashRequest = (id: string, body: { command?: string; excludeFromContext?: boolean }, query = 'directory=%2Frepo'): Request =>
  new Request(`http://host/omp/sessions/${id}/bash?${query}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });

const bashCtx = (id: string) => ({
  params: { id },
  url: new URL(`http://host/omp/sessions/${id}/bash`),
  headers: new Headers(),
  // The mounted handler captures the registerEndpoints engine argument; the
  // context copy is only the dispatch contract shape.
  engine: asEngineDouble({}),
});

const invoke = async (handler: RouteHandler, id: string, body: { command?: string; excludeFromContext?: boolean }, query?: string): Promise<Response> => {
  const out = await handler(bashRequest(id, body, query), bashCtx(id));
  if (!(out instanceof Response)) throw new Error('handler returned void');
  return out;
};

const okOutcome: ExecuteBashOutcome = {
  status: 'ok',
  message: { info: { id: 'msg_live' } },
  result: { output: 'hi\n', exitCode: 0, cancelled: false, truncated: false, timedOut: false },
};

describe('POST /omp/sessions/{id}/bash', () => {
  test('forwards the command with the request directory and returns the outcome', async () => {
    const calls: ExecuteBashInput[] = [];
    const handler = mountBashRoute(async (input) => {
      calls.push(input);
      return okOutcome;
    });

    const response = await invoke(handler, 's1', { command: 'echo hi', excludeFromContext: true });

    expect(response.status).toBe(200);
    expect(calls).toEqual([{ sessionID: 's1', directory: '/repo', command: 'echo hi', excludeFromContext: true }]);
    // SAFETY: test fixture narrowing — the response body is the okOutcome the
    // route mounted above serializes verbatim.
    const body = await response.json() as { ok?: boolean; message?: { info?: { id?: string } }; result?: { exitCode?: number } };
    expect(body.ok).toBe(true);
    expect(body.message?.info?.id).toBe('msg_live');
    expect(body.result?.exitCode).toBe(0);
  });

  test('excludeFromContext defaults to false', async () => {
    const calls: ExecuteBashInput[] = [];
    const handler = mountBashRoute(async (input) => {
      calls.push(input);
      return okOutcome;
    });

    await invoke(handler, 's1', { command: 'pwd' });

    expect(calls[0]?.excludeFromContext).toBe(false);
  });

  test('rejects a missing command', async () => {
    const calls: ExecuteBashInput[] = [];
    const handler = mountBashRoute(async (input) => {
      calls.push(input);
      return okOutcome;
    });

    const response = await invoke(handler, 's1', {});
    expect(response.status).toBe(400);
    expect(calls).toHaveLength(0);

    const blank = await invoke(handler, 's1', { command: '   ' });
    expect(blank.status).toBe(400);
    expect(calls).toHaveLength(0);
  });

  test('rejects when no directory scopes the request', async () => {
    const handler = mountBashRoute(async () => okOutcome);
    const response = await invoke(handler, 's1', { command: 'pwd' }, '');
    expect(response.status).toBe(400);
  });

  test('maps notFound to 404 and refused to 400', async () => {
    const notFoundHandler = mountBashRoute(async () => ({ status: 'notFound' }));
    const missing = await invoke(notFoundHandler, 'gone', { command: 'pwd' });
    expect(missing.status).toBe(404);
    // SAFETY: test fixture narrowing — the route's error body is the
    // `{name, data: {message}}` shape badRequest/notFound produce.
    const missingBody = await missing.json() as { data?: { message?: string } };
    expect(missingBody.data?.message).toBe('session not found');

    const refusedHandler = mountBashRoute(async () => ({ status: 'refused', error: 'cd is pinned' }));
    const refused = await invoke(refusedHandler, 's1', { command: 'cd ..' });
    expect(refused.status).toBe(400);
    // SAFETY: same error-body contract as above.
    const refusedBody = await refused.json() as { data?: { message?: string } };
    expect(refusedBody.data?.message).toBe('cd is pinned');
  });
});
