import { beforeEach, describe, expect, mock, test } from 'bun:test';
import type { WorktreeMetadata } from '@/types/worktree';

// This module is pure orchestration around captured collaborators: the event
// source, the worktree listing, and the two stores. Each is mocked at the
// boundary; `./worktreeManager` gets a stub for partition/equality so the
// assertions target the refresh contract (invalidation, failure preservation,
// empty-success removal, runtime-switch discard), not partitioning itself.
// Time is deterministic: coalesceMs 0 makes the debounce fire on the next
// macrotask, and flushAsync() yields exactly that many ticks.

type CapturedListener = (event: { type: string; directories?: string[] }) => void;

let capturedListener: CapturedListener | null = null;
let unsubscribeCalls = 0;
let runtimeKey = 'runtime-1';

const projectsState = {
  projects: [
    { id: 'p-1', path: '/repo/main' },
    { id: 'p-2', path: '/repo/other' },
  ],
};

const sessionUiState = {
  availableWorktreesByProject: new Map<string, WorktreeMetadata[]>(),
  availableWorktrees: [] as WorktreeMetadata[],
};
let invalidateCalls: string[] = [];
let listCalls: string[] = [];
let listImpl: ((projectDirectory: string) => Promise<WorktreeMetadata[]>) | null = null;

const worktree = (path: string, branch: string): WorktreeMetadata => ({
  source: 'sdk',
  name: branch,
  path,
  projectDirectory: '',
  branch,
  label: branch,
  worktreeRoot: '',
  worktreeStatus: 'ready',
  headState: 'branch',
  worktreeSource: 'existing',
});

mock.module('@/lib/openchamberEvents', () => ({
  subscribeOpenchamberEvents: (listener: CapturedListener) => {
    capturedListener = listener;
    return () => {
      unsubscribeCalls += 1;
      capturedListener = null;
    };
  },
}));

mock.module('@/lib/runtime-switch', () => ({
  getRuntimeKey: () => runtimeKey,
}));

mock.module('@/lib/background-network', () => ({
  runBackgroundNetworkTask: <T,>(task: () => Promise<T>): Promise<T> => task(),
}));

mock.module('@/stores/useProjectsStore', () => ({
  useProjectsStore: {
    getState: () => projectsState,
  },
}));

mock.module('@/sync/session-ui-store', () => ({
  useSessionUIStore: {
    getState: () => sessionUiState,
    setState: (patch: Partial<typeof sessionUiState>) => {
      Object.assign(sessionUiState, patch);
    },
  },
}));

mock.module('@/components/session/sidebar/worktreeFirstSeen', () => ({
  recordWorktreesSeen: mock(),
}));

mock.module('./worktreeManager', () => ({
  invalidateWorktreeList: (projectDirectory: string) => {
    invalidateCalls.push(projectDirectory);
  },
  listProjectWorktrees: (project: { id: string; path: string }) => {
    listCalls.push(project.path);
    return listImpl ? listImpl(project.path) : Promise.resolve([]);
  },
  // Stub partitioning: each project keeps exactly the entries its path key
  // holds in the map, mirroring the real "one bucket per project" shape the
  // refresh contract depends on.
  partitionWorktreesByRegisteredProject: (
    projects: Array<{ path: string }>,
    worktreesByProjectMap: Map<string, WorktreeMetadata[]>,
  ) => {
    const partitioned = new Map<string, WorktreeMetadata[]>();
    for (const project of projects) {
      const entries = worktreesByProjectMap.get(project.path);
      if (entries) partitioned.set(project.path, entries);
    }
    return partitioned;
  },
  worktreeMapsEqual: (
    a: Map<string, WorktreeMetadata[]>,
    b: Map<string, WorktreeMetadata[]>,
  ) => JSON.stringify([...a.entries()]) === JSON.stringify([...b.entries()]),
}));

/** Yield pending macrotasks (the coalesce timer) plus microtask continuations. */
const flushAsync = async (ticks = 4): Promise<void> => {
  for (let i = 0; i < ticks; i += 1) {
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
};

const emitWorktreesChanged = (directories: string[]) => {
  capturedListener?.({ type: 'worktrees-changed', directories });
};

const acquire = async () => {
  const { acquireWorktreeEventSync } = await import('./worktreeEventSync');
  return acquireWorktreeEventSync({ coalesceMs: 0 });
};

describe('worktree event sync', () => {
  beforeEach(() => {
    capturedListener = null;
    unsubscribeCalls = 0;
    runtimeKey = 'runtime-1';
    sessionUiState.availableWorktreesByProject = new Map();
    sessionUiState.availableWorktrees = [];
    invalidateCalls = [];
    listCalls = [];
    listImpl = null;
  });

  test('refreshes the shared map for the affected registered project', async () => {
    const release = await acquire();
    const first = worktree('/repo/main/.wt/feature', 'feature');
    listImpl = async () => [first];

    emitWorktreesChanged(['/repo/main']);
    await flushAsync();

    expect(invalidateCalls).toEqual(['/repo/main']);
    expect(sessionUiState.availableWorktreesByProject.get('/repo/main')).toEqual([first]);
    expect(sessionUiState.availableWorktrees).toEqual([first]);
    release();
  });

  test('ignores directories that are not registered projects', async () => {
    const release = await acquire();
    listImpl = async () => [worktree('/unknown/.wt/x', 'x')];

    emitWorktreesChanged(['/unknown/project']);
    await flushAsync();

    expect(invalidateCalls).toEqual([]);
    expect(listCalls).toEqual([]);
    expect(sessionUiState.availableWorktreesByProject.size).toBe(0);
    release();
  });

  test('keeps last-known worktrees when the listing fails', async () => {
    const release = await acquire();
    const known = worktree('/repo/main/.wt/keep', 'keep');
    sessionUiState.availableWorktreesByProject = new Map([['/repo/main', [known]]]);
    sessionUiState.availableWorktrees = [known];
    listImpl = async () => {
      throw new Error('git unavailable');
    };

    emitWorktreesChanged(['/repo/main']);
    await flushAsync();

    expect(sessionUiState.availableWorktreesByProject.get('/repo/main')).toEqual([known]);
    release();
  });

  test('an empty successful listing removes the project entry', async () => {
    const release = await acquire();
    const known = worktree('/repo/other/.wt/gone', 'gone');
    sessionUiState.availableWorktreesByProject = new Map([['/repo/other', [known]]]);
    sessionUiState.availableWorktrees = [known];
    listImpl = async () => [];

    emitWorktreesChanged(['/repo/other']);
    await flushAsync();

    expect(sessionUiState.availableWorktreesByProject.size).toBe(0);
    expect(sessionUiState.availableWorktrees).toEqual([]);
    release();
  });

  test('stops handling events after the last release', async () => {
    const release = await acquire();
    release();

    expect(unsubscribeCalls).toBe(1);
    expect(capturedListener).toBeNull();

    listImpl = async () => [worktree('/repo/main/.wt/late', 'late')];
    // Direct dispatch is impossible after release; nothing may schedule.
    emitWorktreesChanged(['/repo/main']);
    await flushAsync();

    expect(listCalls).toEqual([]);
    expect(sessionUiState.availableWorktreesByProject.size).toBe(0);
  });

  test('drops in-flight refresh results across a runtime switch', async () => {
    const release = await acquire();
    const listing = Promise.withResolvers<WorktreeMetadata[]>();
    listImpl = () => listing.promise;

    emitWorktreesChanged(['/repo/main']);
    await flushAsync(2);
    expect(listCalls).toEqual(['/repo/main']);

    runtimeKey = 'runtime-2';
    listing.resolve([worktree('/repo/main/.wt/stale', 'stale')]);
    await flushAsync();

    expect(sessionUiState.availableWorktreesByProject.size).toBe(0);
    release();
  });
});
