/**
 * OmpModeSelector — the capability-gated session-mode chip that replaces the
 * composer's legacy build/plan agent picker (master D3 row 1: the agent
 * build/plan dichotomy is deleted in favor of session modes; spec 02 §5.4,
 * 08 §5.2).
 *
 * Rendered only when `modes.v1 && modelRoles.v1` capabilities are on. Mode
 * transitions POST `/api/omp/sessions/{id}/mode`; the omp event stream
 * (`omp.mode.changed`) is the live authority for the chip's display, with
 * the POST response covering the gap until the event lands. A 409
 * mode-conflict tells the user which active mode must be exited first
 * (domain-modes.js `modeConflict`), mirroring the TUI's behavior.
 *
 * With no session yet (new-session draft) non-default options are disabled:
 * there is no session to switch, and faking success is not an option.
 *
 * The transition state (`useOmpModeTransition`) is owned by the embedder so
 * the desktop chip, mobile chip, and mobile panel share one optimism trail.
 */

import React from 'react';
import { toast } from 'sonner';

import { Icon } from '@/components/icon/Icon';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import type { OmpSetModeResult } from '@/lib/api/omp';
import { cn } from '@/lib/utils';

export interface OmpModeOption {
  /** Wire value: 'none' (standard) | 'plan' | 'goal' | 'vibe'. */
  value: string;
  label: string;
}

export interface OmpModeSelectorLabels {
  ariaLabel: string;
  title: string;
  requiresSession: string;
  changeFailed: string;
  formatConflict: (modeLabel: string) => string;
}

export type OmpSetModeCall = (
  sessionID: string,
  mode: string,
  options: { directory: string },
) => Promise<OmpSetModeResult>;

export const useOmpModeTransition = ({
  mode,
  sessionID,
  directory,
  setMode,
  labelForMode,
  labels,
  onSettled,
}: {
  mode: string | null;
  sessionID: string | null;
  directory: string | null;
  setMode: OmpSetModeCall;
  labelForMode: (mode: string) => string;
  labels: OmpModeSelectorLabels;
  onSettled?: () => void;
}) => {
  const [pending, setPending] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  // A store answer matching the pending value means the event landed.
  React.useEffect(() => {
    if (pending !== null && mode === pending) {
      setPending(null);
    }
  }, [mode, pending]);

  // Switching sessions invalidates any in-flight optimism.
  React.useEffect(() => {
    setPending(null);
  }, [sessionID]);

  const selectMode = React.useCallback(async (value: string) => {
    if (busy || !sessionID || !directory) return;
    if (value === (pending ?? mode ?? 'none')) return;
    setBusy(true);
    setPending(value);
    let result: OmpSetModeResult;
    try {
      result = await setMode(sessionID, value, { directory });
    } catch {
      result = { ok: false, unavailable: false };
    }
    setBusy(false);
    if (result.ok) {
      // Keep `pending` as the display until omp.mode.changed confirms.
      onSettled?.();
      return;
    }
    setPending(null);
    if (!result.unavailable && result.conflict !== undefined) {
      toast.error(labels.formatConflict(labelForMode(result.conflict)));
      return;
    }
    toast.error(labels.changeFailed);
  }, [busy, directory, labelForMode, labels, mode, onSettled, pending, sessionID, setMode]);

  return { pending, busy, selectMode };
};

export const OmpModeOptionList: React.FC<{
  mode: string | null;
  pending: string | null;
  busy: boolean;
  sessionID: string | null;
  options: OmpModeOption[];
  labels: OmpModeSelectorLabels;
  onSelect: (value: string) => void;
}> = ({ mode, pending, busy, sessionID, options, labels, onSelect }) => {
  const effective = pending ?? mode ?? 'none';
  return (
    <div className="flex flex-col gap-0.5 p-1" data-testid="omp-mode-options">
      {options.map((option) => {
        const selected = option.value === effective;
        const disabled = busy || (!sessionID && option.value !== 'none' && option.value !== effective);
        return (
          <button
            key={option.value}
            type="button"
            data-mode={option.value}
            disabled={disabled}
            aria-disabled={disabled || undefined}
            title={disabled && !sessionID ? labels.requiresSession : undefined}
            onClick={() => onSelect(option.value)}
            className={cn(
              'typography-meta flex w-full items-center justify-between gap-2 rounded-md px-2 py-1.5 text-left',
              disabled ? 'cursor-default opacity-55' : 'cursor-pointer hover:bg-interactive-hover/50',
              selected && 'bg-interactive-selection/20',
            )}
          >
            <span className="font-medium text-foreground">{option.label}</span>
            {selected ? <Icon name="check" className="size-4 flex-shrink-0 text-primary" /> : null}
          </button>
        );
      })}
      {!sessionID ? (
        <span className="typography-micro px-2 pb-1 text-muted-foreground">{labels.requiresSession}</span>
      ) : null}
    </div>
  );
};

interface OmpModeSelectorProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: string | null;
  pending: string | null;
  busy: boolean;
  sessionID: string | null;
  options: OmpModeOption[];
  labelForMode: (mode: string) => string;
  labels: OmpModeSelectorLabels;
  onSelect: (value: string) => void;
  sizeVariant?: { icon: string; text: string; height: string };
}

/** Desktop inline chip + dropdown; state is owned by the embedder. */
export const OmpModeSelector: React.FC<OmpModeSelectorProps> = ({
  open,
  onOpenChange,
  mode,
  pending,
  busy,
  sessionID,
  options,
  labelForMode,
  labels,
  onSelect,
  sizeVariant,
}) => {
  const effective = pending ?? mode ?? 'none';
  const iconClass = sizeVariant?.icon ?? 'size-4';
  const textClass = sizeVariant?.text ?? 'typography-meta';
  const heightClass = sizeVariant?.height ?? 'h-8';

  return (
    <div className="flex items-center gap-2 min-w-0" data-testid="omp-mode-selector">
      <DropdownMenu open={open} onOpenChange={onOpenChange}>
        <DropdownMenuTrigger asChild>
          <div
            role="button"
            tabIndex={0}
            aria-label={labels.ariaLabel}
            className={cn(
              'model-controls__mode-trigger flex cursor-pointer items-center gap-1.5 transition-colors min-w-0 hover:bg-transparent hover:opacity-70',
              heightClass,
            )}
          >
            <Icon name="target" className={cn(iconClass, 'flex-shrink-0 text-muted-foreground')} />
            <span className={cn('model-controls__mode-label font-medium min-w-0 truncate', textClass)}>
              {labelForMode(effective)}
            </span>
          </div>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" alignOffset={-40} className="w-[min(200px,calc(100vw-2rem))] p-0 flex flex-col">
          <div className="typography-ui-header px-3 pt-2 pb-1 font-semibold text-foreground">{labels.title}</div>
          <OmpModeOptionList
            mode={mode}
            pending={pending}
            busy={busy}
            sessionID={sessionID}
            options={options}
            labels={labels}
            onSelect={onSelect}
          />
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
};
