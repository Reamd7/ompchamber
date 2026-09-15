import React from 'react';
import { useI18n } from '@/lib/i18n';
import type { I18nKey } from '@/lib/i18n/messages/en';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import type { OmpProcessEntry, OmpProcessMember, OmpProcessOutput } from '@/lib/api/omp';
import { useOmpProcessesStore } from '@/stores/useOmpProcessesStore';
import { formatProcessBytes, formatProcessCpu, processStatusLabelKey } from './WorkStatusProcessesSection';

type Props = {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  directory: string | null;
  entry: OmpProcessEntry;
};

const OUTPUT_POLL_MS = 2000;

const memberStateLabel = (member: OmpProcessMember, t: (key: I18nKey) => string): string =>
  member.killedByUser
    ? t('chat.workStatus.processes.killed')
    : member.state === 'running'
      ? t('chat.workStatus.processes.running')
      : t('chat.workStatus.processes.exited');

/**
 * Member table, output tail, and kill controls for one processes entry —
 * shared by the single-entry dialog and the all-processes panel. Each mounted
 * instance keeps its own confirm-arming and polls the bounded output tail
 * while it is on screen; unmount stops the polling.
 */
export const ProcessEntryDetail: React.FC<{ directory: string | null; entry: OmpProcessEntry }> = ({
  directory,
  entry,
}) => {
  const { t } = useI18n();
  const fetchOutput = useOmpProcessesStore((s) => s.output);
  const kill = useOmpProcessesStore((s) => s.kill);
  const [output, setOutput] = React.useState<OmpProcessOutput | null>(null);
  const [armed, setArmed] = React.useState(false);
  const [armedPid, setArmedPid] = React.useState<number | null>(null);
  React.useEffect(() => {
    if (!armed) return undefined;
    const timer = window.setTimeout(() => setArmed(false), 3000);
    return () => window.clearTimeout(timer);
  }, [armed]);
  React.useEffect(() => {
    if (armedPid === null) return undefined;
    const timer = window.setTimeout(() => setArmedPid(null), 3000);
    return () => window.clearTimeout(timer);
  }, [armedPid]);

  React.useEffect(() => {
    if (!entry.hasOutput) return undefined;
    let cancelled = false;
    const refresh = () => {
      void fetchOutput(directory, entry.key).then((next) => {
        if (!cancelled && next) setOutput(next);
      });
    };
    refresh();
    const timer = window.setInterval(refresh, OUTPUT_POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [entry.key, entry.hasOutput, directory, fetchOutput]);

  const killAll = (): void => {
    if (!armed) {
      setArmed(true);
      return;
    }
    setArmed(false);
    void kill(directory, entry);
  };

  const killMember = (member: OmpProcessMember): void => {
    if (armedPid !== member.pid) {
      setArmedPid(member.pid);
      return;
    }
    setArmedPid(null);
    void kill(directory, entry, member.pid);
  };

  const sorted = React.useMemo(
    () => [...entry.processes].sort((a, b) => (a.state === b.state ? a.pid - b.pid : a.state === 'running' ? -1 : 1)),
    [entry.processes],
  );

  return (
    <>
      <div className="flex flex-col gap-1">
        <div className="grid grid-cols-[4.5rem_minmax(0,1fr)_4.5rem_4.5rem_5rem] gap-2 px-1 text-[11px] font-medium text-muted-foreground">
          <span>PID</span>
          <span>{t('chat.workStatus.processes.command')}</span>
          <span className="text-right">{t('chat.workStatus.processes.cpu')}</span>
          <span className="text-right">{t('chat.workStatus.processes.memory')}</span>
          <span />
        </div>
        <div className="max-h-48 overflow-y-auto">
          {sorted.map((member) => (
            <div
              key={member.pid}
              className="grid grid-cols-[4.5rem_minmax(0,1fr)_4.5rem_4.5rem_5rem] items-center gap-2 rounded px-1 py-0.5 text-[12px] tabular-nums"
            >
              <span className="text-muted-foreground">{member.pid}</span>
              <span className="min-w-0 break-all font-mono" title={member.argv}>{member.argv}</span>
              <span className="text-right text-muted-foreground">
                {member.cpuPercent !== undefined ? formatProcessCpu(member.cpuPercent) : '—'}
              </span>
              <span className="text-right text-muted-foreground">
                {member.rssBytes !== undefined ? formatProcessBytes(member.rssBytes) : '—'}
              </span>
              <span className="flex justify-end">
                {member.state === 'running' ? (
                  <button
                    type="button"
                    onClick={() => killMember(member)}
                    className={cn(
                      'rounded-full px-1.5 py-px text-[11px] font-medium leading-4 transition-opacity hover:opacity-80',
                      'text-[var(--status-error)]',
                    )}
                    style={{ backgroundColor: 'color-mix(in srgb, var(--status-error) 18%, transparent)' }}
                  >
                    {armedPid === member.pid
                      ? t('chat.workStatus.processes.killConfirm')
                      : t('chat.workStatus.processes.kill')}
                  </button>
                ) : (
                  <span className="text-[11px] text-muted-foreground">{memberStateLabel(member, t)}</span>
                )}
              </span>
            </div>
          ))}
        </div>
      </div>

      {entry.hasOutput ? (
        <div className="flex min-h-0 flex-col">
          <span className="mb-1 text-[11px] font-medium text-muted-foreground">
            {t('chat.workStatus.processes.output')}
            {output?.truncated ? '…' : ''}
          </span>
          <pre className="max-h-48 overflow-auto rounded-md bg-[var(--surface-muted)] p-2 text-[11px] leading-4 whitespace-pre-wrap break-all">
            {output?.output ?? ''}
          </pre>
        </div>
      ) : null}

      <div className="flex items-center justify-between border-t border-[var(--interactive-border)] pt-3">
        <span className="text-xs text-muted-foreground">
          {entry.liveCount > 0
            ? t('chat.workStatus.processes.liveCount', { count: entry.liveCount })
            : t(processStatusLabelKey[entry.status])}
        </span>
        {/* No whole-subtree kill on unattributed roots — they own foreign
            infrastructure; per-pid kills above stay available. */}
        {entry.liveCount > 0 && entry.sessionID !== null ? (
          <Button variant="destructive" size="xs" onClick={killAll}>
            {armed ? t('chat.workStatus.processes.killConfirm') : t('chat.workStatus.processes.killAll')}
          </Button>
        ) : null}
      </div>
    </>
  );
};

/**
 * Detail view for one processes entry: every tracked member pid with its
 * resource numbers, the invocation's bounded output tail, and kill controls.
 */
export const WorkStatusProcessDialog: React.FC<Props> = ({ open, onOpenChange, directory, entry }) => {
  const { t } = useI18n();
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle className="break-all font-mono text-sm">{entry.command}</DialogTitle>
          <DialogDescription>
            {t(processStatusLabelKey[entry.status])}
            {entry.sessionID === null ? ` · ${t('chat.workStatus.processes.uncertain')}` : ''}
            {entry.statsStale ? ` · ${t('chat.workStatus.processes.statsStale')}` : ''}
          </DialogDescription>
        </DialogHeader>
        <ProcessEntryDetail directory={directory} entry={entry} />
      </DialogContent>
    </Dialog>
  );
};
