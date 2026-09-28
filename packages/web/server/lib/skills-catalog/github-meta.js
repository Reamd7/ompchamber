/**
 * GitHub 仓库元数据（stars、最近 push 时间）查询：供技能目录做可选的展示
 * 增强。内存 + 磁盘双层 TTL 缓存、inFlight 去重；失败写入短暂负缓存，
 * 任何情况下都不阻塞目录响应（fetch 超时远小于目录路由的请求期限）。
 */
import { readDiskCache, writeDiskCache } from './disk-cache.js';

/** GitHub REST API 根地址。 */
const GITHUB_API_BASE = 'https://api.github.com';
/** 成功结果的缓存存活时间：3 小时。 */
const CACHE_TTL_MS = 3 * 60 * 60 * 1000;
/** 失败结果的负缓存存活时间：5 分钟。 */
const FAILURE_CACHE_TTL_MS = 5 * 60 * 1000;
// Keep well under the catalog route's client request deadline so optional
// metadata enrichment can never abort catalog loading.
/** 单次元数据请求的超时：1.5 秒，必须远小于目录路由的客户端请求期限。 */
const FETCH_TIMEOUT_MS = 1500;
/** 磁盘缓存文件名（位于 OMPChamber 数据目录，见 disk-cache.js）。 */
const DISK_CACHE_FILE = 'skills-github-meta.json';

/** 内存缓存：owner/repo → { expiresAt, value }。 */
const metaCache = new Map();
/** 进行中的请求 Promise：同一仓库的并发查询共享一次 fetch。 */
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
  for (const [repo, entry] of Object.entries(persisted)) {
    if (
      entry
      && typeof entry === 'object'
      && typeof entry.expiresAt === 'number'
      && entry.expiresAt > now
      && entry.value
      && typeof entry.value === 'object'
    ) {
      metaCache.set(repo, entry);
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
    for (const [repo, entry] of metaCache.entries()) {
      if (entry.expiresAt > now) {
        persisted[repo] = entry;
      }
    }
    writeDiskCache(DISK_CACHE_FILE, persisted);
  }, 1000);
  if (typeof diskWriteTimer.unref === 'function') {
    diskWriteTimer.unref();
  }
};

/** 从 GitHub API 响应中提取 stars（stargazers_count）与最近 push 时间；字段缺失或非法记为 null。 */
const parseMeta = (payload) => {
  if (!payload || typeof payload !== 'object') {
    return null;
  }
  const pushedAt = payload.pushed_at;
  return {
    stars: Number.isFinite(payload.stargazers_count) ? payload.stargazers_count : null,
    repoUpdatedAt: typeof pushedAt === 'string' && pushedAt ? pushedAt : null,
  };
};

/**
 * 查询单个仓库的元数据（缓存 + 去重）。命中未过期缓存直接返回；HTTP 非
 * 2xx 或 fetch 抛错（含超时）时写入短暂负缓存并返回 null；成功则按
 * CACHE_TTL_MS 缓存。本函数永不 reject。
 */
const fetchRepoMeta = async (normalizedRepo) => {
  loadDiskEntries();
  const cached = metaCache.get(normalizedRepo);
  if (cached && Date.now() < cached.expiresAt) {
    return cached.value;
  }

  const existing = inFlight.get(normalizedRepo);
  if (existing) {
    return existing;
  }

  const run = (async () => {
    try {
      const response = await fetch(`${GITHUB_API_BASE}/repos/${normalizedRepo}`, {
        headers: { Accept: 'application/vnd.github+json' },
        signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
      });
      if (!response.ok) {
        // Cache failures briefly so repeated catalog loads do not re-hit a
        // rate-limited or failing API for the same repository.
        metaCache.set(normalizedRepo, {
          expiresAt: Date.now() + FAILURE_CACHE_TTL_MS,
          value: { stars: null, repoUpdatedAt: null },
        });
        scheduleDiskWrite();
        return null;
      }

      const value = parseMeta(await response.json());
      if (value) {
        metaCache.set(normalizedRepo, { expiresAt: Date.now() + CACHE_TTL_MS, value });
        scheduleDiskWrite();
      }
      return value;
    } catch {
      metaCache.set(normalizedRepo, {
        expiresAt: Date.now() + FAILURE_CACHE_TTL_MS,
        value: { stars: null, repoUpdatedAt: null },
      });
      scheduleDiskWrite();
      return null;
    } finally {
      inFlight.delete(normalizedRepo);
    }
  })();

  inFlight.set(normalizedRepo, run);
  return run;
};

/**
 * Fetch GitHub repository metadata (stars, last push) for a list of
 * `owner/repo` strings. Best-effort: failed lookups resolve to null and
 * never block the catalog response.
 */
/** 批量查询一组仓库元数据（先去重再并发），返回 repo → 元数据或 null 的映射。 */
export async function fetchGitHubRepoMetas(normalizedRepos) {
  const unique = [...new Set(normalizedRepos.filter(Boolean))];
  const entries = await Promise.all(unique.map(async (repo) => [repo, await fetchRepoMeta(repo)]));
  return Object.fromEntries(entries);
}

/** For tests only: clear the in-memory repository metadata cache. */
/** 仅供测试：清空内存缓存，并置 diskLoaded 防止后续用例串读磁盘。 */
export function clearGitHubMetaCache() {
  metaCache.clear();
  inFlight.clear();
  diskLoaded = true;
}
