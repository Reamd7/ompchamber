/**
 * omp-parity 域模块（服务端）：模型选择与 model roles（spec 01）以及
 * settings 代理（spec 06），自包含实现，绝不触碰 engine.js /
 * endpoints.js / omp-parity.js。
 *
 * 核心职责：
 * - 按目录键控的 Settings 实例拓扑（06 §5.1 REVISED R2 / master R6）：
 *   boot 实例既是唯一的全局写入执行者，也是 boot 目录的键控实例；
 *   其余目录经 cloneForCwd 派生共享同一存储句柄与 configPath 的实例，
 *   并加载该目录 `.omp/config.yml` 的 project 层。会话通过
 *   createAgentSession({ settings }) 消费所属目录的实例。
 * - GET /omp/models 载荷（01 §5.3(1)）：roles 快照 + cycleOrder +
 *   enabledModels + fallbackChains + legacyDefaults（R12 只读探测）。
 * - GET/PUT /omp/settings（06 §5.2/§5.3）：schema 驱动的 settings 代理。
 *   GET 返回目录级生效配置，所有凭据键（isCredential，含 ui.secret）
 *   只回显 `{ configured }`，值与默认值一律不回显（R9）；PUT 依据
 *   SETTINGS_SCHEMA 校验，全局写入路由到 boot 实例，project 写入仅限
 *   modelRoles 子树并路由到目录键控实例（R6），随后 flush、递增
 *   revision 并广播 omp.settings.updated 事件。
 * - defaultModel 遗留迁移（01 §5.8，R12）：只读探测 OMPChamber 的
 *   defaultModel，显式导入时仅在未设置的情况下写入 modelRoles.default，
 *   绝不覆盖已有配置。
 */
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
// 透传 SDK 的 SettingValue 类型，供路由/载荷消费方不必直接依赖 SDK 路径。
export type { SettingValue } from '@oh-my-pi/pi-coding-agent/config/settings';

/**
 * 本模块对外暴露的 omp-parity capability key：协调者在 omp-parity.js 的
 * ompFeatures() 里把 modelRoles.v1 / settings.v1 /
 * settings.projectScopes.v1 翻为 true，即宣告挂载了这组路由。
 */
/** omp-parity.js feature keys this surface reports (coordinator flips them). */
export const CAPABILITY_KEYS = {
  models: 'modelRoles.v1',
  settings: 'settings.v1',
  settingsProjectScopes: 'settings.projectScopes.v1',
};

// ─────────────────────────────────────────────────────────────────────────────
// Settings store: per-directory keyed instances (06 §5.1 REVISED R2, R6)
// ─────────────────────────────────────────────────────────────────────────────

/** omp settings 线上协议与 applied 回显携带的 JSON 值域（与 domain-plugins.ts 的 JsonValue 同一形状契约）。 */
/**
 * JSON value the omp settings wire and applied-change reports carry (same
 * shape contract as domain-plugins.ts JsonValue).
 */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/**
 * 开放式字符串键 JSON 对象：提交的变更与 applied 回显都以设置路径为键，
 * 双方都无法提前枚举，因此 record 刻意保持开放、值域限定为具体 JSON。
 */
/** Open string-keyed JSON object: submitted changes and applied echoes are
 * keyed by setting paths neither side enumerates up front, so the record
 * stays intentionally open while its values remain concrete JSON. */
type JsonRecord = Record<string, JsonValue>;

/**
 * 这些路由与 applySettingsChanges 消费的 store 表面：协调者传入
 * createSettingsStore 的返回值或同形状包装器（endpoints.ts 惰性转发
 * engine 的 store）。
 */
/**
 * The store surface these routes and applySettingsChanges consume. The
 * coordinator passes createSettingsStore's return or a same-shape wrapper
 * (endpoints.ts forwards the engine's store lazily).
 */
export interface SettingsStoreSurface {
  /** boot 目录的 Settings 实例：同时是唯一的全局写入执行者（R6）。 */
  boot: Settings;
  /** boot 实例绑定的目录键（normalizeDirectoryKey 归一化后）。 */
  bootDirectory: string;
  /** 取某目录的键控实例；缺省或命中 boot 目录时直接返回 boot。 */
  settingsFor(directoryKey?: string): Promise<Settings>;
  /** 读取当前 settings revision。 */
  getRevision(): number;
  /** 递增并返回 revision，用于成功响应与 omp.settings.updated 事件。 */
  bumpRevision(): number;
  /** 按目标实例键串行化写入：task 失败向调用方冒泡但不阻断后续排队。 */
  /** Serialized write: resolves the task's applied record, or void when a
   * lazy wrapper (endpoints.ts) runs without a backing store. */
  chainWrites(targetKey: string, task: () => Promise<JsonRecord>): Promise<JsonRecord | void>;
  invalidateDerived(): Promise<void>;
}

/** createSettingsStore 的返回类型：store 表面之外追加 disposeAll 收尾。 */
/** createSettingsStore's return: the surface plus teardown. */
export interface SettingsStore extends SettingsStoreSurface {
  /** flush 并解除所有派生 clone 的待写定时器，返回被解除武装的实例。 */
  disposeAll(): Promise<Settings[]>;
}

/**
 * 运行时判别 createSettingsStore 的 boot 入参：存活的 Settings 实例
 * 携带 cloneForCwd 实例方法；纯 SettingsOptions 对象（或经 Settings.init
 * 的 nullish 输入）永远没有。
 */
/** Runtime discrimination for createSettingsStore's boot argument: a live
 * boot Settings instance carries instance methods (cloneForCwd); a plain
 * SettingsOptions object — or a nullish runtime input routed through
 * Settings.init — never does. */
const isLiveSettings = (value: Settings | SettingsOptions | undefined): value is Settings =>
  value != null && 'cloneForCwd' in value && typeof value.cloneForCwd === 'function';

/**
 * 构建按目录键控的 Settings 拓扑并返回 store。
 *
 * 不变量：boot 实例是唯一全局写入者兼 boot 目录的键控实例；其它目录
 * 首次请求时经 cloneForCwd 派生（共享 boot 的存储句柄与 configPath，
 * 只重载该目录的 project 层）并按目录缓存；写入按目标键串行；全局写
 * 之后 invalidateDerived 丢弃缓存副本，避免各目录读到过期的全局层快照。
 */
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
  // 按目标键（global / project:<dir>）串接的写入 promise 链。
  /** Per-target write chains (06 §5.3.7: per-directory promise chaining). */
  const writeChains = new Map<string, Promise<JsonRecord | void>>();

  // 读取（或派生并缓存）某目录的键控实例；boot 目录直接命中 boot。
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

    // 把 task 串到同目标的上一个写入之后；失败不阻断后续排队任务。
    /** Serialize writes per target instance key (boot vs directory). */
    chainWrites(targetKey: string, task: () => Promise<JsonRecord>): Promise<JsonRecord | void> {
      const previous = writeChains.get(targetKey) ?? Promise.resolve();
      const next = previous.then(task, task);
      writeChains.set(targetKey, next.catch(() => {}));
      return next;
    },

    // 全局写后丢弃全部缓存 clone：下次 settingsFor 基于新全局层重新派生。
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

    // 收尾：flush + 解除派生 clone（含自建 boot）的待写定时器。
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

/**
 * 解析 role 的模型选择器字符串（SDK "provider/id[:thinking]" 格式）。
 * 多模型 role 值（SDK loader 逗号拼接）只报告首个主选择器；空串或
 * 不可解析时返回 null。
 */
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

/** models 载荷投影所依赖的最小注册表模型表面（engine registry 模型的子集）。 */
/** Minimal registry-model surface the models payload projects from. */
export interface RegistryModel {
  /** provider id（如 anthropic）。 */
  provider: string;
  /** 模型 id（如 claude-x）。 */
  id: string;
  /** 人类可读模型名（注册表提供时才携带）。 */
  name?: string;
  /** 是否推理模型：决定 thinking 努力档位表面。 */
  reasoning?: boolean;
  /** 上下文窗口大小；SDK 用 null 表示未知，投影经 Number.isFinite 过滤。 */
  /** SDK Model carries `| null` for unknown sizes; projections filter via Number.isFinite. */
  contextWindow?: number | null;
  /** 最大输出 token 数（未知为 null，同样过滤后才携带）。 */
  maxTokens?: number | null;
  /** 内置 thinking 配置：努力档位列表与默认档位。 */
  thinking?: { efforts?: readonly string[]; defaultLevel?: string | null };
}

/** OMPChamber 遗留 defaultModel 的只读探测 / 导入结果（01 §5.8，R12）。 */
/** Legacy OMPChamber defaultModel detect/import result (01 §5.8, R12). */
export interface LegacyDefaultModel {
  /** OMPChamber settings.json 里的 defaultModel 原始字符串。 */
  defaultModel: string;
  /** 从选择器解析出的 provider（可解析时才携带）。 */
  defaultProvider?: string;
}

/** GET /omp/models 载荷中每个 role 的条目（assignment 契约形状）。 */
/** Per-role entry in the GET /omp/models payload (assignment contract). */
export interface ModelRoleEntry {
  /** 该 role 的完整配置字符串（多模型时为逗号拼接原文）。 */
  configured: string;
  /** 主选择器的 provider；未配置为 null。 */
  provider: string | null;
  /** 主选择器的模型 id；未配置为 null。 */
  id: string | null;
  /** 显式 ":thinking" 档位（存在时才携带）。 */
  thinkingLevel?: string;
  /** 配置来源层：project 覆盖优先，其次 global，最后 SDK default。 */
  source: 'project' | 'global' | 'default';
}

/** 注册表模型 → 线上投影：身份字段 + 归一化的 thinking 表面。 */
/** Registry model → wire projection: identity + baked thinking surface. */
export interface ModelThinkingProjection {
  /** provider id。 */
  provider?: string;
  /** 模型 id。 */
  id?: string;
  /** 展示名（注册表提供时才携带）。 */
  name?: string;
  /** 是否推理模型；非推理模型的 supported 为空列表。 */
  reasoning: boolean;
  /** 上下文窗口（有限数值才携带）。 */
  contextWindow?: number;
  /** 最大输出 token 数（有限数值才携带）。 */
  maxTokens?: number;
  /** 努力档位表面：supported 列表 + 默认档位（未知为 null）。 */
  thinking: { supported: string[]; defaultLevel: string | null };
}

/** buildModelsPayload 的可选项。 */
export interface BuildModelsOptions {
  /** OMPChamber 遗留 defaultModel 探测结果，原样透传到载荷。 */
  legacyDefaults?: LegacyDefaultModel | null;
  /** engine registry 模型列表；提供时载荷附带 models[] 投影。 */
  models?: readonly RegistryModel[] | null;
}

/** GET /omp/models 的响应载荷：某目录的模型 + roles 快照（01 §5.3(1)）。 */
export interface ModelsPayload {
  /** 构建 SDK 的 VERSION，用于载荷版本协商。 */
  schemaVersion: typeof VERSION;
  /** 生成快照所用键控实例的目录。 */
  directory: string;
  /** registry 模型投影（options.models 提供时才携带）。 */
  models?: ModelThinkingProjection[];
  /** 每个 role 的条目；未配置的 role 映射为 null。 */
  roles: Record<string, ModelRoleEntry | null>;
  /** role 展示元数据（名称/标签/颜色/hidden）。 */
  roleMeta: Record<string, ModelRoleInfo>;
  /** cycleOrder 设置值（角色循环顺序）。 */
  cycleOrder: SettingValue<'cycleOrder'>;
  /** enabledModels 设置值（启用模型列表）。 */
  enabledModels: SettingValue<'enabledModels'>;
  /** 重试回退链（getRetryFallbackChains 的解析结果）。 */
  fallbackChains: RetryFallbackChains;
  /** modelRoleStorage 设置值：role 写入 project 层还是 global 层。 */
  modelRoleStorage: SettingValue<'modelRoleStorage'>;
  /** 默认 thinking 档位设置值。 */
  defaultThinkingLevel: SettingValue<'defaultThinkingLevel'>;
  /** OMPChamber 遗留 defaultModel 探测结果；无则为 null。 */
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
/**
 * 把单个注册表模型投影为线上形状：undefined/null 尺寸字段直接省略；
 * thinking 表面按 TUI getSupportedEfforts 口径归一（非推理模型 →
 * 空列表，defaultLevel 未知 → null）。
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

/**
 * 基于某目录的键控 Settings 实例构建 GET /omp/models 载荷：遍历
 * getKnownRoleIds 生成 roles + roleMeta，并快照 cycleOrder /
 * enabledModels / fallbackChains 等模型相关设置。不做 registry 全量
 * 解析（那属于 engine 的 roleSnapshot）。
 */
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

/** OMPChamber 遗留配置的默认路径：~/.config/ompchamber/settings.json。 */
const ompchamberSettingsPath = () =>
  path.join(os.homedir(), '.config', 'ompchamber', 'settings.json');

/**
 * 只读探测 OMPChamber 遗留 defaultModel（绝不写任何 omp 配置）。
 * 仅当值非空且含 "/"（可解析为 provider/model）时报告，否则返回 null。
 */
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


/** importLegacyDefaultModel 的结果：成功审计记录或拒绝原因。 */
/** importLegacyDefaultModel result: success audit or refusal reason. */
export interface LegacyImportResult {
  /** 是否实际写入。 */
  imported: boolean;
  /** 写入的 role（成功时恒为 'default'）。 */
  role?: 'default';
  /** 写入的选择器原文。 */
  value?: string;
  /** 写入层：'project' 或 'global'（依实例的 modelRoleStorage）。 */
  scope?: 'global' | 'project';
  /** 成功审计记录，由协调者持久化进 OC settings.json。 */
  audit?: { originalValue: string; importedRole: 'default'; scope: string; at: string };
  /** 拒绝原因：role 已配置或选择器非法。 */
  reason?: 'role-already-configured' | 'invalid-selector';
  /** 拒绝时该 role 已有的配置值。 */
  existing?: string;
}

/**
 * 显式 R12 导入：仅当 modelRoles.default 未设置时写入（绝不覆盖），
 * 按实例的 modelRoleStorage 选层（project 走 setProjectModelRole，
 * 否则 setModelRole），flush 后返回审计记录；调用方把
 * role-already-configured 映射为 409。
 */
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

/** Web 端永不可编辑的终端渲染面（06 §5.6）：整组排除的 tab、排除前缀及前缀豁免名单。 */
/** Terminal-rendering surfaces never editable from the web (06 §5.6). */
/** 整页排除的 tab：appearance 只在终端渲染。 */
const EXCLUDED_TABS = new Set(['appearance']);
/** 按前缀排除的终端专属设置（tui. / terminal. / statusLine. / display.）。 */
const EXCLUDED_PREFIXES = ['tui.', 'terminal.', 'statusLine.', 'display.'];
/** 前缀排除的豁免名单：display.collapseCompacted 在 Web 也可编辑。 */
const EXCLUDED_PREFIX_ALLOWLIST = new Set(['display.collapseCompacted']);

/** 条件名 → 纯 Settings 读取求值器的索引表类型（CONDITION_EVALUATORS 的形状）。 */
/**
 * Server-side mirror of the TUI CONDITIONS table (modes/components/
 * settings-defs.ts:96-147) — every entry is a pure Settings read. Unknown
 * condition names evaluate visible (spec 06 §5.2: 求值失败 → 显示但不隐藏).
 * `hasImageProtocol` is a terminal capability the web never has.
 */
interface ConditionEvaluatorTable {
  [condition: string]: (s: Settings) => boolean;
}

/**
 * TUI CONDITIONS 表的服务端镜像（settings-defs.ts:96-147）：每个条件
 * 都是纯 Settings 读取；未知条件名按“可见”处理（06 §5.2 求值失败 →
 * 显示）。hasImageProtocol 是终端能力，Web 恒为 false。
 */
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

/**
 * 把 schema 的 AnyUiMetadata 投影为线上 UiProjection：逐字段拷贝，
 * options: "runtime"（只有 TUI 主题注册表能解析）改写为
 * 'runtime-unresolved'，投影后无字段的折叠为 undefined。
 */
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

/**
 * 判定某设置键是否被 Web 排除：hasImageProtocol 条件是终端能力 →
 * 'terminal-capability'；排除 tab 或命中排除前缀（豁免名单除外）→
 * 'terminal-only'；其余返回 null（Web 可编辑）。
 */
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

/** 把 schema 设置值渲染到 JSON 线上：JSON 没有 undefined，缺失一律变 null，其余原样通过。 */
/** Render a schema setting value onto the JSON wire: JSON has no undefined,
 * so absent becomes null and everything else passes through unchanged. */
const jsonValue = (value: SettingValue<SettingPath>): JsonValue => (value === undefined ? null : value);

/** 设置载荷条目携带的 UI 元数据投影（06 §5.2）。 */
/** UI metadata projection carried on settings payload entries (06 §5.2). */
export interface UiProjection {
  /** 归属设置页 tab id。 */
  tab?: string;
  /** 页内分组名。 */
  group?: string;
  /** 展示标签。 */
  label?: string;
  /** 字段说明文案。 */
  description?: string;
  /** 显隐条件名（CONDITION_EVALUATORS 求值）。 */
  condition?: string;
  /** 是否按机密处理（值不回显）。 */
  secret?: boolean;
  /** 列表项是否有序。 */
  ordered?: boolean;
  /** 子菜单选项；TUI 运行时解析不了的标记为 'runtime-unresolved'。 */
  options?: readonly SubmenuOption[] | 'runtime-unresolved';
}

/** modelRoles 记录视图条目中单个 role 的视图。 */
/** Per-role view inside the modelRoles record entry. */
export interface ModelRoleValueView {
  /** 该 role 的配置字符串；未配置为 null。 */
  value: string | null;
  /** 配置来源层：project 覆盖优先，其次 global，最后 default。 */
  source: 'project' | 'global' | 'default';
  /** Web 端是否可编辑（恒为 true）。 */
  editable: boolean;
}

/**
 * GET /omp/settings `keys` 下的单个线上条目：schema 驱动条目与
 * modelRoles 记录视图共用这一形状，缺省字段一律省略。
 */
/**
 * One wire entry under GET /omp/settings `keys`. Schema-driven entries and
 * the modelRoles record view share this shape; absent fields are omitted.
 */
export interface SettingsPayloadEntry {
  /** 值类型标签：enum/boolean/string/number/array/record。 */
  type: 'boolean' | 'string' | 'number' | 'enum' | 'array' | 'record';
  /** schema 默认值；凭据键强制 null。 */
  default?: unknown;
  /** 当前生效值；凭据键强制 null（R9 不回显）。 */
  value: unknown;
  /** 写入作用域标签：'global' 或 'global+project'。 */
  scope: string;
  /** enum 类型的候选值列表。 */
  values?: string[];
  /** 是否已显式配置（凭据键据此渲染 configured 状态）。 */
  configured?: boolean;
  /** Web 端是否可编辑（排除项与隐藏项为 false）。 */
  editable?: boolean;
  /** 凭据键标记。 */
  credential?: true;
  /** 只写标记：写入后只回 configured，不回值。 */
  writeOnly?: true;
  /** UI 元数据投影。 */
  ui?: UiProjection;
  /** 终端专属排除原因：terminal-only 或 terminal-capability。 */
  excluded?: 'terminal-only' | 'terminal-capability';
  /** 条件求值后当前上下文应隐藏。 */
  hidden?: true;
  /** modelRoles 记录视图专用：各 role 的值/来源/可编辑。 */
  roles?: Record<string, ModelRoleValueView>;
  /** modelRoles 记录视图专用：role 的写入层设置。 */
  modelRoleStorage?: string;
}

/** GET /omp/settings 的响应载荷：单目录 schema 驱动的设置快照。 */
export interface SettingsPayload {
  /** 构建 SDK 的 VERSION，用于载荷版本协商。 */
  schemaVersion: typeof VERSION;
  /** 生成快照的目录（键控实例的 cwd）。 */
  directory: string;
  /** agent 配置根目录。 */
  agentDir: string;
  /** 全局 config.yml 绝对路径（仅展示）。 */
  globalConfigPath: string;
  /** 该目录 project config.yml 绝对路径（仅展示）。 */
  projectConfigPath: string;
  /** store 当前 revision，供事件驱动 refetch 对账。 */
  revision: number;
  /** 设置页结构：tab id → 标签 + 分组名列表。 */
  tabs: { id: SettingTab; label: string; groups: string[] }[];
  /** 各设置键的条目（含 modelRoles 记录视图）。 */
  keys: Record<string, SettingsPayloadEntry>;
}

/**
 * 为单个目录构建 schema 驱动的设置快照（该目录会话消费的同一键控实例）。
 * 凭据键（isCredential，含 ui.secret）的值与默认值一律不回显（R9），
 * 只报告 configured；keys 参数可把载荷过滤到指定键子集。
 */
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

/** per-role 写入键的固定前缀，完整形态为 "modelRoles.<role>"。 */
const MODEL_ROLES_PREFIX = 'modelRoles.';

/** 判定键是否为 "modelRoles.<role>" 形态：前缀命中且带非空 role 名。 */
const isModelRoleKey = (key: string): boolean =>
  typeof key === 'string' && key.startsWith(MODEL_ROLES_PREFIX) && key.length > MODEL_ROLES_PREFIX.length;

/** role 名合法性：非空且不含点（点会破坏点分设置路径语义）。 */
const validModelRoleName = (role: string): boolean => role.length > 0 && !role.includes('.');

/**
 * 按 SETTINGS_SCHEMA 校验单个提交值：null 恒合法（表示清除）；
 * 凭据键只收字符串；其余按 schema 类型（enum/boolean/number/string/
 * array/record）核对形状。返回拒绝原因字符串，通过时返回 null。
 */
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

/**
 * 从 settings 写异常的消息里提取隔离文件路径（"moved to <path>"）：
 * 命中时 PUT 映射为 409 config-quarantined。非隔离错误返回 null。
 */
const quarantinePathFromError = (cause: unknown): string | null => {
  // SAFETY: settings quarantine errors quote the quarantine path in their message.
  const message = (cause as { message?: unknown } | null | undefined)?.message;
  const match = /moved to (\S+)/.exec(String(message ?? cause ?? ''));
  return match ? match[1] : null;
};

/** PUT /omp/settings 请求体（运行时逐字段形状检查）。 */
/** PUT /omp/settings request body (shape-checked at runtime). */
export interface SettingsChangesInput {
  /** project 写入的目标目录；缺省回退 boot 目录。 */
  directory?: string;
  /** 写入作用域：'global' 或 'project'，缺省 'global'。 */
  scope?: string;
  /** 设置路径 → 新值的映射；null 值表示清除该键。 */
  changes?: JsonRecord;
}

/** omp.settings.updated 事件的发布函数（协调者接线到 ompBus.publish）。 */
/** omp.settings.updated publisher (wired to ompBus.publish by the coordinator). */
export type PublishFn = (
  type: string,
  payload: JsonRecord,
  eventScope: { directory: string; durable?: boolean },
) => void;

/** applySettingsChanges 的可选钩子集合。 */
export interface ApplySettingsHooks {
  /** 写入成功后发布 omp.settings.updated 的函数。 */
  publish?: PublishFn;
}

/** PUT /omp/settings 的处理结果：HTTP 状态码 + 由路由层直接透传的响应体。 */
export interface SettingsChangeResult {
  /** HTTP 状态：200 成功 / 400 校验失败 / 409 配置被隔离 / 500 写入失败。 */
  status: number;
  /** 响应体；错误信息只含键名，绝不回显提交值（R9）。 */
  body: {
    /** 错误码：invalid-scope / invalid-body / validation / config-quarantined 等。 */
    error?: string;
    /** 逐键拒绝列表（key + reason），任何非法键整批拒绝。 */
    rejected?: { key: string; reason: string }[];
    /** 递增后的 revision（成功时携带）。 */
    revision?: number;
    /** 各键写后生效值回显；凭据键只回 { configured }。 */
    applied?: JsonRecord;
    /** 是否已 flush 落盘。 */
    persisted?: boolean;
    /** 兼容字段：成功体中恒为 null（隔离信息走 quarantinedTo）。 */
    quarantined?: string | null;
    /** 被隔离配置文件的新路径（409 时携带）。 */
    quarantinedTo?: string;
    /** 500 时涉及的键名列表。 */
    keys?: string[];
  };
}

/** 校验通过的写入计划项：role 写入或普通设置写入（value 为 undefined 表示清除）。 */
type SettingsPlanItem =
  // role 写入：value 为新选择器字符串，null 表示清除该 role。
  | { kind: 'role'; key: string; role: string; value: string | null }
  // 普通设置写入：clearing 标记空串（仅凭据键）与 null 的清除语义。
  | { kind: 'setting'; key: SettingPath; value: JsonValue | undefined; clearing: boolean };


/**
 * 执行一次 PUT /omp/settings：先逐键校验（任何非法键整批 400，拒绝项
 * 只含键名，R9），再按 scope 路由 —— global 走 boot 实例（唯一全局
 * 写入者）、project 仅接受 modelRoles.<role> 键并走目录键控实例 ——
 * 在 per-target 写入链内串行执行、flush、失效派生副本（仅 global 写）、
 * 递增 revision 并发布 omp.settings.updated；配置被 quarantine 时映射
 * 409，其余写入异常映射 500。
 */
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

/** 从请求 ?directory= 查询参数提取并归一化目录键；缺省时返回 null。 */
const directoryFromRequest = (request: Request): string | null => {
  const raw = new URL(request.url).searchParams.get('directory');
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** omp-host 路由表注册函数类型（与 registerEndpoints 同一挂载机制）。 */
/** omp-host route table registration function (same mechanism as registerEndpoints). */
export type OmpRouteFn = (
  method: string,
  pattern: string,
  handler: (request: Request) => Promise<Response>,
) => void;

/** registerModelSettingsRoutes 的依赖注入选项。 */
export interface RegisterModelSettingsOptions {
  /** settings store 表面：三条路由的全部读写入口。 */
  store: SettingsStoreSurface;
  /** omp.settings.updated 发布函数（可选）。 */
  publish?: PublishFn;
  /** OMPChamber 遗留配置路径覆盖（默认 ~/.config/ompchamber/settings.json）。 */
  legacySettingsPath?: string;
  /** engine registry 模型列表提供者；失败时降级为纯 roles 载荷。 */
  listModels?: (() => Promise<RegistryModel[]>) | null;
}

/**
 * 在 omp-host 路由表上挂载 GET /omp/models、GET /omp/settings、
 * PUT /omp/settings（Basic auth 由 host.js 在这些 handler 之外执行）。
 * publish 由协调者接线到 ompBus.publish（durable omp.settings.updated，
 * spec 06 §5.4）。
 */
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
  // GET /omp/models：目录 roles 快照 + 可选 registry 模型投影 + 遗留 defaultModel 探测。
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

  // GET /omp/settings：目录级设置快照，支持 ?keys= 过滤与 revision 对账。
  route('GET', '/omp/settings', async (request) => {
    const url = new URL(request.url);
    const keysParam = url.searchParams.get('keys');
    const settings = await store.settingsFor(directoryFromRequest(request) ?? undefined);
    return Response.json(buildSettingsPayload(settings, {
      revision: store.getRevision(),
      keys: keysParam ? keysParam.split(',').map((k) => k.trim()).filter(Boolean) : null,
    }));
  });

  // PUT /omp/settings：校验并按 scope 路由写入，成功后发布设置更新事件。
  route('PUT', '/omp/settings', async (request) => {
    // SAFETY: applySettingsChanges validates scope/changes field by field.
    const body = (await request.json().catch(() => ({}))) as SettingsChangesInput;
    const { status, body: payload } = await applySettingsChanges(store, body, { publish });
    return Response.json(payload, { status });
  });
};
