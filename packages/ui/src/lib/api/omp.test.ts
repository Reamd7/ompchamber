import { describe, expect, test } from 'bun:test';
import {
  createOmpAgentDefinitionsAPI,
  createOmpAgentRunsAPI,
  createOmpCapabilitiesAPI,
  createOmpCommandsAPI,
  createOmpEventsAPI,
  createOmpPersonasAPI,
  createOmpSessionAPI,
  createOmpSettingsAPI,
  createOmpTreeAPI,
  createOmpUriAPI,
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

describe('createOmpSettingsAPI (spec 06 §5.2/§5.3)', () => {
  const SETTINGS_PAYLOAD = {
    schemaVersion: '1.2.3',
    directory: '/repo',
    revision: 42,
    tabs: [{ id: 'interaction', label: 'Interaction', groups: ['Approvals', 'Notifications'] }],
    keys: {
      'tools.approvalMode': {
        type: 'enum',
        values: ['always-ask', 'write', 'yolo'],
        default: 'yolo',
        value: 'write',
        configured: true,
        scope: 'global',
        editable: true,
        ui: { tab: 'interaction', group: 'Approvals', label: 'Tool Approval' },
      },
      'hindsight.apiToken': {
        type: 'string',
        default: null,
        value: null,
        configured: true,
        scope: 'global',
        editable: true,
        credential: true,
        writeOnly: true,
      },
      modelRoles: {
        type: 'record',
        default: {},
        value: { default: 'prov/main' },
        roles: { default: { value: 'prov/main', source: 'global', editable: true } },
        modelRoleStorage: 'global',
        scope: 'global+project',
      },
    },
  };

  test('getSettings parses the schema payload and passes directory + keys query', async () => {
    const calls: Array<{ path: string; method: string; query?: Query; body?: string }> = [];
    const api = createOmpSettingsAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ path, method: init?.method ?? 'GET', query: init?.query, body: init?.body as string | undefined });
        return jsonResponse(200, SETTINGS_PAYLOAD);
      }) as unknown as typeof fetch,
    });
    const result = await api.getSettings({ directory: '/repo', keys: ['defaultThinkingLevel'] });
    expect(calls[0]).toEqual({
      path: '/api/omp/settings',
      method: 'GET',
      query: { directory: '/repo', keys: 'defaultThinkingLevel' },
    });
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.data.keys['tools.approvalMode']?.value).toBe('write');
      expect(result.data.keys['hindsight.apiToken']?.configured).toBe(true);
      // The modelRoles record view parses (no `configured` on record entries).
      expect(result.data.keys.modelRoles?.type).toBe('record');
      expect(result.data.tabs[0]?.groups).toEqual(['Approvals', 'Notifications']);
    }
  });

  test('getSettings: 501 → unavailable; malformed payload → failure, never empty success', async () => {
    const off = createOmpSettingsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'feature-off' })) as unknown as typeof fetch,
    });
    expect(await off.getSettings({ directory: '/repo' })).toEqual({ ok: false, unavailable: true });

    const malformed = createOmpSettingsAPI({
      fetchImpl: (async () => jsonResponse(200, { schemaVersion: 1 })) as unknown as typeof fetch,
    });
    expect(await malformed.getSettings({ directory: '/repo' })).toEqual({ ok: false, unavailable: false });
  });

  test('putSettings commits changes and returns revision + applied', async () => {
    const calls: Array<{ method: string; query?: Query; body?: string }> = [];
    const api = createOmpSettingsAPI({
      fetchImpl: (async (_path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ method: init?.method ?? 'GET', query: init?.query, body: init?.body as string | undefined });
        return jsonResponse(200, { revision: 43, applied: { 'tools.approvalMode': 'always-ask' }, persisted: true, quarantined: null });
      }) as unknown as typeof fetch,
    });
    const result = await api.putSettings({ directory: '/repo', changes: { 'tools.approvalMode': 'always-ask' } });
    expect(result).toEqual({ ok: true, revision: 43, applied: { 'tools.approvalMode': 'always-ask' } });
    expect(calls[0]?.method).toBe('PUT');
    expect(calls[0]?.query?.directory).toBe('/repo');
    expect(JSON.parse(calls[0]?.body ?? '{}')).toEqual({ changes: { 'tools.approvalMode': 'always-ask' }, directory: '/repo' });
  });

  test('putSettings maps 400 validation to per-key rejections and 409 to quarantined', async () => {
    const rejected = createOmpSettingsAPI({
      fetchImpl: (async () => jsonResponse(400, {
        error: 'validation',
        rejected: [{ key: 'compaction.strategy', reason: 'invalid-value' }],
      })) as unknown as typeof fetch,
    });
    expect(await rejected.putSettings({ directory: '/repo', changes: { 'compaction.strategy': 'nope' } })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'rejected',
      rejected: [{ key: 'compaction.strategy', reason: 'invalid-value' }],
    });

    const quarantined = createOmpSettingsAPI({
      fetchImpl: (async () => jsonResponse(409, { error: 'config-quarantined', quarantinedTo: '/x.broken-1' })) as unknown as typeof fetch,
    });
    expect(await quarantined.putSettings({ directory: '/repo', changes: { 'todo.reminders': true } })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'quarantined',
    });
  });

  test('putSettings: 501 → unavailable (feature off degrades silently)', async () => {
    const off = createOmpSettingsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'feature-off' })) as unknown as typeof fetch,
    });
    expect(await off.putSettings({ directory: '/repo', changes: { 'todo.reminders': true } })).toEqual({
      ok: false,
      unavailable: true,
    });
  });
});

describe('createOmpUriAPI (spec 04 §5.2.1/§5.2.4 — local:// bridge)', () => {
  const resourceBody = {
    url: 'local://scratch.md',
    content: 'alpha session secret',
    contentType: 'text/markdown',
    immutable: false,
    isDirectory: false,
    token: { id: 'ocuri_' + 'a'.repeat(43), expiresAt: 1234 },
  };

  test('resolve posts the session-pinned body and parses the resource + token', async () => {
    const calls: Array<{ path: string; method: string; body: unknown }> = [];
    const api = createOmpUriAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, method: init?.method ?? '', body: JSON.parse(String(init?.body)) });
        return jsonResponse(200, resourceBody);
      }) as unknown as typeof fetch,
    });

    const result = await api.resolve({ url: 'local://scratch.md', sessionID: 'ses_A', directory: '/repo' });

    expect(calls).toEqual([{
      path: '/api/omp/uri/resolve',
      method: 'POST',
      body: { u: 'local://scratch.md', sessionID: 'ses_A', directory: '/repo' },
    }]);
    expect(result).toEqual({ ok: true, resource: resourceBody });
  });

  test('resolve: 501 → unavailable (uri.v1 off / scheme not enabled)', async () => {
    const api = createOmpUriAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'uri.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await api.resolve({ url: 'local://a.md', sessionID: 's', directory: '/repo' })).toEqual({
      ok: false,
      unavailable: true,
    });
  });

  test('resolve: 404 carries the handler contract message for inline display', async () => {
    const api = createOmpUriAPI({
      fetchImpl: (async () => jsonResponse(404, { error: 'resolve-failed', message: 'Path traversal (..) is not allowed in local:// URLs' })) as unknown as typeof fetch,
    });
    expect(await api.resolve({ url: 'local://../../etc/passwd', sessionID: 's', directory: '/repo' })).toEqual({
      ok: false,
      unavailable: false,
      status: 404,
      error: 'resolve-failed',
      message: 'Path traversal (..) is not allowed in local:// URLs',
    });
  });

  test('resolve: 413 too-large maps the offending size', async () => {
    const api = createOmpUriAPI({
      fetchImpl: (async () => jsonResponse(413, { error: 'too-large', size: 3145728 })) as unknown as typeof fetch,
    });
    expect(await api.resolve({ url: 'local://big.md', sessionID: 's', directory: '/repo' })).toEqual({
      ok: false,
      unavailable: false,
      status: 413,
      error: 'too-large',
      size: 3145728,
    });
  });

  test('resolve: transport failure and malformed payloads never become success', async () => {
    const dead = createOmpUriAPI({
      fetchImpl: (async () => { throw new Error('network down'); }) as unknown as typeof fetch,
    });
    expect(await dead.resolve({ url: 'local://a.md', sessionID: 's', directory: '/repo' })).toEqual({
      ok: false,
      unavailable: false,
    });
    const malformed = createOmpUriAPI({
      fetchImpl: (async () => jsonResponse(200, { nope: true })) as unknown as typeof fetch,
    });
    expect(await malformed.resolve({ url: 'local://a.md', sessionID: 's', directory: '/repo' })).toEqual({
      ok: false,
      unavailable: false,
      status: 200,
    });
  });

  test('open redeems the token against the issuing directory', async () => {
    const calls: Array<{ path: string; body: unknown }> = [];
    const api = createOmpUriAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, body: JSON.parse(String(init?.body)) });
        return jsonResponse(200, {
          url: 'local://scratch.md',
          content: 'alpha session secret',
          size: 19,
          filename: 'scratch.md',
          editable: true,
        });
      }) as unknown as typeof fetch,
    });
    expect(await api.open({ token: 'ocuri_x', directory: '/repo' })).toEqual({
      ok: true,
      payload: { url: 'local://scratch.md', content: 'alpha session secret', size: 19, filename: 'scratch.md', editable: true },
    });
    expect(calls).toEqual([{ path: '/api/omp/uri/open', body: { token: 'ocuri_x', directory: '/repo' } }]);
  });

  test('open: token scope violations surface as errors, not unavailable', async () => {
    const scoped = createOmpUriAPI({
      fetchImpl: (async () => jsonResponse(403, { error: 'scope' })) as unknown as typeof fetch,
    });
    expect(await scoped.open({ token: 'ocuri_x', directory: '/other' })).toEqual({
      ok: false,
      unavailable: false,
      status: 403,
      error: 'scope',
    });
  });
});

describe('createOmpAgentDefinitionsAPI (spec 02 §5.2 — scoped sidecar contract)', () => {
  const RECORD = {
    name: 'reviewer',
    prompt: 'Review code.',
    tools: ['read', 'bash'],
    description: 'Code review worker',
    mode: 'subagent',
    scope: 'global',
  };

  test('list parses the agents array; malformed payload is failure, never empty success', async () => {
    const api = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(200, { agents: [RECORD] })) as unknown as typeof fetch,
    });
    const result = await api.list();
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.data).toHaveLength(1);
      const record = result.data[0];
      expect(record?.name).toBe('reviewer');
      expect(record?.scope).toBe('global');
      expect(record?.tools).toEqual(['read', 'bash']);
    }

    const malformed = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(200, { agents: 'nope' })) as unknown as typeof fetch,
    });
    expect(await malformed.list()).toEqual({ ok: false, unavailable: false });

    const off = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'agentDefinitions.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.list()).toEqual({ ok: false, unavailable: true });
  });

  test('create posts the scoped definition body and returns the record', async () => {
    const calls: Array<{ path: string; method: string; body?: string }> = [];
    const api = createOmpAgentDefinitionsAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, method: init?.method ?? 'GET', body: init?.body as string | undefined });
        return jsonResponse(201, RECORD);
      }) as unknown as typeof fetch,
    });
    const result = await api.create({
      name: 'reviewer',
      prompt: 'Review code.',
      description: 'Code review worker',
      tools: ['read', 'bash'],
      scope: 'project',
    });
    expect(result).toEqual({ ok: true, record: RECORD });
    expect(calls[0]?.path).toBe('/api/omp/agent-definitions');
    expect(calls[0]?.method).toBe('POST');
    expect(JSON.parse(calls[0]?.body ?? '{}')).toEqual({
      scope: 'project',
      definition: {
        name: 'reviewer',
        prompt: 'Review code.',
        description: 'Code review worker',
        tools: ['read', 'bash'],
      },
    });
  });

  test('409 name conflict surfaces the conflict name and error code', async () => {
    const api = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(409, { error: 'agent-definition-exists', name: 'reviewer' })) as unknown as typeof fetch,
    });
    expect(await api.create({ name: 'reviewer', prompt: 'x' })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'rejected',
      reason: 'agent-definition-exists',
      conflictName: 'reviewer',
    });
  });

  test('400 invalid-prompt rejection carries the domain error code', async () => {
    const api = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(400, { error: 'invalid-prompt', message: 'prompt must be a non-empty string' })) as unknown as typeof fetch,
    });
    expect(await api.create({ name: 'x', prompt: '' })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'rejected',
      reason: 'invalid-prompt',
    });
  });

  test('update PUTs patch + renameTo; 404 with a domain body is not-found, not unavailable', async () => {
    const calls: Array<{ path: string; method: string; body?: string }> = [];
    const api = createOmpAgentDefinitionsAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, method: init?.method ?? 'GET', body: init?.body as string | undefined });
        return jsonResponse(200, { ...RECORD, name: 'fresh' });
      }) as unknown as typeof fetch,
    });
    const result = await api.update('old', { definition: { prompt: 'new prompt' }, renameTo: 'fresh' });
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.record.name).toBe('fresh');
    expect(calls[0]?.path).toBe('/api/omp/agent-definitions/old');
    expect(JSON.parse(calls[0]?.body ?? '{}')).toEqual({
      definition: { prompt: 'new prompt' },
      renameTo: 'fresh',
    });

    const missing = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(404, { error: 'not-found' })) as unknown as typeof fetch,
    });
    expect(await missing.update('ghost', { definition: { prompt: 'x' } })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'rejected',
      reason: 'not-found',
    });
  });

  test('remove maps 204 to ok and 404-with-body to not-found; 501 is unavailable', async () => {
    const ok = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => new Response(null, { status: 204 })) as unknown as typeof fetch,
    });
    expect(await ok.remove('reviewer')).toEqual({ ok: true });

    const missing = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(404, { error: 'not-found' })) as unknown as typeof fetch,
    });
    expect(await missing.remove('ghost')).toEqual({ ok: false, unavailable: false, kind: 'not-found' });

    const off = createOmpAgentDefinitionsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'agentDefinitions.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.remove('reviewer')).toEqual({ ok: false, unavailable: true });
  });
});

describe('createOmpPersonasAPI (spec 02 §5.2a)', () => {
  const PERSONA = { name: 'grumpy', description: 'd', systemPrompt: 'Be grumpy.', tools: ['read'] };

  test('list parses the personas array; 501 degrades as unavailable', async () => {
    const api = createOmpPersonasAPI({
      fetchImpl: (async () => jsonResponse(200, { personas: [PERSONA] })) as unknown as typeof fetch,
    });
    const result = await api.list();
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.data[0]).toEqual(PERSONA);

    const off = createOmpPersonasAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'personas.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.list()).toEqual({ ok: false, unavailable: true });
  });

  test('create wraps the persona body; 409 persona-exists surfaces the name', async () => {
    const calls: Array<{ path: string; method: string; body?: string }> = [];
    const api = createOmpPersonasAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, method: init?.method ?? 'GET', body: init?.body as string | undefined });
        return jsonResponse(201, PERSONA);
      }) as unknown as typeof fetch,
    });
    const result = await api.create({ name: 'grumpy', systemPrompt: 'Be grumpy.' });
    expect(result).toEqual({ ok: true, record: PERSONA });
    expect(calls[0]?.path).toBe('/api/omp/personas');
    expect(calls[0]?.method).toBe('POST');
    expect(JSON.parse(calls[0]?.body ?? '{}')).toEqual({
      persona: { name: 'grumpy', systemPrompt: 'Be grumpy.' },
    });

    const conflict = createOmpPersonasAPI({
      fetchImpl: (async () => jsonResponse(409, { error: 'persona-exists', name: 'grumpy' })) as unknown as typeof fetch,
    });
    expect(await conflict.create({ name: 'grumpy' })).toEqual({
      ok: false,
      unavailable: false,
      kind: 'rejected',
      reason: 'persona-exists',
      conflictName: 'grumpy',
    });
  });

  test('update sends the persona patch on the named route', async () => {
    const calls: Array<{ path: string; method: string; body?: string }> = [];
    const api = createOmpPersonasAPI({
      fetchImpl: (async (path: string, init?: RequestInit) => {
        calls.push({ path, method: init?.method ?? 'GET', body: init?.body as string | undefined });
        return jsonResponse(200, { ...PERSONA, systemPrompt: 'Be cheerful.' });
      }) as unknown as typeof fetch,
    });
    const result = await api.update('grumpy', { systemPrompt: 'Be cheerful.' });
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.record.systemPrompt).toBe('Be cheerful.');
    expect(calls[0]?.path).toBe('/api/omp/personas/grumpy');
    expect(JSON.parse(calls[0]?.body ?? '{}')).toEqual({
      persona: { systemPrompt: 'Be cheerful.' },
    });
  });

  test('remove maps 204 to ok; transport failure is error', async () => {
    const ok = createOmpPersonasAPI({
      fetchImpl: (async () => new Response(null, { status: 204 })) as unknown as typeof fetch,
    });
    expect(await ok.remove('grumpy')).toEqual({ ok: true });

    const unreachable = createOmpPersonasAPI({
      fetchImpl: (async () => { throw new Error('transport'); }) as unknown as typeof fetch,
    });
    expect(await unreachable.remove('grumpy')).toEqual({ ok: false, unavailable: false, kind: 'error' });
  });
});

describe('createOmpTreeAPI (spec 04 §5.4 — fork-lineage snapshot)', () => {
  const TREE = {
    leafId: 'ses_2',
    nodes: [
      { id: 'ses_1', parentId: null, title: 'root', time: { created: 1, updated: 5 } },
      { id: 'ses_2', parentId: 'ses_1', title: 'fork of root', time: { created: 2, updated: 9 } },
    ],
  };

  test('parses the {leafId, nodes} snapshot and threads the directory query', async () => {
    const calls: Array<{ path: string; query?: Query }> = [];
    const api = createOmpTreeAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ path, query: init?.query });
        return jsonResponse(200, TREE);
      }) as unknown as typeof fetch,
    });
    const result = await api.getSessionTree('ses_2', { directory: '/repo' });
    expect(result).toEqual({ ok: true, data: TREE });
    expect(calls).toEqual([{ path: '/api/omp/sessions/ses_2/tree', query: { directory: '/repo' } }]);
  });

  test('501/404 degrade as unavailable; malformed payloads never become empty trees', async () => {
    const off = createOmpTreeAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'tree.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.getSessionTree('s', { directory: '/repo' })).toEqual({ ok: false, unavailable: true });

    const malformed = createOmpTreeAPI({
      fetchImpl: (async () => jsonResponse(200, { leafId: 'x', nodes: [{ id: 'ses_1' }] })) as unknown as typeof fetch,
    });
    expect(await malformed.getSessionTree('s', { directory: '/repo' })).toEqual({ ok: false, unavailable: false });
  });
});

describe('createOmpAgentRunsAPI (spec 04 §5.5.1 — directory snapshot)', () => {
  const SNAPSHOT = {
    agentRuns: [{
      key: 'ses_1::Anna',
      sessionID: 'ses_1',
      directory: '/repo',
      agentId: 'Anna',
      displayName: 'Anna',
      status: 'parked',
      createdAt: 1,
      lastActivity: 9,
    }],
    generatedAt: 99,
    revision: 3,
  };

  test('parses rows (parked included) and threads the directory query', async () => {
    const calls: Array<{ path: string; query?: Query }> = [];
    const api = createOmpAgentRunsAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ path, query: init?.query });
        return jsonResponse(200, SNAPSHOT);
      }) as unknown as typeof fetch,
    });
    const result = await api.list({ directory: '/repo' });
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.data.agentRuns).toHaveLength(1);
      expect(result.data.agentRuns[0]?.status).toBe('parked');
      expect(result.data.revision).toBe(3);
    }
    expect(calls).toEqual([{ path: '/api/omp/agent-runs', query: { directory: '/repo' } }]);
  });

  test('unknown statuses and malformed payloads are failure, never empty success', async () => {
    const badStatus = createOmpAgentRunsAPI({
      fetchImpl: (async () => jsonResponse(200, {
        agentRuns: [{ ...SNAPSHOT.agentRuns[0], status: 'sleeping' }],
        generatedAt: 1,
        revision: 1,
      })) as unknown as typeof fetch,
    });
    expect(await badStatus.list({ directory: '/repo' })).toEqual({ ok: false, unavailable: false });

    const off = createOmpAgentRunsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'agentRuns.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.list({ directory: '/repo' })).toEqual({ ok: false, unavailable: true });
  });
});

describe('createOmpCommandsAPI (spec 08 §5.4 — omp command discovery)', () => {
  const COMMANDS = [
    { name: 'debug', description: 'Open debug tools selector', tier: 'client-builtin', source: 'builtin' },
    { name: 'review', description: 'project review', tier: 'engine', source: 'file', argumentHint: '<focus>' },
  ];

  test('parses the bare command array and threads the directory query', async () => {
    const calls: Array<{ path: string; query?: Query }> = [];
    const api = createOmpCommandsAPI({
      fetchImpl: (async (path: string, init?: RequestInit & { query?: Query }) => {
        calls.push({ path, query: init?.query });
        return jsonResponse(200, COMMANDS);
      }) as unknown as typeof fetch,
    });
    const result = await api.getCommands({ directory: '/repo' });
    expect(result).toEqual({ ok: true, data: COMMANDS });
    expect(calls).toEqual([{ path: '/api/omp/commands', query: { directory: '/repo' } }]);
  });

  test('records with a bad tier/source or missing name are failure, never empty success', async () => {
    const badTier = createOmpCommandsAPI({
      fetchImpl: (async () => jsonResponse(200, [
        { name: 'x', tier: 'magic', source: 'file' },
      ])) as unknown as typeof fetch,
    });
    expect(await badTier.getCommands({ directory: '/repo' })).toEqual({ ok: false, unavailable: false });

    const off = createOmpCommandsAPI({
      fetchImpl: (async () => jsonResponse(501, { error: 'commands.v1-unavailable' })) as unknown as typeof fetch,
    });
    expect(await off.getCommands({ directory: '/repo' })).toEqual({ ok: false, unavailable: true });
  });
});
