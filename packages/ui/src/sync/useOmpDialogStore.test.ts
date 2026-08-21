import { beforeEach, describe, expect, test } from 'bun:test';
import type { OmpPendingDialog } from '@/lib/api/omp';
import {
  compareOmpDialogs,
  useOmpDialogStore,
} from './useOmpDialogStore';

const approvalDialog = (overrides: Partial<OmpPendingDialog> & { id: string }): OmpPendingDialog => ({
  sessionId: 'ses_1',
  createdAt: 1000,
  kind: 'approval',
  approval: { prompt: 'Allow bash rm -rf?' },
  ...overrides,
} as OmpPendingDialog);

const reset = (): void => {
  useOmpDialogStore.setState({ directories: {}, runtimeKey: 'rt_1' });
};

beforeEach(reset);

describe('useOmpDialogStore — ingestion lifecycle', () => {
  test('requested upserts; duplicate frame is idempotent', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    expect(Object.keys(useOmpDialogStore.getState().directories['/repo']?.dialogs ?? {})).toEqual(['dlg_a']);
  });

  test('frames from a non-adopted runtime are ignored', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_OTHER', '/repo', approvalDialog({ id: 'dlg_a' }));
    expect(useOmpDialogStore.getState().directories['/repo']).toBe(undefined);
  });

  test('adoptRuntime drops stale slices and switches identity', () => {
    useOmpDialogStore.getState().ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    useOmpDialogStore.getState().adoptRuntime('rt_2');
    expect(useOmpDialogStore.getState().directories).toEqual({});
    expect(useOmpDialogStore.getState().runtimeKey).toBe('rt_2');
  });

  test('scope: the same dialog id never merges across directories or sessions', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a', sessionId: 'ses_1' }));
    store.ingestRequested('rt_1', '/other', approvalDialog({ id: 'dlg_a', sessionId: 'ses_2' }));
    const state = useOmpDialogStore.getState();
    expect(state.directories['/repo']?.dialogs['dlg_a']?.sessionId).toBe('ses_1');
    expect(state.directories['/other']?.dialogs['dlg_a']?.sessionId).toBe('ses_2');
  });

  test('settled removes and tombstones; a stale requested replay never resurrects', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    store.ingestSettled('rt_1', '/repo', 'dlg_a');
    expect(useOmpDialogStore.getState().directories['/repo']?.dialogs['dlg_a']).toBe(undefined);
    // SSE replay of the original requested after the settle:
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    expect(useOmpDialogStore.getState().directories['/repo']?.dialogs['dlg_a']).toBe(undefined);
  });

  test('createdAt ties break deterministically on id', () => {
    const a = approvalDialog({ id: 'dlg_a', createdAt: 5 });
    const b = approvalDialog({ id: 'dlg_b', createdAt: 5 });
    const c = approvalDialog({ id: 'dlg_c', createdAt: 4 });
    expect([c, a, b].sort(compareOmpDialogs).map((dialog) => dialog.id)).toEqual(['dlg_c', 'dlg_a', 'dlg_b']);
    expect([b, a].sort(compareOmpDialogs).map((dialog) => dialog.id)).toEqual(['dlg_a', 'dlg_b']);
    expect([a, b].sort(compareOmpDialogs).map((dialog) => dialog.id)).toEqual(['dlg_a', 'dlg_b']);
  });
});

describe('useOmpDialogStore — authoritative reconcile (GET/SSE races)', () => {
  test('snapshot replaces state: unknown added, missing removed + tombstoned', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_old' }));
    store.reconcileSnapshot('rt_1', '/repo', [
      approvalDialog({ id: 'dlg_new', createdAt: 2000 }),
    ]);
    const slice = useOmpDialogStore.getState().directories['/repo'];
    expect(Object.keys(slice?.dialogs ?? {})).toEqual(['dlg_new']);
    expect(slice?.tombstones['dlg_old']).toBeDefined();
  });

  test('event before GET, stale GET during: settle wins over snapshot content', () => {
    // GET starts while dlg_a pending; settle event arrives; GET returns dlg_a.
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    store.ingestSettled('rt_1', '/repo', 'dlg_a');
    store.reconcileSnapshot('rt_1', '/repo', [approvalDialog({ id: 'dlg_a' })]);
    expect(useOmpDialogStore.getState().directories['/repo']?.dialogs['dlg_a']).toBe(undefined);
  });

  test('ui flags (inflight/error) survive a snapshot that still carries the dialog', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a' }));
    store.markRespondInflight('rt_1', '/repo', 'dlg_a', true);
    store.reconcileSnapshot('rt_1', '/repo', [approvalDialog({ id: 'dlg_a' })]);
    expect(useOmpDialogStore.getState().directories['/repo']?.ui['dlg_a']?.respondInflight).toBe(true);
  });

  test('clearSession drops only that session\'s dialogs; clearDirectory drops the slice', () => {
    const store = useOmpDialogStore.getState();
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_a', sessionId: 'ses_1' }));
    store.ingestRequested('rt_1', '/repo', approvalDialog({ id: 'dlg_b', sessionId: 'ses_2' }));
    store.clearSession('rt_1', '/repo', 'ses_1');
    expect(Object.keys(useOmpDialogStore.getState().directories['/repo']?.dialogs ?? {})).toEqual(['dlg_b']);
    store.clearDirectory('rt_1', '/repo');
    expect(useOmpDialogStore.getState().directories['/repo']).toBe(undefined);
  });
});
