import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { createWorktreeWatcher, resolveGitCommonDir } from './worktree-watcher.js';

// ---------------------------------------------------------------------------
// Shared test infrastructure
// ---------------------------------------------------------------------------

const tempDirs = [];
const watchers = [];

const createTempDir = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-worktree-watcher-'));
  tempDirs.push(dir);
  return dir;
};

afterEach(() => {
  for (const watcher of watchers.splice(0)) {
    try {
      watcher.stop();
    } catch {
      // best-effort teardown
    }
  }
  for (const dir of tempDirs.splice(0)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

const waitFor = async (predicate, timeoutMs = 5_000, intervalMs = 25) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  throw new Error('waitFor timed out');
};

/** Minimal main-repo git layout: <root>/.git directory (+ optional worktrees). */
const makeRepo = (root, { withWorktreesDir = false } = {}) => {
  fs.mkdirSync(path.join(root, '.git'), { recursive: true });
  if (withWorktreesDir) {
    fs.mkdirSync(path.join(root, '.git', 'worktrees'), { recursive: true });
  }
  return root;
};

/** Minimal linked-worktree layout: <root>/.git file pointing at the main repo. */
const makeLinkedWorktreeProject = (root, mainRepo, name) => {
  fs.mkdirSync(root, { recursive: true });
  fs.writeFileSync(path.join(root, '.git'), `gitdir: ${path.join(mainRepo, '.git', 'worktrees', name)}\n`, 'utf8');
  return root;
};

const startWatcher = async (options) => {
  const events = [];
  const onWorktreesChanged = (directories) => events.push(directories);
  const runtime = createWorktreeWatcher({
    onWorktreesChanged,
    logger: { warn: vi.fn() },
    debounceMs: 60,
    settingsRescanDelayMs: 60,
    rearmDelayMs: 60,
    ...options,
  });
  watchers.push(runtime);
  await runtime.start();
  return { runtime, events };
};

// ---------------------------------------------------------------------------
// resolveGitCommonDir
// ---------------------------------------------------------------------------

describe('resolveGitCommonDir', () => {
  it('returns the .git directory for a main repository', () => {
    const repo = makeRepo(createTempDir());
    expect(resolveGitCommonDir(repo)).toBe(path.join(repo, '.git'));
  });

  it('resolves a linked worktree to the main repository common dir', () => {
    const mainRepo = makeRepo(createTempDir());
    const linked = makeLinkedWorktreeProject(createTempDir(), mainRepo, 'feature');
    expect(resolveGitCommonDir(linked)).toBe(path.join(mainRepo, '.git'));
  });

  it('supports relative gitdir targets', () => {
    const mainRepo = makeRepo(createTempDir());
    const linked = createTempDir();
    fs.writeFileSync(
      path.join(linked, '.git'),
      `gitdir: ${path.relative(linked, path.join(mainRepo, '.git', 'worktrees', 'feature'))}\n`,
      'utf8',
    );
    expect(resolveGitCommonDir(linked)).toBe(path.join(mainRepo, '.git'));
  });

  it('returns null for a non-repository directory', () => {
    const dir = createTempDir();
    expect(resolveGitCommonDir(dir)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// watching
// ---------------------------------------------------------------------------

describe('worktree watcher', () => {
  it('reports a registered project when a worktree metadata entry appears and disappears', async () => {
    const repo = makeRepo(createTempDir(), { withWorktreesDir: true });
    const { runtime, events } = await startWatcher({ listProjects: async () => [{ id: 'p1', path: repo }] });

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-a'));
    await waitFor(() => events.some((directories) => directories.includes(path.resolve(repo))));

    fs.rmSync(path.join(repo, '.git', 'worktrees', 'wt-a'), { recursive: true, force: true });
    await waitFor(() => events.filter((directories) => directories.includes(path.resolve(repo))).length >= 2);

    runtime.stop();
  });

  it('coalesces a burst of worktree changes into one report', async () => {
    const repo = makeRepo(createTempDir(), { withWorktreesDir: true });
    const { runtime, events } = await startWatcher({ listProjects: async () => [{ id: 'p1', path: repo }] });

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-1'));
    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-2'));
    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-3'));
    await waitFor(() => events.length > 0);
    // All three creations happened inside one debounce window.
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(events).toHaveLength(1);

    runtime.stop();
  });

  it('detects the worktrees registry appearing on a repository that had none', async () => {
    const repo = makeRepo(createTempDir());
    const { runtime, events } = await startWatcher({ listProjects: async () => [{ id: 'p1', path: repo }] });

    // First linked worktree ever: `.git/worktrees` itself is created.
    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'first'), { recursive: true });
    await waitFor(() => events.some((directories) => directories.includes(path.resolve(repo))));

    runtime.stop();
  });

  it('reports every registered project sharing the repository, including linked-worktree projects', async () => {
    const mainRepo = makeRepo(createTempDir(), { withWorktreesDir: true });
    const linkedProject = makeLinkedWorktreeProject(createTempDir(), mainRepo, 'feature');

    const { runtime, events } = await startWatcher({
      listProjects: async () => [
        { id: 'p-main', path: mainRepo },
        { id: 'p-linked', path: linkedProject },
      ],
    });

    fs.mkdirSync(path.join(mainRepo, '.git', 'worktrees', 'wt-x'));
    await waitFor(() => events.length > 0);
    expect(events[0]).toEqual(expect.arrayContaining([path.resolve(mainRepo), path.resolve(linkedProject)]));

    runtime.stop();
  });

  it('ignores non-repository projects without watchers or events', async () => {
    const plain = createTempDir();
    const repo = makeRepo(createTempDir(), { withWorktreesDir: true });
    const { runtime, events } = await startWatcher({
      listProjects: async () => [
        { id: 'p-plain', path: plain },
        { id: 'p-repo', path: repo },
      ],
    });

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-a'));
    await waitFor(() => events.length > 0);
    expect(events[0]).toEqual([path.resolve(repo)]);

    runtime.stop();
  });

  it('stops reporting after stop()', async () => {
    const repo = makeRepo(createTempDir(), { withWorktreesDir: true });
    const { runtime, events } = await startWatcher({ listProjects: async () => [{ id: 'p1', path: repo }] });

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-a'));
    await waitFor(() => events.length > 0);
    runtime.stop();

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-b'));
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(events).toHaveLength(1);
  });

  it('re-arms watchers for a project added via a settings.json change', async () => {
    const settingsDir = createTempDir();
    const settingsFilePath = path.join(settingsDir, 'settings.json');
    const repoA = makeRepo(createTempDir(), { withWorktreesDir: true });
    const repoB = makeRepo(createTempDir(), { withWorktreesDir: true });
    let projects = [{ id: 'p-a', path: repoA }];

    const { runtime, events } = await startWatcher({
      listProjects: async () => projects,
      settingsFilePath,
    });

    // settings.json is written atomically (tmp file + rename), like the server does.
    const writeSettings = (value) => {
      const tmp = `${settingsFilePath}.tmp`;
      fs.writeFileSync(tmp, JSON.stringify(value), 'utf8');
      fs.renameSync(tmp, settingsFilePath);
    };
    writeSettings({ projects });

    projects = [{ id: 'p-a', path: repoA }, { id: 'p-b', path: repoB }];
    // settings-triggered rescan is debounced; let it arm repoB's watcher first
    await new Promise((resolve) => setTimeout(resolve, 250));
    fs.mkdirSync(path.join(repoB, '.git', 'worktrees', 'wt-b'));
    await waitFor(() => events.some((directories) => directories.includes(path.resolve(repoB))));

    runtime.stop();
  });

  it('keeps existing watchers when listProjects fails', async () => {
    const repo = makeRepo(createTempDir(), { withWorktreesDir: true });
    let fail = false;
    const { runtime, events } = await startWatcher({
      listProjects: async () => {
        if (fail) throw new Error('settings unreadable');
        return [{ id: 'p1', path: repo }];
      },
    });

    fail = true;
    await runtime.rescan();
    fail = false;

    fs.mkdirSync(path.join(repo, '.git', 'worktrees', 'wt-a'));
    await waitFor(() => events.some((directories) => directories.includes(path.resolve(repo))));

    runtime.stop();
  });
});
