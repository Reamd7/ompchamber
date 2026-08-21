/**
 * OmpGoalIndicator — the session goal projection inside the mode menu
 * (spec 02 §5.6 P1 / GAP-B08; TUI parity: status-line segments.ts
 * renderGoalMode — icon + status color + used/budget).
 *
 * Read-only by design: the modes bridge exposes no goal create/update/clear
 * endpoints yet (domain-modes.js registers none), so P1 is a pure projection
 * of `omp.goal.updated` / the mode snapshot's `goal` field. Renders nothing
 * when the session has no goal.
 */

import { Icon } from '@/components/icon/Icon';
import type { IconName } from '@/components/icon/icons';

import { useI18n } from '@/lib/i18n';
import { useOmpSessionGoal } from '@/hooks/useOmpSessionGoal';
import { cn } from '@/lib/utils';

/** Compact `12.3k`-style token counts (TUI formatGoalBudget parity). */
const formatTokenCount = (value: number): string =>
  value >= 1_000_000
    ? `${(value / 1_000_000).toFixed(1)}M`
    : value >= 1_000
      ? `${(value / 1_000).toFixed(1)}k`
      : String(Math.round(value));

interface GoalStatusVisual {
  icon: IconName;
  className: string;
}

/** TUI renderGoalMode colors: paused/budget-limited → warning, complete → success, dropped → dim. */
const visualForStatus = (status: string | undefined): GoalStatusVisual => {
  switch (status) {
    case 'paused': return { icon: 'pause', className: 'text-status-warning' };
    case 'complete': return { icon: 'check', className: 'text-status-success' };
    case 'budget-limited': return { icon: 'error-warning', className: 'text-status-warning' };
    case 'dropped': return { icon: 'close', className: 'text-muted-foreground' };
    default: return { icon: 'target-fill', className: 'text-primary' };
  }
};

export const OmpGoalSection: React.FC<{
  directory: string | null;
  sessionID: string | null;
  modesEnabled: boolean;
}> = ({ directory, sessionID, modesEnabled }) => {
  const { t } = useI18n();
  const goal = useOmpSessionGoal(directory, sessionID, modesEnabled);
  if (goal === null || modesEnabled !== true) return null;

  const status = goal.status;
  const visual = visualForStatus(status);
  const statusLabel = status === 'paused'
    ? t('chat.goalIndicator.status.paused')
    : status === 'complete'
      ? t('chat.goalIndicator.status.complete')
      : status === 'budget-limited'
        ? t('chat.goalIndicator.status.budgetLimited')
        : status === 'dropped'
          ? t('chat.goalIndicator.status.dropped')
          : t('chat.goalIndicator.status.active');
  const objective = goal.objective?.trim() || t('chat.goalIndicator.noObjective');
  const budget = goal.tokenBudget !== undefined && goal.tokensUsed !== undefined
    ? t('chat.goalIndicator.budget', {
        used: formatTokenCount(goal.tokensUsed),
        budget: formatTokenCount(goal.tokenBudget),
      })
    : goal.tokensUsed !== undefined
      ? t('chat.goalIndicator.tokensUsed', { used: formatTokenCount(goal.tokensUsed) })
      : null;

  return (
    <div className="border-t border-border/40 pt-1" data-testid="omp-goal-section">
      <div className="typography-ui-header px-3 pt-1 pb-1 font-semibold text-foreground">
        {t('chat.goalIndicator.title')}
      </div>
      <div className="flex items-start gap-2 px-3 pb-2 pt-0.5">
        <Icon name={visual.icon} className={cn('mt-0.5 size-4 flex-shrink-0', visual.className)} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="typography-meta font-medium text-foreground">{statusLabel}</span>
            {budget ? (
              <span className="typography-meta flex-shrink-0 text-muted-foreground">{budget}</span>
            ) : null}
          </div>
          <p className="typography-meta mt-0.5 line-clamp-2 break-words text-muted-foreground" title={objective}>
            {objective}
          </p>
        </div>
      </div>
    </div>
  );
};
