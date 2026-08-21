import React from 'react';
import { toast } from 'sonner';
import { Icon } from '@/components/icon/Icon';
import { useI18n, type I18nKey } from '@/lib/i18n';
import { useSessionGoal } from '@/hooks/useSessionGoal';
import { setSessionGoalStatus } from '@/lib/sessionGoalActions';
import { sessionGoalStatusColor } from '@/lib/sessionGoalPresentation';
import { SessionGoalDialog } from '@/components/chat/SessionGoalDialog';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { useOmpGoalState } from '@/sync/useOmpSessionStore';
import { WorkStatusRow, WorkStatusRowAction, WorkStatusValue } from './WorkStatusPrimitives';

type Props = {
  sessionId: string | null;
  directory: string | null;
};
/** omp goal status → presentation (SDK GoalStatus, goals/state.ts:4). */
const OMP_GOAL_STATUS_COLOR: Record<string, string> = {
  active: 'var(--status-info)',
  paused: 'var(--surface-muted-foreground)',
  'budget-limited': 'var(--status-warning)',
  complete: 'var(--status-success)',
  dropped: 'var(--surface-muted-foreground)',
};

const OMP_GOAL_STATUS_LABEL_KEY: Record<string, I18nKey> = {
  active: 'chat.workStatus.goal.status.active',
  paused: 'chat.workStatus.goal.status.paused',
  complete: 'chat.workStatus.goal.status.complete',
  'budget-limited': 'chat.workStatus.goal.status.budgetLimited',
  dropped: 'chat.workStatus.goal.status.dropped',
};

/** Parses the raw omp goal payload at the boundary (spec 08 GAP-05). */
const parseOmpGoal = (value: unknown): { objective: string; status: string } | null => {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (typeof record.objective !== 'string' || !record.objective.trim()) return null;
  if (typeof record.status !== 'string' || !(record.status in OMP_GOAL_STATUS_LABEL_KEY)) return null;
  return { objective: record.objective, status: record.status };
}

/** The session goal, on the mapping every other goal surface uses. */
export const WorkStatusGoalRow: React.FC<Props> = ({ sessionId, directory }) => {
  const { t } = useI18n();
  // omp projection (spec 08 GAP-05): under modes.v1 the row reads the
  // engine's goal state (omp.goal.updated). No write affordances — the bridge
  // exposes no goal create/update/clear endpoints yet — so the row is
  // read-only; the legacy OC goal row below is the exact off-capability path.
  const modesEnabled = useOmpFeatureEnabled('modes.v1');
  const ompGoalState = useOmpGoalState(directory ?? '', sessionId ?? undefined);
  const ompGoal = modesEnabled ? parseOmpGoal(ompGoalState?.goal) : null;

  const { goal, enabled } = useSessionGoal(sessionId ?? '', directory ?? undefined);
  const [dialogOpen, setDialogOpen] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const handleToggleStatus = React.useCallback(async (nextStatus: 'active' | 'paused') => {
    if (!sessionId || busy) return;
    setBusy(true);
    try {
      await setSessionGoalStatus(sessionId, directory ?? undefined, nextStatus);
    } catch {
      toast.error(t('chat.workStatus.goal.updateFailed'));
    } finally {
      setBusy(false);
    }
  }, [busy, directory, sessionId, t]);

  if (ompGoal && sessionId) {
    return (
      <WorkStatusRow
        leading={(
          <Icon
            name="target-fill"
            className="size-4 shrink-0"
            style={{ color: OMP_GOAL_STATUS_COLOR[ompGoal.status] }}
          />
        )}
        label={ompGoal.objective.trim()}
        ariaLabel={ompGoal.objective.trim()}
        value={(
          <WorkStatusValue tone={ompGoal.status === 'complete' ? 'success' : 'muted'}>
            {t(OMP_GOAL_STATUS_LABEL_KEY[ompGoal.status])}
          </WorkStatusValue>
        )}
      />
    );
  }

  const objective = enabled && goal ? goal.objective?.trim() || null : null;
  if (!objective || !sessionId) return null;

  // No control while complete: there is nothing left to pause or resume.
  const canPause = goal?.status === 'active';
  const canResume = goal?.status === 'paused'
    || goal?.status === 'blocked'
    || goal?.status === 'budgetLimited';

  return (
    <>
      <WorkStatusRow
        leading={(
          <Icon
            name={goal?.status ? 'target-fill' : 'target'}
            className="size-4 shrink-0"
            style={{ color: goal ? sessionGoalStatusColor[goal.status] : undefined }}
          />
        )}
        label={objective}
        onClick={() => setDialogOpen(true)}
        ariaLabel={t('chat.workStatus.goal.open')}
        value={canPause || canResume ? (
          <WorkStatusRowAction
            tone={canPause ? 'info' : 'warning'}
            disabled={busy}
            ariaLabel={canPause ? t('chat.workStatus.goal.pause') : t('chat.workStatus.goal.resume')}
            onClick={() => { void handleToggleStatus(canPause ? 'paused' : 'active'); }}
          >
            {canPause ? t('chat.workStatus.goal.pause') : t('chat.workStatus.goal.resume')}
          </WorkStatusRowAction>
        ) : undefined}
      />
      <SessionGoalDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        sessionId={sessionId}
        directory={directory ?? undefined}
      />
    </>
  );
};
