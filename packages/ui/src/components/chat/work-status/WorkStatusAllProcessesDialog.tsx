import React from 'react';
import { useI18n } from '@/lib/i18n';
import { Icon } from '@/components/icon/Icon';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import type { OmpProcessEntry } from '@/lib/api/omp';
import { ProcessEntryDetail } from './WorkStatusProcessDialog';
import { processStatusLabelKey } from './WorkStatusProcessesSection';
import { WorkStatusPill, WorkStatusValue } from './WorkStatusPrimitives';

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  directory: string | null;
  /** Entries already filtered to the session's view — the panel shows them all. */
  entries: OmpProcessEntry[];
};

/**
 * The whole ledger for this session at once: every invocation and every
 * unattributed subtree, each with its full member table and kill controls.
 * The sidebar rows stay glanceable; this is where "what is actually running"
 * gets answered when several calls overlap.
 */
export const WorkStatusAllProcessesDialog: React.FC<Props> = ({ open, onOpenChange, directory, entries }) => {
  const { t } = useI18n();
  const [showEnded, setShowEnded] = React.useState(false);
  const liveTotal = entries.reduce((sum, entry) => sum + entry.liveCount, 0);
  // Finished entries stay listed for the retention window (postmortem output
  // and exit codes), but they fold behind a toggle so dead rows never crowd
  // out what is actually running.
  const running = entries.filter((entry) => entry.status === 'running');
  const ended = entries.filter((entry) => entry.status !== 'running');
  const renderEntry = (entry: OmpProcessEntry) => (
    <div key={entry.key} className="flex min-w-0 flex-col gap-2">
      <div className="flex items-start gap-2 px-1">
        <span className="min-w-0 flex-1 break-all font-mono text-xs" title={entry.command}>
          {entry.command}
        </span>
        {entry.sessionID === null ? (
          <WorkStatusPill>{t('chat.workStatus.processes.uncertain')}</WorkStatusPill>
        ) : null}
        <WorkStatusValue tone={entry.status === 'running' ? 'info' : entry.status === 'failed' ? 'error' : 'muted'}>
          {t(processStatusLabelKey[entry.status])}
        </WorkStatusValue>
      </div>
      <ProcessEntryDetail directory={directory} entry={entry} />
    </div>
  );
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-3xl">
        <DialogHeader>
          <DialogTitle>{t('chat.workStatus.section.processes')}</DialogTitle>
          <DialogDescription>
            {t('chat.workStatus.processes.liveCount', { count: liveTotal })}
          </DialogDescription>
        </DialogHeader>
        <div className="flex max-h-[70vh] min-h-0 flex-col gap-3 overflow-y-auto">
          {running.map(renderEntry)}
          {ended.length > 0 ? (
            <>
              <button
                type="button"
                aria-expanded={showEnded}
                onClick={() => setShowEnded((v) => !v)}
                className="flex items-center gap-1 rounded-md px-1 text-left text-xs font-medium text-muted-foreground transition-colors hover:text-foreground"
              >
                <Icon name={showEnded ? 'arrow-down-s' : 'arrow-right-s'} className="size-3.5 shrink-0" />
                {t('chat.workStatus.processes.endedCount', { count: ended.length })}
              </button>
              {showEnded ? ended.map(renderEntry) : null}
            </>
          ) : null}
        </div>
      </DialogContent>
    </Dialog>
  );
};
