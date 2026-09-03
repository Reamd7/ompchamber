/**
 * useOmpAgentRunsStore — directory-level omp subagent rows (spec 04 §5.5.1).
 *
 * `GET /api/omp/agent-runs?directory=` is the authoritative snapshot (one row
 * per sessionID::agentId, parked/historical included). The cache is keyed by
 * `runtimeKey::directory` (rows must not survive a runtime switch), loaded on
 * demand by the WorkStatus subagents section, and refreshed when the omp
 * event pipeline's `agentsRevision` for the directory moves (each
 * `omp.agents.updated` snapshot carries a monotonic revision).
 *
 * Capability-gated (master D6-R2): with `agentRuns.v1` off the store stays
 * empty and callers keep the legacy child-session list. A failed load records
 * nothing so the next mount retries and consumers keep the previous rows —
 * failure is never authoritative empty success.
 */

import React from 'react';
import { create } from 'zustand';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { useOmpSessionStore } from '@/sync/useOmpSessionStore';
import { createOmpAgentRunsAPI, type OmpAgentRunRecord } from '@/lib/api/omp';
import { isOmpFeatureEnabled } from '@/lib/omp/capabilityGate';
import { getRuntimeKey } from '@/lib/runtime-switch';

const agentRunsApi = createOmpAgentRunsAPI();

const cacheKey = (directory: string | null): string | null => {
  if (!directory) return null;
  return `${getRuntimeKey()}::${directory}`;
};

interface OmpAgentRunsStore {
  /** `runtimeKey::directory` → rows. */
  byKey: Record<string, OmpAgentRunRecord[]>;
  /** Last revision observed per key (from the snapshot, not the event bus). */
  revisions: Record<string, number>;
  /**
   * Idempotent, in-flight-deduped load for one directory. `force` re-fetches
   * when the omp event pipeline reported a newer agents revision.
   */
  load: (directory: string | null, options?: { force?: boolean }) => Promise<void>;
  /** Rows for one directory; null while unloaded/off (callers fall back). */
  rows: (directory: string | null) => OmpAgentRunRecord[] | null;
}

const inFlight = new Map<string, Promise<void>>();

export const useOmpAgentRunsStore = create<OmpAgentRunsStore>((set, get) => ({
  byKey: {},
  revisions: {},

  load: async (directory, options) => {
    const key = cacheKey(directory);
    if (!key || !isOmpFeatureEnabled('agentRuns.v1')) return;
    if (!options?.force && (get().byKey[key] || inFlight.has(key))) return;
    const attempt = (async () => {
      const result = await agentRunsApi.list({ directory: directory as string });
      if (result.ok) {
        set((state) => ({
          byKey: { ...state.byKey, [key]: result.data.agentRuns },
          revisions: { ...state.revisions, [key]: result.data.revision },
        }));
      }
      // Failure records nothing: the key stays unset (or keeps its previous
      // rows) and the next mount or revision bump retries.
    })().finally(() => {
      inFlight.delete(key);
    });
    inFlight.set(key, attempt);
    await attempt;
  },

  rows: (directory) => {
    const key = cacheKey(directory);
    return key ? get().byKey[key] ?? null : null;
  },
}));

/** Reactive rows read for components. */
export const useOmpAgentRunsForDirectory = (directory: string | null): OmpAgentRunRecord[] | null => {
  const key = cacheKey(directory);
  return useOmpAgentRunsStore((state) => (key ? state.byKey[key] ?? null : null));
};

/** Reactive snapshot-revision read (the consumer compares it with the omp event pipeline's agentsRevision to force refetch). */
export const useOmpAgentRunsRevision = (directory: string | null): number | undefined => {
  const key = cacheKey(directory);
  return useOmpAgentRunsStore((state) => (key ? state.revisions[key] : undefined));
};

/**
 * Header-badge read: ensures the directory snapshot is loaded (stale-revision
 * refetch, same protocol as the work-status section) and counts running rows.
 * Zero when agentRuns.v1 is off — callers hide the badge on zero.
 */
export const useOmpAgentRunsBusyCount = (directory: string | null): number => {
  const enabled = useOmpFeatureEnabled('agentRuns.v1');
  const agentRuns = useOmpAgentRunsForDirectory(directory);
  const snapshotRevision = useOmpAgentRunsRevision(directory);
  const eventRevision = useOmpSessionStore(
    React.useCallback(
      (state) => (directory ? state.directories[directory]?.domains.agentsRevision : undefined),
      [directory],
    ),
  );
  const load = useOmpAgentRunsStore((s) => s.load);
  React.useEffect(() => {
    if (!enabled) return;
    void load(directory, {
      force: eventRevision !== undefined && snapshotRevision !== undefined && eventRevision > snapshotRevision,
    });
  }, [enabled, load, directory, eventRevision, snapshotRevision]);
  if (!enabled) return 0;
  return (agentRuns ?? []).filter((row) => row.status === 'running').length;
};
