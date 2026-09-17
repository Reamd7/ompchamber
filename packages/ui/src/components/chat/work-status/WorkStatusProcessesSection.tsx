import React from 'react';
import { useI18n } from '@/lib/i18n';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import {
  useOmpProcessesForDirectory,
  useOmpProcessesRevision,
  useOmpProcessesStore,
} from '@/stores/useOmpProcessesStore';
import type { OmpProcessEntry } from '@/lib/api/omp';
import { formatProcessBytes, formatProcessCpu, processStatusLabelKey } from './processFormatting';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import {
  WorkStatusCollapsibleSection,
  WorkStatusPill,
  WorkStatusRow,
  WorkStatusRowAction,
  WorkStatusValue,
} from './WorkStatusPrimitives';
import { useReportWorkStatusPresence } from './presenceContext';
import { WorkStatusProcessDialog } from './WorkStatusProcessDialog';
import { WorkStatusAllProcessesDialog } from './WorkStatusAllProcessesDialog';

type Props = {
  sessionId: string | null;
  directory: string | null;
};
const SECTION_ID = 'processes';
const statusTone = (status: OmpProcessEntry['status']): 'default' | 'muted' | 'error' | 'info' => {
  if (status === 'running') return 'info';
  if (status === 'failed') return 'error';
  return 'muted';
};


/**
 * Processes the session's bash/eval calls produced — including the ones that
 * never got reaped (`&`, nohup, detached descendants). The ledger's ownership
 * is observational, so every row says how it was attributed and ambiguous
 * rows stay listed with an "uncertain" mark rather than being dropped.
 *
 * `processes.v1` gates the whole surface: with the capability off (external
 * runtime, old engine, native module unavailable) the section hides.
 */
export const WorkStatusProcessesSection: React.FC<Props> = ({ sessionId, directory }) => {
  const { t } = useI18n();
  const enabled = useOmpFeatureEnabled('processes.v1');
  const entries = useOmpProcessesForDirectory(directory);
  const snapshotRevision = useOmpProcessesRevision(directory);
  const load = useOmpProcessesStore((s) => s.load);
  const kill = useOmpProcessesStore((s) => s.kill);
  const eventRevision = useOmpSessionStore(
    React.useCallback(
      (state) => (directory ? state.directories[directory]?.domains.processesRevision : undefined),
      [directory],
    ),
  );
  React.useEffect(() => {
    void load(directory, {
      force: eventRevision !== undefined && snapshotRevision !== undefined && eventRevision > snapshotRevision,
    });
  }, [load, directory, eventRevision, snapshotRevision]);

  const rows = React.useMemo(
    () => (enabled && sessionId && entries
      ? entries.filter((entry) =>
          entry.sessionID === sessionId || entry.candidateSessionIds?.includes(sessionId))
      : []),
    [enabled, entries, sessionId],
  );
  // Sidebar shows only what is still alive; ended rows linger in the ledger
  // for postmortem and remain reachable through the all-processes dialog.
  const runningRows = React.useMemo(
    () => rows.filter((entry) => entry.status === 'running'),
    [rows],
  );

  const [detailEntry, setDetailEntry] = React.useState<OmpProcessEntry | null>(null);
  const [allOpen, setAllOpen] = React.useState(false);
  const [armedKey, setArmedKey] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!armedKey) return undefined;
    const timer = window.setTimeout(() => setArmedKey(null), 3000);
    return () => window.clearTimeout(timer);
  }, [armedKey]);
  // The dialog tracks the live entry by key so a refresh updates in place.
  const detailLive = React.useMemo(
    () => (detailEntry ? rows.find((entry) => entry.key === detailEntry.key) ?? null : null),
    [detailEntry, rows],
  );

  useReportWorkStatusPresence(SECTION_ID, runningRows.length > 0);
  if (!enabled || runningRows.length === 0) return null;

  const liveTotal = rows.reduce((sum, entry) => sum + entry.liveCount, 0);

  const killEntry = (entry: OmpProcessEntry): void => {
    if (armedKey !== entry.key) {
      setArmedKey(entry.key);
      return;
    }
    setArmedKey(null);
    void kill(directory, entry);
  };

  return (
    <WorkStatusCollapsibleSection
      id={SECTION_ID}
      title={t('chat.workStatus.section.processes')}
      icon="pulse"
      defaultExpanded
      summary={liveTotal}
      action={(
        <Button
          size="icon"
          variant="ghost"
          className="size-6 shrink-0 text-muted-foreground"
          onClick={() => setAllOpen(true)}
          aria-label={t('chat.workStatus.processes.viewAll')}
          title={t('chat.workStatus.processes.viewAll')}
        >
          <Icon name="list-unordered" className="size-3.5" />
        </Button>
      )}
    >
      <div className="max-h-56 overflow-y-auto">
        {runningRows.map((entry) => (
          <WorkStatusRow
            key={entry.key}
            onClick={() => setDetailEntry(entry)}
            ariaLabel={t('chat.workStatus.processes.view', { command: entry.command })}
            label={<span title={entry.command}>{entry.command}</span>}
            value={(
              <>
                {entry.sessionID === null ? (
                  <WorkStatusPill>{t('chat.workStatus.processes.uncertain')}</WorkStatusPill>
                ) : null}
                {entry.totalCpuPercent !== undefined ? (
                  <WorkStatusValue tone="muted">{formatProcessCpu(entry.totalCpuPercent)}</WorkStatusValue>
                ) : null}
                {entry.totalRssBytes !== undefined ? (
                  <WorkStatusValue tone="muted">{formatProcessBytes(entry.totalRssBytes)}</WorkStatusValue>
                ) : null}
                <WorkStatusValue tone={statusTone(entry.status)}>
                  {t(processStatusLabelKey[entry.status])}
                </WorkStatusValue>
                {entry.liveCount > 0 ? (
                  <WorkStatusRowAction
                    tone="error"
                    ariaLabel={t('chat.workStatus.processes.killAll')}
                    onClick={() => killEntry(entry)}
                  >
                    {armedKey === entry.key
                      ? t('chat.workStatus.processes.killConfirm')
                      : t('chat.workStatus.processes.kill')}
                  </WorkStatusRowAction>
                ) : null}
              </>
            )}
          />
        ))}
      </div>
      {detailEntry ? (
        <WorkStatusProcessDialog
          open
          onOpenChange={(open) => { if (!open) setDetailEntry(null); }}
          directory={directory}
          entry={detailLive ?? detailEntry}
        />
      ) : null}
      <WorkStatusAllProcessesDialog
        open={allOpen}
        onOpenChange={setAllOpen}
        directory={directory}
        entries={rows}
      />
    </WorkStatusCollapsibleSection>
  );
};
