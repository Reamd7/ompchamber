// Domain module: omp custom provider CRUD over the engine's models.yml
// (OMPChamber-owned capability; the SDK has no write API for models.yml).
//
// The omp engine is the only provider authority under modelRoles.v1: the wire
// provider list is built from ModelRegistry.getAvailable(), custom providers
// live in `<agentDir>/models.yml` (schema: models-config-schema-bundle.ts),
// and the legacy OpenCode auth/config writes answer explicit 501s. This
// module gives the Providers settings page a real write path:
//
//   GET    /omp/providers        — engine providers tagged by origin
//                                  (`file` = models.yml-defined, editable;
//                                  `engine` = builtin/login, read-only),
//                                  credentials never echoed (only hasApiKey).
//   PUT    /omp/providers        — upsert ONE file provider. Field-merge
//                                  semantics: only GUI-managed keys are
//                                  written; hand-authored keys the form never
//                                  shows (compat, discovery, modelOverrides,
//                                  per-model thinking/cost/input, transport,
//                                  …) are preserved untouched, and an absent
//                                  apiKey keeps the existing one.
//   DELETE /omp/providers/{id}   — remove a file-defined provider; engine
//                                  (builtin/login) providers answer 409.
//
// Writes are comment-preserving: models.yml is user-authored (the omp
// template ships fully commented), so edits go through the `yaml` Document
// API — never a whole-file re-serialization. A one-time `models.yml.backup`
// anchors recovery to the last pre-GUI state. The merged value is validated
// with the SDK's own schema + validateProviderConfiguration BEFORE anything
// touches disk, and the write is atomic (temp + rename). After a successful
// write the coordinator refreshes the ModelRegistry (mtime-checked static
// reload), so the new provider is live without a host restart.
//
// Capability `providers.v1` gates all three routes (master R2): missing/false
// answers an explicit 501.
//
// SELF-CONTAINED BY CONTRACT: no engine.js/endpoints.js imports; the
// coordinator mounts registerProvidersDomainRoutes(route, { features,
// modelsPath, listEngineModels, refreshModels }).

/**
 * 领域模块：基于引擎 models.yml 的 omp 自定义 provider 增删改查
 * （OMPChamber 自有能力；SDK 没有针对 models.yml 的写 API）。
 *
 * modelRoles.v1 之下 omp 引擎是 provider 的唯一权威：线上 provider 列表由
 * ModelRegistry.getAvailable() 构造，自定义 provider 存放在
 * <agentDir>/models.yml（schema 见 models-config-schema-bundle.ts），旧版
 * OpenCode 的 auth/config 写接口一律显式 501。本模块为 Providers 设置页提供
 * 真正的写路径：
 *
 *   GET    /omp/providers        —— 按 origin 标注引擎 provider
 *                                   （file = models.yml 定义、可编辑；
 *                                   engine = 内置/登录、只读），
 *                                   凭据永不回传（只返回 hasApiKey）。
 *   PUT    /omp/providers        —— upsert 单个 file provider，字段级合并：
 *                                   只写 GUI 管理的键；表单不展示的手工键
 *                                   （compat、discovery、modelOverrides、
 *                                   逐模型 thinking/cost/input、transport 等）
 *                                   原样保留；apiKey 缺省时沿用旧值。
 *   DELETE /omp/providers/{id}   —— 删除 file 定义的 provider；engine
 *                                   （内置/登录）provider 返回 409。
 *
 * 写操作保留注释：models.yml 由用户手工维护（omp 模板整文件带注释），因此编辑
 * 一律走 yaml 的 Document API，绝不整体重新序列化。一次性的 models.yml.backup
 * 把恢复锚定到最后一个 pre-GUI 状态。合并结果在落盘前先用 SDK 自带的 schema +
 * validateProviderConfiguration 校验，写入为原子操作（临时文件 + rename）。写
 * 成功后由 coordinator 刷新 ModelRegistry（按 mtime 检查的静态重载），新
 * provider 无需重启 host 即可生效。
 *
 * 能力开关 providers.v1 同时守卫全部路由（master R2）：缺失/为 false 时显式 501。
 *
 * 契约上的自包含模块：不 import engine.js/endpoints.js；由 coordinator 调用
 * registerProvidersDomainRoutes(route, { features, modelsPath,
 * listEngineModels, refreshModels }) 完成挂载。
 */
import fs from 'node:fs';
import path from 'node:path';
import YAML from 'yaml';
import { getAgentDir } from '@oh-my-pi/pi-coding-agent';
import {
  ModelsConfigFile,
  validateProviderConfiguration,
} from '@oh-my-pi/pi-coding-agent/config/models-config';
import { errorText, errorCode, featureUnavailable, ompFeatures } from './omp-parity.ts';
import { YAMLMap, YAMLSeq, Scalar, isMap, isNode, isSeq, parseDocument, Document, type Node } from 'yaml';

/** What `Response.json` itself accepts — this helper only forwards to it. */
/** Response.json 本身接受的参数类型——该类型只是为 helper 转发签名服务。 */
type ResponseJsonData = Parameters<typeof Response.json>[0];

/** 构造 JSON Response；init 透传 status/headers 等 ResponseInit。 */
const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

/**
 * JSON value models.yml and the provider wire carry (same shape contract as
 * domain-plugins.ts JsonValue).
 */
/** models.yml 与 provider 线上载荷承载的 JSON 值（与 domain-plugins.ts 的 JsonValue 同形契约）。 */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/** Open string-keyed JSON object: provider entries, wire bodies, and header
 * maps are keyed by ids/names neither side enumerates up front, so the record
 * stays intentionally open while its values remain concrete JSON. */
/** 开放的字符串键 JSON 对象：provider 条目、请求体与 header 表都以双方都不预先
 * 枚举的 id/名称为键，故记录有意保持开放，而值仍是具体的 JSON。 */
type JsonRecord = Record<string, JsonValue>;

/** Duck-typed surface of omptype's OmpErrors aggregate this module reads
 * (lazy .map over per-path problems — see schemaProblems). */
/** omptype OmpErrors 聚合在本模块读取的鸭子类型表面
 * （对逐 path 问题做惰性 .map——见 schemaProblems）。 */
interface OmpErrorsAggregate {
  /** 把每条 {path, problem} 错误投影为字符串，返回问题清单。 */
  map: (fn: (error: { path?: PropertyKey[]; problem?: string }) => string) => string[];
}

/** Runtime value domain flowing through this module's shape guards: plain
 * JSON (models.yml values, provider wire bodies) plus the OmpErrors
 * aggregate an SDK schema call may answer. */
/** 流经本模块形状守卫的运行时值域：纯 JSON（models.yml 值、provider 请求体）
 * 加上 SDK schema 调用可能返回的 OmpErrors 聚合。 */
type ProviderRuntimeValue = JsonValue | OmpErrorsAggregate;

/** provider id 的合法形态：小写字母或数字开头，其后为小写字母/数字/连字符/下划线。 */
const PROVIDER_ID_PATTERN = /^[a-z0-9][a-z0-9-_]*$/;

/** API values accepted at provider level (models-config-schema ApiSchema). */
/** provider 级别接受的 API 协议取值（models-config-schema 的 ApiSchema）。 */
const OMP_PROVIDER_APIS = Object.freeze([
  'openai-completions',
  'openai-responses',
  'openai-codex-responses',
  'azure-openai-responses',
  'anthropic-messages',
  'bedrock-converse-stream',
  'google-generative-ai',
  'google-gemini-cli',
  'google-vertex',
]);

/** 默认 models.yml 路径：<agentDir>/models.yml（getAgentDir 解析 omp 配置目录）。 */
const defaultModelsPath = () => path.join(getAgentDir(), 'models.yml');

// ─────────────────────────────────────────────────────────────────────────────
// Named contracts (write-path inputs/options, projections, route mounting)
// ─────────────────────────────────────────────────────────────────────────────

/** Engine model handle this module consumes (origin tags + collision guard). */
/** 本模块消费的引擎 model 句柄（用于 origin 标注与 id 冲突守卫）。 */
export interface OmpEngineModelRef {
  /** 该 model 所属的 provider id。 */
  provider: string;
}

/** 引擎侧 model 列表回调：coordinator 注入的 ModelRegistry 读取口径。 */
export type ListEngineModels = () => Array<OmpEngineModelRef>;

/** 写路径成功后刷新 ModelRegistry 的回调（按 mtime 检查的静态重载）。 */
export type RefreshModels = () => Promise<void>;

/** listOmpProviders 的可选项。 */
export interface OmpListProvidersOptions {
  /** models.yml 路径；缺省为 <agentDir>/models.yml。 */
  modelsPath?: string;
  /** 引擎 model 列表注入；缺省时仅列出 file provider。 */
  listEngineModels?: ListEngineModels;
}

/** putOmpProvider 的可选项。 */
export interface OmpProviderWriteOptions {
  /** models.yml 路径；缺省为 <agentDir>/models.yml。 */
  modelsPath?: string;
  /** origin 冲突守卫：新 id 撞上引擎（内置/登录）provider 时拒绝 409。 */
  listEngineModels?: ListEngineModels;
  /** 落盘成功后的 ModelRegistry 刷新回调；其失败不影响 PUT 结果。 */
  refreshModels?: RefreshModels;
}

/** deleteOmpProvider 的可选项。 */
export interface OmpProviderDeleteOptions {
  /** models.yml 路径；缺省为 <agentDir>/models.yml。 */
  modelsPath?: string;
  /** 用于识别引擎 provider：file 中不存在时据此回答 404/409 语义。 */
  listEngineModels?: ListEngineModels;
  /** 落盘成功后的 ModelRegistry 刷新回调；其失败不影响 DELETE 结果。 */
  refreshModels?: RefreshModels;
}

/** fetchOmpProviderModels 的可选项。 */
export interface FetchOmpProviderModelsOptions {
  /** models.yml 路径；缺省为 <agentDir>/models.yml。 */
  modelsPath?: string;
  /** Callable subset of fetch; @types/node 24 requires preconnect on typeof fetch. */
  /** fetch 的可调用子集；@types/node 24 要求对 typeof fetch 预先连接。 */
  fetchImpl?: (url: string, init?: RequestInit) => Promise<Response>;
}

/** PUT input — unvalidated wire payload; every field is re-checked before use. */
/** PUT 输入——未校验的线上载荷；每个字段在使用前都会重新检查。 */
export interface PutOmpProviderInput {
  /** 待 upsert 的 provider 对象（id、baseUrl、apiKey、headers、models 等）。 */
  provider?: JsonValue;
}

/** DELETE 输入：目标 provider id。 */
export interface DeleteOmpProviderInput {
  /** 待删除的 file provider id。 */
  id?: string;
}

/** fetch-models 输入：id 定位 provider；baseUrl/apiKey 为表单草稿覆盖项。 */
export interface FetchOmpProviderModelsInput {
  /** 目标 provider id（文件中已存在或草稿新建）。 */
  id?: string;
  /** 草稿 baseUrl：未保存的新建 provider 也能先拉取模型列表。 */
  baseUrl?: string;
  /** 草稿 apiKey：优先于文件中保存的键。 */
  apiKey?: string;
}

/** GET /omp/providers model projection (edit-prefill subset). */
/** GET /omp/providers 的模型成本投影（编辑预填子集，单位按 models.yml 原值）。 */
export interface OmpProjectedModelCost {
  /** 每百万输入 token 的价格。 */
  input: number;
  /** 每百万输出 token 的价格。 */
  output: number;
  /** 缓存读取的单价。 */
  cacheRead: number;
  /** 缓存写入的单价。 */
  cacheWrite: number;
}

/** 模型 thinking 块投影：efforts 词表 + 默认档位。 */
export interface OmpProjectedModelThinking {
  /** 引擎归一后的 effort 词表（minimal..max 的子集，保持规范顺序）。 */
  efforts?: string[];
  /** 默认 effort 档位。 */
  defaultLevel?: string;
}

/** 单个模型的线上投影——只含编辑表单预填需要的字段，其余 models.yml 键不回传。 */
export interface OmpProjectedModel {
  /** 模型 id（provider 内唯一）。 */
  id: string;
  /** 展示名。 */
  name?: string;
  /** 是否推理（reasoning）模型。 */
  reasoning?: boolean;
  /** 上下文窗口 token 数。 */
  contextWindow?: number;
  /** 单次响应最大输出 token 数。 */
  maxTokens?: number;
  /** 接受的输入模态：text / image。 */
  input?: string[];
  /** 是否支持 tool 调用。 */
  supportsTools?: boolean;
  /** 是否省略 max output tokens 参数。 */
  omitMaxOutputTokens?: boolean;
  /** 计费成本块。 */
  cost?: OmpProjectedModelCost;
  /** 模型级 baseUrl 覆盖。 */
  baseUrl?: string;
  /** 模型级 API 协议。 */
  api?: string;
  /** 上下文晋升（context promotion）目标模型 id。 */
  contextPromotionTarget?: string;
  /** 压实（compaction）代理模型 id。 */
  compactionModel?: string;
  /** thinking 配置投影。 */
  thinking?: OmpProjectedModelThinking;
}

/** File-defined provider (models.yml): editable, key presence only. */
/** file 定义的 provider（models.yml）：可编辑；apiKey 只暴露"是否存在"。 */
export interface OmpFileProviderProjection {
  /** provider id。 */
  id: string;
  /** 来源标记：models.yml 定义。 */
  source: 'file';
  /** 请求基址（http/https）。 */
  baseUrl?: string;
  /** API 协议。 */
  api?: string;
  /** 是否改用自定义鉴权 header。 */
  authHeader?: boolean;
  /** 附加请求头（值一律为字符串）。 */
  headers?: Record<string, string>;
  /** 是否已保存 apiKey（凭据本身永不回传）。 */
  hasApiKey: boolean;
  /** 该 provider 的模型列表投影。 */
  models: OmpProjectedModel[];
}

/** Engine (builtin/login) provider: read-only listing entry. */
/** 引擎（内置/登录）provider：只读列表条目。 */
export interface OmpEngineProviderProjection {
  /** provider id。 */
  id: string;
  /** 来源标记：引擎内置或登录来源。 */
  source: 'engine';
  /** 引擎侧未提供模型明细，恒为空数组。 */
  models: OmpProjectedModel[];
}

/** GET /omp/providers 的单个条目：按 source 判别 file / engine 两臂。 */
export type OmpListedProvider = OmpFileProviderProjection | OmpEngineProviderProjection;

/** listOmpProviders 的返回值。 */
export interface OmpProviderListResult {
  /** 实际读取的 models.yml 路径。 */
  modelsPath: string;
  /** file + engine 合并后的 provider 列表。 */
  providers: OmpListedProvider[];
}

/** Uniform answer envelope for the write endpoints. */
/** 写端点统一使用的应答信封。 */
export interface OmpProviderRouteBody {
  /** 错误代号（validation / not-found / provider-exists-engine 等）。 */
  error?: string;
  /** 人类可读的错误/状态说明。 */
  message?: string;
  /** PUT 成功时回传的 provider 投影。 */
  provider?: OmpFileProviderProjection;
  /** DELETE 成功时被删除的 provider id。 */
  deleted?: string;
  /** fetch-models 成功时拉到的模型 id 列表。 */
  models?: string[];
}

/** 写端点的统一返回：HTTP 状态码 + 应答体（route 层只做转发）。 */
export interface OmpProviderRouteResult {
  /** HTTP 状态码（200/400/404/409/500/502）。 */
  status: number;
  /** 应答体信封。 */
  body: OmpProviderRouteBody;
}

/** 路由 handler 收到的上下文：模式参数由挂载方解析。 */
export interface ProvidersRouteContext {
  /** 路由模式中的命名参数（如 {id}）。 */
  params: Record<string, string>;
}

/** 领域路由 handler 签名：接收 Request 与可选上下文，返回 Response。 */
export type ProvidersRouteHandler = (request: Request, ctx?: ProvidersRouteContext) => Response | Promise<Response>;

/** 挂载回调签名：coordinator 提供的 route(method, pattern, handler)。 */
export type ProvidersRouteMount = (method: string, pattern: string, handler: ProvidersRouteHandler) => void;

/** registerProvidersDomainRoutes 的依赖注入项。 */
export interface ProvidersDomainDeps {
  /** 能力开关表；providers.v1 控制全部路由的 501 gate。 */
  features?: Record<string, boolean>;
  /** models.yml 路径；缺省为 <agentDir>/models.yml。 */
  modelsPath?: string;
  /** 引擎 model 列表读取回调。 */
  listEngineModels?: ListEngineModels;
  /** 写成功后的 ModelRegistry 刷新回调。 */
  refreshModels?: RefreshModels;
}

// ─────────────────────────────────────────────────────────────────────────────
// File reading (comment-preserving document)
// ─────────────────────────────────────────────────────────────────────────────

/**
 * 读取 models.yml 为保留注释的 yaml Document。
 * ENOENT 时返回空 Document（existed: false，PUT 据此跳过 backup）；
 * 其他读错误原样上抛。merge 关闭、别名守卫关闭（OMPChamber 自有文件，
 * 允许手工维护的锚点/别名在大文件下解析）。
 */
const readDocument = (modelsPath: string) => {
  let raw = '';
  try {
    raw = fs.readFileSync(modelsPath, 'utf8');
  } catch (error) {
    if (errorCode(error) === 'ENOENT') {
      return { doc: new Document(), existed: false };
    }
    throw error;
  }
  // models.yml is OMPChamber-authored state, not untrusted input; the yaml
  // billion-laughs alias-count guard stays off so large hand-maintained files
  // using anchors/aliases still resolve (-1 = guard off, enforced at toJS).
  return { doc: parseDocument(raw, { merge: false }), existed: true };
};

/** 取（或创建）文档顶层 providers 映射节点；后续写入都在该节点上外科手术式进行。 */
const providersMapOf = (doc: Document): YAMLMap => {
  const existing = doc.get('providers');
  if (existing && isMap(existing)) return existing;
  const providers = new YAMLMap();
  doc.set(new Scalar('providers'), providers);
  return providers;
};


/**
 * omptype schema calls return the parsed value on success or an OmpErrors
 * aggregate on failure (NOT an Error instance) — distinguish by shape and
 * surface the per-path problems; null means valid.
 */
/** 判断 schema 调用返回值是否为 OmpErrors 聚合（成功时是解析值，失败时不是 Error 实例）。 */
const isOmpErrorsAggregate = (value: ProviderRuntimeValue | undefined): value is OmpErrorsAggregate =>
  typeof value === 'object' && value !== null && 'map' in value && typeof value.map === 'function';

/**
 * 用 SDK 的 models.yml schema 校验整份文件值。
 * 返回 null 表示合法；否则把逐 path 的 problem 拼成可读字符串
 * （用于落盘前拒绝与 500 应答文案）。
 */
const schemaProblems = (fileValue: JsonValue): string | null => {
  // SAFETY: omptype's `Type` call signature answers `parsed | OmpErrors`;
  // the parsed models.yml value is plain JSON by construction, so only the
  // aggregate shape is added at this seam.
  const check = ModelsConfigFile.schema(fileValue) as ProviderRuntimeValue;
  if (isRecord(check)) return null;
  if (isOmpErrorsAggregate(check)) {
    return check.map((error) => `${(error?.path ?? []).join('.') || 'root'}: ${error?.problem ?? 'invalid'}`).join('; ');
  }
  const message = typeof check === 'object' && check !== null && 'message' in check ? check.message : undefined;
  return typeof message === 'string' ? message : String(check);
};

/** 把 yaml 节点解析为纯 JSON 值（锚点在 maxAliasCount:-1 下解析）；null 节点返回 null。 */
const plainValue = (doc: Document, node: Node | null | undefined): JsonValue | null => {
  if (node == null) return null;
  // SAFETY: models.yml nodes carry plain YAML/JSON data; toJS with the
  // alias-count guard off resolves anchors to those plain values (the same
  // contract readDocument parses under).
  return isNode(node) ? node.toJS(doc, { maxAliasCount: -1 }) as JsonValue : null;
};

/** Whole-document JSON value of models.yml — the module's only doc-level
 * toJS seam (schemaProblems consumes it). */
const documentJsonValue = (doc: Document): JsonValue =>
  // SAFETY: models.yml documents hold plain YAML/JSON data; toJS with the
  // alias-count guard off resolves anchors to plain values.
  doc.toJS({ maxAliasCount: -1 }) as JsonValue;

/** models.yml value the SDK's ConfigFile parses (schema-derived). */
/** SDK 的 models.yml 解析值（schema 派生）。 */
type ParsedModelsFile = ReturnType<typeof ModelsConfigFile.loadOrDefault>;
/** One provider entry in a parsed models.yml (schema-derived). */
/** 解析后 models.yml 中单个 provider 条目（schema 派生）。 */
type ParsedProviderEntry = NonNullable<ParsedModelsFile['providers']>[string];

/** 形状守卫：非数组且非 null 的对象收窄为 JsonRecord。 */
const isRecord = (value: ProviderRuntimeValue | undefined): value is JsonRecord =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

// Canonical effort vocabulary and order (models-config-schema EffortSchema /
// EFFORT_ORDER — not exported by the SDK, mirrored here).
/** 规范 effort 词表与顺序（models-config-schema 的 EffortSchema / EFFORT_ORDER——SDK 未导出，此处镜像）。 */
const THINKING_EFFORT_ORDER = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/**
 * Efforts for UI prefill from canonical or legacy shapes. Mirrors
 * ModelThinkingSchema normalization (efforts beats levels beats the
 * minLevel..maxLevel range) so a hand-authored range block prefills the
 * dialog with the efforts the engine itself resolves — otherwise the dialog
 * shows an empty list and its save silently deletes the block.
 */
/** 从规范或遗留形态推导 UI 预填的 effort 词表（efforts 优先于 levels 优先于 min/max 区间）。 */
const deriveThinkingEfforts = (thinking: { efforts?: unknown; levels?: unknown; minLevel?: unknown; maxLevel?: unknown }) => {
  const list = Array.isArray(thinking.efforts) ? thinking.efforts
    : Array.isArray(thinking.levels) ? thinking.levels : null;
  if (list !== null) {
    const isEffort = (e: string): e is (typeof THINKING_EFFORT_ORDER)[number] =>
      // SAFETY: includes() itself is the membership check; the assertion
      // only satisfies its parameter's declared level-union type.
      THINKING_EFFORT_ORDER.includes(e as (typeof THINKING_EFFORT_ORDER)[number]);
    // SAFETY: efforts/levels arrays hold effort names (provider config);
    // non-members are dropped by the predicate itself.
    return (list as string[]).filter(isEffort);
  }
  const min = typeof thinking.minLevel === 'string' ? THINKING_EFFORT_ORDER.indexOf(thinking.minLevel) : -1;
  const max = typeof thinking.maxLevel === 'string' ? THINKING_EFFORT_ORDER.indexOf(thinking.maxLevel) : -1;
  return min >= 0 && max >= min ? THINKING_EFFORT_ORDER.slice(min, max + 1) : [];
};
/**
 * Header values must be strings: the engine's models.yml schema rejects the
 * whole config on a non-string value, and the wire/UI contracts are
 * string-only. Hand-authored YAML may carry scalars (`X-Request-Id: 42`);
 * coerce those to their string form and drop anything structural, so one
 * loose value never blanks the provider list (plan P15).
 */
/** 把 header 表归一为纯字符串值：标量强转为字符串，对象/数组丢弃，避免单个松散值打穿整个 provider 列表。 */
const stringHeaders = (value: ProviderRuntimeValue | undefined): OmpFileProviderProjection['headers'] => {
  if (!isRecord(value)) return {};
  const out: Record<string, string> = {};
  for (const [key, headerValue] of Object.entries(value)) {
    if (typeof headerValue === 'string') out[key] = headerValue;
    else if (typeof headerValue === 'number' || typeof headerValue === 'boolean') out[key] = String(headerValue);
    // objects/arrays are not header values — dropped.
  }
  return out;
};

/** Projected file provider for GET / edit prefill. apiKey never leaves this
 * module — only `hasApiKey`. */
/** 把 file provider 条目投影为 GET/编辑预填形态；apiKey 永不出模块，只给 hasApiKey。 */
const projectFileProvider = (id: string, value: JsonRecord): OmpFileProviderProjection => {
  const models = Array.isArray(value.models) ? value.models : [];
  const headers = stringHeaders(value.headers);
  return {
    id,
    source: 'file',
    ...(typeof value.baseUrl === 'string' ? { baseUrl: value.baseUrl } : {}),
    ...(value.authHeader !== undefined ? { authHeader: Boolean(value.authHeader) } : {}),
    ...(headers && Object.keys(headers).length > 0 ? { headers } : {}),
    hasApiKey: typeof value.apiKey === 'string' && value.apiKey.length > 0,
    models: models
      .filter((model): model is JsonRecord & { id: string } => isRecord(model) && typeof model.id === 'string')
      .map((model) => ({
        id: model.id,
        ...(typeof model.name === 'string' ? { name: model.name } : {}),
        ...(model.reasoning !== undefined ? { reasoning: Boolean(model.reasoning) } : {}),
        ...(typeof model.contextWindow === 'number' ? { contextWindow: model.contextWindow } : {}),
        ...(typeof model.maxTokens === 'number' ? { maxTokens: model.maxTokens } : {}),
        ...(Array.isArray(model.input) ? { input: model.input.filter((v) => v === 'text' || v === 'image') } : {}),
        ...(model.supportsTools !== undefined ? { supportsTools: Boolean(model.supportsTools) } : {}),
        ...(model.omitMaxOutputTokens !== undefined ? { omitMaxOutputTokens: Boolean(model.omitMaxOutputTokens) } : {}),
        ...(isRecord(model.cost) ? { cost: {
          input: Number(model.cost.input) || 0,
          output: Number(model.cost.output) || 0,
          cacheRead: Number(model.cost.cacheRead) || 0,
          cacheWrite: Number(model.cost.cacheWrite) || 0,
        } } : {}),
        ...(typeof model.baseUrl === 'string' ? { baseUrl: model.baseUrl } : {}),
        ...(typeof model.api === 'string' ? { api: model.api } : {}),
        ...(typeof model.contextPromotionTarget === 'string' ? { contextPromotionTarget: model.contextPromotionTarget } : {}),
        ...(isRecord(model.thinking)
          ? {
              thinking: {
                efforts: deriveThinkingEfforts(model.thinking),
                ...(typeof model.thinking.defaultLevel === 'string' ? { defaultLevel: model.thinking.defaultLevel } : {}),
              },
            }
          : {}),
      })),
  };
};

// ─────────────────────────────────────────────────────────────────────────────
// GET
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Engine providers tagged by origin. Engine-available providers not defined
 * in models.yml are builtin/login providers (`source: 'engine'`, read-only).
 *
 * @param {{ modelsPath?: string, listEngineModels?: () => Array<{provider: string}> }} input
 */
/**
 * GET /omp/providers 的实现：列出 models.yml 的 file provider（带模型投影），
 * 并把引擎可用但未在文件中定义的 provider 标为只读的 engine 来源；
 * listEngineModels 抛错时退回仅文件事实，绝不让列表整体失败。
 */
export const listOmpProviders = async ({ modelsPath = defaultModelsPath(), listEngineModels }: OmpListProvidersOptions = {}): Promise<OmpProviderListResult> => {
  const engineIds = new Set<string>();
  if (typeof listEngineModels === 'function') {
    try {
      for (const model of listEngineModels() ?? []) {
        if (model?.provider) engineIds.add(model.provider);
      }
    } catch {
      // Engine unavailable → file truth only, never a failed listing.
    }
  }

  const { doc } = readDocument(modelsPath);
  const providers: OmpListedProvider[] = [];
  const fileIds = new Set<string>();
  const fileNode = doc.get('providers');
  if (fileNode && isMap(fileNode)) {
    for (const pair of fileNode.items) {
      const id = String(pair.key instanceof Scalar ? pair.key.value ?? '' : '');
      if (!id) continue;
      const value = isNode(pair.value) ? plainValue(doc, pair.value) : null;
      if (!isRecord(value)) continue;
      fileIds.add(id);
      providers.push(projectFileProvider(id, value));
    }
  }
  for (const id of engineIds) {
    if (!fileIds.has(id)) providers.push({ id, source: 'engine', models: [] });
  }
  return { modelsPath, providers };
};

// ─────────────────────────────────────────────────────────────────────────────
// PUT (upsert, field-merge)
// ─────────────────────────────────────────────────────────────────────────────

/** Normalized GUI-managed model row (null clears the key in models.yml). */
/** 归一后的 GUI 管理模型成本行（四项均为非负数）。 */
interface NormalizedModelCost {
  /** 每百万输入 token 价格。 */
  input: number;
  /** 每百万输出 token 价格。 */
  output: number;
  /** 缓存读取单价。 */
  cacheRead: number;
  /** 缓存写入单价。 */
  cacheWrite: number;
}

/** 归一后的 thinking 块（efforts 为空时删除整个块而非写非法配置）。 */
interface NormalizedModelThinking {
  /** thinking 模式，缺省 'effort'。 */
  mode?: string;
  /** 非空 effort 词表。 */
  efforts?: string[];
  /** 默认 effort 档位。 */
  defaultLevel?: string;
}

/** 归一后的入参模型行；可选字段为 null 表示"清除 models.yml 中的该键"。 */
interface NormalizedIncomingModel {
  /** 模型 id（必填，已 trim）。 */
  id: string;
  /** 展示名。 */
  name?: string;
  /** 是否推理模型。 */
  reasoning?: boolean;
  /** 输入模态；null 表示清除。 */
  input?: string[] | null;
  /** 成本块；null 表示清除。 */
  cost?: NormalizedModelCost | null;
  /** 是否支持 tool；null 表示清除。 */
  supportsTools?: boolean | null;
  /** 是否省略 max output tokens；null 表示清除。 */
  omitMaxOutputTokens?: boolean | null;
  /** 上下文晋升目标模型；null 表示清除。 */
  contextPromotionTarget?: string | null;
  /** 压实代理模型；null 表示清除。 */
  compactionModel?: string | null;
  /** 模型级 baseUrl；null 表示清除。 */
  baseUrl?: string | null;
  /** API 协议（仅接受 OMP_PROVIDER_APIS 成员）。 */
  api?: string;
  /** thinking 块；null 表示清除。 */
  thinking?: NormalizedModelThinking | null;
  /** 上下文窗口 token 数（正整数）。 */
  contextWindow?: number;
  /** 最大输出 token 数（正整数）。 */
  maxTokens?: number;
}

/** normalizeIncomingModel 的结果判别联合：成功带 model，失败只带 error 文案。 */
type NormalizedModelResult = { model: NormalizedIncomingModel; error?: undefined } | { error: string; model?: undefined };

/** GUI 管理的可选字符串键（null 语义 = 清除）。 */
type ManagedStringKey = 'contextPromotionTarget' | 'compactionModel' | 'baseUrl';
/** GUI 管理的可选布尔键（null 语义 = 清除）。 */
type ManagedBooleanKey = 'supportsTools' | 'omitMaxOutputTokens';

/**
 * 校验并归一一条入参模型行：id 必填，逐字段做类型/取值检查，
 * 任何非法值都返回带字段路径的 error（调用方转 400），绝不静默丢字段。
 */
const normalizeIncomingModel = (raw: JsonValue, index: number): NormalizedModelResult => {
  if (!isRecord(raw)) return { error: `models[${index}]: expected an object` };
  const id = typeof raw.id === 'string' ? raw.id.trim() : '';
  if (!id) return { error: `models[${index}]: id is required` };
  const model: NormalizedIncomingModel = { id };
  if (raw.name !== undefined) {
    if (typeof raw.name !== 'string' || !raw.name.trim()) return { error: `models[${index}].name: expected a non-empty string` };
    model.name = raw.name.trim();
  }
  if (raw.reasoning !== undefined) {
    if (typeof raw.reasoning !== 'boolean') return { error: `models[${index}].reasoning: expected a boolean` };
    model.reasoning = raw.reasoning;
  }
  // 可选字符串键的统一校验：undefined 保持、null 清除、字符串 trim 后写入。
  const managedOptionalString = (key: ManagedStringKey) => {
    const value = raw[key];
    if (value === undefined) return;
    if (value === null) { model[key] = null; return; }
    if (typeof value !== 'string' || !value.trim()) {
      return { error: `models[${index}].${key}: expected a non-empty string or null` };
    }
    model[key] = value.trim();
  };
  // 可选布尔键的统一校验：undefined 保持、null 清除、布尔直接写入。
  const managedOptionalBoolean = (key: ManagedBooleanKey) => {
    const value = raw[key];
    if (value === undefined) return;
    if (value === null) { model[key] = null; return; }
    if (typeof value !== 'boolean') {
      return { error: `models[${index}].${key}: expected a boolean or null` };
    }
    model[key] = value;
  };
  if (raw.input !== undefined) {
    if (raw.input === null) {
      model.input = null;
    } else if (Array.isArray(raw.input)) {
      const valid = raw.input.every((v) => v === 'text' || v === 'image');
      if (!valid || raw.input.length === 0) return { error: `models[${index}].input: expected ["text"] or ["text","image"]` };
      // SAFETY: the every() check above proved each member is 'text' or
      // 'image', so the deduped array is a string[].
      model.input = [...new Set(raw.input)] as string[];
    } else {
      return { error: `models[${index}].input: expected an array or null` };
    }
  }
  if (raw.cost !== undefined) {
    if (raw.cost === null) {
      model.cost = null;
    } else if (isRecord(raw.cost)) {
      const cost: NormalizedModelCost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
      for (const key of ['input', 'output', 'cacheRead', 'cacheWrite'] as const) {
        const value = Number(raw.cost[key]);
        if (!Number.isFinite(value) || value < 0) return { error: `models[${index}].cost.${key}: expected a non-negative number` };
        cost[key] = value;
      }
      model.cost = cost;
    } else {
      return { error: `models[${index}].cost: expected an object or null` };
    }
  }
  managedOptionalBoolean('supportsTools');
  managedOptionalBoolean('omitMaxOutputTokens');
  managedOptionalString('contextPromotionTarget');
  managedOptionalString('compactionModel');
  managedOptionalString('baseUrl');
  if (raw.api !== undefined) {
    if (typeof raw.api !== 'string' || !OMP_PROVIDER_APIS.includes(raw.api)) return { error: `models[${index}].api: unsupported protocol` };
    model.api = raw.api;
  }
  if (raw.thinking !== undefined) {
    if (raw.thinking === null) {
      model.thinking = null;
    } else if (isRecord(raw.thinking)) {
      const efforts = Array.isArray(raw.thinking.efforts) ? raw.thinking.efforts : null;
      if (efforts !== null && efforts.some((effort) => typeof effort !== 'string' || !effort.trim())) {
        return { error: `models[${index}].thinking.efforts: expected non-empty strings` };
      }
      model.thinking = {
        ...(typeof raw.thinking.mode === 'string' ? { mode: raw.thinking.mode } : {}),
        // SAFETY: the some() guard above rejected every non-string or
        // blank effort, so efforts is a string[] through and through.
        ...(efforts !== null && efforts.length > 0 ? { efforts: (efforts as string[]).map((effort) => effort.trim()) } : {}),
        ...(typeof raw.thinking.defaultLevel === 'string' && raw.thinking.defaultLevel ? { defaultLevel: raw.thinking.defaultLevel } : {}),
      };
    } else {
      return { error: `models[${index}].thinking: expected an object or null` };
    }
  }
  for (const key of ['contextWindow', 'maxTokens'] as const) {
    if (raw[key] !== undefined) {
      const value = Number(raw[key]);
      if (!Number.isFinite(value) || value <= 0) return { error: `models[${index}].${key}: expected a positive number` };
      model[key] = Math.round(value);
    }
  }
  return { model };
};
// Apply GUI-managed model fields onto a model map node. Shared by the update
// (field-merge) and create paths so collection values (input/cost/thinking)
// always become real YAML collection nodes: a Scalar wrapping an array or
// object resolves no tag and the whole write throws
// "Tag not resolved for Array value".
/** GUI 管理的模型标量键清单：applyManagedModelFields 按它做统一的 set/delete 循环。 */
const MANAGED_MODEL_SCALAR_KEYS = [
  'name', 'reasoning', 'contextWindow', 'maxTokens', 'baseUrl', 'api',
  'supportsTools', 'omitMaxOutputTokens', 'contextPromotionTarget', 'compactionModel',
];

/**
 * 把 GUI 管理的模型字段施加到 YAML 模型节点上（更新与新建共用）：
 * undefined 保持原值、null 删除键（绝不写字面 null）、其余写为 Scalar；
 * input/cost/thinking 一律构造成真正的 YAML 集合节点——包着数组/对象的
 * Scalar 解析不出 tag，整个写入会以 "Tag not resolved for Array value" 失败。
 */
const applyManagedModelFields = (modelNode: YAMLMap, incoming: NormalizedIncomingModel) => {
  // SAFETY: MANAGED_MODEL_SCALAR_KEYS names exactly the NormalizedIncomingModel scalar members.
  const managedKeys = MANAGED_MODEL_SCALAR_KEYS as Array<keyof NormalizedIncomingModel>;
  for (const key of managedKeys) {
    if (incoming[key] === undefined) continue;
    // `null` clears the key (documented contract) — a literal null is
    // never written: the engine schema rejects any null model value by
    // dropping the WHOLE models.yml (every custom provider disappears).
    if (incoming[key] === null) modelNode.delete(key);
    else modelNode.set(new Scalar(key), new Scalar(incoming[key]));
  }
  if (incoming.input === undefined) {
    // keep
  } else if (incoming.input === null) {
    modelNode.delete('input');
  } else {
    const inputSeq = new YAMLSeq();
    for (const item of incoming.input) inputSeq.items.push(new Scalar(item));
    modelNode.set(new Scalar('input'), inputSeq);
  }
  if (incoming.cost === undefined) {
    // keep
  } else if (incoming.cost === null) {
    modelNode.delete('cost');
  } else {
    const costMap = new YAMLMap();
    for (const [key, value] of Object.entries(incoming.cost)) {
      costMap.set(new Scalar(key), new Scalar(value));
    }
    modelNode.set(new Scalar('cost'), costMap);
  }
  if (incoming.thinking !== undefined) {
    const efforts = Array.isArray(incoming.thinking?.efforts) ? incoming.thinking.efforts : [];
    // The schema requires efforts (or legacy ranges) — an emptied
    // thinking config removes the block instead of writing an invalid one.
    if (incoming.thinking === null || efforts.length === 0) {
      modelNode.delete('thinking');
    } else {
      // Update in place when a thinking block exists: mode/efforts/defaultLevel
      // are GUI-managed, but keys the dialog never shows (effortMap,
      // supportsDisplay) and their comments survive, and the canonical
      // efforts retire the legacy range vocabulary they replace.
      const priorThinking = modelNode.get('thinking');
      const thinkingNode = isMap(priorThinking) ? priorThinking : new YAMLMap();
      thinkingNode.delete('minLevel');
      thinkingNode.delete('maxLevel');
      thinkingNode.delete('levels');
      const defaultLevel = typeof incoming.thinking.defaultLevel === 'string' && incoming.thinking.defaultLevel
        ? incoming.thinking.defaultLevel : null;
      if (!defaultLevel) thinkingNode.delete('defaultLevel');
      thinkingNode.set(new Scalar('mode'), new Scalar(typeof incoming.thinking.mode === 'string' ? incoming.thinking.mode : 'effort'));
      const effortsSeq = new YAMLSeq();
      for (const effort of efforts) effortsSeq.items.push(new Scalar(effort));
      thinkingNode.set(new Scalar('efforts'), effortsSeq);
      if (defaultLevel) thinkingNode.set(new Scalar('defaultLevel'), new Scalar(defaultLevel));
      modelNode.set(new Scalar('thinking'), thinkingNode);
    }
  }
};


/**
 * Validate + merge + write one provider into models.yml.
 *
 * @param {{ provider: {
 *   id: string,
 *   baseUrl?: string, api?: string,
 *   apiKey?: string | null, authHeader?: boolean | null,
 *   headers?: Record<string, string> | null,
 *   models?: Array<object> | null,
 * }, }} input
 * @param {{ modelsPath?: string, listEngineModels?: () => Array<{provider: string}>, refreshModels?: () => Promise<void>, now?: () => number }} [options]
 */
/**
 * PUT /omp/providers 的实现：校验 + 字段级合并 + 原子写入单个 provider 到
 * models.yml。流程：载荷逐字段 400 校验 → 读取保留注释的 Document → origin
 * 守卫（新 id 撞引擎 provider 回 409）→ 在既有 YAMLMap 上外科手术式合并
 * （undefined 跳过、null 删键）→ null 清扫（引擎 schema 见 null 即丢弃整份
 * models.yml）→ 整文件 schema + validateProviderConfiguration 校验 → 一次性
 * backup → 临时文件 + rename 原子落盘 → 可选 refreshModels 热刷新。
 * 成功返回 200 与合并后的 provider 投影（不含 apiKey）。
 */
export const putOmpProvider = async (input: PutOmpProviderInput, options: OmpProviderWriteOptions = {}): Promise<OmpProviderRouteResult> => {
  const modelsPath = options.modelsPath ?? defaultModelsPath();
  const provider = input?.provider;
  if (!isRecord(provider)) {
    return { status: 400, body: { error: 'validation', message: 'provider object is required' } };
  }
  const id = typeof provider.id === 'string' ? provider.id.trim() : '';
  if (!PROVIDER_ID_PATTERN.test(id)) {
    return { status: 400, body: { error: 'validation', message: 'provider.id must match [a-z0-9][a-z0-9-_]*' } };
  }
  if (provider.api !== undefined && (typeof provider.api !== 'string' || !OMP_PROVIDER_APIS.includes(provider.api))) {
    return { status: 400, body: { error: 'validation', message: `provider.api must be one of: ${OMP_PROVIDER_APIS.join(', ')}` } };
  }
  if (provider.baseUrl !== undefined && (typeof provider.baseUrl !== 'string' || !/^https?:\/\//.test(provider.baseUrl.trim()))) {
    return { status: 400, body: { error: 'validation', message: 'provider.baseUrl must be an http(s) URL' } };
  }
  if (provider.headers !== undefined && provider.headers !== null) {
    if (!isRecord(provider.headers)
      || Object.values(provider.headers).some((v) => v !== null && typeof v !== 'string' && typeof v !== 'number' && typeof v !== 'boolean')) {
      return { status: 400, body: { error: 'validation', message: 'provider.headers must be a string record (scalar values are stringified)' } };
    }
  }

  const incomingModels: NormalizedIncomingModel[] = [];
  if (provider.models !== undefined && provider.models !== null) {
    if (!Array.isArray(provider.models)) {
      return { status: 400, body: { error: 'validation', message: 'provider.models must be an array' } };
    }
    const seen = new Set<string>();
    for (let index = 0; index < provider.models.length; index += 1) {
      const result = normalizeIncomingModel(provider.models[index], index);
      if (result.error !== undefined) {
        return { status: 400, body: { error: 'validation', message: result.error } };
      }
      const { model } = result;
      if (seen.has(model.id)) {
        return { status: 400, body: { error: 'validation', message: `models: duplicate id ${model.id}` } };
      }
      seen.add(model.id);
      incomingModels.push(model);
    }
  }

  const { doc, existed } = readDocument(modelsPath);
  const providersMap = providersMapOf(doc);
  const existingNodeCandidate = providersMap.get(id);
  const existingNode = isMap(existingNodeCandidate) ? existingNodeCandidate : null;
  const existing = existingNode ? plainValue(doc, existingNode) : null;

  // Origin guard: never shadow a builtin/login provider the engine already
  // serves from somewhere other than this file.
  if (!existing && typeof options.listEngineModels === 'function') {
    let engineIds = new Set<string>();
    try {
      engineIds = new Set((options.listEngineModels() ?? []).map((m) => m?.provider).filter(Boolean));
    } catch {
      // Engine unavailable → file-only check; the refresh below still validates.
    }
    if (engineIds.has(id)) {
      return { status: 409, body: { error: 'provider-exists-engine', message: `provider ${id} already exists as an engine (builtin/login) provider` } };
    }
  }

  // ── merge onto the YAML node (comment-preserving, key-surgical) ──
  let target: YAMLMap | null = existingNode;
  if (!target) {
    target = new YAMLMap();
    providersMap.set(new Scalar(id), target);
  }
  const baseUrlValue = typeof provider.baseUrl === 'string' ? provider.baseUrl : undefined;
  const apiKeyValue = typeof provider.apiKey === 'string' ? provider.apiKey : undefined;
  if (baseUrlValue !== undefined) target.set(new Scalar('baseUrl'), new Scalar(baseUrlValue.trim()));
  if (provider.api !== undefined) target.set(new Scalar('api'), new Scalar(provider.api));
  if (provider.apiKey === null) target.delete('apiKey');
  else if (apiKeyValue !== undefined) target.set(new Scalar('apiKey'), new Scalar(apiKeyValue.trim()));
  if (provider.authHeader === null) target.delete('authHeader');
  else if (provider.authHeader !== undefined) target.set(new Scalar('authHeader'), new Scalar(provider.authHeader));
  if (provider.headers === null) target.delete('headers');
  else if (provider.headers !== undefined) {
    const headersMap = new YAMLMap();
    for (const [key, value] of Object.entries(provider.headers)) {
      // Scalars are stringified on write: the engine's models.yml schema
      // rejects non-string header values by dropping the whole config.
      headersMap.set(new Scalar(key), new Scalar(typeof value === 'string' ? value : String(value)));
    }
    target.set(new Scalar('headers'), headersMap);
  }

  if (provider.models !== undefined && provider.models !== null) {
    const mergedModels: YAMLMap[] = [];
    const modelsNode = target.get('models');
    const existingModels = isSeq(modelsNode) ? modelsNode : null;
    const existingById = new Map<string, YAMLMap>();
    if (existingModels) {
      for (const item of existingModels.items) {
        const value = isNode(item) ? plainValue(doc, item) : null;
        if (isRecord(value) && typeof value.id === 'string' && isMap(item)) existingById.set(value.id, item);
      }
    }
    for (const incoming of incomingModels) {
      const priorNode = existingById.get(incoming.id);
      const isUpdate = Boolean(priorNode && isMap(priorNode));
      // Update only GUI-managed keys; cost/input/compat/… survive. The
      // thinking block is GUI-managed via the model dialog (efforts +
      // defaultLevel) and replaces/removes the prior block when provided.
      // New models run the same applier over a fresh map so every value
      // lands as the right node kind.
      const modelNode = isUpdate && priorNode ? priorNode : new YAMLMap();
      if (!isUpdate) modelNode.set(new Scalar('id'), new Scalar(incoming.id));
      applyManagedModelFields(modelNode, incoming);
      mergedModels.push(modelNode);
    }
    const seq = new YAMLSeq();
    for (const node of mergedModels) seq.items.push(node);
    target.set(new Scalar('models'), seq);
  }
  // ── null sweep: the engine schema rejects ANY null value in a provider or
  // model entry by dropping the whole models.yml (every custom provider
  // disappears). Hand-authored nulls and any future null-leaking path are
  // removed before the write instead of shipping a file the engine refuses.
  // 就地删除映射节点中值为 null 的键值对（见上方 null 清扫说明）。
  const stripNullEntries = (mapNode: Node | null | undefined) => {
    if (!isMap(mapNode)) return;
    for (const pair of [...mapNode.items]) {
      if (pair.value == null || (pair.value instanceof Scalar && pair.value.value === null)) {
        mapNode.delete(pair.key);
      }
    }
  };
  stripNullEntries(target);
  const modelsNodeAfterMerge = target.get('models');
  if (isSeq(modelsNodeAfterMerge)) {
    for (const item of modelsNodeAfterMerge.items) {
      if (isNode(item)) stripNullEntries(item);
    }
  }

  // ── validate the resulting whole-file value BEFORE touching disk ──
  const mergedFileValue = documentJsonValue(doc);
  const schemaError = schemaProblems(mergedFileValue);
  const mergedProvider = plainValue(doc, target);
  const merged = isRecord(mergedProvider) ? mergedProvider : {};
  try {
    // SAFETY: the whole-file schema check above passed, so `merged`
    // already satisfies the SDK's parsed models.yml provider shape; the
    // validation call below reads its fields unchanged.
    const mergedEntry = merged as ParsedProviderEntry;
    validateProviderConfiguration(id, {
      baseUrl: mergedEntry.baseUrl,
      headers: mergedEntry.headers,
      apiKey: mergedEntry.apiKey,
      api: mergedEntry.api,
      auth: mergedEntry.auth,
      models: mergedEntry.models ?? [],
    }, 'models-config');
  } catch (error) {
    return { status: 400, body: { error: 'validation', message: errorText(error) } };
  }

  // ── write (one-time backup anchor, atomic replace) ──
  if (existed) {
    const backupPath = `${modelsPath}.backup`;
    if (!fs.existsSync(backupPath)) {
      try {
        fs.copyFileSync(modelsPath, backupPath);
      } catch {
        // Backup is best-effort recovery sugar, never a write blocker.
      }
    }
  }
  const serialized = doc.toString({ lineWidth: 0 });
  const temp = `${modelsPath}.${process.pid}.tmp`;
  fs.mkdirSync(path.dirname(modelsPath), { recursive: true });
  fs.writeFileSync(temp, serialized, 'utf8');
  fs.renameSync(temp, modelsPath);

  if (typeof options.refreshModels === 'function') {
    try {
      await options.refreshModels();
    } catch {
      // The file is written; a refresh failure must not fail the PUT (the
      // registry reloads on its own mtime check with the next refresh).
    }
  }

  return { status: 200, body: { provider: projectFileProvider(id, merged) } };
};

// ─────────────────────────────────────────────────────────────────────────────
// DELETE
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Remove a file-defined provider. Engine (builtin/login) providers and
 * unknown ids never delete.
 *
 * @param {{ id: string }} input
 * @param {{ modelsPath?: string, listEngineModels?: () => Array<{provider: string}>, refreshModels?: () => Promise<void> }} [options]
 */
/**
 * DELETE /omp/providers/{id} 的实现：删除 models.yml 中的 file provider。
 * id 缺失 400；文件中不存在 404（引擎 provider 本就不在文件里，天然不可删）；
 * 删除后的整文件 schema 校验失败时拒绝落盘（500）；写入同样走临时文件 +
 * rename 原子替换，成功后可选 refreshModels 热刷新（失败不影响结果）。
 */
export const deleteOmpProvider = async (input: DeleteOmpProviderInput, options: OmpProviderDeleteOptions = {}): Promise<OmpProviderRouteResult> => {
  const modelsPath = options.modelsPath ?? defaultModelsPath();
  const id = typeof input?.id === 'string' ? input.id.trim() : '';
  if (!id) return { status: 400, body: { error: 'validation', message: 'provider id is required' } };

  const { doc } = readDocument(modelsPath);
  const providersMap = doc.get('providers');
  if (!providersMap || !isMap(providersMap) || !providersMap.has(id)) {
    return { status: 404, body: { error: 'not-found', message: `provider ${id} is not defined in models.yml` } };
  }

  providersMap.delete(id);
  const schemaError = schemaProblems(documentJsonValue(doc));
  if (schemaError) {
    return { status: 500, body: { error: 'invalid-result', message: `refusing to write an invalid models.yml: ${schemaError}` } };
  }
  const temp = `${modelsPath}.${process.pid}.tmp`;
  fs.writeFileSync(temp, doc.toString({ lineWidth: 0 }), 'utf8');
  fs.renameSync(temp, modelsPath);

  if (typeof options.refreshModels === 'function') {
    try {
      await options.refreshModels();
    } catch {
      // Same as PUT: the file write is the durable action.
    }
  }
  return { status: 200, body: { deleted: id } };
};

// ─────────────────────────────────────────────────────────────────────────────
// Fetch the provider's own model list ({baseUrl}/models, server-side)
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Cherry Studio / LobeChat "Fetch models" server half: the host queries the
 * provider's model-list endpoint with the file's baseUrl + apiKey (the
 * browser cannot — CORS + key exposure), returning plain model ids. The UI
 * merges them into its draft; nothing is written by this call.
 *
 * Only OpenAI-compatible list shapes are honored ({data:[{id}]} and flat
 * [{id}] arrays); anthropic/google APIs have no public list endpoint here.
 *
 * @param {{ id: string }} input
 * @param {{ modelsPath?: string, fetchImpl?: typeof fetch, now?: () => number }} [options]
 */
/**
 * POST /omp/providers/{id}/fetch-models 的实现："拉取模型列表"的服务端半边——
 * 由 host 用文件（或草稿覆盖）的 baseUrl + apiKey 请求 {baseUrl}/models 并返回
 * 纯模型 id 列表（浏览器受 CORS 与密钥暴露限制无法自己请求）；本调用不写任何
 * 状态。baseUrl 必须为 http(s)（否则 400，防止 server-side fetch 读本地文件）；
 * 请求失败/非 2xx/无法识别的载荷一律 502，绝不把失败伪装成空列表。
 */
export const fetchOmpProviderModels = async (input: FetchOmpProviderModelsInput, options: FetchOmpProviderModelsOptions = {}): Promise<OmpProviderRouteResult> => {
  const modelsPath = options.modelsPath ?? defaultModelsPath();
  const fetchImpl = options.fetchImpl ?? fetch;
  const id = typeof input?.id === 'string' ? input.id.trim() : '';
  if (!id) return { status: 400, body: { error: 'validation', message: 'provider id is required' } };

  // Draft overrides: the create/edit form sends its current baseUrl/apiKey so
  // an UNSAVED provider can fetch models too (otherwise create is circular:
  // save needs a model, fetch needs a save). Overrides win over the file.
  const draftBaseUrl = typeof input?.baseUrl === 'string' ? input.baseUrl.trim() : '';
  const draftApiKey = typeof input?.apiKey === 'string' ? input.apiKey.trim() : '';

  const { doc } = readDocument(modelsPath);
  const node = doc.get('providers');
  const providerNode = node && YAML.isMap(node) ? node.get(id) : null;
  const value = isNode(providerNode) ? plainValue(doc, providerNode) : null;
  if (!isRecord(value) && !draftBaseUrl) {
    return { status: 404, body: { error: 'not-found', message: `provider ${id} is not defined in models.yml (and no draft baseUrl was provided)` } };
  }
  const fileValue = isRecord(value) ? value : null;
  const baseUrl = draftBaseUrl || (typeof fileValue?.baseUrl === 'string' ? fileValue.baseUrl.trim() : '');
  if (!baseUrl) {
    return { status: 400, body: { error: 'no-base-url', message: `provider ${id} has no baseUrl to fetch from` } };
  }
  // Same contract as PUT: http(s) only. The probe runs server-side, and
  // non-http schemes (Bun's fetch resolves file://) would turn this endpoint
  // into a local-file read.
  if (!/^https?:\/\//.test(baseUrl)) {
    return { status: 400, body: { error: 'validation', message: 'provider.baseUrl must be an http(s) URL' } };
  }
  const apiKey = draftApiKey || (typeof fileValue?.apiKey === 'string' ? fileValue.apiKey : '');

  const url = `${baseUrl.replace(/\/+$/, '')}/models`;
  let response;
  try {
    response = await fetchImpl(url, {
      method: 'GET',
      headers: {
        Accept: 'application/json',
        ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
      },
      signal: AbortSignal.timeout(15000),
    });
  } catch (error) {
    return { status: 502, body: { error: 'fetch-failed', message: `request to ${url} failed: ${errorText(error)}` } };
  }
  if (!response.ok) {
    return { status: 502, body: { error: 'fetch-failed', message: `${url} answered ${response.status}` } };
  }
  const payload: unknown = await response.json().catch((): null => null);
  // SAFETY: response.json() yields JSON; isRecord narrows the record arm and
  // its JsonRecord guard makes the narrowed read type-safe.
  const record = isRecord(payload as ProviderRuntimeValue | undefined) ? (payload as JsonRecord) : null;
  const data = record?.data;
  const list = Array.isArray(data) ? data : (Array.isArray(payload) ? payload : null);
  if (list === null) {
    // A 2xx with an unrecognized body is a failure, not an empty success
    // (sync-state-invariants: fetch failure must not masquerade as truth).
    return { status: 502, body: { error: 'fetch-failed', message: `${url} returned an unrecognized model-list payload` } };
  }
  const models = [...new Set(list
    .map((entry) => (isRecord(entry) && typeof entry.id === 'string' ? entry.id.trim() : ''))
    .filter((modelId) => modelId.length > 0))];
  return { status: 200, body: { models } };
};

// ─────────────────────────────────────────────────────────────────────────────
// Route mounting
// ─────────────────────────────────────────────────────────────────────────────

/** Parse a JSON route body into a plain record; parse failures and non-object
 * payloads collapse to `{}` so guarded field reads stay undefined. */
/** 把 JSON 请求体解析为普通 record；解析失败与非对象载荷坍缩为 {}，让受守卫的字段读取保持 undefined。 */
const routeJsonBody = async (request: Request): Promise<JsonRecord> => {
  // SAFETY: Request.json() parses JSON by construction, so its fulfillment
  // value is always a JsonValue; the catch collapses parse failures to null.
  const value = (await request.json().catch((): null => null)) as JsonValue | null;
  // SAFETY: isRecord's JsonRecord guard certifies the record arm.
  return isRecord(value as ProviderRuntimeValue | undefined) ? (value as JsonRecord) : {};
};

/**
 * Mount the /omp routes owned by this domain. Capability `providers.v1`
 * gates all routes (master R2).
 *
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {{ features?: Record<string, boolean>, modelsPath?: string, listEngineModels?: () => Array<{provider: string}>, refreshModels?: () => Promise<void> }} [options]
 */
/**
 * 挂载本领域拥有的 /omp 路由（GET/PUT/DELETE + fetch-models）。
 * 每条路由先过 providers.v1 能力开关（master R2），未开启时统一 501；
 * handler 只做请求体/路径参数到领域函数的转发与 Response 装配。
 */
export function registerProvidersDomainRoutes(
  route: ProvidersRouteMount,
  { features = ompFeatures(), modelsPath = defaultModelsPath(), listEngineModels, refreshModels }: ProvidersDomainDeps = {},
): void {
  // 能力开关包装器：providers.v1 未开启时直接回答 501，不再进入 handler。
  const gated = (handler: ProvidersRouteHandler): ProvidersRouteHandler => async (request, ctx) => {
    if (features?.['providers.v1'] !== true) return featureUnavailable('providers.v1');
    return handler(request, ctx);
  };

  route('GET', '/omp/providers', gated(async () => {
    return json(await listOmpProviders({ modelsPath, listEngineModels }));
  }));

  route('PUT', '/omp/providers', gated(async (request) => {
    const body = await routeJsonBody(request);
    const { status, body: payload } = await putOmpProvider(body, { modelsPath, listEngineModels, refreshModels });
    return json(payload, { status });
  }));

  route('POST', '/omp/providers/{id}/fetch-models', gated(async (request, ctx) => {
    const body = await routeJsonBody(request);
    const { status, body: payload } = await fetchOmpProviderModels(
      {
        id: ctx?.params?.id ?? '',
        baseUrl: typeof body.baseUrl === 'string' ? body.baseUrl : undefined,
        apiKey: typeof body.apiKey === 'string' ? body.apiKey : undefined,
      },
      { modelsPath },
    );
    return json(payload, { status });
  }));

  route('DELETE', '/omp/providers/{id}', gated(async (request, ctx) => {
    const { status, body: payload } = await deleteOmpProvider(
      { id: ctx?.params?.id ?? new URL(request.url).pathname.split('/').pop() },
      { modelsPath, listEngineModels, refreshModels },
    );
    return json(payload, { status });
  }));
}
