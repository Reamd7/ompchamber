/**
 * seedSessionModel slice integrity — regression test for the transcript
 * crash `Cannot read properties of undefined (reading 'msg_…')`.
 *
 * seedSessionModel used to spread a missing slice (`{...undefined}`), writing
 * a partial slice with only `sessionModel` into an empty directory. The
 * retry leaf selectors iterate EVERY slice and index `superseded`/`notes` by
 * wire message id, so any partial slice crashed ChatMessage on mount (browser
 * load before omp events arrive, desktop 401 bootstrap, etc.).
 *
 * Contract under test:
 * 1. seedSessionModel on a directory with no slice creates the FULL empty
 *    shape (every map selectable), not a partial slice.
 * 2. The retry selectors degrade to "no data" instead of throwing even when
 *    a legacy partial slice is already present (defense-in-depth).
 */
import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { createEmptyOmpDirectoryState } from '@/sync/omp-event-reducer';
import { useOmpRetryNote, useOmpRetrySupersession, useOmpSessionStore } from '@/sync/useOmpSessionStore';

const DIRECTORY = '/repo';
const DIRECTORY_B = '/repo-b';
const WIRE_ID = 'msg_0278c31fb0013yd2UhF1yAcofw';

const resetStore = (directories: Record<string, unknown>) => {
  useOmpSessionStore.setState({ runtimeKey: 'rt', directories: directories as never });
};

// Probe components: mounting these must not throw, whatever the store holds.
const SupersessionProbe = () => <span data-ok={useOmpRetrySupersession(WIRE_ID) ? 'yes' : 'no'} />;
const NoteProbe = () => {
  const note = useOmpRetryNote(WIRE_ID);
  return <span data-note={note === undefined ? 'none' : note} />;
};

describe('seedSessionModel slice integrity', () => {
  test('seeding an empty directory creates the full slice shape', () => {
    resetStore({});
    useOmpSessionStore.getState().seedSessionModel('rt', DIRECTORY, 'ses_1', { provider: 'p', id: 'm' });

    const slice = useOmpSessionStore.getState().directories[DIRECTORY];
    expect(slice).toBeDefined();
    // The maps the leaf selectors index by message id must exist.
    expect(slice?.superseded).toEqual({});
    expect(slice?.notes).toEqual({});
    expect(slice?.loaders).toEqual({});
    expect(slice?.sessionModel.ses_1?.provider).toBe('p');
    expect(slice?.sessionModel.ses_1?.id).toBe('m');
  });

  test('retry selectors do not throw against a legacy partial slice', () => {
    // Simulate a store written by the pre-fix build: a slice that carries
    // only sessionModel. Rendering must return "no data", never throw.
    resetStore({
      [DIRECTORY_B]: { sessionModel: { ses_1: { provider: 'p', id: 'm' } } },
    });

    expect(renderToStaticMarkup(<SupersessionProbe />)).toBe('<span data-ok="no"></span>');
    expect(renderToStaticMarkup(<NoteProbe />)).toBe('<span data-note="none"></span>');

    // Seeding over the partial slice restores the full shape.
    useOmpSessionStore.getState().seedSessionModel('rt', DIRECTORY_B, 'ses_1', { provider: 'p2', id: 'm2' });
    const slice = useOmpSessionStore.getState().directories[DIRECTORY_B];
    expect(slice?.superseded).toEqual({});
    expect(slice?.notes).toEqual({});
    expect(slice?.loaders).toEqual({});
    expect(slice?.sessionModel.ses_1?.provider).toBe('p2');
  });

  test('empty-state factory shape matches what selectors index', () => {
    const empty = createEmptyOmpDirectoryState();
    expect(empty.superseded).toEqual({});
    expect(empty.notes).toEqual({});
    expect(empty.loaders).toEqual({});
    expect(empty.chrome).toEqual({ widgets: {}, status: {} });
  });
});
