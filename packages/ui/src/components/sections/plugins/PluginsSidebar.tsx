import React from 'react';
import { useShallow } from 'zustand/react/shallow';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { toast } from '@/components/ui';
import { Icon } from '@/components/icon/Icon';
import { SettingsSidebarLayout } from '@/components/sections/shared/SettingsSidebarLayout';
import { SettingsProjectSelector } from '@/components/sections/shared/SettingsProjectSelector';
import { SettingsSidebarItem } from '@/components/sections/shared/SettingsSidebarItem';
import { SETTINGS_PANEL_TITLE_CLASS } from '@/components/sections/shared/SettingsSection';
import { useI18n } from '@/lib/i18n';
import { useOmpPluginsStore } from '@/stores/useOmpPluginsStore';
import { useProjectsStore } from '@/stores/useProjectsStore';
import type { OmpExtensionRecord, OmpPluginRecord } from '@/lib/api/omp';

interface PluginsSidebarProps {
  onItemSelect?: () => void;
}

type DeleteTarget =
  | { kind: 'plugin'; id: string; label: string }
  | { kind: 'extension'; id: string; label: string }
  | null;

export const PluginsSidebar: React.FC<PluginsSidebarProps> = ({ onItemSelect }) => {
  const { t } = useI18n();
  const {
    plugins,
    extensions,
    selectedId,
    isLoading,
    isSaving,
    setSelected,
    load,
    install,
    removePlugin,
    removeExtension,
    revealPlugin,
    revealExtension,
    reloadPlugins,
  } = useOmpPluginsStore(useShallow((state) => state));
  const activeProjectId = useProjectsStore((state) => state.activeProjectId);
  const [deleteTarget, setDeleteTarget] = React.useState<DeleteTarget>(null);
  const [installOpen, setInstallOpen] = React.useState(false);
  const [installSpec, setInstallSpec] = React.useState('');
  const [installScope, setInstallScope] = React.useState<'user' | 'project'>('user');

  React.useEffect(() => {
    void load();
  }, [activeProjectId, load]);

  React.useEffect(() => {
    const handleOpenAdd = () => setInstallOpen(true);
    window.addEventListener('openchamber:settings-open-plugin-add', handleOpenAdd);
    return () => window.removeEventListener('openchamber:settings-open-plugin-add', handleOpenAdd);
  }, []);

  const select = (id: string) => {
    setSelected(id);
    onItemSelect?.();
  };

  const handleReload = async () => {
    const result = await reloadPlugins();
    if (result.ok) toast.success(t('settings.plugins.toast.reloaded', { count: result.sessionsRefreshed ?? 0 }));
    else toast.error(t('settings.plugins.sidebar.toast.deleteFailed'));
  };
  const pluginActions = (plugin: OmpPluginRecord) => [
    {
      label: t('settings.plugins.actions.reveal'),
      icon: 'folder' as const,
      onClick: () => { void revealPlugin(plugin.id); },
    },
    ...(plugin.editable ? [{
      label: t('settings.common.actions.delete'),
      icon: 'delete-bin' as const,
      destructive: true,
      onClick: () => setDeleteTarget({ kind: 'plugin', id: plugin.id, label: plugin.name }),
    }] : []),
  ];

  const renderPlugin = (plugin: OmpPluginRecord) => (
    <SettingsSidebarItem
      key={plugin.id}
      title={plugin.name}
      metadata={`${plugin.kind === 'marketplace' ? t('settings.plugins.sidebar.kind.marketplace') : t('settings.plugins.sidebar.kind.npm')} · ${plugin.version}`}
      selected={selectedId === plugin.id}
      onSelect={() => select(plugin.id)}
      icon={<Icon name="plug-2" className="size-4 text-muted-foreground/70" />}
      actions={pluginActions(plugin)}
    />
  );

  const renderExtension = (extension: OmpExtensionRecord) => (
    <SettingsSidebarItem
      key={extension.id}
      title={extension.name}
      metadata={extension.pluginName
        ? `${extension.pluginName} · ${extension.declaredEntry ?? extension.name}`
        : `${t('settings.plugins.sidebar.kind.file')} · ${extension.scope === 'project' ? t('settings.plugins.scope.project') : t('settings.plugins.scope.user')}`}
      selected={selectedId === extension.id}
      onSelect={() => select(extension.id)}
      icon={<Icon name="file-code" className="size-4 text-muted-foreground/70" />}
      actions={[{
        label: t('settings.plugins.actions.reveal'),
        icon: 'folder' as const,
        onClick: () => { void revealExtension(extension.id); },
      }, ...(extension.editable ? [{
        label: t('settings.common.actions.delete'),
        icon: 'delete-bin' as const,
        destructive: true,
        onClick: () => setDeleteTarget({ kind: 'extension', id: extension.id, label: extension.name }),
      }] : [])]}
    />
  );

  const group = (label: string, children: React.ReactNode) => (
    <>
      <div className="px-2 pb-1.5 pt-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">{label}</div>
      {children}
    </>
  );

  const userPlugins = plugins.filter((item) => item.scope === 'user');
  const projectPlugins = plugins.filter((item) => item.scope === 'project');
  const userExtensions = extensions.filter((item) => item.scope === 'user');
  const projectExtensions = extensions.filter((item) => item.scope === 'project');
  const total = plugins.length + extensions.length;

  const handleInstall = async () => {
    const spec = installSpec.trim();
    if (!spec) return;
    const result = await install(spec, installScope);
    if (result.ok) {
      setInstallSpec('');
      setInstallOpen(false);
      toast.success(t('settings.view.pendingRestart.saved'));
    } else {
      // Server rejects project scope for non-marketplace specs with a clear
      // message; surface it verbatim so the user knows how to proceed.
      toast.error(result.error || t('settings.plugins.sidebar.toast.deleteFailed'));
    }
  };

  const handleDelete = async () => {
    if (!deleteTarget) return;
    const result = deleteTarget.kind === 'plugin'
      ? await removePlugin(deleteTarget.id)
      : await removeExtension(deleteTarget.id);
    if (result.ok) toast.success(t('settings.plugins.sidebar.toast.deleted', { name: deleteTarget.label }));
    else toast.error(t('settings.plugins.sidebar.toast.deleteFailed'));
    setDeleteTarget(null);
  };

  return (
    <>
      <SettingsSidebarLayout
        variant="background"
        header={(
          <div className="border-b px-3 pb-3 pt-4">
            <div className="mb-3 flex items-center justify-between gap-3">
              <h2 className={SETTINGS_PANEL_TITLE_CLASS}>{t('settings.plugins.sidebar.title')}</h2>
            </div>
            <div className="mb-3">
              <SettingsProjectSelector />
            </div>
            <div className="flex items-center justify-between gap-2">
              <span className="typography-meta text-muted-foreground">{t('settings.plugins.sidebar.total', { count: total })}</span>
              <div className="flex items-center gap-1">
                <Button type="button" variant="ghost" size="icon" className="h-7 w-7 -my-1 text-muted-foreground" onClick={() => void load({ force: true })} disabled={isLoading} aria-label={t('settings.plugins.sidebar.actions.refresh')} title={t('settings.plugins.sidebar.actions.refresh')}>
                  <Icon name="refresh" className={isLoading ? 'size-4 animate-spin' : 'size-4'} />
                </Button>
                <Button type="button" variant="ghost" size="icon" className="h-7 w-7 -my-1 text-muted-foreground" onClick={() => void handleReload()} aria-label={t('settings.plugins.actions.reload')} title={t('settings.plugins.actions.reload')}>
                  <Icon name="restart" className="size-4" />
                </Button>
                <Button type="button" data-settings-item="plugins.create" variant="ghost" size="icon" className="h-7 w-7 -my-1 text-muted-foreground" onClick={() => setInstallOpen(true)} aria-label={t('settings.plugins.sidebar.actions.addTitle')} title={t('settings.plugins.sidebar.actions.addTitle')}>
                  <Icon name="add" className="size-4" />
                </Button>
              </div>
            </div>
          </div>
        )}
      >
        {total === 0 ? (
          <div className="px-4 py-12 text-center text-muted-foreground">
            <Icon name="plug" className="mx-auto mb-3 size-10 opacity-50" />
            <p className="typography-ui-label font-medium">{t('settings.plugins.sidebar.empty.title')}</p>
            <p className="typography-meta mt-1 opacity-75">{t('settings.plugins.sidebar.empty.description')}</p>
          </div>
        ) : (
          <>
            {group(
              t('settings.plugins.sidebar.group.userEntries'),
              userPlugins.length > 0
                ? userPlugins.map(renderPlugin)
                : <p className="px-2 py-2 typography-meta text-muted-foreground">{t('settings.plugins.sidebar.empty.packages')}</p>,
            )}
            {userExtensions.length > 0 && group(t('settings.plugins.sidebar.group.userFiles'), userExtensions.map(renderExtension))}
            {projectPlugins.length > 0 && group(t('settings.plugins.sidebar.group.projectEntries'), projectPlugins.map(renderPlugin))}
            {projectExtensions.length > 0 && group(t('settings.plugins.sidebar.group.projectFiles'), projectExtensions.map(renderExtension))}
          </>
        )}
      </SettingsSidebarLayout>

      <Dialog open={installOpen} onOpenChange={setInstallOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('settings.plugins.dialog.add.title')}</DialogTitle>
            <DialogDescription>{t('settings.plugins.sidebar.empty.description')}</DialogDescription>
          </DialogHeader>
          <div className="flex gap-2">
            <Button type="button" size="sm" variant={installScope === 'user' ? 'default' : 'outline'} onClick={() => setInstallScope('user')}>{t('settings.plugins.scope.user')}</Button>
            <Button type="button" size="sm" variant={installScope === 'project' ? 'default' : 'outline'} onClick={() => setInstallScope('project')}>{t('settings.plugins.scope.project')}</Button>
          </div>
          {installScope === 'project' ? (
            <p className="text-xs text-muted-foreground">{t('settings.plugins.dialog.add.scopeHint')}</p>
          ) : null}
          <DialogFooter>
            <Button variant="ghost" onClick={() => setInstallOpen(false)} disabled={isSaving}>{t('settings.common.actions.cancel')}</Button>
            <Button onClick={() => void handleInstall()} disabled={isSaving || !installSpec.trim()}>{t('settings.plugins.dialog.add.action.submit')}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={deleteTarget !== null} onOpenChange={(open) => { if (!open && !isSaving) setDeleteTarget(null); }}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('settings.plugins.sidebar.deleteDialog.title')}</DialogTitle>
            <DialogDescription>{t('settings.plugins.sidebar.deleteDialog.description', { name: deleteTarget?.label ?? '' })}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setDeleteTarget(null)} disabled={isSaving}>{t('settings.common.actions.cancel')}</Button>
            <Button variant="destructive" onClick={() => void handleDelete()} disabled={isSaving}>{t('settings.common.actions.delete')}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
};
