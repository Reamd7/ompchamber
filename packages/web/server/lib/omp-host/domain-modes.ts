// Session modes & agents domain — spec docs/omp-parity/02-agents-and-modes.md
// (server side, Wave-1 self-contained module; the coordinator mounts it).
//
// Surfaces:
//   1. createModeTracker       — session mode state machine (02 §5.4) with
//                                 mode_change entry persistence and the
//                                 omp.mode.changed projection.
//   2. agent-definitions CRUD  — omp agent discovery chain as the read
//                                 authority; writes are .md files in the
//                                 user/project agents dirs; bundled is
//                                 read-only (02 §5.2, GAP-B03/B04).
//   3. personas CRUD + personaFor — independent persona resource and the
//                                 materialize-time systemPrompt overlay
//                                 (02 §5.2a, master D6-R12).
//   4. planReviewBridge        — xd://propose hook producing
//                                 omp.plan.review_requested and the GET /plan
//                                 review payload (02 §5.5).
//   5. createModesDomain + registerModesDomainRoutes — per-session
//                                 tracker/bridge ownership and route mounting.
//
// Engine integration points (coordinator wires; this module never imports
// engine.js — all SDK state reaches it through the injected callbacks):
//   - #materialize: persona overlay via personaFor(meta, personasStore) →
//     systemPrompt/toolNames; status 'standard' = no overlay (02 §5.1 D-B2).
//     (The old planYolo mapping is deleted — plan mode is a session mode
//     driven by the mode endpoints, 02 §5.8.)
//   - per hostSession: domain.trackerFor(id, dir) / domain.bridgeFor(id, dir);
//     on plan enter call session.setPlanProposalHandler(bridge.hookFor(session))
//     (SDK: agent-session.ts:1733-1735, mirrors TUI interactive-mode.ts:2739).
//   - #handleEngineEvent 'goal_updated': tracker.applyGoalUpdate(event.goal,
//     event.state) keeps the mode snapshot fresh; the omp.goal.updated publish
//     itself already lives in engine.js (Wave 0).
//   - prompt(): persona-switch rebuild condition (02 §5.1 D-B3).

/**
 * omp-host 领域模块：会话模式（modes）与 agent / persona 定义。
 *
 * 本文件是 Wave-1 自包含的服务端领域模块，由 coordinator 挂载，对外
 * 提供（规范 docs/omp-parity/02-agents-and-modes.md）：
 * 1. createModeTracker——会话模式状态机（02 §5.4），含 mode_change 条目
 *    持久化与 omp.mode.changed 投影；
 * 2. agent-definitions CRUD——omp agent 发现链为读权威，写入是用户 /
 *    项目 agents 目录的 .md 文件，bundled 只读（02 §5.2，GAP-B03/B04）；
 * 3. personas CRUD + personaFor——独立 persona 资源与物化时的
 *    systemPrompt overlay（02 §5.2a，master D6-R12）；
 * 4. planReviewBridge——xd://propose 钩子，产出 omp.plan.review_requested
 *    与 GET /plan 评审载荷（02 §5.5）；
 * 5. createModesDomain + registerModesDomainRoutes——per-session 的
 *    tracker / bridge 所有权与路由挂载。
 *
 * 本模块从不 import engine.js——所有 SDK 状态经注入回调到达
 * （#materialize 的 persona overlay、每 hostSession 的 trackerFor /
 * bridgeFor、goal_updated 的 applyGoalUpdate、persona 切换的重建条件）。
 */
import fs from 'node:fs';
import path from 'node:path';
import { parse as parseYaml, stringify as stringifyYaml } from 'yaml';
import { BUILTIN_TOOLS } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.ts';
import { errorText, errorCode, featureUnavailable, ompFeatures } from './omp-parity.ts';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** 模式投影值（规范 02 §5.4）。prewalk 是正交的状态位（5.7），不在集合内。 */
/** Mode projection value (02 §5.4). Prewalk is an orthogonal status bit (5.7). */
export type ModeValue = 'none' | 'plan' | 'plan_paused' | 'goal' | 'goal_paused' | 'vibe' | 'loop';

/**
 * 模式投影集合（规范 02 §5.4）——ModeValue 的运行时镜像。prewalk 是
 * 正交状态位（5.7）。类型为 readonly string[] 以便 .includes() 直接
 * 接受未校验的路由输入。
 */
/**
 * Mode projection set (02 §5.4) — the runtime mirror of {@link ModeValue}.
 * Prewalk is an orthogonal status bit (5.7). Typed `readonly string[]` so
 * `.includes()` accepts the unvalidated route input.
 */
export const MODE_VALUES: readonly string[] = Object.freeze([
  'none', 'plan', 'plan_paused', 'goal', 'goal_paused', 'vibe', 'loop',
]);

/** TUI 缺省计划文件（interactive-mode.ts:2307-2309 #getPlanFilePath）。 */
/** TUI default plan file (interactive-mode.ts:2307-2309 #getPlanFilePath). */
export const DEFAULT_PLAN_FILE_PATH = 'local://PLAN.md';

/** 计划评审选项（规范 02 §5.5；TUI plan-review-overlay 3979-3982 行）。 */
/** Plan review choices (02 §5.5; TUI plan-review-overlay options 3979-3982). */
export const PLAN_REVIEW_CHOICES: readonly string[] = Object.freeze([
  'approve-execute', 'approve-compact', 'approve-keep', 'refine',
]);

/** ConfiguredThinkingLevel 的合法选择器（SDK thinking.ts:56-65,138-141）。 */
/** ConfiguredThinkingLevel selectors (SDK thinking.ts:56-65,138-141). */
const THINKING_LEVELS = new Set([
  'auto', 'inherit', 'off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
]);

/** agent / persona 名字规则：字母或数字开头，长度 1-64，允许 A-Za-z0-9._- 连接符。 */
const AGENT_NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;


/** 统一的 JSON 响应快捷方式（可选附加 ResponseInit，如 status）。 */
const json = <T>(data: T, init?: ResponseInit): Response => Response.json(data, init);
/** 400 bad-request 响应快捷方式：`{ error:'bad-request', message }`。 */
const badRequest = (message: string) => json({ error: 'bad-request', message }, { status: 400 });

/**
 * 解析 JSON 请求体（损坏输入 → {}）。期望的线上形状由调用点的类型参数
 * 承载；每个字段都由调用方运行时校验，parse 时的断言是唯一的信任跳跃。
 */
/**
 * Parse a JSON request body (corrupt input → {}). The expected wire shape
 * rides the call-site type parameter; every field is runtime-validated by
 * the caller, so the parse-time assertion is the only leap of faith.
 */
const readJsonBody = async <T extends object>(request: Request): Promise<T> => {
  try {
    // SAFETY: the parsed body is only read through T's fields, each of which
    // the caller runtime-validates before use (see the contract above).
    return (await request.json()) as T;
  } catch {
    // SAFETY: a corrupt body degrades to {} so the callers' per-field checks
    // observe "absent" instead of a parse error mid-route.
    return {} as T;
  }
};

/**
 * 领域错误体：`error` 码字符串 + 抛出点回显的上下文字段
 * （名字、冲突、非法输入拒绝等）。
 */
/**
 * Domain error body: an `error` code string plus the context fields the
 * throwing site echoes (names, conflicts, invalid-input rejects).
 */
export interface ModeErrorBody {
  /** 错误码（如 bad-request、mode-conflict）。 */
  error?: string;
  /** 人类可读信息。 */
  message?: string;
  /** mode-conflict / invalid-transition 上下文（规范 02 §5.4）。 */
  /** mode-conflict / invalid-transition context (02 §5.4). */
  conflict?: string;
  /** 相关模式值。 */
  mode?: string;
  /** 错误所涉 agent / persona 名（规范 02 §5.2/§5.2a）。 */
  /** agent/persona name the error is about (02 §5.2/§5.2a). */
  name?: string;
  /** 被拒绝的 scope 值（规范 02 §5.2）。 */
  /** rejected scope value (02 §5.2). */
  scope?: string;
  /** unknown-tools 拒绝清单（规范 02 §5.2）。 */
  /** unknown-tools reject list (02 §5.2). */
  tools?: string[];
  /** 被拒绝的 thinkingLevel：线上原始值（规范 02 §5.2）。 */
  /** rejected thinkingLevel: the raw wire value (02 §5.2). */
  thinkingLevel?: unknown;
  /** 被拒绝的 loop 进入参数（规范 02 §5.4）。 */
  /** rejected loop-enter values (02 §5.4). */
  count?: number;
  /** 时长上限毫秒数。 */
  durationMs?: number;
  /** 非法评审选项上下文（规范 02 §5.5）。 */
  /** invalid review-choice context (02 §5.5). */
  choice?: string;
  /** 合法选项清单（回显给客户端）。 */
  choices?: string[];
}

/** 携带 HTTP status + body 的领域错误；路由 handler 按 1:1 映射为响应。 */
/** Domain error carrying an HTTP status + body; route handlers map it 1:1. */
export class ModeDomainError extends Error {
  /** HTTP 状态码。 */
  status: number;
  /** 错误响应体。 */
  body: ModeErrorBody;

  /** 以 status + body 构造；message 取 body.message ?? body.error 兜底。 */
  constructor(status: number, body: ModeErrorBody) {
    super(body?.message ?? body?.error ?? 'mode-domain-error');
    this.status = status;
    this.body = body;
  }
}

/**
 * 把投影值归并为冲突族：plan / plan_paused → 'plan'，goal / goal_paused
 * → 'goal'，其余原样返回（pause / resume 的缺省目标即冲突族名）。
 */
const conflictFor = (mode: string): string => {
  if (mode === 'plan' || mode === 'plan_paused') return 'plan';
  if (mode === 'goal' || mode === 'goal_paused') return 'goal';
  return mode;
};

/** 构造 409 mode-conflict 错误：提示先退出当前冲突族模式。 */
const modeConflict = (mode: string) =>
  new ModeDomainError(409, {
    error: 'mode-conflict',
    conflict: conflictFor(mode),
    message: `Exit ${conflictFor(mode)} mode first.`,
  });

// ---------------------------------------------------------------------------
// 1a. Route plumbing (host.ts dispatches handler(request, { params, url, headers }))
// ---------------------------------------------------------------------------

/** host 路由表（host.ts）提供的每请求路由上下文。 */
/** Per-request route context supplied by the host route table (host.ts). */
export interface ModesRouteContext {
  /** 路径参数（如 {id}、{name}）。 */
  params?: { [name: string]: string | undefined };
  /** 解析后的请求 URL。 */
  url?: URL;
  /** 请求头。 */
  headers?: Headers;
}

/** omp-host 路由 handler（Basic auth 由 host.js 在这些 handler 之外强制）。 */
/** omp-host route handler (Basic auth is enforced by host.js outside these). */
export type ModesRouteHandler = (
  request: Request,
  ctx?: ModesRouteContext,
) => Response | Promise<Response>;

/** `route(method, pattern, handler)` 注册回调（host.ts / endpoints.ts）。 */
/** `route(method, pattern, handler)` registration callback (host.ts / endpoints.ts). */
export type ModesRouteMount = (
  method: string,
  pattern: string,
  handler: ModesRouteHandler,
) => void;

// ---------------------------------------------------------------------------
// 1. Storage adapters (personas sidecar)
// ---------------------------------------------------------------------------

/** persona 记录（规范 02 §5.2a）：名字 + 可选 overlay 字段。 */
/** Persona record (spec 02 §5.2a): name + optional overlay fields. */
export interface OmpPersona {
  /** persona 名（唯一键）。 */
  name: string;
  /** 描述。 */
  description?: string;
  /** 物化时叠加到 systemPrompt 的提示词。 */
  systemPrompt?: string;
  /** 工具白名单 overlay。 */
  tools?: string[];
}

/** personas 侧边栏存储（jsonFileStore / mapBackedStore 的共同接口）。 */
/** Personas sidecar store (the jsonFileStore / mapBackedStore product). */
export interface PersonaStore {
  /** 同步读全量记录。 */
  load(): OmpPersona[];
  /** 全量覆盖写。 */
  save(records: OmpPersona[]): void;
}

/**
 * JSON 侧边栏文件存储（personas 存储用）。形状 `{ [key]: records[] }`；
 * 文件缺失 / 损坏 / 字段不成形时降级为 []。save 先建目录再整文件覆写。
 */
/**
 * JSON sidecar file store (used by the personas store). Shape:
 * `{ [key]: records[] }`; missing/corrupt file → [].
 */
export function jsonFileStore(filePath: string, key = 'agents'): PersonaStore {
  return {
    load() {
      try {
        const parsed = JSON.parse(fs.readFileSync(filePath, 'utf8'));
        const records = parsed?.[key];
        return Array.isArray(records) ? records.filter((r) => r && typeof r.name === 'string') : [];
      } catch {
        return [];
      }
    },
    save(records) {
      fs.mkdirSync(path.dirname(filePath), { recursive: true });
      fs.writeFileSync(filePath, JSON.stringify({ [key]: records }, null, 2));
    },
  };
}

/** engine 活跃 personas Map 的适配器（coordinator 接线）；save 清空重填并可经 persist 落盘。 */
/** Adapter over the engine's live personas Map (coordinator wiring). */
export function mapBackedStore(map: Map<string, OmpPersona>, persist?: (records: OmpPersona[]) => void): PersonaStore {
  return {
    load() {
      return [...map.values()];
    },
    save(records) {
      map.clear();
      for (const record of records) map.set(record.name, record);
      persist?.(records);
    },
  };
}

// ---------------------------------------------------------------------------
// 2. Agent definitions (02 §5.2 — omp discovery chain + .md file storage)
// ---------------------------------------------------------------------------

/**
 * 解析写请求体的作用域：外层 body.scope 与内层 body.definition.scope 都
 * 可指定；两者都有且不一致时抛 400 scope-mismatch，否则取先出现者。
 */
const definitionScope = (body: AgentDefinitionWriteBody) => {
  const outer = typeof body?.scope === 'string' ? body.scope : undefined;
  const inner = typeof body?.definition?.scope === 'string' ? body.definition.scope : undefined;
  if (outer !== undefined && inner !== undefined && outer !== inner) {
    throw new ModeDomainError(400, { error: 'scope-mismatch', message: 'scope differs between body and definition' });
  }
  return outer ?? inner;
};

/**
 * 校验名字（label 缺省 'name'）：必须匹配 AGENT_NAME_PATTERN，否则抛
 * 400 invalid-name；通过时返回 trim 后的值。
 */
const validateName = (name: AgentDefinitionInput['name'], { label = 'name' }: { label?: string } = {}): string => {
  if (typeof name !== 'string' || !AGENT_NAME_PATTERN.test(name.trim())) {
    throw new ModeDomainError(400, { error: 'invalid-name', message: `${label} must match ${AGENT_NAME_PATTERN}` });
  }
  return name.trim();
};

/**
 * 校验工具清单：undefined / null → []（未限制）；必须为字符串数组且
 * 都在 allowedTools 白名单内，否则抛 400 invalid-tools / unknown-tools；
 * 返回去重后的数组。
 */
const validateTools = (tools: AgentDefinitionInput['tools'], allowedTools: Set<string>): string[] => {
  if (tools === undefined || tools === null) return [];
  if (!Array.isArray(tools) || tools.some((tool) => typeof tool !== 'string')) {
    throw new ModeDomainError(400, { error: 'invalid-tools', message: 'tools must be an array of strings' });
  }
  const unknown = [...new Set(tools)].filter((tool) => !allowedTools.has(tool));
  if (unknown.length > 0) {
    throw new ModeDomainError(400, { error: 'unknown-tools', tools: unknown });
  }
  return [...new Set(tools)];
};

/** 容错解析 model / spawns 字段：字符串按逗号切分并去空白，数组原样返回。 */
const parseCsvList = (value: AgentDefinitionInput['model'] | AgentDefinitionInput['spawns']) => (
  typeof value === 'string'
    ? value.split(',').map((entry) => entry.trim()).filter(Boolean)
    : value
);

/**
 * 校验 prewalk / advisor 类字段：boolean 或非空模型模式串；null 表示
 * 清除该字段（规范 02 §5.2）；其它类型抛 400 invalid-{field}。
 */
/** boolean | non-empty model pattern; `null` clears the field (02 §5.2). */
const validateFlagOrPattern = (field: string, value: AgentDefinitionInput['prewalk']) => {
  if (value === null) return null;
  if (value === true || value === false) return value;
  if (typeof value === 'string' && value.trim()) return value.trim();
  throw new ModeDomainError(400, {
    error: `invalid-${field}`,
    message: `${field} must be a boolean or a model pattern string`,
  });
};

/**
 * 未校验的 /omp/agent-definitions 线上输入（规范 02 §5.2/§5.3）：每个
 * 字段都在下方运行时校验；可选字段接受 null 表示清除。
 */
/**
 * Unvalidated /omp/agent-definitions wire input (02 §5.2/§5.3): every field
 * is runtime-checked below; optional fields accept `null` to clear.
 */
export interface AgentDefinitionInput {
  /** agent 名。 */
  name?: unknown;
  /** 描述（frontmatter 必填）。 */
  description?: unknown;
  /** 系统提示词。 */
  systemPrompt?: unknown;
  /** 模型模式串或逗号分隔串。 */
  model?: unknown;
  /** thinkingLevel 选择器。 */
  thinkingLevel?: unknown;
  /** 工具白名单。 */
  tools?: unknown;
  /** 可派生 agent：'*'、数组或逗号分隔串。 */
  spawns?: unknown;
  /** prewalk：boolean 或模型模式串。 */
  prewalk?: unknown;
  /** advisor：boolean 或模型模式串。 */
  advisor?: unknown;
  /** readSummarize 开关。 */
  readSummarize?: unknown;
  /** 作用域：'user' | 'project'。 */
  scope?: unknown;
}

/** 校验后的定义 patch——null 表示清除对应可选字段（见下方 AgentDefinitionPatch）。 */
/** Validated definition patch; `null` clears an optional field. */
/** patch 与序列化共用的七个任务覆盖键。 */
/** The seven task-override keys shared by patch and serialization. */
const AGENT_OVERRIDE_KEYS = ['model', 'thinkingLevel', 'tools', 'spawns', 'prewalk', 'advisor', 'readSummarize'] as const;
/** AGENT_OVERRIDE_KEYS 的字面量联合类型。 */
type AgentOverrideKey = (typeof AGENT_OVERRIDE_KEYS)[number];

/** 校验后的定义 patch；可选字段为 null 表示清除。 */
export interface AgentDefinitionPatch {
  /** 描述。 */
  description?: string;
  /** 系统提示词。 */
  systemPrompt?: string;
  /** 模型模式串；null 清除。 */
  model?: string[] | null;
  /** thinkingLevel；null 清除。 */
  thinkingLevel?: string | null;
  /** 工具白名单；null 清除。 */
  tools?: string[] | null;
  /** 可派生 agent（'*' 或名字数组）；null 清除。 */
  spawns?: '*' | string[] | null;
  /** prewalk；null 清除。 */
  prewalk?: boolean | string | null;
  /** advisor；null 清除。 */
  advisor?: boolean | string | null;
  /** readSummarize；null 清除。 */
  readSummarize?: boolean | null;
}

/**
 * POST/PUT /omp/agent-definitions 请求体：`{ scope?, renameTo?,
 * definition? }` 包装或裸定义 patch——两种形状都做运行时校验。
 */
/** POST/PUT /omp/agent-definitions body: a `{ scope?, renameTo?, definition? }`
 *  wrapper or a bare definition patch — both shapes runtime-validated. */
export interface AgentDefinitionWriteBody extends AgentDefinitionInput {
  /** 重命名目标名（仅 PUT）。 */
  renameTo?: unknown;
  /** 包装形态的内层定义 patch。 */
  definition?: AgentDefinitionInput;
}

/**
 * 校验定义 patch（规范 02 §5.2/§5.3——omp AgentDefinition frontmatter
 * 契约；OpenCode 的 mode / permission / temperature 字段没有 omp 对应物，
 * 不接受）。可选字段接受 null 表示清除；非法值抛带上下文的 400。
 */
/**
 * Validate a definition patch (02 §5.2/§5.3 — the omp AgentDefinition
 * frontmatter contract; the OpenCode mode/permission/temperature fields
 * have no omp counterpart and are not accepted). Optional fields accept
 * `null` to clear.
 */
const validateDefinitionPatch = (patch: AgentDefinitionInput, allowedTools: Set<string>): AgentDefinitionPatch => {
  const out: AgentDefinitionPatch = {};
  if (patch.description !== undefined) {
    if (typeof patch.description !== 'string' || !patch.description.trim()) {
      throw new ModeDomainError(400, {
        error: 'invalid-description',
        message: 'description must be a non-empty string (required by the omp agent frontmatter)',
      });
    }
    out.description = patch.description;
  }
  if (patch.systemPrompt !== undefined) {
    if (typeof patch.systemPrompt !== 'string' || !patch.systemPrompt.trim()) {
      throw new ModeDomainError(400, { error: 'invalid-prompt', message: 'systemPrompt must be a non-empty string' });
    }
    out.systemPrompt = patch.systemPrompt;
  }
  if (patch.model !== undefined) {
    if (patch.model === null) {
      out.model = null;
    } else {
      const model = parseCsvList(patch.model);
      if (!Array.isArray(model) || model.length === 0
        || model.some((pattern) => typeof pattern !== 'string' || !pattern.trim())) {
        throw new ModeDomainError(400, {
          error: 'invalid-model',
          message: 'model must be an array of model patterns, e.g. ["@smol", "anthropic/*:high"]',
        });
      }
      out.model = model.map((pattern) => pattern.trim());
    }
  }
  if (patch.thinkingLevel !== undefined) {
    if (patch.thinkingLevel === null) {
      out.thinkingLevel = null;
    } else if (typeof patch.thinkingLevel !== 'string' || !THINKING_LEVELS.has(patch.thinkingLevel)) {
      throw new ModeDomainError(400, {
        error: 'invalid-thinking-level',
        thinkingLevel: patch.thinkingLevel,
        message: `thinkingLevel must be one of ${[...THINKING_LEVELS].join(', ')}`,
      });
    } else {
      out.thinkingLevel = patch.thinkingLevel;
    }
  }
  if (patch.tools !== undefined) {
    out.tools = patch.tools === null ? null : validateTools(patch.tools, allowedTools);
  }
  if (patch.spawns !== undefined) {
    if (patch.spawns === null) {
      out.spawns = null;
    } else {
      const spawns = patch.spawns === '*' ? '*' : parseCsvList(patch.spawns);
      if (spawns !== '*' && (!Array.isArray(spawns) || spawns.length === 0
        || spawns.some((name) => typeof name !== 'string' || !name.trim()))) {
        throw new ModeDomainError(400, {
          error: 'invalid-spawns',
          message: 'spawns must be "*" or an array of agent names',
        });
      }
      out.spawns = spawns === '*' ? '*' : spawns.map((name) => name.trim());
    }
  }
  if (patch.prewalk !== undefined) out.prewalk = validateFlagOrPattern('prewalk', patch.prewalk);
  if (patch.advisor !== undefined) out.advisor = validateFlagOrPattern('advisor', patch.advisor);
  if (patch.readSummarize !== undefined) {
    if (patch.readSummarize === null) {
      out.readSummarize = null;
    } else if (typeof patch.readSummarize !== 'boolean') {
      throw new ModeDomainError(400, { error: 'invalid-read-summarize', message: 'readSummarize must be a boolean' });
    } else {
      out.readSummarize = patch.readSummarize;
    }
  }
  return out;
};

/**
 * 作用域门控：缺省 'user'；非 user / project 抛 400 invalid-scope；
 * 'project' 需 settings.projectScopes.v1 开启，否则抛 409
 * project-scope-unavailable。
 */
const gateProjectScope = (scope: string | undefined, settingsProjectScopes?: boolean): 'user' | 'project' => {
  if (scope === undefined || scope === null) return 'user';
  if (scope !== 'user' && scope !== 'project') {
    throw new ModeDomainError(400, { error: 'invalid-scope', scope, message: 'scope must be "user" or "project"' });
  }
  if (scope !== 'project') return scope;
  if (settingsProjectScopes) return 'project';
  throw new ModeDomainError(409, {
    error: 'project-scope-unavailable',
    message: 'Project-scoped agent definitions require settings.projectScopes.v1; use scope "user".',
  });
};

/**
 * 单目录会话生效的 task.* 覆盖值（keyed Settings 合并视图——规范
 * 02 §5.2 读投影）。由 engine 注入；null / 抛错时降级为无覆盖定义。
 */
/**
 * Effective task.* override values for one directory's sessions (the keyed
 * Settings merged view — 02 §5.2 read projection). Injected by the engine;
 * null/throw degrades to override-free definitions.
 */
export interface TaskOverrideValues {
  /** 禁用的 agent 名单。 */
  disabledAgents?: unknown;
  /** 每 agent 的模型覆盖（名字 → 模式串）。 */
  modelOverrides?: unknown;
  /** 每 agent 的 prewalk 覆盖。 */
  prewalk?: unknown;
  /** 每 agent 的 advisor 覆盖。 */
  advisor?: unknown;
}

/** 按目录读取生效 task.* 覆盖值的函数类型（engine 注入）。 */
/** Effective task.* overrides read for one directory (engine-injected). */
export type OverridesFor = (directory: string | null) => Promise<TaskOverrideValues | null>;

/**
 * 从 overrides 对象里安全取出指定 agent 的字符串字段：非普通对象 /
 * 数组 / 字段非字符串一律返回 undefined（YAML 发现产物的运行时收窄）。
 */
const recordEntryFor = (
  name: string,
  value: TaskOverrideValues['modelOverrides'] | TaskOverrideValues['prewalk'] | TaskOverrideValues['advisor'],
): string | undefined => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
  // SAFETY: plain-object overrides hold string fields (YAML discovery output);
  // only a confirmed string field is returned.
  const fields = value as Record<string, string>;
  return typeof fields[name] === 'string' ? fields[name] : undefined;
}

/**
 * 把 settings 级每 agent 覆盖联查（join）到定义记录上（规范 02 §5.2）：
 * disabled 标记 + modelOverride / prewalkOverride / advisorOverride 字段。
 * overridesFor 缺失、返回空或抛错时原样返回记录（读路径不因此失败）。
 */
/** Join the settings-level per-agent overrides onto definition records (02 §5.2). */
const withTaskOverrides = async (records: OmpAgentRecord[], overridesFor: OverridesFor | undefined, directory: string | null): Promise<OmpAgentRecord[]> => {
  if (typeof overridesFor !== 'function') return records;
  let values;
  try {
    values = await overridesFor(directory);
  } catch {
    return records;
  }
  if (!values) return records;
  const disabled = Array.isArray(values.disabledAgents)
    ? new Set(values.disabledAgents.filter((name) => typeof name === 'string'))
    : new Set();
  return records.map((record) => {
    const modelOverride = recordEntryFor(record.name, values.modelOverrides);
    const prewalkOverride = recordEntryFor(record.name, values.prewalk);
    const advisorOverride = recordEntryFor(record.name, values.advisor);
    return {
      ...record,
      disabled: disabled.has(record.name),
      ...(modelOverride !== undefined ? { modelOverride } : {}),
      ...(prewalkOverride !== undefined ? { prewalkOverride } : {}),
      ...(advisorOverride !== undefined ? { advisorOverride } : {}),
    };
  });
};

/** 发现的 omp agent（此处消费的 SDK discoverAgents 记录的结构子集）。 */
/** Discovered omp agent (structural subset of the SDK `discoverAgents` record consumed here). */
export interface DiscoveredAgent {
  /** agent 名。 */
  name: string;
  /** 描述（frontmatter 必填项）。 */
  description?: string;
  /** 来源：project | user | extension | plugin | bundled。 */
  source: string;
  /** 定义 .md 文件路径（bundled 无）。 */
  filePath?: string;
  /** 系统提示词（.md 正文）。 */
  systemPrompt?: string;
  /** 模型模式串数组。 */
  model?: string[];
  /** thinkingLevel 选择器。 */
  thinkingLevel?: string;
  /** 工具白名单。 */
  tools?: string[];
  /** 可派生的 agent 名或 '*'。 */
  spawns?: '*' | string[];
  /** prewalk 开关或模型模式串。 */
  prewalk?: boolean | string;
  /** advisor 开关或模型模式串。 */
  advisor?: boolean | string;
  /** readSummarize 开关。 */
  readSummarize?: boolean;
}

/** discoverAgents(directory) 的结果形状（handlers 消费的部分）。 */
/** `discoverAgents(directory)` result shape consumed by the handlers. */
export interface AgentDiscoveryResult {
  /** 发现的 agent 列表。 */
  agents: DiscoveredAgent[];
  /** 项目 agents 目录（无则 null）。 */
  projectAgentsDir?: string | null;
}

/** omp agent 记录——/omp/agent-definitions 的读投影（规范 02 §5.2）。 */
/** omp agent record — the /omp/agent-definitions read projection (02 §5.2). */
export interface OmpAgentRecord {
  /** agent 名。 */
  name: string;
  /** 描述（缺失时空串）。 */
  description: string;
  /** 来源作用域。 */
  source: string;
  /** 系统提示词（缺失时空串）。 */
  systemPrompt: string;
  /** 定义文件路径（有文件时）。 */
  filePath?: string;
  /** 模型模式串。 */
  model?: string[];
  /** thinkingLevel。 */
  thinkingLevel?: string;
  /** 工具白名单。 */
  tools?: string[];
  /** 可派生 agent：'*' 或名字数组。 */
  spawns?: '*' | string[];
  /** prewalk 开关或模式串。 */
  prewalk?: boolean | string;
  /** advisor 开关或模式串。 */
  advisor?: boolean | string;
  /** readSummarize 开关。 */
  readSummarize?: boolean;
  /** settings 联查字段（withTaskOverrides）。 */
  /** Settings-joined fields (withTaskOverrides). */
  /** 被禁用标记。 */
  disabled?: boolean;
  /** 模型覆盖值。 */
  modelOverride?: string;
  /** prewalk 覆盖值。 */
  prewalkOverride?: string;
  /** advisor 覆盖值。 */
  advisorOverride?: string;
}

/**
 * AgentDefinition（SDK task/types.ts:359-378）→ OmpAgent 记录（规范
 * 02 §5.2）：仅搬运已验证字段，空值不落键。
 */
/** AgentDefinition (SDK task/types.ts:359-378) → OmpAgent record (02 §5.2). */
const definitionToRecord = (agent: DiscoveredAgent): OmpAgentRecord => ({
  name: agent.name,
  description: typeof agent.description === 'string' ? agent.description : '',
  source: agent.source,
  ...(agent.filePath ? { filePath: agent.filePath } : {}),
  systemPrompt: typeof agent.systemPrompt === 'string' ? agent.systemPrompt : '',
  ...(Array.isArray(agent.model) && agent.model.length > 0 ? { model: agent.model } : {}),
  ...(agent.thinkingLevel !== undefined && agent.thinkingLevel !== null
    ? { thinkingLevel: String(agent.thinkingLevel) }
    : {}),
  ...(Array.isArray(agent.tools) && agent.tools.length > 0 ? { tools: agent.tools } : {}),
  ...(agent.spawns !== undefined && agent.spawns !== null ? { spawns: agent.spawns } : {}),
  ...(agent.prewalk !== undefined && agent.prewalk !== null ? { prewalk: agent.prewalk } : {}),
  ...(agent.advisor !== undefined && agent.advisor !== null ? { advisor: agent.advisor } : {}),
  ...(agent.readSummarize !== undefined && agent.readSummarize !== null
    ? { readSummarize: agent.readSummarize }
    : {}),
});

/**
 * serializeAgentMarkdown 的定义输入：name + description 是 SDK
 * frontmatter 解析器的必填项，其余走 omp frontmatter 字段。
 * rawFrontmatter 是更新路径的无损载体——被改写文件的完整解析
 * frontmatter：未知键（含 SDK 的 autoloadSkills / blocking / output）
 * 原样保留，白名单键跟随合并值（undefined 表示清除该键）。
 */
/** Definition input to serializeAgentMarkdown: name + description are required
 * by the SDK frontmatter parser; the rest ride the omp frontmatter fields.
 * `rawFrontmatter` is the update path's lossless carrier: the full parsed
 * frontmatter record of the file being rewritten — unknown keys (incl. the
 * SDK's autoloadSkills/blocking/output) survive verbatim, while whitelist
 * keys follow the merged values (undefined clears the key). The SDK has no
 * round-trip serializer and its read parser mutates on load (yield
 * injection, spawns inference), so raw-record preservation is the only
 * lossless write (plan P9). */
export interface AgentDefinitionSerialization {
  /** agent 名（必填）。 */
  name: string;
  /** 描述（必填）。 */
  description: string;
  /** 系统提示词（.md 正文）。 */
  systemPrompt?: string;
  /** 模型模式串。 */
  model?: string[];
  /** thinkingLevel。 */
  thinkingLevel?: string;
  /** 工具白名单。 */
  tools?: string[];
  /** 可派生 agent。 */
  spawns?: '*' | string[];
  /** prewalk 开关或模式串。 */
  prewalk?: boolean | string;
  /** advisor 开关或模式串。 */
  advisor?: boolean | string;
  /** readSummarize 开关。 */
  readSummarize?: boolean;
  /** 被改写文件的原始 frontmatter（更新路径）。 */
  rawFrontmatter?: AgentFrontmatterRecord;
}

/** 白名单合并拥有的 frontmatter 键；其余键走原样透传。 */
/** Frontmatter keys the whitelist merge owns; raw passthrough covers the rest. */
const SERIALIZED_MERGED_KEYS = ['tools', 'model', 'thinkingLevel', 'spawns', 'prewalk', 'advisor', 'readSummarize'] as const;

/** 解析出的 agent frontmatter 值：用户手写 YAML，形状任意。 */
/** Parsed agent-frontmatter value: user-authored YAML, arbitrary shapes. */
export type AgentFrontmatterValue =
  | string
  | number
  | boolean
  | null
  | AgentFrontmatterValue[]
  | { [key: string]: AgentFrontmatterValue };

/** agent .md 文件解析出的整个 frontmatter 记录。 */
/** The whole parsed frontmatter record of an agent .md file. */
export type AgentFrontmatterRecord = { [key: string]: AgentFrontmatterValue };
/**
 * 把定义序列化为 omp agent markdown：YAML frontmatter（name +
 * description 为 SDK 解析器必填，discovery/helpers.ts:256-260
 * parseAgentFields）+ 正文即 systemPrompt。产物会经 discoverAgents
 * 往返（first-wins 去重，discovery.ts）——返回给客户端的权威是重新
 * 发现的记录，不是本字符串。
 */
/**
 * Serialize a definition to the omp agent markdown shape: YAML frontmatter
 * (name + description are required by the SDK parser,
 * discovery/helpers.ts:256-260 parseAgentFields) with the body as the
 * systemPrompt. Round-trips through `discoverAgents` (first-wins dedup,
 * discovery.ts) — the re-discovered record, not this string, is the
 * authority returned to clients.
 */
export function serializeAgentMarkdown(definition: AgentDefinitionSerialization): string {
  // Raw path (update): start from the file's verbatim frontmatter record so
  // unknown keys survive; whitelist keys follow the merged values, where
  // undefined/null means the patch cleared the key.
  const frontmatter: AgentFrontmatterRecord = definition.rawFrontmatter
    ? { ...definition.rawFrontmatter }
    : { name: definition.name, description: definition.description };
  for (const key of SERIALIZED_MERGED_KEYS) {
    const value = definition[key];
    if (!definition.rawFrontmatter) {
      if (value === undefined || value === null) continue;
      if (key === 'tools' || key === 'model') {
        if (Array.isArray(value) && value.length > 0) frontmatter[key] = value;
        continue;
      }
      frontmatter[key] = value;
      continue;
    }
    if (value === undefined || value === null) delete frontmatter[key];
    else frontmatter[key] = value;
  }
  frontmatter.name = definition.name;
  frontmatter.description = definition.description;
  const body = typeof definition.systemPrompt === 'string' ? definition.systemPrompt : '';
  return `---\n${stringifyYaml(frontmatter)}---\n\n${body}\n`;
}


/**
 * engine 注入的文件/发现适配器（规范 02 §5.2）：读走 omp 发现链，写落
 * .md 文件，变更后热更新。
 */
/**
 * File/discovery adapters injected by the engine (02 §5.2): reads come from
 * the omp discovery chain, writes land as .md files, mutations hot-reload.
 */
export interface AgentDefinitionAdapter {
  /** 按目录执行 agent 发现。 */
  discover: (directory: string | null) => Promise<AgentDiscoveryResult>;
  /** 写定义文件。 */
  writeFile: (filePath: string, content: string) => Promise<void>;
  /** 删除定义文件；返回是否删除。 */
  deleteFile: (filePath: string) => Promise<boolean>;
  /** 无损更新读取器（P9）：取回既有定义文件使原始 frontmatter 在 GUI 编辑后存活；缺省走仅白名单写。 */
  /** Lossless-update reader (P9): fetches an existing definition file so its
   * raw frontmatter survives a GUI edit. Optional — without it updates keep
   * the whitelist-only write. */
  readFile?: (filePath: string) => Promise<string>;
  /** 热更新钩子（engine 进程内的 refreshAgentDiscovery）。 */
  /** Hot-reload hook (refreshAgentDiscovery in the engine process). */
  onDefinitionsChanged?: (directory: string | null) => void | Promise<void>;
  /** 在系统文件管理器中揭示定义文件。 */
  /** Reveal the definition file in the platform file manager. */
  revealFile?: (filePath: string) => Promise<void>;
  /** 用户 agents 目录（~/.omp/agent/agents）。 */
  userAgentsDir: string;
  /** 由会话目录解析项目 agents 目录。 */
  projectAgentsDirFor: (directory: string) => string;
}

/** createAgentDefinitionHandlers 的选项——适配器 + 表格开关。 */
/** createAgentDefinitionHandlers options — the adapter plus table knobs. */
export interface AgentDefinitionHandlersOptions {
  /** 按目录发现（必需）。 */
  discover?: (directory: string | null) => Promise<AgentDiscoveryResult>;
  /** 写文件（必需）。 */
  writeFile?: (filePath: string, content: string) => Promise<void>;
  /** 删文件（必需）。 */
  deleteFile?: (filePath: string) => Promise<boolean>;
  /** 读文件（可选，无损更新）。 */
  readFile?: (filePath: string) => Promise<string>;
  /** 用户 agents 目录（必需）。 */
  userAgentsDir?: string;
  /** 项目 agents 目录解析器（必需）。 */
  projectAgentsDirFor?: (directory: string) => string;
  /** 热更新钩子。 */
  onDefinitionsChanged?: (directory: string | null) => void | Promise<void>;
  /** 揭示文件钩子。 */
  revealFile?: (filePath: string) => Promise<void>;
  /** 工具白名单。 */
  allowedTools?: Set<string> | Iterable<string>;
  /** project 作用域开关。 */
  settingsProjectScopes?: boolean;
  /** settings 覆盖读取器。 */
  overridesFor?: OverridesFor;
}

/** /omp/agent-definitions 的 CRUD handlers（路由 handler 形状）。 */
/** /omp/agent-definitions CRUD handlers (route-handler shaped). */
export interface AgentDefinitionHandlers {
  /** 列表。 */
  list: ModesRouteHandler;
  /** 单条。 */
  get: ModesRouteHandler;
  /** 创建。 */
  create: ModesRouteHandler;
  /** 更新（含改名 / 迁作用域）。 */
  update: ModesRouteHandler;
  /** 删除。 */
  remove: ModesRouteHandler;
  /** 手动刷新发现。 */
  refresh: ModesRouteHandler;
  /** 文件管理器揭示。 */
  reveal: ModesRouteHandler;
}

/**
 * 创建 /omp/agent-definitions 的 CRUD handlers（规范 02 §5.2，GAP-B03/B04）：
 * 读权威是 omp agent 发现链，写落用户/项目 agents 目录的 .md 文件；
 * bundled 与扩展定义只读，覆盖走同名遮蔽。
 */
/**
 * CRUD handlers for /omp/agent-definitions (02 §5.2, GAP-B03/B04): the omp
 * agent discovery chain (project `.omp/agents` > user `~/.omp/agent/agents`
 * > extension packages > bundled — SDK task/discovery.ts `discoverAgents`)
 * is the read authority; writes land as agent markdown files in the user or
 * project agents dir. Bundled and extension/plugin-owned definitions are
 * read-only — overriding rides omp's first-wins shadowing (a same-name user
 * definition), never mutation.
 *
 * @param {AgentDefinitionHandlersOptions} options discovery + file adapters,
 *        plus the tool allowlist / project-scope gate / settings overrides.
 */
/**
 * 解析 agent .md 文件开头的 YAML frontmatter 记录（更新路径，plan P9）。
 * 无读取器、无文件路径、无 frontmatter 块或 YAML 解析失败时返回
 * null——调用方降级为仅白名单写。
 */
/**
 * Parse the leading YAML frontmatter record of an agent .md file (update
 * path, plan P9). Returns null when there is no reader, no file path, no
 * frontmatter block, or the YAML fails to parse — callers degrade to the
 * whitelist-only write.
 */
const readExistingFrontmatter = async (
  filePath: string | undefined,
  readFile: ((filePath: string) => Promise<string>) | undefined,
): Promise<AgentFrontmatterRecord | null> => {
  // A missing reader or path degrades to the whitelist-only write.
  if (!filePath || !readFile) return null;
  try {
    const content = await readFile(filePath);
    const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/);
    if (!match) return null;
    const parsed = parseYaml(match[1]);
    // SAFETY: parseYaml returns unknown; the guards above establish a plain
    // non-array object, which is exactly AgentFrontmatterRecord's shape.
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? { ...(parsed as AgentFrontmatterRecord) }
      : null;
  } catch {
    return null;
  }
};

/**
 * 创建 /omp/agent-definitions 的 CRUD handlers（规范 02 §5.2，GAP-B03/B04）。
 * 读权威是 omp agent 发现链，写入落为用户/项目 agents 目录的 agent
 * markdown 文件；bundled 与扩展定义只读，覆盖走同名遮蔽。缺必要适配器
 * 抛 TypeError（完整契约见上方英文块）。
 */
export function createAgentDefinitionHandlers({
  discover,
  writeFile,
  deleteFile,
  readFile,
  userAgentsDir,
  projectAgentsDirFor,
  allowedTools,
  settingsProjectScopes = false,
  overridesFor,
  onDefinitionsChanged,
  revealFile,
}: AgentDefinitionHandlersOptions = {}): AgentDefinitionHandlers {
  if (typeof discover !== 'function' || typeof writeFile !== 'function' || typeof deleteFile !== 'function'
    || typeof userAgentsDir !== 'string' || typeof projectAgentsDirFor !== 'function') {
    throw new TypeError('agent-definitions handlers require discovery + file adapters');
  }
  // 工具白名单归一化：Set 直接用，Iterable 转 Set，缺省取 BUILTIN_TOOLS 的 key。
  const allow = allowedTools instanceof Set ? allowedTools : new Set(allowedTools ?? Object.keys(BUILTIN_TOOLS ?? {}));

  // 定义热更新钩子（02 §5.2 refresh）：刷新引擎内的发现缓存，失败只告警不影响变更结果。
  /**
   * Hot-reload hook (02 §5.2 refresh): the SDK memoizes the create-time
   * discovery per cwd (task/index.ts discoveryMemo) and every task tool
   * advertises that list to the model. After a definition file changes,
   * refreshAgentDiscovery must run in the engine process or live sessions
   * keep describing the stale agent set. Swallowed failures never fail the
   * mutation — dispatch-time discovery stays fresh regardless.
   */
  const definitionsChanged = async (directory: string | null): Promise<void> => {
    if (typeof onDefinitionsChanged !== 'function') return;
    try {
      await onDefinitionsChanged(directory);
    } catch (error) {
      console.warn('[omp-host] agent discovery refresh failed:', errorText(error));
    }
  };

  // discover 包装：底层失败统一转 503 agent-discovery-failed。
  const discoverSafe = async (directory: string | null): Promise<AgentDiscoveryResult> => {
    try {
      return await discover(directory);
    } catch (error) {
      throw new ModeDomainError(503, {
        error: 'agent-discovery-failed',
        message: errorText(error),
      });
    }
  };
  // 按名在发现结果中定位 agent；找不到返回 null。
  const findAgent = async (directory: string | null, name: string | undefined): Promise<DiscoveredAgent | null> => {
    const { agents } = await discoverSafe(directory);
    return agents.find((agent) => agent?.name === name) ?? null;
  };
  // 写后读回：定义已写入却未通过发现链解析回来时抛 500 definition-not-parsed。
  const recordFor = async (directory: string | null, name: string | undefined): Promise<OmpAgentRecord> => {
    const agent = await findAgent(directory, name);
    if (!agent) {
      throw new ModeDomainError(500, {
        error: 'definition-not-parsed',
        name,
        message: 'the definition was written but did not parse back through discovery',
      });
    }
    return definitionToRecord(agent);
  };
  // 判断文件是否落在本域管理的用户/项目 agents 目录内（可写 / 可揭示）。
  const isManaged = (agent: DiscoveredAgent | null, directory: string | null): boolean => {
    if (!agent?.filePath) return false;
    const resolved = path.resolve(agent.filePath);
    return resolved.startsWith(path.resolve(userAgentsDir) + path.sep)
      || resolved.startsWith(path.resolve(projectAgentsDirFor(directory ?? process.cwd())) + path.sep);
  };
  // 可写断言：bundled 与扩展/插件目录的定义只读，抛 409。
  const assertWritable = (agent: DiscoveredAgent, directory: string | null): void => {
    if (agent.source === 'bundled') {
      throw new ModeDomainError(409, {
        error: 'bundled-read-only',
        name: agent.name,
        message: 'Bundled agents are read-only. Create a definition with the same name in the user or project scope to shadow it.',
      });
    }
    if (!isManaged(agent, directory)) {
      throw new ModeDomainError(409, {
        error: 'definition-not-managed',
        name: agent.name,
        message: 'This definition is owned by an extension or plugin directory. Edit it at its source.',
      });
    }
  };
  // 作用域 → agents 目录路径（user 固定；project 按会话目录解析）。
  const scopeDir = (scope: 'user' | 'project', directory: string | null): string =>
    (scope === 'project' ? projectAgentsDirFor(directory ?? process.cwd()) : userAgentsDir);
  // 现有定义 + patch 合并：七个覆盖键 presence 优先，null/undefined 不落键。
  const mergeDefinition = (existing: Partial<AgentDefinitionSerialization> & { name: string }, patch: AgentDefinitionPatch): AgentDefinitionSerialization => {
    const merged: AgentDefinitionSerialization = {
      name: existing.name,
      description: patch.description ?? existing.description ?? '',
      systemPrompt: patch.systemPrompt !== undefined ? patch.systemPrompt : existing.systemPrompt,
    };
    // SAFETY: the merge walks the seven declared override keys shared by
    // AgentDefinitionPatch and the serialized record; presence wins and the
    // key-union write needs the never-widened assignment TS demands.
    for (const key of AGENT_OVERRIDE_KEYS) {
      const value = patch[key] !== undefined ? patch[key] : existing[key];
      if (value !== null && value !== undefined) {
        // SAFETY: key-union write; value comes from the same field of
        // patch/existing, so the target field type already admits it.
        merged[key] = value as never;
      }
    }
    return merged;
  };

  return {
    // GET /omp/agent-definitions：全量记录 + settings 覆盖联查 + 项目 agents 目录。
    async list(request, ctx) {
      const directory = directoryParam(ctx);
      const { agents, projectAgentsDir } = await discoverSafe(directory);
      return json({
        agents: await withTaskOverrides(agents.map(definitionToRecord), overridesFor, directory),
        projectAgentsDir: projectAgentsDir ?? null,
      });
    },

    // GET /omp/agent-definitions/{name}：单条（含覆盖联查）；未找到 404。
    async get(request, ctx) {
      const directory = directoryParam(ctx);
      const agent = await findAgent(directory, ctx?.params?.name);
      if (!agent) return json({ error: 'not-found' }, { status: 404 });
      const [joined] = await withTaskOverrides([definitionToRecord(agent)], overridesFor, directory);
      return json(joined);
    },

    // POST：校验名/作用域/patch（systemPrompt、description 必填）→ 写 .md → 读回 201。
    async create(request, ctx) {
      const directory = directoryParam(ctx);
      const body = await readJsonBody<AgentDefinitionWriteBody>(request);
      const definition = body?.definition ?? body;
      const name = validateName(definition?.name);
      if (await findAgent(directory, name)) {
        throw new ModeDomainError(409, { error: 'agent-definition-exists', name });
      }
      const scope = gateProjectScope(definitionScope(body), settingsProjectScopes);
      const patch = validateDefinitionPatch(definition, allow);
      if (patch.systemPrompt === undefined) {
        throw new ModeDomainError(400, { error: 'invalid-prompt', message: 'systemPrompt is required' });
      }
      if (patch.description === undefined) {
        throw new ModeDomainError(400, { error: 'invalid-description', message: 'description is required' });
      }
      await writeFile(
        path.join(scopeDir(scope, directory), `${name}.md`),
        serializeAgentMarkdown(mergeDefinition({ name }, patch)),
      );
      const [joined] = await withTaskOverrides([await recordFor(directory, name)], overridesFor, directory);
      await definitionsChanged(directory);
      return json(joined, { status: 201 });
    },

    // PUT：可写断言 + 重命名/作用域迁移 + P9 无损 frontmatter 合并写回；换路径时删旧文件。
    async update(request, ctx) {
      const directory = directoryParam(ctx);
      const name = ctx?.params?.name;
      const existing = await findAgent(directory, name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      assertWritable(existing, directory);
      const body = await readJsonBody<AgentDefinitionWriteBody>(request);
      const renameTo = body?.renameTo !== undefined ? validateName(body.renameTo, { label: 'renameTo' }) : undefined;
      if (renameTo !== undefined && renameTo !== name && await findAgent(directory, renameTo)) {
        throw new ModeDomainError(409, { error: 'agent-definition-exists', name: renameTo });
      }
      const patch = validateDefinitionPatch(body?.definition ?? {}, allow);
      const currentScope = existing.source === 'project' ? 'project' : 'user';
      const nextScope = gateProjectScope(definitionScope(body) ?? currentScope, settingsProjectScopes);
      const targetName = renameTo ?? name;
      const nextPath = path.join(scopeDir(nextScope, directory), `${targetName}.md`);
      // P9 lossless update: re-serialize from the existing file's verbatim
      // frontmatter so unknown keys survive the GUI edit. A missing reader
      // or unreadable file degrades to the whitelist-only write.
      const rawFrontmatter = await readExistingFrontmatter(existing.filePath, readFile);
      await writeFile(nextPath, serializeAgentMarkdown({
        ...mergeDefinition(existing, patch),
        name: targetName ?? existing.name,
        ...(rawFrontmatter ? { rawFrontmatter } : {}),
      }));
      if (existing.filePath && path.resolve(nextPath) !== path.resolve(existing.filePath)) {
        await deleteFile(existing.filePath);
      }
      const [joined] = await withTaskOverrides(
        [await recordFor(directory, targetName)],
        overridesFor,
        directory,
      );
      await definitionsChanged(directory);
      return json(joined);
    },

    // DELETE：可写断言后删除 .md 文件并触发热更新；204。
    async remove(request, ctx) {
      const directory = directoryParam(ctx);
      const name = ctx?.params?.name;
      const existing = await findAgent(directory, name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      assertWritable(existing, directory);
      if (!existing.filePath) throw new ModeDomainError(400, { error: 'not-file-backed', name });
      await deleteFile(existing.filePath);
      await definitionsChanged(directory);
      return new Response(null, { status: 204 });
    },

    // POST /omp/agent-definitions/refresh（02 §5.2）：文件被带外编辑后的手动刷新。
    /** POST /omp/agent-definitions/refresh (02 §5.2): out-of-band file edits. */
    async refresh(request, ctx) {
      const directory = directoryParam(ctx);
      await definitionsChanged(directory);
      return new Response(null, { status: 204 });
    },

    // POST /omp/agent-definitions/{name}/reveal——在系统文件管理器中打开定义文件所在目录。
    /** POST /omp/agent-definitions/{name}/reveal — open the definition file's folder. */
    async reveal(request, ctx) {
      const directory = directoryParam(ctx);
      const existing = await findAgent(directory, ctx?.params?.name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      if (existing.source === 'bundled') {
        throw new ModeDomainError(409, {
          error: 'bundled-read-only',
          name: existing.name,
          message: 'Bundled agents have no definition file. Create a user or project copy to customize it.',
        });
      }
      if (typeof revealFile !== 'function' || !isManaged(existing, directory)) {
        throw new ModeDomainError(404, {
          error: 'definition-not-managed',
          name: existing.name,
          message: 'This definition has no editable file in the user or project agents directory.',
        });
      }
      try {
        if (!existing.filePath) throw new ModeDomainError(400, { error: 'not-file-backed', name: existing.name });
        await revealFile(path.resolve(existing.filePath));
      } catch (error) {
        console.warn('[omp-host] failed to reveal agent definition:', errorText(error));
        return json({ error: 'reveal-failed' }, { status: 500 });
      }
      return json({ ok: true });
    },
  };
}

/** 旧版 ompchamber-agents.json 记录（规范 02 §6.2 迁移源）。 */
/** Legacy `ompchamber-agents.json` record (02 §6.2 migration source). */
export interface SidecarAgentRecord {
  /** 记录名。 */
  name: string;
  /** 描述。 */
  description?: string;
  /** 系统提示词（迁移为 worker .md 正文）。 */
  prompt?: string;
  /** 工具白名单。 */
  tools?: string[];
}

/**
 * migrateSidecarAgents 的适配器集合（engine 接好 sidecar 文件、omp 发现
 * 链与 personas map）。与 `= {}` 缺省一致全部可选——缺失的适配器按
 * 下方重试契约降级。
 */
/**
 * migrateSidecarAgents adapters (the engine wires the sidecar file, the omp
 * discovery chain and the personas map). All-optional matches the `= {}`
 * default — a missing adapter degrades per the retry contract below.
 */
export interface SidecarMigrationOptions {
  /** 读取 sidecar 记录。 */
  loadRecords: () => SidecarAgentRecord[];
  /** 名字是否已存在于发现链。 */
  agentExists: (name: string) => Promise<boolean>;
  /** 写出一条 worker .md。 */
  writeAgent: (record: SidecarAgentRecord) => Promise<void>;
  /** persona 是否已存在。 */
  personaExists: (name: string) => boolean;
  /** 镜像生成 OmpPersona。 */
  mirrorPersona: (record: SidecarAgentRecord) => void;
  /** 全部成功后的完成标记。 */
  markDone: () => void;
  /** 诊断日志；cause 为失败适配器抛出的错误。 */
  /** Diagnostic logger; `cause` is the caught error from the failed adapter call. */
  log?: (message: string, cause?: unknown) => void;
}

/** sidecar 迁移结果；failed 为中断本次运行的记录名。 */
/** Sidecar migration outcome; `failed` names the record that stopped the run. */
export interface SidecarMigrationResult {
  /** 成功迁移条数。 */
  migrated: number;
  /** 跳过条数（重名 / 无效记录）。 */
  skipped: number;
  /** 中断时的记录名；未中断则缺省。 */
  failed?: string;
}

/**
 * 一次性 sidecar → omp 迁移（规范 02 §6.2）：把每条 ompchamber-agents.json
 * 记录落成用户作用域 worker .md 并镜像一个 OmpPersona，让旧版
 * meta.agent 会话仍可解析（D-B2：worker 文件与顶层 persona 是两个资源）。
 * 发现链已有的名字跳过（first-wins）；任一失败保留 sidecar，下次启动
 * 幂等重试。
 */
/**
 * One-time sidecar → omp migration (02 §6.2): every
 * `ompchamber-agents.json` record becomes a user-scope worker `.md` plus a
 * mirrored `OmpPersona`, so legacy `meta.agent` sessions keep resolving
 * (D-B2: the worker file and the top-level persona are separate resources).
 * A name that already exists in discovery is skipped (first-wins). Any
 * failure keeps the sidecar for an idempotent retry on the next boot.
 *
 * @param {SidecarMigrationOptions} options sidecar + discovery adapters.
 * @returns {Promise<SidecarMigrationResult>}
 */
export async function migrateSidecarAgents({
  loadRecords,
  agentExists,
  writeAgent,
  personaExists,
  mirrorPersona,
  markDone,
  log = () => {},
}: SidecarMigrationOptions): Promise<SidecarMigrationResult> {
  let records: SidecarAgentRecord[] = [];
  try {
    records = loadRecords();
  } catch (error) {
    log('sidecar read failed; leaving migration pending', error);
    return { migrated: 0, skipped: 0 };
  }
  if (!Array.isArray(records)) records = [];
  let migrated = 0;
  let skipped = 0;
  for (const record of records) {
    if (!record || typeof record.name !== 'string' || !record.name.trim()) {
      skipped += 1;
      continue;
    }
    try {
      if (await agentExists(record.name)) {
        skipped += 1;
      } else {
        await writeAgent(record);
      }
      if (!personaExists(record.name)) mirrorPersona(record);
      migrated += 1;
    } catch (error) {
      log(`sidecar migration stopped at "${record.name}"; keeping the sidecar for retry`, error);
      return { migrated, skipped, failed: record.name };
    }
  }
  markDone();
  return { migrated, skipped };
}

// ---------------------------------------------------------------------------
// 3. Personas (02 §5.2a, master D6-R12)
// ---------------------------------------------------------------------------

/** 未校验的 /omp/personas 线上输入（`{ persona? }` 包装或裸对象）。 */
/** Unvalidated /omp/personas wire input (a `{ persona? }` wrapper or bare). */
export interface PersonaInput {
  /** persona 名。 */
  name?: unknown;
  /** 描述。 */
  description?: unknown;
  /** 系统提示词。 */
  systemPrompt?: unknown;
  /** 工具白名单。 */
  tools?: unknown;
}

/** POST/PUT /omp/personas 请求体：`{ persona? }` 包装或裸 patch。 */
/** POST/PUT /omp/personas body: a `{ persona? }` wrapper or a bare patch. */
export interface PersonaWriteBody extends PersonaInput {
  /** 包装形态的内层 persona patch。 */
  persona?: PersonaInput;
}

/** 校验后的 persona patch。 */
/** Validated persona patch. */
export interface PersonaPatch {
  /** 描述。 */
  description?: string;
  /** 系统提示词。 */
  systemPrompt?: string;
  /** 工具白名单（已过 allowedTools 校验）。 */
  tools?: string[];
}

/** createPersonaHandlers 选项。 */
/** createPersonaHandlers options. */
export interface PersonaHandlersOptions {
  /** personas 存储（必需，否则抛 TypeError）。 */
  store?: PersonaStore;
  /** 工具白名单。 */
  allowedTools?: Set<string> | Iterable<string>;
}

/** /omp/personas 的 CRUD handlers（路由 handler 形状）。 */
/** /omp/personas CRUD handlers (route-handler shaped). */
export interface PersonaHandlers {
  /** 列表。 */
  list: ModesRouteHandler;
  /** 单条。 */
  get: ModesRouteHandler;
  /** 创建。 */
  create: ModesRouteHandler;
  /** 更新（含改名）。 */
  update: ModesRouteHandler;
  /** 删除。 */
  remove: ModesRouteHandler;
}

/**
 * 创建 /omp/personas 的 CRUD handlers。OmpPersona 只有
 * name / description / systemPrompt / tools——不含 model / thinkingLevel
 * （顶层模型选择属于 model roles，规范 02 §5.2a）。缺 store 抛 TypeError。
 */
/**
 * CRUD handlers for /omp/personas. `OmpPersona = { name, description?,
 * systemPrompt?, tools? }` — no model/thinkingLevel (top-level model choice
 * belongs to model roles, spec 02 §5.2a).
 */
export function createPersonaHandlers({ store, allowedTools }: PersonaHandlersOptions = {}): PersonaHandlers {
  if (!store?.load || !store?.save) throw new TypeError('persona handlers require a store');
  // 工具白名单归一化（缺省取 BUILTIN_TOOLS）。
  const allow = allowedTools instanceof Set ? allowedTools : new Set(allowedTools ?? Object.keys(BUILTIN_TOOLS ?? {}));

  // 按名查 persona；找不到返回 null。
  const find = (name: string | undefined) => store.load().find((persona) => persona.name === name) ?? null;

  // 逐字段校验 patch：description/systemPrompt 必须是字符串，tools 走白名单校验。
  const validatePersonaPatch = (patch: PersonaInput): PersonaPatch => {
    const out: PersonaPatch = {};
    if (patch.description !== undefined) {
      if (typeof patch.description !== 'string') throw new ModeDomainError(400, { error: 'invalid-description' });
      out.description = patch.description;
    }
    if (patch.systemPrompt !== undefined) {
      if (typeof patch.systemPrompt !== 'string') throw new ModeDomainError(400, { error: 'invalid-system-prompt' });
      out.systemPrompt = patch.systemPrompt;
    }
    if (patch.tools !== undefined) out.tools = validateTools(patch.tools, allow);
    return out;
  };

  return {
    // GET /omp/personas：全量列表。
    async list() {
      return json({ personas: store.load() });
    },

    // GET /omp/personas/{name}；未找到 404。
    async get(request, ctx) {
      const persona = find(ctx?.params?.name);
      return persona ? json(persona) : json({ error: 'not-found' }, { status: 404 });
    },

    // POST：校验名唯一 + patch → 追加保存 → 201。
    async create(request) {
      const body = await readJsonBody<PersonaWriteBody>(request);
      const input = body?.persona ?? body;
      const name = validateName(input?.name);
      if (find(name)) throw new ModeDomainError(409, { error: 'persona-exists', name });
      const patch = validatePersonaPatch(input ?? {});
      const persona = {
        name,
        ...(patch.description !== undefined ? { description: patch.description } : {}),
        ...(patch.systemPrompt !== undefined ? { systemPrompt: patch.systemPrompt } : {}),
        ...(patch.tools !== undefined ? { tools: patch.tools } : {}),
      };
      store.save([...store.load(), persona]);
      return json(persona, { status: 201 });
    },

    // PUT：改名去重 + 现值/patch 合并 → 原位替换保存。
    async update(request, ctx) {
      const name = ctx?.params?.name;
      const existing = find(name);
      if (!existing) throw new ModeDomainError(404, { error: 'not-found' });
      const body = await readJsonBody<PersonaWriteBody>(request);
      const input = body?.persona ?? body ?? {};
      const renameTo = input.name !== undefined && input.name !== name
        ? validateName(input.name)
        : undefined;
      if (renameTo !== undefined && find(renameTo)) {
        throw new ModeDomainError(409, { error: 'persona-exists', name: renameTo });
      }
      const patch = validatePersonaPatch(input);
      const persona = { ...existing, ...patch, name: renameTo ?? existing.name };
      store.save(store.load().map((entry) => (entry.name === name ? persona : entry)));
      return json(persona);
    },

    // DELETE：按名过滤保存；204。
    async remove(request, ctx) {
      const name = ctx?.params?.name;
      if (!find(name)) throw new ModeDomainError(404, { error: 'not-found' });
      store.save(store.load().filter((persona) => persona.name !== name));
      return new Response(null, { status: 204 });
    },
  };
}

/** 会话 meta 的 persona 选择（旧版 meta.agent 按迁移契约参与，规范 02 §6.1）。 */
/** Session meta persona selection (legacy `meta.agent` participates, 02 §6.1). */
export interface PersonaMeta {
  /** persona 名（新字段，优先）。 */
  persona?: string;
  /** 旧版 agent 名；'build' / 'plan' / 未设置视为 standard。 */
  agent?: string;
}

/** personaFor 的结果（规范 02 §5.1 D-B2）：'standard' 表示无 overlay。 */
/** personaFor outcome (02 §5.1 D-B2): 'standard' = no overlay. */
export interface PersonaOverlay {
  /** standard=未选择 / active=命中 / missing=按名未找到。 */
  status: 'standard' | 'active' | 'missing';
  /** 请求的 persona 名（missing 时回显）。 */
  name?: string;
  /** 命中的 persona 投影（name + 可选 systemPrompt / tools）。 */
  persona: { name: string; systemPrompt?: string; tools?: string[] } | null;
}

/**
 * 物化时解析会话 meta 的 persona overlay（规范 02 §5.1 D-B2）。旧版
 * meta.agent 按迁移契约（02 §6.1）参与：'build' / 'plan' / 未设置 →
 * standard；其它名字按 persona 名处理。未命中返回 missing（不抛错）。
 */
/**
 * Resolve the persona overlay for a session meta at materialize time
 * (02 §5.1 D-B2). Legacy `meta.agent` values participate per the migration
 * contract (02 §6.1): 'build'/'plan'/unset → standard; any other name is
 * treated as a persona name.
 *
 * @param {PersonaMeta | null} meta
 * @param {Iterable<OmpPersona> | Map<string, OmpPersona>} personas
 * @returns {PersonaOverlay}
 */
export function personaFor(meta: PersonaMeta | null, personas: Iterable<OmpPersona> | Map<string, OmpPersona>): PersonaOverlay {
  const byName = new Map();
  if (personas instanceof Map) {
    for (const [key, value] of personas) byName.set(key, value);
  } else if (personas && typeof personas[Symbol.iterator] === 'function') {
    for (const persona of personas) {
      if (persona && typeof persona.name === 'string') byName.set(persona.name, persona);
    }
  }
  const requested = meta?.persona
    ?? (meta?.agent === 'build' || meta?.agent === 'plan' || meta?.agent === undefined
      ? undefined
      : meta.agent);
  if (requested === undefined) return { status: 'standard', persona: null };
  const persona = byName.get(requested);
  if (!persona) return { status: 'missing', name: requested, persona: null };
  return {
    status: 'active',
    persona: {
      name: persona.name,
      ...(persona.systemPrompt !== undefined ? { systemPrompt: persona.systemPrompt } : {}),
      ...(Array.isArray(persona.tools) ? { tools: persona.tools } : {}),
    },
  };
}

// ---------------------------------------------------------------------------
// 4. Mode tracker (02 §5.4)
// ---------------------------------------------------------------------------

/** 模式进入载荷（POST /omp/sessions/{id}/mode action=enter；按模式取用字段）。 */
/** Mode enter payload (POST /omp/sessions/{id}/mode action=enter; per-mode fields). */
export interface ModeEnterData {
  /** plan：计划文件路径。 */
  /** plan */
  planFilePath?: string;
  /** plan：计划文件是否已有草稿内容。 */
  hasDraftContent?: boolean;
  /** vibe */
  /** vibe：进入前捕获、退出时恢复的工具集。 */
  previousTools?: string[];
  /** goal */
  /** goal：目标描述。 */
  objective?: string;
  /** loop */
  /** loop：次数上限（正整数）。 */
  count?: number;
  /** loop：时长上限毫秒数（正整数）。 */
  durationMs?: number;
  /** loop：每轮 prompt。 */
  prompt?: string;
}

/** POST /omp/sessions/{id}/mode 请求体：`{ action?, mode? }` + 进入载荷。 */
/** POST /omp/sessions/{id}/mode body: `{ action?, mode? }` + the enter payload. */
export interface ModeActionBody extends ModeEnterData {
  /** enter | exit | pause | resume；缺省按 mode 推断。 */
  action?: unknown;
  /** 目标模式（exit 时可为 'none'）。 */
  mode?: string;
}

/**
 * loop 投影（规范 02 §5.4）：host-driver 状态，从不持久化。用别名而非
 * interface，以便随 ModeChangeData 结构化满足 SDK appendModeChange 的
 * Record<string, unknown> 边界。
 */
/**
 * Loop projection (02 §5.4): host-driver state, never persisted. An alias
 * (not an interface) so it satisfies the SDK's `Record<string, unknown>`
 * appendModeChange boundary structurally when riding {@link ModeChangeData}.
 */
export type ModeLoopState = {
  /** running | paused。 */
  state: 'running' | 'paused';
  /** 剩余次数（按 count 驱动时）。 */
  remaining?: number;
  /** 上限：次数或毫秒数。 */
  limit?: number;
  /** 每轮 prompt。 */
  prompt?: string;
};

/**
 * 模式转换时追加/发布的每模式载荷（规范 02 §5.4）：固定投影形状——
 * plan {planFilePath}、goal {objective}、vibe {previousTools}、
 * loop 为 ModeLoopState、prewalk {target} / {active:false}、
 * 冷启动恢复 {recovered:true}。
 */
/**
 * Per-mode payload appended/published on a mode transition (02 §5.4): the
 * fixed projection shapes — plan `{planFilePath}`, goal `{objective}`, vibe
 * `{previousTools}`, loop = {@link ModeLoopState}, prewalk `{target}` /
 * `{active:false}`, cold-start recovery `{recovered:true}`.
 */
export type ModeChangeData =
  | { planFilePath: string }
  | { objective: string }
  | { previousTools: string[] }
  | ModeLoopState
  | { target?: string }
  | { active: boolean }
  | { recovered: boolean };

/** omp goal 载荷透传（goal_updated 事件；status 驱动 goal_paused 投影）。 */
/** omp goal payload passthrough (`goal_updated` events; status drives goal_paused). */
export interface ModeGoal {
  /** goal 标识。 */
  id?: string;
  /** goal 状态；'paused' 时投影为 goal_paused。 */
  status?: unknown;
}

/** GET /omp/sessions/{id}/mode 响应载荷（规范 02 §5.4）。 */
/** GET /omp/sessions/{id}/mode payload (02 §5.4). */
export interface ModeSnapshot {
  /** 当前模式投影值。 */
  mode: ModeValue;
  /** 会话 persona 名（已设置时）。 */
  persona?: string;
  /** plan 态数据（plan / plan_paused 时出现）。 */
  plan?: {
    planFilePath: string;
    paused: boolean;
    hasDraftContent: boolean;
    review?: PlanReviewDetails;
  };
  /** goal 态数据（goal / goal_paused 且有 goal 时出现）。 */
  goal?: ModeGoal & { state?: unknown };
  /** loop 态数据（loop 时出现）。 */
  loop?: ModeLoopState;
  /** prewalk 位数据（armed 时出现）。 */
  prewalk?: { target?: string };
}

/**
 * buildSessionContext() 的恢复载荷（SDK session-context.ts:280-282）：
 * 取路径上最后一条 mode_change 条目的 `{ mode, modeData }`。
 */
/**
 * buildSessionContext() recovery payload (SDK session-context.ts:280-282):
 * `{ mode, modeData }` from the last mode_change entry on the path.
 */
export interface ModeSessionContext {
  /** 持久化的模式值（校验前的原始值）。 */
  mode?: unknown;
  /** 该模式条目的附带数据。 */
  modeData?: ModeSessionContextData | null;
  /** 无标记的 thenable 判别——trackerFor 同步值与 thenable 都接受。 */
  /** Untagged thenable discrimination — trackerFor accepts sync or thenable. */
  then?: undefined;
}

/** 冷启动恢复消费的 modeData 字段（值为校验前的原始值）。 */
/** modeData fields consumed by cold-start recovery (values pre-validation). */
export interface ModeSessionContextData {
  /** plan 模式的计划文件路径。 */
  planFilePath?: unknown;
  /** 计划文件是否已有草稿内容。 */
  hasDraftContent?: unknown;
  /** vibe 模式进入时捕获的工具集。 */
  previousTools?: unknown;
  /** goal 模式的 goal 对象。 */
  goal?: unknown;
}

/**
 * 本领域发布的 omp 总线载荷（events.ts 视为 opaque）：
 * `omp.mode.changed { mode, data? }`（02 §5.4）与
 * `omp.plan.review_requested { details }`（02 §5.5）。
 */
/**
 * omp bus payloads this domain publishes (events.ts treats them as opaque):
 * `omp.mode.changed { mode, data? }` (02 §5.4) and
 * `omp.plan.review_requested { details }` (02 §5.5).
 */
export type OmpEventPayload =
  | { mode: string; data?: ModeChangeData }
  | { details: PlanReviewDetails | null };

/** omp 总线 publish 绑定（每会话的 OmpEventBus.publish，events.ts）。 */
/** omp bus publish binding (per-session OmpEventBus.publish, events.ts). */
export type OmpEventPublish = (
  type: string,
  payload: OmpEventPayload,
  options?: { durable?: boolean },
) => void;

/**
 * sessionManager.appendModeChange 绑定（SDK session-manager.ts:2179）；
 * data 为 tracker 追加的每模式条目载荷（规范 02 §5.4）。
 */
/**
 * sessionManager.appendModeChange binding (SDK session-manager.ts:2179);
 * `data` is the per-mode entry payload this tracker appends (02 §5.4).
 */
export type ModeAppendEntry = (mode: string, data?: ModeChangeData) => string | undefined;

/** createModeTracker 选项。 */
/** createModeTracker options. */
export interface ModeTrackerOptions {
  /** omp 总线 publish 绑定（缺省不发事件）。 */
  publish?: OmpEventPublish;
  /** mode_change 条目追加绑定（缺省不持久化）。 */
  appendEntry?: ModeAppendEntry;
  /** 时间戳来源，缺省 Date.now。 */
  now?: () => number;
}

/** 每会话的 mode tracker（规范 02 §5.4 状态机 + 投影）。 */
/** Per-session mode tracker (02 §5.4 state machine + projection). */
export interface ModeTracker {
  /** 当前投影值（02 §5.4 模式集）。 */
  readonly mode: ModeValue;
  /** 进入模式；互斥冲突时抛 409 mode-conflict。 */
  enterMode(mode: string, data?: ModeEnterData): ModeSnapshot;
  /** 退到 none；paused 只对 plan 记 plan_paused。 */
  exitMode(options?: { paused?: boolean; persist?: boolean }): ModeSnapshot;
  /** 暂停：plan→plan_paused、goal→goal_paused、loop→paused。 */
  pauseMode(mode?: string): ModeSnapshot;
  /** 恢复：三个 paused 态各自回到运行态。 */
  resumeMode(mode?: string): ModeSnapshot;
  /** 消费 goal_updated 事件并派生 goal_paused。 */
  applyGoalUpdate<T>(goal: ModeGoal | null | undefined, goalState?: T): ModeSnapshot;
  /** prewalk 状态位（与互斥模式集正交）。 */
  setPrewalk(active: boolean, options?: { target?: string }): ModeSnapshot;
  /** 计划草稿位——由计划文件写入路径回填。 */
  setPlanDraft(hasDraftContent: boolean): ModeSnapshot;
  /** 评审状态——由 plan review bridge 回填。 */
  setReview(review: PlanReviewDetails | null | undefined): ModeSnapshot;
  /** 会话 persona 选择（只影响投影，不产生事件）。 */
  setPersona(persona: string | null | undefined): ModeSnapshot;
  /** 冷启动恢复：从 buildSessionContext 结果还原投影。 */
  recoverFromSessionContext(sessionContext: ModeSessionContext | null | undefined): ModeSnapshot;
  /** GET /api/omp/sessions/{id}/mode 载荷。 */
  snapshot(): ModeSnapshot;
}

/** createModeTracker 背后的内部状态形状。 */
/** Internal state shape behind createModeTracker. */
interface ModeTrackerState {
  /** 当前模式投影值。 */
  mode: ModeValue;
  /** 会话 persona 名（未选为 undefined）。 */
  persona: string | undefined;
  /** vibe 进入时捕获、退出时待恢复的工具集。 */
  previousTools: string[] | undefined;
  /** plan 态数据：路径 + 是否有草稿。 */
  plan: { planFilePath: string; hasDraftContent: boolean } | null;
  /** goal 态数据：goal 对象 + engine 侧 state。 */
  goal: { goal: ModeGoal; state: unknown } | null;
  /** loop 态数据（host-driver，不持久化）。 */
  loop: ModeLoopState | null;
  /** prewalk 位数据（target 可选）。 */
  prewalk: { target?: string } | null;
  /** 最近一次计划评审详情。 */
  review: PlanReviewDetails | null;
}


/**
 * 每会话的模式状态机（规范 02 §5.4）。
 *
 * 持久化与 TUI 完全对齐：plan 进入/暂停/退出、vibe 进入/退出会追加
 * mode_change 条目；goal 的条目由 SDK GoalRuntime 负责（此处只投影）；
 * loop 从不持久化。进入互斥冲突时抛 409 mode-conflict。
 */
/**
 * Per-session mode state machine.
 *
 * Persistence mirrors the TUI exactly:
 * - plan enter → appendModeChange('plan', { planFilePath })   (interactive-mode.ts:2751)
 * - plan pause/exit → 'plan_paused' / 'none'                  (interactive-mode.ts:2916)
 * - plan resume (from paused) → 'plan' + { planFilePath }     (interactive-mode.ts:3937-3938)
 * - vibe enter → 'vibe' + { previousTools }                   (interactive-mode.ts:3524)
 * - vibe exit → 'none'                                        (vibe/runtime.ts:630)
 * - goal enter/pause: NOT appended here — the SDK's GoalRuntime persist
 *   callback owns goal mode_change entries                    (agent-session.ts:1420-1426)
 * - goal exit → 'none' (TUI parity, interactive-mode.ts:2966) — pass
 *   `{ persist: false }` when the SDK already persisted the drop.
 * - loop: never persisted (the TUI never writes loop entries either).
 *
 * @param {ModeTrackerOptions} [options]
 */
export function createModeTracker({ publish, appendEntry, now = Date.now }: ModeTrackerOptions = {}): ModeTracker {
  // 内部状态（见 ModeTrackerState），初始为 none / 全空。
  const state: ModeTrackerState = {
    mode: 'none',
    persona: undefined,
    previousTools: undefined,
    plan: null,     // { planFilePath, hasDraftContent }
    goal: null,     // { goal, state }
    loop: null,     // { state, remaining?, limit?, prompt? }
    prewalk: null,  // { target? }
    review: null,   // PlanApprovalDetails — fed via setReview (review bridge)
  };

  // 发布 omp.mode.changed（durable）；publish 未注入时为 no-op。
  const emit = (mode: string, data?: ModeChangeData) => {
    publish?.('omp.mode.changed', { mode, ...(data !== undefined ? { data } : {}) }, { durable: true });
  };
  // 追加 mode_change 条目；appendEntry 未注入时为 no-op。
  const append = (mode: string, data?: ModeChangeData) => appendEntry?.(mode, data);

  // 进入前置检查：同类进入幂等、plan 从 paused 恢复、loop 与 goal/vibe 可叠进；否则抛冲突。
  const assertEntering = (entering: string): 'idempotent' | 'resume' | 'enter' => {
    const current = state.mode;
    if (current === entering || (entering === 'goal' && current === 'goal_paused')) return 'idempotent';
    if (entering === 'plan' && current === 'plan_paused') return 'resume';
    if (current === 'none') return 'enter';
    if (entering === 'goal' && current === 'loop') return 'enter';
    if (entering === 'vibe' && current === 'loop') return 'enter';
    if (entering === 'loop' && (current === 'goal' || current === 'goal_paused' || current === 'vibe')) return 'enter';
    throw modeConflict(current);
  };

  // 进入 plan：记录计划路径（缺省 DEFAULT_PLAN_FILE_PATH）并追加+发布 plan 条目。
  const enterPlan = (data: ModeEnterData = {}) => {
    const planFilePath = typeof data.planFilePath === 'string' && data.planFilePath
      ? data.planFilePath
      : DEFAULT_PLAN_FILE_PATH;
    state.mode = 'plan';
    state.plan = { planFilePath, hasDraftContent: Boolean(data.hasDraftContent) };
    if (Array.isArray(data.previousTools)) state.previousTools = data.previousTools;
    append('plan', { planFilePath });
    emit('plan', { planFilePath });
  };

  // 进入 goal：只发布投影（GoalRuntime 拥有持久化，避免重复条目）。
  const enterGoal = (data: ModeEnterData = {}) => {
    state.mode = 'goal';
    // Goal mode_change persistence is owned by the SDK GoalRuntime
    // (agent-session.ts:1420-1426) — appending here would duplicate entries.
    emit('goal', typeof data.objective === 'string' && data.objective ? { objective: data.objective } : undefined);
  };

  // 进入 vibe：必须携带 previousTools（退出时恢复用），追加+发布。
  const enterVibe = (data: ModeEnterData = {}) => {
    if (!Array.isArray(data.previousTools)) {
      throw new ModeDomainError(400, {
        error: 'vibe-requires-previous-tools',
        message: 'vibe enter requires previousTools (the captured toolset to restore on exit)',
      });
    }
    state.mode = 'vibe';
    state.previousTools = data.previousTools;
    append('vibe', { previousTools: data.previousTools });
    emit('vibe', { previousTools: data.previousTools });
  };

  // 进入 loop：校验 count/durationMs 为正整数后初始化 loop 态；只发布不持久化。
  const enterLoop = (data: ModeEnterData = {}) => {
    const count = data.count === undefined ? undefined : Number(data.count);
    const durationMs = data.durationMs === undefined ? undefined : Number(data.durationMs);
    if (count !== undefined && (!Number.isInteger(count) || count < 1)) {
      throw new ModeDomainError(400, { error: 'invalid-loop-count', count: data.count });
    }
    if (durationMs !== undefined && (!Number.isInteger(durationMs) || durationMs < 1)) {
      throw new ModeDomainError(400, { error: 'invalid-loop-duration', durationMs: data.durationMs });
    }
    state.mode = 'loop';
    state.loop = {
      state: 'running',
      ...(count !== undefined ? { remaining: count } : {}),
      limit: count ?? durationMs,
      ...(typeof data.prompt === 'string' && data.prompt ? { prompt: data.prompt } : {}),
    };
    // Loop is host-driver state; the TUI never persists loop mode entries.
    emit('loop', { ...state.loop });
  };

  // 返回的 ModeTracker 实例（方法间经 tracker.snapshot() 互调）。
  const tracker: ModeTracker = {
    // 当前投影值（02 §5.4 模式集）。
    /** Current projection value (02 §5.4 mode set). */
    get mode() {
      return state.mode;
    },

    // 进入模式（plan|goal|vibe|loop）；同类重入幂等，plan 从 plan_paused 恢复。
    /**
     * Enter a mode. `mode` ∈ plan | goal | vibe | loop. Same-mode re-enter is
     * an idempotent no-op; plan enter from plan_paused resumes.
     */
    enterMode(mode: string, data: ModeEnterData = {}) {
      if (!MODE_VALUES.includes(mode) || mode === 'none') {
        throw new ModeDomainError(400, { error: 'invalid-mode', mode });
      }
      const kind = assertEntering(mode);
      if (kind === 'idempotent') return tracker.snapshot();
      if (mode === 'plan') enterPlan(data);
      else if (mode === 'goal') enterGoal(data);
      else if (mode === 'vibe') enterVibe(data);
      else enterLoop(data);
      return tracker.snapshot();
    },

    // 退出到 none；paused 记 plan_paused，persist:false 跳过追加（SDK 已持久化时）。
    /**
     * Exit the active mode to 'none'. `{ paused: true }` from plan records
     * plan_paused instead (TUI three-state toggle). `{ persist: false }`
     * skips the mode_change append when the SDK already persisted the exit.
     */
    exitMode({ paused = false, persist = true }: { paused?: boolean; persist?: boolean } = {}) {
      const from = state.mode;
      if (from === 'none') return tracker.snapshot();
      if (from === 'plan') {
        state.mode = paused ? 'plan_paused' : 'none';
        if (!paused) state.plan = null;
        if (persist) append(paused ? 'plan_paused' : 'none');
        emit(state.mode);
        return tracker.snapshot();
      }
      if (from === 'plan_paused') {
        state.mode = 'none';
        state.plan = null;
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      if (from === 'goal' || from === 'goal_paused') {
        state.mode = 'none';
        state.goal = null;
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      if (from === 'vibe') {
        state.mode = 'none';
        if (persist) append('none');
        emit('none');
        return tracker.snapshot();
      }
      // loop
      state.mode = 'none';
      state.loop = null;
      emit('none');
      return tracker.snapshot();
    },

    // action=pause：plan→plan_paused、goal→goal_paused、loop→paused。
    /** action=pause: plan → plan_paused, goal → goal_paused, loop → paused. */
    pauseMode(mode?: string) {
      const target = mode ?? conflictFor(state.mode);
      if (target === 'plan') {
        if (state.mode === 'plan_paused') {
          throw new ModeDomainError(400, { error: 'already-paused', mode: 'plan' });
        }
        if (state.mode !== 'plan') throw new ModeDomainError(400, { error: 'not-active', mode: 'plan' });
        return tracker.exitMode({ paused: true });
      }
      if (target === 'goal') {
        if (state.mode === 'goal_paused') {
          throw new ModeDomainError(400, { error: 'already-paused', mode: 'goal' });
        }
        if (state.mode !== 'goal') throw new ModeDomainError(400, { error: 'not-active', mode: 'goal' });
        state.mode = 'goal_paused';
        // SDK GoalRuntime owns goal pause entries (agent-session.ts:1420-1426).
        emit('goal_paused');
        return tracker.snapshot();
      }
      if (target === 'loop') {
        if (state.mode !== 'loop' || !state.loop) throw new ModeDomainError(400, { error: 'not-active', mode: 'loop' });
        state.loop = { ...state.loop, state: 'paused' };
        emit('loop', { ...state.loop });
        return tracker.snapshot();
      }
      throw new ModeDomainError(400, { error: 'invalid-mode', mode: target });
    },

    // action=resume：plan_paused→plan、goal_paused→goal、loop paused→running。
    /** action=resume: plan_paused → plan, goal_paused → goal, loop paused → running. */
    resumeMode(mode?: string) {
      const target = mode ?? conflictFor(state.mode);
      if (target === 'plan') {
        if (state.mode === 'plan') throw new ModeDomainError(400, { error: 'not-paused', mode: 'plan' });
        if (state.mode !== 'plan_paused') throw new ModeDomainError(400, { error: 'not-active', mode: 'plan' });
        return tracker.enterMode('plan', { planFilePath: state.plan?.planFilePath });
      }
      if (target === 'goal') {
        if (state.mode === 'goal') throw new ModeDomainError(400, { error: 'not-paused', mode: 'goal' });
        if (state.mode !== 'goal_paused') throw new ModeDomainError(400, { error: 'not-active', mode: 'goal' });
        state.mode = 'goal';
        // SDK GoalRuntime owns goal resume entries.
        emit('goal');
        return tracker.snapshot();
      }
      if (target === 'loop') {
        if (state.mode !== 'loop' || !state.loop) throw new ModeDomainError(400, { error: 'not-active', mode: 'loop' });
        state.loop = { ...state.loop, state: 'running' };
        emit('loop', { ...state.loop });
        return tracker.snapshot();
      }
      throw new ModeDomainError(400, { error: 'invalid-mode', mode: target });
    },

    // 消费 goal_updated 事件（发布在 engine.js，Wave 0）；据 goal.status 派生 goal_paused。
    /**
     * Consume a goal_updated event (the omp.goal.updated publish lives in
     * engine.js, Wave 0). Derives goal_paused from goal.status while a goal
     * mode is active (the projection value set, 02 §5.4).
     */
    applyGoalUpdate<T>(goal: ModeGoal | null | undefined, goalState?: T) {
      state.goal = goal === null || goal === undefined ? null : { goal, state: goalState };
      if (state.mode === 'goal' || state.mode === 'goal_paused') {
        const next = goal?.status === 'paused' ? 'goal_paused' : 'goal';
        if (next !== state.mode) {
          state.mode = next;
          emit(next);
        }
      }
      return tracker.snapshot();
    },

    // prewalk 状态位（02 §5.7），与互斥模式集正交；armed 发布 {target}，disarmed 发布 {active:false}。
    /**
     * Prewalk status bit (02 §5.7): orthogonal to the exclusive mode set.
     * Arming publishes `omp.mode.changed {mode:'prewalk', data:{target}}`
     * (spec projection); disarming publishes `data:{active:false}`.
     */
    setPrewalk(active: boolean, { target }: { target?: string } = {}) {
      if (active) {
        state.prewalk = target !== undefined && target !== null ? { target } : {};
        emit('prewalk', { ...(target !== undefined && target !== null ? { target } : {}) });
      } else {
        state.prewalk = null;
        emit('prewalk', { active: false });
      }
      return tracker.snapshot();
    },

    // 计划草稿位——由计划文件写入路径（04 领域）回填。
    /** Plan draft content bit — fed by the plan-file write path (04 domain). */
    setPlanDraft(hasDraftContent: boolean) {
      if (state.plan) state.plan = { ...state.plan, hasDraftContent: Boolean(hasDraftContent) };
      return tracker.snapshot();
    },

    // 评审状态——由 plan review bridge 的 onReview 接线回填。
    /** Review state — fed by the plan review bridge (onReview wiring). */
    setReview(review: PlanReviewDetails | null | undefined) {
      state.review = review && typeof review === 'object' ? review : null;
      return tracker.snapshot();
    },

    // 设置/清除会话 persona（只影响投影，不产生事件）。
    setPersona(persona: string | null | undefined) {
      state.persona = persona === undefined || persona === null ? undefined : String(persona);
      return tracker.snapshot();
    },

    // 冷启动恢复（02 §5.4）：消费 buildSessionContext 的 {mode, modeData} 还原投影并发布一次；从不追加。
    /**
     * Cold-start recovery (02 §5.4): consume the SDK SessionManager's
     * buildSessionContext() result — `{ mode, modeData }` from the last
     * mode_change entry on the path (session-context.ts:280-282) — restore the
     * projection, and publish omp.mode.changed once. Never appends.
     */
    recoverFromSessionContext(sessionContext: ModeSessionContext | null | undefined) {
      const persisted = sessionContext?.mode;
      const data = sessionContext?.modeData;
      if (persisted === 'plan' || persisted === 'plan_paused') {
        state.mode = persisted;
        state.plan = {
          planFilePath: typeof data?.planFilePath === 'string' && data.planFilePath
            ? data.planFilePath
            : DEFAULT_PLAN_FILE_PATH,
          hasDraftContent: Boolean(data?.hasDraftContent),
        };
      } else if (persisted === 'goal' || persisted === 'goal_paused') {
        state.mode = persisted;
        if (data?.goal && typeof data.goal === 'object') state.goal = { goal: data.goal, state: undefined };
      } else if (persisted === 'vibe') {
        state.mode = 'vibe';
        if (Array.isArray(data?.previousTools)) state.previousTools = data.previousTools;
      } else {
        state.mode = 'none';
      }
      emit(state.mode, state.mode === 'none' ? undefined : { recovered: true });
      return tracker.snapshot();
    },

    // GET /api/omp/sessions/{id}/mode 载荷（02 §5.4）。
    /** GET /api/omp/sessions/{id}/mode payload (02 §5.4). */
    snapshot(): ModeSnapshot {
      const out: ModeSnapshot = { mode: state.mode };
      if (state.persona !== undefined) out.persona = state.persona;
      if (state.mode === 'plan' || state.mode === 'plan_paused') {
        out.plan = {
          planFilePath: state.plan?.planFilePath ?? DEFAULT_PLAN_FILE_PATH,
          paused: state.mode === 'plan_paused',
          hasDraftContent: Boolean(state.plan?.hasDraftContent),
          ...(state.review ? { review: state.review } : {}),
        };
      }
      if ((state.mode === 'goal' || state.mode === 'goal_paused') && state.goal) {
        out.goal = { ...state.goal.goal, ...(state.goal.state !== undefined ? { state: state.goal.state } : {}) };
      }
      if (state.mode === 'loop' && state.loop) out.loop = { ...state.loop };
      if (state.prewalk) out.prewalk = { ...state.prewalk };
      return out;
    },
  };

  return tracker;
}

// ---------------------------------------------------------------------------
// 5. Plan review bridge (02 §5.5)
// ---------------------------------------------------------------------------

/**
 * 计划评审详情载荷（SDK preparePlanForReview 返回的 details；
 * agent-session.ts:933-948）。
 */
/** Plan review details payload (SDK preparePlanForReview `details`; agent-session.ts:933-948). */
export interface PlanReviewDetails {
  /** 计划文件路径（local:// 形式）。 */
  planFilePath?: string;
  /** 计划标题——xd://propose 收到的写入值。 */
  title?: string;
  /** 计划文件是否已存在。 */
  planExists?: boolean;
}

/** preparePlanForReview 的返回值（content 原样透传，不在此解释）。 */
/** preparePlanForReview result (content passes through opaquely). */
export interface PreparePlanReviewResult {
  /** 回给模型的 content 块数组（opaque）。 */
  content: unknown[];
  /** 计划评审详情。 */
  details: PlanReviewDetails;
}

/** bridge 消费的 AgentSession 最小表面：只需 preparePlanForReview。 */
/** AgentSession surface consumed by the bridge. */
export interface PlanProposalSession {
  /** 校验计划产物并返回 content + details。 */
  preparePlanForReview: (title: string) => Promise<PreparePlanReviewResult>;
}

/** POST /omp/sessions/{id}/plan/review 请求体（规范 02 §5.5 步骤 5）。 */
/** POST /omp/sessions/{id}/plan/review body (02 §5.5 step 5). */
export interface PlanReviewDecisionInput {
  /** 评审决定：approve-execute / approve-compact / approve-keep / refine。 */
  choice?: string;
  /** refine 时的反馈文本，透传给引擎重新提示。 */
  feedback?: unknown;
  /** 执行阶段角色（approve 时可选）。 */
  executionRole?: unknown;
  /** 评审时编辑过的计划内容（approve 时可选）。 */
  editedContent?: unknown;
}

/** 经 propose 工具结果回写给模型的决定回显（规范 02 §5.5）。 */
/** Decision echo settled through the propose tool result (02 §5.5). */
export interface PlanReviewDecisionDetails {
  /** 评审决定值。 */
  choice: string;
  /** 计划文件路径。 */
  planFilePath?: string;
  /** refine 反馈（原样透传）。 */
  feedback?: unknown;
  /** 执行角色（原样透传）。 */
  executionRole?: unknown;
  /** 编辑后的内容（原样透传）。 */
  editedContent?: unknown;
}

/** decide() 的结果：refine 不放行执行（dispatched:false），本轮继续 planning。 */
/** decide() outcome: refine keeps the turn in planning (`dispatched:false`). */
export interface PlanReviewDecisionResult {
  /** 是否已放行执行（approve* 为 true）。 */
  dispatched: boolean;
  /** 最近一次 decide 的输入回显；无 pending 时也保留。 */
  decision: PlanReviewDecisionInput | null;
  /** 未放行原因（如 no-pending-proposal）。 */
  reason?: string;
}

/** xd://propose 钩子返回的工具结果（content 原样透传）。 */
/** Tool result returned by the xd://propose hook (content passes through). */
export interface PlanReviewToolResult {
  /** 回给模型的 content 块数组。 */
  content: unknown[];
  /** 评审详情或决定回显（视结清路径而定）。 */
  details?: PlanReviewDetails | PlanReviewDecisionDetails;
}

/** GET /omp/sessions/{id}/plan 响应载荷片段（规范 02 §5.5 步骤 7）。 */
/** GET /omp/sessions/{id}/plan payload fragment (02 §5.5 step 7). */
export interface PlanReviewSnapshot {
  /** 当前计划文件路径（缺省 DEFAULT_PLAN_FILE_PATH）。 */
  planFilePath: string;
  /** 最近一次评审详情（有则带出）。 */
  review?: PlanReviewDetails;
}

/** 挂起的 propose：评审决定到来前 held 的工具结果 promise（规范 02 §5.5 步骤 4）。 */
/** Pending propose held until a review decision (02 §5.5 step 4). */
interface PlanReviewPending {
  /** 结清（settle）时 resolve 的回调。 */
  resolve: (result: PlanReviewToolResult) => void;
  /** 触发本次评审的详情。 */
  details: PlanReviewDetails | null;
  /** 发起时间戳（now()）。 */
  requestedAt: number;
}

/** planReviewBridge 的内部状态形状。 */
/** planReviewBridge internal state. */
interface PlanReviewBridgeState {
  /** preparePlanForReview 绑定（hookFor 注入）。 */
  prepareRef: ((title: string) => Promise<PreparePlanReviewResult>) | null;
  /** 最近一次评审指向的计划文件路径。 */
  planFilePath: string | null;
  /** 最近一次评审详情。 */
  review: PlanReviewDetails | null;
  /** 当前挂起的 propose（至多一个）。 */
  pending: PlanReviewPending | null;
  /** 最近一次 decide() 输入。 */
  decision: PlanReviewDecisionInput | null;
  /** 会话拆除标记；true 后不再挂起新的 pending。 */
  disposed: boolean;
}

/** planReviewBridge 选项。 */
/** planReviewBridge options. */
export interface PlanReviewBridgeOptions {
  /** omp 总线 publish 绑定。 */
  publish?: OmpEventPublish;
  /** preparePlanForReview 绑定（也可经 hookFor 注入）。 */
  prepare?: (title: string) => Promise<PreparePlanReviewResult>;
  /** 评审状态回调（详情变化 / 清空时通知 tracker）。 */
  onReview?: (details: PlanReviewDetails | null) => void;
  /** 时间戳来源，缺省 Date.now。 */
  now?: () => number;
}

/** xd://propose 工具钩子桥（规范 02 §5.5 步骤 3）的实例表面。 */
/** Bridge for the `xd://propose` tool hook (02 §5.5 step 3). */
export interface PlanReviewBridge {
  /** propose 钩子本体：挂起至评审决定后返回工具结果。 */
  hook: (title: string) => Promise<PlanReviewToolResult>;
  /** 把钩子绑定到指定 AgentSession 的 preparePlanForReview 后返回。 */
  hookFor: (session: PlanProposalSession) => (title: string) => Promise<PlanReviewToolResult>;
  /** 提交评审决定并结清 pending。 */
  decide: (input?: PlanReviewDecisionInput) => PlanReviewDecisionResult;
  /** GET /plan 载荷片段。 */
  snapshot: () => PlanReviewSnapshot;
  /** 清空评审状态（plan 退出）；pending 以 superseded 结清。 */
  clear: () => PlanReviewSnapshot;
  /** 会话拆除：pending 以中止通知结清。 */
  dispose: (reason?: string) => void;
}


/** 旧 propose 被新提案取代时的统一工具结果（新评审到达 / clear 时结清旧 pending）。 */
const SUPERSEDED_RESULT: PlanReviewToolResult = {
  content: [{ type: 'text', text: 'Plan review superseded by a newer proposal.' }],
};

/**
 * xd://propose 工具钩子桥（规范 02 §5.5 步骤 3）。
 *
 * 机制：session.setPlanProposalHandler 安装的处理器在模型向 xd://propose
 * 写入计划标题时被调用（tools/resolve.ts:109-110）；TUI 直接挂
 * preparePlanForReview（interactive-mode.ts:2739），本桥在其外再包一层——
 * 先发布 durable 的 omp.plan.review_requested {details}，再让工具结果
 * promise 挂起，直到 decide() 送来评审决定，从而复刻 TUI 阻塞式评审
 * 覆盖层的语义而不中断回合。
 */
/**
 * Bridge for the `xd://propose` tool hook (02 §5.5 step 3).
 *
 * Verified SDK surface: `session.setPlanProposalHandler(handler)` installs the
 * handler `xd://propose` dispatches the written plan title to
 * (tools/resolve.ts:109-110, agent-session.ts:1726-1735); the TUI attaches
 * `title => session.preparePlanForReview(title)` (interactive-mode.ts:2739),
 * which validates the plan artifact and returns
 * `{ content:[{type:'text',text:'Plan ready for review.'}], details:
 * { planFilePath, title, planExists } }` (agent-session.ts:933-948).
 *
 * This bridge wraps that handler: it publishes
 * `omp.plan.review_requested {details}` (durable) and holds the tool result
 * pending until a review decision arrives (`decide`), mirroring the TUI
 * overlay's blocking semantics without the TUI's turn abort.
 *
 * @param {PlanReviewBridgeOptions} [options]
 */
export function planReviewBridge({ publish, prepare, onReview, now = Date.now }: PlanReviewBridgeOptions = {}): PlanReviewBridge {
  // 桥内部状态（见 PlanReviewBridgeState）。
  const state: PlanReviewBridgeState = {
    prepareRef: prepare ?? null,
    planFilePath: null,
    review: null,   // PlanApprovalDetails of the latest propose
    pending: null,  // { resolve, details, requestedAt }
    decision: null, // last decide() input
    disposed: false,
  };

  // 结清当前 pending（至多一个）并 resolve 其 promise；无 pending 时为 no-op。
  const settle = (result: PlanReviewToolResult) => {
    const pending = state.pending;
    state.pending = null;
    pending?.resolve(result);
  };

  const bridge: PlanReviewBridge = {
    // xd://propose 钩子本体：校验计划 → 发布评审请求 → 挂起等待 decide()。
    /**
     * The xd://propose hook. Attach with
     * `session.setPlanProposalHandler(bridge.hook)`; `bridge.hookFor(session)`
     * returns it bound to that session's preparePlanForReview.
     */
    hook: async (title: string): Promise<PlanReviewToolResult> => {
      if (!state.prepareRef) {
        throw new ModeDomainError(400, {
          error: 'plan-review-not-bound',
          message: 'planReviewBridge has no prepare binding; use hookFor(session).',
        });
      }
      const result = await state.prepareRef(title);
      // dispose() may have run while prepare was in flight — never strand a
      // fresh pending promise on a torn-down bridge.
      if (state.disposed) {
        return { content: [{ type: 'text', text: 'Plan review aborted: bridge disposed.' }] };
      }
      const details = result?.details ?? null;
      settle(SUPERSEDED_RESULT);
      state.review = details;
      if (details?.planFilePath) state.planFilePath = details.planFilePath;
      onReview?.(details);
      publish?.('omp.plan.review_requested', { details }, { durable: true });
      return await new Promise((resolve) => {
        state.pending = { resolve, details, requestedAt: now() };
      });
    },

    // 把钩子绑定到活跃 AgentSession 的 preparePlanForReview 上。
    /** Bind the hook to a live AgentSession's preparePlanForReview. */
    hookFor: (session: PlanProposalSession) => {
      if (typeof session?.preparePlanForReview !== 'function') {
        throw new TypeError('planReviewBridge.hookFor requires an AgentSession with preparePlanForReview');
      }
      state.prepareRef = (title) => session.preparePlanForReview(title);
      return bridge.hook;
    },

    // 用评审决定结清挂起的 propose（02 §5.5 步骤 5）；refine 不放行执行。
    /**
     * Settle the pending proposal with a review decision
     * (POST /omp/sessions/{id}/plan/review body, 02 §5.5 step 5).
     * Returns `{ dispatched, decision }` — refine keeps the turn in planning
     * (`dispatched:false`) and the engine re-prompts with `feedback`.
     */
    decide: (input?: PlanReviewDecisionInput): PlanReviewDecisionResult => {
      const choice = input?.choice;
      if (typeof choice !== 'string' || !PLAN_REVIEW_CHOICES.includes(choice)) {
        throw new ModeDomainError(400, { error: 'invalid-choice', choice, choices: [...PLAN_REVIEW_CHOICES] });
      }
      state.decision = { ...input };
      if (!state.pending) {
        return { dispatched: false, decision: state.decision, reason: 'no-pending-proposal' };
      }
      const details = state.pending.details;
      if (choice === 'refine') {
        settle({
          content: [{
            type: 'text',
            text: `Plan refinement requested. Update the plan file, then write ${details?.title ?? 'the plan title'} to xd://propose again when ready.`,
          }],
          details: { choice, ...(input?.feedback !== undefined ? { feedback: input.feedback } : {}) },
        });
        return { dispatched: false, decision: state.decision };
      }
      settle({
        content: [{ type: 'text', text: `Plan approved (${choice}).` }],
        details: {
          choice,
          planFilePath: details?.planFilePath,
          ...(input?.executionRole !== undefined ? { executionRole: input.executionRole } : {}),
          ...(input?.editedContent !== undefined ? { editedContent: input.editedContent } : {}),
        },
      });
      return { dispatched: true, decision: state.decision };
    },

    // GET /omp/sessions/{id}/plan 载荷片段（02 §5.5 步骤 7）。
    /** GET /omp/sessions/{id}/plan payload fragment (02 §5.5 step 7). */
    snapshot: (): PlanReviewSnapshot => {
      return {
        planFilePath: state.planFilePath ?? DEFAULT_PLAN_FILE_PATH,
        ...(state.review ? { review: state.review } : {}),
      };
    },

    // 丢弃评审状态（plan 退出）；任何挂起 propose 以 superseded 结清。
    /** Drop review state (plan exit). Any pending propose settles superseded. */
    clear: (): PlanReviewSnapshot => {
      settle(SUPERSEDED_RESULT);
      state.review = null;
      state.decision = null;
      onReview?.(null);
      return bridge.snapshot();
    },

    // 会话拆除：挂起的 propose 以中止通知结清，之后的钩子调用直接短路。
    /** Session teardown: settle the pending propose with an abort notice. */
    dispose: (reason = 'session disposed'): void => {
      state.disposed = true;
      settle({
        content: [{ type: 'text', text: `Plan review aborted: ${reason}.` }],
      });
      state.pending = null;
    },
  };

  return bridge;
}

// ---------------------------------------------------------------------------
// 6. Modes domain + route mounting
// ---------------------------------------------------------------------------

/**
 * 从路由上下文解析 directory（规范 02 §5.1）：优先 query 参数 `directory`，
 * 其次 `x-opencode-directory` 请求头（URL 解码），再经 normalizeDirectoryKey
 * 归一化；两处都没有时返回 null，由调用方决定是否回答 400。
 */
const directoryParam = (ctx?: ModesRouteContext): string | null => {
  const fromQuery = ctx?.url?.searchParams?.get('directory');
  const fromHeader = ctx?.headers?.get?.('x-opencode-directory');
  const raw = fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

/**
 * 把捕获的错误映射为 HTTP 响应：ModeDomainError 按其 status/body 原样
 * 作答；其它错误视为 bug 原样上抛，由 host 的路由包装层兜底 500
 * （host.js:84-92）。
 */
// Domain errors map 1:1 to their HTTP status; anything else is a bug and
// rethrows so the host's route wrapper answers a 500 (host.js:84-92).
const toResponse = <E>(error: E): Response => {
  if (error instanceof ModeDomainError) return json(error.body, { status: error.status });
  throw error;
};

/**
 * createModesDomain 的依赖注入（规范 02 §5.1/§5.4）：engine 侧逐会话的
 * omp 总线 publish、mode_change 追加器、冷启动恢复上下文，以及
 * agent 定义 / personas 的存储适配器与校验开关。
 */
/** Per-session bindings injected by the engine (spec 02 §5.1/§5.4). */
export interface ModesDomainDeps {
  /** 该会话的 omp 总线 publish 绑定（engine 绑定 #ompPublish）。 */
  /** omp bus publish for that session (engine binds #ompPublish). */
  publishFor?: (sessionId: string, directory: string) => OmpEventPublish;
  /** 经该会话 sessionManager.appendModeChange 追加 (mode, data) 条目。 */
  /** `(mode, data)` appending through that session's sessionManager.appendModeChange. */
  appendFor?: (sessionId: string, directory: string) => ModeAppendEntry;
  /** 冷启动恢复用的 buildSessionContext() 结果（同步或 thenable 均可）。 */
  /** buildSessionContext() (sync or thenable) for cold-start recovery. */
  sessionContextFor?: (
    sessionId: string,
    directory: string,
  ) => ModeSessionContext | PromiseLike<ModeSessionContext> | null | undefined;
  /** omp agent 发现链 + .md 写入适配器（规范 02 §5.2）。 */
  /** omp agent discovery chain + .md write adapters (02 §5.2). */
  agentDefinitions?: AgentDefinitionAdapter;
  /** personas 存储；缺省时整组 personas 路由不挂载。 */
  personasStore?: PersonaStore;
  /** 工具白名单，供 tools 字段的写入校验。 */
  allowedTools?: Set<string> | Iterable<string>;
  /** settings 的 projectScopes.v1 开关；false 时拒绝 project 作用域写入。 */
  settingsProjectScopes?: boolean;
  /** 按目录读取生效 task.* 覆盖值的函数（engine 注入）。 */
  overridesFor?: OverridesFor;
}

/**
 * modes/plan/goal/personas/agent-definitions 领域对象（规范 02）：持有
 * 按会话缓存的 tracker 与 bridge、可选的两组 handlers，并把路由注册
 * 代理到 registerModesDomainRoutes。
 */
/** Modes/plan/goal/personas/agent-definitions domain object (spec 02). */
export interface ModesDomain {
  /** 取（或惰性创建）该会话的 mode tracker。 */
  trackerFor: (sessionId: string, directory: string | null) => ModeTracker;
  /** 取（或惰性创建）该会话的 plan review bridge。 */
  bridgeFor: (sessionId: string, directory: string | null) => PlanReviewBridge;
  /** 会话释放：dispose bridge 并移除两处缓存。 */
  release: (sessionId: string, directory: string | null) => void;
  /** agent 定义 CRUD handlers；未注入适配器时为 null。 */
  agentDefinitions: AgentDefinitionHandlers | null;
  /** personas CRUD handlers；未注入 store 时为 null。 */
  personas: PersonaHandlers | null;
  /** 便捷代理：等价于 registerModesDomainRoutes(route, this, options)。 */
  register: (route: ModesRouteMount, options?: ModesRouteRegistrationOptions) => void;
}

/** registerModesDomainRoutes 选项。 */
/** registerModesDomainRoutes options. */
export interface ModesRouteRegistrationOptions {
  /** 能力开关表；key 为 false 或缺失时对应路由组显式返回 501。 */
  features?: Record<string, boolean>;
}

/**
 * 创建领域对象（规范 02 §5.1/§5.4）：持有 per-session 的 mode tracker 与
 * plan review bridge，所有 engine 状态经注入绑定到达。首次访问某会话的
 * tracker 时用 sessionContextFor 的结果做一次冷启动恢复（thenable 异步
 * 消费，失败静默）。
 */
/**
 * Domain object owning per-session mode trackers and plan review bridges.
 * All engine state reaches it through injected bindings:
 * - `publishFor(sessionId, directory)` → omp bus publish for that session
 *   (engine binds `#ompPublish`).
 * - `appendFor(sessionId, directory)` → `(mode, data)` appending through that
 *   session's `sessionManager.appendModeChange` (SDK session-manager.ts:2179).
 * - `sessionContextFor(sessionId, directory)` → `buildSessionContext()` result
 *   (sync or thenable) for cold-start recovery on first access.
 */
export function createModesDomain({
  publishFor,
  appendFor,
  sessionContextFor,
  agentDefinitions: agentDefinitionsOptions,
  personasStore,
  allowedTools,
  settingsProjectScopes = false,
  overridesFor,
}: ModesDomainDeps = {}): ModesDomain {
  // 会话键（目录 key + sessionId）→ tracker 缓存。
  const trackers = new Map<string, ModeTracker>();
  // 会话键 → plan review bridge 缓存。
  const bridges = new Map<string, PlanReviewBridge>();
  // 会话缓存键：归一化目录 key + sessionId（目录与会话共同唯一）。
  const keyOf = (sessionId: string, directory: string | null): string => `${normalizeDirectoryKey(directory)}${sessionId}`;

  // 取或创建该会话的 tracker；创建后可选消费 buildSessionContext 做冷启动恢复。
  const trackerFor = (sessionId: string, directory: string | null): ModeTracker => {
    const key = keyOf(sessionId, directory);
    const existing = trackers.get(key);
    if (existing) return existing;
    const directoryKey = normalizeDirectoryKey(directory);
    const tracker = createModeTracker({
      publish: publishFor?.(sessionId, directoryKey),
      appendEntry: appendFor?.(sessionId, directoryKey),
    });
    trackers.set(key, tracker);
    if (sessionContextFor) {
      const context = sessionContextFor(sessionId, directoryKey);
      // 只有同步值在此恢复；thenable 由外层 .then 消费（失败静默）。
      const recover = (value: ModeSessionContext | PromiseLike<ModeSessionContext> | null | undefined) => {
        if (value && typeof value === 'object' && typeof value.then !== 'function') {
          tracker.recoverFromSessionContext(value);
        }
      };
      if (context && typeof context.then === 'function') context.then(recover, () => {});
      else recover(context);
    }
    return tracker;
  };

  // 取或创建该会话的 plan review bridge；onReview 回写 tracker.setReview 保持快照同步。
  const bridgeFor = (sessionId: string, directory: string | null): PlanReviewBridge => {
    const key = keyOf(sessionId, directory);
    const existing = bridges.get(key);
    if (existing) return existing;
    const directoryKey = normalizeDirectoryKey(directory);
    const tracker = trackerFor(sessionId, directory);
    const bridge = planReviewBridge({
      publish: publishFor?.(sessionId, directoryKey),
      onReview: (details) => tracker.setReview(details),
    });
    bridges.set(key, bridge);
    return bridge;
  };

  // 会话释放：先 dispose bridge（结清挂起的 propose）再清空两处缓存。
  const release = (sessionId: string, directory: string | null): void => {
    const key = keyOf(sessionId, directory);
    bridges.get(key)?.dispose('session released');
    bridges.delete(key);
    trackers.delete(key);
  };

  const agentDefinitions = agentDefinitionsOptions
    ? createAgentDefinitionHandlers({
      ...agentDefinitionsOptions,
      allowedTools,
      settingsProjectScopes,
      overridesFor,
    })
    : null;
  const personas = personasStore
    ? createPersonaHandlers({ store: personasStore, allowedTools })
    : null;

  // 组装并返回的领域对象。
  const domain: ModesDomain = {
    trackerFor,
    bridgeFor,
    release,
    agentDefinitions,
    personas,
    // 路由注册代理，转发给 registerModesDomainRoutes。
    register(route: ModesRouteMount, options?: ModesRouteRegistrationOptions) {
      return registerModesDomainRoutes(route, domain, options);
    },
  };
  return domain;
}

/**
 * 挂载本领域拥有的全部 /omp 路由（对外路径为 /api/omp/...，web proxy
 * 会剥掉 /api 前缀）。每组路由按能力 key 门控——key 为 false 或缺失时
 * 返回显式 501（master R2；见 omp-parity 的 featureUnavailable），让
 * 客户端大声失败而不是拿到静默 404。
 */
/**
 * Mount the /omp routes owned by this domain (public paths are /api/omp/...;
 * the web proxy strips /api). Each group is gated by its capability key —
 * a `false`/missing key answers an explicit 501 so clients fail loudly
 * (master R2; omp-parity.js featureUnavailable).
 *
 * @param {ModesRouteMount} route
 * @param {ModesDomain} domain
 * @param {ModesRouteRegistrationOptions} [options]
 */
export function registerModesDomainRoutes(
  route: ModesRouteMount,
  domain: ModesDomain,
  { features = ompFeatures() }: ModesRouteRegistrationOptions = {},
): void {
  // 能力门控包装器：features[key] === true 才执行 handler，否则 501。
  const gated = (featureKey: string, handler: ModesRouteHandler): ModesRouteHandler =>
    async (request, ctx) =>
      features?.[featureKey] === true ? handler(request, ctx) : featureUnavailable(featureKey);

  // ---- sessions/{id}/mode (modes.v1) ----
  route('GET', '/omp/sessions/{id}/mode', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    return json(domain.trackerFor(ctx?.params?.id ?? '', directory).snapshot());
  }));

  route('POST', '/omp/sessions/{id}/mode', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<ModeActionBody>(request);
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const action = typeof body?.action === 'string' && body.action
      ? body.action
      : body?.mode === 'none' ? 'exit' : 'enter';
    try {
      if (action === 'enter') tracker.enterMode(typeof body?.mode === 'string' ? body.mode : '', body ?? {});
      else if (action === 'exit') tracker.exitMode();
      else if (action === 'pause') tracker.pauseMode(body?.mode);
      else if (action === 'resume') tracker.resumeMode(body?.mode);
      else return badRequest(`invalid action "${action}"`);
    } catch (error) {
      return toResponse(error);
    }
    return json(tracker.snapshot());
  }));

  // ---- sessions/{id}/plan (modes.v1) ----
  route('GET', '/omp/sessions/{id}/plan', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const bridge = domain.bridgeFor(ctx?.params?.id ?? '', directory);
    const snapshot = tracker.snapshot();
    const bridgeSnapshot = bridge.snapshot();
    const review = snapshot.plan?.review ?? bridgeSnapshot.review ?? null;
    const planActive = snapshot.mode === 'plan' || snapshot.mode === 'plan_paused';
    if (!planActive && !review) {
      return json({ error: 'plan-mode-inactive' }, { status: 404 });
    }
    // The reviewed plan outranks the mode-state path (TUI handlePlanApproval
    // promotes details.planFilePath, interactive-mode.ts:3982-3983).
    return json({
      planFilePath: review?.planFilePath ?? snapshot.plan?.planFilePath ?? bridgeSnapshot.planFilePath,
      ...(review ? { review } : {}),
    });
  }));

  route('POST', '/omp/sessions/{id}/plan/review', gated('modes.v1', async (request, ctx) => {
    const directory = directoryParam(ctx);
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<PlanReviewDecisionInput>(request);
    const tracker = domain.trackerFor(ctx?.params?.id ?? '', directory);
    const bridge = domain.bridgeFor(ctx?.params?.id ?? '', directory);
    try {
      const result = bridge.decide(body ?? {});
      return json({ dispatched: result.dispatched, mode: tracker.snapshot().mode });
    } catch (error) {
      return toResponse(error);
    }
  }));

  // ---- agent-definitions (agentDefinitions.v1) ----
  // agent 定义 handlers；未注入适配器时整组路由不挂载。
  const agentHandlers = domain.agentDefinitions;
  if (agentHandlers) {
    route('GET', '/omp/agent-definitions', gated('agentDefinitions.v1', (request, ctx) => (agentHandlers.list(request, ctx))));
    route('GET', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', (request, ctx) => (agentHandlers.get(request, ctx))));
    route('POST', '/omp/agent-definitions', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.create(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('PUT', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.update(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('DELETE', '/omp/agent-definitions/{name}', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.remove(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('POST', '/omp/agent-definitions/refresh', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.refresh(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('POST', '/omp/agent-definitions/{name}/reveal', gated('agentDefinitions.v1', async (request, ctx) => {
      try {
        return await agentHandlers.reveal(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
  }

  // ---- personas (personas.v1) ----
  // persona handlers；未注入 store 时整组路由不挂载。
  const personaHandlers = domain.personas;
  if (personaHandlers) {
    route('GET', '/omp/personas', gated('personas.v1', personaHandlers.list));
    route('GET', '/omp/personas/{name}', gated('personas.v1', personaHandlers.get));
    route('POST', '/omp/personas', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.create(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('PUT', '/omp/personas/{name}', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.update(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
    route('DELETE', '/omp/personas/{name}', gated('personas.v1', async (request, ctx) => {
      try {
        return await personaHandlers.remove(request, ctx);
      } catch (error) {
        return toResponse(error);
      }
    }));
  }
}
