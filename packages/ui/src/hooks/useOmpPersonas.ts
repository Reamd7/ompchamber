/**
 * useOmpPersonas — the composer persona selector's read model (spec 02
 * §5.2a, D-B2).
 *
 * Personas are an OC-original optional layer on top-level sessions: the
 * composer chip lists `GET /api/omp/personas` plus the default "Standard"
 * entry (undefined persona). The hook surfaces the authoritative list or a
 * degraded state — a failed fetch NEVER fabricates an empty persona list
 * (the selector would then silently strand a persisted persona selection).
 */

import React from 'react';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import type { OmpPersona } from '@/lib/api/omp';

export interface OmpPersonasState {
  /** True once the feature-on fetch settled (either answer). */
  resolved: boolean;
  /** Trusted persona rows; null while unresolved, loading, degraded, or off. */
  personas: OmpPersona[] | null;
}

export const useOmpPersonas = (personasEnabled: boolean): OmpPersonasState => {
  const { ompPersonas } = useRuntimeAPIs();
  const [personas, setPersonas] = React.useState<OmpPersona[] | null>(null);
  const [resolved, setResolved] = React.useState(false);

  React.useEffect(() => {
    if (!personasEnabled) {
      setPersonas(null);
      setResolved(false);
      return;
    }
    let cancelled = false;
    setPersonas(null);
    setResolved(false);
    void ompPersonas.list().then((result) => {
      if (cancelled) return;
      setResolved(true);
      // A degraded answer (surface absent, transport failure, malformed
      // payload) keeps `personas` null so the selector can render its
      // unavailable state instead of a fake "Standard only" universe.
      setPersonas(result.ok ? result.data : null);
    });
    return () => {
      cancelled = true;
    };
  }, [ompPersonas, personasEnabled]);

  return { resolved, personas };
};
