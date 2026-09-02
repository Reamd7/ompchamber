import React from 'react';
import { runtimeFetch, type RuntimeFetchOptions } from '@/lib/runtime-fetch';
import {
  OMP_ENDPOINTS,
  parseOmpSessionEntriesPayload,
  type OmpSessionEntry,
} from '@/lib/api/omp';
import { useI18n } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { Icon } from '@/components/icon/Icon';
import type { IconName } from '@/components/icon/icons';

/**
 * Session timeline (ch 11 §4.2): the structured-read surface over
 * GET /omp/sessions/{id}/entries — compaction, model/mode switches, TTSR
 * injections, retry recovery — as a chronological list for the active
 * session. On-demand fetch (open = fresh data); unavailable surface (404 on
 * older servers) renders the empty state.
 */
const ENTRY_ROW: Partial<Record<string, { icon: IconName; className: string }>> = {
  compaction: { icon: 'camera', className: 'text-muted-foreground' },
  branch_summary: { icon: 'route', className: 'text-muted-foreground' },
  model_change: { icon: 'flashlight', className: 'text-[var(--status-info)]' },
  mode_change: { icon: 'route', className: 'text-[var(--status-info)]' },
  ttsr_injection: { icon: 'shield-check', className: 'text-[var(--status-warning)]' },
  retry_recovery: { icon: 'refresh', className: 'text-[var(--status-warning)]' },
};

const rowStyle = (kind: string) => ENTRY_ROW[kind] ?? { icon: 'history', className: 'text-muted-foreground' };

const describeEntry = (entry: OmpSessionEntry): string => {
  // Entry fields are kind-owned and untyped beyond `kind` (OmpSessionEntry is
  // the server's open row shape); typeof guards are the sanctioned one-off
  // field-read escape for that contract.
  const model = typeof entry.model === 'string' ? entry.model : '';
  const mode = typeof entry.mode === 'string' ? entry.mode : '';
  switch (entry.kind) {
    case 'compaction':
      return typeof entry.tokensBefore === 'number'
        ? `${model || 'compacted'} · ${entry.tokensBefore}→?`
        : model || 'compacted';
    case 'branch_summary':
      return typeof entry.fromId === 'string' ? `⑂ ${entry.fromId}` : 'branch';
    case 'model_change':
      return model;
    case 'mode_change':
      return mode;
    case 'ttsr_injection': {
      const rules = Array.isArray(entry.rules)
        ? entry.rules.filter((r): r is { name: string } =>
            !!r && typeof r === 'object' && 'name' in r && typeof r.name === 'string')
        : [];
      return rules.length > 0 ? rules.map((r) => r.name).join(', ') : 'rules injected';
    }
    case 'retry_recovery':
      return typeof entry.note === 'string' ? entry.note : 'retried';
    default:
      return '';
  }
};

export const SessionTimelineTab: React.FC = () => {
  const { t } = useI18n();
  const sessionID = useSessionUIStore((s) => s.currentSessionId);
  // Session-scoped resolution (matches ChatInput): a session opened from
  // another project must query that project's directory, not the app's
  // currently-active one.
  const directory = useSessionUIStore((s) => (s.currentSessionId ? s.getDirectoryForSession(s.currentSessionId) : null))
  const [entries, setEntries] = React.useState<OmpSessionEntry[] | null>(null);
  const [loading, setLoading] = React.useState(false);

  React.useEffect(() => {
    if (!sessionID) {
      setEntries(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    void runtimeFetch(`${OMP_ENDPOINTS.sessionEntries(sessionID)}?directory=${encodeURIComponent(directory ?? '')}`, {
      method: 'GET',
      headers: { Accept: 'application/json' },
    } as RuntimeFetchOptions)
      .then(async (response) => {
        if (cancelled) return;
        if (!response.ok) {
          setEntries(null);
          return;
        }
        setEntries(parseOmpSessionEntriesPayload(await response.json()));
      })
      .catch(() => {
        if (!cancelled) setEntries(null);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionID, directory]);

  if (!sessionID) {
    return <div className="p-4 typography-meta text-muted-foreground">{t('contextPanel.timeline.emptyNoSession')}</div>;
  }
  if (loading) {
    return <div className="p-4 typography-meta text-muted-foreground">{t('contextPanel.timeline.loading')}</div>;
  }
  if (!entries || entries.length === 0) {
    return <div className="p-4 typography-meta text-muted-foreground">{t('contextPanel.timeline.empty')}</div>;
  }
  return (
    <div className="p-2 flex flex-col gap-0.5 overflow-y-auto h-full">
      {entries.map((entry, index) => {
        const style = rowStyle(entry.kind);
        const detail = describeEntry(entry);
        return (
          <div key={`${entry.kind}-${index}`} className="flex items-center gap-2 py-1 px-2 rounded-md hover:bg-accent/40 min-w-0">
            <Icon name={style.icon} className={`h-3.5 w-3.5 flex-shrink-0 ${style.className}`} aria-hidden="true" />
            <span className="typography-ui-label text-foreground flex-shrink-0">
              {t(`contextPanel.timeline.kind.${entry.kind}` as never)}
            </span>
            {detail ? <span className="typography-meta text-muted-foreground truncate">{detail}</span> : null}
          </div>
        );
      })}
    </div>
  );
};
