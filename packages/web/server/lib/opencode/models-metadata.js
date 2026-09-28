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

/**
 * models.dev 目录元数据获取模块（中文总览，分层细节见上方英文注释）。
 *
 * 对外主入口 getModelsMetadata：内存缓存 -> ETag 条件网络刷新 -> 磁盘
 * 缓存播种/持久化 -> 全部失败时回退过期缓存。网络路径先直连，仅在网络
 * 层错误（非 HTTP 状态错误）时按 env 代理、macOS 系统代理的顺序用
 * 手工 HTTP CONNECT 隧道重试。detectProxyCandidates 与
 * httpsGetViaProxy 单独导出供复用与测试。
 */
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { execFile } from 'node:child_process';

/** models.dev 目录 API 地址。 */
const MODELS_DEV_API_URL = 'https://models.dev/api.json';
/** 内存缓存有效期：10 分钟内的请求直接复用缓存。 */
const DEFAULT_TTL_MS = 10 * 60 * 1000;
// models.dev/api.json is a ~4MB catalog; cold fetches routinely exceed 8s.
// Every failure path falls back to cached data, so a patient timeout is safe.
/** 网络请求超时：20 秒（见上方英文注释，冷取常超 8 秒）。 */
const DEFAULT_TIMEOUT_MS = 20000;
/** 磁盘缓存文件格式版本号，版本不匹配的旧缓存会被忽略。 */
const DISK_CACHE_VERSION = 1;

/**
 * 磁盘缓存文件路径：OMPCHAMBER_DATA_DIR（缺省
 * ~/.config/ompchamber）下的 models-dev.catalog.json。
 */
const cacheFilePath = () => path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'models-dev.catalog.json',
);

/** 内存缓存条目（数据 + ETag + 拉取时间），null 表示尚无缓存；类型见下方 @type。 */
/** @type {{ metadata: object, etag: string | null, fetchedAt: number } | null} */
let memoryCache = null;
/** 是否已尝试从磁盘加载过缓存（每进程只加载一次）。 */
let diskLoaded = false;
/** 进行中的刷新 Promise（并发去重），空闲时为 null。 */
let inflight = null;

// ─────────────────────────────────────────────────────────────────────────────
// Disk cache
// ─────────────────────────────────────────────────────────────────────────────

/**
 * 首次调用时把磁盘缓存读进内存：版本号与结构校验通过才采纳，文件
 * 缺失或损坏则静默保持空缓存（由网络重新填充）；之后调用直接返回。
 */
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

/**
 * 把缓存条目原子写入磁盘：先写 <pid>.tmp 临时文件再 rename。任何
 * 失败都静默吞掉（尽力而为，内存缓存仍可服务本进程），并尽力清理
 * 临时文件。
 */
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

/** scutil 系统代理探测结果缓存 { at, value }，有效期为 SCUTIL_TTL_MS。 */
let scutilCache = { at: 0, value: [] };
/** 系统代理探测结果缓存时长：60 秒。 */
const SCUTIL_TTL_MS = 60_000;

/**
 * 把代理 URL 字符串解析为 { host, port }：仅接受 http(s) 代理（socks
 * 需要另一种隧道，见行内注释）；端口缺省按协议取 443/80；空值或
 * 非法输入返回 null。
 */
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

/** 收集环境变量声明的代理（HTTPS/HTTP/ALL_PROXY 的大小写变体），保持声明顺序。 */
const detectEnvProxies = () => [
  process.env.HTTPS_PROXY,
  process.env.https_proxy,
  process.env.HTTP_PROXY,
  process.env.http_proxy,
  process.env.ALL_PROXY,
  process.env.all_proxy,
].map(httpProxyFromUrl).filter(Boolean);

/**
 * macOS 专用：执行 scutil --proxy 解析系统代理设置（HTTPS 与 HTTP
 * 各自启用时才收集），结果缓存 60 秒；非 darwin 平台返回空数组；
 * scutil 执行失败向上抛（由调用方决定是否忽略）。
 */
const detectScutilProxies = async () => {
  if (process.platform !== 'darwin') return [];
  const now = Date.now();
  if (now - scutilCache.at < SCUTIL_TTL_MS) return scutilCache.value;
  const stdout = await new Promise((resolve, reject) => {
    execFile('scutil', ['--proxy'], { timeout: 3000 }, (error, out) => (error ? reject(error) : resolve(String(out))));
  });
  // 从 scutil 输出中按 “key : value” 形态提取单个键值。
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
/**
 * 汇总代理候选：env 代理优先，darwin 上追加系统代理（scutil 失败时
 * 忽略，见行内注释）；按 host:port 去重；任何情况下都不抛异常。
 */
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
/**
 * 流程细节（补充上方英文注释）：与代理建 TCP 后发 CONNECT，代理响应
 * 2xx 才在原 socket 上做 TLS 握手，随后发 HTTP/1.1 GET（identity
 * 编码、Connection: close、可选 If-None-Match）；响应逐块拼接，连接
 * 关闭时解析状态行 / ETag / 响应体并 resolve。CONNECT 响应头超 8KB、
 * 状态非 2xx 或整体超时（timeoutMs）都会销毁 socket 并 reject。
 */
export const httpsGetViaProxy = (urlString, proxy, timeoutMs, etag) => new Promise((resolve, reject) => {
  const target = new URL(urlString);
  const socket = net.connect({ host: proxy.host, port: proxy.port });
  let settled = false;
  // 首次失败即收尾：销毁 socket 并 reject（幂等，重复调用无副作用）。
  const fail = (error) => {
    if (settled) return;
    settled = true;
    socket.destroy();
    reject(error);
  };
  // 整体超时定时器：从 CONNECT 阶段起算，响应关闭时清除。
  const timer = setTimeout(() => fail(new Error(`proxy CONNECT timed out after ${timeoutMs}ms`)), timeoutMs);

  socket.once('error', fail);
  socket.once('connect', () => {
    socket.write(
      `CONNECT ${target.hostname}:443 HTTP/1.1\r\n`
      + `Host: ${target.hostname}:443\r\n`
      + 'Proxy-Connection: keep-alive\r\n\r\n',
    );
  });

  // CONNECT 响应头累积缓冲，直到出现空行（头结束）为止。
  let connectBuffer = Buffer.alloc(0);
  // 处理 CONNECT 响应：定位头尾后校验状态为 2xx，剩余字节留给 TLS 层。
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
    // 完整 HTTP 响应（头 + 体）的累积缓冲；leftover 的语义见下方英文注释。
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

/** 解析目录 JSON 文本并校验根节点为对象，否则抛错（触发上层回退路径）。 */
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
/**
 * 尝试顺序与失败语义（补充上方英文注释）：fetchImpl 可用时先直连
 * （Bun 的 fetch 也识别代理环境变量），失败后逐个候选代理走 CONNECT
 * 隧道；origin 返回的 HTTP 状态错误（带 httpStatus 标记）不再换代理
 * 重试；304 统一映射为 { notModified: true }。fetchImpl / proxyGet /
 * proxyCandidates 均可注入，供测试替身使用。
 */
const fetchCatalog = async (url, timeoutMs, etag, options = {}) => {
  const fetchImpl = options.fetchImpl ?? (globalThis.fetch?.bind(globalThis) ?? null);
  const proxyGet = options.proxyGet ?? httpsGetViaProxy;
  const proxyCandidates = options.proxyCandidates ?? detectProxyCandidates;

  // 按顺序执行的取数尝试：直连 fetch 优先，其次代理隧道轮询。
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
/**
 * 参数（补充上方英文注释）：url / ttlMs / timeoutMs / cachePath 均可
 * 覆写，其余选项透传给 fetchCatalog 作为测试注入项。流程：按需加载
 * 磁盘缓存 -> TTL 内的内存缓存直接命中（fromCache: true）-> 否则
 * 共享同一个 inflight Promise 做条件刷新（304 时沿用旧数据、只刷新
 * fetchedAt）并持久化到磁盘；网络彻底失败但存在任何缓存时返回
 * stale 标记的旧数据，只有从未成功缓存过才向上抛错。
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
/** 测试缝隙：清空内存缓存、磁盘加载标记与 inflight，恢复模块初始状态。 */
export const __resetModelsMetadataForTests = () => {
  memoryCache = null;
  diskLoaded = false;
  inflight = null;
};

// 二次导出 API 地址常量，供调用方与测试直接引用。
export { MODELS_DEV_API_URL };
