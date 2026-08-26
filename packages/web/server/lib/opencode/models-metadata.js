// models.dev catalog access for the web server: persistent on-disk cache +
// ETag conditional revalidation + automatic system-proxy retry.
//
// Layers (outermost first):
//   1. In-memory copy, fresh within ttlMs (shared by every consumer).
//   2. On-disk cache in the OMPChamber data dir (`models-dev.catalog.json`)
//      — survives restarts, seeds the in-memory copy on boot, and serves as
//      the stale fallback when the network (and proxy) are unreachable.
//   3. Network: conditional GET with If-None-Match (models.dev/Cloudflare
//      revalidates via ETag; 304 keeps the cached body). Direct first; on a
//      NETWORK error (blocked/reset/timeout — not an HTTP status) retry via
//      a detected proxy: env HTTPS_PROXY/HTTP_PROXY/ALL_PROXY first, then
//      macOS system proxy (scutil --proxy). The proxy attempt is a
//      hand-rolled HTTP CONNECT tunnel (node:net + node:tls, HTTP/1.1,
//      identity encoding) because desktop builds run under Node whose fetch
//      ignores proxy env (Bun's fetch honors it, which also makes the
//      direct attempt proxy-aware in dev).

import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { execFile } from 'node:child_process';

const MODELS_DEV_API_URL = 'https://models.dev/api.json';
const DEFAULT_TTL_MS = 10 * 60 * 1000;
// models.dev/api.json is a ~4MB catalog; cold fetches routinely exceed 8s.
// Every failure path falls back to cached data, so a patient timeout is safe.
const DEFAULT_TIMEOUT_MS = 20000;
const DISK_CACHE_VERSION = 1;

const cacheFilePath = () => path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'models-dev.catalog.json',
);

/** @type {{ metadata: object, etag: string | null, fetchedAt: number } | null} */
let memoryCache = null;
let diskLoaded = false;
let inflight = null;

// ─────────────────────────────────────────────────────────────────────────────
// Disk cache
// ─────────────────────────────────────────────────────────────────────────────

const loadDiskCache = (cachePath) => {
  if (diskLoaded) return;
  diskLoaded = true;
  try {
    const parsed = JSON.parse(fs.readFileSync(cachePath, 'utf8'));
    if (
      parsed?.version === DISK_CACHE_VERSION
      && parsed.data && typeof parsed.data === 'object'
      && typeof parsed.fetchedAt === 'number'
    ) {
      memoryCache = {
        metadata: parsed.data,
        etag: typeof parsed.etag === 'string' ? parsed.etag : null,
        fetchedAt: parsed.fetchedAt,
      };
    }
  } catch {
    // Missing/corrupt cache file: start empty, the network will repopulate.
  }
};

const persistDiskCache = (cachePath, entry) => {
  const payload = JSON.stringify({
    version: DISK_CACHE_VERSION,
    etag: entry.etag,
    fetchedAt: entry.fetchedAt,
    data: entry.metadata,
  });
  const temp = `${cachePath}.${process.pid}.tmp`;
  try {
    fs.mkdirSync(path.dirname(cachePath), { recursive: true });
    fs.writeFileSync(temp, payload, 'utf8');
    fs.renameSync(temp, cachePath);
  } catch {
    // Best-effort persistence; the in-memory copy still serves this process.
    try { fs.unlinkSync(temp); } catch { /* never created */ }
  }
};

// ─────────────────────────────────────────────────────────────────────────────
// Proxy detection (env first, then macOS system proxy)
// ─────────────────────────────────────────────────────────────────────────────

let scutilCache = { at: 0, value: [] };
const SCUTIL_TTL_MS = 60_000;

const httpProxyFromUrl = (raw) => {
  if (!raw) return null;
  try {
    const url = new URL(raw);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return null; // socks:// needs a different tunnel
    return { host: url.hostname, port: Number(url.port) || (url.protocol === 'https:' ? 443 : 80) };
  } catch {
    return null;
  }
};

const detectEnvProxies = () => [
  process.env.HTTPS_PROXY,
  process.env.https_proxy,
  process.env.HTTP_PROXY,
  process.env.http_proxy,
  process.env.ALL_PROXY,
  process.env.all_proxy,
].map(httpProxyFromUrl).filter(Boolean);

const detectScutilProxies = async () => {
  if (process.platform !== 'darwin') return [];
  const now = Date.now();
  if (now - scutilCache.at < SCUTIL_TTL_MS) return scutilCache.value;
  const stdout = await new Promise((resolve, reject) => {
    execFile('scutil', ['--proxy'], { timeout: 3000 }, (error, out) => (error ? reject(error) : resolve(String(out))));
  });
  const read = (key) => {
    const match = stdout.match(new RegExp(`${key}\\s*:\\s*(\\S+)`));
    return match ? match[1] : null;
  };
  const proxies = [];
  if (read('HTTPSEnable') === '1' && read('HTTPSProxy') && read('HTTPSPort')) {
    proxies.push({ host: read('HTTPSProxy'), port: Number(read('HTTPSPort')) });
  }
  if (read('HTTPEnable') === '1' && read('HTTPProxy') && read('HTTPPort')) {
    proxies.push({ host: read('HTTPProxy'), port: Number(read('HTTPPort')) });
  }
  scutilCache = { at: now, value: proxies };
  return proxies;
};

/** env proxies + (darwin) system proxies, env first. Never throws. */
export const detectProxyCandidates = async () => {
  const env = detectEnvProxies();
  let system = [];
  try {
    system = await detectScutilProxies();
  } catch {
    // scutil unavailable/timeout: env proxies (if any) still apply.
  }
  const seen = new Set();
  return [...env, ...system].filter((proxy) => {
    const key = `${proxy.host}:${proxy.port}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
};

// ─────────────────────────────────────────────────────────────────────────────
// HTTP CONNECT tunnel GET (Node- and Bun-compatible; fetch can't take a proxy)
// ─────────────────────────────────────────────────────────────────────────────

/**
 * HTTPS GET through an HTTP proxy via CONNECT. HTTP/1.1 + identity encoding
 * keeps the response framing simple (content-length delimited).
 * @returns {Promise<{ status: number, etag: string | null, body: string }>}
 */
export const httpsGetViaProxy = (urlString, proxy, timeoutMs, etag) => new Promise((resolve, reject) => {
  const target = new URL(urlString);
  const socket = net.connect({ host: proxy.host, port: proxy.port });
  let settled = false;
  const fail = (error) => {
    if (settled) return;
    settled = true;
    socket.destroy();
    reject(error);
  };
  const timer = setTimeout(() => fail(new Error(`proxy CONNECT timed out after ${timeoutMs}ms`)), timeoutMs);

  socket.once('error', fail);
  socket.once('connect', () => {
    socket.write(
      `CONNECT ${target.hostname}:443 HTTP/1.1\r\n`
      + `Host: ${target.hostname}:443\r\n`
      + 'Proxy-Connection: keep-alive\r\n\r\n',
    );
  });

  let connectBuffer = Buffer.alloc(0);
  const onConnectData = (chunk) => {
    connectBuffer = Buffer.concat([connectBuffer, chunk]);
    const headerEnd = connectBuffer.indexOf('\r\n\r\n');
    if (headerEnd === -1) {
      if (connectBuffer.length > 8192) fail(new Error('proxy CONNECT response too large'));
      return;
    }
    socket.off('data', onConnectData);
    const statusLine = connectBuffer.subarray(0, headerEnd).toString('utf8').split('\r\n')[0] || '';
    const statusCode = Number(statusLine.split(' ')[1]);
    if (!Number.isInteger(statusCode) || statusCode < 200 || statusCode >= 300) {
      fail(new Error(`proxy CONNECT rejected: ${statusLine || 'no status'}`));
      return;
    }
    const leftover = connectBuffer.subarray(headerEnd + 4);
    const tlsSocket = tls.connect({ socket, servername: target.hostname }, () => {
      tlsSocket.write(
        `GET ${target.pathname}${target.search} HTTP/1.1\r\n`
        + `Host: ${target.hostname}\r\n`
        + 'Accept: application/json\r\n'
        + 'Accept-Encoding: identity\r\n'
        + 'Connection: close\r\n'
        + (etag ? `If-None-Match: ${etag}\r\n` : '')
        + '\r\n',
      );
    });
    tlsSocket.once('error', fail);
    let responseBuffer = leftover.length > 0 ? Buffer.concat([leftover]) : Buffer.alloc(0);
    // The TLS handshake may consume bytes already read from the proxy
    // socket; TLS writes its handshake over `socket`, and any application
    // data the server sends lands on `tlsSocket` only after `secureConnect`,
    // so `leftover` (pre-handshake bytes) can only be proxy CONNECT residue.
    tlsSocket.on('data', (tlsChunk) => {
      responseBuffer = Buffer.concat([responseBuffer, tlsChunk]);
    });
    tlsSocket.once('close', () => {
      clearTimeout(timer);
      if (settled) return;
      settled = true;
      const headerEnd = responseBuffer.indexOf('\r\n\r\n');
      if (headerEnd === -1) {
        reject(new Error('empty response through proxy'));
        return;
      }
      const headerBlock = responseBuffer.subarray(0, headerEnd).toString('utf8');
      const status = Number((headerBlock.split('\r\n')[0] || '').split(' ')[1]);
      const responseEtag = /etag:\s*(.+)/i.exec(headerBlock)?.[1]?.trim() ?? null;
      const body = responseBuffer.subarray(headerEnd + 4).toString('utf8');
      if (!Number.isInteger(status)) {
        reject(new Error(`unparseable response through proxy: ${headerBlock.split('\r\n')[0]}`));
        return;
      }
      resolve({ status, etag: responseEtag, body });
    });
  };
  socket.on('data', onConnectData);
});

// ─────────────────────────────────────────────────────────────────────────────
// Fetch orchestration
// ─────────────────────────────────────────────────────────────────────────────

const parseCatalog = (bodyText) => {
  const metadata = JSON.parse(bodyText);
  if (!metadata || typeof metadata !== 'object') {
    throw new Error('models.dev returned an unexpected payload');
  }
  return metadata;
};

/**
 * Conditional catalog fetch: direct first (Bun's fetch also honors proxy env),
 * then via each detected proxy on network errors. HTTP status errors (4xx/5xx
 * from the origin) do NOT trigger the proxy retry — the origin answered.
 * @returns {Promise<{ notModified: true } | { metadata: object, etag: string | null }>}
 */
const fetchCatalog = async (url, timeoutMs, etag, options = {}) => {
  const fetchImpl = options.fetchImpl ?? (globalThis.fetch?.bind(globalThis) ?? null);
  const proxyGet = options.proxyGet ?? httpsGetViaProxy;
  const proxyCandidates = options.proxyCandidates ?? detectProxyCandidates;

  const attempts = [];
  if (fetchImpl) {
    attempts.push(async () => {
      const response = await fetchImpl(url, {
        headers: {
          Accept: 'application/json',
          ...(etag ? { 'If-None-Match': etag } : {}),
        },
        signal: AbortSignal.timeout(timeoutMs),
      });
      if (response.status === 304) return { notModified: true };
      if (!response.ok) {
        const error = new Error(`models.dev responded with status ${response.status}`);
        error.httpStatus = response.status;
        throw error;
      }
      return { metadata: parseCatalog(await response.text()), etag: response.headers.get('etag') };
    });
  }
  attempts.push(async () => {
    const candidates = await proxyCandidates();
    let lastError = new Error('no proxy configured');
    for (const proxy of candidates) {
      try {
        const { status, etag: responseEtag, body } = await proxyGet(url, proxy, timeoutMs, etag);
        if (status === 304) return { notModified: true };
        if (status < 200 || status >= 300) {
          lastError = new Error(`models.dev responded with status ${status} via proxy`);
          continue;
        }
        return { metadata: parseCatalog(body), etag: responseEtag };
      } catch (error) {
        lastError = error;
      }
    }
    throw lastError;
  });

  let lastError = null;
  for (const attempt of attempts) {
    try {
      return await attempt();
    } catch (error) {
      lastError = error;
      // An origin HTTP status error is a real answer; do not proxy-retry it.
      if (error?.httpStatus) throw error;
    }
  }
  throw lastError ?? new Error('no fetch implementation available');
};

/**
 * Returns the models.dev catalog. Fresh in-memory copy first, then a
 * conditional network refresh (ETag), seeding from and persisting to the
 * on-disk cache. On total network failure any cached copy is served stale;
 * the error only propagates when nothing has ever been cached.
 */
export async function getModelsMetadata({
  url = MODELS_DEV_API_URL,
  ttlMs = DEFAULT_TTL_MS,
  timeoutMs = DEFAULT_TIMEOUT_MS,
  cachePath = cacheFilePath(),
  ...fetchOptions
} = {}) {
  loadDiskCache(cachePath);
  const now = Date.now();
  if (memoryCache && now - memoryCache.fetchedAt < ttlMs) {
    return { metadata: memoryCache.metadata, fromCache: true };
  }

  if (!inflight) {
    inflight = (async () => {
      const etag = memoryCache?.etag ?? null;
      const result = await fetchCatalog(url, timeoutMs, etag, fetchOptions);
      const entry = result.notModified
        ? { metadata: memoryCache.metadata, etag, fetchedAt: Date.now() }
        : { metadata: result.metadata, etag: result.etag, fetchedAt: Date.now() };
      memoryCache = entry;
      persistDiskCache(cachePath, entry);
      return entry;
    })().finally(() => {
      inflight = null;
    });
  }

  try {
    const entry = await inflight;
    return { metadata: entry.metadata, fromCache: false };
  } catch (error) {
    if (memoryCache) {
      return { metadata: memoryCache.metadata, fromCache: true, stale: true };
    }
    throw error;
  }
}

/** Test seam: reset the module-level caches between cases. */
export const __resetModelsMetadataForTests = () => {
  memoryCache = null;
  diskLoaded = false;
  inflight = null;
};

export { MODELS_DEV_API_URL };
