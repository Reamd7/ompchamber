import React from 'react';
import { toast } from 'sonner';

import type { OmpSetModeResult } from '@/lib/api/omp';

export interface OmpModeSelectorLabels {
  ariaLabel: string;
  title: string;
  requiresSession: string;
  changeFailed: string;
  formatConflict: (modeLabel: string) => string;
}

type OmpSetModeCall = (
  sessionID: string,
  mode: string,
  options: { directory: string },
) => Promise<OmpSetModeResult>;

/**
 * Shared optimism trail for mode transitions, owned by the embedder so the
 * desktop chip, mobile chip, and mobile panel all show the same pending state
 * (see OmpModeSelector's header). Lives in its own module so the selector
 * components stay the only exports of their file.
 */
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
