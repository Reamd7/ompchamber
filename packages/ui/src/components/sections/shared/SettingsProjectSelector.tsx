import React from 'react';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Icon } from "@/components/icon/Icon";
import { useProjectsStore } from '@/stores/useProjectsStore';
import { isVSCodeRuntime } from '@/lib/desktop';
import { cn } from '@/lib/utils';
import { useI18n } from '@/lib/i18n';

const formatProjectLabel = (label: string): string => label.trim();

/**
 * Compact project switcher for settings sidebars: a single ghost icon button
 * that opens the project menu. The active project is shown in the menu (radio
 * selection) and the button tooltip — never as persistent chrome, so the
 * sidebar header stays quiet until the user asks to switch directories.
 */
export const SettingsProjectSelector: React.FC<{ className?: string }> = ({ className }) => {
  const { t } = useI18n();
  const projects = useProjectsStore((state) => state.projects);
  const activeProjectId = useProjectsStore((state) => state.activeProjectId);
  const setActiveProject = useProjectsStore((state) => state.setActiveProject);

  const [menuOpen, setMenuOpen] = React.useState(false);
  const isVSCode = React.useMemo(() => isVSCodeRuntime(), []);
  const sortedProjects = React.useMemo(() => {
    return [...projects].sort((a, b) => (a.label || a.path).localeCompare(b.label || b.path));
  }, [projects]);

  const activeProject = React.useMemo(() => {
    if (sortedProjects.length === 0) {
      return null;
    }
    return sortedProjects.find((p) => p.id === activeProjectId) ?? sortedProjects[0];
  }, [activeProjectId, sortedProjects]);

  if (isVSCode || sortedProjects.length === 0) {
    return null;
  }

  const rawLabel = activeProject?.label && activeProject.label.trim().length > 0
    ? activeProject.label
    : (activeProject?.path.split('/').filter(Boolean).pop() || activeProject?.path || t('settings.shared.projectSelector.fallbackProject'));
  const label = formatProjectLabel(rawLabel);

  return (
    <div className={cn('flex items-center', className)}>
      <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={t('settings.shared.projectSelector.switchProjectAria')}
            title={`${t('settings.shared.projectSelector.switchProjectTitle')} — ${label}`}
            className="text-foreground border border-border/80 appearance-none flex h-8 w-full min-w-0 rounded-lg bg-transparent px-3 py-1 outline-none hover:border-input focus-visible:ring-1 focus-visible:ring-primary/50 focus-visible:border-primary/70 items-center gap-1.5 text-left"
          >
            <Icon name="folder" className="size-4 shrink-0 opacity-70" />
            <span className="min-w-0 flex-1 truncate typography-ui-label font-medium">{label}</span>
            <Icon name="arrow-down-s" className="size-4 shrink-0 opacity-50" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-auto">
          <DropdownMenuRadioGroup
            value={activeProject?.id ?? ''}
            onValueChange={(value) => {
              if (!value) return;
              setActiveProject(value);
              setMenuOpen(false);
            }}
          >
            {sortedProjects.map((project) => {
              const raw = project.label?.trim()
                ? project.label.trim()
                : (project.path.split('/').filter(Boolean).pop() || project.path);
              const itemLabel = formatProjectLabel(raw);
              return (
                <DropdownMenuRadioItem key={project.id} value={project.id}>
                  <span className="min-w-0 truncate typography-ui">{itemLabel}</span>
                </DropdownMenuRadioItem>
              );
            })}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
};
