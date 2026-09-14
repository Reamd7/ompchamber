import React from 'react';
import {
  createOmpDiagnosticsAPI,
  OMP_ENDPOINTS,
  type OmpDiagnostics,
  type OmpDiagnosticsSessionRow,
} from '@/lib/api/omp';
import { runtimeFetch } from '@/lib/runtime-fetch';
import { toast } from '@/components/ui';
import { Button } from '@/components/ui/button';
import { useI18n, type I18nKey } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useGlobalSessionsStore } from '@/stores/useGlobalSessionsStore';

const REFRESH_INTERVAL_MS = 5_000;
const LARGE_SESSION_BYTES = 10 * 1024 * 1024;

type SnapshotState =
  | { kind: 'loading' }
  | { kind: 'unavailable' }
  | { kind: 'error' }
  | { kind: 'ok'; data: OmpDiagnostics };

type MonitorTab = 'overview' | 'sessions';

const formatBytes = (bytes: number | null | undefined): string => {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB'];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (value >= 1024 && unit < units.length - 1);
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
};

const formatIdle = (idleMs: number): string => {
  if (!Number.isFinite(idleMs) || idleMs < 1000) return '0s';
  const minutes = Math.floor(idleMs / 60_000);
  if (minutes >= 60) return `${Math.floor(minutes / 60)}h${minutes % 60}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${Math.floor(idleMs / 1000)}s`;
};

const shortDirectory = (directory: string): string => {
  const normalized = directory.replace(/\\/g, '/').replace(/\/+$/, '');
  const tail = normalized.split('/').filter(Boolean);
  return tail.slice(-2).join('/') || directory;
};

const STATE_TONE = {
  live: 'bg-primary',
  materializing: 'bg-status-info',
  evicting: 'bg-status-warning',
  failed: 'bg-destructive',
} satisfies Record<OmpDiagnosticsSessionRow['state'], string>;

const STATE_LABEL_KEY = {
  live: 'openCodeStatusDialog.monitor.stateLive',
  materializing: 'openCodeStatusDialog.monitor.stateMaterializing',
  evicting: 'openCodeStatusDialog.monitor.stateEvicting',
  failed: 'openCodeStatusDialog.monitor.stateFailed',
} satisfies Record<OmpDiagnosticsSessionRow['state'], I18nKey>;

const CategoryBar: React.FC<{
  segments: Array<{ key: string; label: string; bytes: number; className: string }>;
  total: number;
}> = ({ segments, total }) => (
  <div>
    <div className="flex h-2 w-full overflow-hidden rounded-full bg-surface-muted">
      {segments.map((segment) => (
        <div
          key={segment.key}
          className={cn('h-full', segment.className)}
          style={{ width: `${total > 0 ? Math.max(0, Math.min(100, (segment.bytes / total) * 100)) : 0}%` }}
        />
      ))}
    </div>
    <div className="mt-1.5 flex flex-wrap gap-x-4 gap-y-0.5 typography-meta text-muted-foreground">
      {segments.map((segment) => (
        <span key={segment.key} className="inline-flex items-center gap-1.5">
          <span className={cn('inline-block h-2 w-2 rounded-sm', segment.className)} />
          {segment.label} · {formatBytes(segment.bytes)}
        </span>
      ))}
    </div>
  </div>
);

const MetricCard: React.FC<{ label: string; value: string; hint?: string }> = ({ label, value, hint }) => (
  <div className="rounded-md bg-surface-muted/60 p-2.5">
    <div className="typography-meta text-muted-foreground">{label}</div>
    <div className="typography-markdown font-semibold text-foreground tabular-nums">{value}</div>
    {hint ? <div className="typography-meta text-muted-foreground/80">{hint}</div> : null}
  </div>
);

/**
 * Trae-style engine resource monitor for the Engine Status dialog. Polls
 * `/api/omp/diagnostics` while mounted — the dialog only mounts this section
 * when open, so a closed dialog spends nothing. Per-session sizes are the
 * host's transcript-size proxy (declared estimate, ~1.3× resident heap),
 * never a measured attribution.
 */
export const EngineMemorySection: React.FC = () => {
  const { t } = useI18n();
  const [tab, setTab] = React.useState<MonitorTab>('overview');
  const [snapshot, setSnapshot] = React.useState<SnapshotState>({ kind: 'loading' });
  const [largeOnly, setLargeOnly] = React.useState(false);
  const [releasing, setReleasing] = React.useState<string | null>(null);
  const activeSessions = useGlobalSessionsStore((state) => state.activeSessions);
  const sessionTitles = React.useMemo(() => {
    const map = new Map<string, string>();
    for (const session of activeSessions) {
      if (session.title) map.set(session.id, session.title);
    }
    return map;
  }, [activeSessions]);

  const apiRef = React.useRef(createOmpDiagnosticsAPI());
  const load = React.useCallback(async () => {
    const result = await apiRef.current.get();
    setSnapshot((prev) =>
      result.ok
        ? { kind: 'ok', data: result.data }
        : result.unavailable
          ? { kind: 'unavailable' }
          : // A transient fetch failure keeps the last good snapshot visible —
            // diagnostics are never authoritative-empty.
            prev.kind === 'ok'
            ? prev
            : { kind: 'error' },
    );
  }, []);

  React.useEffect(() => {
    void load();
    const interval = setInterval(() => void load(), REFRESH_INTERVAL_MS);
    return () => clearInterval(interval);
  }, [load]);

  const release = React.useCallback(
    async (row: OmpDiagnosticsSessionRow) => {
      setReleasing(row.id);
      try {
        const response = await runtimeFetch(OMP_ENDPOINTS.sessionRelease(row.id), {
          method: 'POST',
          query: { directory: row.directory },
        });
        if (response.status === 409) {
          toast.error(t('openCodeStatusDialog.monitor.releaseBusy'));
        } else if (!response.ok) {
          toast.error(t('openCodeStatusDialog.monitor.releaseFailed'));
        }
        // Success flips the row via the post-dispose session.updated; the
        // next poll reconciles regardless.
      } catch {
        toast.error(t('openCodeStatusDialog.monitor.releaseFailed'));
      } finally {
        setReleasing(null);
        void load();
      }
    },
    [load, t],
  );

  const data = snapshot.kind === 'ok' ? snapshot.data : null;
  const sessions = data?.dataProportional.liveSessions;
  const rows = (sessions?.sessions ?? [])
    .slice()
    .filter((row) => !largeOnly || (row.transcriptBytes ?? 0) >= LARGE_SESSION_BYTES)
    .sort((a, b) => (b.transcriptBytes ?? -1) - (a.transcriptBytes ?? -1));

  const residentBytes = sessions?.sessions.reduce((sum, row) => sum + (row.transcriptBytes ?? 0), 0) ?? 0;
  const bufferBytes = (data?.wireBus.retainedBytes ?? 0) + (data?.ompBus.retainedBytes ?? 0);
  const heap = data?.process.heapUsedBytes ?? 0;
  const unattributed = Math.max(0, heap - residentBytes - bufferBytes);

  return (
    <section className="@container rounded-lg border border-border/60 bg-surface-muted/40 text-xs">
      <div className="flex items-center justify-between border-b border-border/60 px-3 py-2">
        <div className="flex items-center gap-1">
          {(['overview', 'sessions'] as const).map((key) => (
            <button
              key={key}
              type="button"
              onClick={() => setTab(key)}
              className={cn(
                'rounded-md px-2.5 py-1 typography-ui-label transition-colors',
                tab === key ? 'bg-surface-elevated text-foreground' : 'text-muted-foreground hover:text-foreground',
              )}
            >
              {t(key === 'overview' ? 'openCodeStatusDialog.monitor.tabOverview' : 'openCodeStatusDialog.monitor.tabSessions')}
            </button>
          ))}
        </div>
        <span className="text-muted-foreground">{t('openCodeStatusDialog.memory.refreshNote')}</span>
      </div>

      <div className="p-3">
        {snapshot.kind === 'loading' ? (
          <p className="text-muted-foreground">{t('openCodeStatusDialog.memory.loading')}</p>
        ) : snapshot.kind === 'unavailable' ? (
          <p className="text-muted-foreground">{t('openCodeStatusDialog.memory.unavailable')}</p>
        ) : snapshot.kind === 'error' ? (
          <p className="text-muted-foreground">{t('openCodeStatusDialog.memory.error')}</p>
        ) : data ? (
          tab === 'overview' ? (
            <div className="flex flex-col gap-3">
              <div className="grid grid-cols-2 gap-2 @lg:grid-cols-4">
                <MetricCard label={t('openCodeStatusDialog.memory.processHeap')} value={formatBytes(data.process.heapUsedBytes)} />
                <MetricCard label={t('openCodeStatusDialog.memory.processRss')} value={formatBytes(data.process.rssBytes)} />
                <MetricCard
                  label={t('openCodeStatusDialog.memory.residentEstimate')}
                  value={`~${formatBytes(residentBytes)}`}
                  hint={t('openCodeStatusDialog.monitor.residentCount', { total: sessions?.total ?? 0 })}
                />
                <MetricCard
                  label={t('openCodeStatusDialog.memory.processExternal')}
                  value={formatBytes(data.process.externalBytes + data.process.arrayBufferBytes)}
                />
              </div>

              <CategoryBar
                total={heap}
                segments={[
                  {
                    key: 'sessions',
                    label: t('openCodeStatusDialog.monitor.catSessions'),
                    bytes: residentBytes,
                    className: 'bg-primary',
                  },
                  {
                    key: 'buffers',
                    label: t('openCodeStatusDialog.monitor.catBuffers'),
                    bytes: bufferBytes,
                    className: 'bg-status-info',
                  },
                  {
                    key: 'unattributed',
                    label: t('openCodeStatusDialog.monitor.catUnattributed'),
                    bytes: unattributed,
                    className: 'bg-muted-foreground/40',
                  },
                ]}
              />

              <div className="grid grid-cols-1 @sm:grid-cols-2 gap-x-6 gap-y-1 text-muted-foreground">
                <div className="flex items-center justify-between gap-3">
                  <span>{t('openCodeStatusDialog.memory.wireBus')}</span>
                  <span className="tabular-nums">
                    {t('openCodeStatusDialog.memory.busValue', {
                      entries: data.wireBus.retainedEntries,
                      size: formatBytes(data.wireBus.retainedBytes),
                      capacity: data.wireBus.capacity,
                    })}
                  </span>
                </div>
                <div className="flex items-center justify-between gap-3">
                  <span>{t('openCodeStatusDialog.memory.ompBus')}</span>
                  <span className="tabular-nums">
                    {t('openCodeStatusDialog.memory.busValue', {
                      entries: data.ompBus.retainedEntries,
                      size: formatBytes(data.ompBus.retainedBytes),
                      capacity: data.ompBus.capacity,
                    })}
                  </span>
                </div>
                <div className="flex items-center justify-between gap-3">
                  <span>{t('openCodeStatusDialog.memory.wireIdEchoes')}</span>
                  <span className="tabular-nums">{data.dataProportional.wireIdEchoes}</span>
                </div>
                <div className="flex items-center justify-between gap-3">
                  <span>{t('openCodeStatusDialog.memory.inFlightUnderflow')}</span>
                  <span className="tabular-nums">{sessions?.inFlightUnderflow ?? 0}</span>
                </div>
              </div>

              <p className="text-muted-foreground/80">{t('openCodeStatusDialog.memory.estimateNote')}</p>
            </div>
          ) : (
            <div className="flex flex-col gap-2">
              <div className="flex items-center justify-between">
                <span className="text-muted-foreground">
                  {t('openCodeStatusDialog.memory.sessionsTitle', {
                    live: sessions?.live ?? 0,
                    total: sessions?.total ?? 0,
                  })}
                </span>
                <label className="inline-flex cursor-pointer items-center gap-1.5 text-muted-foreground">
                  <input
                    type="checkbox"
                    checked={largeOnly}
                    onChange={(event) => setLargeOnly(event.target.checked)}
                    className="h-3.5 w-3.5 accent-primary"
                  />
                  {t('openCodeStatusDialog.monitor.largeOnly')}
                </label>
              </div>

              {rows.length === 0 ? (
                <p className="text-muted-foreground">
                  {t(largeOnly ? 'openCodeStatusDialog.monitor.noLarge' : 'openCodeStatusDialog.memory.noResident')}
                </p>
              ) : (
                <div className="max-h-64 overflow-auto">
                  <table className="w-full border-collapse text-left">
                    <thead className="sticky top-0 bg-surface-muted">
                      <tr className="text-muted-foreground">
                        <th className="py-0.5 pr-2 font-normal">{t('openCodeStatusDialog.memory.colSession')}</th>
                        <th className="py-0.5 pr-2 font-normal">{t('openCodeStatusDialog.memory.colDirectory')}</th>
                        <th className="py-0.5 pr-2 font-normal">{t('openCodeStatusDialog.memory.colState')}</th>
                        <th className="py-0.5 pr-2 text-right font-normal">{t('openCodeStatusDialog.memory.colSize')}</th>
                        <th className="py-0.5 pr-2 text-right font-normal">{t('openCodeStatusDialog.memory.colIdle')}</th>
                        <th className="py-0.5 text-right font-normal">{t('openCodeStatusDialog.monitor.colAction')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {rows.map((row) => {
                        const title = sessionTitles.get(row.id);
                        const releasable = row.state === 'live' && row.inFlight === 0;
                        return (
                          <tr key={`${row.directory}:${row.id}`} className="border-t border-border/40">
                            <td className="max-w-40 truncate py-1 pr-2" title={title ?? row.id}>
                              {title ?? row.id.slice(0, 12)}
                            </td>
                            <td className="max-w-28 truncate py-1 pr-2 text-muted-foreground" title={row.directory}>
                              {shortDirectory(row.directory)}
                            </td>
                            <td className="py-1 pr-2">
                              <span className="inline-flex items-center gap-1.5">
                                <span className={cn('inline-block h-1.5 w-1.5 flex-shrink-0 rounded-full', STATE_TONE[row.state])} />
                                {t(STATE_LABEL_KEY[row.state])}
                                {row.inFlight > 0 ? ` ·${row.inFlight}` : ''}
                              </span>
                            </td>
                            <td className="py-1 pr-2 text-right tabular-nums">
                              {row.transcriptBytes === null ? '—' : `~${formatBytes(row.transcriptBytes)}`}
                            </td>
                            <td className="py-1 pr-2 text-right tabular-nums text-muted-foreground">{formatIdle(row.idleMs)}</td>
                            <td className="py-1 text-right">
                              {releasable ? (
                                <Button
                                  size="xs"
                                  variant="ghost"
                                  disabled={releasing === row.id}
                                  onClick={() => void release(row)}
                                  title={t('openCodeStatusDialog.monitor.releaseHint')}
                                >
                                  {t('openCodeStatusDialog.monitor.release')}
                                </Button>
                              ) : null}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              )}
              {sessions?.truncated ? (
                <p className="text-muted-foreground">{t('openCodeStatusDialog.memory.truncated')}</p>
              ) : null}
              <p className="text-muted-foreground/80">{t('openCodeStatusDialog.memory.estimateNote')}</p>
            </div>
          )
        ) : null}
      </div>
    </section>
  );
};
