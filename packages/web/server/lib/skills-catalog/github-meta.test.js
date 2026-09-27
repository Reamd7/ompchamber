/**
 * GitHub 仓库元数据（github-meta.js）测试：stars 与 pushed_at 解析、失败
 * 降级为 null、失败负缓存避免重复请求，以及仓库去重。fetch 以全局 mock
 * 替换，每个用例使用独立临时数据目录。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { clearGitHubMetaCache, fetchGitHubRepoMetas } from './github-meta.js';

/** 保存真实 fetch，afterEach 中恢复。 */
const originalFetch = globalThis.fetch;

/** 每个用例独立的临时数据目录（设为 OMPCHAMBER_DATA_DIR）。 */
let tempDataDir;

// 每个用例前：新建临时数据目录并让磁盘缓存指向它。
beforeEach(() => {
  tempDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'github-meta-test-'));
  process.env.OMPCHAMBER_DATA_DIR = tempDataDir;
});

// 每个用例后：还原 fetch 与环境变量、清空模块缓存并删除临时目录。
afterEach(() => {
  delete process.env.OMPCHAMBER_DATA_DIR;
  globalThis.fetch = originalFetch;
  clearGitHubMetaCache();
  vi.restoreAllMocks();
  fs.rmSync(tempDataDir, { recursive: true, force: true });
});

/** fetchGitHubRepoMetas：成功解析、失败降级、负缓存与去重。 */
describe('fetchGitHubRepoMetas', () => {
  it('returns stars and pushed_at from the GitHub API', async () => {
    const fetchMock = vi.fn(async () => new Response(
      JSON.stringify({ stargazers_count: 42, pushed_at: '2026-08-01T00:00:00Z' }),
      { status: 200 },
    ));
    globalThis.fetch = fetchMock;

    const metas = await fetchGitHubRepoMetas(['anthropics/skills']);

    expect(metas).toEqual({
      'anthropics/skills': { stars: 42, repoUpdatedAt: '2026-08-01T00:00:00Z' },
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('resolves failed lookups to null without throwing', async () => {
    globalThis.fetch = vi.fn(async () => new Response('rate limited', { status: 403 }));

    const metas = await fetchGitHubRepoMetas(['anthropics/skills']);

    expect(metas).toEqual({ 'anthropics/skills': null });
  });

  it('caches failed lookups briefly to avoid repeat hits', async () => {
    const fetchMock = vi.fn(async () => new Response('rate limited', { status: 403 }));
    globalThis.fetch = fetchMock;

    await fetchGitHubRepoMetas(['anthropics/skills']);
    const second = await fetchGitHubRepoMetas(['anthropics/skills']);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(second).toEqual({ 'anthropics/skills': { stars: null, repoUpdatedAt: null } });
  });

  it('deduplicates repositories', async () => {
    const fetchMock = vi.fn(async () => new Response(
      JSON.stringify({ stargazers_count: 1, pushed_at: null }),
      { status: 200 },
    ));
    globalThis.fetch = fetchMock;

    const metas = await fetchGitHubRepoMetas(['a/b', 'a/b', null]);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(metas['a/b']).toEqual({ stars: 1, repoUpdatedAt: null });
  });
});
