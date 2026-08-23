import { afterAll, describe, expect, test } from 'vitest';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {
  __resetModelsMetadataForTests,
  detectProxyCandidates,
  getModelsMetadata,
  httpsGetViaProxy,
} from './models-metadata.js';

const makeCacheDir = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'models-metadata-'));
  return { dir, cachePath: path.join(dir, 'models-dev.catalog.json') };
};

const dirs = [];
afterAll(() => {
  for (const dir of dirs) {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* best effort */ }
  }
});

const freshEnv = () => {
  __resetModelsMetadataForTests();
  const { dir, cachePath } = makeCacheDir();
  dirs.push(dir);
  return { cachePath };
};

const writeDisk = (cachePath, { etag = '"e1"', fetchedAt = Date.now() - 60_000_000, data = { anthropic: {} } } = {}) => {
  fs.writeFileSync(cachePath, JSON.stringify({ version: 1, etag, fetchedAt, data }), 'utf8');
};

describe('getModelsMetadata disk cache', () => {
  test('serves the disk cache with no network available (stale fallback)', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath);
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => { throw new TypeError('fetch failed'); },
      proxyCandidates: async () => [],
    });
    expect(result.fromCache).toBe(true);
    expect(result.stale).toBe(true);
    expect(result.metadata).toEqual({ anthropic: {} });
  });

  test('a fresh disk cache is served without touching the network', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath, { fetchedAt: Date.now() });
    let called = 0;
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => { called += 1; throw new TypeError('should not fetch'); },
      proxyCandidates: async () => [],
    });
    expect(result.fromCache).toBe(true);
    expect(result.stale).toBeUndefined();
    expect(called).toBe(0);
  });

  test('200 refresh persists etag + payload back to disk', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath);
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => new Response(JSON.stringify({ openai: { models: {} } }), {
        status: 200,
        headers: { etag: '"e2"' },
      }),
      proxyCandidates: async () => [],
    });
    expect(result.fromCache).toBe(false);
    expect(result.metadata).toEqual({ openai: { models: {} } });
    const disk = JSON.parse(fs.readFileSync(cachePath, 'utf8'));
    expect(disk.etag).toBe('"e2"');
    expect(disk.data).toEqual({ openai: { models: {} } });
  });
});

describe('getModelsMetadata ETag revalidation', () => {
  test('sends If-None-Match and keeps the cached body on 304', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath, { etag: '"e1"', data: { cached: true } });
    const seenHeaders = [];
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async (_url, init) => {
        seenHeaders.push(init.headers['If-None-Match']);
        return new Response(null, { status: 304 });
      },
      proxyCandidates: async () => [],
    });
    expect(seenHeaders).toEqual(['"e1"']);
    expect(result.metadata).toEqual({ cached: true });
    expect(result.fromCache).toBe(false); // revalidated, not stale fallback
    // fetchedAt updated → immediately fresh now
    const again = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => { throw new TypeError('unreached'); },
      proxyCandidates: async () => [],
    });
    expect(again.fromCache).toBe(true);
    expect(again.stale).toBeUndefined();
  });

  test('origin HTTP errors do not trigger the proxy retry (stale cache serves)', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath, { data: { cached: true } });
    let proxyTried = 0;
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => new Response('nope', { status: 503 }),
      proxyCandidates: async () => { proxyTried += 1; return [{ host: 'p', port: 1 }]; },
      proxyGet: async () => { proxyTried += 100; throw new Error('must not be called'); },
    });
    // The origin answered (503): no proxy retry, stale disk copy serves.
    expect(result).toMatchObject({ fromCache: true, stale: true });
    expect(result.metadata).toEqual({ cached: true });
    expect(proxyTried).toBe(0);
  });
});

describe('proxy fallback', () => {
  test('direct network failure retries through the detected proxy', async () => {
    const { cachePath } = freshEnv();
    writeDisk(cachePath);
    const result = await getModelsMetadata({
      cachePath,
      fetchImpl: async () => { throw new TypeError('fetch failed: socket hang up'); },
      proxyCandidates: async () => [{ host: '127.0.0.1', port: 9 }],
      proxyGet: async () => ({ status: 200, etag: '"p1"', body: JSON.stringify({ via: 'proxy' }) }),
    });
    expect(result.fromCache).toBe(false);
    expect(result.metadata).toEqual({ via: 'proxy' });
  });

  test('proxy candidates come from env and are deduped (http proxies only)', async () => {
    const previous = {
      HTTPS_PROXY: process.env.HTTPS_PROXY,
      https_proxy: process.env.https_proxy,
      ALL_PROXY: process.env.ALL_PROXY,
      all_proxy: process.env.all_proxy,
    };
    process.env.HTTPS_PROXY = 'http://127.0.0.1:7890';
    process.env.https_proxy = 'http://127.0.0.1:7890';
    process.env.ALL_PROXY = 'socks5://127.0.0.1:1080';
    try {
      const candidates = await detectProxyCandidates();
      expect(candidates).toEqual([{ host: '127.0.0.1', port: 7890 }]);
    } finally {
      for (const [key, value] of Object.entries(previous)) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
    }
  });
});

describe('httpsGetViaProxy (CONNECT tunnel)', () => {
  // A fake HTTP proxy speaking CONNECT over a plain socket, then echoing a
  // fixed HTTP/1.1 response over TLS is nontrivial; here we verify the
  // failure surface: an unreachable proxy rejects (not hangs).
  test('unreachable proxy rejects with an error', async () => {
    await expect(
      httpsGetViaProxy('https://models.dev/api.json', { host: '127.0.0.1', port: 1 }, 2000),
    ).rejects.toThrow();
  });
});
