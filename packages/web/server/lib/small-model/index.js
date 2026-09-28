/**
 * Small Model 服务层入口：承担 "用哪个小模型" 的完整决策链——设置/配置
 * 覆盖与自动解析（resolve.js）、输入按上下文预算截断、输出预算封顶——
 * 再把请求交给 call.js 的对应 wire format 执行；同时向 UI 提供可用
 * provider 列表与当前模型的能力描述（不发起真实调用）。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';
import { readAuthFile } from '../opencode/auth.js';
import { readConfigLayers } from '../opencode/shared.js';
import { getModelCatalog } from './catalog.js';
import { resolveSmallModel, parseModelRef, isUsableAuthEntry, getAuthEntryForProvider } from './resolve.js';
import { DEDICATED_WIRE_FORMAT_PROVIDERS, callSmallModel, resolveProviderLogin } from './call.js';
import { getRuntimeProviderSnapshot } from './runtime-providers.js';

// Never a small model, whatever the transport looks like. A plugin can publish
// an OpenAI-compatible endpoint for Claude Code, but it is a façade over the
// Claude Agent SDK, which spawns the Claude Code CLI per request and spends
// the user's Claude subscription rate limit. Paying that for a session title
// or a summary is the wrong trade, so the refusal is unconditional rather than
// conditional on an endpoint existing.
/** 绝不作为小模型使用的 provider：其端点是 Claude Agent SDK 的门面，每次调用都消耗 Claude 订阅额度。 */
const CLAUDE_CODE_PROVIDER = 'claude-code';

/** OMPChamber 自身 settings.json 的路径（可被 OMPCHAMBER_DATA_DIR 环境变量覆盖）。 */
const OMPCHAMBER_SETTINGS_FILE = path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'settings.json',
);

// OMPChamber's own settings: when the user unchecks "use default small model"
// their explicit override outranks every other resolution step.
/**
 * 读取用户设置里的小模型覆盖（仅当 smallModelUseDefault === false 时生效）；
 * 文件不存在、JSON 非法或没有有效覆盖值时返回 null，绝不抛错。
 */
const readSmallModelSettingsOverride = () => {
  try {
    const raw = fs.readFileSync(OMPCHAMBER_SETTINGS_FILE, 'utf8');
    const settings = JSON.parse(raw);
    if (!settings || typeof settings !== 'object') return null;
    if (settings.smallModelUseDefault !== false) return null;
    const override = typeof settings.smallModelOverride === 'string' ? settings.smallModelOverride.trim() : '';
    return override || null;
  } catch {
    return null;
  }
};

// Rough safety clamp so a huge input never blows the model's context window.
// Token estimate is ~4 chars/token; when the catalog has no limit for the
// model (Copilot/codex utility models are not listed) a conservative default
// applies.
/** 目录未给出上下文上限时的保守默认值（token 数）。 */
const DEFAULT_CONTEXT_TOKENS = 64_000;
/** 调用方未声明输出预留时的默认值（token 数）。 */
const OUTPUT_RESERVE_TOKENS = 4_000;

/**
 * Input budget in characters, given how much of the context the caller intends
 * to leave for the answer. The reserve must match the output budget the caller
 * will actually request, or the two disagree and the model overruns its context.
 */
/**
 * 计算输入字符预算：上下文 token 数减去输出预留（下限 1000 token），
 * 按 ~4 字符/token 折算成字符；同时返回实际采用的上下文与它是否来自目录。
 */
export const getModelInputCharBudget = ({ catalog, providerID, modelID, outputReserveTokens }) => {
  const limit = catalog?.[providerID]?.models?.[modelID]?.limit;
  const known = Number(limit?.context) > 0;
  const contextTokens = known ? Number(limit.context) : DEFAULT_CONTEXT_TOKENS;
  const reserve = Number(outputReserveTokens) > 0 ? Number(outputReserveTokens) : OUTPUT_RESERVE_TOKENS;
  const inputBudgetTokens = Math.max(1_000, contextTokens - reserve);
  return { maxChars: inputBudgetTokens * 4, contextTokens, contextKnown: known };
};

/**
 * The output budget to actually request: what the caller asked for, capped by
 * what the model admits it can emit. Asking for more than `limit.output` is
 * rejected outright by some providers and silently ignored by others.
 */
/**
 * 解析实际请求的输出 token 预算：取调用方申请值与目录 limit.output 的
 * 较小者；调用方未申请时不设上限（返回 undefined）。
 */
const resolveOutputTokens = ({ catalog, providerID, modelID, maxOutputTokens }) => {
  const requested = Number(maxOutputTokens) > 0 ? Number(maxOutputTokens) : 0;
  if (!requested) return undefined;
  const limit = Number(catalog?.[providerID]?.models?.[modelID]?.limit?.output);
  return limit > 0 ? Math.min(requested, limit) : requested;
};

// `truncate` keeps the historical behavior for callers whose prompt losing its
// tail is survivable (summaries, commit messages). `error` is for callers whose
// output would be quietly wrong on a clipped input — they need the failure.
/**
 * 按模型输入预算处理超长 prompt：默认截断到预算并追加省略号（truncated
 * 为 true）；onOverflow 为 'error' 时抛 statusCode 413 / code
 * 'context-too-small'，让无法容忍截断的调用方拿到明确失败而不是失真的输入。
 */
const clampPromptToModelLimit = ({ prompt, catalog, providerID, modelID, onOverflow, outputReserveTokens }) => {
  const { maxChars } = getModelInputCharBudget({ catalog, providerID, modelID, outputReserveTokens });
  if (prompt.length <= maxChars) {
    return { prompt, truncated: false };
  }
  if (onOverflow === 'error') {
    throw Object.assign(
      new Error(`Input is too large for ${providerID}/${modelID}: ${prompt.length} characters exceeds the ${maxChars} the model's context allows`),
      { statusCode: 413, code: 'context-too-small', providerID, modelID, requiredChars: prompt.length, availableChars: maxChars },
    );
  }
  return { prompt: `${prompt.slice(0, maxChars)}…`, truncated: true };
};

/**
 * 读取 OpenCode 配置层合并后的 small_model（"provider/model" 字符串）；
 * 读取失败或值不是字符串时返回 null。
 */
const readConfiguredSmallModel = (workingDirectory) => {
  try {
    const { mergedConfig } = readConfigLayers(workingDirectory);
    const value = mergedConfig?.small_model;
    return typeof value === 'string' ? value : null;
  } catch {
    return null;
  }
};

/**
 * Generates text with the user's small model, resolved and authenticated
 * entirely server-side from the OpenCode config and auth store.
 */
/**
 * 小模型文本生成主入口：校验 prompt → 解析模型（显式 model 优先，否则走
 * resolveSmallModel 链）→ 无条件拒绝 Claude Code → 可选锁定会话 provider →
 * 解析输出预算并按预算截断输入 → 调用 callSmallModel。
 * 返回文本与实际使用的 provider/model/source；输入被截断时附带
 * inputTruncated: true。prompt 缺失抛 400，无可用模型抛 404。
 */
export async function generateSmallModelText({ prompt, system, maxOutputTokens, model, directory, preferredProviderID, preferredModelID, restrictToPreferredProvider = false, responseSchema, timeoutMs, signal, onOverflow = 'truncate' }) {
  if (typeof prompt !== 'string' || !prompt.trim()) {
    throw Object.assign(new Error('prompt is required'), { statusCode: 400 });
  }

  const auth = readAuthFile();
  const catalog = await getModelCatalog().catch(() => ({}));

  const explicit = parseModelRef(model);
  const resolved = explicit
    ? { ...explicit, source: 'request' }
    : resolveSmallModel({
      auth,
      catalog,
      settingsSmallModel: readSmallModelSettingsOverride(),
      configSmallModel: readConfiguredSmallModel(directory),
      preferredProviderID,
      preferredModelID,
    });

  if (!resolved) {
    throw Object.assign(
      new Error('No small model available — no authenticated provider has a suitable model'),
      { statusCode: 404 },
    );
  }

  if (resolved.providerID === CLAUDE_CODE_PROVIDER) {
    throw Object.assign(
      new Error('Claude Code cannot be used for background small-model actions. Choose another Small Model in Settings → Sessions.'),
      { statusCode: 422, code: 'small-model-provider-unsupported' },
    );
  }

  // Callers with a session context can forbid silently switching providers:
  // an explicit user choice (settings override, opencode config, request
  // model) is always allowed, anything else must stay on the session's
  // provider.
  if (restrictToPreferredProvider
    && !['settings', 'config', 'request'].includes(resolved.source)
    && resolved.providerID !== preferredProviderID) {
    throw Object.assign(
      new Error('No small model available within the session provider'),
      { statusCode: 404 },
    );
  }

  const outputTokens = resolveOutputTokens({
    catalog,
    providerID: resolved.providerID,
    modelID: resolved.modelID,
    maxOutputTokens,
  });

  const clamped = clampPromptToModelLimit({
    prompt: prompt.trim(),
    catalog,
    providerID: resolved.providerID,
    modelID: resolved.modelID,
    onOverflow,
    outputReserveTokens: outputTokens,
  });

  const text = await callSmallModel({
    auth,
    catalog,
    workingDirectory: directory,
    providerID: resolved.providerID,
    modelID: resolved.modelID,
    prompt: clamped.prompt,
    system: typeof system === 'string' && system.trim() ? system.trim() : undefined,
    maxOutputTokens: outputTokens,
    responseSchema,
    timeoutMs,
    signal,
  });

  return {
    text: text.trim(),
    providerID: resolved.providerID,
    modelID: resolved.modelID,
    source: resolved.source,
    ...(clamped.truncated ? { inputTruncated: true } : {}),
  };
}

/**
 * Provider ids the small model can actually call — an auth.json login, or a
 * credential and endpoint the running OpenCode resolved for a plugin. Used by
 * the Small Model and Changes Walkthrough pickers to hide providers that would
 * only ever fail (e.g. opencode free models without a token).
 */
/**
 * 小模型真正可调用的 provider id 列表：auth.json 可用登录 + 运行时
 * （插件）可调用 provider，并剔除 Claude Code；任何失败都返回空数组，
 * 运行时查询失败只损失它本可补充的条目，不影响已从磁盘确立的登录。
 */
export async function listAuthenticatedProviders() {
  try {
    const auth = readAuthFile();
    const ids = new Set(
      Object.keys(auth || {}).filter((providerID) => isUsableAuthEntry(auth[providerID])),
    );
    // The catalog id is github-copilot while legacy auth entries may sit
    // under the copilot alias.
    if (isUsableAuthEntry(getAuthEntryForProvider(auth, 'github-copilot'))) {
      ids.add('github-copilot');
    }
    // Kept separate so a runtime lookup that goes wrong costs the providers it
    // would have added, never the logins already established from disk.
    try {
      for (const providerID of await listRuntimeCallableProviders()) ids.add(providerID);
    } catch {
      // The auth.json set below stands on its own.
    }
    ids.delete(CLAUDE_CODE_PROVIDER);
    return Array.from(ids);
  } catch {
    return [];
  }
}

/**
 * Providers that only the running OpenCode knows about — plugin-registered
 * ones, and any whose endpoint is resolved at startup.
 *
 * The test is the same one applied to an auth.json login: a credential we may
 * use and somewhere to send it. Whether the endpoint answers the protocol we
 * speak is not knowable from any field OpenCode reports, and guessing it wrong
 * removes a working model from the picker with nothing to explain it.
 */
/** 只存在于运行中 OpenCode 里的可调用 provider：有可用的凭证与端点，且不属于专用 wire format。 */
async function listRuntimeCallableProviders() {
  const snapshot = await getRuntimeProviderSnapshot();
  if (!snapshot) return [];
  const ids = [];
  for (const id of snapshot.connected) {
    const provider = snapshot.providers.get(id);
    // No credential we may use — including the zen sentinel, whose free models
    // belong to OpenCode's own server.
    if (!provider?.apiKey || !provider.baseURL) continue;
    // Reached through a dedicated wire format and already covered by the
    // auth.json scan above.
    if (DEDICATED_WIRE_FORMAT_PROVIDERS.has(id)) continue;
    ids.push(id);
  }
  return ids;
}

/**
 * Reports which model would be used, without calling it.
 *
 * `inputCharBudget` and `structuredOutput` let callers refuse work before
 * spending a request: the walkthrough needs both a big enough context and
 * schema-shaped output, and would rather tell the user to pick another model
 * than send a doomed prompt. `structuredOutput` is deliberately tri-state —
 * the catalog omits the field for roughly half of all models (aggregators and
 * proxies especially), and treating "unknown" as "unsupported" would hide
 * models that work fine.
 */
/**
 * The reserve, resolved against the model that was actually picked.
 *
 * A caller that wants "as much answer room as this model allows" cannot state a
 * number up front — it does not know which model it will get. Passing a
 * function lets it decide once the limits are known, and keeps the reserve and
 * the eventual request the same number by construction.
 */
/** 输出预留允许传函数：拿到解析后模型的真实限额（上下文/输出上限）再决定数值。 */
const resolveReserveTokens = (outputReserveTokens, limits) => (
  typeof outputReserveTokens === 'function' ? outputReserveTokens(limits) : outputReserveTokens
);

/**
 * 报告当前会用到哪个小模型及其能力，不发起调用：输入字符预算、上下文
 * 是否已知、输出预算与上限、structured output 三态（true/false/null，
 * null 表示目录未标注，不能当作 "不支持"），以及是否有可用登录
 * （resolveProviderLogin）。供 walkthrough 等调用方在花钱请求前决定放行。
 */
export async function describeSmallModel({ directory, preferredProviderID, preferredModelID, outputReserveTokens, overrideModel } = {}) {
  const auth = readAuthFile();
  const catalog = await getModelCatalog().catch(() => ({}));
  // A caller with its own model setting (the diff walkthrough) outranks the
  // small-model chain entirely — it asked for this model on purpose.
  const explicit = parseModelRef(overrideModel);
  const resolved = explicit
    ? { ...explicit, source: 'request' }
    : resolveSmallModel({
      auth,
      catalog,
      settingsSmallModel: readSmallModelSettingsOverride(),
      configSmallModel: readConfiguredSmallModel(directory),
      preferredProviderID,
      preferredModelID,
    });
  if (!resolved) return resolved;

  const entry = catalog?.[resolved.providerID]?.models?.[resolved.modelID];
  const outputTokenLimit = Number(entry?.limit?.output) > 0 ? Number(entry.limit.output) : null;
  // Two passes: the first only to learn the context, which a caller-supplied
  // reserve function needs before it can answer.
  const { contextTokens, contextKnown } = getModelInputCharBudget({
    catalog,
    providerID: resolved.providerID,
    modelID: resolved.modelID,
  });
  const reserveTokens = resolveReserveTokens(outputReserveTokens, { contextTokens, outputTokenLimit });
  const { maxChars } = getModelInputCharBudget({
    catalog,
    providerID: resolved.providerID,
    modelID: resolved.modelID,
    outputReserveTokens: reserveTokens,
  });

  // Settings/config/request overrides can name a provider with no usable login.
  // Report that here so readiness can refuse before the user pays for a 401.
  const hasLogin = Boolean(await resolveProviderLogin({
    auth,
    workingDirectory: directory,
    providerID: resolved.providerID,
  }));

  return {
    ...resolved,
    hasLogin,
    inputCharBudget: maxChars,
    contextTokens,
    contextKnown,
    // What the caller should ask for, so the request and the reserve above
    // cannot drift apart.
    outputTokens: Number(reserveTokens) > 0 ? Number(reserveTokens) : null,
    structuredOutput: typeof entry?.structured_output === 'boolean' ? entry.structured_output : null,
    outputTokenLimit,
  };
}
