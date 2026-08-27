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
}

export const useOmpSessionModelSwitch = ({
  sessionID,
  directory,
  authoritativeModel,
  applyLocalModel,
  changeFailedLabel,
}: UseOmpSessionModelSwitchArgs) => {
  const { ompModels } = useRuntimeAPIs();
  const epochRef = React.useRef(0);

  // Read completion inputs at settle time so a locale switch or a newer
  // badge never serves stale strings/rollback targets to an old promise.
  const completionRef = React.useRef({ applyLocalModel, changeFailedLabel, authoritativeModel });
  completionRef.current = { applyLocalModel, changeFailedLabel, authoritativeModel };

  const switchSessionModel = React.useCallback((providerId: string, modelId: string, thinkingLevel?: string) => {
    // Mirror of the prompt-omission gate (client.ts reads the same flag at
    // send time): wherever prompts stop carrying the model, the picker owns
    // the switch explicitly. Legacy runtimes keep the prompt-time path.
    if (!isOmpModelRolesEnabled()) return;
    if (!sessionID || !directory || !providerId || !modelId) return;

    const epoch = ++epochRef.current;
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
      if (result.ok) return;
      const completion = completionRef.current;
      if (!result.unavailable) {
        toast.error(completion.changeFailedLabel);
      }
      const rollback = completion.authoritativeModel;
      if (rollback && (rollback.provider !== providerId || rollback.id !== modelId)) {
        completion.applyLocalModel(rollback.provider, rollback.id);
      }
    });
  }, [directory, ompModels, sessionID]);

  return { switchSessionModel };
};
