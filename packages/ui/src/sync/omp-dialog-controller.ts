/**
 * omp dialog controller — the I/O half of the approval/ask bridge (spec 03
 * §5.6.2-3/§5.6.3). The store owns state; this module owns POSTs and the
 * reconciliation discipline:
 *
 * - `respond`: optimistic settle on success (the SSE `omp.dialog.settled`
 *   echo is idempotent against the tombstone). A 409 means another client or
 *   a timeout settled first — settle locally after reconciling, which drops
 *   the card authoritatively. Any other failure keeps the dialog open with
 *   the error surfaced on it (retry stays available); a failed settings
 *   write in the "always allow" transaction never approves (R10 order is
 *   enforced by the caller performing the write first).
 * - `presented`: exactly one ack per activation. A failed ack re-arms so the
 *   next activation/reconnect retries (T_answer never starts without the
 *   server's anchor).
 * - `reconcile`: authoritative GET; tombstones in the store keep a stale
 *   in-flight snapshot from resurrecting a settled dialog.
 */

import {
  OMP_ENDPOINTS,
  createOmpDialogsAPI,
  type OmpDialogRespondResult,
  type OmpDialogsAPI,
} from '@/lib/api/omp';
import { runtimeFetch } from '@/lib/runtime-fetch';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { useOmpDialogStore } from './useOmpDialogStore';

export interface OmpDialogControllerDeps {
  api?: OmpDialogsAPI;
  getRuntimeKey?: () => string;
}

export type OmpRespondFlowResult =
  | { ok: true }
  /** 409 — settled elsewhere; the card is already gone after reconcile. */
  | { ok: false; conflict: true }
  /** Unavailable surface (old engine / feature off) — dialog dropped. */
  | { ok: false; unavailable: true }
  /** Transport/validation failure — dialog kept open, error surfaced. */
  | { ok: false; conflict: false; error?: string };

export interface OmpDialogController {
  /** POST respond with the full inflight/409/error discipline. */
  respond(directory: string, dialogId: string, result: OmpDialogRespondResult): Promise<OmpRespondFlowResult>;
  /** presented-ack for the queue front's current activation. */
  presented(directory: string, dialogId: string): Promise<void>;
  /** POST abort (user Stop path); local settle on success. */
  abort(directory: string, dialogId: string): Promise<void>;
  /** Authoritative GET → store.reconcileSnapshot. */
  reconcile(directory: string): Promise<void>;
  /**
   * "Always allow" transaction (spec 03 §5.3.2): write
   * `tools.approval.<tool> = "allow"` first; ONLY on a successful write
   * send the Approve respond. A failed write approves nothing — the dialog
   * stays open (R10 order). A 409 after a successful write converges
   * without repeating the write.
   */
  alwaysAllowAndApprove(directory: string, dialogId: string, toolName: string): Promise<OmpRespondFlowResult>;
  writeAlwaysAllow(directory: string, toolName: string): Promise<{ ok: true } | { ok: false; error?: string }>;
}

export interface OmpDialogControllerDeps {
  api?: OmpDialogsAPI;
  getRuntimeKey?: () => string;
  /** Settings-write seam (default: chapter-06 PUT channel). */
  settingsWrite?: (directory: string, changes: Record<string, unknown>) => Promise<{ ok: true } | { ok: false; error?: string }>;
}

export const createOmpDialogController = (deps: OmpDialogControllerDeps = {}): OmpDialogController => {
  const api = deps.api ?? createOmpDialogsAPI();
  const settingsWrite = deps.settingsWrite ?? defaultSettingsWrite;
  const getRuntime = deps.getRuntimeKey ?? getRuntimeKey;

  return {
    async respond(directory, dialogId, result) {
      const runtimeKey = getRuntime();
      const state = useOmpDialogStore.getState();
      if (state.runtimeKey !== runtimeKey) return { ok: false, conflict: false };
      if (state.directories[directory]?.ui[dialogId]?.respondInflight) {
        return { ok: false, conflict: false };
      }
      state.markRespondInflight(runtimeKey, directory, dialogId, true);
      const outcome = await api.respond(directory, dialogId, result);
      if (outcome.ok) {
        useOmpDialogStore.getState().ingestSettled(runtimeKey, directory, dialogId);
        return { ok: true };
      }
      if (outcome.unavailable) {
        // Surface vanished (engine restart / rollback): authoritative state
        // is "no pending dialogs" — settle locally without a doomed GET.
        useOmpDialogStore.getState().ingestSettled(runtimeKey, directory, dialogId);
        return { ok: false, unavailable: true };
      }
      if (outcome.status === 409) {
        // Settled by the other end (race) or by timeout: reconcile against
        // the server first, then settle locally if it somehow survived.
        await this.reconcile(directory);
        useOmpDialogStore.getState().ingestSettled(runtimeKey, directory, dialogId);
        return { ok: false, conflict: true };
      }
      useOmpDialogStore.getState().markRespondInflight(runtimeKey, directory, dialogId, false, outcome.error);
      return { ok: false, conflict: false, ...(outcome.error !== undefined ? { error: outcome.error } : {}) };
    },

    async presented(directory, dialogId) {
      const runtimeKey = getRuntime();
      const state = useOmpDialogStore.getState();
      if (state.runtimeKey !== runtimeKey) return;
      const ui = state.directories[directory]?.ui[dialogId];
      if (ui === undefined || ui.presentedAckSent) return;
      state.markPresentedAck(runtimeKey, directory, dialogId, true);
      const outcome = await api.presented(directory, dialogId);
      if (!outcome.ok) {
        // Re-arm: T_answer must not silently never start.
        useOmpDialogStore.getState().markPresentedAck(runtimeKey, directory, dialogId, false);
      }
    },

    async abort(directory, dialogId) {
      const runtimeKey = getRuntime();
      const state = useOmpDialogStore.getState();
      if (state.runtimeKey !== runtimeKey) return;
      const outcome = await api.abort(directory, dialogId);
      if (outcome.ok || outcome.unavailable) {
        state.ingestSettled(runtimeKey, directory, dialogId);
      }
    },

    async reconcile(directory) {
      const runtimeKey = getRuntime();
      if (useOmpDialogStore.getState().runtimeKey !== runtimeKey) return;
      const snapshot = await api.getSnapshot(directory);
      if (!snapshot.ok) return; // failure keeps prior state (D2)
      useOmpDialogStore.getState().reconcileSnapshot(runtimeKey, directory, snapshot.dialogs);
    },

    async writeAlwaysAllow(directory, toolName) {
      return settingsWrite(directory, { [`tools.approval.${toolName}`]: 'allow' });
    },

    async alwaysAllowAndApprove(directory, dialogId, toolName) {
      const write = await this.writeAlwaysAllow(directory, toolName);
      if (!write.ok) {
        // R10 order: nothing was approved; the dialog stays open.
        return { ok: false, conflict: false, ...(write.error !== undefined ? { error: write.error } : {}) };
      }
      // The settings write succeeded, so the respond below is a plain
      // Approve; a 409 converges without repeating the write.
      return this.respond(directory, dialogId, { kind: 'select', value: 'Approve' });
    },
  };
};

const defaultSettingsWrite = async (
  directory: string,
  changes: Record<string, unknown>,
): Promise<{ ok: true } | { ok: false; error?: string }> => {
  try {
    const response = await runtimeFetch(OMP_ENDPOINTS.settings, {
      method: 'PUT',
      headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
      body: JSON.stringify({ directory, scope: 'global', changes }),
    });
    if (!response.ok) {
      let error: string | undefined;
      try {
        const payload = (await response.json()) as { error?: string };
        error = typeof payload?.error === 'string' ? payload.error : undefined;
      } catch {
        // status alone is enough
      }
      return { ok: false, ...(error !== undefined ? { error } : {}) };
    }
    return { ok: true };
  } catch {
    return { ok: false };
  }
};

/** Shared production instance; tests construct their own with stub APIs. */
export const ompDialogController: OmpDialogController = createOmpDialogController();
