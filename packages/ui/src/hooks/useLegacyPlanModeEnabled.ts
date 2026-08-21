/**
 * useLegacyPlanModeEnabled — the retired experimental plan flag's single
 * choke point (GAP-F9, 06 §5.8/§6.4 stage 1).
 *
 * With `modes.v1` on, the omp mode domain owns plan surfaces: the
 * `planModeExperimentalEnabled` flag stops being produced (the /health read
 * in the app shells) and consumed (every planModeEnabled reader routes
 * through here) at the same time, under the capability gate. With the
 * capability off — old engine, feature disabled, or probe unresolved —
 * every consumer keeps its exact legacy behavior.
 *
 * The store key itself stays dormant; physical deletion lands with the
 * chapter-07 sweep.
 */

import { isOmpFeatureEnabled } from '@/lib/omp/capabilityGate';
import { useOmpFeatureFlags } from '@/hooks/useOmpModelRoles';
import { useFeatureFlagsStore } from '@/stores/useFeatureFlagsStore';

export const useLegacyPlanModeEnabled = (): boolean => {
  const modes = useOmpFeatureFlags().modes;
  const planModeEnabled = useFeatureFlagsStore((state) => state.planModeEnabled);
  return !modes && planModeEnabled;
};

/** Non-React read for imperative call sites (keyboard shortcuts, getters). */
export const isLegacyPlanModeEnabled = (): boolean =>
  !isOmpFeatureEnabled('modes.v1') && useFeatureFlagsStore.getState().planModeEnabled;
