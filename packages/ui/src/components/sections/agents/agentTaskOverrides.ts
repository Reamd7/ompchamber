/**
 * omp task.* override helpers — the per-agent chips form's write model
 * (spec 02 §5.2/§5.3, GAP-B05).
 *
 * Overrides are settings-level keys (`task.disabledAgents`,
 * `task.agentModelOverrides`, `task.agentPrewalk`, `task.agentAdvisor`) written
 * through `PUT /api/omp/settings` — global scope (the project-scope layer
 * only carries modelRoles today, 06 §5.3 R6). The PUT sends the FULL key
 * value, so every change is computed against the authoritative raw records
 * read from `GET /api/omp/settings`; sibling agents' entries (including
 * bundled agents absent from the definitions list) are preserved.
 */

import { z } from 'zod';

export const TASK_OVERRIDE_SETTING_KEYS = [
  'task.disabledAgents',
  'task.agentModelOverrides',
  'task.agentPrewalk',
  'task.agentAdvisor',
] as const;

export interface TaskOverrideRecords {
  disabledAgents: string[];
  modelOverrides: Record<string, string>;
  prewalk: Record<string, string>;
  advisor: Record<string, string>;
}

export const emptyTaskOverrideRecords = (): TaskOverrideRecords => ({
  disabledAgents: [],
  modelOverrides: {},
  prewalk: {},
  advisor: {},
});

const stringArray = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === 'string') : [];

const stringRecord = (value: unknown): Record<string, string> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return {};
  const out: Record<string, string> = {};
  for (const [name, entry] of Object.entries(value)) {
    if (typeof entry === 'string' && entry.length > 0) out[name] = entry;
  }
  return out;
};

const SettingsKeysSchema = z.record(z.string(), z.object({ value: z.unknown().optional() }));

const SettingsSnapshotKeysSchema = z.object({ keys: SettingsKeysSchema });

/**
 * Extract the raw task.* records from a `GET /api/omp/settings` snapshot.
 * Missing/invalid entries read as their empty value — the settings face is
 * schema-driven and defaults live server-side; the form only needs the
 * currently persisted entries to compute a non-destructive full-key write.
 */
export const parseTaskOverrideRecords = (snapshot: unknown): TaskOverrideRecords => {
  const parsed = SettingsSnapshotKeysSchema.safeParse(snapshot);
  if (!parsed.success) return emptyTaskOverrideRecords();
  const valueOf = (key: string): unknown => parsed.data.keys[key]?.value;
  return {
    disabledAgents: stringArray(valueOf('task.disabledAgents')),
    modelOverrides: stringRecord(valueOf('task.agentModelOverrides')),
    prewalk: stringRecord(valueOf('task.agentPrewalk')),
    advisor: stringRecord(valueOf('task.agentAdvisor')),
  };
};

export interface TaskOverridePatch {
  /** Enable/disable the agent for task dispatch (task.disabledAgents). */
  disabled?: boolean;
  /** Model pattern override ('' clears the agent's entry). */
  modelOverride?: string;
  prewalkOverride?: string;
  advisorOverride?: string;
}

/**
 * Compute the `PUT /api/omp/settings` changes for one agent's override edit.
 * Only touched override kinds produce changes entries; every produced value
 * is the complete next key value.
 */
export const buildTaskOverrideChanges = (
  agentName: string,
  current: TaskOverrideRecords,
  patch: TaskOverridePatch,
): Record<string, unknown> => {
  const changes: Record<string, unknown> = {};
  if (patch.disabled !== undefined) {
    const disabled = new Set(current.disabledAgents);
    if (patch.disabled) disabled.add(agentName);
    else disabled.delete(agentName);
    changes['task.disabledAgents'] = [...disabled];
  }
  const withEntry = (key: string, record: Record<string, string>, value: string | undefined): void => {
    if (value === undefined) return;
    const next = { ...record };
    if (value.trim() === '') delete next[agentName];
    else next[agentName] = value.trim();
    changes[key] = next;
  };
  withEntry('task.agentModelOverrides', current.modelOverrides, patch.modelOverride);
  withEntry('task.agentPrewalk', current.prewalk, patch.prewalkOverride);
  withEntry('task.agentAdvisor', current.advisor, patch.advisorOverride);
  return changes;
};

/** Fold a successful PUT's `applied` echo back into the local records. */
export const applyTaskOverrideChanges = (
  current: TaskOverrideRecords,
  applied: Record<string, unknown>,
): TaskOverrideRecords => {
  const next = { ...current };
  if ('task.disabledAgents' in applied) next.disabledAgents = stringArray(applied['task.disabledAgents']);
  if ('task.agentModelOverrides' in applied) next.modelOverrides = stringRecord(applied['task.agentModelOverrides']);
  if ('task.agentPrewalk' in applied) next.prewalk = stringRecord(applied['task.agentPrewalk']);
  if ('task.agentAdvisor' in applied) next.advisor = stringRecord(applied['task.agentAdvisor']);
  return next;
};
