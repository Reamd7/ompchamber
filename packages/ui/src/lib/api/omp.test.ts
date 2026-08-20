import { describe, expect, test } from 'bun:test';
import {
  createOmpCapabilitiesAPI,
  createOmpEventsAPI,
  createOmpSessionAPI,
  parseOmpEnvelope,
  parseOmpSseBlock,
  type OmpEventEnvelope,
} from './omp';

type Query = Record<string, string | number | boolean | undefined>;

const promiseWithResolversShim = (): { promise: Promise<void>; resolve: () => void } => {
  let settle: (() => void) | null = null;
  const promise = new Promise<void>((resolve) => { settle = resolve; });
  return { promise, resolve: () => settle?.() };
};

const jsonResponse = (status: number, body: unknown): Response =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });

const sseResponse = (frames: string[]): { response: Response; ended: Promise<void> } => {
  const { promise, resolve } = promiseWithResolversShim();
  const encoder = new TextEncoder();
  let index = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (index < frames.length) {
        controller.enqueue(encoder.encode(frames[index]));
        index += 1;
        return;
      }
      controller.close();
      resolve();
    },
  });
  return { response: new Response(stream, { status: 200, headers: { 'Content-Type': 'text/event-stream' } }), ended: promise };
};

const envelopeFrame = (envelope: OmpEventEnvelope): string =>
  `id: ${envelope.id}\nevent: ${envelope.type}\ndata: ${JSON.stringify(envelope)}\n\n`;

const envelope = (overrides: Partial<OmpEventEnvelope> = {}): OmpEventEnvelope => ({
  id: 1,
  type: 'omp.notice.raised',
  directory: '/repo',
  schemaVersion: '1.0',
  createdAt: 1000,
  payload: {},
  ...overrides,
});

describe('parseOmpSseBlock', () => {
  test('parses id/event/data triple and ignores comments', () => {
    const frame = parseOmpSseBlock(': heartbeat\nid: 42\nevent: omp.mode.changed\ndata: {"id":42}');
    expect(frame?.eventName).toBe('omp.mode.changed');
    expect(frame?.data).toBe('{"id":42}');
  });

  test('returns null for comment-only or blank blocks', () => {
    expect(parseOmpSseBlock('')).toBeNull();
    expect(parseOmpSseBlock(':heartbeat')).toBeNull();
  });
});

describe('parseOmpEnvelope', () => {
  test('accepts a well-formed envelope; optional sessionID stays absent', () => {
    const parsed = parseOmpEnvelope({
      id: 7, type: 'omp.mode.changed', directory: '/repo', schemaVersion: '1.0', createdAt: 5, payload: { mode: 'goal' },
    });
    expect(parsed?.id).toBe(7);
    expect('sessionID' in (parsed ?? {})).toBe(false);
  });

  test('rejects missing required fields', () => {
    expect(parseOmpEnvelope({ type: 'x', directory: '/repo' })).toBeNull();
    expect(parseOmpEnvelope(null)).toBeNull();
  });
});

describe('createOmpEventsAPI.subscribeEvents', () => {
  test('delivers envelopes and resync control frames; sends Last-Event-ID on reconnect', async () => {
    const seen: OmpEventEnvelope[] = [];
    const resyncs: Array<{ scope: string[]; lastEventId: number | null }> = [];
    const requestHeaders: Array<Record<string, string> | undefined> = [];
    const queries: Array<Query | undefined> = [];
    let connections = 0;
    let secondStream: ReturnType<typeof sseResponse> | null = null;

    const fetchImpl = (async (_path: string, init?: RequestInit & { query?: Query }) => {
      connections += 1;
      requestHeaders.push(init?.headers as Record<string, string> | undefined);
      queries.push(init?.query);
      if (connections === 1) {
        return sseResponse([
          ':ok\n\n',
          envelopeFrame(envelope({ id: 11, type: 'omp.mode.changed', sessionID: 'ses_1', payload: { mode: 'goal' } })),
        ]).response;
      }
      secondStream = sseResponse([
        'event: omp.stream.resync\ndata: {"id":12,"type":"omp.stream.resync","directory":"/repo","schemaVersion":"1.0","createdAt":2000,"payload":{"scope":["model","settings"],"lastEventId":11}}\n\n',
        envelopeFrame(envelope({ id: 13, type: 'omp.settings.updated', payload: { revision: 4 } })),
      ]);
      return secondStream.response;
    }) as unknown as typeof fetch;

    let settle_deliverAll: (() => void) | null = null; // eslint-disable-line
  const delivered = new Promise<void>((resolve) => { settle_deliverAll = resolve; });
  const deliverAll = settle_deliverAll!;
    const api = createOmpEventsAPI({ fetchImpl });
    const subscription = api.subscribeEvents('/repo', {
      onEvent: (env) => {
        seen.push(env);
        if (env.id === 13) deliverAll();
      },
      onResync: (payload) => resyncs.push(payload),
    });

    await delivered;
    await secondStream!.ended;
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(seen.map((env) => env.id)).toEqual([11, 13]);
    expect(resyncs).toEqual([{ scope: ['model', 'settings'], lastEventId: 11 }]);
    expect(queries[0]?.directory).toBe('/repo');
    expect(requestHeaders[0]?.['Last-Event-ID']).toBe(undefined);
    expect(requestHeaders[1]?.['Last-Event-ID']).toBe('11');
    subscription.close();
  });

  test('heartbeats count as stream activity and do not produce events', async () => {
    const seen: OmpEventEnvelope[] = [];
    const { response, ended } = sseResponse([':ok\n\n', ':heartbeat\n\n', envelopeFrame(envelope({ id: 3 }))]);
    const fetchImpl = (async () => response) as unknown as typeof fetch;
    const api = createOmpEventsAPI({ fetchImpl });
    const { promise: delivered, resolve } = promiseWithResolversShim();
    const subscription = api.subscribeEvents(null, {
      onEvent: (env) => {
        seen.push(env);
        resolve();
      },
      onResync: () => {},
    });
    await delivered;
    await ended;
    expect(seen).toHaveLength(1);
    subscription.close();
  });

  test('permanent 4xx takes the long cap: one attempt, one onDisconnect', async () => {
    let attempts = 0;
    let disconnects = 0;
    const fetchImpl = (async () => {
      attempts += 1;
      return jsonResponse(404, { error: 'nope' });
    }) as unknown as typeof fetch;
    const api = createOmpEventsAPI({ fetchImpl });
    const subscription = api.subscribeEvents(null, {
      onEvent: () => {},
      onResync: () => {},
      onDisconnect: () => {
        disconnects += 1;
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    subscription.close();
    expect(attempts).toBe(1);
    expect(disconnects).toBe(1);
  });
});

describe('createOmpCapabilitiesAPI.getCapabilities (degradation matrix, spec 05 §5.2.3)', () => {
  test('404 (old engine) → null, wire-only degradation', async () => {
    const api = createOmpCapabilitiesAPI({ fetchImpl: (async () => jsonResponse(404, {})) as unknown as typeof fetch });
    expect(await api.getCapabilities()).toBeNull();
  });

  test('501 (feature explicitly off) → null', async () => {
    const api = createOmpCapabilitiesAPI({ fetchImpl: (async () => jsonResponse(501, { error: 'events-unavailable' })) as unknown as typeof fetch });
    expect(await api.getCapabilities()).toBeNull();
  });

  test('200 payload without eventSchema → null (old engine shape)', async () => {
    const api = createOmpCapabilitiesAPI({
      fetchImpl: (async () => jsonResponse(200, { version: 1, features: {} })) as unknown as typeof fetch,
    });
    expect(await api.getCapabilities()).toBeNull();
  });

  test('healthy payload resolves; features.events gate readable', async () => {
    const api = createOmpCapabilitiesAPI({
      fetchImpl: (async () => jsonResponse(200, {
        version: 1, eventSchema: '1.0', features: { events: true }, minUiVersion: '0.0.0',
      })) as unknown as typeof fetch,
    });
    const capabilities = await api.getCapabilities();
    expect(capabilities?.eventSchema).toBe('1.0');
    expect(capabilities?.features.events).toBe(true);
  });

  test('transport failure throws (never masquerades as absent)', async () => {
    const api = createOmpCapabilitiesAPI({
      fetchImpl: (async () => { throw new Error('network down'); }) as unknown as typeof fetch,
    });
    await expect(api.getCapabilities()).rejects.toThrow('omp capabilities fetch failed');
  });
});

describe('createOmpSessionAPI reads', () => {
  test('parses custom message rows and passes the directory query', async () => {
    const calls: Array<{ path: string; query?: Query }> = [];
    const api = createOmpSessionAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ path, query: init?.query });
        return jsonResponse(200, [
          { wireMessageID: 'msg_1', customType: 'advisor', timestamp: 5, attribution: 'a1', details: { severity: 'high' } },
        ]);
      }) as unknown as typeof fetch,
    });
    const result = await api.getCustomMessages('ses_1', { directory: '/repo' });
    expect(result.ok && result.data[0]?.customType).toBe('advisor');
    expect(calls[0]?.path).toBe('/api/omp/sessions/ses_1/custom-messages');
    expect(calls[0]?.query?.directory).toBe('/repo');
  });

  test('501 → unavailable:true (surface not landed, degrade silently)', async () => {
    const api = createOmpSessionAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'feature-off' })) as unknown as typeof fetch,
    });
    const result = await api.getTelemetry('ses_1', { directory: '/repo' });
    expect(result).toEqual({ ok: false, unavailable: true });
  });

  test('malformed rows → failure result, never empty success', async () => {
    const api = createOmpSessionAPI({
      fetchImpl: (async () => jsonResponse(200, [{ noWireId: true }])) as unknown as typeof fetch,
    });
    const result = await api.getCustomMessages('ses_1', { directory: '/repo' });
    expect(result).toEqual({ ok: false, unavailable: false });
  });

  test('entries read passes kinds filter through the query', async () => {
    const calls: Array<{ query?: Query }> = [];
    const api = createOmpSessionAPI({
      fetchImpl: (async (_path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ query: init?.query });
        return jsonResponse(200, [{ kind: 'compaction', summary: 's' }]);
      }) as unknown as typeof fetch,
    });
    const result = await api.getEntries('ses_1', { directory: '/repo', kinds: ['compaction', 'retry_recovery'] });
    expect(result.ok && result.data[0]?.kind).toBe('compaction');
    expect(calls[0]?.query?.kinds).toBe('compaction,retry_recovery');
  });
});
