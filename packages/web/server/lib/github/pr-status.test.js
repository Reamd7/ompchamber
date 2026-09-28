/**
 * findBranchPrCandidates（open PR 与历史 PR 的取舍）单元测试套件。
 *
 * 通过 mock pulls.list 与模块内缓存，验证：open PR 优先且不为它花费
 * 历史查询、共享 open 列表不完整时由 per-head 查询兜底、历史记录取
 * 最新一条、历史缓存命中与过期的差异化 TTL 行为，以及次级目标不查询
 * 历史。
 */
import { afterEach, beforeEach, describe, expect, mock, test, vi } from 'bun:test';

/** octokit.rest.pulls.list 的 mock：默认返回空列表，各用例按 state/head 参数改写实现。 */
const listMock = mock(async () => ({ data: [] }));

mock.module('../git/index.js', () => ({
  getRemotes: async () => [],
  getStatus: async () => null,
}));

mock.module('./repo/index.js', () => ({
  resolveGitHubRepoFromDirectory: async () => null,
}));

mock.module('./rate-limit.js', () => ({
  noteIfGitHubRateLimit: () => {},
}));

// 在 mock.module 生效后动态加载被测模块。
const { findBranchPrCandidates, invalidateRepoPullsCache } = await import('./pr-status.js');

/** 测试数据：acme/app 仓库 feature 分支上的一条 open PR（#15）。 */
const openPr = {
  number: 15,
  state: 'open',
  head: {
    ref: 'feature',
    label: 'acme:feature',
    user: { login: 'acme' },
    repo: { owner: { login: 'acme' }, name: 'app' },
  },
};

/** 测试数据：同一 head 已合并的 PR（#12），作为历史记录候选。 */
const mergedPr = {
  number: 12,
  state: 'closed',
  merged_at: '2026-01-01T00:00:00Z',
  head: {
    ref: 'feature',
    label: 'acme:feature',
    user: { login: 'acme' },
    repo: { owner: { login: 'acme' }, name: 'app' },
  },
};

/** 测试数据：更早合并的同分支 PR（#7），用于验证历史只取最新一条。 */
const olderMergedPr = {
  ...mergedPr,
  number: 7,
  merged_at: '2025-11-01T00:00:00Z',
};

/** 以默认参数调用 findBranchPrCandidates，并允许用例按需覆盖单项。 */
const call = (overrides = {}) => findBranchPrCandidates({
  octokit: { rest: { pulls: { list: listMock } } },
  target: { repo: { owner: 'acme', repo: 'app' }, remoteName: 'origin' },
  branch: 'feature',
  sourceCandidates: [{ repo: { owner: 'acme', repo: 'app' } }],
  force: true,
  includeHistory: true,
  ...overrides,
});

/** findBranchPrCandidates 在 open PR 与历史 PR 之间的取舍行为。 */
describe('findBranchPrCandidates', () => {
  beforeEach(() => {
    listMock.mockReset();
    invalidateRepoPullsCache('acme', 'app');
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  test('an open PR wins and no history lookup is spent', async () => {
    listMock.mockImplementation(async ({ state }) => (
      state === 'open' ? { data: [openPr] } : { data: [mergedPr] }
    ));

    const { open, historical } = await call();

    expect(open?.number).toBe(15);
    expect(historical).toBeNull();
    expect(listMock.mock.calls.every((entry) => entry[0]?.state === 'open')).toBe(true);
  });

  test('an open PR still wins when the shared open list missed it', async () => {
    // A repo with more than one page of open PRs: the shared list is incomplete,
    // so the per-head query is the one that must find the open PR.
    listMock.mockImplementation(async ({ head }) => (
      head ? { data: [mergedPr, openPr] } : { data: new Array(100).fill(null).map((_, index) => ({ number: index, state: 'open', head: { ref: 'other' } })) }
    ));

    const { open, historical } = await call();

    expect(open?.number).toBe(15);
    expect(historical).toBeNull();
  });

  test('returns the branch history when no open PR exists', async () => {
    listMock.mockImplementation(async ({ head }) => (
      head ? { data: [olderMergedPr, mergedPr] } : { data: [] }
    ));

    const { open, historical } = await call();

    expect(open).toBeNull();
    // The newest past PR for the head is the relevant record.
    expect(historical?.number).toBe(12);
  });

  test('returns no history for a branch that never had a PR', async () => {
    listMock.mockImplementation(async () => ({ data: [] }));

    const { open, historical } = await call();

    expect(open).toBeNull();
    expect(historical).toBeNull();
    expect(listMock.mock.calls.some((entry) => entry[0]?.state === 'all')).toBe(true);
  });

  test('spends no call on history for a secondary target', async () => {
    listMock.mockImplementation(async ({ head }) => (
      head ? { data: [mergedPr] } : { data: [] }
    ));

    const { open, historical } = await call({ includeHistory: false });

    expect(open).toBeNull();
    expect(historical).toBeNull();
    // The complete open list already answered the only question that matters
    // for a secondary repo in the fork network.
    expect(listMock.mock.calls).toHaveLength(1);
    expect(listMock.mock.calls[0]?.[0]?.state).toBe('open');
  });

  test('reuses the cached history instead of re-querying every poll', async () => {
    listMock.mockImplementation(async ({ head }) => (
      head ? { data: [mergedPr] } : { data: [] }
    ));

    await call();
    const callsAfterFirst = listMock.mock.calls.length;

    // A non-forced poll is answered entirely from the shared open list cache
    // plus the remembered history — no extra GitHub call.
    const { open, historical } = await call({ force: false });

    expect(open).toBeNull();
    expect(historical?.number).toBe(12);
    expect(listMock.mock.calls.length).toBe(callsAfterFirst);
  });

  test('a found record outlives the shorter "no history" window', async () => {
    const startedAt = Date.now();
    listMock.mockImplementation(async ({ head }) => (
      head ? { data: [mergedPr] } : { data: [] }
    ));

    await call();
    const callsAfterFirst = listMock.mock.calls.length;

    // Past the "no history" expiry, but far short of the found-record one. The
    // shared open list is re-fetched; the history answer is not re-queried.
    vi.useFakeTimers();
    vi.setSystemTime(new Date(startedAt + 30 * 60 * 1000));
    const { historical } = await call({ force: false });

    expect(historical?.number).toBe(12);
    expect(listMock.mock.calls.length).toBe(callsAfterFirst + 1);
    expect(listMock.mock.calls.at(-1)?.[0]?.state).toBe('open');
  });

  test('re-queries a branch with no history once its shorter window passes', async () => {
    const startedAt = Date.now();
    listMock.mockImplementation(async () => ({ data: [] }));

    await call();
    const callsAfterFirst = listMock.mock.calls.length;

    vi.useFakeTimers();
    vi.setSystemTime(new Date(startedAt + 30 * 60 * 1000));
    await call({ force: false });

    expect(listMock.mock.calls.some((entry) => entry[0]?.state === 'all')).toBe(true);
    expect(listMock.mock.calls.length).toBeGreaterThan(callsAfterFirst + 1);
  });
});
