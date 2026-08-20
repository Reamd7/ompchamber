/**
 * useOmpSessionMode — the composer mode chip's read model (spec 02 §5.4).
 *
 * The event stream (`omp.mode.changed` via useOmpSessionStore) is the live
 * authority. When the store has no answer yet — fresh mount, idle session,
 * events not yet replayed — one authoritative `GET /api/omp/sessions/{id}/mode`
 * seeds the chip. A failed or absent seed reads as no answer (null → chip
 * shows the default mode label); it never fabricates a mode.
 */

import React from 'react';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpModeState } from '@/sync/useOmpSessionStore';

export const useOmpSessionMode = (
  directory: string | null | undefined,
  sessionID: string | null | undefined,
  modesEnabled: boolean,
): string | null => {
  const storeMode = useOmpModeState(directory ?? '', sessionID ?? undefined)?.mode ?? null;
  const { ompModes } = useRuntimeAPIs();
  const [seededMode, setSeededMode] = React.useState<string | null>(null);
  const seededKeyRef = React.useRef<string | null>(null);

  const directoryKey = directory ?? null;
  const activeSessionID = sessionID ?? null;
  const seedKey = modesEnabled && activeSessionID !== null && storeMode === null
    ? `${directoryKey ?? ''}::${activeSessionID}`
    : null;

  React.useEffect(() => {
    if (seedKey === null) {
      setSeededMode(null);
      return;
    }
    if (seededKeyRef.current === seedKey) return;
    if (!directoryKey || activeSessionID === null) return;
    seededKeyRef.current = seedKey;
    let cancelled = false;
    void ompModes.getMode(activeSessionID, { directory: directoryKey }).then((result) => {
      if (cancelled) return;
      setSeededMode(result.ok ? result.data.mode : null);
    });
    return () => {
      cancelled = true;
    };
  }, [ompModes, seedKey, activeSessionID, directoryKey]);
  // A real store answer always outranks the seed.
  return storeMode ?? seededMode;
};
