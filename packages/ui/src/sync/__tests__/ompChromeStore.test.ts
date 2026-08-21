/**
 * useOmpSessionStore chrome reconcile — store wiring (spec 09 §5.0).
 *
 * Covers the authoritative snapshot path: complete parsed snapshots replace
 * the slice, wrong runtime keys and missing slices are ignored (no phantom
 * directories), and identical snapshots are reference-stable no-ops.
 */
import { describe, expect, test } from 'bun:test';

import { createEmptyOmpDirectoryState } from '@/sync/omp-event-reducer';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';

const DIRECTORY = '/repo';

describe('reconcileChromeSnapshot', () => {
  test('replaces the chrome slice from a complete snapshot', () => {
    useOmpSessionStore.setState({ runtimeKey: 'rt', directories: { [DIRECTORY]: createEmptyOmpDirectoryState() } });
    useOmpSessionStore.getState().reconcileChromeSnapshot('rt', DIRECTORY, {
      widgets: [{ key: 'zhipu', lines: ['a'], placement: 'aboveEditor', sessionId: 's', updatedAt: 1 }],
      status: [{ key: 'tps', text: 'x', sessionId: 's', updatedAt: 1 }],
    });
    const chrome = useOmpSessionStore.getState().directories[DIRECTORY]?.chrome;
    expect(chrome?.widgets.zhipu?.lines).toEqual(['a']);
    expect(chrome?.status.tps?.text).toBe('x');
  });

  test('identical snapshot keeps the previous slice reference (no-op commit)', () => {
    const before = useOmpSessionStore.getState().directories[DIRECTORY]?.chrome;
    useOmpSessionStore.getState().reconcileChromeSnapshot('rt', DIRECTORY, {
      widgets: [{ key: 'zhipu', lines: ['a'], placement: 'aboveEditor', sessionId: 's', updatedAt: 1 }],
      status: [{ key: 'tps', text: 'x', sessionId: 's', updatedAt: 1 }],
    });
    expect(useOmpSessionStore.getState().directories[DIRECTORY]?.chrome).toBe(before);
  });

  test('wrong runtime key and missing directory slice are ignored', () => {
    useOmpSessionStore.getState().reconcileChromeSnapshot('other', DIRECTORY, { widgets: [], status: [] });
    expect(useOmpSessionStore.getState().directories[DIRECTORY]?.chrome.widgets.zhipu).toBeDefined();
    useOmpSessionStore.getState().reconcileChromeSnapshot('rt', '/nowhere', { widgets: [], status: [] });
    expect(useOmpSessionStore.getState().directories['/nowhere']).toBe(undefined);
  });
});
