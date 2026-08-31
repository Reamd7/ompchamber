/**
 * InternalUriViewer — resolves one internal URI (P1: local://) through the
 * omp URI bridge and renders the content (spec 04 §5.2.5, GAP-02).
 *
 * Mounted by the chat view alongside OmpDialogLayer for the active
 * (directory, sessionId); message markdown opens it through
 * useInternalUriViewerStore. Resolve failures render inline — the endpoint's
 * own error contract (04 §5.2.1: the handler's message is the user-facing
 * text) is shown verbatim, with distinct copy only for the unavailable and
 * transport classes. Absolute paths never arrive (server strips sourcePath,
 * 04 §5.2.4) and resource tokens stay in memory, never logged.
 */

import React from 'react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { toast } from '@/components/ui';
import { useI18n, type I18nContextValue } from '@/lib/i18n';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import type { OmpUriFailure, OmpUriResource } from '@/lib/api/omp';
import { useInternalUriViewerStore } from '@/stores/useInternalUriViewerStore';
import { SimpleMarkdownRenderer } from '@/components/chat/MarkdownRenderer';
import { JsonTreeViewer } from '@/components/ui/JsonTreeViewer';
import { VirtualizedCodeBlock, type CodeLine } from '@/components/chat/message/parts/VirtualizedCodeBlock';

interface InternalUriViewerProps {
  directory: string | undefined;
  sessionId: string | undefined;
}

type ViewerState =
  | { phase: 'idle' }
  | { phase: 'loading' }
  | { phase: 'ready'; resource: OmpUriResource }
  | { phase: 'error'; failure: OmpUriFailure };

/** Content bucket the resolved resource renders as (SDK contract: markdown / json / plain). */
const contentKindOf = (resource: OmpUriResource): 'markdown' | 'json' | 'text' => {
  if (resource.contentType === 'text/markdown') return 'markdown';
  if (resource.contentType === 'application/json') return 'json';
  return 'text';
};

const formatBytes = (size: number): string => `${Math.max(1, Math.round(size / 1024))} KiB`;

const errorTextOf = (failure: OmpUriFailure, t: I18nContextValue['t']): string => {
  if (failure.unavailable) return t('dialogs.internalUri.error.unavailable');
  if (failure.message !== undefined) return failure.message;
  if (failure.error === 'too-large' && typeof failure.size === 'number') {
    return t('dialogs.internalUri.error.tooLarge', { size: formatBytes(failure.size) });
  }
  return t('dialogs.internalUri.error.resolveFailed');
};

const BinaryImageBody: React.FC<{ resource: OmpUriResource; directory: string }> = ({ resource, directory }) => {
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
  }, [ompUri, tokenId, directory]);

  if (failed) {
    return <p className="typography-meta text-muted-foreground">{t('dialogs.internalUri.error.binaryFailed')}</p>;
  }
  if (imageSrc === null) {
    return <p className="typography-meta animate-pulse text-muted-foreground">{t('dialogs.internalUri.loading')}</p>;
  }
  return (
    <div className="flex max-h-[60vh] justify-center overflow-auto rounded-md border border-border/40">
      <img src={imageSrc} alt={resource.url} className="max-h-[60vh] w-auto object-contain" />
    </div>
  );
};

const ViewerBody: React.FC<{ resource: OmpUriResource; directory: string }> = ({ resource, directory }) => {
  if (resource.binary === true) return <BinaryImageBody resource={resource} directory={directory} />;
  const kind = contentKindOf(resource);
  const text = resource.content ?? '';
  if (kind === 'markdown') {
    return <SimpleMarkdownRenderer content={text} variant="tool" />;
  }
  if (kind === 'json') {
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed !== null && typeof parsed === 'object') {
        return <JsonTreeViewer data={parsed} className="max-h-[60vh]" />;
      }
    } catch {
      /* fall through to the plain-text view */
    }
  }
  const lines: CodeLine[] = text.split('\n').map((lineText) => ({ text: lineText }));
  return <VirtualizedCodeBlock lines={lines} language="text" maxHeight="60vh" />;
};

export const InternalUriViewer: React.FC<InternalUriViewerProps> = ({ directory, sessionId }) => {
  const { t } = useI18n();
  const { ompUri } = useRuntimeAPIs();
  const enabled = useOmpFeatureEnabled('uri.v1');
  const url = useInternalUriViewerStore((state) => state.url);
  const target = useInternalUriViewerStore((state) => state.target);
  const close = useInternalUriViewerStore((state) => state.close);
  const [state, setState] = React.useState<ViewerState>({ phase: 'idle' });

  // Host-level supervision (artifacts browser): a `target` pins BOTH resolve
  // ids to the session that owns the file, so one session's local:// content
  // stays openable while another session is in view. Without a target the
  // active (directory, sessionId) behavior is unchanged.
  const resolveSessionId = target?.sessionID ?? sessionId;
  const resolveDirectory = target?.directory ?? directory;

  const open = enabled && url !== null && resolveDirectory !== undefined && resolveDirectory.length > 0
    && resolveSessionId !== undefined && resolveSessionId.length > 0;

  React.useEffect(() => {
    if (url === null || resolveDirectory === undefined || resolveSessionId === undefined) {
      setState({ phase: 'idle' });
      return;
    }
    let cancelled = false;
    setState({ phase: 'loading' });
    void ompUri.resolve({ url, sessionID: resolveSessionId, directory: resolveDirectory }).then((result) => {
      if (cancelled) return;
      setState(result.ok ? { phase: 'ready', resource: result.resource } : { phase: 'error', failure: result });
    });
    return () => {
      cancelled = true;
    };
  }, [ompUri, url, resolveDirectory, resolveSessionId]);

  const copyUrl = React.useCallback(() => {
    if (url === null) return;
    void navigator.clipboard?.writeText(url).then(
      () => toast.success(t('dialogs.internalUri.copied')),
      () => toast.error(t('dialogs.internalUri.copyFailed')),
    );
  }, [t, url]);

  if (!open) {
    return null;
  }

  return (
    <Dialog open onOpenChange={(nextOpen) => { if (!nextOpen) close(); }}>
      <DialogContent className="max-w-3xl" aria-describedby={undefined}>
        <DialogHeader>
          <DialogTitle className="flex min-w-0 items-center gap-2">
            <Icon name="file" className="size-4 shrink-0 text-muted-foreground" />
            <span className="truncate font-mono text-sm" title={url ?? undefined}>{url}</span>
            {state.phase === 'ready' && state.resource.immutable === true ? (
              <span className="shrink-0 rounded-full border border-border/40 px-2 py-0.5 typography-meta text-muted-foreground">
                {t('dialogs.internalUri.immutableBadge')}
              </span>
            ) : null}
            <Button
              variant="ghost"
              size="icon"
              className="ml-auto shrink-0"
              onClick={copyUrl}
              aria-label={t('dialogs.internalUri.copyUrl')}
              title={t('dialogs.internalUri.copyUrl')}
            >
              <Icon name="clipboard" className="size-4" />
            </Button>
          </DialogTitle>
          <DialogDescription className="sr-only">{t('dialogs.internalUri.dialogDescription')}</DialogDescription>
        </DialogHeader>
        {state.phase === 'loading' ? (
          <p className="typography-meta animate-pulse text-muted-foreground">{t('dialogs.internalUri.loading')}</p>
        ) : state.phase === 'error' ? (
          <div className="flex items-start gap-2 rounded-md bg-status-error/10 px-3 py-2 text-sm text-status-error">
            <Icon name="error-warning" className="mt-0.5 size-4 shrink-0" />
            <span className="min-w-0 break-words">{errorTextOf(state.failure, t)}</span>
          </div>
        ) : state.phase === 'ready' ? (
          <ViewerBody resource={state.resource} directory={resolveDirectory ?? ''} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
};
