/**
 * WorkStatus goal row source switch (spec 08 GAP-05): under modes.v1 the row
 * reads the omp goal projection (omp.goal.updated payload: objective/status,
 * read-only — the bridge has no goal write endpoints yet); with the
 * capability off/unsettled the legacy OC goal row (pause/resume actions,
 * SessionGoalDialog) renders exactly as before.
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

let modesEnabled = false;
let ompGoalPayload: { goal?: unknown; state?: string; updatedAt: number } | null = null;
let legacyGoal: { objective: string; status: string } | null = null;

mock.module('@/hooks/useOmpFeatureEnabled', () => ({
  useOmpFeatureEnabled: (key: string) => key === 'modes.v1' && modesEnabled,
}));

mock.module('@/hooks/useSessionGoal', () => ({
  useSessionGoal: () => ({ goal: legacyGoal, enabled: legacyGoal !== null }),
}));

mock.module('@/sync/useOmpSessionStore', () => ({
  useOmpGoalState: () => ompGoalPayload,
  useOmpSessionStore: Object.assign(
    (selector: (state: Record<string, unknown>) => unknown) => selector({}),
    {},
  ),
}));

mock.module('@/lib/sessionGoalActions', () => ({
  setSessionGoalStatus: mock(async () => undefined),
  setSessionGoal: mock(async () => undefined),
  clearSessionGoal: mock(async () => undefined),
  fetchGoalObjectiveContent: async () => null,
}));


mock.module('@/components/chat/SessionGoalDialog', () => ({
  SessionGoalDialog: () => <div data-testid="legacy-goal-dialog" />,
}));

const { I18nProvider } = await import('@/lib/i18n');
const { WorkStatusGoalRow } = await import('./WorkStatusGoalRow');

const render = (): string =>
  renderToStaticMarkup(
    <I18nProvider>
      <WorkStatusGoalRow sessionId="ses_1" directory="/repo" />
    </I18nProvider>,
  );

describe('WorkStatusGoalRow source switch (08 GAP-05)', () => {
  beforeEach(() => {
    modesEnabled = false;
    ompGoalPayload = null;
    legacyGoal = null;
  });

  test('modes.v1 on: omp objective + status render read-only (no pause/resume actions)', () => {
    modesEnabled = true;
    ompGoalPayload = {
      goal: { objective: 'Ship the tree dialog', status: 'active', tokensUsed: 10 },
      state: 'active',
      updatedAt: 1,
    };
    const markup = render();
    expect(markup).toContain('Ship the tree dialog');
    expect(markup).toContain('active');
    // Read-only projection: the legacy pause affordance must not appear.
    expect(markup).not.toContain('Pause');
  });

  test('modes.v1 on: malformed or absent goal payload hides the row', () => {
    modesEnabled = true;
    ompGoalPayload = { goal: { objective: '', status: 'active' }, updatedAt: 1 };
    expect(render()).toBe('');

    ompGoalPayload = { goal: { objective: 'x', status: 'sleeping' }, updatedAt: 1 };
    expect(render()).toBe('');

    ompGoalPayload = null;
    expect(render()).toBe('');
  });

  test('modes.v1 off: legacy goal row with pause action renders unchanged', () => {
    legacyGoal = { objective: 'Legacy goal text', status: 'active' };
    const markup = render();
    expect(markup).toContain('Legacy goal text');
    expect(markup).toContain('Pause');
  });

  test('modes.v1 off without a legacy goal renders nothing', () => {
    expect(render()).toBe('');
  });
});
