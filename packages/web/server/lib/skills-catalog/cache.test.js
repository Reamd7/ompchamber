/**
 * 技能扫描缓存（cache.js）测试：同 key 并发去重、全局并发上限、失败不
 * 入缓存、refresh 强制重扫，以及成功结果落盘。每个用例使用独立临时数据目录。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { clearCache, scanWithCache, setCachedScan, getCachedScan } from './cache.js';

/** 每个用例独立的临时数据目录（设为 OMPCHAMBER_DATA_DIR）。 */
let tempDataDir;

// 每个用例前：新建临时数据目录并让磁盘缓存指向它。
beforeEach(() => {
  tempDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'skills-cache-test-'));
  process.env.OMPCHAMBER_DATA_DIR = tempDataDir;
});

// 每个用例后：还原环境变量、清空模块缓存并删除临时目录。
afterEach(() => {
  delete process.env.OMPCHAMBER_DATA_DIR;
  clearCache();
  vi.restoreAllMocks();
  fs.rmSync(tempDataDir, { recursive: true, force: true });
});

/** 等待 1.2 秒，跨过缓存模块 1 秒去抖的磁盘写入窗口。 */
const flushDiskWrites = async () => new Promise((resolve) => setTimeout(resolve, 1200));

/** scanWithCache：去重、限流、失败与刷新语义、磁盘持久化。 */
describe('scanWithCache', () => {
  it('deduplicates concurrent loaders for the same key', async () => {
    const loader = vi.fn(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
      return { ok: true, items: [] };
    });

    const [a, b] = await Promise.all([
      scanWithCache('k', loader),
      scanWithCache('k', loader),
    ]);

    expect(loader).toHaveBeenCalledTimes(1);
    expect(a).toEqual(b);
  });

  it('limits concurrent scans across different keys', async () => {
    let running = 0;
    let peak = 0;
    const loader = async () => {
      running += 1;
      peak = Math.max(peak, running);
      await new Promise((resolve) => setTimeout(resolve, 20));
      running -= 1;
      return { ok: true, items: [] };
    };

    await Promise.all(Array.from({ length: 6 }, (_, i) => scanWithCache(`key-${i}`, loader)));

    expect(peak).toBeLessThanOrEqual(2);
  });

  it('does not cache failed scans', async () => {
    await scanWithCache('bad', async () => ({ ok: false, error: { kind: 'networkError', message: 'x' } }));

    expect(getCachedScan('bad')).toBeNull();
  });

  it('refresh bypasses the cache', async () => {
    setCachedScan('fresh', { ok: true, items: ['cached'] });

    const result = await scanWithCache('fresh', async () => ({ ok: true, items: ['reloaded'] }), { refresh: true });

    expect(result.items).toEqual(['reloaded']);
    expect(getCachedScan('fresh').items).toEqual(['reloaded']);
  });

  it('persists successful scans to disk for later processes', async () => {
    await scanWithCache('persisted', async () => ({ ok: true, items: [{ skillName: 'x' }] }));
    await flushDiskWrites();

    const onDisk = JSON.parse(fs.readFileSync(path.join(tempDataDir, 'skills-catalog-cache.json'), 'utf8'));
    expect(onDisk.persisted.value.items).toEqual([{ skillName: 'x' }]);
  });
});
