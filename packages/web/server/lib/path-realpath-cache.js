/**
 * realpath 结果缓存工厂：包装一个异步 realpath 实现，以路径字符串为键缓存
 * 解析结果，用于削减高频重复的路径解析 IO。支持成功/失败不同的 TTL、容量
 * 上限（超出淘汰最旧）、同路径并发去重（共享 in-flight Promise）以及可选的
 * 失败回退原路径。
 */
/**
 * 创建一个 realpath 缓存实例。
 * @param {object} [options]
 * @param {Function} [options.realpath] 实际执行解析的异步函数；未提供时 resolve 原样返回输入
 * @param {number} [options.successTtlMs=600_000] 成功结果的缓存时长
 * @param {number} [options.failureTtlMs=60_000] 失败结果的缓存时长（避免对坏路径反复打 IO）
 * @param {number} [options.maxEntries=256] 缓存容量上限；<=0 或非有限数时禁用缓存
 * @param {() => number} [options.now] 时间源，可注入以便测试
 * @returns {{ resolve: (value: string) => Promise<string>, clear: () => void, size: () => number }}
 */
export const createRealpathCache = ({
  realpath,
  successTtlMs = 600_000,
  failureTtlMs = 60_000,
  maxEntries = 256,
  fallbackOnError = false,
  now = () => Date.now(),
} = {}) => {
  const cache = new Map();
  const resolveRealpath = typeof realpath === 'function' ? realpath : null;

  /** 淘汰最旧条目直到条目数回到 maxEntries 以内（Map 的迭代顺序即插入顺序）。 */
  const prune = () => {
    while (cache.size > maxEntries) {
      const oldestKey = cache.keys().next().value;
      if (oldestKey === undefined) {
        return;
      }
      cache.delete(oldestKey);
    }
  };

  /** 写入一条缓存：先删后插以刷新淘汰位置，按 ttl 计算过期时刻；缓存被禁用时跳过。 */
  const remember = (key, entry, ttlMs) => {
    if (!Number.isFinite(maxEntries) || maxEntries <= 0) {
      return;
    }
    cache.delete(key);
    cache.set(key, { ...entry, expiresAt: now() + Math.max(0, ttlMs) });
    prune();
  };

  /**
   * 解析单条路径（带缓存）。命中未过期条目时直接复用：失败条目在 fallbackOnError
   * 模式下返回原路径、否则重抛缓存的原错误；命中仍在进行的 Promise 时直接共享，
   * 避免并发重复解析。未命中时发起解析：成功缓存 successTtlMs，失败缓存
   * failureTtlMs，并以临时 Promise 占位。输入非字符串或未配置 realpath 时原样返回。
   */
  const resolve = async (value) => {
    if (typeof value !== 'string' || value.length === 0 || !resolveRealpath) {
      return value;
    }

    const cached = cache.get(value);
    const currentTime = now();
    if (cached?.promise) {
      return cached.promise;
    }
    if (cached && cached.expiresAt > currentTime) {
      cache.delete(value);
      cache.set(value, cached);
      if (cached.error) {
        if (fallbackOnError) {
          return value;
        }
        throw cached.error;
      }
      return cached.value;
    }
    if (cached) {
      cache.delete(value);
    }

    const promise = Promise.resolve()
      .then(() => resolveRealpath(value))
      .then((resolved) => {
        const next = typeof resolved === 'string' && resolved.length > 0 ? resolved : value;
        remember(value, { value: next }, successTtlMs);
        return next;
      })
      .catch((error) => {
        remember(value, { value, error }, failureTtlMs);
        if (!fallbackOnError) {
          throw error;
        }
        return value;
      });

    if (Number.isFinite(maxEntries) && maxEntries > 0) {
      cache.delete(value);
      cache.set(value, { value, expiresAt: 0, promise });
      prune();
    }

    return promise;
  };

  return {
    resolve,
    /** 清空全部缓存条目。 */
    clear: () => cache.clear(),
    /** 返回当前缓存条目数。 */
    size: () => cache.size,
  };
};
