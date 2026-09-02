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

import fs from 'node:fs';
import path from 'node:path';
import YAML from 'yaml';
import { getAgentDir } from '@oh-my-pi/pi-coding-agent';
import {
  ModelsConfigFile,
  validateProviderConfiguration,
} from '@oh-my-pi/pi-coding-agent/config/models-config';
import { featureUnavailable, ompFeatures } from './omp-parity.ts';
import { YAMLMap, YAMLSeq, Scalar, isMap, isNode, isSeq, parseDocument, Document, type Node } from 'yaml';

/** What `Response.json` itself accepts — this helper only forwards to it. */
type ResponseJsonData = Parameters<typeof Response.json>[0];

const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

/**
 * JSON value models.yml and the provider wire carry (same shape contract as
 * domain-plugins.ts JsonValue).
 */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/** Open string-keyed JSON object: provider entries, wire bodies, and header
 * maps are keyed by ids/names neither side enumerates up front, so the record
 * stays intentionally open while its values remain concrete JSON. */
type JsonRecord = Record<string, JsonValue>;

/** Duck-typed surface of omptype's OmpErrors aggregate this module reads
 * (lazy .map over per-path problems — see schemaProblems). */
interface OmpErrorsAggregate {
  map: (fn: (error: { path?: PropertyKey[]; problem?: string }) => string) => string[];
}

/** Runtime value domain flowing through this module's shape guards: plain
 * JSON (models.yml values, provider wire bodies) plus the OmpErrors
 * aggregate an SDK schema call may answer. */
type ProviderRuntimeValue = JsonValue | OmpErrorsAggregate;

const PROVIDER_ID_PATTERN = /^[a-z0-9][a-z0-9-_]*$/;

/** API values accepted at provider level (models-config-schema ApiSchema). */
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

const defaultModelsPath = () => path.join(getAgentDir(), 'models.yml');

// ─────────────────────────────────────────────────────────────────────────────
// Named contracts (write-path inputs/options, projections, route mounting)
// ─────────────────────────────────────────────────────────────────────────────

/** Engine model handle this module consumes (origin tags + collision guard). */
export interface OmpEngineModelRef {
  provider: string;
}

export type ListEngineModels = () => Array<OmpEngineModelRef>;

export type RefreshModels = () => Promise<void>;

export interface OmpListProvidersOptions {
  modelsPath?: string;
  listEngineModels?: ListEngineModels;
}

export interface OmpProviderWriteOptions {
  modelsPath?: string;
  listEngineModels?: ListEngineModels;
  refreshModels?: RefreshModels;
}

export interface OmpProviderDeleteOptions {
  modelsPath?: string;
  listEngineModels?: ListEngineModels;
  refreshModels?: RefreshModels;
}

export interface FetchOmpProviderModelsOptions {
  modelsPath?: string;
  /** Callable subset of fetch; @types/node 24 requires preconnect on typeof fetch. */
  fetchImpl?: (url: string, init?: RequestInit) => Promise<Response>;
}

/** PUT input — unvalidated wire payload; every field is re-checked before use. */
export interface PutOmpProviderInput {
  provider?: JsonValue;
}

export interface DeleteOmpProviderInput {
  id?: string;
}

export interface FetchOmpProviderModelsInput {
  id?: string;
  baseUrl?: string;
  apiKey?: string;
}

/** GET /omp/providers model projection (edit-prefill subset). */
export interface OmpProjectedModelCost {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

export interface OmpProjectedModelThinking {
  efforts?: string[];
  defaultLevel?: string;
}

export interface OmpProjectedModel {
  id: string;
  name?: string;
  reasoning?: boolean;
  contextWindow?: number;
  maxTokens?: number;
  input?: string[];
  supportsTools?: boolean;
  omitMaxOutputTokens?: boolean;
  cost?: OmpProjectedModelCost;
  baseUrl?: string;
  api?: string;
  contextPromotionTarget?: string;
  compactionModel?: string;
  thinking?: OmpProjectedModelThinking;
}

/** File-defined provider (models.yml): editable, key presence only. */
export interface OmpFileProviderProjection {
  id: string;
  source: 'file';
  baseUrl?: string;
  api?: string;
  authHeader?: boolean;
  headers?: Record<string, string>;
  hasApiKey: boolean;
  models: OmpProjectedModel[];
}

/** Engine (builtin/login) provider: read-only listing entry. */
export interface OmpEngineProviderProjection {
  id: string;
  source: 'engine';
  models: OmpProjectedModel[];
}

export type OmpListedProvider = OmpFileProviderProjection | OmpEngineProviderProjection;

export interface OmpProviderListResult {
  modelsPath: string;
  providers: OmpListedProvider[];
}

/** Uniform answer envelope for the write endpoints. */
export interface OmpProviderRouteBody {
  error?: string;
  message?: string;
  provider?: OmpFileProviderProjection;
  deleted?: string;
  models?: string[];
}

export interface OmpProviderRouteResult {
  status: number;
  body: OmpProviderRouteBody;
}

export interface ProvidersRouteContext {
  params: Record<string, string>;
}

export type ProvidersRouteHandler = (request: Request, ctx?: ProvidersRouteContext) => Response | Promise<Response>;

export type ProvidersRouteMount = (method: string, pattern: string, handler: ProvidersRouteHandler) => void;

export interface ProvidersDomainDeps {
  features?: Record<string, boolean>;
  modelsPath?: string;
  listEngineModels?: ListEngineModels;
  refreshModels?: RefreshModels;
}

// ─────────────────────────────────────────────────────────────────────────────
// File reading (comment-preserving document)
// ─────────────────────────────────────────────────────────────────────────────

const readDocument = (modelsPath: string) => {
  let raw = '';
  try {
    raw = fs.readFileSync(modelsPath, 'utf8');
  } catch (error) {
    if (error?.code === 'ENOENT') {
      return { doc: new Document(), existed: false };
    }
    throw error;
  }
  // models.yml is OMPChamber-authored state, not untrusted input; the yaml
  // billion-laughs alias-count guard stays off so large hand-maintained files
  // using anchors/aliases still resolve (-1 = guard off, enforced at toJS).
  return { doc: parseDocument(raw, { merge: false }), existed: true };
};

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
const isOmpErrorsAggregate = (value: ProviderRuntimeValue | undefined): value is OmpErrorsAggregate =>
  typeof value === 'object' && value !== null && 'map' in value && typeof value.map === 'function';

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
type ParsedModelsFile = ReturnType<typeof ModelsConfigFile.loadOrDefault>;
/** One provider entry in a parsed models.yml (schema-derived). */
type ParsedProviderEntry = NonNullable<ParsedModelsFile['providers']>[string];

const isRecord = (value: ProviderRuntimeValue | undefined): value is JsonRecord =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

// Canonical effort vocabulary and order (models-config-schema EffortSchema /
// EFFORT_ORDER — not exported by the SDK, mirrored here).
const THINKING_EFFORT_ORDER = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/**
 * Efforts for UI prefill from canonical or legacy shapes. Mirrors
 * ModelThinkingSchema normalization (efforts beats levels beats the
 * minLevel..maxLevel range) so a hand-authored range block prefills the
 * dialog with the efforts the engine itself resolves — otherwise the dialog
 * shows an empty list and its save silently deletes the block.
 */
const deriveThinkingEfforts = (thinking) => {
  const list = Array.isArray(thinking.efforts) ? thinking.efforts
    : Array.isArray(thinking.levels) ? thinking.levels : null;
  if (list !== null) return list.filter((e) => THINKING_EFFORT_ORDER.includes(e));
  const min = THINKING_EFFORT_ORDER.indexOf(thinking.minLevel);
  const max = THINKING_EFFORT_ORDER.indexOf(thinking.maxLevel);
  return min >= 0 && max >= min ? THINKING_EFFORT_ORDER.slice(min, max + 1) : [];
};
/**
 * Header values must be strings: the engine's models.yml schema rejects the
 * whole config on a non-string value, and the wire/UI contracts are
 * string-only. Hand-authored YAML may carry scalars (`X-Request-Id: 42`);
 * coerce those to their string form and drop anything structural, so one
 * loose value never blanks the provider list (plan P15).
 */
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
const projectFileProvider = (id: string, value: JsonRecord): OmpFileProviderProjection => {
  const models = Array.isArray(value.models) ? value.models : [];
  const headers = stringHeaders(value.headers);
  return {
    id,
    source: 'file',
    ...(typeof value.baseUrl === 'string' ? { baseUrl: value.baseUrl } : {}),
    ...(value.authHeader !== undefined ? { authHeader: Boolean(value.authHeader) } : {}),
    ...(Object.keys(headers).length > 0 ? { headers } : {}),
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
interface NormalizedModelCost {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

interface NormalizedModelThinking {
  mode?: string;
  efforts?: string[];
  defaultLevel?: string;
}

interface NormalizedIncomingModel {
  id: string;
  name?: string;
  reasoning?: boolean;
  input?: string[] | null;
  cost?: NormalizedModelCost | null;
  supportsTools?: boolean | null;
  omitMaxOutputTokens?: boolean | null;
  contextPromotionTarget?: string | null;
  compactionModel?: string | null;
  baseUrl?: string | null;
  api?: string;
  thinking?: NormalizedModelThinking | null;
  contextWindow?: number;
  maxTokens?: number;
}

type NormalizedModelResult = { model: NormalizedIncomingModel; error?: undefined } | { error: string; model?: undefined };

type ManagedStringKey = 'contextPromotionTarget' | 'compactionModel' | 'baseUrl';
type ManagedBooleanKey = 'supportsTools' | 'omitMaxOutputTokens';

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
  const managedOptionalString = (key: ManagedStringKey) => {
    const value = raw[key];
    if (value === undefined) return;
    if (value === null) { model[key] = null; return; }
    if (typeof value !== 'string' || !value.trim()) {
      return { error: `models[${index}].${key}: expected a non-empty string or null` };
    }
    model[key] = value.trim();
  };
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
const MANAGED_MODEL_SCALAR_KEYS = [
  'name', 'reasoning', 'contextWindow', 'maxTokens', 'baseUrl', 'api',
  'supportsTools', 'omitMaxOutputTokens', 'contextPromotionTarget', 'compactionModel',
];

const applyManagedModelFields = (modelNode: YAMLMap, incoming: NormalizedIncomingModel) => {
  for (const key of MANAGED_MODEL_SCALAR_KEYS) {
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
      const { model, error } = normalizeIncomingModel(provider.models[index], index);
      if (error) return { status: 400, body: { error: 'validation', message: error } };
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
    return { status: 400, body: { error: 'validation', message: error?.message ?? String(error) } };
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
    return { status: 502, body: { error: 'fetch-failed', message: `request to ${url} failed: ${error?.message ?? error}` } };
  }
  if (!response.ok) {
    return { status: 502, body: { error: 'fetch-failed', message: `${url} answered ${response.status}` } };
  }
  const payload = await response.json().catch(() => null);
  const data = isRecord(payload) ? payload.data : undefined;
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
const routeJsonBody = async (request: Request): Promise<JsonRecord> => {
  // SAFETY: Request.json() parses JSON by construction, so its fulfillment
  // value is always a JsonValue; the catch collapses parse failures to null.
  const value = (await request.json().catch(() => null)) as JsonValue | null;
  return isRecord(value) ? value : {};
};

/**
 * Mount the /omp routes owned by this domain. Capability `providers.v1`
 * gates all routes (master R2).
 *
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {{ features?: Record<string, boolean>, modelsPath?: string, listEngineModels?: () => Array<{provider: string}>, refreshModels?: () => Promise<void> }} [options]
 */
export function registerProvidersDomainRoutes(
  route: ProvidersRouteMount,
  { features = ompFeatures(), modelsPath = defaultModelsPath(), listEngineModels, refreshModels }: ProvidersDomainDeps = {},
): void {
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
