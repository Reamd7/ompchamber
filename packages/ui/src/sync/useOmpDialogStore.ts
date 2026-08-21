/**
 * useOmpDialogStore — per-directory pending-dialog state for the omp
 * approval/ask bridge (spec 03 §5.6.3).
 *
 * State = server snapshot + event increments. The store performs no I/O:
 * ingestion comes from the omp event pipeline effects (`dialog-requested` /
 * `dialog-settled`) and authoritative snapshots, while respond/presented/
 * lease POSTs live in `omp-dialog-controller` / `omp-dialog-lease`.
 *
 * Invariants:
 * - Scope key is (directory, sessionId, dialogId); a dialog id never merges
 *   across directories or sessions (server registry is the authority).
 * - `settled` removes; a settled id is tombstoned so a stale in-flight GET
 *   snapshot (fetched before the settle) cannot resurrect it. Tombstones
 *   expire after OMP_DIALOG_TOMBSTONE_TTL_MS.
 * - Ordering is server-mirrored: createdAt asc, id as the deterministic
 *   tie-breaker. Only the queue front is the "active modal" (ack semantics
 *   belong to the controller).
 * - Runtime guard: slices from a non-adopted runtime are never applied.
 */

import { create } from 'zustand';
import { useMemo } from 'react';
import type { OmpPendingDialog } from '@/lib/api/omp';

/** GET/SSE race guard window — a settled id cannot resurrect within it. */
export const OMP_DIALOG_TOMBSTONE_TTL_MS = 10 * 60 * 1000;
/** Bounded tombstone map so a long-lived client cannot grow it forever. */
const MAX_TOMBSTONES = 256;

export interface OmpDialogUiState {
  /** A respond POST is in flight for this dialog. */
  respondInflight: boolean;
  /** Last respond failure (cleared on retry); the dialog stays open. */
  respondError?: string;
  /** presented-ack sent for the CURRENT activation (reconnect re-acks once). */
  presentedAckSent: boolean;
}

interface OmpDialogDirectorySlice {
  dialogs: Record<string, OmpPendingDialog>;
  ui: Record<string, OmpDialogUiState>;
  /** dialogId -> settledAt; stale-snapshot resurrection guard. */
  tombstones: Record<string, number>;
}

interface OmpDialogStoreState {
  directories: Record<string, OmpDialogDirectorySlice>;
  /** Runtime identity guard — frames from another runtime are never valid. */
  runtimeKey: string;
}

interface OmpDialogStoreActions {
  /** Pipeline (re)mount — adopt the runtime identity and drop stale slices. */
  adoptRuntime: (runtimeKey: string) => void;
  /** `omp.dialog.requested` — upsert unless tombstoned (stale replay). */
  ingestRequested: (runtimeKey: string, directory: string, dialog: OmpPendingDialog) => void;
  /** `omp.dialog.settled` / accepted local respond — remove + tombstone. */
  ingestSettled: (runtimeKey: string, directory: string, dialogId: string) => void;
  /** Authoritative GET result — replace, honoring tombstones (D2 reconcile). */
  reconcileSnapshot: (runtimeKey: string, directory: string, dialogs: OmpPendingDialog[]) => void;
  markRespondInflight: (runtimeKey: string, directory: string, dialogId: string, inflight: boolean, error?: string) => void;
  /** Ack bookkeeping for the current activation; false re-arms a re-ack. */
  markPresentedAck: (runtimeKey: string, directory: string, dialogId: string, sent: boolean) => void;
  /** Wire session.deleted — drop the session's dialogs. */
  clearSession: (runtimeKey: string, directory: string, sessionId: string) => void;
  /** Directory store disposed/evicted — drop the slice. */
  clearDirectory: (runtimeKey: string, directory: string) => void;
}

export type OmpDialogStore = OmpDialogStoreState & OmpDialogStoreActions;

/** Server-mirrored ordering: createdAt asc, id as deterministic tie-breaker. */
export const compareOmpDialogs = (a: OmpPendingDialog, b: OmpPendingDialog): number =>
  a.createdAt - b.createdAt || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);

const emptyUi = (): OmpDialogUiState => ({ respondInflight: false, presentedAckSent: false });

const expireTombstones = (tombstones: Record<string, number>, at: number): Record<string, number> => {
  let next: Record<string, number> | null = null;
  for (const [id, settledAt] of Object.entries(tombstones)) {
    if (at - settledAt > OMP_DIALOG_TOMBSTONE_TTL_MS) {
      next ??= { ...tombstones };
      delete next[id];
    }
  }
  if (next === null) return tombstones;
  const ids = Object.keys(next);
  if (ids.length > MAX_TOMBSTONES) {
    ids.sort((a, b) => next![a] - next![b]);
    for (const id of ids.slice(0, ids.length - MAX_TOMBSTONES)) delete next[id];
  }
  return next;
};

export const useOmpDialogStore = create<OmpDialogStore>((set, get) => ({
  directories: {},
  runtimeKey: '',

  adoptRuntime(runtimeKey) {
    if (runtimeKey === get().runtimeKey) return;
    set({ directories: {}, runtimeKey });
  },

  ingestRequested(runtimeKey, directory, dialog) {
    if (runtimeKey !== get().runtimeKey) return;
    const at = Date.now();
    set((state) => {
      const slice = state.directories[directory];
      const tombstones = expireTombstones(slice?.tombstones ?? {}, at);
      if (tombstones[dialog.id] !== undefined) {
        // Stale replay after a settle — consumed, never resurrected.
        return slice ? { directories: { ...state.directories, [directory]: { ...slice, tombstones } } } : state;
      }
      const existing = slice?.dialogs[dialog.id];
      if (existing !== undefined && existing.createdAt === dialog.createdAt) {
        return state; // duplicate frame — idempotent
      }
      const nextUi = { ...(slice?.ui ?? {}) };
      if (nextUi[dialog.id] === undefined) nextUi[dialog.id] = emptyUi();
      return {
        directories: {
          ...state.directories,
          [directory]: {
            dialogs: { ...(slice?.dialogs ?? {}), [dialog.id]: dialog },
            ui: nextUi,
            tombstones,
          },
        },
      };
    });
  },

  ingestSettled(runtimeKey, directory, dialogId) {
    if (runtimeKey !== get().runtimeKey) return;
    const at = Date.now();
    set((state) => {
      const slice = state.directories[directory];
      if (slice === undefined) return state;
      if (slice.dialogs[dialogId] === undefined && slice.ui[dialogId] === undefined) return state;
      const dialogs = { ...slice.dialogs };
      delete dialogs[dialogId];
      const ui = { ...slice.ui };
      delete ui[dialogId];
      return {
        directories: {
          ...state.directories,
          [directory]: {
            dialogs,
            ui,
            tombstones: expireTombstones({ ...slice.tombstones, [dialogId]: at }, at),
          },
        },
      };
    });
  },

  reconcileSnapshot(runtimeKey, directory, dialogs) {
    if (runtimeKey !== get().runtimeKey) return;
    const at = Date.now();
    set((state) => {
      const slice = state.directories[directory];
      const tombstones = expireTombstones(slice?.tombstones ?? {}, at);
      const nextDialogs: Record<string, OmpPendingDialog> = {};
      const nextUi: Record<string, OmpDialogUiState> = {};
      for (const dialog of dialogs) {
        // A settle we already applied wins over a snapshot fetched before it.
        if (tombstones[dialog.id] !== undefined) continue;
        nextDialogs[dialog.id] = dialog;
        nextUi[dialog.id] = slice?.ui[dialog.id] ?? emptyUi();
      }
      // Settle dialogs the authoritative snapshot no longer carries: their
      // respond (if any) raced a server-side settle — tombstone guards the
      // next in-flight snapshot too.
      let nextTombstones = tombstones;
      for (const id of Object.keys(slice?.dialogs ?? {})) {
        if (nextDialogs[id] === undefined) {
          nextTombstones = { ...nextTombstones, [id]: at };
        }
      }
      return {
        directories: {
          ...state.directories,
          [directory]: { dialogs: nextDialogs, ui: nextUi, tombstones: nextTombstones },
        },
      };
    });
  },

  markRespondInflight(runtimeKey, directory, dialogId, inflight, error) {
    if (runtimeKey !== get().runtimeKey) return;
    set((state) => {
      const slice = state.directories[directory];
      const ui = slice?.ui[dialogId];
      if (slice === undefined || ui === undefined) return state;
      return {
        directories: {
          ...state.directories,
          [directory]: {
            ...slice,
            ui: {
              ...slice.ui,
              [dialogId]: {
                ...ui,
                respondInflight: inflight,
                ...(inflight ? { respondError: undefined } : {}),
                ...(!inflight && error !== undefined ? { respondError: error } : {}),
              },
            },
          },
        },
      };
    });
  },

  markPresentedAck(runtimeKey, directory, dialogId, sent) {
    if (runtimeKey !== get().runtimeKey) return;
    set((state) => {
      const slice = state.directories[directory];
      const ui = slice?.ui[dialogId];
      if (slice === undefined || ui === undefined || ui.presentedAckSent === sent) return state;
      return {
        directories: {
          ...state.directories,
          [directory]: {
            ...slice,
            ui: { ...slice.ui, [dialogId]: { ...ui, presentedAckSent: sent } },
          },
        },
      };
    });
  },

  clearSession(runtimeKey, directory, sessionId) {
    if (runtimeKey !== get().runtimeKey) return;
    set((state) => {
      const slice = state.directories[directory];
      if (slice === undefined) return state;
      let touched = false;
      const dialogs: Record<string, OmpPendingDialog> = {};
      const ui: Record<string, OmpDialogUiState> = {};
      for (const [id, dialog] of Object.entries(slice.dialogs)) {
        if (dialog.sessionId === sessionId) {
          touched = true;
          continue;
        }
        dialogs[id] = dialog;
        ui[id] = slice.ui[id] ?? emptyUi();
      }
      if (!touched) return state;
      return { directories: { ...state.directories, [directory]: { dialogs, ui, tombstones: slice.tombstones } } };
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
}));

// ---------------------------------------------------------------------------
// Leaf selectors (store DOCUMENTATION selector rules — never subscribe to a
// container from a component)
// ---------------------------------------------------------------------------

const EMPTY_DIALOGS: OmpPendingDialog[] = [];

/** Pending dialogs for one session, server-ordered (queue front = active). */
export const useOmpDialogsForSession = (directory: string, sessionId: string | undefined): OmpPendingDialog[] => {
  const dialogsById = useOmpDialogStore((state) => state.directories[directory]?.dialogs ?? EMPTY_DIALOGS_BY_ID);
  return useMemo(() => {
    if (!sessionId) return EMPTY_DIALOGS;
    const dialogs = Object.values(dialogsById).filter((dialog) => dialog.sessionId === sessionId);
    return dialogs.length === 0 ? EMPTY_DIALOGS : dialogs.sort(compareOmpDialogs);
  }, [dialogsById, sessionId]);
};

/** Per-dialog UI flags (inflight/error/ack) for the active modal. */
export const useOmpDialogUi = (directory: string, dialogId: string | undefined): OmpDialogUiState | null =>
  useOmpDialogStore((state) =>
    dialogId ? state.directories[directory]?.ui[dialogId] ?? null : null,
  );

/** Any pending dialog in a directory — eviction/dispose guard (spec 03 §5.6.4). */
export const hasOmpPendingDialogs = (directory: string): boolean => {
  const slice = useOmpDialogStore.getState().directories[directory];
  return slice !== undefined && Object.keys(slice.dialogs).length > 0;
};

const countPendingDialogsBySession = (dialogs: Record<string, OmpPendingDialog>): Map<string, number> => {
  const out = new Map<string, number>();
  for (const dialog of Object.values(dialogs)) {
    out.set(dialog.sessionId, (out.get(dialog.sessionId) ?? 0) + 1);
  }
  return out;
};

/** Sessions with pending dialogs in a directory (WorkStatus subagents read). */
export const ompPendingDialogSessions = (directory: string): Map<string, number> =>
  countPendingDialogsBySession(useOmpDialogStore.getState().directories[directory]?.dialogs ?? {});

const EMPTY_DIALOGS_BY_ID: Record<string, OmpPendingDialog> = {};

/**
 * Reactive `ompPendingDialogSessions` for render: one subscription on the
 * directory's dialogs record (its identity changes only on mutation — never
 * subscribe to the directories container), counts derived per render.
 */
export const useOmpPendingDialogSessions = (directory: string): Map<string, number> => {
  const dialogs = useOmpDialogStore((state) => state.directories[directory]?.dialogs ?? EMPTY_DIALOGS_BY_ID);
  return useMemo(() => countPendingDialogsBySession(dialogs), [dialogs]);
};
