import React from 'react';
import { subscribeOmpchamberEvents } from '@/lib/openchamberEvents';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { normalizePath } from '@/lib/pathNormalization';
import { runBackgroundNetworkTask } from '@/lib/background-network';
import { useProjectsStore } from '@/stores/useProjectsStore';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { recordWorktreesSeen } from '@/components/session/sidebar/worktreeFirstSeen';
import {
  invalidateWorktreeList,
  listProjectWorktrees,
  partitionWorktreesByRegisteredProject,
  worktreeMapsEqual,
} from './worktreeManager';

/**
 * Server-pushed worktree topology updates.
 *
 * The web server fs-watches every registered project's `.git/worktrees`
 * registry and emits `worktrees-changed` with the affected project paths
 * (see packages/web/server/lib/git/worktree-watcher.js). This module turns
 * those events into an incremental refresh of the shared
 * `availableWorktreesByProject` map in `useSessionUIStore`, so every surface
 * (sidebar, mobile sheet, mini chat, draft branch selector) sees worktrees
 * created or removed by ANY source — terminal git, another client, scheduled
 * runs — without waiting for the pull-based discovery passes.
 *
 * The event is only an invalidation signal: the worktree list itself always
 * comes from `git worktree list` via `listProjectWorktrees`, never from the
 * event payload. A failed listing keeps that project's last-known worktrees
 * (failure is not empty success); an empty successful listing removes them.
 */

const REFRESH_COALESCE_MS = 300;

/** Named timer handle: the module owns the debounce timer's identity. */
type RefreshTimerHandle = ReturnType<typeof setTimeout>;

let activeCount = 0;
let unsubscribe: (() => void) | null = null;
let refreshTimer: RefreshTimerHandle | null = null;
let refreshCoalesceMs = REFRESH_COALESCE_MS;
let pendingProjectPaths = new Set<string>();

const refreshPendingProjects = async () => {
  const runtimeKey = getRuntimeKey();
  const targets = pendingProjectPaths;
  pendingProjectPaths = new Set();

  for (const projectPath of targets) {
    if (getRuntimeKey() !== runtimeKey) return;
    const project = useProjectsStore
      .getState()
      .projects.find((entry) => normalizePath(entry.path) === projectPath);
    if (!project) continue;

    // The event means the topology changed, so the 30s listing cache must not
    // serve the pre-change snapshot.
    invalidateWorktreeList(projectPath);
    let worktrees;
    try {
      worktrees = await runBackgroundNetworkTask(() =>
        listProjectWorktrees({ id: project.id, path: projectPath }),
      );
    } catch {
      // Listing failed — keep this project's last-known worktrees.
      continue;
    }
    if (getRuntimeKey() !== runtimeKey) return;

    const sessionUi = useSessionUIStore.getState();
    const worktreesByProject = new Map(sessionUi.availableWorktreesByProject);
    if (worktrees.length === 0) {
      worktreesByProject.delete(projectPath);
    } else {
      worktreesByProject.set(projectPath, worktrees);
    }
    const partitioned = partitionWorktreesByRegisteredProject(
      useProjectsStore.getState().projects,
      worktreesByProject,
    );
    const allWorktrees = [...partitioned.values()].flat();
    // Newly appearing worktrees sort to the top of their project's worktree
    // list (see worktreeFirstSeen.ts), same as the pull-based discovery.
    recordWorktreesSeen(allWorktrees.map((worktree) => worktree.path), Date.now());
    if (!worktreeMapsEqual(partitioned, sessionUi.availableWorktreesByProject)) {
      useSessionUIStore.setState({
        availableWorktrees: allWorktrees,
        availableWorktreesByProject: partitioned,
      });
    }
  }
};

const scheduleRefresh = (directories: readonly string[]) => {
  for (const directory of directories) {
    const normalized = normalizePath(directory);
    if (normalized) pendingProjectPaths.add(normalized);
  }
  if (pendingProjectPaths.size === 0) return;
  if (refreshTimer !== null) return;
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    void refreshPendingProjects();
  }, refreshCoalesceMs);
};

/**
 * Start (or join) the shared event subscription. Refcounted so multiple app
 * roots can acquire it and the subscription dies with the last one.
 * `coalesceMs` is a test seam for deterministic timing.
 */
export const acquireWorktreeEventSync = (options?: { coalesceMs?: number }): (() => void) => {
  if (typeof options?.coalesceMs === 'number') {
    refreshCoalesceMs = options.coalesceMs;
  }
  activeCount += 1;
  if (!unsubscribe) {
    unsubscribe = subscribeOmpchamberEvents((event) => {
      if (event.type === 'worktrees-changed') {
        scheduleRefresh(event.directories);
      }
    });
  }
  return () => {
    activeCount = Math.max(0, activeCount - 1);
    if (activeCount === 0 && unsubscribe) {
      unsubscribe();
      unsubscribe = null;
      if (refreshTimer !== null) {
        clearTimeout(refreshTimer);
        refreshTimer = null;
      }
      pendingProjectPaths.clear();
    }
  };
};

/** Mount once per app root; keeps the shared worktree map event-fed. */
export const useWorktreeEventSync = (): void => {
  React.useEffect(() => acquireWorktreeEventSync(), []);
};
