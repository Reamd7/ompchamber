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
import { useOmpSettingsRevision } from '@/sync/useOmpSessionStore';

export interface OmpFeatureFlagsState {
  /** The capability probe settled (either answer). */
  resolved: boolean;
  modelRoles: boolean;
  modes: boolean;
  /** personas.v1 — the composer agent chip becomes a persona selector. */
  personas: boolean;
  /** agentDefinitions.v1 — the agents settings surface writes /api/omp/agent-definitions. */
  agentDefinitions: boolean;
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
  const [state, setState] = React.useState<OmpFeatureFlagsState>({
    resolved: false,
    modelRoles: false,
    modes: false,
    personas: false,
    agentDefinitions: false,
  });
  const runtimeKey = getRuntimeKey();

  React.useEffect(() => {
    let cancelled = false;
    setState({
      resolved: false,
      modelRoles: false,
      modes: false,
      personas: false,
      agentDefinitions: false,
    });
    void primeOmpCapabilityGate().then((entry) => {
      if (cancelled) return;
      const features = entry.capabilities?.features ?? {};
      setState({
        resolved: true,
        modelRoles: features['modelRoles.v1'] === true,
        modes: features['modes.v1'] === true,
        personas: features['personas.v1'] === true,
        agentDefinitions: features['agentDefinitions.v1'] === true,
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
  /** Persisted layer for the assignment ('global' | 'project'); absent when unconfigured. */
  source?: string;
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
    ...(entry?.source !== undefined ? { source: entry.source } : {}),
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
  /** Persona selector replaces the agent chip (personas.v1, spec 02 §5.1 D-B2). */
  personasEnabled: boolean;
  snapshot: OmpModelsSnapshot | null;
  roles: OmpRoleSlot[];
  /** True while a (re)fetch is in flight — lets settings surfaces distinguish "loading" from "unavailable". */
  pending: boolean;
  /** Re-fetches the models snapshot (settings writes need immediate feedback). */
  reload: () => void;
}

export const useOmpModelRoles = (directory: string | null | undefined): OmpModelRolesState => {
  const flags = useOmpFeatureFlags();
  const { ompModels } = useRuntimeAPIs();
  const [snapshot, setSnapshot] = React.useState<OmpModelsSnapshot | null>(null);
  const [reloadEpoch, setReloadEpoch] = React.useState(0);
  const [pending, setPending] = React.useState(false);


  const directoryKey = directory ?? null;
  const featureOn = flags.modelRoles;

  // Live refresh signal: every settings write (engine settings page,
  // "Set as role" in this picker, omp CLI) broadcasts omp.settings.updated;
  // the reducer stores the revision here. A jump refetches the models
  // snapshot so role chips follow settings changes without remounting.
  const settingsRevision = useOmpSettingsRevision(directoryKey ?? '');

  React.useEffect(() => {
    if (!featureOn || !directoryKey) {
      setSnapshot(null);
      setPending(false);
      return;
    }
    let cancelled = false;
    setPending(true);
    void ompModels.getModels({ directory: directoryKey }).then((result) => {
      if (cancelled) return;
      // Fetch failure / malformed payload → keep the legacy picker when
      // nothing authoritative exists yet, and keep the previous snapshot
      // when one does (a failed refresh must not blank valid role chips);
      // never render roles from a non-authoritative answer.
      setSnapshot((previous) => (result.ok ? result.data : previous));
      setPending(false);
    });
    return () => {
      cancelled = true;
    };
  }, [ompModels, featureOn, directoryKey, reloadEpoch, settingsRevision]);

  // Scope changes invalidate everything; a settings-driven refresh keeps
  // the previous slots rendered until the fresh snapshot lands (no
  // legacy-picker flicker on every settings write).
  React.useEffect(() => {
    setSnapshot(null);
  }, [featureOn, directoryKey]);






  const reload = React.useCallback(() => setReloadEpoch((epoch) => epoch + 1), []);
  const roles = React.useMemo(() => (snapshot ? buildRoleSlots(snapshot) : []), [snapshot]);

  return {
    resolved: flags.resolved,
    modelRolesEnabled: featureOn && snapshot !== null,
    modesEnabled: flags.modes && flags.modelRoles,
    personasEnabled: flags.personas,
    snapshot,
    roles,
    pending,
    reload,
  };
};
/**
 * The composer's agent field under the omp concept system: with model roles
 * active, new sessions carry no agent (sessions default to standard; master
 * D3 row 1). Explicit `@agent` mentions flow through `agentMentions`, not
 * this field, and stay untouched.
 *
 * Under personas.v1 the same wire field becomes the explicit persona
 * carrier (spec 02 §5.1 D-B2/D-B3): a selected persona rides the next
 * prompt as the agent param — the engine persists it to the session's
 * registry meta and rebuilds the AgentSession — and an explicit switch back
 * to "Standard" sends 'build', the engine's standard sentinel, so a
 * previously persisted persona is cleared. No selection (null/empty) omits
 * the field and the engine keeps the session's persisted persona.
 */
export const resolveSendAgent = (
  legacyAgent: string | null | undefined,
  modelRolesEnabled: boolean,
  personasEnabled = false,
): string | undefined => {
  if (personasEnabled) return legacyAgent || undefined;
  return modelRolesEnabled ? undefined : legacyAgent || undefined;
};

/** Re-exported for hook-level tests. */
export { __resetOmpCapabilityGateForTests };
