// omp-parity foundation: capabilities negotiation + event registry access
// (spec 05 §5.2.2/§5.2.3, master D6-R1/R2).
//
// `GET /api/omp/capabilities` is the single server-adjudicated switchboard:
// feature keys defined here gate every /api/omp domain surface, and the
// event schema version negotiates the omp event channel. Domain modules
// flip their key when their surface lands; consumers must treat a missing
// key or a 404 as "feature off" and degrade to wire-only behavior.

// The event registry is bundled build-time data: a static JSON import is
// inlined by every bundler (including `bun build --compile`, where a
// runtime readFileSync against __dirname looks into the bunfs root and the
// file is not there — capabilities 500 on every packaged build).
/**
 * omp-parity 基础：capability 协商 + 事件注册表访问
 * （spec 05 §5.2.2/§5.2.3，master D6-R1/R2）。`GET /api/omp/capabilities`
 * 是唯一的服务端裁决开关板：此处定义的 feature 键门控每个 /api/omp
 * 域表面，事件 schema 版本协商 omp 事件通道；消费方必须把缺键或 404
 * 视为「feature 关闭」并降级到 wire-only 行为。
 */
import ompEventRegistry from './omp-event-registry.json' with { type: 'json' };

/** 事件注册表中单个 omp 事件的条目：持久性、作用域与关联端点。 */
export interface OmpEventRegistryEntry {
  /** 是否 durable（重连可重放）。 */
  durable: boolean;
  /** 作用域（如 session/directory/global）。 */
  scope: string;
  /** 断线重连后可恢复该事件的快照端点列表。 */
  snapshotEndpoints: string[];
  /** 引入该事件的最低版本。 */
  since: string;
  /** 门控该事件的 capability 键（可选）。 */
  gated?: string;
  /** 是否控制类事件（可选）。 */
  control?: boolean;
}

/** 事件注册表清单：schema 版本 + 事件名 → 条目。 */
export interface OmpEventRegistryManifest {
  /** 事件 schema 版本串。 */
  eventSchema: string;
  /** 事件名 → 注册表条目。 */
  events: Record<string, OmpEventRegistryEntry>;
}

/** 读取构建期内联的 omp 事件注册表清单。 */
export const loadOmpEventRegistry = (): OmpEventRegistryManifest => {
  const manifest = ompEventRegistry;
  return { eventSchema: manifest.eventSchema, events: manifest.events };
};

/** feature 开关表，服务端裁决（master R2）：域落地时在此翻键；
 *  false 键的端点保持显式 501（见 featureUnavailable）。 */
/**
 * Feature flags, server-adjudicated (master R2). A domain landing flips its
 * key here; `false` keys keep their endpoints answering explicit 501s with
 */
export const ompFeatures = () => ({
  // 05: event channel + transcript structured reads (foundation).
  events: true,
  'sessions.telemetry': true,
  // 01/06: model roles + settings proxy + per-directory keyed instances.
  'modelRoles.v1': true,
  'settings.v1': true,
  'settings.projectScopes.v1': true,
  // OMPChamber-owned GUI CRUD over the engine's custom provider file
  // (models.yml); the SDK itself has no write API for it.
  'providers.v1': true,
  // 02: session modes + agent-definitions + personas resources.
  'modes.v1': true,
  'agentDefinitions.v1': true,
  'personas.v1': true,
  // 03: approval + ask dialog bridge (atomic C3+C4+C5 landed; lease-driven
  // hasUI per R13).
  'dialogs.v1': true,
  // 04: local:// URI bridge + session tree + agent-runs hub.
  'uri.v1': true,
  'tree.v1': true,
  'agentRuns.v1': true,
  // Session process monitor (PLAN-session-process-monitor.md): per-session
  // process tracking, output tails and kill over the host's descendant tree.
  // The domain self-gates to 501 while the native platform is unresolved.
  'processes.v1': true,
  // 04: artifacts browse — host-level read-only listing of every session's
  // private local:// root (spec 04 §1 P2 item; endpoint /api/omp/artifacts).
  artifacts: true,
  // OMP plugin manager surface (npm + marketplace registries).
  'plugins.v1': true,
  // 09: extension chrome projection — widget/status strings via the dialog
  // bridge, mirroring RpcExtensionUIRequest (chapter 09 §5.0).
  // 08 §5.4: engine slash-command discovery (skills + file commands via the
  // headless buildAvailableSlashCommands session). Upstream has no
  // capability gate for command enumeration (TUI/ACP enumerate
  // unconditionally, settings-gated only), so the switch is ours alone.
  'commands.v1': true,
  // 07 §3.2 / GAP-G05: `!` local execution surface — POST /omp/sessions/{id}/bash
  // runs the session's own BashRunner (executeBash) and projects the persisted
  // bashExecution record onto the wire as a synthetic user card.
  'bash.v1': true,
  // jobs: SDK AsyncJobManager only attaches to the first top-level session;
  // capability stays false until upstream injection (master R12).
  'jobs.v1': false,
  // 08: queue ack protocol needs SDK extension first (master R14).
  'queue.v1': false,
  // R15: MCP executable endpoints are out of scope this cycle; read-only +
  // disabled switches are the long-term steady state.
  'mcp.executable': false,
  'mcp.readOnly': true,
}) satisfies OmpFeatures;

/** GET /api/omp/capabilities 的响应体。 */
export interface OmpCapabilities {
  /** capability 载荷版本（当前为 1）。 */
  version: number;
  /** 协商的 omp 事件 schema 版本。 */
  eventSchema: string;
  /** 全量 feature 开关表。 */
  features: OmpFeatures;
  /** 客户端最低兼容 UI 版本。 */
  minUiVersion: string;
}

/** 组装 capability 载荷：版本 + 事件 schema + 当前 feature 表。 */
export const buildCapabilities = (): OmpCapabilities => {
  const registry = loadOmpEventRegistry();
  return {
    version: 1,
    eventSchema: registry.eventSchema,
    features: ompFeatures(),
    minUiVersion: '0.0.0',
  };
};

/** feature 开关表类型：只读的键 → 布尔映射。 */
export type OmpFeatures = { readonly [key: string]: boolean };

/** 判定某 capability 载荷上指定 feature 键是否开启（缺键即关闭）。 */
export const featureEnabled = (capabilities: OmpCapabilities | null | undefined, key: string): boolean =>
  Boolean(capabilities?.features?.[key]);

/** 被门控关闭的域表面的显式 501 响应体（响亮失败，R2）。 */
/** Explicit 501 body for gated-off domain surfaces (fail loudly, R2). */
export const featureUnavailable = (key: string): Response =>
  Response.json({ error: `${key}-unavailable` }, { status: 501 });

/** catch 变量探针：抛出值携带 message 时取之，否则渲染原值。 */
/** Catch-variable probe: the message string when the thrown value carries one, else the value rendered. */
export const errorText = (cause: unknown): string => {
  if (cause instanceof Error) return cause.message;
  // SAFETY: non-Error throws may carry a message field; anything else renders.
  return String((cause as { message?: unknown } | null | undefined)?.message ?? cause);
};

/** catch 变量探针：存在 NodeJS 风格 code 字符串时取之，否则 undefined。 */
/** Catch-variable probe: a NodeJS-style code string when present. */
export const errorCode = (cause: unknown): string | undefined => {
  // SAFETY: NodeJS fs/process errors carry a string code field; absent otherwise.
  const code = (cause as { code?: unknown } | null | undefined)?.code;
  // oxlint-disable-next-line no-runtime-typeof
  return typeof code === 'string' ? code : undefined;
};
