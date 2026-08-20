/**
 * useOmpModelRoles — capability-gated omp role/mode inputs for the composer
 * (spec 01 §5.3(1)/§5.5, 08 §5.1–5.2; master D6-R2).
 *
 * Three-state contract per surface:
 *  - capability off / probe unresolved / probe failed → legacy behavior
 *    (`modelRolesEnabled === false`, `roles === []`): the existing picker
 *    renders unchanged (three-matrix degradation, spec 01 §6.4);
 *  - `modelRoles.v1` on but the models snapshot fetch failed or is
 *    malformed → still legacy: the roles surface never renders from a
 *    non-authoritative answer;
 *  - feature on + snapshot present → role slots are exposed for the picker.
 *
 * The modes selector needs no models data, so `modesEnabled` follows the
 * capability keys only (`modes.v1 && modelRoles.v1`).
 */

import React from 'react';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import type { OmpModelsSnapshot } from '@/lib/api/omp';
import {
  primeOmpCapabilityGate,
  __resetOmpCapabilityGateForTests,
} from '@/lib/omp/capabilityGate';
import { getRuntimeKey } from '@/lib/runtime-switch';
import { useConfigStore } from '@/stores/useConfigStore';

export interface OmpFeatureFlagsState {
  /** The capability probe settled (either answer). */
  resolved: boolean;
  modelRoles: boolean;
  modes: boolean;
}

const reResolvedRuntimes = new Set<string>();
/**
 * When the capability probe settles ON after the config store already
 * applied legacy defaults (probe raced the provider boot), re-resolve once
 * per runtime so the default-agent cascade can drop the build/plan
 * fallback. Manual selections survive via the store's manual guard.
 */
const reResolveDefaultSelection = (): void => {
  const runtimeKey = getRuntimeKey();
  if (reResolvedRuntimes.has(runtimeKey)) return;
  reResolvedRuntimes.add(runtimeKey);
  useConfigStore.getState().applyOpenCodeConfigDefaults();
};
/**
 * Subscribes to the runtime's omp capability gate. The probe itself runs
 * once per runtime (see lib/omp/capabilityGate); this hook only surfaces
 * the settled answer to React.
 */
export const useOmpFeatureFlags = (): OmpFeatureFlagsState => {
  const [state, setState] = React.useState<OmpFeatureFlagsState>({ resolved: false, modelRoles: false, modes: false });
  const runtimeKey = getRuntimeKey();

  React.useEffect(() => {
    let cancelled = false;
    setState({ resolved: false, modelRoles: false, modes: false });
    void primeOmpCapabilityGate().then((entry) => {
      if (cancelled) return;
      const features = entry.capabilities?.features ?? {};
      setState({
        resolved: true,
        modelRoles: features['modelRoles.v1'] === true,
        modes: features['modes.v1'] === true,
      });
      if (features['modelRoles.v1'] === true) reResolveDefaultSelection();
    });
    return () => {
      cancelled = true;
    };
  }, [runtimeKey]);

  return state;
};

/** One picker-ready role row built from the models snapshot. */
export interface OmpRoleSlot {
  id: string;
  name: string;
  tag?: string;
  configured: boolean;
  model: { provider: string; id: string; thinkingLevel?: string } | null;
}

const toRoleSlot = (id: string, snapshot: OmpModelsSnapshot): OmpRoleSlot | null => {
  const meta = snapshot.roleMeta[id];
  if (meta?.hidden === true) return null;
  const entry = snapshot.roles[id];
  const model = entry?.provider && entry.id
    ? {
        provider: entry.provider,
        id: entry.id,
        ...(entry.thinkingLevel !== undefined ? { thinkingLevel: entry.thinkingLevel } : {}),
      }
    : null;
  return {
    id,
    name: meta?.name ?? id,
    ...(meta?.tag !== undefined ? { tag: meta.tag } : {}),
    configured: entry != null,
    model,
  };
};

export const buildRoleSlots = (snapshot: OmpModelsSnapshot): OmpRoleSlot[] => {
  const ordered: OmpRoleSlot[] = [];
  const seen = new Set<string>();
  for (const id of snapshot.cycleOrder) {
    const slot = toRoleSlot(id, snapshot);
    if (slot) {
      ordered.push(slot);
      seen.add(id);
    }
  }
  for (const id of Object.keys(snapshot.roles)) {
    if (seen.has(id)) continue;
    const slot = toRoleSlot(id, snapshot);
    if (slot) ordered.push(slot);
  }
  return ordered;
};

export interface OmpModelRolesState {
  resolved: boolean;
  /** Roles surface may render (feature on AND authoritative snapshot present). */
  modelRolesEnabled: boolean;
  /** Mode selector replaces the agent chip (modes.v1 && modelRoles.v1). */
  modesEnabled: boolean;
  snapshot: OmpModelsSnapshot | null;
  roles: OmpRoleSlot[];
}

export const useOmpModelRoles = (directory: string | null | undefined): OmpModelRolesState => {
  const flags = useOmpFeatureFlags();
  const { ompModels } = useRuntimeAPIs();
  const [snapshot, setSnapshot] = React.useState<OmpModelsSnapshot | null>(null);

  const directoryKey = directory ?? null;
  const featureOn = flags.modelRoles;

  React.useEffect(() => {
    if (!featureOn || !directoryKey) {
      setSnapshot(null);
      return;
    }
    let cancelled = false;
    setSnapshot(null);
    void ompModels.getModels({ directory: directoryKey }).then((result) => {
      if (cancelled) return;
      // Fetch failure / malformed payload / surface absent → keep the legacy
      // picker; never render roles from a non-authoritative answer.
      setSnapshot(result.ok ? result.data : null);
    });
    return () => {
      cancelled = true;
    };
  }, [ompModels, featureOn, directoryKey]);

  const roles = React.useMemo(() => (snapshot ? buildRoleSlots(snapshot) : []), [snapshot]);

  return {
    resolved: flags.resolved,
    modelRolesEnabled: featureOn && snapshot !== null,
    modesEnabled: flags.modes && flags.modelRoles,
    snapshot,
    roles,
  };
};

/**
 * The composer's agent field under the omp concept system: with model roles
 * active, new sessions carry no agent (sessions default to standard; master
 * D3 row 1). Explicit `@agent` mentions flow through `agentMentions`, not
 * this field, and stay untouched.
 */
export const resolveSendAgent = (
  legacyAgent: string | null | undefined,
  modelRolesEnabled: boolean,
): string | undefined => (modelRolesEnabled ? undefined : legacyAgent || undefined);

/** Re-exported for hook-level tests. */
export { __resetOmpCapabilityGateForTests };
