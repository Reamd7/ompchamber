/**
 * WorkStatus subagents blocking badge (spec 03 §5.6.4, GAP-C10): a child
 * session with a pending omp dialog must show the omp "waiting for your
 * answer" badge, taking precedence over the legacy permission/question
 * badges (kept as fallback until the P3 protocol removal).
 *
 * renderToStaticMarkup reads zustand's server snapshot, so the data sources
 * are stubbed at module boundaries and the assertions cover the component's
 * precedence chain over the exact values its hooks return.
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { Session } from '@/lib/opencode/wire';
import type { State } from '@/sync/types';

let liveSessions: Session[] = [];
let statuses: Record<string, { type: 'busy' | 'idle' }> = {};
let legacyPermission: Record<string, unknown[]> = {};
let legacyQuestions: Record<string, unknown[]> = {};
let ompCounts = new Map<string, number>();
let agentRunsEnabled = false;
let agentRunsRows: Array<{
  key: string; sessionID: string; directory: string; agentId: string;
  displayName: string; status: string; createdAt: number; lastActivity: number;
  live?: { tokens: number; cost: number; durationMs: number };
}> | null = null;

mock.module('@/sync/sync-context', () => ({
  useAllLiveSessions: () => liveSessions,
  useAllSessionStatuses: () => statuses,
  useDirectorySync: (selector: (state: State) => unknown) =>
    selector({ permission: legacyPermission, question: legacyQuestions } as State),
}));

mock.module('@/sync/useOmpDialogStore', () => ({
  useOmpPendingDialogSessions: () => ompCounts,
}));

mock.module('@/hooks/useOmpFeatureEnabled', () => ({
  useOmpFeatureEnabled: (key: string) => key === 'agentRuns.v1' && agentRunsEnabled,
}));
mock.module('@/stores/useOmpAgentRunsStore', () => ({
  useOmpAgentRunsForDirectory: () => agentRunsRows,
  useOmpAgentRunsRevision: () => undefined,
  useOmpAgentRunsStore: Object.assign(
    (selector: (state: { load: () => Promise<void> }) => unknown) => selector({ load: async () => undefined }),
    { getState: () => ({ load: async () => undefined }) },
  ),
}));


mock.module('@/sync/useOmpSessionStore', () => ({
  useOmpSessionStore: (selector: (state: { directories: Record<string, unknown> }) => unknown) =>
    selector({ directories: {} }),
}));

import { I18nProvider } from '@/lib/i18n';
import { WorkStatusSubagentsSection } from './WorkStatusSubagentsSection';

const childSession = (id: string, title: string): Session =>
  ({ id, title, parentID: 'ses_parent' } as Session);

const render = (): string =>
  renderToStaticMarkup(
    <I18nProvider>
      <WorkStatusSubagentsSection sessionId="ses_parent" directory="/repo" />
    </I18nProvider>,
  );

describe('WorkStatusSubagentsSection blocking badge (omp dialogs)', () => {
  beforeEach(() => {
    liveSessions = [childSession('ses_child_a', 'Alpha'), childSession('ses_child_b', 'Beta')];
    statuses = { ses_child_a: { type: 'busy' }, ses_child_b: { type: 'busy' } };
    legacyPermission = {};
    legacyQuestions = {};
    ompCounts = new Map();
    agentRunsEnabled = false;
    agentRunsRows = null;
  });

  test('child with a pending omp dialog shows the omp waiting badge', () => {
    ompCounts = new Map([['ses_child_a', 2]]);
    const markup = render();
    expect(markup).toContain('waiting for your answer');
    // The sibling without dialogs keeps the plain busy readout.
    expect(markup).toContain('is working');
  });

  test('omp badge wins over the legacy permission badge when both apply', () => {
    ompCounts = new Map([['ses_child_a', 1]]);
    legacyPermission = { ses_child_a: [{ id: 'perm_1' }] };
    const markup = render();
    expect(markup).toContain('waiting for your answer');
    expect(markup).not.toContain('needs permission');
  });

  test('legacy permission badge still renders when only the legacy signal fires', () => {
    legacyPermission = { ses_child_a: [{ id: 'perm_1' }] };
    const markup = render();
    expect(markup).toContain('needs permission');
    expect(markup).not.toContain('waiting for your answer');
  });

  test('section renders nothing without child sessions', () => {
    liveSessions = [];
    expect(render()).toBe('');
  });
});

describe('WorkStatusSubagentsSection omp agent-runs source (08 GAP-04)', () => {
  beforeEach(() => {
    liveSessions = [];
    statuses = {};
    legacyPermission = {};
    legacyQuestions = {};
    ompCounts = new Map();
    agentRunsEnabled = true;
    agentRunsRows = null;
  });

  test('agentRuns.v1 renders session rows with running/parked states', () => {
    agentRunsRows = [
      { key: 'ses_parent::Main', sessionID: 'ses_parent', directory: '/repo', agentId: 'Main', displayName: 'Main', status: 'running', createdAt: 1, lastActivity: 9 },
      { key: 'ses_parent::Anna', sessionID: 'ses_parent', directory: '/repo', agentId: 'Anna', displayName: 'Anna', status: 'running', createdAt: 1, lastActivity: 8 },
      { key: 'ses_parent::Belle', sessionID: 'ses_parent', directory: '/repo', agentId: 'Belle', displayName: 'Belle', status: 'parked', createdAt: 1, lastActivity: 7 },
      { key: 'ses_other::Ghost', sessionID: 'ses_other', directory: '/repo', agentId: 'Ghost', displayName: 'Ghost', status: 'idle', createdAt: 1, lastActivity: 6 },
    ];
    const markup = render();
    expect(markup).toContain('Anna');
    expect(markup).toContain('Belle');
    expect(markup).toContain('parked');
    // Main and other sessions' rows never appear.
    expect(markup).not.toContain('>Main<');
    expect(markup).not.toContain('Ghost');
  });

  test('agentRuns.v1 rows surface live usage, duration and cost when present', () => {
    agentRunsRows = [
      { key: 'ses_parent::Anna', sessionID: 'ses_parent', directory: '/repo', agentId: 'Anna', displayName: 'Anna', status: 'running', createdAt: 1, lastActivity: 8, live: { tokens: 1500, cost: 0.05, durationMs: 65000 } },
    ];
    const markup = render();
    expect(markup).toContain('Anna');
    expect(markup).toContain('is working');
    expect(markup).toContain('1.5K');
    expect(markup).toContain('1m05s');
    expect(markup).toContain('$0.05');
  });

  test('agentRuns.v1 rows stay status-only without live metrics', () => {
    agentRunsRows = [
      { key: 'ses_parent::Anna', sessionID: 'ses_parent', directory: '/repo', agentId: 'Anna', displayName: 'Anna', status: 'running', createdAt: 1, lastActivity: 8 },
    ];
    const markup = render();
    expect(markup).toContain('is working');
    expect(markup).not.toContain('$');
  });

  test('agentRuns.v1 with no rows renders nothing even with legacy children present', () => {
    liveSessions = [childSession('ses_child_a', 'Alpha')];
    expect(render()).toBe('');
  });
});
