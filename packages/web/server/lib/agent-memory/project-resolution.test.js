/**
 * 项目归属解析 project-resolution.js 的测试套件。
 *
 * 用桩替换项目列表与 git worktree 解析，验证：worktree 归并到所属项
 * 目、已注册目录保持自身、托管 Chats 目录共享根存储、路径规整不分裂
 * 存储，以及项目列表或 git 不可用时仍能收敛而不是失败。
 */
import { describe, expect, test } from 'bun:test';

import { createMemoryProjectResolver } from './project-resolution.js';
import { createProjectIdFromPath } from '../projects/project-id.js';

/** 桩中的主项目目录（模拟用户配置的正式 checkout）。 */
const PROJECT = '/Users/x/projects/openchamber';
/** 模拟同一仓库的 git worktree 路径（OpenCode 托管 worktree 的布局）。 */
const WORKTREE = '/Users/x/.local/share/opencode/worktree/abc/jammy-koala';

/** 造一个使用默认桩（项目列表、worktree 解析）的解析器；overrides 可逐项替换。 */
const createResolver = (overrides = {}) => createMemoryProjectResolver({
  listProjectPaths: async () => [PROJECT],
  resolvePrimaryWorktreeRoot: async (directory) => (
    directory === WORKTREE ? { root: PROJECT } : { root: directory }
  ),
  ...overrides,
});

// 会话目录 → 项目的常规解析：worktree 归并、显式注册优先、路径规整。
describe('resolving a session directory to its project', () => {
  test('a worktree resolves to the project it belongs to', async () => {
    const resolve = createResolver();

    // The bug this exists for: keyed by its own path, a worktree wrote memory
    // into a project the panel never reads.
    expect(await resolve(WORKTREE)).toBe(createProjectIdFromPath(PROJECT));
  });

  test('the project directory resolves to itself', async () => {
    const resolve = createResolver();

    expect(await resolve(PROJECT)).toBe(createProjectIdFromPath(PROJECT));
  });

  test('every worktree of one repository shares a store', async () => {
    const second = '/Users/x/.local/share/opencode/worktree/abc/other';
    const resolve = createResolver({
      resolvePrimaryWorktreeRoot: async () => ({ root: PROJECT }),
    });

    expect(await resolve(WORKTREE)).toBe(await resolve(second));
  });

  test('a worktree registered as a project in its own right keeps its own store', async () => {
    // The user's explicit choice wins over the git topology.
    const resolve = createResolver({ listProjectPaths: async () => [PROJECT, WORKTREE] });

    expect(await resolve(WORKTREE)).toBe(createProjectIdFromPath(WORKTREE));
  });

  test('a directory outside any repository keys by itself', async () => {
    const resolve = createResolver();

    expect(await resolve('/tmp/loose')).toBe(createProjectIdFromPath('/tmp/loose'));
  });

  test('managed chat session directories share the Chats root store', async () => {
    const chatsRoot = '/Users/x/.config/ompchamber/chats';
    const resolve = createResolver({ managedProjectRoots: [chatsRoot] });

    expect(await resolve(`${chatsRoot}/2026-08-21/session-a`)).toBe(createProjectIdFromPath(chatsRoot));
    expect(await resolve(`${chatsRoot}/2026-08-21/session-b`)).toBe(createProjectIdFromPath(chatsRoot));
    expect(await resolve('/Users/x/.config/ompchamber/chats-other/session-a')).not.toBe(createProjectIdFromPath(chatsRoot));
  });

  test('no directory resolves to nothing rather than to some default project', async () => {
    const resolve = createResolver();

    expect(await resolve('')).toBe('');
    expect(await resolve(null)).toBe('');
  });

  test('trailing slashes and relative segments do not fork the store', async () => {
    const resolve = createResolver();

    expect(await resolve(`${PROJECT}/`)).toBe(createProjectIdFromPath(PROJECT));
    expect(await resolve(`${PROJECT}/packages/..`)).toBe(createProjectIdFromPath(PROJECT));
  });
});

// 依赖不可用时的回退：项目列表读不出或 git 缺席都不丢失记忆归属。
describe('when something is unavailable', () => {
  test('an unreadable project list still converges worktrees on the repository', async () => {
    const resolve = createResolver({
      listProjectPaths: async () => { throw new Error('settings unreadable'); },
    });

    expect(await resolve(WORKTREE)).toBe(createProjectIdFromPath(PROJECT));
  });

  test('git being unavailable falls back to the directory instead of failing', async () => {
    const resolve = createResolver({
      resolvePrimaryWorktreeRoot: async () => { throw new Error('git missing'); },
    });

    expect(await resolve(WORKTREE)).toBe(createProjectIdFromPath(WORKTREE));
  });
});
