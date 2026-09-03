// omp-parity domain module: model selection + model roles (spec 01) and the
// settings proxy (spec 06) — server side, self-contained.
//
// Owns (per chapter specs and master rulings):
// - Per-directory keyed Settings instances (06 §5.1 REVISED R2, master R6):
//   a boot instance is the single global-write executor AND the keyed
//   instance for the boot directory; every other directory gets a
//   cloneForCwd-derived instance that shares the boot storage handle and
//   configPath but loads that directory's `.omp/config.yml` project layer.
//   Sessions in a directory consume exactly this instance via
//   `createAgentSession({ settings })` (sdk.ts:1273-1275 injection point).
//   `reloadForCwd` is never referenced (R6).
// - GET /omp/models payload (01 §5.3(1)): roles snapshot + cycleOrder +
//   enabledModels + fallbackChains + legacyDefaults (R12 read-only detect).
// - GET/PUT /omp/settings (06 §5.2/§5.3): schema-driven settings proxy.
//   GET returns directory-scoped effective settings with every credential
//   key (isCredential, incl. ui.secret — schema:5628-5631) reduced to
//   `{ configured }`; values AND defaults never echo (R9). PUT validates
//   against SETTINGS_SCHEMA, routes global writes to the boot instance and
//   project writes ONLY within the modelRoles subtree to the directory's
//   keyed instance (R6), flushes, bumps revision, and broadcasts
//   `omp.settings.updated` (registered in omp-event-registry.json; the
//   publish callback is wired by the coordinator to ompBus).
// - defaultModel legacy migration (01 §5.8, R12): read-only detect of the
//   OMPChamber defaultModel + explicit import that writes
//   modelRoles.default only when unset (never overwrites).
//
// Integration contract for the coordinator (this module never touches
// engine.js / endpoints.js / omp-parity.js):
// - flip `modelRoles.v1`, `settings.v1`, `settings.projectScopes.v1` in
//   omp-parity.js `ompFeatures()` when mounting these routes
//   (see CAPABILITY_KEYS below);
// - engine boot: `settingsStore = await createSettingsStore({ cwd, agentDir })`
//   (or pass an existing boot Settings instance), then inject
//   `settings: await settingsStore.settingsFor(directoryKey)` into
//   `createAgentSession` options inside `#materialize` (engine.js:440-484);
// - route mounting: `registerModelSettingsRoutes(route, { store, publish })`
//   on the omp-host route table (public paths /api/omp/models,
//   /api/omp/settings — the web proxy strips /api).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Settings, VERSION } from '@oh-my-pi/pi-coding-agent';
import {
  SETTINGS_SCHEMA,
  SETTING_TABS,
  TAB_METADATA,
  TAB_GROUPS,
  getDefault,
  getEnumValues,
  getType,
  getUi,
  isCredential,
} from '@oh-my-pi/pi-coding-agent/config/settings';
import { getKnownRoleIds, getRoleInfo } from '@oh-my-pi/pi-coding-agent/config/model-roles';
import { parseModelString } from '@oh-my-pi/pi-coding-agent/config/model-resolver';
import { getRetryFallbackChains } from '@oh-my-pi/pi-coding-agent/session/retry-fallback-chains';
import { normalizeDirectoryKey } from './registry.ts';
import type {
  AnyUiMetadata,
  SettingsOptions,
  SettingPath,
  SettingTab,
  SettingValue,
  SubmenuOption,
} from '@oh-my-pi/pi-coding-agent/config/settings';
import type { ModelRoleInfo } from '@oh-my-pi/pi-coding-agent/config/model-roles';
import type { RetryFallbackChains } from '@oh-my-pi/pi-coding-agent/session/retry-fallback-chains';
export type { SettingValue } from '@oh-my-pi/pi-coding-agent/config/settings';

/** omp-parity.js feature keys this surface reports (coordinator flips them). */
export const CAPABILITY_KEYS = {
  models: 'modelRoles.v1',
  settings: 'settings.v1',
  settingsProjectScopes: 'settings.projectScopes.v1',
};

// ─────────────────────────────────────────────────────────────────────────────
// Settings store: per-directory keyed instances (06 §5.1 REVISED R2, R6)
// ─────────────────────────────────────────────────────────────────────────────

/**
 * JSON value the omp settings wire and applied-change reports carry (same
 * shape contract as domain-plugins.ts JsonValue).
 */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/** Open string-keyed JSON object: submitted changes and applied echoes are
 * keyed by setting paths neither side enumerates up front, so the record
 * stays intentionally open while its values remain concrete JSON. */
type JsonRecord = Record<string, JsonValue>;

/**
 * The store surface these routes and applySettingsChanges consume. The
 * coordinator passes createSettingsStore's return or a same-shape wrapper
 * (endpoints.ts forwards the engine's store lazily).
 */
export interface SettingsStoreSurface {
  boot: Settings;
  bootDirectory: string;
  settingsFor(directoryKey?: string): Promise<Settings>;
  getRevision(): number;
  bumpRevision(): number;
  /** Serialized write: resolves the task's applied record, or void when a
   * lazy wrapper (endpoints.ts) runs without a backing store. */
  chainWrites(targetKey: string, task: () => Promise<JsonRecord>): Promise<JsonRecord | void>;
  invalidateDerived(): Promise<void>;
}

/** createSettingsStore's return: the surface plus teardown. */
export interface SettingsStore extends SettingsStoreSurface {
  disposeAll(): Promise<Settings[]>;
}

/** Runtime discrimination for createSettingsStore's boot argument: a live
 * boot Settings instance carries instance methods (cloneForCwd); a plain
 * SettingsOptions object — or a nullish runtime input routed through
 * Settings.init — never does. */
const isLiveSettings = (value: Settings | SettingsOptions | undefined): value is Settings =>
  value != null && 'cloneForCwd' in value && typeof value.cloneForCwd === 'function';

/**
 * Build the per-directory keyed Settings topology.
 *
 * @param {Settings | { cwd?: string, agentDir?: string }} bootSettingsIsh
 *   Either a live boot `Settings` instance (used as-is: it doubles as the
 *   global-write executor and the boot directory's keyed instance), or
 *   SettingsOptions routed through `Settings.init` (the process singleton
 *   path — spec 06 §5.1.1: the boot directory is bound once and never
 *   re-scoped).
 * @returns {Promise<SettingsStore>}
 */
export const createSettingsStore = async (
  bootSettingsIsh: Settings | SettingsOptions,
): Promise<SettingsStore> => {
  const boot = isLiveSettings(bootSettingsIsh)
    ? bootSettingsIsh
    : await Settings.init(bootSettingsIsh ?? {});
  const bootDirectory = normalizeDirectoryKey(boot.getCwd());
  const byDirectory = new Map<string, Settings>();
  const deriving = new Map<string, Promise<Settings>>();
  let ownsBoot = boot !== bootSettingsIsh;
  let revision = 0;
  // Bumped whenever cached clones are dropped. A clone derivation that
  // straddles an invalidation must not re-cache itself: it would pin the
  // pre-invalidation global layer (cloneForCwd structuredClones boot's
  // global at derivation start, settings.ts:607-625).
  let derivedEpoch = 0;
  /** Per-target write chains (06 §5.3.7: per-directory promise chaining). */
  const writeChains = new Map<string, Promise<JsonRecord | void>>();

  const settingsFor = async (directoryKey?: string): Promise<Settings> => {
    const key = directoryKey ? normalizeDirectoryKey(directoryKey) : bootDirectory;
    if (key === bootDirectory) return boot;
    const cached = byDirectory.get(key);
    if (cached) return cached;
    let pending = deriving.get(key);
    if (!pending) {
      // cloneForCwd (settings.ts:607-625): shares the boot storage handle and
      // configPath, re-loads this directory's project layer, does not run the
      // full #load (no agent.db / migrations / marker files), and does not
      // mutate the boot instance.
      const epochAtDerive = derivedEpoch;
      pending = boot.cloneForCwd(key).then((clone) => {
        if (epochAtDerive === derivedEpoch && !byDirectory.has(key)) byDirectory.set(key, clone);
        return clone;
      });
      pending.finally(() => deriving.delete(key)).catch(() => {});
      deriving.set(key, pending);
    }
    return pending;
  };

  return {
    boot,
    bootDirectory,
    settingsFor,

    getRevision: () => revision,
    bumpRevision: () => {
      revision += 1;
      return revision;
    },

    /** Serialize writes per target instance key (boot vs directory). */
    chainWrites(targetKey: string, task: () => Promise<JsonRecord>): Promise<JsonRecord | void> {
      const previous = writeChains.get(targetKey) ?? Promise.resolve();
      const next = previous.then(task, task);
      writeChains.set(targetKey, next.catch(() => {}));
      return next;
    },

    /**
     * Drop every cached non-boot clone so the next `settingsFor(dir)`
     * re-derives from boot's CURRENT global layer plus that directory's
     * project file. Called after global-scope writes: clones snapshot the
     * global layer at derivation time, so without this every directory that
     * already has a keyed instance reads the write back stale — the roles
     * editor, `GET /omp/settings|models`, and new-session role resolution
     * for that directory (spec 06 §5.1.7b: only already-live sessions keep
     * their injected pre-write instance; 01 §6.3: a global role write must
     * reach new sessions in every directory). Each clone is flushed first
     * so an in-flight project-layer write is not lost; a failed flush keeps
     * its debounce armed — the write still persists through the same
     * in-lock per-key merge (06 §3.3), so invalidation never drops writes.
     */
    invalidateDerived: async () => {
      derivedEpoch += 1;
      const clones = [...byDirectory.values()];
      byDirectory.clear();
      deriving.clear();
      for (const clone of clones) {
        try {
          await clone.flush();
        } catch {
          // best-effort — the discarded clone's armed debounce timer is
          // left to retry through the write lock; cancelPendingSaves here
          // would drop the pending project write.
        }
      }
    },

    /**
     * Teardown: flush + disarm every derived clone so no armed debounce
     * timer races a successor's file locks (Settings.cancelPendingSaves
     * contract). A caller-provided boot instance is flushed but left armed —
     * its owner decides its lifetime. Repeated settingsFor calls after
     * disposeAll re-derive fresh instances that reload the project layer
     * from disk.
     */
    disposeAll: async () => {
      derivedEpoch += 1;
      const clones = [...byDirectory.values()];
      byDirectory.clear();
      deriving.clear();
      writeChains.clear();
      const disarm: Settings[] = [];
      for (const clone of clones) {
        try {
          await clone.flush();
        } catch {
          // best-effort teardown
        }
        clone.cancelPendingSaves();
        disarm.push(clone);
      }
      if (ownsBoot) {
        try {
          await boot.flush();
        } catch {
          // best-effort teardown
        }
        boot.cancelPendingSaves();
        ownsBoot = false;
      }
      return disarm;
    },
  };
};


// ─────────────────────────────────────────────────────────────────────────────
// Role value parsing (SDK model-selector format "provider/id[:thinking]")
// ─────────────────────────────────────────────────────────────────────────────

const parseRoleModelValue = (value: string) => {
  if (value === '') return null;
  // Multi-model role values (comma-joined by the SDK's loader) report the
  // primary selector; the full configured string is echoed as `configured`.
  const primary = value.split(',')[0];
  const parsed = parseModelString(primary);
  if (!parsed) return null;
  return parsed;
};

// ─────────────────────────────────────────────────────────────────────────────
// GET /omp/models payload (01 §5.3(1))
// ─────────────────────────────────────────────────────────────────────────────

/** Minimal registry-model surface the models payload projects from. */
export interface RegistryModel {
  provider: string;
  id: string;
  name?: string;
  reasoning?: boolean;
  /** SDK Model carries `| null` for unknown sizes; projections filter via Number.isFinite. */
  contextWindow?: number | null;
  maxTokens?: number | null;
  thinking?: { efforts?: readonly string[]; defaultLevel?: string | null };
}

/** Legacy OMPChamber defaultModel detect/import result (01 §5.8, R12). */
export interface LegacyDefaultModel {
  defaultModel: string;
  defaultProvider?: string;
}

/** Per-role entry in the GET /omp/models payload (assignment contract). */
export interface ModelRoleEntry {
  configured: string;
  provider: string | null;
  id: string | null;
  thinkingLevel?: string;
  source: 'project' | 'global' | 'default';
}

/** Registry model → wire projection: identity + baked thinking surface. */
export interface ModelThinkingProjection {
  provider?: string;
  id?: string;
  name?: string;
  reasoning: boolean;
  contextWindow?: number;
  maxTokens?: number;
  thinking: { supported: string[]; defaultLevel: string | null };
}

export interface BuildModelsOptions {
  legacyDefaults?: LegacyDefaultModel | null;
  models?: readonly RegistryModel[] | null;
}

export interface ModelsPayload {
  schemaVersion: typeof VERSION;
  directory: string;
  models?: ModelThinkingProjection[];
  roles: Record<string, ModelRoleEntry | null>;
  roleMeta: Record<string, ModelRoleInfo>;
  cycleOrder: SettingValue<'cycleOrder'>;
  enabledModels: SettingValue<'enabledModels'>;
  fallbackChains: RetryFallbackChains;
  modelRoleStorage: SettingValue<'modelRoleStorage'>;
  defaultThinkingLevel: SettingValue<'defaultThinkingLevel'>;
  legacyDefaults: LegacyDefaultModel | null;
}

/**
 * Model + roles snapshot for a directory's keyed Settings instance.
 *
 * Per-role entries are `{ provider, id }`-shaped objects (assignment
 * contract) carrying the configured string, its explicit thinking selector,
 * and the persisted source layer; unconfigured roles map to `null`.
 * `resolved`-style full model resolution against the registry belongs to the
 * engine's `roleSnapshot` (01 §5.3(1) — needs availableModels); this payload
 * is the settings-side truth. When `models` (engine registry models) is
 * supplied, a `models[]` projection with thinking metadata is included
 * (01 §5.3(1)/§5.4 GAP-06).
 *
 * @param {Settings} settings
 * @param {{ legacyDefaults?: { defaultModel: string, defaultProvider?: string } | null, models?: Array<object> }} [options]
 */
/** Registry model → wire projection: identity + baked thinking surface. */
export const projectModelThinking = (model: RegistryModel | null | undefined): ModelThinkingProjection => ({
  provider: model?.provider,
  id: model?.id,
  ...(model?.name ? { name: model.name } : {}),
  reasoning: Boolean(model?.reasoning),
  ...(model?.contextWindow !== undefined && model.contextWindow !== null && Number.isFinite(model.contextWindow) ? { contextWindow: model.contextWindow } : {}),
  ...(model?.maxTokens !== undefined && model.maxTokens !== null && Number.isFinite(model.maxTokens) ? { maxTokens: model.maxTokens } : {}),
  thinking: {
    // Mirrors the TUI's getSupportedEfforts: a non-reasoning model has no
    // effort surface (empty list), reasoning models read baked efforts.
    supported: model?.reasoning ? [...(model?.thinking?.efforts ?? [])] : [],
    defaultLevel: model?.thinking?.defaultLevel ?? null,
  },
});

export const buildModelsPayload = (
  settings: Settings,
  { legacyDefaults = null, models = null }: BuildModelsOptions = {},
): ModelsPayload => {
  const roles: Record<string, ModelRoleEntry | null> = {};
  const roleMeta: Record<string, ModelRoleInfo> = {};
  for (const role of getKnownRoleIds(settings)) {
    const configured = settings.getModelRole(role) ?? null;
    const parsed = configured ? parseRoleModelValue(configured) : null;
    roles[role] = configured
      ? {
          configured,
          provider: parsed?.provider ?? null,
          id: parsed?.id ?? null,
          ...(parsed?.thinkingLevel ? { thinkingLevel: parsed.thinkingLevel } : {}),
          source: settings.getModelRoleSource(role),
        }
      : null;
    const info = getRoleInfo(role, settings);
    roleMeta[role] = {
      ...(info.tag ? { tag: info.tag } : {}),
      name: info.name,
      ...(info.color ? { color: info.color } : {}),
      ...(info.hidden ? { hidden: true } : {}),
    };
  }
  return {
    schemaVersion: VERSION,
    directory: settings.getCwd(),
    ...(models ? { models: models.map(projectModelThinking) } : {}),
    roles,
    roleMeta,
    cycleOrder: settings.get('cycleOrder'),
    enabledModels: settings.get('enabledModels'),
    fallbackChains: getRetryFallbackChains(settings),
    modelRoleStorage: settings.get('modelRoleStorage'),
    defaultThinkingLevel: settings.get('defaultThinkingLevel'),
    legacyDefaults,
  };
};

// ─────────────────────────────────────────────────────────────────────────────
// Legacy defaultModel migration (01 §5.8, master R12)
// ─────────────────────────────────────────────────────────────────────────────

const ompchamberSettingsPath = () =>
  path.join(os.homedir(), '.config', 'ompchamber', 'settings.json');

/**
 * Read-only detect of the OMPChamber legacy `defaultModel`
 * (`~/.config/ompchamber/settings.json`, same path the web server reads).
 * Never writes any omp configuration. Only a non-empty value containing "/"
 * is reported (mirroring settings-normalization-runtime.js:177 which keeps
 * project defaultModel only when parseable).
 *
 * @param {{ settingsPath?: string }} [options]
 * @returns {{ defaultModel: string, defaultProvider?: string } | null}
 */
export const detectLegacyDefaultModel = ({ settingsPath }: { settingsPath?: string } = {}): LegacyDefaultModel | null => {
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(settingsPath ?? ompchamberSettingsPath(), 'utf8'));
  } catch {
    return null;
  }
  const raw = typeof parsed?.defaultModel === 'string' ? parsed.defaultModel.trim() : '';
  if (!raw || !raw.includes('/')) return null;
  const model = parseRoleModelValue(raw);
  return {
    defaultModel: raw,
    ...(model?.provider ? { defaultProvider: model.provider } : {}),
  };
};


/** importLegacyDefaultModel result: success audit or refusal reason. */
export interface LegacyImportResult {
  imported: boolean;
  role?: 'default';
  value?: string;
  scope?: 'global' | 'project';
  audit?: { originalValue: string; importedRole: 'default'; scope: string; at: string };
  reason?: 'role-already-configured' | 'invalid-selector';
  existing?: string;
}

/**
 * Explicit R12 import: write `modelRoles.default` ONLY when unset
 * (never overwrites — the caller maps `role-already-configured` to 409).
 * Honors the instance's `modelRoleStorage` ('project' → project layer on
 * this instance's cwd, else the global layer); the caller must pass the boot
 * instance for global imports / the directory's keyed instance for project
 * imports. Flushes before returning. The audit record is returned for the
 * coordinator to persist into OC settings.json (omp-host never writes that
 * file itself).
 *
 * @param {Settings} settings
 * @param {string} selector "provider/model" (optionally ":thinking")
 * @returns {Promise<
 *   | { imported: true, role: 'default', value: string, scope: 'global' | 'project', audit: { originalValue: string, importedRole: 'default', scope: string, at: string } }
 *   | { imported: false, reason: 'role-already-configured' | 'invalid-selector', existing?: string }
 * >}
 */
export const importLegacyDefaultModel = async (settings: Settings, selector: string): Promise<LegacyImportResult> => {
  if (settings.getModelRole('default')) {
    return { imported: false, reason: 'role-already-configured', existing: settings.getModelRole('default') };
  }
  const parsed = parseRoleModelValue(selector);
  if (!parsed || !parsed.provider || !parsed.id) {
    return { imported: false, reason: 'invalid-selector' };
  }
  const scope = settings.get('modelRoleStorage') === 'project' ? 'project' : 'global';
  if (scope === 'project') {
    settings.setProjectModelRole('default', selector);
  } else {
    settings.setModelRole('default', selector);
  }
  await settings.flush();
  return {
    imported: true,
    role: 'default',
    value: selector,
    scope,
    audit: {
      originalValue: selector,
      importedRole: 'default',
      scope,
      at: new Date().toISOString(),
    },
  };
};

// ─────────────────────────────────────────────────────────────────────────────
// Settings GET payload (06 §5.2)
// ─────────────────────────────────────────────────────────────────────────────

/** Terminal-rendering surfaces never editable from the web (06 §5.6). */
const EXCLUDED_TABS = new Set(['appearance']);
const EXCLUDED_PREFIXES = ['tui.', 'terminal.', 'statusLine.', 'display.'];
const EXCLUDED_PREFIX_ALLOWLIST = new Set(['display.collapseCompacted']);

/**
 * Server-side mirror of the TUI CONDITIONS table (modes/components/
 * settings-defs.ts:96-147) — every entry is a pure Settings read. Unknown
 * condition names evaluate visible (spec 06 §5.2: 求值失败 → 显示但不隐藏).
 * `hasImageProtocol` is a terminal capability the web never has.
 */
interface ConditionEvaluatorTable {
  [condition: string]: (s: Settings) => boolean;
}

const CONDITION_EVALUATORS: ConditionEvaluatorTable = {
  hasImageProtocol: () => false,
  advisorEnabled: (s) => s.get('advisor.enabled') === true,
  hindsightActive: (s) => s.get('memory.backend') === 'hindsight',
  mnemopiActive: (s) => s.get('memory.backend') === 'mnemopi',
  autolearnActive: (s) => s.get('autolearn.enabled') === true,
  autoThinkingActive: (s) => s.get('defaultThinkingLevel') === 'auto',
  usageAwareFallbackEnabled: (s) => s.get('retry.usageAwareFallback') === true,
  planModeEnabled: (s) => Boolean(s.get('plan.enabled')),
};

const uiProjection = (ui: AnyUiMetadata | undefined): UiProjection | undefined => {
  if (!ui) return undefined;
  const out: UiProjection = {};
  if (ui.tab !== undefined) out.tab = ui.tab;
  if (ui.group !== undefined) out.group = ui.group;
  if (ui.label !== undefined) out.label = ui.label;
  if (ui.description !== undefined) out.description = ui.description;
  if (ui.condition !== undefined) out.condition = ui.condition;
  if (ui.secret !== undefined) out.secret = ui.secret;
  if (ui.ordered !== undefined) out.ordered = ui.ordered;
  if (ui.options !== undefined) {
    // TUI resolves `options: "runtime"` through its theme registry; the web
    // cannot (06 §5.2) — flag it instead of guessing.
    out.options = ui.options === 'runtime' ? 'runtime-unresolved' : ui.options;
  }
  return Object.keys(out).length > 0 ? out : undefined;
};

const exclusionFor = (keyPath: SettingPath): 'terminal-only' | 'terminal-capability' | null => {
  const ui = getUi(keyPath);
  // Most specific marker first: the hasImageProtocol condition is a terminal
  // capability the web never has (06 §5.2 → excluded:"terminal-capability").
  if (ui?.condition === 'hasImageProtocol') return 'terminal-capability';
  if (ui?.tab && EXCLUDED_TABS.has(ui.tab)) return 'terminal-only';
  if (EXCLUDED_PREFIXES.some((prefix) => keyPath.startsWith(prefix)) && !EXCLUDED_PREFIX_ALLOWLIST.has(keyPath)) {
    return 'terminal-only';
  }
  return null;
};

/** Render a schema setting value onto the JSON wire: JSON has no undefined,
 * so absent becomes null and everything else passes through unchanged. */
const jsonValue = (value: SettingValue<SettingPath>): JsonValue => (value === undefined ? null : value);

/** UI metadata projection carried on settings payload entries (06 §5.2). */
export interface UiProjection {
  tab?: string;
  group?: string;
  label?: string;
  description?: string;
  condition?: string;
  secret?: boolean;
  ordered?: boolean;
  options?: readonly SubmenuOption[] | 'runtime-unresolved';
}

/** Per-role view inside the modelRoles record entry. */
export interface ModelRoleValueView {
  value: string | null;
  source: 'project' | 'global' | 'default';
  editable: boolean;
}

/**
 * One wire entry under GET /omp/settings `keys`. Schema-driven entries and
 * the modelRoles record view share this shape; absent fields are omitted.
 */
export interface SettingsPayloadEntry {
  type: 'boolean' | 'string' | 'number' | 'enum' | 'array' | 'record';
  default?: unknown;
  value: unknown;
  scope: string;
  values?: string[];
  configured?: boolean;
  editable?: boolean;
  credential?: true;
  writeOnly?: true;
  ui?: UiProjection;
  excluded?: 'terminal-only' | 'terminal-capability';
  hidden?: true;
  roles?: Record<string, ModelRoleValueView>;
  modelRoleStorage?: string;
}

export interface SettingsPayload {
  schemaVersion: typeof VERSION;
  directory: string;
  agentDir: string;
  globalConfigPath: string;
  projectConfigPath: string;
  revision: number;
  tabs: { id: SettingTab; label: string; groups: string[] }[];
  keys: Record<string, SettingsPayloadEntry>;
}

/**
 * Schema-driven settings snapshot for one directory (the same keyed instance
 * that directory's sessions consume). Credential keys (isCredential, incl.
 * ui.secret) never echo value or default (R9) — only `configured`.
 *
 * @param {Settings} settings
 * @param {{ revision?: number, keys?: string[] | null }} [options]
 */
export const buildSettingsPayload = (
  settings: Settings,
  { revision = 0, keys = null }: { revision?: number; keys?: string[] | null } = {},
): SettingsPayload => {
  const wanted = keys && keys.length > 0 ? new Set(keys) : null;
  const entries: Record<string, SettingsPayloadEntry> = {};
  // SAFETY: Object.keys returns SETTINGS_SCHEMA's own enumerable keys,
  // which are exactly its declared SettingPath entries.
  for (const keyPath of Object.keys(SETTINGS_SCHEMA) as SettingPath[]) {
    if (wanted && !wanted.has(keyPath)) continue;
    if (keyPath === 'modelRoles') continue; // special record view below
    const credential = isCredential(keyPath);
    const excluded = exclusionFor(keyPath);
    const ui = getUi(keyPath);
    let hidden = false;
    if (ui?.condition && !excluded) {
      const evaluate = CONDITION_EVALUATORS[ui.condition];
      hidden = evaluate ? !evaluate(settings) : false;
    }
    entries[keyPath] = {
      type: getType(keyPath),
      ...(() => { const values = getEnumValues(keyPath); return values ? { values: [...values] } : {}; })(),
      default: credential ? null : jsonValue(getDefault(keyPath)),
      value: credential ? null : jsonValue(settings.get(keyPath)),
      configured: settings.isConfigured(keyPath),
      scope: 'global',
      editable: !excluded && !hidden,
      ...(credential ? { credential: true, writeOnly: true } : {}),
      ...(uiProjection(ui) ? { ui: uiProjection(ui) } : {}),
      ...(excluded ? { excluded } : {}),
      ...(hidden ? { hidden: true } : {}),
    };
  }
  if (!wanted || wanted.has('modelRoles')) {
    const roles: Record<string, ModelRoleValueView> = {};
    for (const role of getKnownRoleIds(settings)) {
      roles[role] = {
        value: settings.getModelRole(role) ?? null,
        source: settings.getModelRoleSource(role),
        editable: true,
      };
    }
    entries.modelRoles = {
      type: 'record',
      value: { ...settings.getModelRoles() },
      roles,
      modelRoleStorage: settings.get('modelRoleStorage'),
      scope: 'global+project',
    };
  }
  return {
    schemaVersion: VERSION,
    directory: settings.getCwd(),
    agentDir: settings.getAgentDir(),
    globalConfigPath: path.join(settings.getAgentDir(), 'config.yml'),
    projectConfigPath: path.join(settings.getCwd(), '.omp', 'config.yml'),
    revision,
    tabs: SETTING_TABS.map((id) => ({
      id,
      label: TAB_METADATA[id].label,
      groups: [...TAB_GROUPS[id]],
    })),
    keys: entries,
  };
};

// ─────────────────────────────────────────────────────────────────────────────
// PUT /omp/settings (06 §5.3)
// ─────────────────────────────────────────────────────────────────────────────

const MODEL_ROLES_PREFIX = 'modelRoles.';

const isModelRoleKey = (key: string): boolean =>
  typeof key === 'string' && key.startsWith(MODEL_ROLES_PREFIX) && key.length > MODEL_ROLES_PREFIX.length;

const validModelRoleName = (role: string): boolean => role.length > 0 && !role.includes('.');

const validateSettingValue = (keyPath: SettingPath, value: JsonValue): string | null => {
  if (value === null) return null; // null always means "clear"
  if (isCredential(keyPath)) {
    return typeof value === 'string' ? null : 'invalid-type';
  }
  const type = getType(keyPath);
  switch (type) {
    case 'enum': {
      const values = getEnumValues(keyPath);
      return values && !values.some((entryValue) => entryValue === value) ? 'invalid-value' : null;
    }
    case 'boolean':
      return typeof value === 'boolean' ? null : 'invalid-type';
    case 'number':
      return typeof value === 'number' && Number.isFinite(value) ? null : 'invalid-type';
    case 'string':
      return typeof value === 'string' ? null : 'invalid-type';
    case 'array':
      return Array.isArray(value) ? null : 'invalid-type';
    case 'record':
      return typeof value === 'object' && value !== null && !Array.isArray(value) ? null : 'invalid-type';
    default:
      return null;
  }
};

const quarantinePathFromError = (error: { message?: unknown }): string | null => {
  const match = /moved to (\S+)/.exec(String(error?.message ?? error ?? ''));
  return match ? match[1] : null;
};

/** PUT /omp/settings request body (shape-checked at runtime). */
export interface SettingsChangesInput {
  directory?: string;
  scope?: string;
  changes?: JsonRecord;
}

/** omp.settings.updated publisher (wired to ompBus.publish by the coordinator). */
export type PublishFn = (
  type: string,
  payload: JsonRecord,
  eventScope: { directory: string; durable?: boolean },
) => void;

export interface ApplySettingsHooks {
  publish?: PublishFn;
}

export interface SettingsChangeResult {
  status: number;
  body: {
    error?: string;
    rejected?: { key: string; reason: string }[];
    revision?: number;
    applied?: JsonRecord;
    persisted?: boolean;
    quarantined?: string | null;
    quarantinedTo?: string;
    keys?: string[];
  };
}

type SettingsPlanItem =
  | { kind: 'role'; key: string; role: string; value: string | null }
  | { kind: 'setting'; key: SettingPath; value: JsonValue | undefined; clearing: boolean };


/**
 * Apply a PUT /omp/settings request.
 *
 * Validation (rule 1): every key must exist in SETTINGS_SCHEMA (special
 * `modelRoles.<role>` syntax for per-role writes; the bare `modelRoles`
 * record is rejected — the SDK merges roles per-key, whole-record writes
 * would clobber sibling roles). Rejected entries carry key + reason only,
 * never the submitted value (R9).
 *
 * Write routing (rule 2, R6): `scope:"global"` always executes on the boot
 * instance — the single global-write executor — regardless of `directory`;
 * `scope:"project"` accepts ONLY `modelRoles.<role>` keys (the omp project
 * layer authoritatively carries just the modelRoles subtree) and executes on
 * that directory's keyed instance (`null` clears via clearProjectModelRole).
 *
 * @param {ReturnType<typeof createSettingsStore>} store
 * @param {{ directory?: string, scope?: string, changes?: JsonRecord }} input
 * @param {{ publish?: (type: string, payload: JsonRecord, scope: { directory: string, durable?: boolean }) => void }} [hooks]
 * @returns {Promise<SettingsChangeResult>}
 */
export const applySettingsChanges = async (
  store: SettingsStoreSurface,
  input: SettingsChangesInput | Record<string, never>,
  { publish }: ApplySettingsHooks = {},
): Promise<SettingsChangeResult> => {
  // SAFETY: route JSON is untyped; this read view is validated field by
  // field below (scope literal, changes object) before anything is used.
  const body = (input !== null && typeof input === 'object' ? input : {}) as SettingsChangesInput;
  const scope = body?.scope ?? 'global';
  if (scope !== 'global' && scope !== 'project') {
    return { status: 400, body: { error: 'invalid-scope' } };
  }
  const changes = body?.changes;
  if (!changes || typeof changes !== 'object' || Array.isArray(changes)) {
    return { status: 400, body: { error: 'invalid-body' } };
  }

  const rejected: { key: string; reason: string }[] = [];
  const plan: SettingsPlanItem[] = [];
  for (const [key, value] of Object.entries(changes)) {
    if (key === 'modelRoles') {
      rejected.push({ key, reason: 'record-write-unsupported' });
      continue;
    }
    if (isModelRoleKey(key)) {
      const role = key.slice(MODEL_ROLES_PREFIX.length);
      // Rejection precedence: invalid-role > invalid-type > invalid-value.
      // Branches narrow via typeof (strict:false drops null/undefined
      // equality narrowing) with identical outcomes.
      if (!validModelRoleName(role)) {
        rejected.push({ key, reason: 'invalid-role' });
      } else if (typeof value === 'string') {
        if (value === '') {
          rejected.push({ key, reason: 'invalid-value' });
        } else {
          plan.push({ kind: 'role', key, role, value });
        }
      } else if (value === null) {
        plan.push({ kind: 'role', key, role, value: null });
      } else {
        rejected.push({ key, reason: 'invalid-type' });
      }
      continue;
    }
    if (!(key in SETTINGS_SCHEMA)) {
      rejected.push({ key, reason: 'unknown' });
      continue;
    }
    // SAFETY: the `in` guard above proved `key` is one of SETTINGS_SCHEMA's
    // declared keys — this single bridge covers every SettingPath use below.
    const settingKey = key as SettingPath;
    if (exclusionFor(settingKey)) {
      rejected.push({ key, reason: 'not-editable' });
      continue;
    }
    if (scope === 'project') {
      rejected.push({ key, reason: 'project-scope-model-roles-only' });
      continue;
    }
    const reason = validateSettingValue(settingKey, value);
    if (reason) {
      rejected.push({ key, reason });
      continue;
    }
    // TUI text-editor convention (settings-selector): an empty string on a
    // credential key clears it; null clears any key.
    const clearing = value === null || (value === '' && isCredential(settingKey));
    plan.push({ kind: 'setting', key: settingKey, value: clearing ? undefined : value, clearing });
  }
  if (rejected.length > 0) {
    return { status: 400, body: { error: 'validation', rejected } };
  }

  const directoryKey = body?.directory
    ? normalizeDirectoryKey(body.directory)
    : store.bootDirectory;
  const target = scope === 'project' ? await store.settingsFor(directoryKey) : store.boot;

  if (plan.length === 0) {
    // No-op PUT (e.g. `{}` changes): idempotent success, no revision bump,
    // no keys-empty event.
    return { status: 200, body: { revision: store.getRevision(), applied: {}, persisted: true, quarantined: null } };
  }

  try {
    // All global writes execute on the boot instance — serialize them on one
    // chain; project writes serialize per directory.
    // SAFETY: the task above always resolves its appliedNow record; the void
    // arm exists only for the lazy endpoints wrapper running without a
    // backing store — there Object.keys below must keep throwing into the
    // same catch, exactly as before.
    const applied = (await store.chainWrites(scope === 'global' ? 'global' : `project:${directoryKey}`, async () => {
      const appliedNow: JsonRecord = {};
      for (const item of plan) {
        if (item.kind === 'role') {
          if (scope === 'project') {
            if (item.value === null) target.clearProjectModelRole(item.role);
            else target.setProjectModelRole(item.role, item.value);
          } else {
            target.setModelRole(item.role, item.value === null || item.value === '' ? undefined : item.value);
          }
          appliedNow[item.key] = target.getModelRole(item.role) ?? null;
        } else {
          // SAFETY: validateSettingValue cleared this key's value shape
          // before it entered the plan, so the wire value already satisfies
          // the schema type Settings.set demands for this path.
          target.set(item.key, item.value as SettingValue<SettingPath>);
          if (isCredential(item.key)) {
            appliedNow[item.key] = { configured: target.isConfigured(item.key) };
          } else {
            appliedNow[item.key] = jsonValue(target.get(item.key));
          }
        }
      }
      await target.flush();
      return appliedNow;
    })) as JsonRecord;

    // Global writes execute on boot while cached per-directory clones hold a
    // structuredClone'd global snapshot from their derivation time — drop
    // them so the next read/session for those directories re-derives with
    // the post-write layer (roles editor read-after-write, GET /omp/models|settings,
    // new-session role resolution; 06 §5.1.7b / 01 §6.3). Before the publish:
    // event-driven refetches must observe the fresh value. Project-scope
    // writes need no invalidation — only their own directory's instance
    // consumes that layer, and that instance applied the write in memory.
    if (scope === 'global') await store.invalidateDerived();
    const revision = store.bumpRevision();
    // Spec 05 §5.0.2 envelope normalization: directory lives on the envelope,
    // not the payload (redundant copies are dropped).
    publish?.('omp.settings.updated', {
      revision,
      keys: Object.keys(applied),
      origin: 'web',
    }, { directory: directoryKey, durable: true });
    return { status: 200, body: { revision, applied, persisted: true, quarantined: null } };
  } catch (error) {
    const quarantinedTo = quarantinePathFromError(error);
    if (quarantinedTo) {
      return { status: 409, body: { error: 'config-quarantined', quarantinedTo } };
    }
    // R9: key names only, never submitted values.
    return { status: 500, body: { error: 'settings-write-failed', keys: plan.map((item) => item.key) } };
  }
};

// ─────────────────────────────────────────────────────────────────────────────
// Route mounting (omp-host route table; public paths are /api/omp/... —
// the web proxy strips /api)
// ─────────────────────────────────────────────────────────────────────────────

const directoryFromRequest = (request: Request): string | null => {
  const raw = new URL(request.url).searchParams.get('directory');
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** omp-host route table registration function (same mechanism as registerEndpoints). */
export type OmpRouteFn = (
  method: string,
  pattern: string,
  handler: (request: Request) => Promise<Response>,
) => void;

export interface RegisterModelSettingsOptions {
  store: SettingsStoreSurface;
  publish?: PublishFn;
  legacySettingsPath?: string;
  listModels?: (() => Promise<RegistryModel[]>) | null;
}

/**
 * Mount GET /omp/models, GET /omp/settings, PUT /omp/settings on the
 * omp-host route table (same `route(method, pattern, handler)` mechanism as
 * registerEndpoints; Basic auth is enforced by host.js outside these
 * handlers). `publish` is wired by the coordinator to
 * `ompBus.publish` (durable omp.settings.updated, spec 06 §5.4).
 */
export const registerModelSettingsRoutes = (
  route: OmpRouteFn,
  { store, publish, legacySettingsPath, listModels = null }: RegisterModelSettingsOptions,
) => {
  route('GET', '/omp/models', async (request) => {
    const settings = await store.settingsFor(directoryFromRequest(request) ?? undefined);
    const legacyDefaults = detectLegacyDefaultModel(
      legacySettingsPath ? { settingsPath: legacySettingsPath } : {},
    );
    let models: RegistryModel[] | null = null;
    if (typeof listModels === 'function') {
      // Engine registry models (needs boot); failures degrade to a
      // roles-only payload, never a failed snapshot.
      try {
        models = await listModels();
      } catch {
        models = null;
      }
    }
    return Response.json(buildModelsPayload(settings, { legacyDefaults, models }));
  });

  route('GET', '/omp/settings', async (request) => {
    const url = new URL(request.url);
    const keysParam = url.searchParams.get('keys');
    const settings = await store.settingsFor(directoryFromRequest(request) ?? undefined);
    return Response.json(buildSettingsPayload(settings, {
      revision: store.getRevision(),
      keys: keysParam ? keysParam.split(',').map((k) => k.trim()).filter(Boolean) : null,
    }));
  });

  route('PUT', '/omp/settings', async (request) => {
    // SAFETY: applySettingsChanges validates scope/changes field by field.
    const body = (await request.json().catch(() => ({}))) as SettingsChangesInput;
    const { status, body: payload } = await applySettingsChanges(store, body, { publish });
    return Response.json(payload, { status });
  });
};
