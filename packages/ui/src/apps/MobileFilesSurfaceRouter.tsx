/**
 * MobileFilesSurfaceRouter — files surface with a workspace/session scope
 * toggle (spec 04 artifacts browse). "Workspace" renders the full desktop
 * FilesView (already mobile-aware); "Session" renders MobileSessionFiles
 * (the active session's local:// files). The toggle rides the same
 * per-directory scope state as the desktop context-panel tree. The toggle
 * itself is NOT capability-gated: a hidden entry can't be discovered, and
 * the session view degrades to its own error state when the server doesn't
 * offer the artifacts capability.
 */
import { useI18n } from '@/lib/i18n';
import { useEffectiveDirectory } from '@/hooks/useEffectiveDirectory';
import { useUIStore } from '@/stores/useUIStore';
import { MobileFilesSurface } from './MobileFilesSurface';
import { MobileSessionFiles } from './MobileSessionFiles';
import { cn } from '@/lib/utils';

const normalizeDirectoryKey = (value: string): string => {
  if (!value) return '';
  return value.replace(/\\/g, '/').replace(/\/+$/g, '').replace(/\/+/g, '/') || '/';
};


export const MobileFilesSurfaceRouter: React.FC = () => {
  const { t } = useI18n();
  const effectiveDirectory = useEffectiveDirectory() ?? '';
  const scope = useUIStore(
    (state) => state.contextTreeScopeByDirectory[normalizeDirectoryKey(effectiveDirectory)] ?? 'workspace',
  );
  const setContextTreeScope = useUIStore((state) => state.setContextTreeScope);

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div
        className="flex h-9 shrink-0 items-center gap-1 border-b border-border/40 bg-background px-2"
        role="tablist"
        aria-label={t('contextPanel.localFiles.scopeAria')}
      >
        <button
          type="button"
          role="tab"
          aria-selected={scope === 'workspace'}
          onClick={() => effectiveDirectory && setContextTreeScope(effectiveDirectory, 'workspace')}
          className={cn(
            'flex-1 truncate typography-meta px-2 py-1 rounded-md text-muted-foreground',
            scope === 'workspace' && 'bg-interactive-selection text-foreground',
          )}
        >
          {t('contextPanel.localFiles.backToFiles')}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={scope === 'session'}
          onClick={() => effectiveDirectory && setContextTreeScope(effectiveDirectory, 'session')}
          className={cn(
            'flex-1 truncate typography-meta px-2 py-1 rounded-md text-muted-foreground',
            scope === 'session' && 'bg-interactive-selection text-foreground',
          )}
        >
          {t('contextPanel.localFiles.title')}
        </button>
      </div>
      <div className="min-h-0 flex-1">
        {scope === 'session' ? (
          <MobileSessionFiles />
        ) : (
          <MobileFilesSurface />
        )}
      </div>
    </div>
  );
};
