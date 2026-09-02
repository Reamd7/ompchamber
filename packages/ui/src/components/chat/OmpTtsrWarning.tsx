/**
 * OmpTtsrWarning — transient steering-rule warning strip (TUI
 * TtsrNotificationComponent parity). The reducer merges consecutive
 * `omp.ttsr.triggered` events into one volatile per-session block with a
 * 2-minute TTL sweep (`useOmpSessionStore`), which this strip renders inside
 * the transcript flow. Manual dismissal drops the block from the store, so a
 * later trigger raises a fresh one instead of resurrecting dismissed rules.
 */
import React from 'react';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useOmpSessionStore, useOmpTtsrWarning } from '@/sync/useOmpSessionStore';
import type { OmpTtsrWarning as OmpTtsrWarningBlock } from '@/sync/omp-event-reducer';

interface TtsrWarningViewProps {
  warning: OmpTtsrWarningBlock;
  dismissAria: string;
  title: string;
  onDismiss: () => void;
}

/** Pure presentational strip so both states render in SSR tests. */
export const TtsrWarningView: React.FC<TtsrWarningViewProps> = ({ warning, dismissAria, title, onDismiss }) => (
  <div
    data-omp-ttsr-warning
    role="status"
    className="mx-1 my-2 flex items-start gap-2 rounded-lg px-3 py-2 text-xs"
    style={{
      backgroundColor: 'var(--status-warning-background)',
      color: 'var(--status-warning)'
    }}
  >
    <Icon name="alert" className="size-3.5 shrink-0" />
    <div className="min-w-0 flex-1">
      <p className="font-medium">{title}</p>
      <p className="break-words opacity-90">{warning.rules.join(' · ')}</p>
    </div>
    <button type="button" aria-label={dismissAria} className="shrink-0 rounded p-0.5 transition-colors hover:opacity-80 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--interactive-focus-ring)]" onClick={onDismiss}>
      <Icon name="close" className="size-3" />
    </button>
  </div>
);

interface OmpTtsrWarningProps {
  sessionId: string | undefined;
}

export const OmpTtsrWarning: React.FC<OmpTtsrWarningProps> = ({ sessionId }) => {
  const { t } = useI18n();
  const getDirectoryForSession = useSessionUIStore((state) => state.getDirectoryForSession);
  const directory = sessionId ? getDirectoryForSession(sessionId) : null;
  const warning = useOmpTtsrWarning(directory ?? '', sessionId);
  const dismissTtsrWarning = useOmpSessionStore((state) => state.dismissTtsrWarning);

  if (!warning || warning.rules.length === 0) return null;

  return <TtsrWarningView warning={warning} title={t('chat.omp.ttsrWarning.title')} dismissAria={t('chat.omp.ttsrWarning.dismissAria')} onDismiss={() => dismissTtsrWarning(directory ?? '', sessionId ?? '')} />;
};
