import { beforeEach, describe, expect, mock, test } from 'bun:test';
import type { CreateWorktreeArgs, ProjectRef } from '@/lib/worktrees/worktreeManager';

interface MockBranchTracking {
  main?: string;
}

const project: ProjectRef = { id: 'project-1', path: '/repo' };

let fetchSourceEnabled = true;
let projectRoot = '/repo';
let gitStatus: {
  current: string;
  tracking: string | null;
  ahead: number;
  behind: number;
} | null = null;
let branchTracking: MockBranchTracking = {};
let gitFetchError: Error | null = null;
let gitFetchSuccess = true;
const gitFetchCalls: Array<{ directory: string; remote?: string; branch?: string }> = [];
const createdPayloads: CreateWorktreeArgs[] = [];
const toastWarnings: string[] = [];

mock.module('@/components/ui', () => ({
  toast: {
    warning: (message: string) => {
      toastWarnings.push(message);
    },
  },
}));

mock.module('@/stores/useConfigStore', () => ({
  useConfigStore: {
    getState: () => ({ settingsWorktreeFetchSource: fetchSourceEnabled }),
  },
}));

mock.module('@/lib/gitApi', () => ({
  getGitStatus: () => (gitStatus ? Promise.resolve(gitStatus) : Promise.reject(new Error('no status'))),
  getGitBranches: () => Promise.resolve({
    all: [],
    current: '',
    branches: branchTracking.main
      ? { main: { current: false, name: 'main', commit: 'abc1234', label: '', tracking: branchTracking.main } }
      : {},
  }),
  gitFetch: (directory: string, options: { remote?: string; branch?: string } = {}) => {
    gitFetchCalls.push({ directory, ...options });
    if (gitFetchError) {
      return Promise.reject(gitFetchError);
    }
    return Promise.resolve({ success: gitFetchSuccess });
  },
}));

mock.module('@/lib/worktrees/worktreeStatus', () => ({
  resolveProjectRoot: (directory: string) => Promise.resolve(projectRoot || directory),
  invalidateResolvedProjectRootCache: mock(),
  getRootBranch: () => Promise.resolve(gitStatus?.current || 'HEAD'),
}));

mock.module('@/lib/worktrees/worktreeManager', () => ({
  createWorktree: (_project: ProjectRef, args: CreateWorktreeArgs) => {
    createdPayloads.push(args);
    return Promise.resolve({
      source: 'sdk',
      name: 'wt',
      path: '/repo/.oc/worktrees/wt',
      projectDirectory: '/repo',
      branch: 'wt',
      label: 'wt',
      worktreeRoot: '/repo/.oc/worktrees/wt',
      worktreeStatus: 'ready',
      headState: 'branch',
      worktreeSource: 'created-for-session',
    });
  },
}));

const { createWorktreeWithDefaults, withWorktreeFetchedStartRef } = await import('./worktreeCreate');

const baseArgs = (overrides: CreateWorktreeArgs = {}): CreateWorktreeArgs => ({
  preferredName: 'openchamber/feature',
  mode: 'new',
  branchName: 'openchamber/feature',
  worktreeName: 'openchamber/feature',
  ...overrides,
});

describe('withWorktreeFetchedStartRef', () => {
  beforeEach(() => {
    fetchSourceEnabled = true;
    projectRoot = '/repo';
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 0 };
    branchTracking = {};
    gitFetchError = null;
    gitFetchSuccess = true;
    gitFetchCalls.length = 0;
    createdPayloads.length = 0;
    toastWarnings.length = 0;
  });

  test('bases the new worktree on the fetched remote-tracking ref', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };

    const args = await withWorktreeFetchedStartRef(project, baseArgs());

    expect(args.startRef).toBe('remotes/origin/main');
    expect(gitFetchCalls).toEqual([{ directory: '/repo', remote: 'origin', branch: 'main' }]);
  });

  test('fetches the branch the explicit start ref names when it is the current branch', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 3 };

    const args = await withWorktreeFetchedStartRef(project, baseArgs({ startRef: 'main' }));

    expect(args.startRef).toBe('remotes/origin/main');
    expect(gitFetchCalls).toHaveLength(1);
  });

  test('falls back to the original args when the fetch fails', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };
    gitFetchError = new Error('network down');

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(resolved.startRef).toBe(undefined);
    expect(gitFetchCalls).toHaveLength(1);
    expect(toastWarnings).toHaveLength(1);
  });

  test('falls back to the original args when the fetch resolves unsuccessful', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };
    gitFetchSuccess = false;

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(resolved.startRef).toBe(undefined);
    expect(gitFetchCalls).toHaveLength(1);
    expect(toastWarnings).toHaveLength(1);
  });

  test('does not fetch when disabled in config', async () => {
    fetchSourceEnabled = false;

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
    expect(toastWarnings).toHaveLength(0);
  });

  test('does not fetch when the base branch has local-only commits', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 2, behind: 15 };

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('does not fetch without upstream tracking', async () => {
    gitStatus = { current: 'main', tracking: null, ahead: 0, behind: 0 };

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('does not fetch when git status is unavailable', async () => {
    gitStatus = null;

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('keeps an explicit non-root local start ref untouched', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };

    const args = baseArgs({ startRef: 'release/1.2' });
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('keeps an explicit remote start ref untouched', async () => {
    const args = baseArgs({ startRef: 'remotes/origin/main' });
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('keeps a commit SHA start ref untouched', async () => {
    const sha = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0';
    const args = baseArgs({ startRef: sha });
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('does not fetch in existing mode', async () => {
    const args = baseArgs({ mode: 'existing', existingBranch: 'origin/feature' });
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('does not fetch on a detached root checkout', async () => {
    gitStatus = { current: '', tracking: null, ahead: 0, behind: 0 };

    const args = baseArgs();
    const resolved = await withWorktreeFetchedStartRef(project, args);

    expect(resolved).toBe(args);
    expect(gitFetchCalls).toHaveLength(0);
  });

  test('reads the root checkout state, not the project directory state', async () => {
    projectRoot = '/primary';
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };

    const args = await withWorktreeFetchedStartRef(project, baseArgs());

    expect(args.startRef).toBe('remotes/origin/main');
    expect(gitFetchCalls).toEqual([{ directory: '/repo', remote: 'origin', branch: 'main' }]);
  });
});

describe('createWorktreeWithDefaults fetch integration', () => {
  beforeEach(() => {
    fetchSourceEnabled = true;
    projectRoot = '/repo';
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 0 };
    branchTracking = { main: 'origin/main' };
    gitFetchError = null;
    gitFetchSuccess = true;
    gitFetchCalls.length = 0;
    createdPayloads.length = 0;
    toastWarnings.length = 0;
  });

  test('sets the new branch\'s own upstream when refreshed from remote', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };

    await createWorktreeWithDefaults(project, baseArgs());

    expect(createdPayloads).toHaveLength(1);
    expect(createdPayloads[0].startRef).toBe('remotes/origin/main');
    expect(createdPayloads[0].setUpstream).toBe(true);
    expect(createdPayloads[0].upstreamRemote).toBe('origin');
    expect(createdPayloads[0].upstreamBranch).toBe('openchamber/feature');
  });

  test('keeps upstream defaults when the start ref is untouched', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 2, behind: 0 };

    await createWorktreeWithDefaults(project, baseArgs());

    expect(createdPayloads).toHaveLength(1);
    expect(createdPayloads[0].startRef).toBe(undefined);
    expect(createdPayloads[0].setUpstream).toBe(true);
    expect(createdPayloads[0].upstreamRemote).toBe('origin');
    expect(createdPayloads[0].upstreamBranch).toBe('openchamber/feature');
  });

  test('keeps upstream defaults when the fetch falls back', async () => {
    gitStatus = { current: 'main', tracking: 'origin/main', ahead: 0, behind: 15 };
    gitFetchError = new Error('network down');

    await createWorktreeWithDefaults(project, baseArgs());

    expect(createdPayloads).toHaveLength(1);
    expect(createdPayloads[0].startRef).toBe(undefined);
    expect(createdPayloads[0].setUpstream).toBe(true);
    expect(createdPayloads[0].upstreamRemote).toBe('origin');
    expect(createdPayloads[0].upstreamBranch).toBe('openchamber/feature');
  });

  test('passes an explicit remote start ref through with upstream defaults as before', async () => {
    await createWorktreeWithDefaults(project, baseArgs({ startRef: 'remotes/origin/main' }));

    expect(createdPayloads).toHaveLength(1);
    expect(createdPayloads[0].startRef).toBe('remotes/origin/main');
    expect(createdPayloads[0].setUpstream).toBe(true);
    expect(createdPayloads[0].upstreamRemote).toBe('origin');
    expect(createdPayloads[0].upstreamBranch).toBe('openchamber/feature');
    expect(gitFetchCalls).toHaveLength(0);
  });
});
