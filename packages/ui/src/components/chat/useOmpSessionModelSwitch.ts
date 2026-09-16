/**
 * useOmpSessionModelSwitch — the composer picker's omp model-switch write
 * path (spec 01 GAP-02/GAP-04).
 *
 * Under the model-roles capability prompts are model-free, so selecting a
 * model must switch the session server-side through POST
 * /api/omp/sessions/{id}/model — the omp /switch equivalent (AgentSession
 * setModel + model_change transcript entry + omp.model.changed). The
 * embedder keeps its local optimistic apply; this hook owns the server
 * write, stale-completion guarding, and failure rollback to the last
 * authoritative session model (the omp.model.changed-fed badge).
 *
 * Persistence contract: the embedder's per-session selection is only a
 * client-side cache. `onConfirmedModel` reports the selection the session
 * actually ended up with — the requested pair after a successful switch,
 * the authoritative model after a failure rollback — so the cache can only
 * ever hold server-confirmed values. A pick whose write never reaches the
 * engine (session exists but its directory cannot be resolved) is a
 * failure with a toast, never a silent local-only change.
 */
import React from 'react';
import { toast } from 'sonner';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { isOmpModelRolesEnabled } from '@/lib/omp/capabilityGate';

export interface UseOmpSessionModelSwitchArgs {
  /** Session being switched; absent disables the write path. */
  sessionID: string | null | undefined;
  /** omp bus directory key scoping the session. */
  directory: string | null | undefined;
  /**
   * Last authoritative session model (the badge). A failed switch rolls the
   * embedder's optimistic selection back to this; null skips rollback.
   */
  authoritativeModel: { provider: string; id: string } | null;
  /** Local-only apply used for rollback (never a server write). */
  applyLocalModel: (providerId: string, modelId: string) => void;
  /** Failure toast text. */
  changeFailedLabel: string;
  /**
   * Persist the server-confirmed selection: the requested pair on success,
   * the authoritative model after a rollback. Called only once per settled
   * switch attempt.
   */
  onConfirmedModel?: (providerId: string, modelId: string) => void;
}

interface PendingModel {
  provider: string;
  id: string;
}

export const useOmpSessionModelSwitch = ({
  sessionID,
  directory,
  authoritativeModel,
  applyLocalModel,
  changeFailedLabel,
  onConfirmedModel,
}: UseOmpSessionModelSwitchArgs) => {
  const { ompModels } = useRuntimeAPIs();
  const epochRef = React.useRef(0);
  const [pendingModel, setPendingModel] = React.useState<PendingModel | null>(null);

  // Read completion inputs at settle time so a locale switch or a newer
  // badge never serves stale strings/rollback targets to an old promise.
  const completionRef = React.useRef({ applyLocalModel, changeFailedLabel, authoritativeModel, onConfirmedModel });
  completionRef.current = { applyLocalModel, changeFailedLabel, authoritativeModel, onConfirmedModel };
  const settleAsFailure = React.useCallback((requested: PendingModel, options: { silent?: boolean } = {}) => {
    const completion = completionRef.current;
    if (!requested.provider || !requested.id) return;
    // `unavailable` means the runtime dropped the feature mid-session —
    // rollback still converges the local state, but a toast would claim a
    // failure the user cannot act on.
    if (!options.silent) toast.error(completion.changeFailedLabel);
    const rollback = completion.authoritativeModel;
    if (rollback && (rollback.provider !== requested.provider || rollback.id !== requested.id)) {
      completion.applyLocalModel(rollback.provider, rollback.id);
      completion.onConfirmedModel?.(rollback.provider, rollback.id);
    }
  }, []);

  const switchSessionModel = React.useCallback((providerId: string, modelId: string, thinkingLevel?: string) => {
    // Mirror of the prompt-omission gate (client.ts reads the same flag at
    // send time): wherever prompts stop carrying the model, the picker owns
    // the switch explicitly. Legacy runtimes keep the prompt-time path.
    if (!isOmpModelRolesEnabled()) return;
    // A draft has no session to switch — the local selection is the whole
    // state and prompts carry it. Not a failure.
    if (!sessionID) return;
    if (!providerId || !modelId) return;
    // A session whose directory cannot be resolved can never be switched
    // server-side: fail loudly instead of leaving a local-only selection
    // that silently diverges from the session.
    if (!directory) {
      settleAsFailure({ provider: providerId, id: modelId });
      return;
    }

    const epoch = ++epochRef.current;
    setPendingModel({ provider: providerId, id: modelId });
    void ompModels.setSessionModel(
      sessionID,
      { providerID: providerId, modelID: modelId },
      {
        directory,
        // An explicit thinking level rides the same switch (role picks carry
        // their resolved level); undefined leaves the session's level alone.
        ...(thinkingLevel !== undefined ? { thinkingLevel } : {}),
      },
    ).then((result) => {
      // A newer selection already owns the UI; this completion is stale.
      if (epoch !== epochRef.current) return;
      setPendingModel(null);
      if (result.ok) {
        completionRef.current.onConfirmedModel?.(providerId, modelId);
        return;
      }
      settleAsFailure({ provider: providerId, id: modelId }, { silent: result.unavailable });
    });
  }, [directory, ompModels, sessionID, settleAsFailure]);

  return { switchSessionModel, pendingModel };
};
