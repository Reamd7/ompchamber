/**
 * useOmpSessionStore — per-directory omp stream-domain state (spec
 * docs/omp-parity/05 §5.2.2; skill sync-state-invariants).
 *
 * One zustand store whose slices are keyed by directory (the omp bus routes
 * envelopes by directory key, same normalization as the wire /event stream).
 * The reducer (`omp-event-reducer.ts`) owns every transition; the store owns
 * directory lifecycle:
 *   - slices are created lazily by the pipeline when a directory's first
 *     envelope arrives
 *   - `clearDirectory` runs from the sync child-store dispose hook so an
 *     evicted directory never leaks overlay state
 *   - `clearAll(runtimeKey)` runs when the pipeline mounts — it both adopts
 *     the runtime identity and drops slices from any previous runtime
 *   - `clearSession` runs from wire session.deleted handling
 *   - `settleSession` runs from wire session.idle: volatile loaders and the
 *     awaiting-async marker self-heal on the authoritative wire terminal
 *     state (volatile omp events are not replayed after a gap)
 *   - volatile entries (loaders/ttsr/awaitingAsync) additionally carry a TTL
 *     sweeper so a missed terminal frame cannot pin a loader forever
 *
 * Consumers use leaf selectors (`useOmpRetrySupersession`, …), never the
 * whole `directories` map (store DOCUMENTATION selector rules).
 */

import { create } from 'zustand';
import { applyOmpEvent, createEmptyOmpDirectoryState, type OmpDirectoryState, type OmpEventEffect, type OmpSessionLoaders } from './omp-event-reducer';
import type { OmpChromeSnapshot, OmpEventEnvelope } from '@/lib/api/omp';

/** Volatile TTLs — missed terminal frames must not pin state forever. */
export const OMP_COMPACTION_LOADER_TTL_MS = 10 * 60 * 1000;
export const OMP_RETRY_LOADER_TTL_MS = 5 * 60 * 1000;
export const OMP_TTSR_TTL_MS = 2 * 60 * 1000;
export const OMP_AWAITING_ASYNC_TTL_MS = 10 * 60 * 1000;

interface OmpSessionStoreState {
  directories: Record<string, OmpDirectoryState>;
  /** Runtime identity guard — slices from another runtime are never valid. */
  runtimeKey: string;
}

interface OmpSessionStoreActions {
  /**
   * Applies one envelope to its directory slice. Returns the effects the
   * pipeline must execute (notices, revision refetches) — the store never
   * performs I/O. Frames from a different runtime than the adopted one are
   * ignored.
   */
  applyEvent: (runtimeKey: string, directory: string, envelope: OmpEventEnvelope) => OmpEventEffect[];
  /** Wire session.idle reached — clear volatile per-session state. */
  settleSession: (runtimeKey: string, directory: string, sessionID: string) => void;
  seedSessionModel: (
    runtimeKey: string,
    directory: string,
    sessionID: string,
    model: { provider: string; id: string },
  ) => void;
  /** Wire session.deleted reached — drop every trace of the session. */
  clearSession: (runtimeKey: string, directory: string, sessionID: string) => void;
  /** Directory store disposed/evicted — drop the slice. */
  clearDirectory: (runtimeKey: string, directory: string) => void;
  /**
   * Authoritative chrome snapshot reconcile (spec 09 §5.0, resync matrix):
   * a complete parsed snapshot replaces the chrome slice for its directory
   * (first-load replacement is the snapshot contract; D2 lives in the
   * caller — failure never reaches here).
   */
  reconcileChromeSnapshot: (
    runtimeKey: string,
    directory: string,
    snapshot: OmpChromeSnapshot,
  ) => void;
  /** Pipeline (re)mount — adopt the runtime identity and drop stale slices. */
  clearAll: (runtimeKey: string) => void;
}

export type OmpSessionStore = OmpSessionStoreState & OmpSessionStoreActions;

const sweepVolatile = (state: OmpDirectoryState, now: number): boolean => {
  let mutated = false;
  for (const [sessionID, loaders] of Object.entries(state.loaders)) {
    const retryStale = loaders.retry !== undefined && now - loaders.retry.startedAt > OMP_RETRY_LOADER_TTL_MS;
    const compactionStale = loaders.compaction !== undefined && now - loaders.compaction.startedAt > OMP_COMPACTION_LOADER_TTL_MS;
    if (retryStale || compactionStale) {
      const kept: OmpSessionLoaders = {};
      if (!retryStale && loaders.retry !== undefined) kept.retry = loaders.retry;
      if (!compactionStale && loaders.compaction !== undefined) kept.compaction = loaders.compaction;
      const nextLoaders = { ...state.loaders };
      delete nextLoaders[sessionID];
      if (kept.retry !== undefined || kept.compaction !== undefined) {
        nextLoaders[sessionID] = kept;
      }
      state.loaders = nextLoaders;
      mutated = true;
    }
  }
  for (const [sessionID, warning] of Object.entries(state.ttsr)) {
    if (now - warning.raisedAt > OMP_TTSR_TTL_MS) {
      const nextTtsr = { ...state.ttsr };
      delete nextTtsr[sessionID];
      state.ttsr = nextTtsr;
      mutated = true;
    }
  }
  for (const [sessionID, marker] of Object.entries(state.awaitingAsync)) {
    if (now - marker.since > OMP_AWAITING_ASYNC_TTL_MS) {
      const nextAwaiting = { ...state.awaitingAsync };
      delete nextAwaiting[sessionID];
      state.awaitingAsync = nextAwaiting;
      mutated = true;
    }
  }
  return mutated;
};

/** Exposed for deterministic TTL tests; production reaches it via the coalesced sweeper. */
export const sweepOmpVolatile = (state: OmpDirectoryState, now: number): boolean => sweepVolatile(state, now);
const EMPTY_CHROME: OmpDirectoryState['chrome'] = { widgets: {}, status: {} };

const chromeEntriesEqual = (
  a: Record<string, unknown>,
  b: Record<string, unknown>,
): boolean => {
  const aKeys = Object.keys(a);
  if (aKeys.length !== Object.keys(b).length) return false;
  for (const key of aKeys) {
    if (!(key in b)) return false;
    // Records are tiny (string rows + scalars); field-wise JSON keeps the
    // no-op check exact without building a deep-equal utility.
    if (JSON.stringify(a[key]) !== JSON.stringify(b[key])) return false;
  }
  return true;
};

export const useOmpSessionStore = create<OmpSessionStore>((set, get) => {
  // One deferred sweeper pass per TTL window while events keep arriving;
  // coalesced so a burst scans once, not once per event.
  let sweepScheduled = false;
  const scheduleSweep = (): void => {
    if (sweepScheduled) return;
    sweepScheduled = true;
    setTimeout(() => {
      sweepScheduled = false;
      const now = Date.now();
      set((state) => {
        const nextDirectories: Record<string, OmpDirectoryState> = {};
        let mutated = false;
        for (const [directory, slice] of Object.entries(state.directories)) {
          const draft = {
            ...slice,
            loaders: { ...slice.loaders },
            ttsr: { ...slice.ttsr },
            awaitingAsync: { ...slice.awaitingAsync },
          };
          if (sweepVolatile(draft, now)) {
            mutated = true;
            nextDirectories[directory] = draft;
          } else {
            nextDirectories[directory] = slice;
          }
        }
        return mutated ? { directories: nextDirectories } : state;
      });
    }, Math.min(OMP_RETRY_LOADER_TTL_MS, OMP_TTSR_TTL_MS));
  };

  return {
    directories: {},
    runtimeKey: '',

    applyEvent(runtimeKey, directory, envelope) {
      if (runtimeKey !== get().runtimeKey) return [];
      const effects: OmpEventEffect[] = [];
      set((state) => {
        // Empty-shape base then overlay: a legacy partial slice (pre-fix
        // seedSessionModel could write one) must not leak missing maps into
        // the draft the reducer indexes.
        const existing = { ...createEmptyOmpDirectoryState(), ...state.directories[directory] };
        const draft: OmpDirectoryState = {
          ...existing,
          loaders: { ...existing.loaders },
          superseded: { ...existing.superseded },
          notes: { ...existing.notes },
          customDetails: { ...existing.customDetails },
          sessionModel: { ...existing.sessionModel },
          fallback: { ...existing.fallback },
          mode: { ...existing.mode },
          goal: { ...existing.goal },
          thinking: { ...existing.thinking },
          retryTerminal: { ...existing.retryTerminal },
          awaitingAsync: { ...existing.awaitingAsync },
          ttsr: { ...existing.ttsr },
          telemetry: { ...existing.telemetry },
          chrome: {
            widgets: { ...existing.chrome.widgets },
            status: { ...existing.chrome.status },
          },
          domains: {
            ...existing.domains,
            queueVersionBySession: { ...existing.domains.queueVersionBySession },
          },
        };
        const outcome = applyOmpEvent(draft, envelope);
        effects.push(...outcome.effects);
        // Commit when state changed OR the id-gate high-water mark advanced
        // (no-op and unknown frames still mark the envelope as consumed).
        if (!outcome.changed && draft.lastAppliedEventId === existing.lastAppliedEventId) {
          return state;
        }
        return {
          directories: {
            ...state.directories,
            [directory]: draft,
          },
        };
      });
      scheduleSweep();
      return effects;
    },

    /**
     * Seeds the session-model badge from the wire `Session.model` projection
     * (spec 01 §5.5: initial value comes from the wire, events refresh it).
     * Authoritative omp.model.changed events still win when they arrive.
     */
    seedSessionModel(runtimeKey, directory, sessionID, model) {
      if (runtimeKey !== get().runtimeKey) return;
      if (!model?.provider || !model?.id) return;
      set((state) => {
        // Never seed a partial slice: empty-shape base + overlay gives the
        // full maps even when the directory has no slice (or a legacy
        // partial one) — leaf selectors index these maps by message id.
        const slice = { ...createEmptyOmpDirectoryState(), ...state.directories[directory] };
        const existing = slice.sessionModel[sessionID];
        if (existing && existing.provider === model.provider && existing.id === model.id) return state;
        return {
          directories: {
            ...state.directories,
            [directory]: {
              ...slice,
              sessionModel: {
                ...slice.sessionModel,
                [sessionID]: {
                  provider: model.provider,
                  id: model.id,
                  ...(existing?.thinkingLevel ? { thinkingLevel: existing.thinkingLevel } : {}),
                  ...(existing?.role ? { role: existing.role } : {}),
                  updatedAt: Date.now(),
                },
              },
            },
          },
        };
      });
    },

    settleSession(runtimeKey, directory, sessionID) {
      if (runtimeKey !== get().runtimeKey) return;
      set((state) => {
        const slice = state.directories[directory];
        if (!slice) return state;
        const loaders = slice.loaders[sessionID];
        const awaiting = slice.awaitingAsync[sessionID];
        if (!loaders && !awaiting) return state;
        const nextLoaders = { ...slice.loaders };
        delete nextLoaders[sessionID];
        const nextAwaiting = { ...slice.awaitingAsync };
        delete nextAwaiting[sessionID];
        return {
          directories: {
            ...state.directories,
            [directory]: { ...slice, loaders: nextLoaders, awaitingAsync: nextAwaiting },
          },
        };
      });
    },

    clearSession(runtimeKey, directory, sessionID) {
      if (runtimeKey !== get().runtimeKey) return;
      set((state) => {
        const slice = state.directories[directory];
        if (!slice) return state;
        const strip = <T>(map: Record<string, T>): Record<string, T> => {
          if (!(sessionID in map)) return map;
          const next = { ...map };
          delete next[sessionID];
          return next;
        };
        const nextQueueVersions = { ...slice.domains.queueVersionBySession };
        delete nextQueueVersions[sessionID];
        return {
          directories: {
            ...state.directories,
            [directory]: {
              ...slice,
              loaders: strip(slice.loaders),
              sessionModel: strip(slice.sessionModel),
              fallback: strip(slice.fallback),
              mode: strip(slice.mode),
              goal: strip(slice.goal),
              thinking: strip(slice.thinking),
              retryTerminal: strip(slice.retryTerminal),
              awaitingAsync: strip(slice.awaitingAsync),
              ttsr: strip(slice.ttsr),
              telemetry: strip(slice.telemetry),
              domains: { ...slice.domains, queueVersionBySession: nextQueueVersions },
            },
          },
        };
      });
    },

    clearDirectory(runtimeKey, directory) {
      if (runtimeKey !== get().runtimeKey) return;
      set((state) => {
        if (!(directory in state.directories)) return state;
        const next = { ...state.directories };
        delete next[directory];
        return { directories: next };
      });
    },

    reconcileChromeSnapshot(runtimeKey, directory, snapshot) {
      if (runtimeKey !== get().runtimeKey) return;
      set((state) => {
        const existing = state.directories[directory];
        if (!existing) return state;
        const widgets: OmpDirectoryState['chrome']['widgets'] = {};
        for (const widget of snapshot.widgets) {
          widgets[widget.key] = {
            key: widget.key,
            lines: widget.lines,
            ...(widget.placement !== undefined ? { placement: widget.placement } : {}),
            sessionId: widget.sessionId,
            updatedAt: widget.updatedAt,
          };
        }
        const status: OmpDirectoryState['chrome']['status'] = {};
        for (const row of snapshot.status) {
          status[row.key] = { ...row };
        }
        const nextChrome = { widgets, status };
        if (
          chromeEntriesEqual(existing.chrome.widgets, widgets)
          && chromeEntriesEqual(existing.chrome.status, status)
        ) return state;
        return {
          directories: {
            ...state.directories,
            [directory]: { ...existing, chrome: nextChrome },
          },
        };
      });
    },

    clearAll(runtimeKey) {
      set({ directories: {}, runtimeKey });
    },
  };
});

// ---------------------------------------------------------------------------
// Imperative getters (non-React call sites: queue gate, wire-event hooks)
// ---------------------------------------------------------------------------

export const getOmpDirectoryState = (directory: string): OmpDirectoryState | null =>
  useOmpSessionStore.getState().directories[directory] ?? null;

/** Extension chrome slice (spec 09 §5): stable empty constant keeps the
 *  selector reference-stable for empty directories. */
export const useOmpChromeState = (directory: string): OmpDirectoryState['chrome'] =>
  useOmpSessionStore((state) => state.directories[directory]?.chrome ?? EMPTY_CHROME);

/** Queue gate integration (spec 05 §5.5): compaction keeps the session busy. */
export const isOmpCompactionActive = (directory: string, sessionID: string): boolean => {
  const slice = useOmpSessionStore.getState().directories[directory];
  return slice?.loaders[sessionID]?.compaction !== undefined;
};

// ---------------------------------------------------------------------------
// Leaf selectors (store DOCUMENTATION selector rules — never subscribe to
// the whole directories map from a component)
// ---------------------------------------------------------------------------
export const useOmpThinkingState = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.thinking[sessionID] ?? null : null));
export const useOmpRetrySupersession = (messageID: string | undefined): boolean =>
  useOmpSessionStore((state) => {
    if (!messageID) return false;
    for (const slice of Object.values(state.directories)) {
      // Defensive `?.`: a partial slice must degrade to "no data", never crash
      // the transcript (defense-in-depth behind the seedSessionModel fix).
      if (slice.superseded?.[messageID]) return true;
    }
    return false;
  });

export const useOmpRetryNote = (messageID: string | undefined): string | undefined =>
  useOmpSessionStore((state) => {
    if (!messageID) return undefined;
    for (const slice of Object.values(state.directories)) {
      const note = slice.notes?.[messageID];
      if (note?.note !== undefined) return note.note;
    }
    return undefined;
  });

export const useOmpSessionLoaders = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.loaders[sessionID] ?? null : null));

export const useOmpCustomDetails = (directory: string, wireMessageID: string | undefined) =>
  useOmpSessionStore((state) => (wireMessageID ? state.directories[directory]?.customDetails[wireMessageID] ?? null : null));

export const useOmpSessionModelBadge = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.sessionModel[sessionID] ?? null : null));

export const useOmpFallbackState = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.fallback[sessionID] ?? null : null));

export const useOmpAwaitingAsync = (directory: string, sessionID: string | undefined): boolean =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.awaitingAsync[sessionID] !== undefined : false));

export const useOmpModeState = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.mode[sessionID] ?? null : null));

export const useOmpGoalState = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.goal[sessionID] ?? null : null));

export const useOmpPlanReview = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.planReview[sessionID] ?? null : null));

export const useOmpTelemetry = (directory: string, sessionID: string | undefined) =>
  useOmpSessionStore((state) => (sessionID ? state.directories[directory]?.telemetry[sessionID] ?? null : null));
