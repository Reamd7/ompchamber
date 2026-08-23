import React from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { toast } from '@/components/ui';
import { Icon } from '@/components/icon/Icon';
import { useI18n } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { SettingsPageLayout } from '@/components/sections/shared/SettingsPageLayout';
import { SettingsCheckboxRow, SettingsSection } from '@/components/sections/shared/SettingsSection';
import { useOmpPluginsStore } from '@/stores/useOmpPluginsStore';
import type { OmpPluginRecord, OmpPluginSetting } from '@/lib/api/omp';

const ScopeBadge: React.FC<{ scope: 'user' | 'project'; label: string }> = ({ scope, label }) => (
  <span className={cn(
    'typography-micro rounded-full border px-2 py-0.5',
    scope === 'project'
      ? 'border-[var(--status-info-border)] bg-[var(--status-info-background)] text-[var(--status-info)]'
      : 'border-[var(--interactive-border)] bg-[var(--surface-elevated)] text-foreground',
  )}>
    {label}
  </span>
);

const PluginSettingControl: React.FC<{
  plugin: OmpPluginRecord;
  settingKey: string;
  setting: OmpPluginSetting;
}> = ({ plugin, settingKey, setting }) => {
  const { t } = useI18n();
  const updatePlugin = useOmpPluginsStore((state) => state.updatePlugin);
  const isSaving = useOmpPluginsStore((state) => state.isSaving);
  const [value, setValue] = React.useState<unknown>(setting.value ?? setting.default ?? '');

  const save = async () => {
    const result = await updatePlugin({ id: plugin.id, setting: { key: settingKey, value } });
    if (result.ok) toast.success(t('settings.view.pendingRestart.saved'));
    else toast.error(t('settings.plugins.sidebar.toast.deleteFailed'));
  };

  if (setting.type === 'boolean') {
    return (
      <div className="space-y-2">
        <SettingsCheckboxRow
          checked={Boolean(value)}
          onChange={setValue}
          label={settingKey}
          ariaLabel={settingKey}
          disabled={!plugin.permissions.settings || isSaving}
        />
        {plugin.permissions.settings ? <Button size="sm" onClick={() => void save()} disabled={isSaving}>{t('settings.plugins.page.action.save')}</Button> : null}
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {setting.description ? <p className="typography-meta text-muted-foreground">{setting.description}</p> : null}
      <Input
        value={String(value ?? '')}
        onChange={(event) => setValue(event.target.value)}
        type={setting.secret ? 'password' : 'text'}
        disabled={!plugin.permissions.settings || isSaving}
      />
      {plugin.permissions.settings ? <Button size="sm" onClick={() => void save()} disabled={isSaving}>{t('settings.plugins.page.action.save')}</Button> : null}
    </div>
  );
};

const PluginDetails: React.FC<{ plugin: OmpPluginRecord }> = ({ plugin }) => {
  const { t } = useI18n();
  const updatePlugin = useOmpPluginsStore((state) => state.updatePlugin);
  const revealPlugin = useOmpPluginsStore((state) => state.revealPlugin);
  const reloadPlugins = useOmpPluginsStore((state) => state.reloadPlugins);
  const extensions = useOmpPluginsStore((state) => state.extensions);
  const isSaving = useOmpPluginsStore((state) => state.isSaving);
  const manifestExtensions = extensions.filter((extension) => extension.pluginId === plugin.id);
  const applied = useOmpPluginsStore((state) => state.applied);
  const appliedHere = applied.filter((session) => session.pluginNames.includes(plugin.name)
    || session.extensionIds.some((id) => manifestExtensions.some((extension) => extension.id === id)));

  return (
    <SettingsPageLayout
      title={plugin.name}
      titleAccessory={(
        <ScopeBadge
          scope={plugin.scope}
          label={plugin.scope === 'project'
            ? t('settings.plugins.sidebar.group.projectEntries')
            : t('settings.plugins.sidebar.group.userEntries')}
        />
      )}
      showSaveStatus={false}
    >
      <SettingsSection title={t('settings.plugins.page.field.spec')} divider={false} settingsItem="plugins.spec">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="font-mono typography-meta">{plugin.name}@{plugin.version}</div>
            {plugin.description ? <p className="typography-ui text-muted-foreground">{plugin.description}</p> : null}
          </div>
          <Button variant="outline" size="sm" className="shrink-0" onClick={() => { void revealPlugin(plugin.id); }}>
            <Icon name="folder" className="size-3.5" />
            {t('settings.plugins.actions.reveal')}
          </Button>
        </div>
      </SettingsSection>

      {plugin.permissions.toggle ? (
        <SettingsSection title={t('settings.plugins.page.header.entry')} settingsItem="plugins.options">
          <SettingsCheckboxRow
            checked={plugin.enabled}
            onChange={(enabled) => void updatePlugin({ id: plugin.id, enabled })}
            label={plugin.enabled
              ? t('sessions.scheduledTasks.dialog.taskToggle.enabled')
              : t('settings.remoteInstances.relay.state.disabled')}
            ariaLabel={t('settings.plugins.page.header.entry')}
            disabled={isSaving}
          />
        </SettingsSection>
      ) : null}

      {Object.entries(plugin.settings ?? {}).map(([key, setting]) => (
        <SettingsSection key={key} title={key}>
          <PluginSettingControl plugin={plugin} settingKey={key} setting={setting} />
        </SettingsSection>
      ))}

      {plugin.features?.length ? (
        <SettingsSection title={t('settings.plugins.page.field.options')}>
          <div className="space-y-1">
            {plugin.features.map((feature) => (
              <SettingsCheckboxRow
                key={feature.name}
                checked={feature.enabled}
                onChange={(enabled) => {
                  const enabledFeatures = plugin.features?.filter((item) => item.name !== feature.name && item.enabled).map((item) => item.name) ?? [];
                  if (enabled) enabledFeatures.push(feature.name);
                  void updatePlugin({ id: plugin.id, enabledFeatures });
                }}
                label={feature.name}
                ariaLabel={feature.name}
                info={feature.description}
                disabled={!plugin.permissions.features || isSaving}
              />
            ))}
          </div>
        </SettingsSection>
      ) : null}

      {manifestExtensions.length > 0 ? (
        <SettingsSection title={t('settings.plugins.page.field.content')}>
          <div className="space-y-2">
            {manifestExtensions.map((extension) => (
              <div key={extension.id} className="rounded border border-border/60 px-2 py-1.5">
                <div className="font-mono typography-meta">{extension.declaredEntry ?? extension.name}</div>
                <div className="typography-micro text-muted-foreground">{extension.loaded ? extension.name : t('settings.plugins.registry.banner.invalid.pathMissing')}</div>
              </div>
            ))}
          </div>
        </SettingsSection>
      ) : null}

      <SettingsSection title={t('settings.plugins.page.applied.title')}>
        {appliedHere.length === 0 ? (
          <p className="typography-meta text-muted-foreground">{t('settings.plugins.page.applied.empty')}</p>
        ) : (
          <div className="space-y-1">
            {appliedHere.map((session) => (
              <div key={session.sessionId} className="typography-meta flex items-center justify-between gap-3 rounded border border-border/60 px-2 py-1.5">
                <span className="min-w-0 truncate font-mono">{session.sessionId}</span>
                <span className="shrink-0 text-muted-foreground">
                  {t('settings.plugins.page.applied.since', { time: new Date(session.appliedAt).toLocaleTimeString() })}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6 shrink-0 text-muted-foreground"
                  onClick={() => { void reloadPlugins(session.sessionId); }}
                  aria-label={t('settings.plugins.page.applied.refreshSession')}
                  title={t('settings.plugins.page.applied.refreshSession')}
                >
                  <Icon name="restart" className="size-3" />
                </Button>
              </div>
            ))}
          </div>
        )}
      </SettingsSection>
    </SettingsPageLayout>
  );
};

export const PluginsPage: React.FC = () => {
  const { t } = useI18n();
  const selectedId = useOmpPluginsStore((state) => state.selectedId);
  const revealExtension = useOmpPluginsStore((state) => state.revealExtension);
  const reloadPlugins = useOmpPluginsStore((state) => state.reloadPlugins);
  const plugins = useOmpPluginsStore((state) => state.plugins);
  const extensions = useOmpPluginsStore((state) => state.extensions);
  const readExtension = useOmpPluginsStore((state) => state.readExtension);
  const updateExtension = useOmpPluginsStore((state) => state.updateExtension);
  const isSaving = useOmpPluginsStore((state) => state.isSaving);
  const selectedPlugin = selectedId ? plugins.find((item) => item.id === selectedId) ?? null : null;
  const selectedExtension = selectedId ? extensions.find((item) => item.id === selectedId) ?? null : null;
  const [content, setContent] = React.useState('');
  const [originalContent, setOriginalContent] = React.useState('');
  const [isLoading, setIsLoading] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    if (!selectedExtension) {
      setContent('');
      setOriginalContent('');
      return () => { cancelled = true; };
    }
    setIsLoading(true);
    void readExtension(selectedExtension.id).then((result) => {
      if (cancelled) return;
      const next = result?.content ?? '';
      setContent(next);
      setOriginalContent(next);
      setIsLoading(false);
    });
    return () => { cancelled = true; };
  }, [readExtension, selectedExtension]);
  const appliedAll = useOmpPluginsStore((state) => state.applied);
  if (selectedPlugin) return <PluginDetails plugin={ selectedPlugin } />;

  const appliedForExtension = selectedId
    ? appliedAll.filter((session) => session.extensionIds.includes(selectedId))
    : [];
  if (selectedExtension) {
    const extensionContent = useOmpPluginsStore.getState().extensionContent[selectedExtension.id];
    const editable = extensionContent?.editable ?? selectedExtension.editable;
    const save = async () => {
      if (!editable) return;
      const result = await updateExtension(selectedExtension.id, content);
      if (result.ok) {
        setOriginalContent(content);
        toast.success(t('settings.view.pendingRestart.saved'));
      } else toast.error(t('settings.plugins.sidebar.toast.deleteFailed'));
    };
    return (
      <SettingsPageLayout
        title={t('settings.plugins.page.header.file')}
        titleAccessory={<ScopeBadge scope={selectedExtension.scope} label={selectedExtension.scope === 'project' ? t('settings.plugins.sidebar.group.projectFiles') : t('settings.plugins.sidebar.group.userFiles')} />}
        showSaveStatus={false}
      >
        <SettingsSection title={selectedExtension.name} divider={false}>
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0 space-y-1 typography-meta text-muted-foreground">
              <div>{selectedExtension.source}</div>
              {selectedExtension.pluginName ? <div>{selectedExtension.pluginName}</div> : null}
              {selectedExtension.declaredEntry ? <div className="font-mono">{selectedExtension.declaredEntry}</div> : null}
              {!selectedExtension.loaded ? <div className="text-[var(--status-warning)]">{t('settings.plugins.registry.banner.invalid.pathMissing')}</div> : null}
            </div>
            {selectedExtension.loaded ? (
              <Button variant="outline" size="sm" className="shrink-0" onClick={() => { void revealExtension(selectedExtension.id); }}>
                <Icon name="folder" className="size-3.5" />
                {t('settings.plugins.actions.reveal')}
              </Button>
            ) : null}
          </div>
        </SettingsSection>
        {selectedExtension.loaded ? (
          <SettingsSection title={t('settings.plugins.page.field.content')} settingsItem="plugins.content">
            <Textarea value={content} onChange={(event) => setContent(event.target.value)} rows={16} className="font-mono typography-meta min-h-[320px]" spellCheck={false} disabled={isLoading || !editable} />
            {editable ? (
              <div className="flex items-center gap-2 pt-3">
                <Button size="sm" onClick={() => void save()} disabled={isLoading || isSaving || content === originalContent}>{t('settings.plugins.page.action.save')}</Button>
                <Button variant="outline" size="sm" onClick={() => setContent(originalContent)} disabled={isLoading || isSaving || content === originalContent}>{t('settings.plugins.page.action.discard')}</Button>
              </div>
            ) : null}
          </SettingsSection>
        ) : null}

        <SettingsSection title={t('settings.plugins.page.applied.title')}>
          {appliedForExtension.length === 0 ? (
            <p className="typography-meta text-muted-foreground">{t('settings.plugins.page.applied.empty')}</p>
          ) : (
            <div className="space-y-1">
              {appliedForExtension.map((session) => (
                <div key={session.sessionId} className="typography-meta flex items-center justify-between gap-3 rounded border border-border/60 px-2 py-1.5">
                  <span className="min-w-0 truncate font-mono">{session.sessionId}</span>
                  <span className="shrink-0 text-muted-foreground">
                    {t('settings.plugins.page.applied.since', { time: new Date(session.appliedAt).toLocaleTimeString() })}
                  </span>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="h-6 w-6 shrink-0 text-muted-foreground"
                    onClick={() => { void reloadPlugins(session.sessionId); }}
                    aria-label={t('settings.plugins.page.applied.refreshSession')}
                    title={t('settings.plugins.page.applied.refreshSession')}
                  >
                    <Icon name="restart" className="size-3" />
                  </Button>
                </div>
              ))}
            </div>
          )}
        </SettingsSection>
      </SettingsPageLayout>
    );
  }

  return (
    <div className="flex h-full items-center justify-center">
      <div className="text-center text-muted-foreground">
        <Icon name="plug" className="mx-auto mb-3 size-12 opacity-50" />
        <p className="typography-body">{t('settings.plugins.page.empty.select')}</p>
        <p className="typography-meta mt-1 opacity-75">{t('settings.plugins.page.empty.add')}</p>
      </div>
    </div>
  );
};
