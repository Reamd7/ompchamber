/**
 * 技能目录扫描缓存：内存 Map + 跨进程共享的磁盘 JSON 双层结构，带 TTL、
 * 并发去重（inFlight）与全局并发上限（同时最多 MAX_CONCURRENT_SCANS 个扫描）。
 * 磁盘写入做 1 秒去抖并 unref 定时器，不会阻塞进程退出。
 */
import { readDiskCache, writeDiskCache } from './disk-cache.js';

/** 扫描结果的默认存活时间（TTL）：3 小时。 */
const DEFAULT_TTL_MS = 3 * 60 * 60 * 1000;
/** 磁盘缓存文件名（位于 OMPChamber 数据目录，见 disk-cache.js）。 */
const DISK_CACHE_FILE = 'skills-catalog-cache.json';
/** 全局同时进行的仓库扫描数上限，防止并发 git 克隆压垮磁盘与网络。 */
const MAX_CONCURRENT_SCANS = 2;

/** 内存缓存：cache key → { expiresAt, value }，value 为扫描结果。 */
const cache = new Map();
/** 进行中的扫描 Promise：同一 key 的并发调用共享同一次加载。 */
const inFlight = new Map();

/** 磁盘缓存是否已加载过；每个进程只加载一次。 */
let diskLoaded = false;
/** 去抖的磁盘写入定时器句柄；非 null 表示已有待执行的写入。 */
let diskWriteTimer = null;

/** 惰性把磁盘缓存加载进内存，只接受未过期且结构合法的条目。 */
const loadDiskEntries = () => {
  if (diskLoaded) {
    return;
  }
  diskLoaded = true;
  const persisted = readDiskCache(DISK_CACHE_FILE);
  if (!persisted) {
    return;
  }
  const now = Date.now();
  for (const [key, entry] of Object.entries(persisted)) {
    if (
      entry
      && typeof entry === 'object'
      && typeof entry.expiresAt === 'number'
      && entry.expiresAt > now
      && entry.value
      && typeof entry.value === 'object'
    ) {
      cache.set(key, entry);
    }
  }
};

/** 1 秒去抖地把未过期条目写回磁盘；定时器 unref，不阻止进程退出。 */
const scheduleDiskWrite = () => {
  if (diskWriteTimer) {
    return;
  }
  diskWriteTimer = setTimeout(() => {
    diskWriteTimer = null;
    const now = Date.now();
    const persisted = {};
    for (const [key, entry] of cache.entries()) {
      if (entry.expiresAt > now) {
        persisted[key] = entry;
      }
    }
    writeDiskCache(DISK_CACHE_FILE, persisted);
  }, 1000);
  if (typeof diskWriteTimer.unref === 'function') {
    diskWriteTimer.unref();
  }
};

/** 生成缓存键：normalizedRepo::subpath::identityId（各段 trim，空值归一为空串）。 */
export function getCacheKey({ normalizedRepo, subpath, identityId }) {
  const safeRepo = String(normalizedRepo || '').trim();
  const safeSubpath = String(subpath || '').trim();
  const safeIdentity = String(identityId || '').trim();
  return `${safeRepo}::${safeSubpath}::${safeIdentity}`;
}

/** 读取缓存值；命中但已过期则顺手删除并返回 null，未命中也返回 null。 */
export function getCachedScan(key) {
  loadDiskEntries();
  const entry = cache.get(key);
  if (!entry) return null;
  if (Date.now() >= entry.expiresAt) {
    cache.delete(key);
    return null;
  }
  return entry.value;
}

/** 写入缓存并调度磁盘持久化；ttlMs 非有限数字时回退到默认 TTL。 */
export function setCachedScan(key, value, ttlMs = DEFAULT_TTL_MS) {
  const ttl = Number.isFinite(ttlMs) ? ttlMs : DEFAULT_TTL_MS;
  cache.set(key, { expiresAt: Date.now() + ttl, value });
  scheduleDiskWrite();
}

/** 清空内存缓存与进行中索引（主要供测试使用）。 */
export function clearCache() {
  cache.clear();
  inFlight.clear();
}

// ─── Concurrency-limited scan orchestration ───

/** 当前正在运行的扫描数量。 */
let activeScans = 0;
/** 等待获取扫描槽位的 resolve 回调队列（FIFO）。 */
const scanQueue = [];

/** 排队获取一个扫描槽位；拿不到时 Promise 挂起，由 pumpScanQueue 唤醒。 */
const acquireScanSlot = () => new Promise((resolve) => {
  scanQueue.push(resolve);
  pumpScanQueue();
});

/** 归还一个扫描槽位并唤醒队列中的下一个等待者。 */
const releaseScanSlot = () => {
  activeScans -= 1;
  pumpScanQueue();
};

/** 在 MAX_CONCURRENT_SCANS 上限内尽可能放行排队等待的扫描。 */
const pumpScanQueue = () => {
  while (activeScans < MAX_CONCURRENT_SCANS && scanQueue.length > 0) {
    const resolve = scanQueue.shift();
    activeScans += 1;
    resolve();
  }
};

/**
 * Run `loader` for a scan cache key with deduplication and a global
 * concurrency limit. Concurrent callers for the same key share one loader
 * run; at most MAX_CONCURRENT_SCANS loaders run at once. Only successful
 * (`ok: true`) results are cached.
 */
/**
 * 带缓存、去重与全局并发上限地执行 loader（详细英文说明见上）。
 * refresh 为 true 时跳过缓存读取强制重新扫描；仅 ok: true 的结果会被缓存。
 */
export async function scanWithCache(key, loader, { refresh = false } = {}) {
  if (!refresh) {
    const cached = getCachedScan(key);
    if (cached) {
      return cached;
    }
  }

  const existing = inFlight.get(key);
  if (existing) {
    return existing;
  }

  const run = (async () => {
    await acquireScanSlot();
    try {
      const result = await loader();
      if (result && result.ok) {
        setCachedScan(key, result);
      }
      return result;
    } finally {
      releaseScanSlot();
      inFlight.delete(key);
    }
  })();

  inFlight.set(key, run);
  return run;
}
