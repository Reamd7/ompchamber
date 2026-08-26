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
import { featureUnavailable, ompFeatures } from './omp-parity.js';
import { YAMLMap, YAMLSeq, Scalar, isMap, isSeq, parseDocument, Document } from 'yaml';

const json = (data, init) => Response.json(data, init);

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
// File reading (comment-preserving document)
// ─────────────────────────────────────────────────────────────────────────────

const readDocument = (modelsPath) => {
  let raw = '';
  try {
    raw = fs.readFileSync(modelsPath, 'utf8');
  } catch (error) {
    if (error?.code === 'ENOENT') {
      return { doc: new Document(), existed: false };
    }
    throw error;
  }
  // models.yml is OMPChamber-authored state, not untrusted input; disable
  // the yaml billion-laughs alias-count guard so large hand-maintained
  // files using anchors/aliases still parse (-1 = guard off).
  return { doc: parseDocument(raw, { merge: false, maxAliasCount: -1 }), existed: true };
};

const providersMapOf = (doc) => {
  let providers = doc.get('providers');
  if (!providers || !isMap(providers)) {
    providers = new YAMLMap();
    doc.set(new Scalar('providers'), providers);
  }
  return providers;
};


/**
 * omptype schema calls return the parsed value on success or an OmpErrors
 * aggregate on failure (NOT an Error instance) — distinguish by shape and
 * surface the per-path problems; null means valid.
 */
const schemaProblems = (check) => {
  if (isRecord(check)) return null;
  if (typeof check?.map === 'function') {
    return check.map((error) => `${(error?.path ?? []).join('.') || 'root'}: ${error?.problem ?? 'invalid'}`).join('; ');
  }
  return check?.message ?? String(check);
};
const plainValue = (doc, node) => (node == null ? null : node.toJS(doc, { maxAliasCount: -1 }));

const isRecord = (value) => typeof value === 'object' && value !== null && !Array.isArray(value);

/** Projected file provider for GET / edit prefill. apiKey never leaves this
 * module — only `hasApiKey`. */
const projectFileProvider = (id, value) => {
  const models = Array.isArray(value.models) ? value.models : [];
  return {
    id,
    source: 'file',
    ...(typeof value.baseUrl === 'string' ? { baseUrl: value.baseUrl } : {}),
    ...(typeof value.api === 'string' ? { api: value.api } : {}),
    ...(value.authHeader !== undefined ? { authHeader: Boolean(value.authHeader) } : {}),
    ...(isRecord(value.headers) ? { headers: value.headers } : {}),
    hasApiKey: typeof value.apiKey === 'string' && value.apiKey.length > 0,
    models: models
      .filter((model) => isRecord(model) && typeof model.id === 'string')
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
        ...(typeof model.compactionModel === 'string' ? { compactionModel: model.compactionModel } : {}),
        ...(isRecord(model.thinking)
          ? {
              thinking: {
                ...(Array.isArray(model.thinking.efforts) ? { efforts: model.thinking.efforts.filter((e) => typeof e === 'string') } : {}),
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
export const listOmpProviders = async ({ modelsPath = defaultModelsPath(), listEngineModels } = {}) => {
  const engineIds = new Set();
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
  const providers = [];
  const fileIds = new Set();
  const fileNode = doc.get('providers');
  if (fileNode && isMap(fileNode)) {
    for (const pair of fileNode.items) {
      const id = String(pair.key?.value ?? '');
      if (!id) continue;
      const value = plainValue(doc, pair.value);
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

const normalizeIncomingModel = (raw, index) => {
  if (!isRecord(raw)) return { error: `models[${index}]: expected an object` };
  const id = typeof raw.id === 'string' ? raw.id.trim() : '';
  if (!id) return { error: `models[${index}]: id is required` };
  const model = { id };
  if (raw.name !== undefined) {
    if (typeof raw.name !== 'string' || !raw.name.trim()) return { error: `models[${index}].name: expected a non-empty string` };
    model.name = raw.name.trim();
  }
  if (raw.reasoning !== undefined) {
    if (typeof raw.reasoning !== 'boolean') return { error: `models[${index}].reasoning: expected a boolean` };
    model.reasoning = raw.reasoning;
  }
  const managedOptionalString = (key) => {
    if (raw[key] === undefined) return;
    if (raw[key] === null) { model[key] = null; return; }
    if (typeof raw[key] !== 'string' || !raw[key].trim()) {
      return { error: `models[${index}].${key}: expected a non-empty string or null` };
    }
    model[key] = raw[key].trim();
  };
  const managedOptionalBoolean = (key) => {
    if (raw[key] === undefined) return;
    if (raw[key] === null) { model[key] = null; return; }
    if (typeof raw[key] !== 'boolean') {
      return { error: `models[${index}].${key}: expected a boolean or null` };
    }
    model[key] = raw[key];
  };
  if (raw.input !== undefined) {
    if (raw.input === null) {
      model.input = null;
    } else if (Array.isArray(raw.input)) {
      const valid = raw.input.every((v) => v === 'text' || v === 'image');
      if (!valid || raw.input.length === 0) return { error: `models[${index}].input: expected ["text"] or ["text","image"]` };
      model.input = [...new Set(raw.input)];
    } else {
      return { error: `models[${index}].input: expected an array or null` };
    }
  }
  if (raw.cost !== undefined) {
    if (raw.cost === null) {
      model.cost = null;
    } else if (isRecord(raw.cost)) {
      const cost = {};
      for (const key of ['input', 'output', 'cacheRead', 'cacheWrite']) {
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
    if (!OMP_PROVIDER_APIS.includes(raw.api)) return { error: `models[${index}].api: unsupported protocol` };
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
        ...(efforts !== null && efforts.length > 0 ? { efforts: efforts.map((effort) => effort.trim()) } : {}),
        ...(typeof raw.thinking.defaultLevel === 'string' && raw.thinking.defaultLevel ? { defaultLevel: raw.thinking.defaultLevel } : {}),
      };
    } else {
      return { error: `models[${index}].thinking: expected an object or null` };
    }
  }
  for (const key of ['contextWindow', 'maxTokens']) {
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

const applyManagedModelFields = (modelNode, incoming) => {
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
      const thinkingNode = new YAMLMap();
      thinkingNode.set(new Scalar('mode'), new Scalar(typeof incoming.thinking.mode === 'string' ? incoming.thinking.mode : 'effort'));
      const effortsSeq = new YAMLSeq();
      for (const effort of efforts) effortsSeq.items.push(new Scalar(effort));
      thinkingNode.set(new Scalar('efforts'), effortsSeq);
      if (typeof incoming.thinking.defaultLevel === 'string' && incoming.thinking.defaultLevel) {
        thinkingNode.set(new Scalar('defaultLevel'), new Scalar(incoming.thinking.defaultLevel));
      }
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
export const putOmpProvider = async (input, options = {}) => {
  const modelsPath = options.modelsPath ?? defaultModelsPath();
  const provider = input?.provider;
  if (!isRecord(provider)) {
    return { status: 400, body: { error: 'validation', message: 'provider object is required' } };
  }
  const id = typeof provider.id === 'string' ? provider.id.trim() : '';
  if (!PROVIDER_ID_PATTERN.test(id)) {
    return { status: 400, body: { error: 'validation', message: 'provider.id must match [a-z0-9][a-z0-9-_]*' } };
  }
  if (provider.baseUrl !== undefined && (typeof provider.baseUrl !== 'string' || !/^https?:\/\//.test(provider.baseUrl.trim()))) {
    return { status: 400, body: { error: 'validation', message: 'provider.baseUrl must be an http(s) URL' } };
  }
  if (provider.api !== undefined && !OMP_PROVIDER_APIS.includes(provider.api)) {
    return { status: 400, body: { error: 'validation', message: `provider.api must be one of: ${OMP_PROVIDER_APIS.join(', ')}` } };
  }
  if (provider.apiKey !== undefined && provider.apiKey !== null && (typeof provider.apiKey !== 'string' || !provider.apiKey.trim())) {
    return { status: 400, body: { error: 'validation', message: 'provider.apiKey must be a non-empty string or null' } };
  }
  if (provider.authHeader !== undefined && provider.authHeader !== null && typeof provider.authHeader !== 'boolean') {
    return { status: 400, body: { error: 'validation', message: 'provider.authHeader must be a boolean or null' } };
  }
  if (provider.headers !== undefined && provider.headers !== null) {
    if (!isRecord(provider.headers) || Object.values(provider.headers).some((v) => typeof v !== 'string')) {
      return { status: 400, body: { error: 'validation', message: 'provider.headers must be a string record' } };
    }
  }

  const incomingModels = [];
  if (provider.models !== undefined && provider.models !== null) {
    if (!Array.isArray(provider.models)) {
      return { status: 400, body: { error: 'validation', message: 'provider.models must be an array' } };
    }
    const seen = new Set();
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
  const existingNode = isMap(providersMap.get(id)) ? providersMap.get(id) : null;
  const existing = existingNode ? plainValue(doc, existingNode) : null;

  // Origin guard: never shadow a builtin/login provider the engine already
  // serves from somewhere other than this file.
  if (!existing && typeof options.listEngineModels === 'function') {
    let engineIds = new Set();
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
  let target = existingNode;
  if (!target) {
    target = new YAMLMap();
    providersMap.set(new Scalar(id), target);
  }
  if (provider.baseUrl !== undefined) target.set(new Scalar('baseUrl'), new Scalar(provider.baseUrl.trim()));
  if (provider.api !== undefined) target.set(new Scalar('api'), new Scalar(provider.api));
  if (provider.apiKey === null) target.delete('apiKey');
  else if (provider.apiKey !== undefined) target.set(new Scalar('apiKey'), new Scalar(provider.apiKey.trim()));
  if (provider.authHeader === null) target.delete('authHeader');
  else if (provider.authHeader !== undefined) target.set(new Scalar('authHeader'), new Scalar(provider.authHeader));
  if (provider.headers === null) target.delete('headers');
  else if (provider.headers !== undefined) {
    const headersMap = new YAMLMap();
    for (const [key, value] of Object.entries(provider.headers)) {
      headersMap.set(new Scalar(key), new Scalar(value));
    }
    target.set(new Scalar('headers'), headersMap);
  }

  if (provider.models !== undefined && provider.models !== null) {
    const mergedModels = [];
    const existingModels = isSeq(target.get('models')) ? target.get('models') : null;
    const existingById = new Map();
    if (existingModels) {
      for (const item of existingModels.items) {
        const value = plainValue(doc, item);
        if (isRecord(value) && typeof value.id === 'string') existingById.set(value.id, item);
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
      const modelNode = isUpdate ? priorNode : new YAMLMap();
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
  const stripNullEntries = (mapNode) => {
    if (!isMap(mapNode)) return;
    for (const pair of [...mapNode.items]) {
      if (pair.value == null || pair.value?.value === null) {
        mapNode.delete(pair.key);
      }
    }
  };
  stripNullEntries(target);
  const modelsNodeAfterMerge = target.get('models');
  if (isSeq(modelsNodeAfterMerge)) {
    for (const item of modelsNodeAfterMerge.items) stripNullEntries(item);
  }

  // ── validate the resulting whole-file value BEFORE touching disk ──
  const mergedFileValue = doc.toJS({ maxAliasCount: -1 });
  const schemaError = schemaProblems(ModelsConfigFile.schema(mergedFileValue));
  if (schemaError) {
    return { status: 400, body: { error: 'validation', message: `models.yml schema rejected the result: ${schemaError}` } };
  }
  const mergedProvider = plainValue(doc, target);
  try {
    validateProviderConfiguration(id, {
      baseUrl: mergedProvider.baseUrl,
      headers: mergedProvider.headers,
      apiKey: mergedProvider.apiKey,
      api: mergedProvider.api,
      auth: mergedProvider.auth,
      models: mergedProvider.models ?? [],
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

  return { status: 200, body: { provider: projectFileProvider(id, mergedProvider) } };
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
export const deleteOmpProvider = async (input, options = {}) => {
  const modelsPath = options.modelsPath ?? defaultModelsPath();
  const id = typeof input?.id === 'string' ? input.id.trim() : '';
  if (!id) return { status: 400, body: { error: 'validation', message: 'provider id is required' } };

  const { doc } = readDocument(modelsPath);
  const providersMap = doc.get('providers');
  if (!providersMap || !isMap(providersMap) || !providersMap.has(id)) {
    return { status: 404, body: { error: 'not-found', message: `provider ${id} is not defined in models.yml` } };
  }

  providersMap.delete(id);
  const schemaError = schemaProblems(ModelsConfigFile.schema(doc.toJS({ maxAliasCount: -1 })));
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
export const fetchOmpProviderModels = async (input, options = {}) => {
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
  const value = providerNode ? plainValue(doc, providerNode) : null;
  if (!isRecord(value) && !draftBaseUrl) {
    return { status: 404, body: { error: 'not-found', message: `provider ${id} is not defined in models.yml (and no draft baseUrl was provided)` } };
  }
  const baseUrl = draftBaseUrl || (typeof value?.baseUrl === 'string' ? value.baseUrl.trim() : '');
  if (!baseUrl) {
    return { status: 400, body: { error: 'no-base-url', message: `provider ${id} has no baseUrl to fetch from` } };
  }
  const apiKey = draftApiKey || (typeof value?.apiKey === 'string' ? value.apiKey : '');

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
  const list = Array.isArray(payload?.data) ? payload.data : (Array.isArray(payload) ? payload : null);
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

/**
 * Mount the /omp routes owned by this domain. Capability `providers.v1`
 * gates all routes (master R2).
 *
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {{ features?: Record<string, boolean>, modelsPath?: string, listEngineModels?: () => Array<{provider: string}>, refreshModels?: () => Promise<void> }} [options]
 */
export function registerProvidersDomainRoutes(
  route,
  { features = ompFeatures(), modelsPath = defaultModelsPath(), listEngineModels, refreshModels } = {},
) {
  const gated = (handler) => async (request, ctx) => {
    if (features?.['providers.v1'] !== true) return featureUnavailable('providers.v1');
    return handler(request, ctx);
  };

  route('GET', '/omp/providers', gated(async () => {
    return json(await listOmpProviders({ modelsPath, listEngineModels }));
  }));

  route('PUT', '/omp/providers', gated(async (request) => {
    const body = await request.json().catch(() => ({}));
    const { status, body: payload } = await putOmpProvider(body, { modelsPath, listEngineModels, refreshModels });
    return json(payload, { status });
  }));

  route('POST', '/omp/providers/{id}/fetch-models', gated(async (request, ctx) => {
    const body = await request.json().catch(() => ({}));
    const { status, body: payload } = await fetchOmpProviderModels(
      { id: ctx?.params?.id ?? '', baseUrl: body?.baseUrl, apiKey: body?.apiKey },
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
