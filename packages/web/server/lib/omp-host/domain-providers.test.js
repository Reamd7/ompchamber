// domain-providers tests: GUI CRUD over the engine's models.yml
// (capability `providers.v1`). Uses throwaway agent dirs with a realistic
// commented models.yml so comment preservation, field-merge semantics,
// credential masking, and validation rejections are exercised against real
// files and the real SDK schema/validators.

import { describe, test, expect, afterAll } from 'bun:test';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import {
  listOmpProviders,
  putOmpProvider,
  deleteOmpProvider,
  fetchOmpProviderModels,
  registerProvidersDomainRoutes,
} from './domain-providers.js';

const cleanupDirs = [];
const makeDir = () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'omp-domain-providers-'));
  cleanupDirs.push(dir);
  return dir;
};

const TEMPLATE = `# oh-my-pi (omp) custom provider config.
# Docs: https://omp.sh/docs/providers

providers:
  # gateway one
  alpha:
    baseUrl: https://alpha.example.com/v1
    apiKey: ak-secret-one
    api: openai-responses
    models:
      - id: a1
        name: Alpha One
        reasoning: true
        thinking:
          mode: effort
          efforts: [low, high]
        contextWindow: 200000
      - id: a2
        name: Alpha Two
  # gateway two
  beta:
    baseUrl: https://beta.example.com/v1
    apiKey: ak-secret-two
    api: openai-completions
    authHeader: true
    models:
      - id: b1
`;

afterAll(async () => {
  for (const dir of cleanupDirs) {
    try {
      rmSync(dir, { recursive: true, force: true, maxRetries: 5 });
    } catch {
      // best-effort teardown on Windows file locks
    }
  }
});

const makeEnv = ({ template = TEMPLATE } = {}) => {
  const agentDir = makeDir();
  const modelsPath = path.join(agentDir, 'models.yml');
  writeFileSync(modelsPath, template);
  return { modelsPath };
};

describe('GET /omp/providers (listOmpProviders)', () => {
  test('tags file providers with config + masked key; engine-only providers tagged engine', async () => {
    const { modelsPath } = makeEnv();
    const result = await listOmpProviders({
      modelsPath,
      listEngineModels: () => [{ provider: 'alpha' }, { provider: 'zhipu-coding-plan' }],
    });
    const byId = Object.fromEntries(result.providers.map((p) => [p.id, p]));
    expect(byId.alpha.source).toBe('file');
    expect(byId.alpha.baseUrl).toBe('https://alpha.example.com/v1');
    expect(byId.alpha.hasApiKey).toBe(true);
    expect(JSON.stringify(byId.alpha)).not.toContain('ak-secret');
    expect(byId.alpha.models.map((m) => m.id)).toEqual(['a1', 'a2']);
    expect(byId['zhipu-coding-plan'].source).toBe('engine');
    expect(byId['zhipu-coding-plan'].models).toEqual([]);
    expect(result.providers.some((p) => p.id === 'beta' && p.source === 'file')).toBe(true);
  });
});

describe('PUT /omp/providers (putOmpProvider)', () => {
  test('creates a provider in a commented file, preserving every comment', async () => {
    const { modelsPath } = makeEnv();
    const result = await putOmpProvider({
      provider: {
        id: 'gamma',
        baseUrl: 'https://gamma.example.com/v1',
        api: 'openai-responses',
        apiKey: 'gk-123',
        models: [{ id: 'g1', name: 'Gamma One', reasoning: true, contextWindow: 128000 }],
      },
    }, { modelsPath });
    expect(result.status).toBe(200);
    expect(result.body.provider.hasApiKey).toBe(true);
    expect(JSON.stringify(result.body)).not.toContain('gk-123');

    const written = readFileSync(modelsPath, 'utf8');
    expect(written).toContain('# gateway one');
    expect(written).toContain('# gateway two');
    expect(written).toContain('# Docs: https://omp.sh/docs/providers');
    expect(written).toContain('gamma:');
    expect(written).toContain('gk-123'); // plaintext key lives in the file, as omp does
  });

  test('edit merges only managed keys; hand-authored thinking blocks survive; absent apiKey keeps existing', async () => {
    const { modelsPath } = makeEnv();
    const result = await putOmpProvider({
      provider: {
        id: 'alpha',
        baseUrl: 'https://alpha-2.example.com/v1',
        models: [
          { id: 'a1', name: 'Alpha One Renamed' },
          { id: 'a3', name: 'Alpha Three' },
        ],
      },
    }, { modelsPath });
    expect(result.status).toBe(200);

    const written = readFileSync(modelsPath, 'utf8');
    // Edited managed key.
    expect(written).toContain('alpha-2.example.com');
    // apiKey absent from the PUT → kept.
    expect(written).toContain('ak-secret-one');
    // Hand-authored thinking block on a1 survived the rename.
    expect(written).toContain('efforts:');
    // a2 dropped (absent from the incoming list), a3 added.
    expect(written).not.toContain('Alpha Two');
    expect(written).toContain('Alpha Three');
    // Comments intact.
    expect(written).toContain('# gateway one');
  });

  test('model dialog thinking block replaces/removes and untouched blocks survive', async () => {
    const { modelsPath } = makeEnv();
    // alpha.a1 has a hand-authored thinking block (efforts [low, high]).
    const edited = await putOmpProvider({
      provider: {
        id: 'alpha',
        baseUrl: 'https://alpha.example.com/v1',
        models: [
          { id: 'a1', name: 'Alpha One', thinking: { efforts: ['medium', 'high', 'xhigh'], defaultLevel: 'high' } },
          { id: 'a2', name: 'Alpha Two' },
        ],
      },
    }, { modelsPath });
    expect(edited.status).toBe(200);
    const written = readFileSync(modelsPath, 'utf8');
    expect(written).toContain('defaultLevel: high');
    expect(written).not.toContain('low'); // replaced, not merged with old efforts
    // Untouched a2 (no thinking key in payload) keeps no block; and clearing:
    const cleared = await putOmpProvider({
      provider: {
        id: 'alpha',
        baseUrl: 'https://alpha.example.com/v1',
        models: [{ id: 'a1', name: 'Alpha One', thinking: null }, { id: 'a2', name: 'Alpha Two' }],
      },
    }, { modelsPath });
    expect(cleared.status).toBe(200);
    expect(readFileSync(modelsPath, 'utf8')).not.toContain('defaultLevel');
    // GET projection surfaces thinking for the dialog prefill.
    const listed = await listOmpProviders({ modelsPath });
    expect(listed.providers[0].models[0]).not.toHaveProperty('thinking');
  });

  test('extended managed keys: input/cost/tools round trip; null clears', async () => {
    const { modelsPath } = makeEnv();
    const result = await putOmpProvider({
      provider: {
        id: 'alpha',
        baseUrl: 'https://alpha.example.com/v1',
        models: [{
          id: 'a1',
          input: ['text', 'image'],
          supportsTools: false,
          cost: { input: 1.25, output: 10, cacheRead: 0.1, cacheWrite: 2.5 },
          contextPromotionTarget: 'alpha/a2',
          compactionModel: '@smol',
        }],
      },
    }, { modelsPath });
    expect(result.status).toBe(200);
    const written = readFileSync(modelsPath, 'utf8');
    expect(written).toContain('- image');
    expect(written).toContain('supportsTools: false');
    expect(written).toContain('cacheWrite: 2.5');
    expect(written).toContain('contextPromotionTarget: alpha/a2');
    expect(written).toContain('compactionModel: "@smol"');
    // GET projection surfaces them for prefill/badges
    const listed = await listOmpProviders({ modelsPath });
    const a1 = listed.providers[0].models[0];
    expect(a1.input).toEqual(['text', 'image']);
    expect(a1.supportsTools).toBe(false);
    expect(a1.cost.cacheWrite).toBe(2.5);
    expect(a1.contextPromotionTarget).toBe('alpha/a2');
    // null clears
    const cleared = await putOmpProvider({
      provider: {
        id: 'alpha',
        baseUrl: 'https://alpha.example.com/v1',
        models: [{ id: 'a1', input: null, supportsTools: null, cost: null, contextPromotionTarget: null, compactionModel: null }],
      },
    }, { modelsPath });
    expect(cleared.status).toBe(200);
    const w2 = readFileSync(modelsPath, 'utf8');
    expect(w2).not.toContain('- image');
    expect(w2).not.toContain('supportsTools');
    expect(w2).not.toContain('contextPromotionTarget');
  });

  test('rejects invalid payloads without touching the file', async () => {
    const { modelsPath } = makeEnv();
    const before = readFileSync(modelsPath, 'utf8');
    const cases = [
      [{ provider: { id: 'BAD ID' } }, 'provider.id'],
      [{ provider: { id: 'ok1', baseUrl: 'ftp://x.example' } }, 'baseUrl must be an http'],
      [{ provider: { id: 'ok1', baseUrl: 'https://x.example', api: 'nope' } }, 'provider.api'],
      [{ provider: { id: 'ok1', baseUrl: 'https://x.example/v1', api: 'openai-responses', models: [{}] } }, 'id is required'],
    ];
    for (const [input, fragment] of cases) {
      const result = await putOmpProvider(input, { modelsPath });
      expect(result.status).toBe(400);
      expect(result.body.message).toContain(fragment);
    }
    // SDK validation: models require baseUrl+apiKey unless auth none.
    const noKey = await putOmpProvider({
      provider: { id: 'ok1', api: 'openai-responses', models: [{ id: 'm1' }] },
    }, { modelsPath });
    expect(noKey.status).toBe(400);
    expect(readFileSync(modelsPath, 'utf8')).toBe(before);
  });

  test('409 when the id collides with an engine (builtin/login) provider', async () => {
    const { modelsPath } = makeEnv();
    const result = await putOmpProvider({
      provider: { id: 'zhipu-coding-plan', baseUrl: 'https://z.example/v1', api: 'openai-responses', apiKey: 'k', models: [{ id: 'm' }] },
    }, { modelsPath, listEngineModels: () => [{ provider: 'zhipu-coding-plan' }] });
    expect(result.status).toBe(409);
    expect(result.body.error).toBe('provider-exists-engine');
  });

  test('calls refreshModels after a successful write; first write creates the backup anchor', async () => {
    const { modelsPath } = makeEnv();
    const refreshes = [];
    const result = await putOmpProvider({
      provider: { id: 'delta', baseUrl: 'https://delta.example/v1', api: 'openai-responses', apiKey: 'dk', models: [{ id: 'd1' }] },
    }, { modelsPath, refreshModels: async () => { refreshes.push(1); } });
    expect(result.status).toBe(200);
    expect(refreshes).toHaveLength(1);
    expect(existsSync(`${modelsPath}.backup`)).toBe(true);
    const backup = readFileSync(`${modelsPath}.backup`, 'utf8');
    expect(backup).not.toContain('delta:'); // anchor = pre-GUI state
  });
});

describe('DELETE /omp/providers/{id} (deleteOmpProvider)', () => {
  test('removes a file provider, keeps others and comments; 404 unknown; refresh called', async () => {
    const { modelsPath } = makeEnv();
    const refreshes = [];
    const removed = await deleteOmpProvider({ id: 'beta' }, { modelsPath, refreshModels: async () => { refreshes.push(1); } });
    expect(removed.status).toBe(200);
    expect(refreshes).toHaveLength(1);

    const written = readFileSync(modelsPath, 'utf8');
    expect(written).not.toContain('beta:');
    expect(written).not.toContain('ak-secret-two');
    expect(written).toContain('alpha:');
    // Comments attached to the deleted provider's key go with it; comments
    // on surviving keys stay.
    expect(written).toContain('# gateway one');
    expect(written).not.toContain('# gateway two');

    const unknown = await deleteOmpProvider({ id: 'nope' }, { modelsPath });
    expect(unknown.status).toBe(404);
  });
});

describe('route mounting', () => {
  test('mounts the three routes; capability off answers explicit 501', async () => {
    const routes = [];
    const route = (method, pattern, handler) => routes.push({ method, pattern, handler });
    registerProvidersDomainRoutes(route, {
      features: { 'providers.v1': false },
      modelsPath: path.join(makeDir(), 'models.yml'),
    });
    expect(routes.map((r) => `${r.method} ${r.pattern}`)).toEqual([
      'GET /omp/providers',
      'PUT /omp/providers',
      'POST /omp/providers/{id}/fetch-models',
      'DELETE /omp/providers/{id}',
    ]);
    const get = routes[0].handler;
    const response = await get(new Request('http://host/omp/providers'));
    expect(response.status).toBe(501);
    await response.json();
  });

  test('PUT + DELETE wire through the routes', async () => {
    const { modelsPath } = makeEnv();
    const routes = [];
    registerProvidersDomainRoutes((m, p, h) => routes.push({ m, p, h }), { modelsPath });
    const put = routes.find((r) => r.m === 'PUT' && r.p === '/omp/providers').h;
    const del = routes.find((r) => r.m === 'DELETE' && r.p === '/omp/providers/{id}').h;

    const putResponse = await put(new Request('http://host/omp/providers', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        provider: { id: 'routed', baseUrl: 'https://routed.example/v1', api: 'openai-responses', apiKey: 'rk', models: [{ id: 'r1' }] },
      }),
    }));
    expect(putResponse.status).toBe(200);
    expect(readFileSync(modelsPath, 'utf8')).toContain('routed:');

    const delResponse = await del(new Request('http://host/omp/providers/routed', { method: 'DELETE' }), { params: { id: 'routed' } });
    expect(delResponse.status).toBe(200);
    expect(readFileSync(modelsPath, 'utf8')).not.toContain('routed:');
  });
});

describe('POST /omp/providers/{id}/fetch-models (fetchOmpProviderModels)', () => {
  test('queries {baseUrl}/models with the stored key and returns deduped ids', async () => {
    const { modelsPath } = makeEnv();
    const calls = [];
    const result = await fetchOmpProviderModels(
      { id: 'alpha' },
      {
        modelsPath,
        fetchImpl: async (url, init) => {
          calls.push({ url, auth: init?.headers?.Authorization });
          return new Response(JSON.stringify({ data: [{ id: 'a1' }, { id: 'a2' }, { id: 'a1' }, { no: 'id' }] }), { status: 200 });
        },
      },
    );
    expect(result.status).toBe(200);
    expect(result.body.models).toEqual(['a1', 'a2']);
    expect(calls).toEqual([{ url: 'https://alpha.example.com/v1/models', auth: 'Bearer ak-secret-one' }]);
  });

  test('draft baseUrl/apiKey overrides let an unsaved provider fetch', async () => {
    const { modelsPath } = makeEnv({ template: 'providers:\n' });
    const calls = [];
    const result = await fetchOmpProviderModels(
      { id: 'brand-new', baseUrl: 'https://draft.example.com/v1', apiKey: 'dk-1' },
      {
        modelsPath,
        fetchImpl: async (url, init) => {
          calls.push({ url, auth: init?.headers?.Authorization });
          return new Response(JSON.stringify([{ id: 'm1' }, { id: 'm2' }]), { status: 200 });
        },
      },
    );
    expect(result.status).toBe(200);
    expect(result.body.models).toEqual(['m1', 'm2']);
    expect(calls).toEqual([{ url: 'https://draft.example.com/v1/models', auth: 'Bearer dk-1' }]);
  });

  test('404 for unknown provider; 400 when the provider has no baseUrl yet', async () => {
    const { modelsPath } = makeEnv();
    const unknown = await fetchOmpProviderModels({ id: 'nope' }, { modelsPath, fetchImpl: async () => new Response('[]') });
    expect(unknown.status).toBe(404);

    const { modelsPath: emptyPath } = makeEnv({ template: 'providers:\n  bare:\n    apiKey: k\n' });
    const noUrl = await fetchOmpProviderModels({ id: 'bare' }, { modelsPath: emptyPath, fetchImpl: async () => new Response('[]') });
    expect(noUrl.status).toBe(400);
    expect(noUrl.body.error).toBe('no-base-url');
  });

  test('a 2xx with an unrecognized body is 502, not an empty success', async () => {
    const { modelsPath } = makeEnv();
    const result = await fetchOmpProviderModels(
      { id: 'alpha' },
      { modelsPath, fetchImpl: async () => new Response('<html>login page</html>', { status: 200 }) },
    );
    expect(result.status).toBe(502);
    expect(result.body.error).toBe('fetch-failed');
    expect(result.body.message).toContain('unrecognized');
  });

  test('gateway failures surface as 502 with context, never as empty success', async () => {
    const { modelsPath } = makeEnv();
    const http = await fetchOmpProviderModels({ id: 'alpha' }, { modelsPath, fetchImpl: async () => new Response('nope', { status: 401 }) });
    expect(http.status).toBe(502);
    expect(http.body.message).toContain('answered 401');
    const network = await fetchOmpProviderModels({ id: 'alpha' }, { modelsPath, fetchImpl: async () => { throw new Error('boom'); } });
    expect(network.status).toBe(502);
    expect(network.body.message).toContain('boom');
  });
});
