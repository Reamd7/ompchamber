/**
 * OmpRoleSlots — the capability-gated model-roles surface for the composer's
 * model picker (spec 01 §5.5 role track, 08 §5.1 GAP-01 input migration).
 *
 * Rendered only when the omp model-roles capability is on AND an
 * authoritative `/api/omp/models` snapshot exists (see useOmpModelRoles).
 * Picking a configured role applies that role's resolved model through the
 * picker's existing model-selection path — there is no new server write
 * here; roles are configured in Settings (the `onConfigure` deep-link), not
 * edited inline.
 *
 * Labels (and the thinking formatter) are injected so the component stays
 * i18n-free, mirroring the ModelPickerList labels contract.
 */
import { Icon } from '@/components/icon/Icon';
import React from 'react';
import type { OmpRoleSlot } from '@/hooks/useOmpModelRoles';
import { cn } from '@/lib/utils';


export interface OmpRoleSlotsLabels {
  sectionTitle: string;
  notConfigured: string;
  configure: string;
  formatThinking: (level: string) => string;
}

interface OmpRoleSlotsProps {
  roles: OmpRoleSlot[];
  selectedModel: { provider: string; id: string } | null;
  onSelect: (slot: OmpRoleSlot) => void;
  onConfigure: () => void;
  labels: OmpRoleSlotsLabels;
}

export const OmpRoleSlots: React.FC<OmpRoleSlotsProps> = ({
  roles,
  selectedModel,
  onSelect,
  onConfigure,
  labels,
}) => {
  if (roles.length === 0) return null;

  return (
    <div className="flex flex-col gap-0.5 p-1 border-b border-border/40" data-testid="omp-role-slots">
      <span className="typography-ui-header px-2 pt-1 pb-1 font-semibold text-foreground">
        {labels.sectionTitle}
      </span>
      {roles.map((slot) => {
        const isSelected = slot.model !== null
          && selectedModel !== null
          && slot.model.provider === selectedModel.provider
          && slot.model.id === selectedModel.id;
        return (
          <button
            key={slot.id}
            type="button"
            disabled={slot.model === null}
            data-role={slot.id}
            onClick={() => {
              if (slot.model !== null) onSelect(slot);
            }}
            className={cn(
              'typography-meta flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left',
              slot.model === null
                ? 'cursor-default opacity-60'
                : 'cursor-pointer hover:bg-interactive-hover/50',
              isSelected && 'bg-interactive-selection/20',
            )}
          >
            <span className="flex min-w-0 flex-1 flex-col gap-0.5">
              <span className="flex items-center gap-1.5">
                <span className="font-medium text-foreground">{slot.name}</span>
                {slot.tag ? (
                  <span className="typography-micro rounded border border-border/50 px-1 text-muted-foreground">
                    {slot.tag}
                  </span>
                ) : null}
              </span>
              {slot.model !== null ? (
                <span className="truncate text-muted-foreground">
                  {slot.model.provider}/{slot.model.id}
                  {slot.model.thinkingLevel !== undefined ? ` · ${labels.formatThinking(slot.model.thinkingLevel)}` : ''}
                </span>
              ) : (
                <span className="text-muted-foreground/70">{labels.notConfigured}</span>
              )}
            </span>
            {isSelected ? <Icon name="check" className="size-4 flex-shrink-0 text-primary" /> : null}
          </button>
        );
      })}
      <button
        type="button"
        onClick={onConfigure}
        className="typography-meta flex w-full cursor-pointer items-center gap-1.5 rounded-md px-2 py-1.5 hover:bg-interactive-hover/50"
      >
        <Icon name="settings-3" className="size-3.5 text-muted-foreground" />
        <span className="font-medium">{labels.configure}</span>
      </button>
    </div>
  );
};
