/**
 * OmpPersonaSelector — the capability-gated persona chip that replaces the
 * composer's legacy agent picker under personas.v1 (spec 02 §5.1 D-B2;
 * master D3: the build/plan dichotomy is deleted, the top-level "persona"
 * is an OC-original optional layer).
 *
 * Selection is session-level explicit switching (D-B3): picking a persona
 * stages it for the next prompt, which carries the persona on the wire
 * agent param — the engine persists it to the session's registry meta and
 * rebuilds the AgentSession over the same transcript. "Standard" is the
 * default entry (undefined persona); choosing it sends the engine's
 * standard sentinel so a persisted persona is explicitly cleared.
 *
 * With modes.v1 also on, the mode chip stays and this list renders inside
 * the mode dropdown (via `OmpPersonaOptionList` as the mode menu's persona
 * section) per the composer's existing layout gates.
 *
 * State is owned by the embedder; this component is presentation-only.
 */

import React from 'react';

import { Icon } from '@/components/icon/Icon';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { cn } from '@/lib/utils';

export interface OmpPersonaOption {
  /** '' = Standard (undefined persona); otherwise the persona name. */
  value: string;
  label: string;
  description?: string;
}

export interface OmpPersonaSelectorLabels {
  ariaLabel: string;
  title: string;
  standard: string;
}

export const OmpPersonaOptionList: React.FC<{
  options: OmpPersonaOption[];
  selected: string;
  onSelect: (value: string) => void;
}> = ({ options, selected, onSelect }) => (
  <div className="flex flex-col gap-0.5 p-1" data-testid="omp-persona-options">
    {options.map((option) => {
      const isSelected = option.value === selected;
      return (
        <button
          key={option.value || '__standard__'}
          type="button"
          data-persona={option.value}
          aria-pressed={isSelected || undefined}
          onClick={() => onSelect(option.value)}
          className={cn(
            'typography-meta flex w-full cursor-pointer items-center justify-between gap-2 rounded-md px-2 py-1.5 text-left hover:bg-interactive-hover/50',
            isSelected && 'bg-interactive-selection/20',
          )}
        >
          <span className="flex min-w-0 flex-col gap-0.5">
            <span className="font-medium text-foreground">{option.label}</span>
            {option.description ? (
              <span className="typography-meta text-muted-foreground truncate">{option.description}</span>
            ) : null}
          </span>
          {isSelected ? <Icon name="check" className="size-4 flex-shrink-0 text-primary" /> : null}
        </button>
      );
    })}
  </div>
);

interface OmpPersonaSelectorProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  options: OmpPersonaOption[];
  /** Current persona name; '' = Standard. */
  selected: string;
  labels: OmpPersonaSelectorLabels;
  onSelect: (value: string) => void;
  sizeVariant?: { icon: string; text: string; height: string };
}

/** Desktop inline chip + dropdown; state is owned by the embedder. */
export const OmpPersonaSelector: React.FC<OmpPersonaSelectorProps> = ({
  open,
  onOpenChange,
  options,
  selected,
  labels,
  onSelect,
  sizeVariant,
}) => {
  const selectedOption = options.find((option) => option.value === selected)
    ?? options.find((option) => option.value === '')
    ?? options[0];
  const iconClass = sizeVariant?.icon ?? 'size-4';
  const textClass = sizeVariant?.text ?? 'typography-meta';
  const heightClass = sizeVariant?.height ?? 'h-8';

  return (
    <div className="flex items-center gap-2 min-w-0" data-testid="omp-persona-selector">
      <DropdownMenu open={open} onOpenChange={onOpenChange}>
        <DropdownMenuTrigger asChild>
          <div
            role="button"
            tabIndex={0}
            aria-label={labels.ariaLabel}
            className={cn(
              'model-controls__persona-trigger flex cursor-pointer items-center gap-1.5 transition-colors min-w-0 hover:bg-transparent hover:opacity-70',
              heightClass,
            )}
          >
            <Icon name="user-3" className={cn(iconClass, 'flex-shrink-0 text-muted-foreground')} />
            <span className={cn('model-controls__persona-label font-medium min-w-0 truncate', textClass)}>
              {selectedOption?.label ?? (selected || labels.standard)}
            </span>
          </div>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" alignOffset={-40} className="w-[min(240px,calc(100vw-2rem))] p-0 flex flex-col">
          <div className="typography-ui-header px-3 pt-2 pb-1 font-semibold text-foreground">{labels.title}</div>
          <OmpPersonaOptionList options={options} selected={selected} onSelect={onSelect} />
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
};
