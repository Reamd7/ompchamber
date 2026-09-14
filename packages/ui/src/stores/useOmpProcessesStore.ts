/**
 * useOmpProcessesStore — directory-level omp process rows (processes.v1).
 *
 * `GET /api/omp/processes?directory=` is the authoritative snapshot: one row
 * per tracked invocation plus unattributed roots (the ledger's ownership is
 * best-effort — ambiguous rows stay visible with candidateSessionIds). The
 * cache is keyed by `runtimeKey::directory`, loaded on demand by the
 * WorkStatus processes section, and refreshed when the event pipeline's
 * `processesRevision` for the directory moves.
 *
 * Capability-gated: with `processes.v1` off the store stays empty and the
 * section hides. A failed load records nothing so the next mount retries and
 * consumers keep the previous rows — failure is never authoritative empty.
 */

import { create } from 'zustand';
import { createOmpProcessesAPI, type OmpProcessEntry, type OmpProcessOutput, type OmpProcessKillResult, type OmpProcessesAPI } from '@/lib/api/omp';
import { isOmpFeatureEnabled } from '@/lib/omp/capabilityGate';
import { getRuntimeKey } from '@/lib/runtime-switch';

const processesApi = createOmpProcessesAPI();

const cacheKey = (directory: string | null): string | null => {
  if (!directory) return null;
  return `${getRuntimeKey()}::${directory}`;
};

interface OmpProcessesStore {
  /** `runtimeKey::directory` → snapshot entries. */
  byKey: Record<string, OmpProcessEntry[]>;
  /** Last snapshot revision observed per key. */
  revisions: Record<string, number>;
  /** Idempotent, in-flight-deduped load; `force` re-fetches when the omp event
   * pipeline reported a newer processes revision. */
  load: (directory: string | null, options?: { force?: boolean }) => Promise<void>;
  /** Entries for one directory; null while unloaded/off (callers fall back). */
  entries: (directory: string | null) => OmpProcessEntry[] | null;
  /** Bounded output tail for one invocation key. */
  output: (directory: string | null, key: string) => Promise<OmpProcessOutput | null>;
  /** Kill one process (pid) or the whole entry's live members. */
  kill: (directory: string | null, entry: OmpProcessEntry, pid?: number) => Promise<OmpProcessKillResult | null>;
}

const inFlight = new Map<string, Promise<void>>();

export const useOmpProcessesStore = create<OmpProcessesStore>((set, get) => ({
  byKey: {},
  revisions: {},

  load: async (directory, options) => {
    const key = cacheKey(directory);
    if (!key || !isOmpFeatureEnabled('processes.v1')) return;
    if (!options?.force && (get().byKey[key] || inFlight.has(key))) return;
    const attempt = (async () => {
      // SAFETY: `key` is non-null only when `directory` is.
      const result = await processesApi.list({ directory: directory as string });
      if (result.ok) {
        set((state) => ({
          byKey: { ...state.byKey, [key]: result.data.entries },
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

  entries: (directory) => {
    const key = cacheKey(directory);
    return key ? get().byKey[key] ?? null : null;
  },

  output: async (directory, key) => {
    if (!directory || !isOmpFeatureEnabled('processes.v1')) return null;
    const result = await processesApi.output({ directory, key });
    return result.ok ? result.data : null;
  },

  kill: async (directory, entry, pid) => {
    if (!directory || !isOmpFeatureEnabled('processes.v1')) return null;
    // Unattributed roots carry no single owner — the caller picks any
    // candidate session it is scoped to; the server re-checks membership.
    const sessionID = entry.sessionID ?? entry.candidateSessionIds?.[0];
    if (!sessionID) return null;
    const killArgs: Parameters<OmpProcessesAPI['kill']>[0] = { sessionID, key: entry.key };
    if (pid !== undefined) killArgs.pid = pid;
    const result = await processesApi.kill(killArgs);
    if (result.ok) {
      // The kill marks the ledger dirty → a processes.updated bump refetches;
      // force anyway so the UI reflects the kill even if the event lags.
      await get().load(directory, { force: true });
      return result.data;
    }
    return null;
  },
}));

/** Reactive entries read for components. */
export const useOmpProcessesForDirectory = (directory: string | null): OmpProcessEntry[] | null => {
  const key = cacheKey(directory);
  return useOmpProcessesStore((state) => (key ? state.byKey[key] ?? null : null));
};

/** Reactive snapshot-revision read (the consumer compares it with the omp event pipeline's processesRevision to force refetch). */
export const useOmpProcessesRevision = (directory: string | null): number | undefined => {
  const key = cacheKey(directory);
  return useOmpProcessesStore((state) => (key ? state.revisions[key] : undefined));
};
