/**
 * omp-parity 插件管理域（服务端）：把 omp 规范的包 / marketplace
 * 注册表与扩展加载器投影给 Web（Settings → Plugins 数据面）；
 * OpenCode 的 `opencode.json#plugin` 从不被读取。
 */
// OMP plugin management domain.
//
// Settings → Plugins projects omp's canonical package/marketplace registries
// and extension loader. OpenCode's `opencode.json#plugin` is never consulted.

import { execFile } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';
import { Settings } from '@oh-my-pi/pi-coding-agent/config/settings';
import { getAgentDir } from '@oh-my-pi/pi-coding-agent';
import { classifyInstallTarget } from '@oh-my-pi/pi-coding-agent/cli/classify-install-target';
import {
  clearPluginRootsAndCaches,
  getExtensionNameFromPath,
  resolveActiveProjectRegistryPath,
  resolveOrDefaultProjectRegistryPath,
} from '@oh-my-pi/pi-coding-agent/discovery/helpers';
import { discoverExtensionPaths } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import {
  getEnabledPlugins,
  getPluginSettings,
  parseSettingValue,
  PluginManager,
  resolvePluginManifestEntries,
  validateSetting,
} from '@oh-my-pi/pi-coding-agent/extensibility/plugins';
import {
  MarketplaceManager,
  getInstalledPluginsRegistryPath,
  getMarketplacesCacheDir,
  getMarketplacesRegistryPath,
  getPluginsCacheDir,
} from '@oh-my-pi/pi-coding-agent/extensibility/plugins/marketplace';
import { errorText, errorCode, featureUnavailable, ompFeatures } from './omp-parity.ts';
import type { InstalledPlugin, PluginFeature, PluginManifest, PluginSettingType, ProjectPluginOverrides, ScopedInstalledPlugin } from '@oh-my-pi/pi-coding-agent/extensibility/plugins';

/** 插件来源类别：npm 包安装或 marketplace 引用安装。 */
type PluginKind = 'npm' | 'marketplace';
/** 插件作用域：用户级（全局）或项目级。 */
type PluginScope = 'user' | 'project';
/** 请求体 / 设置文件里的 JSON 值 —— omp 为插件设置与路由载荷持久化的开放值域。 */
/** JSON value as it arrives from a request body or settings file — the open
 * value domain omp persists for plugin settings and route payloads. */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/** 开放式字符串键 JSON 对象：路由体与设置映射的键名双方都无法提前枚举，因此刻意保持开放。 */
/** Open string-keyed JSON object: route bodies and setting maps are keyed by
 * names neither side enumerates up front, so the record stays intentionally
 * open while its values remain concrete JSON. */
type JsonRecord = Record<string, JsonValue>;

/** base64url 插件 id 解码结果：kind/scope/name 三元组。 */
interface DecodedPluginId {
  /** 来源类别。 */
  kind: PluginKind;
  /** 作用域。 */
  scope: PluginScope;
  /** 包名或 marketplace id。 */
  name: string;
}

/** 插件 manifest 的最小读取面（package.json 内 omp/pi 字段的结构子集）。 */
interface PluginManifestLike {
  /** 插件描述。 */
  description?: string;
  /** 声明的扩展入口文件列表。 */
  extensions?: string[];
  /** feature 名 → 元数据（含默认开关）。 */
  features?: Record<string, PluginFeature>;
  /** 设置键 → 设置 schema。 */
  settings?: Record<string, PluginSettingLike>;
}

/** 插件设置 schema 的宽松读取面（类型字段以 SDK PluginSettingType 为准）。 */
interface PluginSettingLike {
  /** 值类型（string/number/boolean 等）。 */
  type?: PluginSettingType;
  /** 设置说明。 */
  description?: string;
  /** 是否机密（值不回显）。 */
  secret?: boolean;
  /** 默认值。 */
  default?: unknown;
  /** 枚举候选值。 */
  values?: unknown;
  /** 数值下界。 */
  min?: unknown;
  /** 数值上界。 */
  max?: unknown;
  /** 数值步进。 */
  step?: unknown;
}

/** extensionFilesById 登记的扩展文件元数据（读取/删除/reveal 路由复用）。 */
interface ExtensionFileMeta {
  /** 规范化绝对路径。 */
  path: string;
  /** 是否允许 Web 编辑删除。 */
  editable?: boolean;
  /** 所属作用域。 */
  scope?: PluginScope;
  /** 来源标签（native/configured/plugin-manifest）。 */
  source?: string;
  /** 所属插件 id（插件声明入口时携带）。 */
  pluginId?: string;
  /** 所属插件名。 */
  pluginName?: string;
}

/** 线上扩展条目（插件列表 extensions[] 的元素形状）。 */
interface ExtensionRecord {
  /** 稳定扩展 id（rememberExtension 登记的 ext_ 或缺失入口的 missing_）。 */
  id: string;
  /** 条目类别，恒为 'extension'。 */
  kind: 'extension';
  /** 作用域。 */
  scope: PluginScope;
  /** 从路径提取的扩展名。 */
  name: string;
  /** 来源标签。 */
  source: string;
  /** 是否可编辑删除。 */
  editable: boolean;
  /** 是否被本轮发现加载（缺失入口为 false）。 */
  loaded: boolean;
  /** 所属插件 id。 */
  pluginId?: string;
  /** 所属插件名。 */
  pluginName?: string;
  /** 插件 manifest 声明的入口相对路径。 */
  declaredEntry?: string;
}

/** projectExtension 的输入：列表阶段的一条原始扩展发现信息。 */
interface ExtensionInput {
  /** 扩展文件绝对路径。 */
  filePath: string;
  /** 作用域。 */
  scope: PluginScope;
  /** 来源标签。 */
  source: string;
  /** 是否可编辑。 */
  editable: boolean;
  /** 所属插件 id。 */
  pluginId?: string;
  /** 所属插件名。 */
  pluginName?: string;
  /** manifest 声明入口。 */
  declaredEntry?: string;
  /** 是否已加载（缺省 true）。 */
  loaded?: boolean;
}

/** projectPlugin 可投影的最小插件形状（PluginManager 与启用插件行的并集子集）。 */
interface ProjectablePlugin {
  /** 包名。 */
  name: string;
  /** 版本（缺省投影为 'unknown'）。 */
  version?: string;
  /** 是否启用（缺省视为启用）。 */
  enabled?: boolean;
  /** 作用域（缺省 'user'）。 */
  scope?: PluginScope;
  /** manifest（可缺）。 */
  manifest?: PluginManifestLike | null;
  /** 已启用 feature 名列表（null = 按 manifest 默认）。 */
  enabledFeatures?: string[] | null;
  /** 磁盘根路径。 */
  path?: string;
  /** 发现时的工作目录。 */
  cwd?: string;
}

/** projectPlugin 的覆盖选项（marketplace 行借此覆写 kind/name/scope）。 */
interface ProjectPluginOptions {
  /** 来源类别（缺省 'npm'）。 */
  kind?: PluginKind;
  /** 覆写展示名（缺省用插件自身 name）。 */
  name?: string;
  /** 覆写作用域（缺省 'user'）。 */
  scope?: PluginScope;
  /** 已持久化的设置值映射。 */
  settingValues?: JsonRecord;
}

/** 线上插件设置条目：schema + 当前配置状态（机密值不回显）。 */
interface ProjectedPluginSetting {
  /** 值类型。 */
  type?: PluginSettingType;
  /** 设置说明。 */
  description?: string;
  /** 是否机密。 */
  secret: boolean;
  /** 是否已配置。 */
  configured: boolean;
  /** 当前值（机密或未配置时省略）。 */
  value?: unknown;
  /** 默认值。 */
  default?: unknown;
  /** 枚举候选。 */
  values?: unknown[];
  /** 数值下界。 */
  min?: number;
  /** 数值上界。 */
  max?: number;
  /** 数值步进。 */
  step?: number;
}

/** 线上插件 feature 条目。 */
interface ProjectedPluginFeature {
  /** feature 名。 */
  name: string;
  /** 是否启用。 */
  enabled: boolean;
  /** feature 说明。 */
  description?: string;
}

/** 线上插件条目：GET /omp/plugins 的 plugins[] 元素形状。 */
export interface ProjectedPlugin {
  /** base64url 插件 id（kind:scope:name 编码）。 */
  id: string;
  /** 来源类别。 */
  kind: PluginKind;
  /** 作用域。 */
  scope: PluginScope;
  /** 包名 / marketplace id。 */
  name: string;
  /** 版本（未知为 'unknown'）。 */
  version: string;
  /** 是否启用。 */
  enabled: boolean;
  /** Web 是否可变更状态。 */
  editable: boolean;
  /** 各操作允许矩阵：toggle/features/settings/uninstall。 */
  permissions: { toggle: boolean; features: boolean; settings: boolean; uninstall: boolean };
  /** manifest 描述。 */
  description?: string;
  /** feature 投影列表。 */
  features: ProjectedPluginFeature[];
  /** 设置键 → 投影条目。 */
  settings: Record<string, ProjectedPluginSetting>;
  /** 关联扩展条目 id 列表（列表阶段回填）。 */
  extensionEntries: string[];
}

/** GET /omp/plugins 的响应：插件条目 + 独立扩展条目两组投影。 */
export interface PluginListResult {
  /** 已安装插件（npm + marketplace）。 */
  plugins: ProjectedPlugin[];
  /** 扩展文件条目（插件 manifest 入口 + 原生/配置目录发现）。 */
  extensions: ExtensionRecord[];
}

/** omp 加载器返回的插件行（补上 cwd 供后续 registry 解析）。 */
interface RawPlugin extends ScopedInstalledPlugin {
  /** 发现时的工作目录。 */
  cwd: string;
}

/** marketplace 安装目录 package.json 的读取面。 */
interface MarketplacePackageJson {
  /** 包版本。 */
  version?: string;
  /** omp 字段的完整 manifest。 */
  omp?: PluginManifest;
  /** pi 字段 manifest（omp 缺失时的回退）。 */
  pi?: PluginManifest;
}

/** marketplace 行的原始信息（reveal 定位与投影时保留）。 */
interface MarketplaceRawPlugin {
  /** marketplace 插件 id。 */
  name: string;
  /** 作用域。 */
  scope: PluginScope;
  /** 版本。 */
  version: string;
  /** 是否启用。 */
  enabled: boolean;
  /** 已启用 feature（无 per-feature 覆盖时为 null）。 */
  enabledFeatures: string[] | null;
  /** 从 package.json 读到的 manifest。 */
  manifest: PluginManifest | undefined;
  /** 安装路径。 */
  path: string;
  /** 发现目录。 */
  cwd: string;
}

/** 单次插件状态变更（PATCH 体），经 applyProjectOverride 落盘。 */
interface PluginMutation {
  /** 启停开关。 */
  enabled?: boolean;
  /** 覆盖启用 feature 列表。 */
  enabledFeatures?: string[];
  /** 设置写入或删除（remove: true 表示删除）。 */
  setting?: { key?: string; value?: unknown; remove?: boolean } | null;
}

/** engine 侧会话实际应用的插件快照（协调者注入 snapshots 时提供）。 */
export interface AppliedPluginsSnapshot {
  /** 会话 id。 */
  sessionId: string;
  /** 会话目录。 */
  directory: string;
  /** 应用时间戳（ms）。 */
  appliedAt: number;
  /** 已加载扩展的绝对路径列表。 */
  extensionPaths: string[];
  /** 已启用插件名列表。 */
  pluginNames: string[];
}

/** 域路由 handler 的第二参数：路径参数等上下文。 */
export interface DomainRouteContext {
  /** 路径模式提取的命名参数（如 {id}）。 */
  params: Record<string, string>;
}

/** 域路由 handler：接收 Request 与可选上下文，返回或异步返回 Response。 */
export type DomainRouteHandler = (request: Request, ctx?: DomainRouteContext) => Response | Promise<Response>;

/** 域路由注册函数类型（omp-host 路由表注入）。 */
export type DomainRoute = (method: string, pattern: string, handler: DomainRouteHandler) => void;

/** registerPluginsDomainRoutes 的依赖注入（测试可整体替换）。 */
export interface PluginsDomainDeps {
  /** capability 开关表（缺省 ompFeatures()）。 */
  features?: Record<string, boolean>;
  /** 列表实现（缺省 listPlugins）。 */
  list?: (directory: string) => Promise<PluginListResult>;
  /** applied 快照提供者（null 时 /applied 回 400）。 */
  snapshots?: (() => AppliedPluginsSnapshot[]) | null;
  /** 会话重载实现（null 时 /reload 回 400）。 */
  reloadSessions?: ((directory: string, sessionId: string | null) => Promise<{ sessionsRefreshed: number }>) | null;
}

/** JSON 响应薄封装：Response.json + 可选 init（状态码等）。 */
const json = <T,>(data: T, init?: ResponseInit) => Response.json(data, init);
/** 400 响应：{ error: message }。 */
const badRequest = (message: string) => json({ error: message }, { status: 400 });
/** 404 响应：{ error: message }。 */
const notFound = (message: string) => json({ error: message }, { status: 404 });
/** 500 响应：{ error: message }。 */
const failed = (message: string) => json({ error: message }, { status: 500 });
/**
 * 把已解析的 JSON 值收窄为纯 record：非对象、数组或缺失返回 null，
 * 调用方用 ?? {} 兜底，保持 body?.field 读到 undefined 的旧语义。
 */
/** Narrow a parsed JSON value to a plain record; parse failures and
 * non-object payloads collapse to `{}` so `body?.field` reads stay undefined,
 * exactly like the untyped access pattern this replaces. */
const asJsonRecord = (value: JsonValue | null | undefined): JsonRecord | null =>
  typeof value === 'object' && value !== null && !Array.isArray(value) ? value : null;

/**
 * 读取并解析请求体：解析失败或载荷不是对象一律折叠为 {}，使后续
 * body?.field 读取保持 undefined，等价于原先的无类型访问模式。
 */
const jsonBody = async (request: Request): Promise<JsonRecord> => {
  // SAFETY: Request.json() parses JSON by construction, so its fulfillment
  // value is always a JsonValue; the catch collapses parse failures to null.
  const value = (await request.json().catch((): null => null)) as JsonValue | null;
  return asJsonRecord(value) ?? {};
};

/** 新建扩展文件名的合法性正则：字母数字开头，仅含 - _ .，扩展名 js/ts/mjs/cjs。 */
const EXTENSION_FILE_PATTERN = /^[a-z0-9][a-z0-9-_.]*\.(js|ts|mjs|cjs)$/i;
/** 模块级扩展注册表（id → 文件元数据）：每次 listPlugins 重建，供读取/删除/reveal 路由复用。 */
const extensionFilesById = new Map<string, ExtensionFileMeta>();

/** 构造“已受理、需重启 engine 生效”的统一成功响应体。 */
const restartDeferred = (message: string) => ({
  ok: true,
  requiresRestart: true,
  restartDeferred: true,
  message,
});

/** 把 kind:scope:name 三元组编码为 base64url 插件 id（可安全作路径参数）。 */
const encodePluginId = (kind: PluginKind, scope: PluginScope, name: string) =>
  Buffer.from(`${kind}:${scope}:${name}`).toString('base64url');

/**
 * 解码 base64url 插件 id 还原 { kind, scope, name }：kind/scope 取值
 * 非法或名称缺失时返回 null（路由据此回 400 invalid plugin id）。
 */
const decodePluginId = (id: string): DecodedPluginId | null => {
  try {
    const decoded = Buffer.from(id, 'base64url').toString('utf8');
    const first = decoded.indexOf(':');
    const second = decoded.indexOf(':', first + 1);
    if (first <= 0 || second <= first + 1) return null;
    const kind = decoded.slice(0, first);
    const scope = decoded.slice(first + 1, second);
    const name = decoded.slice(second + 1);
    if (kind !== 'npm' && kind !== 'marketplace') return null;
    if ((scope !== 'user' && scope !== 'project') || !name) return null;
    return { kind, scope, name };
  } catch {
    return null;
  }
};

/** 规范化绝对路径：优先 realpathSync（解析符号链接），失败回退 path.resolve。 */
const canonicalPath = (filePath: string): string => {
  try {
    return fs.realpathSync(filePath);
  } catch {
    return path.resolve(filePath);
  }
};

/** 判定 filePath 是否位于 rootPath 之内（含相等；以相对路径判断，避免前缀误判）。 */
const isWithin = (filePath: string, rootPath: string): boolean => {
  const relative = path.relative(rootPath, filePath);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
};

/** 由规范化路径派生的稳定扩展 id：ext_ 前缀 + sha256 base64url 截断 20 位。 */
const extensionId = (filePath: string): string =>
  `ext_${crypto.createHash('sha256').update(canonicalPath(filePath)).digest('base64url').slice(0, 20)}`;

/** 把扩展文件登记进 extensionFilesById 并返回其 id（后续 GET/DELETE/reveal 复用）。 */
const rememberExtension = (filePath: string, metadata: Omit<ExtensionFileMeta, 'path'>): string => {
  const id = extensionId(filePath);
  extensionFilesById.set(id, { path: canonicalPath(filePath), ...metadata });
  return id;
};

/**
 * 投影 manifest features：enabledFeatures 为 null 时按 manifest 的
 * default !== false 推导启用集；否则以显式列表为准。
 */
const manifestFeatures = (manifest: PluginManifestLike | null | undefined, enabledFeatures: string[] | null | undefined): ProjectedPluginFeature[] => {
  const enabled = enabledFeatures === null
    ? new Set(Object.entries(manifest?.features ?? {})
      .filter(([, value]) => value?.default !== false)
      .map(([name]) => name))
    : new Set(enabledFeatures ?? []);
  return Object.entries(manifest?.features ?? {}).map(([name, value]) => ({
    name,
    enabled: enabled.has(name),
    ...(value?.description ? { description: value.description } : {}),
  }));
};

/**
 * 清空进程级插件发现缓存（插件根 + 启用插件）：与 omp TUI 行为对齐；
 * 否则下一次列表会把新装插件的 manifest 条目投影成未加载。
 */
/** omp TUI parity: every plugin mutation clears the process-global discovery
 * caches (plugin roots + enabled plugins), else the next list projects a
 * freshly installed plugin's manifest entries as not loaded. */
const invalidatePluginCaches = async (directory: string): Promise<void> => {
  try {
    const projectRegistryPath = await resolveOrDefaultProjectRegistryPath(directory);
    clearPluginRootsAndCaches(projectRegistryPath ? [projectRegistryPath] : undefined);
  } catch {
    clearPluginRootsAndCaches();
  }
};

/**
 * 投影 manifest settings：逐键合并 schema 与已存值；机密设置不回显
 * 值，未配置的键省略 value 字段。
 */
const manifestSettings = (manifest: PluginManifestLike | null | undefined, values: JsonRecord | null | undefined): Record<string, ProjectedPluginSetting> => Object.fromEntries(
  Object.entries(manifest?.settings ?? {}).map(([key, schema]) => {
    const value = values?.[key];
    return [key, {
      type: schema?.type,
      description: schema?.description,
      secret: schema?.secret === true,
      configured: value !== undefined,
      ...(schema?.secret === true || value === undefined ? {} : { value }),
      ...(schema?.default === undefined ? {} : { default: schema.default }),
      ...(Array.isArray(schema?.values) ? { values: schema.values } : {}),
      ...(typeof schema?.min === 'number' ? { min: schema.min } : {}),
      ...(typeof schema?.max === 'number' ? { max: schema.max } : {}),
      ...(typeof schema?.step === 'number' ? { step: schema.step } : {}),
    }];
  }),
);

/** 按目录构造 MarketplaceManager：注入全局/项目注册表与缓存目录路径。 */
const marketplaceManagerFor = async (directory: string): Promise<MarketplaceManager> => new MarketplaceManager({
  marketplacesRegistryPath: getMarketplacesRegistryPath(),
  installedRegistryPath: getInstalledPluginsRegistryPath(),
  projectInstalledRegistryPath: await resolveOrDefaultProjectRegistryPath(directory),
  marketplacesCacheDir: getMarketplacesCacheDir(),
  pluginsCacheDir: getPluginsCacheDir(),
});

/** 项目级插件覆盖文件路径：<directory>/.omp/plugin-overrides.json。 */
const projectOverridesPath = (directory: string): string =>
  path.join(directory, '.omp', 'plugin-overrides.json');

/** 读取项目覆盖文件；文件缺失或损坏一律回退为空覆盖对象。 */
const readProjectOverrides = (directory: string): ProjectPluginOverrides => {
  const overridesPath = projectOverridesPath(directory);
  try {
    return JSON.parse(fs.readFileSync(overridesPath, 'utf8'));
  } catch {
    return {};
  }
};

/**
 * 原子写入项目覆盖文件：先写同目录临时文件再 rename，并按需创建
 * .omp 目录。
 */
const writeProjectOverrides = (directory: string, overrides: ProjectPluginOverrides): void => {
  const overridesPath = projectOverridesPath(directory);
  fs.mkdirSync(path.dirname(overridesPath), { recursive: true });
  const tempPath = `${overridesPath}.tmp-${process.pid}-${Date.now()}`;
  fs.writeFileSync(tempPath, JSON.stringify(overrides, null, 2), 'utf8');
  fs.renameSync(tempPath, overridesPath);
};

/**
 * 把单条 toggle/feature/setting 变更合并进 .omp/plugin-overrides.json
 * （与 omp TUI 管理的是同一个文件），让项目级包插件可完整编辑；
 * 下一次发现缓存失效后会读到新值。
 */
/** Toggle / feature / setting mutation through .omp/plugin-overrides.json —
 * the same file omp TUI manages. Cache invalidation picks it up on the next
 * discovery pass, so project-scoped package plugins become fully editable. */
const applyProjectOverride = (directory: string, pluginName: string, mutation: PluginMutation): void => {
  const overrides = readProjectOverrides(directory);
  if (mutation.enabled === false) {
    overrides.disabled = [...new Set([...(overrides.disabled ?? []), pluginName])];
  } else if (mutation.enabled === true) {
    overrides.disabled = (overrides.disabled ?? []).filter((name) => name !== pluginName);
  }
  if (Array.isArray(mutation.enabledFeatures)) {
    overrides.features = { ...(overrides.features ?? {}), [pluginName]: mutation.enabledFeatures };
  }
  const setting = mutation.setting;
  if (setting && typeof setting.key === 'string' && setting.key) {
    const settings = { ...(overrides.settings ?? {}) };
    const pluginSettings = { ...(settings[pluginName] ?? {}) };
    if (setting.remove === true) delete pluginSettings[setting.key];
    else pluginSettings[setting.key] = setting.value;
    settings[pluginName] = pluginSettings;
    overrides.settings = settings;
  }
  writeProjectOverrides(directory, overrides);
};

/** execFile 的 promise 化封装（reveal 文件管理器命令用）。 */
const execFileAsync = promisify(execFile);

/** execFile 可直接执行的 reveal 命令：可执行文件 + 字面参数。 */
/** execFile-ready file-manager reveal: executable plus its literal arguments. */
export interface RevealCommand {
  /** 可执行文件名或路径。 */
  command: string;
  /** 逐字参数列表（不经 shell 拼接）。 */
  args: string[];
}

/**
 * 按平台构造 reveal 命令：darwin 用 open -R 选中文件，win32 用
 * explorer /select,，其余平台用 xdg-open 打开父目录。纯构造函数，
 * 可脱离真实文件管理器做单测。
 */
/**
 * Platform file-manager reveal command for an absolute target. macOS selects
 * the file (open -R) or opens the directory; Windows selects via
 * `explorer /select,`; other platforms open the parent directory only.
 * Pure builder — unit-testable without touching a real file manager.
 */
export const revealCommand = (platform: NodeJS.Platform, targetPath: string): RevealCommand => {
  if (platform === 'darwin') return { command: 'open', args: ['-R', targetPath] };
  if (platform === 'win32') {
    return fs.existsSync(targetPath) && fs.statSync(targetPath).isFile()
      ? { command: 'explorer', args: [`/select,${targetPath}`] }
      : { command: 'explorer', args: [targetPath] };
  }
  return { command: 'xdg-open', args: [fs.existsSync(targetPath) ? path.dirname(targetPath) : targetPath] };
};

/** 在系统文件管理器中展示目标路径；成功 resolve true，失败 reject 由调用方兜底。 */
const revealInFileManager = (targetPath: string): Promise<boolean> => {
  const { command, args } = revealCommand(process.platform, targetPath);
  return execFileAsync(command, args, { windowsHide: true }).then(() => true);
};

/**
 * 经 omp 注册表把包插件 id 解析为磁盘根路径：npm 走 PluginManager.list
 * 按名匹配；marketplace 走已安装摘要（取首个启用 entry，无则第一个）。
 * 找不到返回 null（路由回 404）。
 */
/** Resolve a package plugin id to its on-disk root via omp registries. */
const pluginPathForId = async (target: DecodedPluginId, directory: string): Promise<string | null> => {
  if (target.kind === 'npm') {
    const plugin = (await new PluginManager(directory).list()).find((item) => item.name === target.name);
    return plugin?.path ?? null;
  }
  const summary = (await (await marketplaceManagerFor(directory)).listInstalledPlugins())
    .find((item) => item.id === target.name);
  const entry = summary?.entries?.find((item) => item.enabled !== false) ?? summary?.entries?.[0];
  return entry?.installPath ?? null;
};

/**
 * 收集原生扩展目录：用户级 <agentDir>/extensions 恒在；该目录存在
 * 项目 registry 时追加项目 .omp/extensions。
 */
const nativeExtensionDirectories = async (directory: string): Promise<Array<{ scope: PluginScope; path: string }>> => {
  const directories: Array<{ scope: PluginScope; path: string }> = [{ scope: 'user' as const, path: path.join(getAgentDir(), 'extensions') }];
  const projectRegistryPath = await resolveActiveProjectRegistryPath(directory);
  if (projectRegistryPath) {
    const projectOmpDir = path.dirname(path.dirname(projectRegistryPath));
    directories.push({ scope: 'project' as const, path: path.join(projectOmpDir, 'extensions') });
  }
  return directories;
};

/**
 * 只读加载目录设置，提取 extensions 与 disabledExtensions 两个列表；
 * 加载失败回退双空列表，扩展发现照常进行。
 */
const settingsExtensionPaths = async (directory: string): Promise<{ configured: string[]; disabled: string[] }> => {
  try {
    const settings = await Settings.loadReadOnly({ cwd: directory, agentDir: getAgentDir() });
    return {
      configured: settings.get('extensions') ?? [],
      disabled: settings.get('disabledExtensions') ?? [],
    };
  } catch {
    return { configured: [], disabled: [] };
  }
};

/**
 * 把一条扩展发现结果投影为线上 ExtensionRecord：已加载的登记进
 * extensionFilesById 换取可操作 id；缺失入口派生 missing_ 前缀 id 且
 * loaded 为 false。
 */
const projectExtension = ({ filePath, scope, source, editable, pluginId, pluginName, declaredEntry, loaded = true }: ExtensionInput): ExtensionRecord => {
  const name = getExtensionNameFromPath(filePath);
  const id = loaded
    ? rememberExtension(filePath, { editable, scope, source, pluginId, pluginName })
    : `missing_${crypto.createHash('sha256').update(`${pluginId}:${declaredEntry}`).digest('base64url').slice(0, 20)}`;
  return {
    id,
    kind: 'extension',
    scope,
    name,
    source,
    editable,
    loaded,
    ...(pluginId ? { pluginId } : {}),
    ...(pluginName ? { pluginName } : {}),
    ...(declaredEntry ? { declaredEntry } : {}),
  };
};


/**
 * 把插件行投影为线上 ProjectedPlugin：npm 包允许 toggle/features/
 * settings（仅用户级可 uninstall）；marketplace 行仅 toggle/uninstall。
 * editable 与 permissions 矩阵都由 kind 与 scope 决定。
 */
const projectPlugin = (plugin: ProjectablePlugin, {
  kind = 'npm',
  name = plugin.name,
  scope = plugin.scope ?? 'user',
  settingValues = {},
}: ProjectPluginOptions = {}): ProjectedPlugin => {
  const userPackage = kind === 'npm' && scope === 'user';
  const npmPackage = kind === 'npm';
  return {
    id: encodePluginId(kind, scope, name),
    kind,
    scope,
    name,
    version: plugin.version || 'unknown',
    enabled: plugin.enabled !== false,
    editable: npmPackage,
    permissions: {
      toggle: npmPackage,
      features: npmPackage,
      settings: npmPackage,
      uninstall: userPackage,
    },
    ...(plugin.manifest?.description ? { description: plugin.manifest.description } : {}),
    features: manifestFeatures(plugin.manifest, plugin.enabledFeatures),
    settings: manifestSettings(plugin.manifest, settingValues),
    extensionEntries: [],
  };
};

/**
 * 汇总单目录的插件与扩展投影：并发拉取 PluginManager 列表、显式启用
 * 插件、marketplace 安装摘要、扩展发现与原生目录；先投影 npm/启用
 * 插件（合并已存设置值），再投影 marketplace 行，最后为插件 manifest
 * 入口与独立发现的扩展文件生成 ExtensionRecord。每次调用重建
 * extensionFilesById 注册表。
 */
const listPlugins = async (directory: string): Promise<PluginListResult> => {
  extensionFilesById.clear();
  const manager = new PluginManager(directory);
  const extensionSettings = await settingsExtensionPaths(directory);
  const [managedPlugins, enabledPlugins, marketplaceManager, discoveredPaths, nativeDirs] = await Promise.all([
    manager.list(),
    getEnabledPlugins(directory),
    marketplaceManagerFor(directory),
    discoverExtensionPaths(extensionSettings.configured, directory, extensionSettings.disabled),
    nativeExtensionDirectories(directory),
  ]);
  const marketplacePlugins = await marketplaceManager.listInstalledPlugins();

  const rawPlugins = new Map<string, RawPlugin>();
  for (const plugin of managedPlugins) rawPlugins.set(`user:${plugin.name}`, { ...plugin, scope: 'user' as const, cwd: directory });
  for (const plugin of enabledPlugins) rawPlugins.set(`${plugin.scope}:${plugin.name}`, { ...plugin, cwd: directory });

  const plugins: ProjectedPlugin[] = [];
  const pluginRawById = new Map<string, RawPlugin>();
  for (const plugin of rawPlugins.values()) {
    // SAFETY: omp's loader types merged setting values as Record<string,
    // unknown>, but every persisted value is JSON from config/override files,
    // so the record already satisfies JsonRecord — nothing is translated.
    const settingValues = (await getPluginSettings(plugin.name, directory)) as JsonRecord;
    const projected = projectPlugin(plugin, { settingValues });
    plugins.push(projected);
    pluginRawById.set(projected.id, plugin);
  }

  const marketplaceRawById = new Map<string, MarketplaceRawPlugin>();
  for (const summary of marketplacePlugins) {
    const entry = summary.entries?.find((item) => item.enabled !== false) ?? summary.entries?.[0];
    if (!entry) continue;
    const packageJsonPath = path.join(entry.installPath, 'package.json');
    let packageJson: MarketplacePackageJson | null;
    try {
      packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
    } catch {
      packageJson = null;
    }
    const manifest = packageJson?.omp ?? packageJson?.pi;
    const projected: ProjectedPlugin = {
      id: encodePluginId('marketplace', summary.scope, summary.id),
      kind: 'marketplace',
      scope: summary.scope,
      name: summary.id,
      version: entry.version || packageJson?.version || 'unknown',
      enabled: entry.enabled !== false,
      editable: true,
      permissions: {
        toggle: true,
        features: false,
        settings: false,
        uninstall: true,
      },
      ...(manifest?.description ? { description: manifest.description } : {}),
      features: manifestFeatures(manifest, null),
      settings: manifestSettings(manifest, {}),
      extensionEntries: [],
    };
    plugins.push(projected);
    marketplaceRawById.set(projected.id, {
      name: summary.id,
      scope: summary.scope,
      version: projected.version,
      enabled: projected.enabled,
      enabledFeatures: null,
      manifest,
      path: entry.installPath,
      cwd: directory,
    });
  }

  const discovered = new Map<string, string>(discoveredPaths.map((item): [string, string] => [canonicalPath(item), item]));
  const extensions: ExtensionRecord[] = [];
  const extensionIds = new Set<string>();


  for (const plugin of plugins) {
    const raw = pluginRawById.get(plugin.id);
    // Only installed RawPlugin rows carry the manifest surface the SDK
    // resolver reads; marketplace rows have no local manifest to walk.
    if (!raw) continue;
    for (const entry of resolvePluginManifestEntries(raw, 'extensions')) {
      const resolved = entry.resolvedPath ? canonicalPath(entry.resolvedPath) : null;
      const record = projectExtension({
        filePath: resolved ?? path.join(raw.path, entry.entry),
        scope: plugin.scope,
        source: 'plugin-manifest',
        editable: false,
        pluginId: plugin.id,
        pluginName: plugin.name,
        declaredEntry: entry.entry,
        loaded: resolved ? discovered.has(resolved) : false,
      });
      plugin.extensionEntries.push(record.id);
      if (!extensionIds.has(record.id)) {
        extensionIds.add(record.id);
        extensions.push(record);
      }
    }
  }

  const configuredRoots = extensionSettings.configured.map((item) => canonicalPath(path.isAbsolute(item) ? item : path.resolve(directory, item)));
  for (const discoveredPath of discovered.keys()) {
    if (extensionIds.has(extensionId(discoveredPath))) continue;
    const native = nativeDirs.find((entry) => isWithin(discoveredPath, canonicalPath(entry.path)));
    const configured = configuredRoots.some((root) => isWithin(discoveredPath, root));
    const scope = native?.scope ?? (isWithin(discoveredPath, canonicalPath(directory)) ? 'project' : 'user');
    const record = projectExtension({
      filePath: discoveredPath,
      scope,
      source: native ? 'native' : configured ? 'configured' : 'discovered',
      editable: Boolean(native && path.dirname(discoveredPath) === canonicalPath(native.path)),
    });
    extensionIds.add(record.id);
    extensions.push(record);
  }

  return { plugins, extensions };
};

/**
 * 在域路由表上挂载插件管理端点（全部要求 plugins.v1 capability 开启）：
 * 列表/安装/PATCH 状态变更/卸载、扩展读取/新建/删除、两个 reveal、
 * applied 快照与会话重载。变更类操作成功后统一清空发现缓存并返回
 * restartDeferred（需重启 engine 才生效）。
 */
export const registerPluginsDomainRoutes = (
  route: DomainRoute,
  { features = ompFeatures(), list = listPlugins, snapshots = null, reloadSessions = null }: PluginsDomainDeps = {},
): void => {
  // GET /omp/plugins：列出目录的插件与扩展投影。
  route('GET', '/omp/plugins', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      return json(await list(directory));
    } catch (error) {
      console.warn('[omp-host] failed to list plugins:', errorText(error));
      return failed('Failed to list omp plugins');
    }
  });

  // POST /omp/plugins：安装/链接插件；project 作用域仅支持 marketplace 引用。
  route('POST', '/omp/plugins', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const body = await jsonBody(request);
    const spec = typeof body?.spec === 'string' ? body.spec.trim() : '';
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    if (!spec) return badRequest('spec is required');
    try {
      const manager = new PluginManager(directory);
      const marketplaceManager = await marketplaceManagerFor(directory);
      const marketplaces = await marketplaceManager.listMarketplaces();
      const target = classifyInstallTarget(spec, new Set(marketplaces.map((entry) => entry.name)));
      const scope = body?.scope === 'project' ? 'project' : 'user';
      // Project scope is the stricter choice; silently downgrading it to user
      // would betray the explicit intent. Reject instead — the UI explains the
      // marketplace-only rule the moment Project is selected.
      if (scope === 'project' && target.type !== 'marketplace') {
        return badRequest('Project scope is only supported for marketplace installs (name@marketplace). Install user-scope, or use a marketplace reference.');
      }
      if (target.type === 'marketplace') {
        await marketplaceManager.installPlugin(target.name, target.marketplace, { scope });
      } else if (target.type === 'local') {
        const localPath = target.path === '~' || target.path.startsWith('~/') || target.path.startsWith('~\\')
          ? path.join(os.homedir(), target.path.slice(2))
          : target.path;
        await manager.link(localPath);
      } else {
        await manager.install(target.spec);
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin installed. Restart the omp engine to apply it.'));
    } catch (error) {
      console.warn('[omp-host] failed to install plugin:', errorText(error));
      return failed('Failed to install omp plugin');
    }
  });

  // GET /omp/plugins/extensions/{id}：读取已登记扩展的源码与元数据。
  route('GET', '/omp/plugins/extensions/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    if (!id) return badRequest('extension id required');
    const target = extensionFilesById.get(id);
    if (!target) return notFound('extension not found');
    try {
      return json({
        fileName: path.basename(target.path),
        scope: target.scope,
        content: fs.readFileSync(target.path, 'utf8'),
        editable: target.editable === true,
        source: target.source,
      });
    } catch {
      return failed('Failed to read omp extension');
    }
  });

  // POST /omp/plugins/extensions：在原生扩展目录新建扩展文件（重名回 409）。
  route('POST', '/omp/plugins/extensions', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const body = await jsonBody(request);
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    const scope = body?.scope === 'project' ? 'project' : 'user';
    const fileName = typeof body?.fileName === 'string' ? body.fileName.trim() : '';
    if (!EXTENSION_FILE_PATTERN.test(fileName)) return badRequest('invalid extension file name');
    const dirs = await nativeExtensionDirectories(directory);
    const targetDir = dirs.find((entry) => entry.scope === scope);
    if (!targetDir) return badRequest('project extension scope unavailable');
    const targetPath = path.join(targetDir.path, fileName);
    if (fs.existsSync(targetPath)) return json({ error: 'extension already exists' }, { status: 409 });
    fs.mkdirSync(targetDir.path, { recursive: true });
    fs.writeFileSync(targetPath, typeof body?.content === 'string' ? body.content : '', 'utf8');
    rememberExtension(targetPath, { editable: true, scope, source: 'native' });
    return json(restartDeferred('OMP extension created. Restart the omp engine to apply it.'));
  });

  // DELETE /omp/plugins/extensions/{id}：删除可编辑扩展文件（只读条目拒绝）。
  route('DELETE', '/omp/plugins/extensions/{id}', async (_request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    if (!id) return badRequest('extension id required');
    const target = extensionFilesById.get(id);
    if (!target) return notFound('extension not found');
    if (target.editable !== true) return badRequest('extension entry is read-only');
    fs.unlinkSync(target.path);
    extensionFilesById.delete(id);
    return json(restartDeferred('OMP extension removed. Restart the omp engine to apply it.'));
  });

  // PATCH /omp/plugins/{id}：变更插件状态 —— marketplace 启停；项目级走覆盖文件；用户级写全局注册表。
  route('PATCH', '/omp/plugins/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    const target = id ? decodePluginId(id) : null;
    if (!target) return badRequest('invalid plugin id');
    const body = await jsonBody(request);
    const directory = typeof body?.directory === 'string' && body.directory ? body.directory : process.cwd();
    try {
      if (target.kind === 'marketplace') {
        if (typeof body?.enabled !== 'boolean') return badRequest('enabled must be boolean');
        const manager = await marketplaceManagerFor(directory);
        await manager.setPluginEnabled(target.name, body.enabled, target.scope);
      } else if (target.scope === 'project') {
        // Project-scoped package plugins mutate through .omp/plugin-overrides.json
        // (the same file omp TUI manages) instead of the global lockfile.
        // SAFETY: PATCH body contract — the plugins UI posts exactly these
        // mutation fields as JSON; the override writer re-checks every field
        // before it reaches a write, so malformed shapes stay no-ops.
        applyProjectOverride(directory, target.name, body as PluginMutation);
      } else {
        const manager = new PluginManager(directory);
        if (typeof body?.enabled === 'boolean') await manager.setEnabled(target.name, body.enabled);
        if (Array.isArray(body?.enabledFeatures)) {
          // SAFETY: the plugins UI posts feature-name strings; omp persists
          // the array verbatim, so the parsed JSON array is the SDK's string[].
          await manager.setEnabledFeatures(target.name, body.enabledFeatures as string[]);
        }
        const setting = asJsonRecord(body.setting);
        if (setting && typeof setting.key === 'string') {
          if (setting.remove === true) {
            await manager.deletePluginSetting(target.name, setting.key);
          } else {
            const plugin = (await manager.list()).find((item) => item.name === target.name);
            const schema = plugin?.manifest?.settings?.[setting.key];
            if (!schema) return badRequest('unknown plugin setting');
            const value = typeof setting.value === 'string'
              ? parseSettingValue(setting.value, schema)
              : setting.value;
            const validation = validateSetting(value, schema);
            if (!validation.valid) return badRequest(validation.error ?? 'invalid plugin setting');
            await manager.setPluginSetting(target.name, setting.key, value);
          }
        }
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin state updated. Restart the omp engine to apply it.'));
    } catch (error) {
      if (/not found|does not exist/i.test(errorText(error))) return notFound('plugin not found');
      console.warn('[omp-host] failed to update plugin:', errorText(error));
      return failed('Failed to update omp plugin');
    }
  });

  // DELETE /omp/plugins/{id}：卸载插件（项目级包插件只读，拒删）。
  route('DELETE', '/omp/plugins/{id}', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    const target = id ? decodePluginId(id) : null;
    if (!target) return badRequest('invalid plugin id');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      if (target.kind === 'marketplace') {
        const manager = await marketplaceManagerFor(directory);
        await manager.uninstallPlugin(target.name, target.scope);
      } else {
        if (target.scope !== 'user') return badRequest('project package plugins are read-only');
        await new PluginManager(directory).uninstall(target.name);
      }
      await invalidatePluginCaches(directory);
      return json(restartDeferred('OMP plugin removed. Restart the omp engine to apply it.'));
    } catch (error) {
      if (/not found|does not exist/i.test(errorText(error))) return notFound('plugin not found');
      console.warn('[omp-host] failed to remove plugin:', errorText(error));
      return failed('Failed to remove omp plugin');
    }
  });

  // POST /omp/plugins/{id}/reveal：在系统文件管理器中定位插件安装目录。
  route('POST', '/omp/plugins/{id}/reveal', async (request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    const target = id ? decodePluginId(id) : null;
    if (!target) return badRequest('invalid plugin id');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    try {
      const targetPath = await pluginPathForId(target, directory);
      if (!targetPath || !fs.existsSync(targetPath)) return notFound('plugin path not found');
      await revealInFileManager(targetPath);
      return json({ ok: true });
    } catch (error) {
      console.warn('[omp-host] failed to reveal plugin:', errorText(error));
      return failed('Failed to reveal omp plugin');
    }
  });

  // POST /omp/plugins/extensions/{id}/reveal：在系统文件管理器中定位扩展文件。
  route('POST', '/omp/plugins/extensions/{id}/reveal', async (_request, ctx) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const id = ctx?.params.id;
    if (!id) return badRequest('extension id required');
    const target = extensionFilesById.get(id);
    if (!target) return notFound('extension not found');
    if (!fs.existsSync(target.path)) return notFound('extension path not found');
    try {
      await revealInFileManager(target.path);
      return json({ ok: true });
    } catch (error) {
      console.warn('[omp-host] failed to reveal extension:', errorText(error));
      return failed('Failed to reveal omp extension');
    }
  });
  // GET /omp/plugins/applied：把各会话应用的扩展路径 join 成列表 id 后投影。
  route('GET', '/omp/plugins/applied', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!snapshots) return badRequest('applied snapshots unavailable');
    try {
      // Populate the extension id map first so snapshot paths can join the
      // same projected ids the Settings list already renders.
      await list(directory ?? process.cwd());
      const byPath = new Map([...extensionFilesById.entries()].map(([id, meta]) => [meta.path, id]));
      const sessions = snapshots()
        .filter((snapshot) => !directory || snapshot.directory === directory)
        .map((snapshot) => ({
          sessionId: snapshot.sessionId,
          directory: snapshot.directory,
          appliedAt: snapshot.appliedAt,
          extensionIds: snapshot.extensionPaths
            .map((item) => byPath.get(item))
            .filter(Boolean),
          pluginNames: snapshot.pluginNames,
        }));
      return json({ sessions });
    } catch (error) {
      console.warn('[omp-host] failed to project applied plugins:', errorText(error));
      return failed('Failed to project applied plugins');
    }
  });

  // POST /omp/plugins/reload：触发目录（或单会话）的插件重载并返回刷新数。
  route('POST', '/omp/plugins/reload', async (request) => {
    if (features?.['plugins.v1'] !== true) return featureUnavailable('plugins.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    if (typeof reloadSessions !== 'function') return badRequest('reload unavailable');
    try {
      const { sessionsRefreshed } = await reloadSessions(directory, url.searchParams.get('sessionId'));
      return json({ ok: true, sessionsRefreshed });
    } catch (error) {
      console.warn('[omp-host] failed to reload plugins:', errorText(error));
      return failed('Failed to reload omp plugins');
    }
  });
};

// 尾部具名导出：让路由内部工具可被测试与协调者直接复用。
export { decodePluginId, encodePluginId, listPlugins, projectExtension, projectPlugin };
