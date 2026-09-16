import React from 'react';
import { useI18n } from '@/lib/i18n';
import { useAllLiveSessions, useAllSessionStatuses, useDirectorySync } from '@/sync/sync-context';
import { useUIStore } from '@/stores/useUIStore';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { isVSCodeRuntime } from '@/lib/desktop';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { isEmbeddedSessionChat } from '@/components/layout/contextPanelEmbeddedChat';
import { formatAgentDuration, readTaskRunTitle } from '../message/parts/taskToolModel';
import { getSyncMessages, getSyncParts } from '@/sync/sync-refs';
import { openSubagentRun } from '@/lib/omp/openSubagentRun';
import {
  useOmpAgentRunsForDirectory,
  useOmpAgentRunsRevision,
  useOmpAgentRunsStore,
} from '@/stores/useOmpAgentRunsStore';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import { WorkStatusCollapsibleSection, WorkStatusRow, WorkStatusValue } from './WorkStatusPrimitives';
import { useReportWorkStatusPresence } from './presenceContext';
import { useOmpPendingDialogSessions } from '@/sync/useOmpDialogStore';
import { formatCost } from './subagentCost';
import { useSubagentCostRollup } from './useSubagentCostRollup';
import { formatCompactTokenCount } from '../message/turnUsage';
import type { State } from '@/sync/types';
import { createOmpTreeAPI, type OmpAgentRunRecord, type OmpFetchJsonResult, type OmpSessionTreeSnapshot } from '@/lib/api/omp';

type Props = {
  sessionId: string | null;
  directory: string | null;
};
const SECTION_ID = 'subagents';
const treeApi = createOmpTreeAPI();

/**
 * Ancestor session ids of `sessionId` inside its fork tree. Forks inherit
 * the artifacts directory of the session they forked from, so a run's
 * agent-runs row is keyed to an ancestor session while its task card lives
 * in the fork's transcript — this set is what lets the fork's panel adopt
 * those rows. Cycle- and gap-tolerant: a malformed parentId chain yields
 * whatever was collected before the break.
 */
export const ancestorLineageOf = (snapshot: OmpSessionTreeSnapshot, sessionId: string): Set<string> => {
  const byId = new Map(snapshot.nodes.map((node) => [node.id, node]));
  const lineage = new Set<string>();
  const visited = new Set<string>([sessionId]);
  let cursor = byId.get(sessionId);
  while (cursor?.parentId) {
    const parent = byId.get(cursor.parentId);
    if (!parent || visited.has(parent.id)) break;
    visited.add(parent.id);
    lineage.add(parent.id);
    cursor = parent;
  }
  return lineage;
};

/** Tree fetch source; overridable in tests (mock.module on the api module is
 * not reliable for this import graph, so the seam lives here). */
let fetchSessionTree: (sessionId: string, directory: string) =>
  Promise<OmpFetchJsonResult<OmpSessionTreeSnapshot>> = (sessionId, directory) =>
    treeApi.getSessionTree(sessionId, { directory });

const lineageByKey = new Map<string, Set<string>>();
const lineageInFlight = new Map<string, Promise<Set<string> | null>>();

/**
 * Fetch and cache the fork lineage once per (directory, session). Failures
 * (tree.v1 off, transport, malformed payload) leave the cache unset so the
 * section keeps its current-session-only behavior — never an authoritative
 * "no ancestors" answer built from a failed read.
 */
export const primeSubagentLineage = (sessionId: string, directory: string): Promise<Set<string> | null> => {
  const key = `${directory}\n${sessionId}`;
  const cached = lineageByKey.get(key);
  if (cached) return Promise.resolve(cached);
  const existing = lineageInFlight.get(key);
  if (existing) return existing;
  const request = (async () => {
    const result = await fetchSessionTree(sessionId, directory);
    if (!result.ok) return null;
    const lineage = ancestorLineageOf(result.data, sessionId);
    if (lineage.size > 0) lineageByKey.set(key, lineage);
    return lineage;
  })().catch(() => null).finally(() => {
    lineageInFlight.delete(key);
  });
  lineageInFlight.set(key, request);
  return request;
};

/** Test seam: clears the lineage cache between cases (capabilityGate pattern). */
export const __clearSubagentLineageForTests = (): void => {
  lineageByKey.clear();
  lineageInFlight.clear();
};

/** Test seam: replaces the tree fetch source. */
export const __setSubagentTreeSourceForTests = (
  source: (sessionId: string, directory: string) => Promise<OmpFetchJsonResult<OmpSessionTreeSnapshot>>,
): void => {
  fetchSessionTree = source;
};

/** Synchronous cache read; `primeSubagentLineage` fills it. */
const subagentLineageOf = (sessionId: string, directory: string): Set<string> | null =>
  lineageByKey.get(`${directory}\n${sessionId}`) ?? null;

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

  const ownRuns = React.useMemo(
    () => (agentRunsEnabled && sessionId && agentRuns
      ? agentRuns.filter((row: OmpAgentRunRecord) => row.sessionID === sessionId && row.agentId !== 'Main')
      : []),
    [agentRunsEnabled, agentRuns, sessionId],
  );

  // Fork fallback (docs/plans/subagent-run-visibility stage 1): a fork's
  // inherited runs key their rows to ancestor sessions, so when this session
  // has no rows of its own, adopt ancestor rows via the fork lineage. Only
  // fetched on the empty branch — the common path never pays for the tree.
  const [lineageRead, setLineageRead] = React.useState(0);
  React.useEffect(() => {
    if (!agentRunsEnabled || !sessionId || !directory || ownRuns.length > 0) return;
    let cancelled = false;
    void primeSubagentLineage(sessionId, directory).then((lineage) => {
      if (!cancelled && lineage && lineage.size > 0) setLineageRead((count) => count + 1);
    });
    return () => {
      cancelled = true;
    };
  }, [agentRunsEnabled, sessionId, directory, ownRuns.length]);
  const lineage = React.useMemo(
    () => (sessionId && directory ? subagentLineageOf(sessionId, directory) : null),
    // lineageRead invalidates the memoized cache read after a prime lands.
    [sessionId, directory, lineageRead],
  );

  const runs = React.useMemo(() => {
    if (!agentRunsEnabled || !sessionId || !agentRuns) return [];
    if (ownRuns.length > 0) return ownRuns;
    if (!lineage) return [];
    return agentRuns.filter((row: OmpAgentRunRecord) => lineage.has(row.sessionID) && row.agentId !== 'Main');
  }, [agentRunsEnabled, agentRuns, sessionId, ownRuns, lineage]);


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
    // Dispatch titles for the loaded window, newest-first (the card usually
    // sits near the tail); one shared parts scan for every row.
    const runTitleFor = React.useCallback(
      (runId: string) => (sessionId
        ? readTaskRunTitle(
            (messageID) => getSyncParts(messageID, directory ?? undefined),
            getSyncMessages(sessionId, directory ?? undefined).map((message) => message.id),
            runId,
          )
        : null),
      [directory, sessionId],
    );
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
          // Card parity: prefer the dispatch title (the same chain the task
          // card header renders) read from the loaded session parts; the run
          // name stays the fallback for runs whose card is not loaded.
          const label = runTitleFor(row.agentId) ?? row.displayName?.trim() ?? row.agentId;
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
              onClick={directory && row.childSessionID && row.sessionID
                ? () => openSubagentRun({ childSessionID: row.childSessionID ?? '', parentSessionID: row.sessionID, runId: row.agentId, label, directory })
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
