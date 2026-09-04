import React from 'react';
import { useI18n } from '@/lib/i18n';
import { useAllLiveSessions, useAllSessionStatuses, useDirectorySync } from '@/sync/sync-context';
import { useUIStore } from '@/stores/useUIStore';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { isVSCodeRuntime } from '@/lib/desktop';
import { isEmbeddedSessionChat } from '@/components/layout/contextPanelEmbeddedChat';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import {
  useOmpAgentRunsForDirectory,
  useOmpAgentRunsRevision,
  useOmpAgentRunsStore,
} from '@/stores/useOmpAgentRunsStore';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import { WorkStatusCollapsibleSection, WorkStatusRow, WorkStatusValue } from './WorkStatusPrimitives';
import { useReportWorkStatusPresence } from './presenceContext';
import { useOmpPendingDialogSessions } from '@/sync/useOmpDialogStore';
import type { OmpAgentRunRecord } from '@/lib/api/omp';
import { formatCost } from './subagentCost';
import { useSubagentCostRollup } from './useSubagentCostRollup';
import { formatCompactTokenCount } from '../message/turnUsage';
import { formatAgentDuration } from '../message/parts/taskToolModel';
import type { State } from '@/sync/types';

type Props = {
  sessionId: string | null;
  directory: string | null;
};
const SECTION_ID = 'subagents';

/**
 * Running subagents and, more importantly, their blockers: a dialog raised by
 * a child session has no representation in the transcript, so this panel is
 * the only place it becomes visible. Pending omp dialogs are the primary
 * blocker signal (spec 03 §5.6.4); the legacy permission/question records
 * below stay as a fallback until the P3 protocol removal.
 */
export const WorkStatusSubagentsSection: React.FC<Props> = ({ sessionId, directory }) => {
  const { t } = useI18n();
  const isMobile = useUIStore((state) => state.isMobile);

  // omp agent-runs source (spec 08 GAP-04 → 04 §5.5.1): one row per
  // sessionID::agentId under agentRuns.v1; legacy child-session rows below
  // stay the exact pre-capability behavior when the key is off/unsettled.
  const agentRunsEnabled = useOmpFeatureEnabled('agentRuns.v1');
  const agentRuns = useOmpAgentRunsForDirectory(directory);
  const snapshotRevision = useOmpAgentRunsRevision(directory);
  const loadAgentRuns = useOmpAgentRunsStore((s) => s.load);
  // The event pipeline's agentsRevision is monotonic per directory; a jump
  // past our snapshot revision means the snapshot is stale → forced refetch.
  const eventRevision = useOmpSessionStore(
    React.useCallback(
      (state) => (directory ? state.directories[directory]?.domains.agentsRevision : undefined),
      [directory],
    ),
  );
  React.useEffect(() => {
    void loadAgentRuns(directory, {
      force: eventRevision !== undefined && snapshotRevision !== undefined && eventRevision > snapshotRevision,
    });
  }, [loadAgentRuns, directory, eventRevision, snapshotRevision]);

  const runs = React.useMemo(
    () => (agentRunsEnabled && sessionId && agentRuns
      ? agentRuns.filter((row: OmpAgentRunRecord) => row.sessionID === sessionId && row.agentId !== 'Main')
      : []),
    [agentRunsEnabled, agentRuns, sessionId],
  );

  const liveSessions = useAllLiveSessions();
  const statuses = useAllSessionStatuses();
  const children = React.useMemo(
    () => (sessionId && !agentRunsEnabled ? liveSessions.filter((candidate) => candidate.parentID === sessionId) : []),
    [liveSessions, sessionId, agentRunsEnabled],
  );

  // Each child's own subtree total (its cost plus every descendant of its
  // own), so nested subagent-of-subagent cost rolls up under the immediate
  // child row shown here rather than disappearing.
  const { perChildCost } = useSubagentCostRollup(sessionId);

  // One subscription covers every child: per-session hooks would multiply
  // store subscriptions by the number of subagents.
  const permissions = useDirectorySync(React.useCallback((state: State) => state.permission, []));
  const questions = useDirectorySync(React.useCallback((state: State) => state.question, []));

  // Authoritative blocker signal (spec 03 §5.6.4): one subscription covers
  // every child's pending omp dialog count for this directory.
  const ompDialogCounts = useOmpPendingDialogSessions(directory ?? '');

  const openContextPanelTab = useUIStore((state) => state.openContextPanelTab);
  const setCurrentSession = useSessionUIStore((state) => state.setCurrentSession);
  const setSectionExpanded = useUIStore((state) => state.setWorkStatusSectionExpanded);

  // Subagents appearing where there were none is the one moment this section
  // has something urgent to say, so it opens itself. Only on the empty→present
  // edge: re-expanding on every count change would fight a user who just
  // collapsed it.
  const hadChildren = React.useRef(children.length > 0);
  React.useEffect(() => {
    const present = children.length > 0;
    if (present && !hadChildren.current) setSectionExpanded(SECTION_ID, true);
    hadChildren.current = present;
  }, [children.length, setSectionExpanded]);

  // Same branch the transcript's Task tool takes: surfaces that cannot host an
  // embedded panel navigate to the child session instead of opening a tab.
  const openChildSession = React.useCallback((childId: string, label: string) => {
    if (!directory) return;
    if (isEmbeddedSessionChat() || isMobile || isVSCodeRuntime()) {
      setCurrentSession(childId, directory);
      return;
    }
    openContextPanelTab(directory, {
      mode: 'chat',
      dedupeKey: `session:${childId}`,
      label,
      readOnly: true,
    });
  }, [directory, isMobile, openContextPanelTab, setCurrentSession]);

  useReportWorkStatusPresence('subagents', children.length > 0 || runs.length > 0);

  if (agentRunsEnabled) {
    if (runs.length === 0) return null;
    const busyRuns = runs.filter((row) => row.status === 'running').length;
    return (
      <WorkStatusCollapsibleSection
        id={SECTION_ID}
        title={t('chat.workStatus.section.subagents')}
        icon="ai-agent"
        defaultExpanded
        summary={busyRuns > 0 ? `${busyRuns}/${runs.length}` : runs.length}
      >
        <div className="max-h-56 overflow-y-auto">
        {runs.map((row) => {
          const label = row.displayName?.trim() || row.agentId;
          const ompBlocked = (ompDialogCounts.get(row.sessionID) ?? 0) > 0;
          const live = row.live;
          const statusValue = ompBlocked && row.status === 'running' ? (
            <WorkStatusValue tone="warning">{t('dialogs.omp.workStatus.waitingAnswer')}</WorkStatusValue>
          ) : row.status === 'running' ? (
            <WorkStatusValue tone="info">{t('chat.workStatus.subagent.working')}</WorkStatusValue>
          ) : row.status === 'parked' ? (
            <WorkStatusValue tone="warning">{t('chat.workStatus.subagent.parked')}</WorkStatusValue>
          ) : row.status === 'aborted' ? (
            <WorkStatusValue tone="muted">{t('chat.workStatus.subagent.aborted')}</WorkStatusValue>
          ) : (
            <WorkStatusValue tone="muted">{t('chat.workStatus.subagent.done')}</WorkStatusValue>
          );
          return (
            <WorkStatusRow
              key={row.key}
              label={label}
              onClick={directory && !isEmbeddedSessionChat() && !isMobile && !isVSCodeRuntime() && row.hasTranscript
                ? () => openContextPanelTab(directory, {
                    mode: 'agentRun',
                    dedupeKey: `run:${row.key}`,
                    label,
                    readOnly: true,
                    agentRun: { sessionID: row.sessionID, agentId: row.agentId },
                  })
                : undefined}
              ariaLabel={t('chat.workStatus.action.openAgentRun', { name: label })}
              value={(
                <>
                  {statusValue}
                  {live && live.tokens > 0 ? <WorkStatusValue tone="muted">{formatCompactTokenCount(live.tokens)}</WorkStatusValue> : null}
                  {live && live.durationMs > 0 ? <WorkStatusValue tone="muted">{formatAgentDuration(live.durationMs)}</WorkStatusValue> : null}
                  {live && live.cost > 0 ? <WorkStatusValue tone="muted">{formatCost(live.cost)}</WorkStatusValue> : null}
                </>
              )}
            />
          );
        })}
        </div>
      </WorkStatusCollapsibleSection>
    );
  }

  if (children.length === 0) return null;

  const busyChildren = children.filter((child) => statuses[child.id]?.type === 'busy').length;

  return (
    <WorkStatusCollapsibleSection
      id={SECTION_ID}
      title={t('chat.workStatus.section.subagents')}
      icon="ai-agent"
      defaultExpanded
      summary={busyChildren > 0 ? `${busyChildren}/${children.length}` : children.length}
    >
      <div className="max-h-56 overflow-y-auto">
        {children.map((child) => {
          const ompBlocked = (ompDialogCounts.get(child.id) ?? 0) > 0;
          const blocked = (permissions[child.id]?.length ?? 0) > 0;
          const asked = (questions[child.id]?.length ?? 0) > 0;
          const busy = statuses[child.id]?.type === 'busy';
          const label = child.title?.trim() || t('chat.workStatus.subagent.untitled');
          const childCost = perChildCost.get(child.id) ?? 0;
          return (
            <WorkStatusRow
              key={child.id}
              onClick={directory ? () => openChildSession(child.id, label) : undefined}
              ariaLabel={t('chat.workStatus.action.openSubagent', { name: label })}
              label={label}
              value={(
                <>
                  {ompBlocked ? (
                    <WorkStatusValue tone="warning">{t('dialogs.omp.workStatus.waitingAnswer')}</WorkStatusValue>
                  ) : blocked ? (
                    <WorkStatusValue tone="warning">{t('chat.workStatus.subagent.needsPermission')}</WorkStatusValue>
                  ) : asked ? (
                    <WorkStatusValue tone="warning">{t('chat.workStatus.subagent.askedQuestion')}</WorkStatusValue>
                  ) : busy ? (
                    <WorkStatusValue tone="info">{t('chat.workStatus.subagent.working')}</WorkStatusValue>
                  ) : (
                    <WorkStatusValue tone="muted">{t('chat.workStatus.subagent.done')}</WorkStatusValue>
                  )}
                  {childCost > 0 ? <WorkStatusValue tone="muted">{formatCost(childCost)}</WorkStatusValue> : null}
                </>
              )}
            />
          );
        })}
      </div>
    </WorkStatusCollapsibleSection>
  );
};
