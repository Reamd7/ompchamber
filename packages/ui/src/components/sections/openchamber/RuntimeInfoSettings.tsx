import React from 'react';

import { useI18n } from '@/lib/i18n';
import { toast } from '@/components/ui';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import {
  getDesktopRuntimeInfo,
  listenBunRuntimeProgress,
  manageDesktopBunRuntime,
  type BunRuntimeProgress,
  type DesktopRuntimeInfo,
} from '@/lib/desktop';
import {
  SettingsSection,
  SETTINGS_FIELD_LABEL_CLASS,
} from '@/components/sections/shared/SettingsSection';

type RuntimeRowProps = {
  label: string;
  children: React.ReactNode;
};

const RuntimeRow: React.FC<RuntimeRowProps> = ({ label, children }) => (
  <div className="flex flex-col gap-2 border-b border-border/40 px-4 py-3 @xl:flex-row @xl:items-center @xl:justify-between">
    <span className={SETTINGS_FIELD_LABEL_CLASS}>{label}</span>
    <span className="typography-meta text-muted-foreground font-mono">{children}</span>
  </div>
);

/**
 * Runtime & updates section of the About page: which update channel answered
 * (light in-app layer vs full installer), the active replaceable-layer
 * version, the managed Bun runtime version/source, live download progress
 * for the runtime and engine native components, and verify/redownload
 * actions. Hidden when the desktop runtime-info bridge is unavailable
 * (web/VS Code surfaces).
 */
export const RuntimeInfoSettings: React.FC = () => {
  const { t } = useI18n();
  const [info, setInfo] = React.useState<DesktopRuntimeInfo | null>(null);
  const [progress, setProgress] = React.useState<BunRuntimeProgress | null>(null);
  const [busy, setBusy] = React.useState<'verify' | 'redownload' | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    void getDesktopRuntimeInfo().then((value) => {
      if (!cancelled) setInfo(value);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  React.useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void listenBunRuntimeProgress((event) => {
      if (disposed) return;
      setProgress(event);
      if (event.phase === 'done' || event.phase === 'natives-done' || event.phase === 'error') {
        void getDesktopRuntimeInfo().then((value) => {
          if (!disposed) setInfo(value);
        });
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const percent = progress && progress.total
    ? Math.min(100, Math.round(((progress.received ?? 0) / progress.total) * 100))
    : null;

  const handleAction = async (action: 'verify' | 'redownload') => {
    setBusy(action);
    try {
      const result = await manageDesktopBunRuntime(action);
      if (result?.ok) {
        toast.success(t(
          action === 'verify'
            ? 'settings.ompchamber.runtime.toast.verified'
            : 'settings.ompchamber.runtime.toast.redownloaded',
          { version: result.version ?? '' },
        ));
        const fresh = await getDesktopRuntimeInfo();
        setInfo(fresh);
      } else {
        toast.error(t('settings.ompchamber.runtime.downloadFailed'));
      }
    } finally {
      setBusy(null);
    }
  };

  if (!info) return null;

  return (
    <SettingsSection title={t('settings.ompchamber.runtime.title')} info={t('settings.ompchamber.runtime.infoHint')}>
      <div className="rounded-lg bg-[var(--surface-elevated)]/70 overflow-hidden flex flex-col">
        <RuntimeRow label={t('settings.ompchamber.runtime.field.updateChannel')}>
          {info.updateChannel === 'asar'
            ? t('settings.ompchamber.runtime.channel.light')
            : t('settings.ompchamber.runtime.channel.full')}
        </RuntimeRow>
        <RuntimeRow label={t('settings.ompchamber.runtime.field.layerVersion')}>
          {info.layer.version ?? t('settings.ompchamber.about.state.unknown')}
        </RuntimeRow>
        <RuntimeRow label={t('settings.ompchamber.runtime.field.bunRuntime')}>
          {info.bun
            ? `${info.bun.version} (${info.bun.source === 'managed'
              ? t('settings.ompchamber.runtime.source.managed')
              : t('settings.ompchamber.runtime.source.path')})`
            : t('settings.ompchamber.about.state.unknown')}
        </RuntimeRow>

        {progress && (progress.phase === 'download' || progress.phase === 'natives-download') && (
          <div className="space-y-2 border-t border-border/40 px-4 py-3">
            <div className="flex items-center justify-between typography-meta">
              <span className="text-muted-foreground">
                {progress.phase === 'natives-download'
                  ? t('settings.ompchamber.runtime.phase.natives')
                  : t('settings.ompchamber.runtime.phase.download')}
              </span>
              {percent !== null && <span className="font-mono">{percent}%</span>}
            </div>
            <div className="h-1.5 bg-[var(--surface-subtle)] rounded-full overflow-hidden">
              <div
                className="h-full bg-[var(--primary-base)] transition-all duration-300"
                style={{ width: `${percent ?? 2}%` }}
              />
            </div>
          </div>
        )}

        {progress?.phase === 'error' && (
          <div className="border-t border-border/40 px-4 py-2">
            <p className="typography-meta text-[var(--status-error)]">
              {t('settings.ompchamber.runtime.downloadFailed')}
            </p>
          </div>
        )}

        <div className="flex items-center gap-3 border-t border-border/40 px-4 py-3">
          <Button size="sm" variant="outline" onClick={() => void handleAction('verify')} disabled={busy !== null}>
            {busy === 'verify' ? <Icon name="loader" className="h-4 w-4 mr-1 animate-spin" /> : null}
            {t('settings.ompchamber.runtime.actions.verify')}
          </Button>
          <Button size="sm" variant="outline" onClick={() => void handleAction('redownload')} disabled={busy !== null}>
            {busy === 'redownload' ? <Icon name="loader" className="h-4 w-4 mr-1 animate-spin" /> : null}
            {t('settings.ompchamber.runtime.actions.redownload')}
          </Button>
        </div>
      </div>
    </SettingsSection>
  );
};
