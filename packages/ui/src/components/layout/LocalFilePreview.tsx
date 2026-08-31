/**
 * LocalFilePreview — read-only preview of one session's local:// file,
 * rendered in the context panel's editor area (spec 04 artifacts browse).
 *
 * The no-dialog counterpart of InternalUriViewer's body: the SAME resolve
 * + token pipeline (session-pinned, absolute paths never leave the server),
 * but presented like a normal file tab. Text renders markdown/json/plain;
 * previewable binaries (the SDK's image fallback writes webp here) stream
 * bytes via the token content endpoint into a blob image. A null target is
 * the browse state: pick a file in the session tree.
 */

import React from 'react';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { SimpleMarkdownRenderer } from '@/components/chat/MarkdownRenderer';
import { JsonTreeViewer } from '@/components/ui/JsonTreeViewer';
import { VirtualizedCodeBlock, type CodeLine } from '@/components/chat/message/parts/VirtualizedCodeBlock';
import type { OmpUriResource } from '@/lib/api/omp';

type PreviewState =
  | { phase: 'idle' }
  | { phase: 'loading' }
  | { phase: 'ready'; resource: OmpUriResource }
  | { phase: 'error'; message?: string };

const BinaryImageBody: React.FC<{ resource: OmpUriResource; directory: string; sessionID: string }> = ({
  resource,
  directory,
  sessionID,
}) => {
  const { ompUri } = useRuntimeAPIs();
  const { t } = useI18n();
  const [imageSrc, setImageSrc] = React.useState<string | null>(null);
  const [failed, setFailed] = React.useState(false);
  const tokenId = resource.token?.id;

  React.useEffect(() => {
    let objectUrl: string | null = null;
    let cancelled = false;
    if (tokenId === undefined) {
      setFailed(true);
      return;
    }
    void ompUri.fetchContent({ token: tokenId, directory }).then((result) => {
      if (cancelled) return;
      if (!result.ok) {
        setFailed(true);
        return;
      }
      objectUrl = URL.createObjectURL(result.blob);
      setImageSrc(objectUrl);
    });
    return () => {
      cancelled = true;
      if (objectUrl !== null) URL.revokeObjectURL(objectUrl);
    };
  }, [ompUri, tokenId, directory, sessionID]);

  if (failed) {
    return <p className="typography-meta text-muted-foreground">{t('contextPanel.localFiles.previewFailed')}</p>;
  }
  if (imageSrc === null) {
    return <p className="typography-meta animate-pulse text-muted-foreground">{t('contextPanel.localFiles.filesLoading')}</p>;
  }
  return (
    <div className="flex max-h-full justify-center overflow-auto p-2">
      <img src={imageSrc} alt={resource.url} className="max-h-full w-auto object-contain" />
    </div>
  );
};

const TextBody: React.FC<{ resource: OmpUriResource }> = ({ resource }) => {
  const text = resource.content ?? '';
  if (resource.contentType === 'text/markdown') {
    return (
    <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
        <SimpleMarkdownRenderer content={text} variant="tool" />
      </div>
    );
  }
  if (resource.contentType === 'application/json') {
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed instanceof Object) {
        return (
          <div className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
            <JsonTreeViewer data={parsed} className="max-h-full" />
          </div>
        );
      }
    } catch {
      /* fall through to the plain-text view */
    }
  }
  const lines: CodeLine[] = text.split('\n').map((lineText) => ({ text: lineText }));
  return <VirtualizedCodeBlock lines={lines} language="text" maxHeight="100%" />;
};

export const LocalFilePreview: React.FC<{
  directory: string;
  sessionID: string;
  fileRef: string | null;
}> = ({ directory, sessionID, fileRef }) => {
  const { t } = useI18n();
  const { ompUri } = useRuntimeAPIs();
  const [state, setState] = React.useState<PreviewState>({ phase: 'idle' });
  const fetchEpoch = React.useRef(0);

  React.useEffect(() => {
    if (fileRef === null || fileRef.length === 0) {
      setState({ phase: 'idle' });
      return;
    }
    const epoch = ++fetchEpoch.current;
    setState({ phase: 'loading' });
    const scheme = 'local';
    void ompUri.resolve({ url: `${scheme}://${fileRef}`, sessionID, directory }).then((result) => {
      if (fetchEpoch.current !== epoch) return;
      if (result.ok) {
        setState({ phase: 'ready', resource: result.resource });
        return;
      }
      const nextState: PreviewState = { phase: 'error' };
      if (!result.unavailable && result.message !== undefined) {
        nextState.message = result.message;
      }
      setState(nextState);
    });
  }, [ompUri, fileRef, sessionID, directory]);

  if (fileRef === null || fileRef.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
        <Icon name="folder" className="h-12 w-12 text-muted-foreground/50" />
        <div className="typography-ui-header text-foreground">{t('contextPanel.localFiles.title')}</div>
        <div className="max-w-sm typography-micro text-muted-foreground">{t('contextPanel.localFiles.browseHint')}</div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden">
      <div className="flex h-8 shrink-0 items-center gap-2 border-b border-border/40 px-3">
        <span className="min-w-0 truncate typography-meta font-mono text-muted-foreground">{'local://' + fileRef}</span>
      </div>
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
        {state.phase === 'loading' || state.phase === 'idle' ? (
          <div className="flex items-center justify-center gap-2 py-8 text-muted-foreground">
            <Icon name="refresh" className="size-4 animate-spin" />
            <span className="typography-meta">{t('contextPanel.localFiles.filesLoading')}</span>
          </div>
        ) : state.phase === 'error' ? (
          <div className="flex items-start gap-2 px-4 py-3 text-sm text-status-error">
            <Icon name="error-warning" className="mt-0.5 size-4 shrink-0" />
            <span className="min-w-0 break-words">
              {state.message ?? t('contextPanel.localFiles.previewFailed')}
            </span>
          </div>
        ) : state.resource.binary === true ? (
          <BinaryImageBody resource={state.resource} directory={directory} sessionID={sessionID} />
        ) : (
          <TextBody resource={state.resource} />
        )}
      </div>
    </div>
  );
};
