/**
 * StrippedToolCallsLine — elided-activity line for an assistant message whose
 * branch/fork history rewrite removed unpaired tool calls (wire
 * `info.metadata.ompStrippedToolCalls`, TUI StrippedToolCallsPlaceholder:
 * "N tool calls elided — no result on this branch").
 */
import React from 'react';
import { useI18n } from '@/lib/i18n';

export const StrippedToolCallsLine: React.FC<{ count: number }> = ({ count }) => {
  const { t } = useI18n();
  if (!Number.isFinite(count) || count <= 0) return null;
  const label = count === 1 ? t('chat.omp.strippedToolCalls.one', { count }) : t('chat.omp.strippedToolCalls.many', { count });
  return (
    <p data-omp-stripped-tool-calls={count} className="my-1 px-1 text-xs italic text-muted-foreground/60">
      {label}
    </p>
  );
};
