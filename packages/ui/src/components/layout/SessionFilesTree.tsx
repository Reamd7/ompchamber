/**
 * SessionFilesTree — the context panel's editor tree scoped to the ACTIVE
 * session's private local:// files (spec 04 artifacts browse; capability
 * `artifacts`).
 *
 * Replaces the workspace tree while the tree scope is 'session': same pane,
 * same row anatomy (FileTypeIcon, indent guides), no dialog. The tree
 * follows the current session — switching sessions switches trees. Rows are
 * per-session (sessionID, ref) keyed and never absolute paths (R7). Failure
 * never masquerades as empty: an error keeps prior rows and surfaces a
 * message; zero files is an authoritative empty state. Read-only by
 * contract — writes stay on each session's own tool paths.
 */

import React from 'react';
import { Icon } from '@/components/icon/Icon';
import { FileTypeIcon } from '@/components/icons/FileTypeIcon';
import { getCurrentIntlLocale, useI18n } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useUIStore } from '@/stores/useUIStore';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { buildLocalFileRows, formatLocalFileBytes, type LocalFileRow } from '@/components/files/localFileTree';
import { cn } from '@/lib/utils';

type SessionFilesState =
  | { phase: 'idle' }
  | { phase: 'loading' }
  | { phase: 'ready'; rows: LocalFileRow[]; truncated: boolean }
  | { phase: 'error' };

export const SessionFilesTree: React.FC<{ directory: string }> = ({ directory }) => {
  const { t } = useI18n();
  const { ompArtifacts } = useRuntimeAPIs();
  const enabled = useOmpFeatureEnabled('artifacts');
  const openContextLocalFile = useUIStore((state) => state.openContextLocalFile);
  const setContextTreeScope = useUIStore((state) => state.setContextTreeScope);
  // The tree follows the current session, not the directory alone: a session
  // in a worktree of this project still owns its own local:// root.
  const sessionId = useSessionUIStore((state) => state.currentSessionId);
  const sessionDirectory = useSessionUIStore((state) => state.currentSessionDirectory);

  const [filesState, setFilesState] = React.useState<SessionFilesState>({ phase: 'idle' });
  const [collapsedDirs, setCollapsedDirs] = React.useState<ReadonlySet<string>>(new Set());
  // Generation token: a session switch mid-fetch must not let the stale
  // response clobber the new tree (same pattern as SessionTreeDialog).
  const fetchEpoch = React.useRef(0);

  const effectiveSessionId = sessionId !== null && sessionId.length > 0 ? sessionId : null;
  // Prefer the session's own directory (worktree sessions list their own
  // root); fall back to the panel directory for directory-level opens.
  const effectiveDirectory = sessionDirectory !== null && sessionDirectory.length > 0
    ? sessionDirectory
    : directory;
  const hasIds = effectiveSessionId !== null && effectiveDirectory.length > 0;
  const shouldFetch = enabled && hasIds;

  React.useEffect(() => {
    if (!shouldFetch || effectiveSessionId === null) {
      setFilesState({ phase: 'idle' });
      return;
    }
    const epoch = ++fetchEpoch.current;
    setFilesState((previous) => (previous.phase === 'ready' ? previous : { phase: 'loading' }));
    void ompArtifacts.listSessionArtifacts({
      directory: effectiveDirectory,
      sessionID: effectiveSessionId,
    }).then((result) => {
      if (fetchEpoch.current !== epoch) return;
      if (!result.ok) {
        setFilesState((previous) => (previous.phase === 'ready' ? previous : { phase: 'error' }));
        return;
      }
      setFilesState({ phase: 'ready', rows: buildLocalFileRows(result.files, new Set()), truncated: result.truncated });
      setCollapsedDirs(new Set());
    });
  }, [shouldFetch, effectiveSessionId, effectiveDirectory, ompArtifacts]);

  const toggleDir = (ref: string) => {
    setCollapsedDirs((previous) => {
      const next = new Set(previous);
      if (next.has(ref)) next.delete(ref);
      else next.add(ref);
      return next;
    });
  };

  const formatTime = (value: number): string =>
    value > 0
      ? new Date(value).toLocaleString(getCurrentIntlLocale(), {
        month: 'short',
        day: 'numeric',
        hour: 'numeric',
        minute: '2-digit',
        hour12: true,
      })
      : '';

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="min-h-0 flex-1 overflow-y-auto p-1">
        {!enabled || !hasIds ? (
          <div className="px-2 py-6 text-center typography-meta text-muted-foreground">
            {effectiveSessionId === null
              ? t('contextPanel.localFiles.noSession')
              : t('contextPanel.localFiles.noDirectory')}
          </div>
        ) : filesState.phase === 'loading' || filesState.phase === 'idle' ? (
          <div className="flex items-center justify-center gap-2 py-6 text-muted-foreground">
            <Icon name="refresh" className="size-4 animate-spin" />
            <span className="typography-meta">{t('contextPanel.localFiles.filesLoading')}</span>
          </div>
        ) : filesState.phase === 'error' ? (
          <div className="px-2 py-6 text-center typography-meta text-status-error">
            {t('contextPanel.localFiles.filesError')}
          </div>
        ) : filesState.rows.length === 0 ? (
          <div className="px-2 py-6 text-center typography-meta text-muted-foreground">
            {t('contextPanel.localFiles.emptyFiles')}
          </div>
        ) : (
          <>
            {filesState.rows.map((row) => row.kind === 'dir' ? (
              <button
                key={`dir:${row.ref}`}
                type="button"
                onClick={() => toggleDir(row.ref)}
                aria-expanded={!collapsedDirs.has(row.ref)}
                className="flex w-full items-center gap-2 rounded-md px-1.5 py-1 text-left hover:bg-interactive-hover"
                style={{ paddingLeft: `${0.375 + row.depth * 0.875}rem` }}
              >
                <Icon
                  name={collapsedDirs.has(row.ref) ? 'arrow-right-s' : 'arrow-down-s'}
                  className="size-3.5 shrink-0 text-muted-foreground"
                />
                <Icon name="folder" className="size-4 shrink-0 text-muted-foreground" />
                <span className="min-w-0 flex-1 truncate typography-ui-label">{row.name}</span>
              </button>
            ) : (
              <button
                key={`file:${row.ref}`}
                type="button"
                onClick={() => effectiveSessionId !== null
                  && openContextLocalFile(effectiveDirectory, effectiveSessionId, row.ref)}
                aria-label={t('contextPanel.localFiles.openFile', { ref: row.ref })}
                className={cn('flex w-full items-center gap-1.5 rounded-md px-1.5 py-1 text-left hover:bg-interactive-hover')}
                style={{ paddingLeft: `${0.5 + row.depth * 0.875}rem` }}
                title={row.ref}
              >
                <FileTypeIcon filePath={row.name} className="size-4 shrink-0" />
                <span className="min-w-0 flex-1 truncate typography-ui-label font-mono">{row.name}</span>
                {row.size !== undefined ? (
                  <span className="shrink-0 typography-meta text-[10px] tabular-nums text-muted-foreground">{formatLocalFileBytes(row.size)}</span>
                ) : null}
              </button>
            ))}
            {filesState.truncated ? (
              <div className="px-2 py-2 text-center typography-meta text-muted-foreground">
                {t('contextPanel.localFiles.truncated')}
              </div>
            ) : null}
          </>
        )}
      </div>
    </div>
  );
};
