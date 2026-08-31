/**
 * MobileSessionFiles — full-screen mobile view of the ACTIVE session's
 * private local:// files (spec 04 artifacts browse; capability `artifacts`).
 *
 * Mobile counterpart of the desktop context-panel scope switch: the files
 * surface header toggles between "Workspace" and "Session"; this component
 * renders the session scope with the same tree rows (FileTypeIcon, name,
 * date, size). Tapping a file opens LocalFilePreview full-screen with a back
 * row — the same navigation model as MobileFilesSurface. The tree follows
 * the current session; switching sessions switches trees.
 */

import React from 'react';
import { RiArrowLeftLine, RiRefreshLine } from '@remixicon/react';
import { Icon } from '@/components/icon/Icon';
import { FileTypeIcon } from '@/components/icons/FileTypeIcon';
import { getCurrentIntlLocale, useI18n } from '@/lib/i18n';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { buildLocalFileRows, formatLocalFileBytes, type LocalFileRow } from '@/components/files/localFileTree';
import { LocalFilePreview } from '@/components/layout/LocalFilePreview';

type SessionFilesState =
  | { phase: 'idle' }
  | { phase: 'loading' }
  | { phase: 'ready'; rows: LocalFileRow[]; truncated: boolean }
  | { phase: 'error' };

type MobileRoute =
  | { type: 'list' }
  | { type: 'preview'; fileRef: string; sessionID: string; directory: string };

const HEADER_HEIGHT = 'var(--oc-header-height,56px)';

export const MobileSessionFiles: React.FC = () => {
  const { t } = useI18n();
  const { ompArtifacts } = useRuntimeAPIs();
  const enabled = useOmpFeatureEnabled('artifacts');
  const sessionId = useSessionUIStore((state) => state.currentSessionId);
  const sessionDirectory = useSessionUIStore((state) => state.currentSessionDirectory);

  const [filesState, setFilesState] = React.useState<SessionFilesState>({ phase: 'idle' });
  const [route, setRoute] = React.useState<MobileRoute>({ type: 'list' });
  const fetchEpoch = React.useRef(0);

  const effectiveSessionId = sessionId !== null && sessionId.length > 0 ? sessionId : null;
  const effectiveDirectory = sessionDirectory !== null && sessionDirectory.length > 0 ? sessionDirectory : '';
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
    });
  }, [shouldFetch, effectiveSessionId, effectiveDirectory, ompArtifacts]);

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

  // --- Preview route ---
  if (route.type === 'preview') {
    return (
      <div className="flex h-full flex-col overflow-hidden bg-background text-foreground">
        <header
          className="flex shrink-0 items-center gap-2 border-b border-border/70 px-3 text-foreground"
          style={{ height: HEADER_HEIGHT }}
        >
          <button
            type="button"
            className="-ml-1 flex size-10 shrink-0 items-center justify-center rounded-lg text-muted-foreground hover:bg-interactive-hover hover:text-foreground"
            aria-label={t('header.actions.backAria')}
            onClick={() => setRoute({ type: 'list' })}
          >
            <RiArrowLeftLine className="size-5" />
          </button>
          <span className="min-w-0 flex-1 truncate typography-ui-label font-mono">
            {'local://' + route.fileRef}
          </span>
        </header>
        <div className="min-h-0 flex-1">
          <LocalFilePreview
            directory={route.directory}
            sessionID={route.sessionID}
            fileRef={route.fileRef}
          />
        </div>
      </div>
    );
  }

  // --- List route ---
  return (
    <div className="flex h-full flex-col overflow-hidden bg-background text-foreground">
      <header
        className="flex shrink-0 items-center gap-2 border-b border-border/70 px-3 text-foreground"
        style={{ height: HEADER_HEIGHT }}
      >
        <span className="min-w-0 flex-1 truncate typography-ui-header font-semibold">
          {t('contextPanel.localFiles.title')}
        </span>
        {filesState.phase === 'loading' ? (
          <RiRefreshLine className="size-4 shrink-0 animate-spin text-muted-foreground" />
        ) : null}
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {!enabled || !hasIds ? (
          <p className="px-4 py-8 text-center typography-meta text-muted-foreground">
            {effectiveSessionId === null
              ? t('contextPanel.localFiles.noSession')
              : t('contextPanel.localFiles.noDirectory')}
          </p>
        ) : filesState.phase === 'loading' || filesState.phase === 'idle' ? (
          <div className="flex items-center justify-center gap-2 py-10 text-muted-foreground">
            <RiRefreshLine className="size-4 animate-spin" />
            <span className="typography-meta">{t('contextPanel.localFiles.filesLoading')}</span>
          </div>
        ) : filesState.phase === 'error' ? (
          <p className="px-4 py-8 text-center typography-meta text-status-error">
            {t('contextPanel.localFiles.filesError')}
          </p>
        ) : filesState.rows.length === 0 ? (
          <p className="px-4 py-8 text-center typography-meta text-muted-foreground">
            {t('contextPanel.localFiles.emptyFiles')}
          </p>
        ) : (
          <>
            {filesState.rows.map((row) => (
              <button
                key={row.ref}
                type="button"
                onClick={() => effectiveSessionId !== null
                  && setRoute({ type: 'preview', fileRef: row.ref, sessionID: effectiveSessionId, directory: effectiveDirectory })}
                aria-label={t('contextPanel.localFiles.openFile', { ref: row.ref })}
                className="flex w-full items-center gap-3 border-b border-border/30 px-4 py-3 text-left active:bg-interactive-hover"
              >
                <FileTypeIcon filePath={row.name} className="size-5 shrink-0" />
                <div className="min-w-0 flex-1">
                  <p className="truncate typography-ui-label font-mono">{row.name}</p>
                  <p className="truncate typography-meta text-muted-foreground">
                    {(row.modifiedAt ?? 0) > 0 ? formatTime(row.modifiedAt ?? 0) : ''}
                    {row.size !== undefined ? ` · ${formatLocalFileBytes(row.size)}` : ''}
                  </p>
                </div>
                <Icon name="arrow-right-s" className="size-4 shrink-0 text-muted-foreground" />
              </button>
            ))}
            {filesState.truncated ? (
              <p className="px-4 py-3 text-center typography-meta text-muted-foreground">
                {t('contextPanel.localFiles.truncated')}
              </p>
            ) : null}
          </>
        )}
      </div>
    </div>
  );
};
