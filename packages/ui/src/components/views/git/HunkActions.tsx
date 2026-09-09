import React, { useMemo } from 'react';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { splitPatchIntoHunks } from '@/lib/diff/patchFileDiff';

export type HunkDiffAction = 'stage' | 'unstage' | 'discard';

export type HunkBusyState = {
  index: number;
  action: HunkDiffAction;
} | null;

interface HunkActionsProps {
  filePath: string;
  patch: string;
  staged: boolean;
  busyHunk: HunkBusyState;
  disabled: boolean;
  onAction: (hunkIndex: number, action: HunkDiffAction) => void;
}

interface HunkSummary {
  patch: string;
  insertions: number;
  deletions: number;
}

const summarizeHunks = (patch: string): HunkSummary[] =>
  splitPatchIntoHunks(patch).map((hunkPatch) => {
    let insertions = 0;
    let deletions = 0;
    for (const line of hunkPatch.split('\n')) {
      if (line.startsWith('+++') || line.startsWith('---')) continue;
      if (line.startsWith('+')) insertions += 1;
      else if (line.startsWith('-')) deletions += 1;
    }
    return { patch: hunkPatch, insertions, deletions };
  });

export const HunkActions = React.memo<HunkActionsProps>(function HunkActions({
  filePath,
  patch,
  staged,
  busyHunk,
  disabled,
  onAction,
}) {
  const { t } = useI18n();
  const hunks = useMemo(() => summarizeHunks(patch), [patch]);

  if (hunks.length < 2) {
    return null;
  }

  const primaryAction: HunkDiffAction = staged ? 'unstage' : 'stage';

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          className="flex h-6 shrink-0 items-center gap-1 rounded-full border border-[var(--interactive-border)]/45 bg-[var(--surface-background)]/95 px-2 typography-micro font-semibold text-muted-foreground shadow-sm backdrop-blur-md hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--interactive-focus-ring)] disabled:cursor-not-allowed disabled:opacity-50"
          aria-label={t('diffView.hunk.label')}
          title={t('diffView.hunk.label')}
        >
          <Icon name="stack" className="size-3.5" />
          <span>{t('diffView.hunk.label')} · {hunks.length}</span>
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" sideOffset={6} className="w-64">
        <DropdownMenuLabel className="max-w-full truncate" title={filePath}>
          {t('diffView.hunk.label')} · {filePath}
        </DropdownMenuLabel>
        <DropdownMenuSeparator />
        {hunks.map((hunk, index) => {
          const displayIndex = index + 1;
          const busyPrimary = busyHunk?.index === index && busyHunk.action === primaryAction;
          const busyDiscard = busyHunk?.index === index && busyHunk.action === 'discard';
          const rowBusy = busyPrimary || busyDiscard;
          const primaryTitle = staged
            ? t('diffView.hunk.unstageTitle', { index: displayIndex })
            : t('diffView.hunk.stageTitle', { index: displayIndex });
          const discardTitle = t('diffView.hunk.discardTitle', { index: displayIndex });
          return (
            <div
              key={index}
              className="flex items-center gap-2 px-2 py-1.5"
              aria-label={primaryTitle}
            >
              <span className="min-w-0 flex-1 truncate typography-ui-label text-foreground">
                Hunk {displayIndex}
                <span className="ml-1.5 typography-micro">
                  {hunk.insertions > 0 ? (
                    <span style={{ color: 'var(--status-success)' }}>+{hunk.insertions}</span>
                  ) : null}
                  {hunk.insertions > 0 && hunk.deletions > 0 ? (
                    <span className="mx-0.5 text-muted-foreground">/</span>
                  ) : null}
                  {hunk.deletions > 0 ? (
                    <span style={{ color: 'var(--status-error)' }}>-{hunk.deletions}</span>
                  ) : null}
                </span>
              </span>
              <button
                type="button"
                disabled={disabled || rowBusy}
                onClick={() => onAction(index, primaryAction)}
                className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary disabled:cursor-not-allowed disabled:opacity-50"
                aria-label={primaryTitle}
                title={primaryTitle}
              >
                {busyPrimary ? (
                  <Icon name="loader-4" className="size-3.5 animate-spin" />
                ) : (
                  <Icon name="add" className="size-3.5" />
                )}
              </button>
              {!staged ? (
                <button
                  type="button"
                  disabled={disabled || rowBusy}
                  onClick={() => onAction(index, 'discard')}
                  className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary disabled:cursor-not-allowed disabled:opacity-50"
                  aria-label={discardTitle}
                  title={discardTitle}
                >
                  {busyDiscard ? (
                    <Icon name="loader-4" className="size-3.5 animate-spin" />
                  ) : (
                    <Icon name="arrow-go-back" className="size-3.5" />
                  )}
                </button>
              ) : null}
            </div>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
});
