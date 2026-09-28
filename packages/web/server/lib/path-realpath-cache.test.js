/**
 * createRealpathCache 的单元测试套件（vitest）。通过注入可控的 now 与
 * realpath 覆盖：成功 TTL 内的缓存复用与过期重查、同路径并发请求共享
 * in-flight Promise、默认抛出 realpath 错误、fallbackOnError 下的原路径回退
 * 与失败结果的短暂缓存。
 */
import { describe, expect, it } from 'vitest';

import { createRealpathCache } from './path-realpath-cache.js';

/** 缓存核心行为：TTL 过期、并发去重与错误处理。 */
describe('createRealpathCache', () => {
  it('caches successful realpath lookups until the success TTL expires', async () => {
    let now = 1_000;
    let calls = 0;
    const cache = createRealpathCache({
      now: () => now,
      successTtlMs: 1_000,
      realpath: async () => {
        calls += 1;
        return `/real-${calls}`;
      },
    });

    await expect(cache.resolve('/link')).resolves.toBe('/real-1');
    await expect(cache.resolve('/link')).resolves.toBe('/real-1');
    expect(calls).toBe(1);

    now += 1_001;
    await expect(cache.resolve('/link')).resolves.toBe('/real-2');
    expect(calls).toBe(2);
  });

  it('shares in-flight realpath lookups for the same path', async () => {
    let calls = 0;
    // 稍后手动放行 pending Promise 的开关，制造 in-flight 窗口。
    let release = () => undefined;
    const pending = new Promise((resolve) => {
      release = () => resolve('/real/path');
    });
    const cache = createRealpathCache({
      realpath: async () => {
        calls += 1;
        return pending;
      },
    });

    const first = cache.resolve('/link/path');
    const second = cache.resolve('/link/path');
    await Promise.resolve();

    expect(calls).toBe(1);
    release();
    await expect(Promise.all([first, second])).resolves.toEqual(['/real/path', '/real/path']);
  });

  it('throws realpath failures by default', async () => {
    const error = Object.assign(new Error('missing'), { code: 'ENOENT' });
    const cache = createRealpathCache({
      realpath: async () => {
        throw error;
      },
    });

    await expect(cache.resolve('/missing')).rejects.toBe(error);
  });

  it('can fall back to the original path and cache failures briefly', async () => {
    let calls = 0;
    const cache = createRealpathCache({
      fallbackOnError: true,
      failureTtlMs: 1_000,
      realpath: async () => {
        calls += 1;
        throw new Error('missing');
      },
    });

    await expect(cache.resolve('/missing')).resolves.toBe('/missing');
    await expect(cache.resolve('/missing')).resolves.toBe('/missing');
    expect(calls).toBe(1);
  });
});
