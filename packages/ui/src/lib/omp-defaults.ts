/**
 * omp-defaults — shared default-input resolver for OpenChamber original
 * surfaces (spec 08 §5.1 GAP-01).
 *
 * Every original surface (multirun, scheduled tasks, GitHub issue picker,
 * new worktree) resolves its "default model" through this single helper
 * instead of carrying per-dialog copies of the legacy cascade. Under the
 * omp model-roles capability the default is the directory's `roles.default`
 * assignment (settings-side truth from GET /api/omp/models); resolving to no
 * model is a legal state — the surface then sends model-free prompts and the
 * engine resolves the role at run time. Legacy behavior (explicit cascade
 * values) applies whenever the capability is off, unresolved, or the snapshot
 * fetch fails; a failed fetch is never treated as authoritative emptiness.
 */

import { getRegisteredRuntimeAPIs } from '@/contexts/runtimeAPIRegistry';
import { isOmpModelRolesEnabled, isOmpPersonasEnabled, primeOmpCapabilityGate } from '@/lib/omp/capabilityGate';

export interface OmpDefaultModel {
  providerID: string;
  modelID: string;
  thinkingLevel?: string;
}

export interface OmpDefaultsResult {
  /** Role-default model for the directory; null = follow the engine default. */
  model: OmpDefaultModel | null;
  /** True only when the capability settled on and the snapshot answered. */
  modelRolesEnabled: boolean;
  /**
   * personas.v1 settled on (spec 08 GAP-02): persona-typed surfaces must not
   * synthesize a legacy default agent — undefined persona (standard session)
   * is the correct default. Does not require the models snapshot.
   */
  personasEnabled: boolean;
}

export const resolveOmpDefaults = async (
  directory: string | null | undefined,
): Promise<OmpDefaultsResult> => {
  if (!directory) return { model: null, modelRolesEnabled: false, personasEnabled: false };
  await primeOmpCapabilityGate();
  const personasEnabled = isOmpPersonasEnabled();
  if (!isOmpModelRolesEnabled()) return { model: null, modelRolesEnabled: false, personasEnabled };
  const apis = getRegisteredRuntimeAPIs();
  const result = await apis?.ompModels?.getModels({ directory });
  // Transport failure / malformed payload → degrade to the legacy display
  // chain (D2: failure is not authoritative empty success).
  if (!result?.ok) return { model: null, modelRolesEnabled: false, personasEnabled };
  const role = result.data.roles.default;
  if (!role?.provider || !role?.id) {
    // Role unconfigured is a legitimate "follow the engine default" state.
    return { model: null, modelRolesEnabled: true, personasEnabled };
  }
  return {
    model: {
      providerID: role.provider,
      modelID: role.id,
      ...(role.thinkingLevel ? { thinkingLevel: role.thinkingLevel } : {}),
    },
    modelRolesEnabled: true,
    personasEnabled,
  };
};
