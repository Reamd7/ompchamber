/**
 * omp capability gate — the single UI-side source of truth for
 * `GET /api/omp/capabilities` feature keys (master D6-R2: capabilities are
 * server-adjudicated; the UI never keeps a local feature flag).
 *
 * The probe runs at most once per runtime key and settles a module cache;
 * hooks (`useOmpModelRoles`) subscribe to the settled value, while
 * non-React consumers (useConfigStore's default-agent cascade) read it
 * synchronously via `isOmpFeatureEnabled`. Until the probe resolves, every
 * feature reads `false` — the legacy behavior — so the three degradation
 * matrices (new UI/old engine, old UI/new engine, relay bundle) all land on
 * "existing picker unchanged" (spec 01 §6.4).
 *
 * A transport failure is cached as "absent" for the runtime's lifetime: a
 * flaky probe must not flip the composer between two picker universes, and
 * a runtime switch re-probes under the new key.
 */

import { getRegisteredRuntimeAPIs } from '@/contexts/runtimeAPIRegistry';
import type { OmpCapabilities } from '@/lib/api/omp';
import { getRuntimeKey } from '@/lib/runtime-switch';

export interface OmpCapabilityGateResult {
  capabilities: OmpCapabilities | null;
}

const probes = new Map<string, Promise<OmpCapabilityGateResult>>();
const settled = new Map<string, OmpCapabilityGateResult>();

const probeRuntime = (runtimeKey: string): Promise<OmpCapabilityGateResult> => {
  const existing = probes.get(runtimeKey);
  if (existing) return existing;

  const attempt = (async (): Promise<OmpCapabilityGateResult> => {
    const apis = getRegisteredRuntimeAPIs();
    if (!apis?.ompCapabilities) return { capabilities: null };
    try {
      // `null` = definitive absence (old engine / feature off). A throw is a
      // transport failure — degraded, not authoritative absence — but cached
      // all the same so one flaky mount cannot oscillate the UI.
      return { capabilities: await apis.ompCapabilities.getCapabilities() };
    } catch {
      return { capabilities: null };
    }
  })();

  probes.set(runtimeKey, attempt);
  void attempt.then((entry) => settled.set(runtimeKey, entry));
  return attempt;
};

/** Starts (or joins) this runtime's capability probe. Idempotent. */
export const primeOmpCapabilityGate = (): Promise<OmpCapabilityGateResult> => probeRuntime(getRuntimeKey());

/**
 * Synchronous feature read. `false` while unresolved, absent, off, or after
 * a failed probe — callers degrade to legacy behavior in every one of those
 * states (never fail open onto a surface the server did not advertise).
 */
export const isOmpFeatureEnabled = (featureKey: string): boolean => {
  const gate = settled.get(getRuntimeKey());
  return gate?.capabilities?.features?.[featureKey] === true;
};

/** Convenience read for the model-roles gate (01 §5.0 / master D6-R2). */
export const isOmpModelRolesEnabled = (): boolean => isOmpFeatureEnabled('modelRoles.v1');

/** Convenience read for the session-modes gate (02 §5.4). */
export const isOmpModesEnabled = (): boolean => isOmpFeatureEnabled('modes.v1');

/** Convenience read for the agent-definitions CRUD gate (02 §5.2). */
export const isOmpAgentDefinitionsEnabled = (): boolean => isOmpFeatureEnabled('agentDefinitions.v1');

/** Convenience read for the personas gate (02 §5.2a). */
export const isOmpPersonasEnabled = (): boolean => isOmpFeatureEnabled('personas.v1');

/** Test seam: clears the per-runtime cache and settled answers. */
export const __resetOmpCapabilityGateForTests = (): void => {
  probes.clear();
  settled.clear();
};
