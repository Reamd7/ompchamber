import { beforeEach, describe, expect, test } from 'bun:test';
import { sanitizeByRoot, useFilesViewTabsStore } from './useFilesViewTabsStore';

describe('useFilesViewTabsStore', () => {
  beforeEach(() => {
    useFilesViewTabsStore.setState({ byRoot: {}, activeRuntimeKey: 'runtime-a', runtimeSnapshots: {} });
  });

  test('ignores runtime paths outside the requested root', () => {
    const root = '/repo';
    const store = useFilesViewTabsStore.getState();

    store.addOpenPath(root, '/other/file.ts');
    store.setSelectedPath(root, '/other/file.ts');
    store.expandPath(root, '/other');
    store.toggleExpandedPath(root, '/other');

    expect(useFilesViewTabsStore.getState().byRoot).toEqual({});
  });

  test('filters expanded path batches to the requested root', () => {
    const root = '/repo';

    useFilesViewTabsStore.getState().expandPaths(root, [
      '/repo/src',
      '/other/src',
    ]);

    expect(useFilesViewTabsStore.getState().byRoot[root]?.expandedPaths).toEqual(['/repo/src']);
  });

  test('rejects realpath children of workspace symlinks (issue 2627)', () => {
    const root = '/workspace';
    const store = useFilesViewTabsStore.getState();

    store.toggleExpandedPath(root, '/workspace/pkg');
    store.toggleExpandedPath(root, '/real/pkg/src');
    store.toggleExpandedPath(root, '/workspace/pkg/src');

    expect(useFilesViewTabsStore.getState().byRoot[root]?.expandedPaths).toEqual([
      '/workspace/pkg',
      '/workspace/pkg/src',
    ]);
  });

  test('removes stale expanded paths by prefix without closing files', () => {
    const root = '/repo';
    const store = useFilesViewTabsStore.getState();

    store.addOpenPath(root, '/repo/src/index.ts');
    store.expandPaths(root, [
      '/repo/src',
      '/repo/bun test packages',
      '/repo/bun test packages/web',
      '/repo/other',
    ]);

    store.removeExpandedPathsByPrefix(root, '/repo/bun test packages');

    const state = useFilesViewTabsStore.getState().byRoot[root];
    expect(state?.openPaths).toEqual(['/repo/src/index.ts']);
    expect(state?.expandedPaths).toEqual(['/repo/src', '/repo/other']);
  });

  test('restores independent active projections across runtime switches', () => {
    useFilesViewTabsStore.getState().addOpenPath('/repo', '/repo/a.ts');
    useFilesViewTabsStore.getState().resetForRuntimeSwitch('runtime-b');
    expect(useFilesViewTabsStore.getState().byRoot).toEqual({});
    useFilesViewTabsStore.getState().addOpenPath('/repo', '/repo/b.ts');

    useFilesViewTabsStore.getState().resetForRuntimeSwitch('runtime-a');
    expect(useFilesViewTabsStore.getState().byRoot['/repo']?.openPaths).toEqual(['/repo/a.ts']);
    useFilesViewTabsStore.getState().resetForRuntimeSwitch('runtime-b');
    expect(useFilesViewTabsStore.getState().byRoot['/repo']?.openPaths).toEqual(['/repo/b.ts']);
  });

  test('rejects URL-scheme paths at the write boundary', () => {
    const root = '/repo';
    const store = useFilesViewTabsStore.getState();

    // A URL resolved as a relative path survives as `https:` segment; writing
    // it would make the tree probe nonexistent paths on every session open.
    store.addOpenPath(root, '/repo/https:/api.github.com/repos');
    store.expandPaths(root, ['/repo/src', '/repo/https:/api.github.com/repos']);
    store.setSelectedPath(root, '/repo/https:/api.github.com/repos');
    store.toggleExpandedPath(root, '/repo/https:/api.github.com');

    const state = useFilesViewTabsStore.getState().byRoot[root];
    expect(state?.openPaths).toEqual([]);
    expect(state?.expandedPaths).toEqual(['/repo/src']);
    expect(state?.selectedPath).toBeNull();
  });

  test('sanitizeByRoot drops persisted URL-scheme paths and keeps drive roots', () => {
    const sanitized = sanitizeByRoot({
      '/repo': {
        openPaths: ['/repo/a.ts', '/repo/https:/api.github.com/x'],
        selectedPath: '/repo/https:/api.github.com/x',
        expandedPaths: ['/repo/src', '/repo/https:'],
        touchedAt: Date.now(),
      },
      'C:/Repo': {
        openPaths: ['C:/Repo/src/a.ts'],
        selectedPath: 'C:/Repo/src/a.ts',
        expandedPaths: ['C:/Repo/src'],
        touchedAt: Date.now(),
      },
    });

    expect(sanitized['/repo']?.openPaths).toEqual(['/repo/a.ts']);
    expect(sanitized['/repo']?.selectedPath).toBe('/repo/a.ts');
    expect(sanitized['/repo']?.expandedPaths).toEqual(['/repo/src']);
    expect(sanitized['C:/Repo']?.openPaths).toEqual(['C:/Repo/src/a.ts']);
    expect(sanitized['C:/Repo']?.selectedPath).toBe('C:/Repo/src/a.ts');
    expect(sanitized['C:/Repo']?.expandedPaths).toEqual(['C:/Repo/src']);
  });
});
