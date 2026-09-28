// Domain module: omp-parity chapter 08 §5.4 (slash command pipeline), server
// side.
//
// GET /omp/commands (public path /api/omp/commands, R3) is the omp command
// discovery surface feeding the UI's three-layer slash pipeline:
//   Tier A (`tier: 'client-builtin'`) — omp built-in semantic commands, names
//     reserved by the engine (the full BUILTIN_SLASH_COMMANDS_INTERNAL
//     registry, including TUI-only handlers, because collision resolution is
//     about NAMES — /debug /compact /review must never silently resolve to an
//     OMPChamber layer).
//   Tier B (`tier: 'engine'`) — commands the engine expands itself when the
//     text reaches a materialized session: file markdown commands (loaded
//     from the directory), skills, and (on live sessions) extension/custom
//     TS commands.
//
// Discovery is headless: buildAvailableSlashCommands runs against a synthetic
// AvailableCommandsSession (skills via discoverSkills — same precedent as the
// wire /skill route — and file commands from the requested directory's cwd).
// Extension/custom TS commands need a live session's extension runner and are
// therefore absent here; the engine still expands them if sent, and the UI
// must not treat this list as exhaustive for those sources.
//
// Capability `commands.v1` gates the endpoint (master R2): a missing/false
// key answers an explicit 501 so the client falls back to its legacy
// two-source resolution (skills store + OC commands store, 08 §5.4).
//
// SELF-CONTAINED BY CONTRACT: no engine.js/endpoints.js imports; the
// coordinator mounts registerCommandsDomainRoutes(route, { features }).
/**
 * 域模块：omp 斜杠命令管线（omp-parity 第 08 章 §5.4），服务端。
 *
 * GET /omp/commands（公开路径 /api/omp/commands，R3）是喂给 UI 三层
 * 斜杠管线的命令发现面：Tier A（tier: 'client-builtin'）为 omp 内建
 * 语义命令，名字由引擎保留（完整的 BUILTIN_SLASH_COMMANDS_INTERNAL
 * 注册表，含 TUI-only handler——碰撞裁决关乎名字本身）；Tier B
 * （tier: 'engine'）为引擎在会话上自行展开的命令（file markdown、
 * skills 与 live 会话上的扩展命令）。发现过程无头（headless）：
 * buildAvailableSlashCommands 跑在合成的 AvailableCommandsSession 上。
 * capability `commands.v1` 门控端点（master R2），关闭时显式 501 让
 * 客户端回退旧的双源解析。契约上自包含：不 import
 * engine.js/endpoints.js，由协调器挂载 registerCommandsDomainRoutes。
 */

import { discoverSkills } from '@oh-my-pi/pi-coding-agent';
import {
  buildAvailableSlashCommands,
} from '@oh-my-pi/pi-coding-agent/slash-commands/available-commands';
import {
  BUILTIN_SLASH_COMMANDS_INTERNAL,
} from '@oh-my-pi/pi-coding-agent/slash-commands/builtin-registry';
import type {
  AvailableCommandsSession,
  InternalAvailableSlashCommand,
} from '@oh-my-pi/pi-coding-agent/slash-commands/available-commands';
import type { Skill } from '@oh-my-pi/pi-coding-agent/extensibility/skills';
import { featureUnavailable, ompFeatures } from './omp-parity.ts';

/** Response.json 自身的参数契约——平台持有的类型透传。 */
/** Response.json's own parameter contract — platform-owned passthrough. */
type ResponseJsonData = Parameters<typeof Response.json>[0];
/** JSON 响应构造直通函数。 */
const json = (data: ResponseJsonData, init?: ResponseInit) => Response.json(data, init);

/** 响应分层枚举（08 §5.4：client-builtin = Tier A，engine = Tier B）。 */
/** Response tiers (08 §5.4: 'client-builtin' = Tier A, 'engine' = Tier B). */
export const OMP_COMMAND_TIERS = Object.freeze(['client-builtin', 'engine'] as const);

/** 由 source 推导分层：builtin 视为客户端侧，其余均为引擎可展开。 */
/** Engine-expandable sources; everything else (builtin) is client-side. */
const tierForSource = (source?: string): OmpCommandTier => (source === 'builtin' ? 'client-builtin' : 'engine');

/** 命令分层字面量联合类型：'client-builtin' | 'engine'。 */
export type OmpCommandTier = (typeof OMP_COMMAND_TIERS)[number];

/** 可投影的内部命令形状：SDK 的 InternalAvailableSlashCommand /
 *  SlashCommandSpec 的结构子集（字段全部可选，投影时逐项校验）。 */
export interface ProjectableOmpCommand {
  /** 命令名（唯一保证存在的字段）。 */
  name?: string;
  /** 别名列表（可选）。 */
  aliases?: string[];
  /** 人类可读描述（可选）。 */
  description?: string;
  /** 参数模板提示（input.hint 形式，可选）。 */
  input?: { hint?: string };
  /** ACP 形式的参数提示（可选）。 */
  acpInputHint?: string;
  /** 行内提示（可选）。 */
  inlineHint?: string;
  /** 命令来源：'builtin' | 'file' | 'skill' | 'extension' 等。 */
  source?: string;
}

/** 线缆上的命令记录：GET /omp/commands 返回的行结构。 */
export interface OmpCommandRecord {
  /** 命令名（保证非空）。 */
  name: string;
  /** 描述（可选）。 */
  description?: string;
  /** 分层：Tier A 或 Tier B。 */
  tier: OmpCommandTier;
  /** 原始来源标签（builtin/file/skill/extension…）。 */
  source: string;
  /** 过滤后的非空别名列表（可选）。 */
  aliases?: string[];
  /** 参数模板提示（可选）。 */
  argumentHint?: string;
}

/** 把一条内部命令投影为线缆记录：名称缺失或非字符串返回 null；
 *  description 与参数模板保持可选——仅 name/tier/source 有保证。 */
/**
 * Project one InternalAvailableSlashCommand / SlashCommandSpec into the wire
 * record. `description` and the argument template stay optional — the only
 * guaranteed fields are name/tier/source.
 */
export const projectOmpCommand = (internal: ProjectableOmpCommand | null): OmpCommandRecord | null => {
  if (!internal || typeof internal.name !== 'string' || !internal.name) return null;
  const hint = internal.input?.hint ?? internal.acpInputHint ?? internal.inlineHint;
  return {
    name: internal.name,
    ...(typeof internal.description === 'string' && internal.description
      ? { description: internal.description }
      : {}),
    tier: tierForSource(internal.source),
    source: internal.source ?? 'engine',
    ...(Array.isArray(internal.aliases) && internal.aliases.length > 0
      ? { aliases: internal.aliases.filter((a) => typeof a === 'string' && a) }
      : {}),
    ...(typeof hint === 'string' && hint ? { argumentHint: hint } : {}),
  };
};

/** Tier A 行：引擎保留的内建命令名（完整注册表，含 TUI-only 名字）。 */
/** Tier A rows: the engine's reserved built-in command names. */
export const builtinOmpCommands = (): OmpCommandRecord[] => {
  const rows: OmpCommandRecord[] = [];
  for (const spec of BUILTIN_SLASH_COMMANDS_INTERNAL) {
    // BUILTIN_SLASH_COMMANDS_INTERNAL rows carry the raw description;
    // acpDescription overrides it where defined (available-commands.ts:49).
    const projected = projectOmpCommand({
      name: spec.name,
      aliases: spec.aliases,
      description: spec.acpDescription ?? spec.description,
      input: spec.acpInputHint ?? spec.inlineHint ? { hint: spec.acpInputHint ?? spec.inlineHint } : undefined,
      source: 'builtin',
    });
    if (projected) rows.push(projected);
  }
  return rows;
};

/** 生产环境 Tier B 加载器（跑无头 AvailableCommandsSession 聚合）。 */
/** Production Tier B loader (headless AvailableCommandsSession). */
const defaultLoadAvailable = (session: AvailableCommandsSession) => buildAvailableSlashCommands(session);

/** 生产环境 skills 加载器——与线缆 /skill 路由同一套 discoverSkills。 */
/** Production skills loader — same discovery the wire /skill route uses. */
const defaultLoadSkills = async (directory: string): Promise<readonly Skill[]> => {
  const { skills } = await discoverSkills(directory);
  return skills ?? [];
};

/** live 会话扩展命令加载器：按目录返回 pi.registerCommand 注册的命令；
 *  无 live 会话时返回 null/undefined，失败由调用方按降级处理。 */
export type OmpLiveCommandsLoader = (
  directory: string,
) => Promise<readonly ProjectableOmpCommand[] | null | undefined>;

/** listOmpCommands 的输入：目标目录与三个可注入的加载器（测试替身用）。 */
export interface ListOmpCommandsInput {
  /** 命令发现的作用目录（file 命令与 skills 的 cwd）。 */
  directory: string;
  /** Tier B 聚合加载器；缺省为 buildAvailableSlashCommands。 */
  loadAvailable?: (
    session: AvailableCommandsSession,
  ) => Promise<readonly InternalAvailableSlashCommand[] | null | undefined>;
  /** skills 加载器；缺省为 discoverSkills。 */
  loadSkills?: (directory: string) => Promise<readonly Skill[]>;
  /** live 扩展命令加载器；null 表示不合并 live 源（旧调用形态）。 */
  loadLiveCommands?: OmpLiveCommandsLoader | null;
}

/** 聚合单个目录的 omp 命令列表：内建行永远在前（名字保留），引擎命令
 *  按名去重后追加（first-seen-wins）；任一发现源失败都降级为仅内建
 *  列表——绝不返回空成功，也不抛 500 拖垮自动补全合并。 */
/**
 * Aggregate the omp command list for one directory. Builtins always lead
 * (name reservation); engine commands are appended name-deduped, mirroring
 * buildAvailableSlashCommands' first-seen-wins semantics. A discovery failure
 * degrades to the builtin-only list — never to an empty success, and never to
 * a 500 that would take the whole autocomplete merge down.
 *
 * @param {{ directory: string, loadAvailable?: (session: object) => Promise<Array<object>>, loadSkills?: (directory: string) => Promise<Array<object>> }} input
 */
export const listOmpCommands = async ({
  directory,
  loadAvailable = defaultLoadAvailable,
  loadSkills = defaultLoadSkills,
  loadLiveCommands = null,
}: ListOmpCommandsInput): Promise<OmpCommandRecord[]> => {
  const commands: OmpCommandRecord[] = [];
  const seen = new Set<string>();
  const append = (record: OmpCommandRecord | null) => {
    if (!record || seen.has(record.name)) return;
    seen.add(record.name);
    commands.push(record);
  };
  for (const record of builtinOmpCommands()) append(record);
  try {
    const skills = await loadSkills(directory);
    const available = await loadAvailable({
      customCommands: [],
      skills: Array.isArray(skills) ? skills : [],
      // Default per settings-schema.ts:4786-4790 (skills.enableSkillCommands).
      skillsSettings: { enableSkillCommands: true },
      setSlashCommands: () => {},
      sessionManager: { getCwd: () => directory },
    });
    for (const internal of available ?? []) {
      if (internal?.source === 'builtin') continue; // already covered above
      append(projectOmpCommand(internal));
    }
  } catch {
    // Degraded, not authoritative-empty: the builtin half is still real.
  }
  // Live-session extension commands (09 §5.4 discovery gap): the headless
  // synthetic session above has no extension runner, so commands registered
  // by user extensions (pi.registerCommand) only exist on materialized
  // sessions. A degraded/absent live source appends nothing — the builtin
  // + headless halves stay authoritative.
  if (typeof loadLiveCommands === 'function') {
    try {
      for (const live of (await loadLiveCommands(directory)) ?? []) {
        append(projectOmpCommand({ ...live, source: live?.source ?? 'extension' }));
      }
    } catch {
      // Live source unavailable — same degradation contract as above.
    }
  }
  return commands;
};

/** registerCommandsDomainRoutes 的挂载选项。 */
export interface CommandsRouteMountOptions {
  /** feature 键值表；缺省取 ompFeatures()。 */
  features?: Record<string, boolean>;
  /** 列表聚合函数；缺省 listOmpCommands（测试可注入替身）。 */
  list?: typeof listOmpCommands;
  /** live 扩展命令加载器；null 表示路由不带 live 源。 */
  liveCommandsFor?: OmpLiveCommandsLoader | null;
}

/** 路由挂载函数类型：（method, pattern, handler）三元组注册。 */
type CommandsRouteMount = (
  method: string,
  pattern: string,
  handler: (request: Request) => Response | Promise<Response>,
) => void;

/** 挂载本域拥有的 /omp 路由。按 master R2 做 capability 门控：
 *  `commands.v1` 关闭 ⇒ 显式 501（客户端回退旧双源解析，绝不静默空表）。 */
/**
 * Mount the /omp routes owned by this domain. Capability-gated per master R2:
 * `commands.v1` off ⇒ explicit 501 (clients fall back to the legacy
 * two-source resolution, never a silent empty list).
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {{ features?: Record<string, boolean>, list?: typeof listOmpCommands, liveCommandsFor?: ((directory: string) => Promise<Array<object>>) | null }} [options]
 */
export function registerCommandsDomainRoutes(
  route: CommandsRouteMount,
  { features = ompFeatures(), list = listOmpCommands, liveCommandsFor = null }: CommandsRouteMountOptions = {},
): void {
  route('GET', '/omp/commands', async (request) => {
    if (features?.['commands.v1'] !== true) return featureUnavailable('commands.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    return json(await list({
      directory,
      ...(typeof liveCommandsFor === 'function' ? { loadLiveCommands: liveCommandsFor } : {}),
    }));
  });
}
