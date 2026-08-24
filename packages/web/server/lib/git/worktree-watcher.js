import fs from 'node:fs';
import path from 'node:path';

/**
 * Worktree topology watcher.
 *
 * Watches the git metadata of every registered project for linked-worktree
 * creation and removal, regardless of who performed it (OpenChamber's own
 * worktree routes, the CLI, a terminal `git worktree add`, or another client).
 * Each linked worktree is registered under the repository's common git dir as
 * `.git/worktrees/<name>`, so watching that one directory per repository
 * observes every worktree topology change for the repo. The watcher never
 * interprets the topology: it reports the affected registered project paths
 * and the client re-lists worktrees authoritatively via `git worktree list`.
 *
 * Deliberate boundaries:
 * - A worktree checkout directory deleted without `git worktree remove`/
 *   `prune` leaves the metadata entry in place, so it fires no event here.
 *   The existing per-worktree "missing folder" status covers that case.
 * - Watch failures (repo moved/deleted, FS watch limits) close that repo's
 *   watchers and retry a bounded number of times; discovery then falls back to
 *   the client's existing pull-based worktree listing.
 * - The waiting-mode registry watcher relies on fs.watch reporting event
 *   filenames; platforms that omit them degrade to pull-based discovery until
 *   the registry already exists (worktrees mode filters nothing).
 */

const PARSE_GITDIR_PATTERN = /^gitdir:\s*(.+?)\s*$/m;
const MAX_REARM_ATTEMPTS = 5;

/** Extract the gitdir target from a linked-worktree `.git` file. */
const parseGitdirTarget = (content) => {
  const match = PARSE_GITDIR_PATTERN.exec(content);
  return match ? match[1] : null;
};

/**
 * Resolve the common git dir that owns a repository's worktree registry.
 * - main repository: `<project>/.git` (a directory)
 * - linked worktree: `<project>/.git` is a file pointing at
 *   `<main>/.git/worktrees/<name>`; the common dir is that path's grandparent
 * Returns null for non-repositories and unreadable layouts.
 */
export const resolveGitCommonDir = (projectPath) => {
  const dotGitPath = path.join(projectPath, '.git');
  let stat;
  try {
    stat = fs.statSync(dotGitPath);
  } catch {
    return null;
  }
  if (stat.isDirectory()) {
    return dotGitPath;
  }
  if (!stat.isFile()) {
    return null;
  }
  let content;
  try {
    content = fs.readFileSync(dotGitPath, 'utf8');
  } catch {
    return null;
  }
  const gitdir = parseGitdirTarget(content);
  if (!gitdir) {
    return null;
  }
  const gitdirPath = path.resolve(projectPath, gitdir);
  if (path.basename(path.dirname(gitdirPath)) === 'worktrees') {
    return path.dirname(path.dirname(gitdirPath));
  }
  return gitdirPath;
};

export const createWorktreeWatcher = ({
  listProjects,
  settingsFilePath,
  onWorktreesChanged,
  logger = console,
  debounceMs = 500,
  settingsRescanDelayMs = 400,
  rearmDelayMs = 5_000,
}) => {
  // common git dir -> watch state for every registered project of that repo.
  const repoEntries = new Map();
  let settingsWatcher = null;
  let settingsRescanTimer = null;
  let settingsRearmTimer = null;
  let rescanChain = Promise.resolve();
  let disposed = false;

  const logWarn = (message, detail) => {
    try {
      logger.warn(`[WorktreeWatcher] ${message}`, detail ?? '');
    } catch {
      // logging must never break watching
    }
  };

  const disposeRepoEntry = (entry) => {
    entry.stopped = true;
    if (entry.emitTimer) {
      clearTimeout(entry.emitTimer);
      entry.emitTimer = null;
    }
    if (entry.rearmTimer) {
      clearTimeout(entry.rearmTimer);
      entry.rearmTimer = null;
    }
    for (const watcher of entry.watchers) {
      try {
        watcher.close();
      } catch {
        // already closed
      }
    }
    entry.watchers.clear();
  };

  const emitFor = (entry) => {
    if (disposed || entry.stopped || entry.projectPaths.size === 0) return;
    if (entry.emitTimer) return; // burst already coalescing
    entry.emitTimer = setTimeout(() => {
      entry.emitTimer = null;
      if (disposed || entry.stopped) return;
      try {
        onWorktreesChanged([...entry.projectPaths]);
      } catch (error) {
        logWarn('change listener failed:', error?.message || error);
      }
    }, debounceMs);
    entry.emitTimer.unref?.();
  };

  const watchDirectory = (entry, dirPath, filenameFilter) => {
    try {
      const watcher = fs.watch(dirPath, (event, filename) => {
        if (disposed || entry.stopped) return;
        if (filenameFilter && (!filename || path.basename(filename) !== filenameFilter)) return;
        if (filenameFilter) {
          // The worktrees registry itself appeared or disappeared: re-arm so
          // the direct registry watcher tracks the new state, then report it.
          armRepo(entry);
        }
        emitFor(entry);
      });
      watcher.on('error', () => {
        if (disposed || entry.stopped) return;
        scheduleRearm(entry);
      });
      watcher.on('close', () => {
        entry.watchers.delete(watcher);
      });
      entry.watchers.add(watcher);
      return true;
    } catch {
      return false;
    }
  };

  /**
   * Arm one repository's watchers.
   * - `worktrees` metadata dir present: watch it directly for entry
   *   add/remove (the actual topology events).
   * - always watch the common git dir filtered to the `worktrees` name so the
   *   registry's appearance (first linked worktree) and disappearance (last
   *   one pruned) are observed too.
   * Returns false when any desired watcher could not be armed.
   */
  const armRepo = (entry) => {
    if (disposed || entry.stopped) return true;
    for (const watcher of entry.watchers) {
      try {
        watcher.close();
      } catch {
        // already closed
      }
    }
    entry.watchers.clear();
    const worktreesDir = path.join(entry.commonGitDir, 'worktrees');
    let failed = false;

    if (fs.existsSync(worktreesDir)) {
      entry.mode = 'worktrees';
      if (!watchDirectory(entry, worktreesDir, null)) failed = true;
    } else {
      entry.mode = 'waiting';
    }
    // The common-dir watcher is kept in both modes: deleting the whole
    // worktrees registry only surfaces as an event on its parent, and the
    // filter keeps the busy `.git` churn to one basename compare per event.
    if (!watchDirectory(entry, entry.commonGitDir, 'worktrees')) failed = true;
    return !failed;
  };

  const scheduleRearm = (entry) => {
    if (disposed || entry.stopped || entry.rearmTimer) return;
    entry.failureCount = (entry.failureCount ?? 0) + 1;
    if (entry.failureCount > MAX_REARM_ATTEMPTS) {
      if (!entry.gaveUp) {
        entry.gaveUp = true;
        logWarn(`giving up watching ${entry.commonGitDir} until the project list changes`);
      }
      return;
    }
    entry.rearmTimer = setTimeout(() => {
      entry.rearmTimer = null;
      if (disposed || entry.stopped) return;
      if (armRepo(entry)) {
        entry.failureCount = 0;
        entry.gaveUp = false;
      } else {
        scheduleRearm(entry);
      }
    }, rearmDelayMs);
    entry.rearmTimer.unref?.();
  };

  const closeSettingsWatcher = () => {
    if (!settingsWatcher) return;
    try {
      settingsWatcher.close();
    } catch {
      // already closed
    }
    settingsWatcher = null;
  };

  const scheduleSettingsRearm = () => {
    if (disposed || settingsRearmTimer) return;
    settingsRearmTimer = setTimeout(() => {
      settingsRearmTimer = null;
      armSettingsWatcher();
    }, rearmDelayMs);
    settingsRearmTimer.unref?.();
  };

  const scheduleSettingsRescan = () => {
    if (disposed || settingsRescanTimer) return;
    settingsRescanTimer = setTimeout(() => {
      settingsRescanTimer = null;
      rescan();
    }, settingsRescanDelayMs);
    settingsRescanTimer.unref?.();
  };

  const armSettingsWatcher = () => {
    if (disposed || !settingsFilePath || settingsWatcher) return;
    const settingsDir = path.dirname(settingsFilePath);
    const settingsFileName = path.basename(settingsFilePath);
    try {
      const watcher = fs.watch(settingsDir, (_event, filename) => {
        if (!filename || path.basename(filename) !== settingsFileName) return;
        scheduleSettingsRescan();
      });
      const onInactive = () => {
        if (settingsWatcher !== watcher) return;
        settingsWatcher = null;
        scheduleSettingsRearm();
      };
      watcher.on('error', onInactive);
      watcher.on('close', onInactive);
      settingsWatcher = watcher;
    } catch {
      scheduleSettingsRearm();
    }
  };

  /** Reconcile watchers with the current registered project list. */
  const rescan = () => {
    rescanChain = rescanChain
      .then(async () => {
        if (disposed) return;
        let projects = [];
        try {
          projects = await listProjects();
        } catch (error) {
          logWarn('listProjects failed; keeping current watchers:', error?.message || error);
          return;
        }

        // listProjects awaited; stop() may have run meanwhile.
        if (disposed) return;
        const projectPathsByCommonDir = new Map();
        for (const project of Array.isArray(projects) ? projects : []) {
          const rawPath = typeof project?.path === 'string' ? project.path.trim() : '';
          if (!rawPath) continue;
          const projectPath = path.resolve(rawPath);
          const commonGitDir = resolveGitCommonDir(projectPath);
          if (!commonGitDir) continue;
          if (!projectPathsByCommonDir.has(commonGitDir)) {
            projectPathsByCommonDir.set(commonGitDir, new Set());
          }
          projectPathsByCommonDir.get(commonGitDir).add(projectPath);
        }

        for (const [commonGitDir, entry] of repoEntries) {
          if (!projectPathsByCommonDir.has(commonGitDir)) {
            disposeRepoEntry(entry);
            repoEntries.delete(commonGitDir);
          }
        }
        for (const [commonGitDir, projectPaths] of projectPathsByCommonDir) {
          const entry = repoEntries.get(commonGitDir);
          if (entry) {
            const samePaths =
              entry.projectPaths.size === projectPaths.size &&
              [...projectPaths].every((projectPath) => entry.projectPaths.has(projectPath));
            if (samePaths) continue;
            entry.projectPaths = projectPaths;
            // Attribution changed but the watched directory did not; the
            // existing watchers stay armed and pick up the new paths.
            entry.gaveUp = false;
            entry.failureCount = 0;
            continue;
          }
          const nextEntry = {
            commonGitDir,
            projectPaths,
            watchers: new Set(),
            emitTimer: null,
            rearmTimer: null,
            mode: 'waiting',
            failureCount: 0,
            gaveUp: false,
            stopped: false,
          };
          repoEntries.set(commonGitDir, nextEntry);
          if (!armRepo(nextEntry)) {
            scheduleRearm(nextEntry);
          }
        }
      })
      .catch((error) => {
        logWarn('rescan failed:', error?.message || error);
      });
    return rescanChain;
  };

  return {
    start: () => {
      if (disposed) return Promise.resolve();
      armSettingsWatcher();
      return rescan();
    },
    stop: () => {
      disposed = true;
      for (const entry of repoEntries.values()) {
        disposeRepoEntry(entry);
      }
      repoEntries.clear();
      closeSettingsWatcher();
      if (settingsRescanTimer) {
        clearTimeout(settingsRescanTimer);
        settingsRescanTimer = null;
      }
      if (settingsRearmTimer) {
        clearTimeout(settingsRearmTimer);
        settingsRearmTimer = null;
      }
    },
    rescan,
  };
};
