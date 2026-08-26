/**
 * useOmpCommandsStore — the omp slash-command discovery cache (spec 08 §5.4).
 *
 * Tier A/B omp commands come from `GET /api/omp/commands?directory=` and feed
 * the three-layer composer pipeline: omp commands → project/user custom
 * commands → OMPChamber magic prompts. The cache is keyed by
 * `runtimeKey:directory` (command sets are directory-scoped and must not
 * survive a runtime switch), filled on demand by the autocomplete, and read
 * synchronously by the submit path (`routeMessage` decides command-channel vs
 * prompt without awaiting anything).
 *
 * Capability-gated like every omp surface (master D6-R2): with `commands.v1`
 * off the store stays empty and `isEngineCommand` answers false, so submission
 * keeps the legacy two-source resolution (skills + OC commands).
 *
 * Failure is never authoritative empty success: a failed load records nothing,
 * so the next mount retries, and callers keep seeing the previous rows.
 */

import { create } from 'zustand';
import { createOmpCommandsAPI, type OmpCommandRecord } from '@/lib/api/omp';
import { isOmpFeatureEnabled } from '@/lib/omp/capabilityGate';
import { getRuntimeKey } from '@/lib/runtime-switch';

const commandsApi = createOmpCommandsAPI();

const cacheKey = (directory: string | null): string | null => {
  if (!directory) return null;
  return `${getRuntimeKey()}::${directory}`;
};

interface OmpCommandsStore {
  /** `runtimeKey::directory` → records. */
  byKey: Record<string, OmpCommandRecord[]>;
  /**
   * Idempotent, in-flight-deduped load for one directory. No-op when the
   * capability is off, the directory is unknown, or the key already settled.
   */
  load: (directory: string | null) => Promise<void>;
  /** Exact-name lookup; null when the surface is off or not loaded. */
  lookup: (name: string, directory: string | null) => OmpCommandRecord | null;
  /** True when the ENGINE expands this name (Tier B — command-channel route). */
  isEngineCommand: (name: string, directory: string | null) => boolean;
  /** True when omp reserves this name in any tier (collision/rename checks). */
  hasCommand: (name: string, directory: string | null) => boolean;
}

const inFlight = new Map<string, Promise<void>>();

export const useOmpCommandsStore = create<OmpCommandsStore>((set, get) => ({
  byKey: {},

  load: async (directory) => {
    const key = cacheKey(directory);
    if (!key || !isOmpFeatureEnabled('commands.v1')) return;
    if (get().byKey[key] || inFlight.has(key)) return;
    const attempt = (async () => {
      const result = await commandsApi.getCommands({ directory: directory as string });
      if (result.ok) {
        // A complete authoritative list (even a short one) settles the key.
        set((state) => ({ byKey: { ...state.byKey, [key]: result.data } }));
      }
      // Failure records nothing: the key stays unset and the next mount
      // retries — never a fake "no omp commands" answer.
    })().finally(() => {
      inFlight.delete(key);
    });
    inFlight.set(key, attempt);
    await attempt;
  },

  lookup: (name, directory) => {
    const key = cacheKey(directory);
    if (!key) return null;
    const rows = get().byKey[key];
    if (!rows) return null;
    return rows.find((record) => record.name === name) ?? null;
  },

  isEngineCommand: (name, directory) => get().lookup(name, directory)?.tier === 'engine',

  hasCommand: (name, directory) => get().lookup(name, directory) !== null,
}));

/** Records for one directory, or null while unloaded/off. Reactive read for components. */
export const useOmpCommandsForDirectory = (directory: string | null): OmpCommandRecord[] | null => {
  const key = cacheKey(directory);
  return useOmpCommandsStore((state) => (key ? state.byKey[key] ?? null : null));
};
