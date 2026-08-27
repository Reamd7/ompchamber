/**
 * Turn-state snapshots on user messages (GAP-06 history tail).
 *
 * omp's SDK persists neither model nor thinking level on user messages —
 * only model_change / thinking_level_change transcript entries. The wire
 * projection must therefore stamp every user message with the exact state
 * it was sent with: the engine passes the effective level on live sends,
 * and buildTurnStateStamper folds the entry log for replays.
 */
import { describe, expect, test } from 'bun:test';

import {
  buildTurnStateStamper,
  deterministicWireId,
  projectConversation,
  projectTurnEventDivider,
  projectUserMessage,
} from './projection.js';

const now = 1_787_811_000_000;

const userMessage = (text, timestamp = now) => ({ role: 'user', content: text, timestamp });

describe('projectUserMessage thinking snapshot', () => {
  test('stamps the thinking level as model.variant and omits it when unknown', () => {
    const stamped = projectUserMessage(userMessage('hi'), {
      sessionID: 's1',
      model: { provider: 'p', id: 'm' },
      thinkingLevel: 'xhigh',
    });
    expect(stamped.info.model).toEqual({ providerID: 'p', modelID: 'm', variant: 'xhigh' });

    const bare = projectUserMessage(userMessage('hi'), {
      sessionID: 's1',
      model: { provider: 'p', id: 'm' },
    });
    expect(bare.info.model).toEqual({ providerID: 'p', modelID: 'm' });

    const empty = projectUserMessage(userMessage('hi'), {
      sessionID: 's1',
      model: { provider: 'p', id: 'm' },
      thinkingLevel: '',
    });
    expect(empty.info.model).toEqual({ providerID: 'p', modelID: 'm' });
  });
});

describe('buildTurnStateStamper', () => {
  test('folds model and thinking changes into per-message send-time state', () => {
    const first = userMessage('one', now);
    const second = userMessage('two', now + 10_000);
    const third = userMessage('three', now + 20_000);
    const entries = [
      { type: 'message', message: first, timestamp: now },
      { type: 'model_change', model: 'p/a', role: 'default', timestamp: now + 1_000 },
      { type: 'thinking_level_change', thinkingLevel: 'high', configured: 'high', timestamp: now + 2_000 },
      { type: 'message', message: second, timestamp: now + 10_000 },
      { type: 'model_change', model: 'p/b', role: 'temporary', timestamp: now + 11_000 },
      { type: 'thinking_level_change', thinkingLevel: null, configured: null, timestamp: now + 12_000 },
      { type: 'message', message: third, timestamp: now + 20_000 },
    ];
    const turnStateFor = buildTurnStateStamper(entries);
    expect(turnStateFor(first)).toEqual({ model: null, thinkingLevel: null });
    expect(turnStateFor(second)).toEqual({ model: 'p/a', thinkingLevel: 'high' });
    expect(turnStateFor(third)).toEqual({ model: 'p/b', thinkingLevel: null });
    // Non-user messages never resolve.
    expect(turnStateFor({ role: 'assistant', content: [], timestamp: now })).toBeNull();
    // Unknown messages (not in the log) resolve to null, never a stale state.
    expect(turnStateFor(userMessage('absent', now + 30_000))).toBeNull();
  });

  test('respects wireIdFor overrides the same way the projector does', () => {
    const message = userMessage('one', now);
    const entries = [
      { type: 'model_change', model: 'p/a', role: 'default', timestamp: now - 1 },
      { type: 'message', message, timestamp: now },
    ];
    const wireIdFor = (candidate) => (candidate === message ? 'msg_override' : undefined);
    const turnStateFor = buildTurnStateStamper(entries, { wireIdFor });
    expect(turnStateFor(message)).toEqual({ model: 'p/a', thinkingLevel: null });
  });
});

describe('projectConversation turn-state threading', () => {
  test('turn state overrides the projection-wide model and adds the variant', () => {
    const first = userMessage('one', now);
    const second = userMessage('two', now + 10_000);
    const byWireId = new Map([
      [deterministicWireId(first), { model: 'p/old', thinkingLevel: 'high' }],
      [deterministicWireId(second), { model: 'p/new', thinkingLevel: null }],
    ]);
    const projected = projectConversation([first, second], {
      sessionID: 's1',
      model: 'p/current',
      turnStateFor: (message) => byWireId.get(deterministicWireId(message)) ?? null,
    });
    expect(projected[0].info.model).toEqual({ providerID: 'p', modelID: 'old', variant: 'high' });
    expect(projected[1].info.model).toEqual({ providerID: 'p', modelID: 'new' });
  });

  test('messages without turn state keep the projection-wide model', () => {
    const projected = projectConversation([userMessage('one', now)], {
      sessionID: 's1',
      model: 'p/current',
      turnStateFor: () => null,
    });
    expect(projected[0].info.model).toEqual({ providerID: 'p', modelID: 'current' });
  });
});

describe('projectTurnEventDivider', () => {
  test('projects a role-tagged model change with fallback flag as a divider', () => {
    const wire = projectTurnEventDivider(
      { type: 'model_change', model: 'p/m', role: 'temporary', resolvedModelIsFallback: true, timestamp: now },
      { sessionID: 's1' },
    );
    expect(wire?.info.metadata).toEqual({
      ompRole: 'modelChange',
      model: 'p/m',
      role: 'temporary',
      fallback: true,
    });
    expect(wire?.parts[0]?.text).toBe('[omp:modelChange] p/m · temporary · fallback');
    expect(wire?.info.role).toBe('assistant');
    expect(wire?.info.parentID).toBeUndefined();
  });

  test('skips role-less init bookkeeping and unknown kinds', () => {
    expect(projectTurnEventDivider(
      { type: 'model_change', model: 'p/m', timestamp: now },
      { sessionID: 's1' },
    )).toBeNull();
    expect(projectTurnEventDivider(
      { type: 'thinking_level_change', thinkingLevel: 'high', timestamp: now },
      { sessionID: 's1' },
    )).toBeNull();
    expect(projectTurnEventDivider(null, { sessionID: 's1' })).toBeNull();
  });

  test('projects a mode change carrying the mode value', () => {
    const wire = projectTurnEventDivider(
      { type: 'mode_change', mode: 'plan', timestamp: now },
      { sessionID: 's1' },
    );
    expect(wire?.info.metadata).toEqual({ ompRole: 'modeChange', mode: 'plan' });
    expect(wire?.parts[0]?.text).toBe('[omp:modeChange] plan');
  });
});
