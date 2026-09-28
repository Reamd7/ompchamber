import fs from 'fs';
import os from 'os';
import path from 'path';
import { readAuthFile, writeAuthFile } from '../opencode/auth.js';
import { readConfig, readConfigLayers, isPlainObject } from '../opencode/shared.js';
import { getCatalogProvider } from './catalog.js';
import { getAuthEntryForProvider } from './resolve.js';
import { getRuntimeProvider } from './runtime-providers.js';

// Direct, non-streaming text generation against the provider APIs, replicating
// how OpenCode authenticates each of them (see the plugin auth loaders in the
// opencode repo). auth.json credentials never leave this process.
/**
 * 小模型的直连调用层：绕过 OpenCode，直接用各 provider 的原生 HTTP 协议
 * （OpenAI chat/completions、Responses、Anthropic messages、Google
 * generateContent、Copilot 端点协商、codex SSE 流）完成非流式文本生成。
 * 各 provider 的认证方式逐一复刻 OpenCode 的插件 auth loader；
 * auth.json 凭证只在当前进程内使用、绝不外发。
 */

/** 单次生成请求的默认超时（毫秒）；调用方可用 timeoutMs 覆盖（如 diff 走查需要更长时限）。 */
const REQUEST_TIMEOUT_MS = 60_000;
/** 拉取 Copilot /models 端点元数据的独立短超时，避免拖慢主请求。 */
const COPILOT_MODELS_TIMEOUT_MS = 5_000;
// Generous default: thinking models that can't be switched off (DeepSeek,
// Qwen, …) spend part of this budget on reasoning before the actual answer.
/** 默认输出 token 预算：给无法关闭思考的模型留出推理开销。 */
const DEFAULT_MAX_OUTPUT_TOKENS = 4_000;

/** 发给 provider 的 User-Agent，与 OpenCode 客户端标识保持一致。 */
const USER_AGENT = 'opencode/1.0 ompchamber';

/**
 * 大小写不敏感地合并 HTTP 头：overrides 中与 base 同名（忽略大小写）的键
 * 会替换 base 里的原键，其余原样保留；返回合并后的新对象。
 */
const mergeHeadersCaseInsensitive = (base, overrides) => {
  const merged = { ...base };
  for (const [name, value] of Object.entries(overrides || {})) {
    const existingName = Object.keys(merged).find((key) => key.toLowerCase() === name.toLowerCase());
    if (existingName) {
      delete merged[existingName];
    }
    merged[name] = value;
  }
  return merged;
};

/** OpenAI OAuth refresh token 端点（codex / ChatGPT 计划）。 */
const CODEX_TOKEN_URL = 'https://auth.openai.com/oauth/token';
/** codex 后端使用的 OAuth client id，与 OpenCode 一致。 */
const CODEX_CLIENT_ID = 'app_EMoamEEZ73f0CkXaXp7hrann';
/** ChatGPT 计划的 codex Responses 端点。 */
const CODEX_RESPONSES_URL = 'https://chatgpt.com/backend-api/codex/responses';

/**
 * 把非 2xx 的 fetch 响应转换为带上下文的 Error：消息含状态码与响应体前
 * 300 字符，并挂 status/provider 属性，供调用方区分 "请求形状被拒"
 * 与 "服务不可用" 两类失败。
 */
const httpError = async (response, provider) => {
  const body = await response.text().catch(() => '');
  const snippet = body ? `: ${body.slice(0, 300)}` : '';
  // Callers need the status to tell "this provider rejected the request shape"
  // (retryable with a different shape) from "this provider is down".
  return Object.assign(new Error(`${provider} request failed with ${response.status}${snippet}`), {
    status: response.status,
    provider,
  });
};

// Callers own two independent reasons to stop: their own abort signal (user
// navigated away, request cancelled) and a per-call deadline. Long-running
// callers such as the diff walkthrough need a deadline well past the default.
/**
 * 组合调用方的 AbortSignal 与按 timeoutMs 计算的截止信号，任一触发即中止
 * 请求；timeoutMs 缺省或非正数时回落到默认超时。
 */
const requestSignal = (timeoutMs, signal) => {
  const deadline = AbortSignal.timeout(Number(timeoutMs) > 0 ? Number(timeoutMs) : REQUEST_TIMEOUT_MS);
  return signal ? AbortSignal.any([deadline, signal]) : deadline;
};

/** structured output 模式下统一的 schema 名称（response_format 或 tool 名）。 */
const STRUCTURED_OUTPUT_NAME = 'response';

// Google's schema dialect is OpenAPI-flavored and rejects JSON Schema keywords
// it does not know, so unsupported keys are dropped rather than passed through.
/** Google 方言不认识的 JSON Schema 关键字，转换时直接丢弃。 */
const GOOGLE_UNSUPPORTED_SCHEMA_KEYS = new Set([
  '$schema',
  'additionalProperties',
  'definitions',
  '$defs',
  '$ref',
  'strict',
]);

/**
 * 递归剔除 schema 中 Google 不支持的关键字，其余结构原样保留，
 * 以适配 Gemini 的 OpenAPI 风格 schema 方言。
 */
const toGoogleSchema = (schema) => {
  if (Array.isArray(schema)) return schema.map(toGoogleSchema);
  if (!schema || typeof schema !== 'object') return schema;
  const result = {};
  for (const [key, value] of Object.entries(schema)) {
    if (GOOGLE_UNSUPPORTED_SCHEMA_KEYS.has(key)) continue;
    result[key] = toGoogleSchema(value);
  }
  return result;
};

// ---------------------------------------------------------------------------
// OpenAI OAuth (ChatGPT plan / codex) token refresh — single-flight, with the
// refreshed token written back to auth.json exactly like OpenCode does.
// ---------------------------------------------------------------------------

/** 进行中的 OpenAI OAuth 刷新 Promise，并发刷新去重（single-flight）。 */
let openaiRefreshPromise = null;

/** 解出 JWT 的 payload claims；token 格式非法时返回 null 而不抛错。 */
const decodeJwtClaims = (token) => {
  try {
    const payload = token.split('.')[1];
    return JSON.parse(Buffer.from(payload, 'base64url').toString('utf8'));
  } catch {
    return null;
  }
};

/**
 * 从访问令牌的 JWT claims 里取 ChatGPT 账号 id（chatgpt_account_id）；
 * 缺失或不是非空字符串时返回 null。
 */
const extractChatgptAccountId = (accessToken) => {
  const claims = decodeJwtClaims(accessToken);
  const auth = claims?.['https://api.openai.com/auth'];
  const value = auth?.chatgpt_account_id;
  return typeof value === 'string' && value ? value : null;
};

/**
 * 用 refresh token 向 OpenAI 换取新的访问令牌，并把结果写回 auth.json
 * （与 OpenCode 行为一致）。并发调用共享同一个 Promise（single-flight），
 * 结束后清空共享状态以便后续重试。
 */
const refreshOpenaiOauth = async (entry) => {
  if (!openaiRefreshPromise) {
    openaiRefreshPromise = (async () => {
      const response = await fetch(CODEX_TOKEN_URL, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          grant_type: 'refresh_token',
          refresh_token: entry.refresh,
          client_id: CODEX_CLIENT_ID,
        }),
        signal: AbortSignal.timeout(30_000),
      });
      if (!response.ok) {
        throw await httpError(response, 'OpenAI token refresh');
      }
      const payload = await response.json();
      const access = typeof payload?.access_token === 'string' ? payload.access_token : '';
      if (!access) {
        throw new Error('OpenAI token refresh returned no access token');
      }
      const refreshed = {
        ...entry,
        type: 'oauth',
        access,
        refresh: typeof payload?.refresh_token === 'string' && payload.refresh_token
          ? payload.refresh_token
          : entry.refresh,
        expires: Date.now() + (Number(payload?.expires_in) > 0 ? Number(payload.expires_in) : 3600) * 1000,
      };
      const auth = readAuthFile();
      auth.openai = refreshed;
      writeAuthFile(auth);
      return refreshed;
    })().finally(() => {
      openaiRefreshPromise = null;
    });
  }
  return openaiRefreshPromise;
};

/**
 * 确保拿到未过期的访问令牌：当前 access 仍有效则原样返回，否则触发刷新；
 * 没有 refresh token 时直接抛错。
 */
const ensureFreshOpenaiOauth = async (entry) => {
  if (entry.access && Number(entry.expires) > Date.now()) {
    return entry;
  }
  if (!entry.refresh) {
    throw new Error('OpenAI OAuth entry has no refresh token');
  }
  return refreshOpenaiOauth(entry);
};

// ---------------------------------------------------------------------------
// Wire formats
// ---------------------------------------------------------------------------

/**
 * OpenAI 兼容 wire format：POST {baseURL}/chat/completions。
 * 支持 system 消息、max_tokens、json_schema 结构化输出与 extraBody 合并；
 * 把 content 为字符串或分段数组两种返回形状归一为纯文本。思考型模型把
 * 输出预算全花在 reasoning 上时，抛 code 为 'output-exhausted' 的错误
 * （预算问题，与传输失败区分开，便于调用方处置）。
 */
const callOpenaiCompatible = async ({ baseURL, headers, modelID, prompt, system, maxOutputTokens, providerLabel, extraBody, responseSchema, timeoutMs, signal }) => {
  const trimmedBase = baseURL.replace(/\/+$/, '');
  console.log('[small-model:diagnostic] request', {
    provider: providerLabel,
    model: modelID,
    maxOutputTokens,
    thinkingDisabled: extraBody?.thinking?.type === 'disabled',
    promptChars: prompt.length,
    systemChars: system?.length ?? 0,
    inputChars: prompt.length + (system?.length ?? 0),
  });
  const response = await fetch(`${trimmedBase}/chat/completions`, {
    method: 'POST',
    headers: mergeHeadersCaseInsensitive({
      'Content-Type': 'application/json',
      Accept: 'application/json',
    }, headers),
    body: JSON.stringify({
      model: modelID,
      messages: [
        ...(system ? [{ role: 'system', content: system }] : []),
        { role: 'user', content: prompt },
      ],
      max_tokens: maxOutputTokens,
      stream: false,
      ...(responseSchema
        ? {
          response_format: {
            type: 'json_schema',
            json_schema: { name: STRUCTURED_OUTPUT_NAME, strict: true, schema: responseSchema },
          },
        }
        : {}),
      ...(extraBody || {}),
    }),
    signal: requestSignal(timeoutMs, signal),
  });
  console.log('[small-model:diagnostic] response', {
    provider: providerLabel,
    model: modelID,
    httpStatus: response.status,
    ok: response.ok,
  });
  if (!response.ok) {
    throw await httpError(response, providerLabel);
  }
  const payload = await response.json();
  const message = payload?.choices?.[0]?.message;
  console.log('[small-model:diagnostic] completion', {
    provider: providerLabel,
    model: modelID,
    finishReason: payload?.choices?.[0]?.finish_reason ?? null,
    contentType: Array.isArray(message?.content) ? 'parts' : typeof message?.content,
    contentChars: typeof message?.content === 'string'
      ? message.content.length
      : Array.isArray(message?.content)
        ? message.content.reduce((total, part) => total + (typeof part?.text === 'string' ? part.text.length : 0), 0)
        : 0,
    reasoningChars: typeof message?.reasoning_content === 'string' ? message.reasoning_content.length : 0,
  });

  // Providers disagree on the content shape: plain string, an array of
  // typed parts, or (thinking models) an empty content with the budget spent
  // on reasoning_content.
  let text = '';
  if (typeof message?.content === 'string') {
    text = message.content;
  } else if (Array.isArray(message?.content)) {
    text = message.content
      .map((part) => (typeof part?.text === 'string' ? part.text : ''))
      .join('');
  }
  const finishReason = payload?.choices?.[0]?.finish_reason;
  if (!text.trim() && (finishReason === 'length' || (typeof message?.reasoning_content === 'string' && message.reasoning_content.trim()))) {
    // The model produced only reasoning, or was cut off before answering. This
    // is a budget problem, not a transport problem, and callers can act on it.
    throw Object.assign(
      new Error(
        `${providerLabel} spent the output budget on reasoning and returned no answer`
        + (finishReason ? ` (finish_reason: ${finishReason})` : ''),
      ),
      { code: 'output-exhausted', provider: providerLabel },
    );
  }
  if (!text.trim()) {
    throw new Error(`${providerLabel} returned no message content`);
  }
  return text;
};

/**
 * OpenAI Responses API wire format：POST {baseURL}/responses。
 * system 走 instructions 字段；输出从 output_text 或 output 数组中的
 * output_text 分段归并；结构化输出用 text.format.json_schema 表达。
 */
const callOpenaiResponses = async ({ baseURL, headers, modelID, prompt, system, maxOutputTokens, providerLabel, responseSchema, timeoutMs, signal }) => {
  const trimmedBase = baseURL.replace(/\/+$/, '');
  const response = await fetch(`${trimmedBase}/responses`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      ...headers,
    },
    body: JSON.stringify({
      model: modelID,
      ...(system ? { instructions: system } : {}),
      input: [{
        role: 'user',
        content: [{ type: 'input_text', text: prompt }],
      }],
      max_output_tokens: maxOutputTokens,
      ...(responseSchema
        ? {
          text: {
            format: {
              type: 'json_schema',
              name: STRUCTURED_OUTPUT_NAME,
              strict: true,
              schema: responseSchema,
            },
          },
        }
        : {}),
      stream: false,
      store: false,
    }),
    signal: requestSignal(timeoutMs, signal),
  });
  if (!response.ok) {
    throw await httpError(response, providerLabel);
  }
  const payload = await response.json();
  const text = typeof payload?.output_text === 'string'
    ? payload.output_text
    : Array.isArray(payload?.output)
      ? payload.output
        .flatMap((item) => (Array.isArray(item?.content) ? item.content : []))
        .map((part) => (part?.type === 'output_text' && typeof part.text === 'string' ? part.text : ''))
        .join('')
      : '';
  if (!text.trim()) {
    throw new Error(`${providerLabel} returned no text output`);
  }
  return text;
};

/**
 * Anthropic messages wire format 的通用实现（也被 Copilot 的 /v1/messages
 * 端点复用）。messages API 没有 response_format，结构化输出改用强制
 * tool_choice 调用单个工具，并把 tool_use.input 序列化为 JSON 字符串返回。
 */
const callMessages = async ({ url, headers, modelID, prompt, system, maxOutputTokens, providerLabel, responseSchema, timeoutMs, signal }) => {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      ...headers,
    },
    body: JSON.stringify({
      model: modelID,
      max_tokens: maxOutputTokens,
      ...(system ? { system } : {}),
      messages: [{ role: 'user', content: prompt }],
      // The messages API has no response_format; a forced single-tool call is
      // the supported way to get schema-shaped output.
      ...(responseSchema
        ? {
          tools: [{
            name: STRUCTURED_OUTPUT_NAME,
            description: 'Return the answer in the required structure.',
            input_schema: responseSchema,
          }],
          tool_choice: { type: 'tool', name: STRUCTURED_OUTPUT_NAME },
        }
        : {}),
    }),
    signal: requestSignal(timeoutMs, signal),
  });
  if (!response.ok) {
    throw await httpError(response, providerLabel);
  }
  const payload = await response.json();

  if (responseSchema) {
    const toolUse = (payload?.content || []).find(
      (part) => part?.type === 'tool_use' && part.name === STRUCTURED_OUTPUT_NAME,
    );
    if (!toolUse || typeof toolUse.input !== 'object' || toolUse.input === null) {
      throw new Error(`${providerLabel} returned no structured output`);
    }
    return JSON.stringify(toolUse.input);
  }

  const text = (payload?.content || [])
    .filter((part) => part?.type === 'text' && typeof part.text === 'string')
    .map((part) => part.text)
    .join('');
  if (!text) {
    throw new Error(`${providerLabel} returned no text content`);
  }
  return text;
};

/**
 * Anthropic 官方 API 调用：baseURL 视为完整 API 前缀（通常已含 /v1），
 * 直接追加 /messages，避免把配置里的 /v1 重复拼一遍；凭证走 x-api-key 头。
 */
const callAnthropic = async ({ apiKey, baseURL, modelID, prompt, system, maxOutputTokens, responseSchema, timeoutMs, signal }) => callMessages({
  // Matches @ai-sdk/anthropic: baseURL is the full API prefix (commonly
  // already ending in /v1), so it gets /messages appended as-is rather than
  // having /v1/messages appended, which would double up a configured /v1.
  url: `${(baseURL || 'https://api.anthropic.com/v1').replace(/\/+$/, '')}/messages`,
  headers: {
    'x-api-key': apiKey,
    'anthropic-version': '2023-06-01',
  },
  modelID,
  prompt,
  system,
  maxOutputTokens,
  providerLabel: 'Anthropic',
  responseSchema,
  timeoutMs,
  signal,
});

/**
 * 查询 Copilot /models，确定目标模型支持的文本端点（优先级 /v1/messages
 * > /responses > /chat/completions）；未标注 supported_endpoints 时按
 * 'chat' 处理；模型不在列表或元数据非法时抛错。
 */
const getCopilotEndpoint = async ({ baseURL, headers, modelID }) => {
  const trimmedBase = baseURL.replace(/\/+$/, '');
  const response = await fetch(`${trimmedBase}/models`, {
    headers: {
      Accept: 'application/json',
      ...headers,
    },
    signal: AbortSignal.timeout(COPILOT_MODELS_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw await httpError(response, 'GitHub Copilot models');
  }

  let payload;
  try {
    payload = await response.json();
  } catch {
    throw new Error('GitHub Copilot models returned invalid JSON');
  }
  if (!Array.isArray(payload?.data)) {
    throw new Error('GitHub Copilot models returned an invalid model list');
  }

  const model = payload.data.find((item) => item && typeof item === 'object' && item.id === modelID);
  if (!model) {
    throw new Error(`GitHub Copilot model "${modelID}" was not returned by /models`);
  }
  if (model.supported_endpoints === undefined) {
    return 'chat';
  }
  if (!Array.isArray(model.supported_endpoints)) {
    throw new Error(`GitHub Copilot model "${modelID}" returned invalid endpoint metadata`);
  }
  if (model.supported_endpoints.includes('/v1/messages')) {
    return 'messages';
  }
  if (model.supported_endpoints.includes('/responses')) {
    return 'responses';
  }
  if (model.supported_endpoints.includes('/chat/completions')) {
    return 'chat';
  }
  throw new Error(`GitHub Copilot model "${modelID}" has no supported text endpoint`);
};

/**
 * Google generateContent 调用：按模型代系关闭或调低思考（gemini-3 用
 * thinkingLevel，gemini-2 用 thinkingBudget: 0）；schema 先经 toGoogleSchema
 * 清洗；无文本输出时抛错。
 */
const callGoogle = async ({ apiKey, modelID, prompt, system, maxOutputTokens, responseSchema, timeoutMs, signal }) => {
  const url = `https://generativelanguage.googleapis.com/v1beta/models/${encodeURIComponent(modelID)}:generateContent`;
  const lowerModelID = modelID.toLowerCase();
  const thinkingConfig = lowerModelID.startsWith('gemini-3')
    ? { thinkingLevel: lowerModelID.includes('flash') ? 'minimal' : 'low' }
    : lowerModelID.startsWith('gemini-2') ? { thinkingBudget: 0 } : null;
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      'x-goog-api-key': apiKey,
    },
    body: JSON.stringify({
      contents: [{ role: 'user', parts: [{ text: prompt }] }],
      ...(system && { systemInstruction: { parts: [{ text: system }] } }),
      generationConfig: {
        maxOutputTokens,
        ...(thinkingConfig && { thinkingConfig }),
        ...(responseSchema && { responseMimeType: 'application/json', responseSchema: toGoogleSchema(responseSchema) }),
      },
    }),
    signal: requestSignal(timeoutMs, signal),
  });
  if (!response.ok) {
    throw await httpError(response, 'Google');
  }
  const payload = await response.json();
  const text = (payload?.candidates?.[0]?.content?.parts || [])
    .map((part) => (typeof part?.text === 'string' ? part.text : ''))
    .join('');
  if (!text) {
    throw new Error('Google returned no text content');
  }
  return text;
};

// ChatGPT-plan traffic goes to the codex backend, which only speaks the
// streaming Responses API — collect the output_text deltas from the SSE body.
/**
 * ChatGPT 计划的 codex 后端只支持流式 Responses API：读取 SSE 响应体，
 * 聚合 output_text.delta 增量（若收到 output_text.done 的完整文本则以其
 * 优先），遇到 response.failed 或 error 事件时抛错。
 */
const callCodexResponses = async ({ accessToken, accountId, modelID, prompt, system, timeoutMs, signal }) => {
  const response = await fetch(CODEX_RESPONSES_URL, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'text/event-stream',
      Authorization: `Bearer ${accessToken}`,
      ...(accountId ? { 'ChatGPT-Account-Id': accountId } : {}),
      originator: 'opencode',
      'User-Agent': USER_AGENT,
    },
    body: JSON.stringify({
      model: modelID,
      ...(system ? { instructions: system } : {}),
      input: [
        {
          type: 'message',
          role: 'user',
          content: [{ type: 'input_text', text: prompt }],
        },
      ],
      // The codex backend rejects max_output_tokens (OpenCode forces it to
      // undefined for this provider too).
      stream: true,
      store: false,
    }),
    signal: requestSignal(timeoutMs, signal),
  });
  if (!response.ok) {
    throw await httpError(response, 'OpenAI (ChatGPT plan)');
  }

  const raw = await response.text();
  let text = '';
  let completedText = '';
  for (const line of raw.split('\n')) {
    if (!line.startsWith('data:')) continue;
    const data = line.slice(5).trim();
    if (!data || data === '[DONE]') continue;
    let event;
    try {
      event = JSON.parse(data);
    } catch {
      continue;
    }
    if (event?.type === 'response.output_text.delta' && typeof event.delta === 'string') {
      text += event.delta;
    }
    if (event?.type === 'response.output_text.done' && typeof event.text === 'string') {
      completedText = event.text;
    }
    if (event?.type === 'response.failed' || event?.type === 'error') {
      const message = event?.response?.error?.message || event?.message || 'response failed';
      throw new Error(`OpenAI (ChatGPT plan) stream error: ${message}`);
    }
  }
  const result = completedText || text;
  if (!result) {
    throw new Error('OpenAI (ChatGPT plan) returned no text output');
  }
  return result;
};

// ---------------------------------------------------------------------------
// Custom provider configuration support
// ---------------------------------------------------------------------------

/**
 * 解析 provider 配置中的 {env:NAME} 与 {file:path} 占位值：env 取进程
 * 环境变量；file 的相对路径以 "声明该值的配置文件所在目录" 为基准解析
 * （~ 展开为主目录），读取失败或文件为空时抛错；普通字符串原样返回。
 */
const resolveConfigValue = (value, workingDirectory, providerID, headerName = null) => {
  const envMatch = value.match(/^\{env:([^}]+)\}$/i);
  if (envMatch) {
    return process.env[envMatch[1].trim()]?.trim() || null;
  }

  const fileMatch = value.match(/^\{file:(.+)\}$/i);
  if (!fileMatch) return value;

  const configuredPath = fileMatch[1].trim();
  let resolvedPath;
  if (configuredPath === '~' || configuredPath.startsWith('~/') || configuredPath.startsWith('~\\')) {
    resolvedPath = path.join(os.homedir(), configuredPath.slice(2));
  } else if (path.isAbsolute(configuredPath)) {
    resolvedPath = configuredPath;
  } else {
    const layers = readConfigLayers(workingDirectory);
    const source = [
      { config: layers.customConfig, filePath: layers.paths.customPath },
      { config: layers.projectConfig, filePath: layers.paths.projectPath },
      { config: layers.userConfig, filePath: layers.paths.userPath },
    ].find(({ config }) => {
      const options = config?.provider?.[providerID]?.options;
      return headerName
        ? options?.headers?.[headerName] === value
        : options?.apiKey === value;
    });
    resolvedPath = path.resolve(source?.filePath ? path.dirname(source.filePath) : workingDirectory || process.cwd(), configuredPath);
  }

  try {
    const key = fs.readFileSync(resolvedPath, 'utf8').trim();
    if (!key) throw new Error('empty file');
    return key;
  } catch {
    throw new Error(`Failed to resolve configured ${headerName ? `header "${headerName}"` : 'apiKey'} file for provider "${providerID}"`);
  }
};

/**
 * `options.headers` from the provider config, with the same `{env:…}`/`{file:…}`
 * substitutions the API key gets.
 *
 * OpenCode sends these on every request, so dropping them here would have the
 * small model authenticating differently from the request path against the same
 * URL. Gateways fronted by an API-management layer reject a bearer-only request
 * outright, because the header is the credential rather than a supplement to it.
 */
/**
 * 读取配置里的 options.headers 并做与 apiKey 相同的 {env:…}/{file:…}
 * 占位符替换；非字符串的畸形条目跳过，全部解析为空时返回 null。
 */
const readConfiguredHeaders = (providerCfg, workingDirectory, providerID) => {
  const configured = providerCfg?.options?.headers;
  if (!isPlainObject(configured)) return null;
  const headers = {};
  for (const [name, value] of Object.entries(configured)) {
    // Config headers are strings; a malformed entry is skipped rather than
    // stringified into a header the gateway would reject.
    if (String(value) !== value) continue;
    const resolved = resolveConfigValue(value.trim(), workingDirectory, providerID, name);
    if (resolved) headers[name] = resolved;
  }
  return Object.keys(headers).length ? headers : null;
};

/**
 * 读取 OpenCode 配置中 provider.<id> 的 baseURL/apiKey/headers；apiKey
 * 经占位符解析后包装成 {type:'api'} 形状的 auth 条目，使其能参与统一的
 * 凭证优先级比较。配置读取失败时返回 null（回退到仅目录解析）。
 */
const readProviderConfig = (workingDirectory, providerID) => {
  try {
    const config = readConfig(workingDirectory);
    const providerCfg = config?.provider?.[providerID];
    if (!providerCfg || typeof providerCfg !== 'object') return null;
    const baseURL = typeof providerCfg?.options?.baseURL === 'string' ? providerCfg.options.baseURL.trim() : null;
    const rawApiKey = typeof providerCfg?.options?.apiKey === 'string' ? providerCfg.options.apiKey.trim() : null;
    const apiKey = rawApiKey ? resolveConfigValue(rawApiKey, workingDirectory, providerID) : null;
    return {
      baseURL,
      headers: readConfiguredHeaders(providerCfg, workingDirectory, providerID),
      // Shape the config-supplied key as a regular api-key auth entry so it
      // can win the precedence check below and flow through the dispatch's
      // `entry.type === 'api' ? entry.key : ...` branch unchanged.
      auth: apiKey ? { type: 'api', key: apiKey } : null,
    };
  } catch {
    // Provider config is non-essential — continue with catalog-only resolution.
    return null;
  }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/**
 * Providers reached through a dedicated wire format below: a token exchange,
 * an OAuth refresh, or a non-bearer header. OpenCode's runtime
 * `options.apiKey` is not the value those branches need — the ChatGPT-plan
 * `openai` login is the clearest case, where the runtime key is an OAuth
 * access token that api.openai.com answers with 401 — so the runtime
 * credential never stands in for them, and the runtime listing skips them
 * because the auth.json scan already covers them.
 */
/** 走专用 wire format 的 provider 集合（token 交换、OAuth 刷新或非 bearer 头认证）；运行时 apiKey 不适用于它们，由 auth.json 扫描覆盖。 */
export const DEDICATED_WIRE_FORMAT_PROVIDERS = new Set(['github-copilot', 'copilot', 'openai', 'anthropic', 'google']);

/**
 * The runtime credential shaped as an auth entry, or `null` when the provider
 * owns its credential handling or OpenCode reports nothing usable.
 */
/** 把运行时 apiKey 包装成 auth 条目形状；专用 wire format provider 或无凭证时返回 null。 */
const runtimeCredential = (providerID, runtime) => (
  !DEDICATED_WIRE_FORMAT_PROVIDERS.has(providerID) && runtime?.apiKey
    ? { type: 'api', key: runtime.apiKey }
    : null
);

/**
 * Same credential resolution the request path uses: config
 * `provider.<id>.options.apiKey` wins, then the runtime credential OpenCode
 * resolved for a plugin provider, then the auth.json entry.
 * Callers that need to refuse before spending a request (walkthrough readiness)
 * must use this rather than inventing a second rule.
 */
/**
 * 与请求路径完全相同的凭证解析优先级：配置 provider.<id>.options.apiKey
 * > OpenCode 运行时凭证（插件 provider 凭证的唯一来源）> auth.json 条目；
 * 都没有时返回 null。需要在发请求前判定可用性（如 walkthrough 就绪检查）
 * 的调用方必须复用这里，而不是另立第二套规则。
 */
export async function resolveProviderLogin({ auth, workingDirectory, providerID }) {
  const providerConfig = readProviderConfig(workingDirectory, providerID);
  return providerConfig?.auth
    || runtimeCredential(providerID, await getRuntimeProvider(providerID))
    || getAuthEntryForProvider(auth, providerID)
    || null;
}

/**
 * 小模型调用总入口：解析凭证与端点后按 provider 分发到对应 wire format。
 * github-copilot：存储的 token 直接作 bearer，按 /models 协商端点；
 * openai 的 oauth 登录：走 codex 流（不支持结构化输出，带 schema 直接报错）；
 * anthropic / google：各自原生 API；其余一律按 OpenAI 兼容
 * chat/completions 调用，baseURL 依次取 配置 > openai 默认 > 运行时 > 目录。
 * 无法关闭思考的已知模型附加 thinking disabled 开关；任何凭证都解析不到时
 * 抛 statusCode 401 / code 'no-provider-login'，供调用方展示阻塞原因。
 */
export async function callSmallModel({ auth, catalog, workingDirectory, providerID, modelID, prompt, system, maxOutputTokens, responseSchema, timeoutMs, signal }) {
  const tokens = Number(maxOutputTokens) > 0 ? Number(maxOutputTokens) : DEFAULT_MAX_OUTPUT_TOKENS;
  const providerConfig = readProviderConfig(workingDirectory, providerID);
  const runtimeProvider = await getRuntimeProvider(providerID);
  // Match OpenCode's resolveSDK precedence: config `provider.<id>.options`
  // wins, then what OpenCode itself resolved at runtime (the only place a
  // plugin's credential exists), and the auth.json entry last.
  const entry = providerConfig?.auth
    || runtimeCredential(providerID, runtimeProvider)
    || getAuthEntryForProvider(auth, providerID);
  if (!entry) {
    // Structured so the walkthrough (and any other caller) can show a blocker
    // instead of a raw 500 banner with this developer-oriented sentence.
    throw Object.assign(new Error(`No OpenCode login found for provider "${providerID}"`), {
      statusCode: 401,
      code: 'no-provider-login',
      providerID,
    });
  }

  if (providerID === 'github-copilot') {
    // OpenCode uses the stored device-OAuth token directly as the bearer —
    // access === refresh, no exchange, no expiry.
    const token = entry.refresh || entry.access || entry.key;
    if (!token) {
      throw new Error('GitHub Copilot login has no token');
    }
    const baseURL = entry.enterpriseUrl
      ? `https://copilot-api.${String(entry.enterpriseUrl).replace(/^https?:\/\//, '').replace(/\/+$/, '')}`
      : 'https://api.githubcopilot.com';
    const authHeaders = {
      Authorization: `Bearer ${token}`,
      'User-Agent': USER_AGENT,
      'X-GitHub-Api-Version': '2026-06-01',
    };
    const headers = {
      ...authHeaders,
      'Openai-Intent': 'conversation-edits',
      'x-initiator': 'agent',
    };
    const endpoint = await getCopilotEndpoint({
      baseURL,
      headers: authHeaders,
      modelID,
    });
    const request = {
      baseURL,
      headers,
      modelID,
      prompt,
      system,
      maxOutputTokens: tokens,
      providerLabel: 'GitHub Copilot',
      responseSchema,
      timeoutMs,
      signal,
    };
    if (endpoint === 'messages') {
      return callMessages({
        ...request,
        url: `${baseURL.replace(/\/+$/, '')}/v1/messages`,
        headers: {
          ...headers,
          'anthropic-version': '2023-06-01',
        },
      });
    }
    if (endpoint === 'responses') {
      return callOpenaiResponses(request);
    }
    return callOpenaiCompatible(request);
  }

  if (providerID === 'openai' && entry.type === 'oauth') {
    // The codex backend speaks only the streaming Responses API and rejects
    // the structured-output fields, so a schema request fails loudly here
    // instead of silently returning free-form prose.
    if (responseSchema) {
      throw Object.assign(
        new Error('The ChatGPT-plan OpenAI login does not support structured output — choose another small model'),
        { code: 'structured-output-unsupported' },
      );
    }
    const fresh = await ensureFreshOpenaiOauth(entry);
    return callCodexResponses({
      accessToken: fresh.access,
      accountId: fresh.accountId || extractChatgptAccountId(fresh.access),
      modelID,
      prompt,
      system,
      timeoutMs,
      signal,
    });
  }

  const apiKey = entry.type === 'api' ? entry.key
    : entry.type === 'wellknown' ? entry.token
      : entry.access;
  if (!apiKey) {
    throw new Error(`OpenCode login for "${providerID}" has no usable credential`);
  }

  if (providerID === 'anthropic') {
    return callAnthropic({ apiKey, baseURL: providerConfig?.baseURL, modelID, prompt, system, maxOutputTokens: tokens, responseSchema, timeoutMs, signal });
  }
  if (providerID === 'google') {
    return callGoogle({ apiKey, modelID, prompt, system, maxOutputTokens: tokens, responseSchema, timeoutMs, signal });
  }

  // Everything else: OpenAI-compatible chat completions against the catalog's
  // base URL for that provider (openai itself included). When a custom provider
  // is not in the catalog (e.g. a user-configured OpenAI-compatible proxy),
  // fall back to its baseURL from the OpenCode provider config, then to the
  // endpoint OpenCode resolved at runtime — which for a plugin provider is the
  // only place it exists, and for several of them is a local proxy the plugin
  // itself runs. The openai provider also respects
  // provider.openai.options.baseURL — OpenCode itself uses the same config for
  // all providers including openai.
  const provider = getCatalogProvider(catalog, providerID);
  const providerConfigUrl = providerConfig?.baseURL;
  const defaultOpenaiUrl = 'https://api.openai.com/v1';
  const baseURL = typeof providerConfigUrl === 'string' && providerConfigUrl
    ? providerConfigUrl
    : providerID === 'openai'
      ? defaultOpenaiUrl
      : runtimeProvider?.baseURL
        ?? (typeof provider?.api === 'string' && provider.api
          ? provider.api
          : null);
  if (!baseURL) {
    throw new Error(`Provider "${providerID}" has no known API base URL`);
  }

  // Thinking models burn the output budget on reasoning and leave content
  // empty — disable thinking where a wire-format switch exists (mirrors
  // OpenCode's smallOptions/variants special cases). There is NO universal
  // parameter: unknown body fields 400 on some providers, so this stays an
  // explicit allowlist. Models without a switch (DeepSeek, Qwen, Kimi, …)
  // just get the generous output budget.
  const lowerModel = modelID.toLowerCase();
  const supportsThinkingToggle = providerID.includes('zai')
    || providerID.includes('zhipu')
    || lowerModel.includes('glm')
    || lowerModel.includes('minimax-m3');
  const extraBody = supportsThinkingToggle ? { thinking: { type: 'disabled' } } : undefined;

  return callOpenaiCompatible({
    baseURL,
    // Configured headers last: a gateway that authenticates on its own header
    // must be able to override the bearer default rather than sit beside it.
    headers: mergeHeadersCaseInsensitive({ Authorization: `Bearer ${apiKey}` }, providerConfig?.headers),
    modelID,
    prompt,
    system,
    maxOutputTokens: tokens,
    providerLabel: provider?.name || providerID,
    extraBody,
    responseSchema,
    timeoutMs,
    signal,
  });
}
