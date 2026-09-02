import { describe, expect, test } from 'bun:test';
import type { OmpEventEnvelope } from '@/lib/api/omp';
import { applyOmpEvent, createEmptyOmpDirectoryState, type OmpDirectoryState } from './omp-event-reducer';

const envelope = (overrides: Partial<OmpEventEnvelope> & { type: string }): OmpEventEnvelope => ({
  id: 1,
  directory: '/repo',
  schemaVersion: '1.0',
  createdAt: 1000,
  payload: {},
  ...overrides,
});

const state = (): OmpDirectoryState => createEmptyOmpDirectoryState();

describe('applyOmpEvent — id gate', () => {
  test('replayed/duplicated ids below the high-water mark are skipped wholesale', () => {
    const draft = state();
    expect(applyOmpEvent(draft, envelope({ id: 10, type: 'omp.usage.turn', sessionID: 'ses_1', payload: { messageID: 'm1' } })).changed).toBe(true);
    expect(draft.lastAppliedEventId).toBe(10);
    // Duplicate with different payload must not mutate.
    expect(applyOmpEvent(draft, envelope({ id: 10, type: 'omp.usage.turn', sessionID: 'ses_1', payload: { messageID: 'm2' } })).changed).toBe(false);
    expect(draft.telemetry.ses_1).toHaveLength(1);
  });

  test('no-op and unknown frames still advance the high-water mark', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 5, type: ['omp','unknown','future'].join('.') }));
    expect(draft.lastAppliedEventId).toBe(5);
    expect(applyOmpEvent(draft, envelope({ id: 5, type: 'omp.unknown.futureevent' })).changed).toBe(false);
  });
});

describe('applyOmpEvent — retry lifecycle (spec 05 §5.3.2 P1)', () => {
  test('retry.started sets the loader and the superseded overlay; ended clears the loader and merges notes', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({
      id: 1, type: 'omp.retry.started', sessionID: 'ses_1', createdAt: 100,
      payload: { attempt: 1, maxAttempts: 3, delayMs: 5000, errorMessage: 'boom', supersededMessageID: 'msg_A' },
    }));
    expect(draft.loaders.ses_1?.retry?.attempt).toBe(1);
    expect(draft.superseded.msg_A).toEqual({ since: 100 });

    applyOmpEvent(draft, envelope({
      id: 2, type: 'omp.retry.ended', sessionID: 'ses_1', createdAt: 200,
      payload: { success: false, attempt: 1, finalError: 'still failing', retryErrors: [{ messageID: 'msg_A', note: 'credential' }] },
    }));
    expect(draft.loaders.ses_1?.retry).toBe(undefined);
    expect(draft.notes.msg_A).toEqual({ note: 'credential', appliedAt: 200 });
    expect(draft.retryTerminal.ses_1?.finalError).toBe('still failing');
    // The superseded overlay persists — P1 is pure presentation, no removal.
    expect(draft.superseded.msg_A).toEqual({ since: 100 });
  });

  test('a later successful retry supersedes the terminal-failure marker', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.retry.ended', sessionID: 'ses_1', createdAt: 100, payload: { success: false, attempt: 1 } }));
    expect(draft.retryTerminal.ses_1).toBeDefined();
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.retry.ended', sessionID: 'ses_1', createdAt: 200, payload: { success: true, attempt: 2 } }));
    expect(draft.retryTerminal.ses_1).toBe(undefined);
  });

  test('a replayed retry.ended alone restores the superseded overlay (reload path)', () => {
    const draft = state();
    // Fresh page load: the volatile omp.retry.started never re-arrives; the
    // durable ended event is the only carrier of what was superseded.
    applyOmpEvent(draft, envelope({
      id: 7, type: 'omp.retry.ended', sessionID: 'ses_1', createdAt: 200,
      payload: { success: true, attempt: 2, retryErrors: [{ messageID: 'msg_A', note: 'flaky' }] },
    }));
    expect(draft.notes.msg_A).toEqual({ note: 'flaky', appliedAt: 200 });
    expect(draft.superseded.msg_A).toEqual({ since: 200 });
  });

  test('stale volatile retry frames are rejected', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.retry.started', sessionID: 'ses_1', createdAt: 200, payload: { attempt: 2 } }));
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.retry.started', sessionID: 'ses_1', createdAt: 100, payload: { attempt: 1 } }));
    expect(draft.loaders.ses_1?.retry?.attempt).toBe(2);
  });
});

describe('applyOmpEvent — compaction + custom + volatile TTL state', () => {
  test('compaction started/ended drive the loader', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.compaction.started', sessionID: 'ses_1', createdAt: 10, payload: { reason: 'context-full', action: 'snapcompact' } }));
    expect(draft.loaders.ses_1?.compaction?.action).toBe('snapcompact');
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.compaction.ended', sessionID: 'ses_1', createdAt: 20, payload: { action: 'snapcompact', aborted: false } }));
    expect(draft.loaders.ses_1?.compaction).toBe(undefined);
  });

  test('custom.appended joins details by wireMessageID; display:false never stores a card', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({
      id: 1, type: 'omp.custom.appended', sessionID: 'ses_1',
      payload: { message: { wireMessageID: 'msg_c1', customType: 'irc:incoming', attribution: 'irc', text: 'hi', display: true } },
    }));
    expect(draft.customDetails.msg_c1?.customType).toBe('irc:incoming');
    const outcome = applyOmpEvent(draft, envelope({
      id: 2, type: 'omp.custom.appended', sessionID: 'ses_1',
      payload: { message: { wireMessageID: 'msg_c2', customType: 'eager-todo-prelude', display: false } },
    }));
    expect(outcome.changed).toBe(false);
    expect(draft.customDetails.msg_c2).toBe(undefined);
  });

  test('identical custom.appended replays are no-ops', () => {
    const draft = state();
    const frame = envelope({
      id: 1, type: 'omp.custom.appended', sessionID: 'ses_1',
      payload: { message: { wireMessageID: 'msg_c1', customType: 'advisor' } },
    });
    applyOmpEvent(draft, frame);
    const before = draft.customDetails.msg_c1;
    applyOmpEvent(draft, { ...frame, id: 3 });
    expect(draft.customDetails.msg_c1).toBe(before);
  });

  test('ttsr consecutive triggers merge into one warning block', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.ttsr.triggered', sessionID: 'ses_1', createdAt: 10, payload: { rules: [{ name: 'no-secrets' }] } }));
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.ttsr.triggered', sessionID: 'ses_1', createdAt: 20, payload: { rules: [{ name: 'stay-on-task' }] } }));
    expect(draft.ttsr.ses_1?.rules).toEqual(['no-secrets', 'stay-on-task']);
  });

  test('session.settled only marks awaiting-async for isTerminal:false', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.session.settled', sessionID: 'ses_1', payload: { isTerminal: true } }));
    expect(draft.awaitingAsync.ses_1).toBe(undefined);
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.session.settled', sessionID: 'ses_1', payload: { isTerminal: false } }));
    expect(draft.awaitingAsync.ses_1).toEqual({ since: 1000 });
  });
});

describe('applyOmpEvent — model/fallback/mode/goal/thinking', () => {
  test('fallback.applied then succeeded flip the badge state', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.fallback.applied', sessionID: 'ses_1', payload: { from: 'anthropic/claude', to: 'openai/gpt' } }));
    expect(draft.fallback.ses_1?.active).toBe(true);
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.fallback.succeeded', sessionID: 'ses_1', payload: { model: 'openai/gpt' } }));
    expect(draft.fallback.ses_1?.active).toBe(false);
    expect(draft.fallback.ses_1?.model).toBe('openai/gpt');
  });

  test('model.changed updates without clobbering fields the payload omits', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.model.changed', sessionID: 'ses_1', payload: { model: { provider: 'p1', id: 'm1' }, role: 'default' } }));
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.model.changed', sessionID: 'ses_1', payload: { model: { id: 'm2' } } }));
    expect(draft.sessionModel.ses_1?.provider).toBe('p1');
    expect(draft.sessionModel.ses_1?.id).toBe('m2');
    expect(draft.sessionModel.ses_1?.role).toBe('default');
  });

  test('identical mode.changed is a no-op; different mode replaces', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.mode.changed', sessionID: 'ses_1', payload: { mode: 'goal' } }));
    expect(applyOmpEvent(draft, envelope({ id: 2, type: 'omp.mode.changed', sessionID: 'ses_1', payload: { mode: 'goal' } })).changed).toBe(false);
    expect(applyOmpEvent(draft, envelope({ id: 3, type: 'omp.mode.changed', sessionID: 'ses_1', payload: { mode: 'plan' } })).changed).toBe(true);
    expect(draft.mode.ses_1?.mode).toBe('plan');
  });
});

describe('applyOmpEvent — effects', () => {
  test('notice.raised produces a toast effect with no stored state', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({
      id: 1, type: 'omp.notice.raised',
      payload: { level: 'error', message: 'provider quota exceeded', source: 'pi-ai' },
    }));
    expect(outcome.changed).toBe(false);
    expect(outcome.effects).toEqual([{ kind: 'notice', level: 'error', message: 'provider quota exceeded', source: 'pi-ai' }]);
  });

  test('settings.updated emits a refetch effect only on a revision jump', () => {
    const draft = state();
    const first = applyOmpEvent(draft, envelope({ id: 1, type: 'omp.settings.updated', payload: { revision: 7, keys: ['followUpBehavior'], origin: 'web' } }));
    expect(first.effects).toEqual([{ kind: 'settings-revision', revision: 7, keys: ['followUpBehavior'], origin: 'web' }]);
    const repeat = applyOmpEvent(draft, envelope({ id: 2, type: 'omp.settings.updated', payload: { revision: 7, keys: ['x'], origin: 'web' } }));
    expect(repeat.changed).toBe(false);
    expect(repeat.effects).toHaveLength(0);
    const older = applyOmpEvent(draft, envelope({ id: 3, type: 'omp.settings.updated', payload: { revision: 6 } }));
    expect(older.changed).toBe(false);
    const jump = applyOmpEvent(draft, envelope({ id: 4, type: 'omp.settings.updated', payload: { revision: 8 } }));
    expect(jump.effects[0]?.kind).toBe('settings-revision');
  });
});

describe('applyOmpEvent — other-domain tracking + unknown types', () => {
  test('dialog/jobs/tree/plan events only advance domain lastEventId', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 3, type: 'omp.dialog.requested', payload: { dialog: { id: 'dlg_1' } } }));
    applyOmpEvent(draft, envelope({ id: 4, type: 'omp.tree.updated', payload: { leafId: 'leaf' } }));
    applyOmpEvent(draft, envelope({ id: 5, type: 'omp.plan.updated', payload: { planFilePath: '/tmp/plan.md' } }));
    expect(draft.domains.lastEventId).toBe(5);
  });

  test('agents.updated tracks the highest revision only', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.agents.updated', payload: { revision: 10 } }));
    expect(applyOmpEvent(draft, envelope({ id: 2, type: 'omp.agents.updated', payload: { revision: 9 } })).changed).toBe(false);
    expect(draft.domains.agentsRevision).toBe(10);
  });

  test('queue.changed versions are monotonic per session', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.queue.changed', sessionID: 'ses_1', payload: { version: 3 } }));
    expect(applyOmpEvent(draft, envelope({ id: 2, type: 'omp.queue.changed', sessionID: 'ses_1', payload: { version: 3 } })).changed).toBe(false);
    expect(draft.domains.queueVersionBySession.ses_1).toBe(3);
  });

  test('unknown event types are ignored without throwing', () => {
    const draft = state();
    let threw = false;
    try { applyOmpEvent(draft, envelope({ id: 1, type: ['omp','unknown','futureevent'].join('.') })); } catch { threw = true; }
    expect(threw).toBe(false);
    expect(applyOmpEvent(draft, envelope({ id: 1, type: ['omp','domain','futureevent'].join('.') })).changed).toBe(false);
  });

  test('payload schema violations drop the frame without state change', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.retry.started', sessionID: 'ses_1', payload: { attempt: 'not-a-number' } }));
    expect(draft.loaders.ses_1).toBe(undefined);
    expect(draft.lastAppliedEventId).toBe(1);
  });
});

describe('applyOmpEvent — usage telemetry', () => {
  test('turns are keyed by messageID and bounded', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.usage.turn', sessionID: 'ses_1', payload: { messageID: 'm1', usage: { output: 10 }, ttftMs: 120, durationMs: 900 } }));
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.usage.turn', sessionID: 'ses_1', payload: { messageID: 'm1', usage: { output: 12 }, ttftMs: 120, durationMs: 900 } }));
    expect(draft.telemetry.ses_1).toHaveLength(1);
    expect((draft.telemetry.ses_1?.[0]?.usage as { output: number }).output).toBe(12);
  });
});

describe('applyOmpEvent — dialogs (spec 03 §5.6.3)', () => {
  const approvalPayload = {
    dialog: {
      id: 'dlg_a',
      sessionId: 'ses_1',
      createdAt: 100,
      kind: 'approval',
      approval: { prompt: 'Allow bash?', toolName: 'bash', tier: 'write' },
    },
  };

  test('requested parses the dialog and emits the effect, advancing the id gate', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({ id: 7, type: 'omp.dialog.requested', payload: approvalPayload }));
    expect(outcome.changed).toBe(true);
    expect(draft.lastAppliedEventId).toBe(7);
    expect(draft.domains.lastEventId).toBe(7);
    expect(outcome.effects).toEqual([{
      kind: 'dialog-requested',
      dialog: {
        id: 'dlg_a', sessionId: 'ses_1', createdAt: 100, kind: 'approval',
        approval: { prompt: 'Allow bash?', toolName: 'bash', tier: 'write' },
      },
    }]);
  });

  test('requested with a malformed dialog is consumed with no state change', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({ id: 8, type: 'omp.dialog.requested', payload: { dialog: { kind: 'approval' } } }));
    expect(outcome.effects).toEqual([]);
    expect(draft.lastAppliedEventId).toBe(8);
  });

  test('settled emits the settle effect with outcome', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({
      id: 9, type: 'omp.dialog.settled',
      payload: { dialogId: 'dlg_a', sessionId: 'ses_1', outcome: 'responded' },
    }));
    expect(outcome.effects).toEqual([{ kind: 'dialog-settled', dialogId: 'dlg_a', sessionId: 'ses_1', outcome: 'responded' }]);
  });

  test('ask payload parses through the effect boundary (multi/recommended preserved)', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({
      id: 10, type: 'omp.dialog.requested',
      payload: {
        dialog: {
          id: 'dlg_b', sessionId: 'ses_1', createdAt: 200, kind: 'ask',
          ask: {
            questions: [{
              id: 'q1', question: 'Pick one', options: [{ label: 'A' }, { label: 'B', description: 'second' }],
              multi: true, recommended: 0,
            }],
            timeoutMs: 0,
          },
        },
      },
    }));
    const effect = outcome.effects[0];
    expect(effect?.kind).toBe('dialog-requested');
    if (effect?.kind !== 'dialog-requested' || effect.dialog.kind !== 'ask') {
      throw new Error('expected parsed ask dialog effect');
    }

    expect(effect.dialog.ask.questions[0]).toEqual({
      id: 'q1', question: 'Pick one', options: [{ label: 'A' }, { label: 'B', description: 'second' }],
      multi: true, recommended: 0,
    });
  });
});

describe('applyOmpEvent — extension chrome (spec 09 §5.0)', () => {
  test('widget set stores lines+placement; clear removes; identical set is a no-op', () => {
    const draft = state();
    const set = (id: number, payload: Record<string, unknown>) =>
      applyOmpEvent(draft, envelope({ id, type: 'omp.chrome.updated', sessionID: 'ses_1', payload }));
    expect(set(1, { kind: 'widget', key: 'zhipu', lines: ['GLM Max', '16%'], placement: 'aboveEditor' }).changed).toBe(true);
    expect(draft.chrome.widgets.zhipu).toEqual({
      key: 'zhipu', lines: ['GLM Max', '16%'], placement: 'aboveEditor', sessionId: 'ses_1', updatedAt: 1000,
    });
    // Identical content re-set: no change, but id gate still consumes.
    expect(set(2, { kind: 'widget', key: 'zhipu', lines: ['GLM Max', '16%'], placement: 'aboveEditor' }).changed).toBe(false);
    expect(set(3, { kind: 'widget', key: 'zhipu' }).changed).toBe(true);
    expect(draft.chrome.widgets.zhipu).toBe(undefined);
    // Clearing an absent widget: no change.
    expect(set(4, { kind: 'widget', key: 'zhipu' }).changed).toBe(false);
  });

  test('widget without placement defaults to aboveEditor on read; status set/clear mirrors', () => {
    const draft = state();
    applyOmpEvent(draft, envelope({ id: 1, type: 'omp.chrome.updated', sessionID: 's', payload: { kind: 'widget', key: 'k', lines: ['x'] } }));
    expect(draft.chrome.widgets.k?.placement).toBe(undefined);
    applyOmpEvent(draft, envelope({ id: 2, type: 'omp.chrome.updated', sessionID: 's', payload: { kind: 'status', key: 'tps', text: '38 tok/s' } }));
    expect(draft.chrome.status.tps?.text).toBe('38 tok/s');
    applyOmpEvent(draft, envelope({ id: 3, type: 'omp.chrome.updated', sessionID: 's', payload: { kind: 'status', key: 'tps' } }));
    expect(draft.chrome.status.tps).toBe(undefined);
  });

  test('malformed payloads are dropped without touching state', () => {
    const draft = state();
    const outcome = applyOmpEvent(draft, envelope({ id: 1, type: 'omp.chrome.updated', payload: { kind: 'widget' } }));
    expect(outcome.changed).toBe(false);
    expect(draft.chrome.widgets).toEqual({});
  });
});
