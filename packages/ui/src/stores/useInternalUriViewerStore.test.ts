/**
 * useInternalUriViewerStore target contract (spec 04 artifacts browse):
 * `open(url, target)` pins BOTH resolve ids to the owning session; markdown
 * clicks (no target) keep the active-session behavior; close clears both.
 */
import { beforeEach, describe, expect, test } from 'bun:test';
import { useInternalUriViewerStore } from './useInternalUriViewerStore';

describe('useInternalUriViewerStore target pinning', () => {
  beforeEach(() => {
    useInternalUriViewerStore.setState({ url: null, target: null });
  });

  test('open without target keeps active-session behavior (target null)', () => {
    useInternalUriViewerStore.getState().open('local://PLAN.md');
    expect(useInternalUriViewerStore.getState().url).toBe('local://PLAN.md');
    expect(useInternalUriViewerStore.getState().target).toBeNull();
  });

  test('open with target pins sessionID + directory', () => {
    useInternalUriViewerStore.getState().open('local://notes.md', {
      sessionID: 'ses_B',
      directory: '/repo',
    });
    expect(useInternalUriViewerStore.getState().target).toEqual({ sessionID: 'ses_B', directory: '/repo' });
  });

  test('malformed targets are dropped, not half-pinned', () => {
    useInternalUriViewerStore.getState().open('local://a.md', { sessionID: '', directory: '/repo' });
    expect(useInternalUriViewerStore.getState().target).toBeNull();
    useInternalUriViewerStore.getState().open('local://a.md', { sessionID: 's' } as { sessionID: string; directory: string });
    expect(useInternalUriViewerStore.getState().target).toBeNull();
  });

  test('close clears url and target together', () => {
    useInternalUriViewerStore.getState().open('local://a.md', { sessionID: 's', directory: '/d' });
    useInternalUriViewerStore.getState().close();
    expect(useInternalUriViewerStore.getState().url).toBeNull();
    expect(useInternalUriViewerStore.getState().target).toBeNull();
  });
});
