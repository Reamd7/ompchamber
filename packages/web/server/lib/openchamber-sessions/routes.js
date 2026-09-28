/**
 * OMPChamber 会话服务与 HTTP 路由（openchamber-sessions）。
 *
 * createOMPChamberSessionService 实现会话的 create/send/fork 全链路：
 * 目录/项目解析、可选 worktree 建立与引导等待、模型/agent/variant 的
 * 校验与缺省回退、goal 元数据创建、slash 命令分发、prompt_async 派发
 * 与"落盘确认"，并把结果经 emitSessionCreatedEvent 广播。
 * registerOMPChamberSessionRoutes 把它们挂到 POST /api/ompchamber/sessions
 * （及 /:sessionId/send、/:sessionId/fork），错误统一经 OMPChamberControlError
 * 映射（含 fork-created / goal-configured 的 partial 详情）。
 */
import express from 'express';
import { createLocalEngineClient } from '../opencode/local-engine-client.js';
import { createWorktree, getWorktreeBootstrapStatus } from '../git/index.js';
import { expandSnippets } from '../opencode/snippets.js';
import { expandCommandGoalObjective, parseScheduledCommandPrompt } from '../scheduled-tasks/runtime.js';
import { buildGoalIntroText, createSessionGoal } from '../session-goal/create.js';
import { OMPChamberControlError, asControlError } from '../openchamber-control/error.js';

/** 规整可选字符串：trim 后非空返回 trim 结果，否则返回 null。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 拆分 "provider/model"；缺省或格式非法返回 null（不抛错）。 */
const splitModel = (value) => {
  const model = asNonEmptyString(value);
  if (!model) return null;
  const slashIndex = model.indexOf('/');
  if (slashIndex <= 0 || slashIndex === model.length - 1) return null;
  return {
    providerID: model.slice(0, slashIndex),
    modelID: model.slice(slashIndex + 1),
  };
};

/** 从 payload 解析请求模型：优先 model 字符串，其次 providerID+modelID 组合；都没有返回 null。 */
const resolveRequestedModel = (payload) => {
  const model = splitModel(payload?.model);
  if (model) return model;

  const providerID = asNonEmptyString(payload?.providerID);
  const modelID = asNonEmptyString(payload?.modelID);
  return providerID && modelID ? { providerID, modelID } : null;
};

/** 模型全缺省时的兜底 provider id。 */
const FALLBACK_PROVIDER_ID = 'opencode';
/** 模型全缺省时的兜底 model id。 */
const FALLBACK_MODEL_ID = 'big-pickle';
/** goal token 预算下限。 */
const MIN_GOAL_TOKEN_BUDGET = 1_000;
/** goal token 预算上限。 */
const MAX_GOAL_TOKEN_BUDGET = 100_000_000;

/**
 * 解析 goal 输入并校验组合：goalTokenBudget 必须搭配 goal；goal 必须有
 * prompt；预算必须是 [MIN, MAX] 内的整数。返回 {ok, enabled, tokenBudget}
 * 或 {ok:false, error}——校验失败不抛错，由调用方决定状态码。
 */
const resolveGoalInput = (payload, prompt) => {
  const enabled = payload?.goal === true;
  if (payload?.goalTokenBudget !== undefined && !enabled) {
    return { ok: false, error: 'goalTokenBudget requires goal' };
  }
  if (enabled && !prompt) {
    return { ok: false, error: 'prompt is required when goal is enabled' };
  }
  if (payload?.goalTokenBudget === undefined) {
    return { ok: true, enabled, tokenBudget: null };
  }
  const tokenBudget = payload.goalTokenBudget;
  if (!Number.isSafeInteger(tokenBudget)
    || tokenBudget < MIN_GOAL_TOKEN_BUDGET
    || tokenBudget > MAX_GOAL_TOKEN_BUDGET) {
    return { ok: false, error: `goalTokenBudget must be an integer from ${MIN_GOAL_TOKEN_BUDGET} to ${MAX_GOAL_TOKEN_BUDGET}` };
  }
  return { ok: true, enabled, tokenBudget };
};

/** 判断 agent mode 是否可作为主入口（未声明、primary 或 all）。 */
const isPrimaryAgentMode = (mode) => !mode || mode === 'primary' || mode === 'all';

/** 取 provider 的模型列表；models 可能是数组也可能是对象（取 values）。 */
const providerModels = (provider) => {
  if (Array.isArray(provider?.models)) return provider.models;
  if (provider?.models && typeof provider.models === 'object') return Object.values(provider.models);
  return [];
};

/** 判断 providers 里是否存在指定的 provider/model 组合。 */
const hasProviderModel = (providers, providerID, modelID) => {
  return providers.some((provider) => provider?.id === providerID
    && providerModels(provider).some((model) => model?.id === modelID));
};

/**
 * 校验 variant 是否属于该模型：模型不存在或 variants 里没有该键时返回
 * undefined（视为未指定），存在则原样返回。
 */
const resolveVariant = (providers, providerID, modelID, variant) => {
  const normalized = asNonEmptyString(variant);
  if (!normalized) return undefined;
  const provider = providers.find((entry) => entry?.id === providerID);
  const model = providerModels(provider).find((entry) => entry?.id === modelID);
  return model?.variants && Object.prototype.hasOwnProperty.call(model.variants, normalized)
    ? normalized
    : undefined;
};

/** 解析配置里的默认模型字符串（settings.defaultModel / opencode model）。 */
const parseConfigModel = (value) => splitModel(value);

/** 构造 OpenCode 的目录 header；directory 为空时不携带该 header。 */
const buildDirectoryHeaders = (directory) => ({
  // OpenCode rejects non-ASCII header values; the official SDK sends this
  // header percent-encoded, so match that wire format (non-ASCII checkout
  // paths such as "Masaüstü" otherwise fail every dispatched prompt).
  ...(directory ? { 'x-opencode-directory': encodeURIComponent(directory) } : {}),
});

/** 带 auth + 目录 header 拉取 JSON；非 2xx 或解析失败一律返回 fallback。 */
const fetchJson = async (url, authHeaders, fallback, directory) => {
  const response = await fetch(url.toString(), {
    headers: { ...authHeaders, ...buildDirectoryHeaders(directory), accept: 'application/json' },
  });
  if (!response.ok) return fallback;
  return response.json().catch(() => fallback);
};

/**
 * 并行拉取模型/agent 选择所需的全部输入：settings（磁盘）、providers、
 * agents、OpenCode config（默认 agent 与默认模型）；任何一项失败回退
 * 空值，不阻塞其余项。
 */
const fetchSelectionInputs = async ({ buildOpenCodeUrl, authHeaders, directory, readSettingsFromDiskMigrated }) => {
  const settings = await readSettingsFromDiskMigrated();
  const providersUrl = new URL(buildOpenCodeUrl('/config/providers', ''));
  providersUrl.searchParams.set('directory', directory);
  const agentsUrl = new URL(buildOpenCodeUrl('/agent', ''));
  agentsUrl.searchParams.set('directory', directory);
  const configUrl = new URL(buildOpenCodeUrl('/config', ''));
  configUrl.searchParams.set('directory', directory);

  const [providersBody, agentsBody, configBody] = await Promise.all([
    fetchJson(providersUrl, authHeaders, { providers: [] }, directory),
    fetchJson(agentsUrl, authHeaders, [], directory),
    fetchJson(configUrl, authHeaders, {}, directory),
  ]);

  return {
    settings,
    providers: Array.isArray(providersBody?.providers) ? providersBody.providers : [],
    agents: Array.isArray(agentsBody) ? agentsBody : [],
    opencodeDefaultAgent: asNonEmptyString(configBody?.default_agent) || asNonEmptyString(configBody?.defaultAgent),
    opencodeDefaultModel: asNonEmptyString(configBody?.model),
  };
};

/**
 * 解析缺省 agent 与模型：agent 依次尝试 settings 默认 → OpenCode 默认
 * （须为主模式且未隐藏）→ build → 第一个主 agent → 第一个 agent；模型
 * 依次尝试 settings 默认（须真实存在）→ 已解析 agent 的模型 → OpenCode
 * 默认 → opencode/big-pickle 兜底 → 第一个 provider 的第一个模型，并尽
 * 力带上匹配的 variant。
 */
const resolveDefaultSelection = ({ agents, providers, settings, opencodeDefaultAgent, opencodeDefaultModel }) => {
  const primaryAgents = agents.filter((agent) => isPrimaryAgentMode(agent?.mode) && agent?.hidden !== true);
  let resolvedAgent = null;
  const settingsDefaultAgent = asNonEmptyString(settings?.defaultAgent);
  if (settingsDefaultAgent) {
    resolvedAgent = agents.find((agent) => agent?.name === settingsDefaultAgent) || null;
  }
  if (!resolvedAgent && opencodeDefaultAgent) {
    const candidate = agents.find((agent) => agent?.name === opencodeDefaultAgent) || null;
    if (candidate && isPrimaryAgentMode(candidate.mode) && candidate.hidden !== true) {
      resolvedAgent = candidate;
    }
  }
  if (!resolvedAgent) {
    resolvedAgent = primaryAgents.find((agent) => agent?.name === 'build') || primaryAgents[0] || agents[0] || null;
  }

  let model = null;
  let variant;
  const settingsDefaultModel = parseConfigModel(settings?.defaultModel);
  if (settingsDefaultModel && hasProviderModel(providers, settingsDefaultModel.providerID, settingsDefaultModel.modelID)) {
    model = settingsDefaultModel;
    variant = resolveVariant(providers, model.providerID, model.modelID, settings?.defaultVariant);
  }

  if (!model && resolvedAgent?.model?.providerID && resolvedAgent?.model?.modelID
    && hasProviderModel(providers, resolvedAgent.model.providerID, resolvedAgent.model.modelID)) {
    model = { providerID: resolvedAgent.model.providerID, modelID: resolvedAgent.model.modelID };
    variant = resolveVariant(providers, model.providerID, model.modelID, resolvedAgent.variant);
  }

  const opencodeModel = parseConfigModel(opencodeDefaultModel);
  if (!model && opencodeModel && hasProviderModel(providers, opencodeModel.providerID, opencodeModel.modelID)) {
    model = opencodeModel;
  }

  if (!model && hasProviderModel(providers, FALLBACK_PROVIDER_ID, FALLBACK_MODEL_ID)) {
    model = { providerID: FALLBACK_PROVIDER_ID, modelID: FALLBACK_MODEL_ID };
  }

  if (!model) {
    const provider = providers[0];
    const firstModel = providerModels(provider)[0];
    if (provider?.id && firstModel?.id) {
      model = { providerID: provider.id, modelID: firstModel.id };
    }
  }

  return {
    agent: resolvedAgent?.name,
    model,
    variant,
  };
};

/** 直接向 OpenCode 的 prompt_async 端点 POST payload；非 2xx 抛带状态码与响应体的错误。 */
const runPromptAsync = async ({ baseUrl, authHeaders, sessionID, directory, payload }) => {
  const promptUrl = new URL(`${baseUrl}/session/${encodeURIComponent(sessionID)}/prompt_async`);
  promptUrl.searchParams.set('directory', directory);
  const response = await fetch(promptUrl.toString(), {
    method: 'POST',
    headers: {
      ...authHeaders,
      ...buildDirectoryHeaders(directory),
      'content-type': 'application/json',
      accept: 'application/json',
    },
    body: JSON.stringify(payload),
  });

  if (!response.ok) {
    const body = await response.text().catch(() => '');
    throw new Error(`prompt_async failed (${response.status})${body ? `: ${body}` : ''}`);
  }
};

/** 经 REST 创建空会话并返回其 id；失败或响应缺 id 抛错。 */
const createSession = async ({ baseUrl, authHeaders, directory, title }) => {
  const sessionUrl = new URL(`${baseUrl}/session`);
  sessionUrl.searchParams.set('directory', directory);
  const response = await fetch(sessionUrl.toString(), {
    method: 'POST',
    headers: {
      ...authHeaders,
      ...buildDirectoryHeaders(directory),
      'content-type': 'application/json',
      accept: 'application/json',
    },
    body: JSON.stringify({ directory, ...(title ? { title } : {}) }),
  });

  if (!response.ok) {
    const body = await response.text().catch(() => '');
    throw new Error(`session create failed (${response.status})${body ? `: ${body}` : ''}`);
  }

  const body = await response.json().catch(() => null);
  const sessionID = body?.id || body?.data?.id;
  if (!sessionID) {
    throw new Error('failed to create session');
  }
  return sessionID;
};

/** 经 SDK 客户端 fork 会话（messageID 可选为分叉边界）；响应缺 id 抛错。 */
const forkSession = async ({ client, sessionID, directory, messageID }) => {
  const response = await client.session.fork({
    sessionID,
    directory,
    ...(messageID ? { messageID } : {}),
  });
  const session = response?.data;
  if (!session?.id) {
    throw new Error('failed to fork session');
  }
  return session;
};

/**
 * 找会话里最近一条已完成的 assistant 消息 id（send 等待的基线）；拉取
 * 失败返回 null，没有已完成 assistant 消息也返回 null。
 */
const latestCompletedAssistantMessageID = async ({ client, sessionID, directory }) => {
  let response;
  try {
    response = await client.session.messages({ sessionID, directory, limit: 100 });
  } catch {
    return null;
  }
  const messages = Array.isArray(response?.data) ? response.data : [];
  let latest = null;
  for (const message of messages) {
    const info = message?.info;
    if (info?.role !== 'assistant' || !Number.isFinite(info?.time?.completed)) continue;
    if (!latest || (info.time.created || 0) >= (latest.time?.created || 0)) latest = info;
  }
  return asNonEmptyString(latest?.id);
};

/**
 * 解析目标目录：payload 带 projectId/projectID 时按项目表查路径（查无
 * 404），否则用 directory 字段；最终都经 validateDirectoryPath 校验（失
 * 败 400）。成功返回 {ok, directory, projectId?}。
 */
const resolveRequestedDirectory = async ({ payload, readSettingsFromDiskMigrated, sanitizeProjects, validateDirectoryPath }) => {
  const projectID = asNonEmptyString(payload?.projectId) || asNonEmptyString(payload?.projectID);
  if (projectID) {
    const settings = await readSettingsFromDiskMigrated();
    const projects = sanitizeProjects(settings?.projects || []);
    const project = projects.find((entry) => entry.id === projectID) || null;
    if (!project?.path) {
      return { ok: false, status: 404, error: 'Project not found' };
    }
    const validated = await validateDirectoryPath(project.path);
    return validated.ok
      ? { ok: true, directory: validated.directory, projectId: projectID }
      : { ok: false, status: 400, error: validated.error || 'Invalid project directory' };
  }

  const directory = asNonEmptyString(payload?.directory);
  const validated = await validateDirectoryPath(directory);
  return validated.ok
    ? { ok: true, directory: validated.directory }
    : { ok: false, status: 400, error: validated.error || 'Invalid directory' };
};

/** 等待 prompt 落盘（出现在会话消息里）的超时（毫秒）。 */
const PROMPT_LANDED_TIMEOUT_MS = 5_000;
/** prompt 落盘轮询间隔（毫秒）。 */
const PROMPT_LANDED_POLL_MS = 150;

// createWorktree returns while the worktree is still being populated in the
// background (git reset --hard after a --no-checkout add). Dispatching a
// prompt into a half-populated directory makes opencode's run die with
// UnknownError (agent and config files are not there yet), so wait until the
// bootstrap reaches git-ready (population done) or fails before creating the
// session and dispatching.
// 中文补充：createWorktree 返回时 worktree 仍在后台填充（--no-checkout
// add 之后的 git reset --hard）。往填了一半的目录里派 prompt 会让
// opencode 的运行以 UnknownError 暴死（agent 与配置文件还没就位），所以
// 创建会话、派发之前必须等到引导达到 git-ready（填充完成）或失败。
const WORKTREE_BOOTSTRAP_TIMEOUT_MS = 60_000;
/** worktree 引导状态轮询间隔（毫秒）。 */
const WORKTREE_BOOTSTRAP_POLL_MS = 150;

/**
 * 轮询 worktree 引导直到 ready/git-ready/setup-ready；status 为 failed
 * 或超时（60s）抛 OMPChamberControlError（500）。
 */
const waitForWorktreeBootstrapReady = async ({ directory }) => {
  const deadline = Date.now() + WORKTREE_BOOTSTRAP_TIMEOUT_MS;
  for (;;) {
    const status = await getWorktreeBootstrapStatus(directory);
    if (status?.status === 'failed') {
      throw new OMPChamberControlError(`Worktree bootstrap failed: ${status.error || 'unknown error'}`, 500);
    }
    const phase = status?.phase;
    if (status?.status === 'ready' || phase === 'git-ready' || phase === 'setup-ready') return;
    if (Date.now() >= deadline) {
      throw new OMPChamberControlError('Timed out waiting for the worktree bootstrap', 500);
    }
    await new Promise((resolve) => setTimeout(resolve, WORKTREE_BOOTSTRAP_POLL_MS));
  }
};

/**
 * 找会话里最近一条 user 消息 id；拉取失败返回 {ok:false}（调用方不应把
 * 查询失败当作 prompt 丢失的证据）。
 */
const latestUserMessageID = async ({ client, sessionID, directory }) => {
  let response;
  try {
    response = await client.session.messages({ sessionID, directory, limit: 100 });
  } catch {
    return { ok: false, messageID: null };
  }
  const messages = Array.isArray(response?.data) ? response.data : [];
  let latest = null;
  for (const message of messages) {
    const info = message?.info;
    if (info?.role !== 'user') continue;
    if (!latest || (info.time?.created || 0) >= (latest.time?.created || 0)) latest = info;
  }
  return { ok: true, messageID: asNonEmptyString(latest?.id) };
};

// `prompt_async` answers 204 as soon as OpenCode forks the run, and every later
// failure is reported only on the session event stream. Confirm the prompt was
// actually recorded so `promptDispatched` never claims a dispatch that vanished.
// 中文补充：prompt_async 在 OpenCode 分叉出运行后立刻回 204，之后的
// 失败只出现在会话事件流上。这里确认 prompt 确实被记录，保证
// promptDispatched 永不虚报一个消失了的派发。
const waitForPromptLanded = async ({ client, sessionID, directory, baselineUserMessageID }) => {
  const deadline = Date.now() + PROMPT_LANDED_TIMEOUT_MS;
  for (;;) {
    const latest = await latestUserMessageID({ client, sessionID, directory });
    // A failed lookup is not authoritative evidence that the prompt was lost.
    if (!latest.ok) return true;
    if (latest.messageID && latest.messageID !== baselineUserMessageID) return true;
    if (Date.now() >= deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, PROMPT_LANDED_POLL_MS));
  }
};

/** 从 payload 提取 worktree 输入；未提供或没有 name 返回 null（name 缺失由调用方报 400）。 */
const resolveWorktreeInput = (payload) => {
  if (!payload?.worktree || typeof payload.worktree !== 'object') return null;
  const name = asNonEmptyString(payload.worktree.name);
  if (!name) return null;
  const branchName = asNonEmptyString(payload.worktree.branchName);
  const startRef = asNonEmptyString(payload.worktree.startRef);
  return {
    mode: 'new',
    name,
    ...(branchName ? { branchName } : {}),
    ...(startRef ? { startRef } : {}),
    ...(typeof payload.setUpstream === 'boolean' ? { setUpstream: payload.setUpstream } : {}),
  };
};

/**
 * 创建 OMPChamber 会话服务（create/send/fork）。dependencies 注入
 * settings 读取、项目清洗、目录校验、OpenCode URL/认证/就绪等待、
 * 会话创建事件广播、可选的 createSessionGoal 覆盖与
 * sessionKnowledgeRuntime（项目背景知识注入）。返回
 * { create, send, fork }，三者共享目录解析与 prompt 派发管线。
 */
export const createOMPChamberSessionService = (dependencies) => {
  const {
    readSettingsFromDiskMigrated,
    sanitizeProjects,
    validateDirectoryPath,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    waitForOpenCodeReady,
    emitSessionCreatedEvent,
    createSessionGoal: createSessionGoalOverride,
    sessionKnowledgeRuntime = null,
  } = dependencies;

  // Last user message of an existing session, as a selection to reuse. Returns
  // null when the session has no user message carrying a model.
  // 中文补充：取既有会话最后一条带模型的 user 消息，作为可复用的选择；
  // 会话里没有携带模型的 user 消息时返回 null。
  const fetchLastUserSelection = async ({ client, sessionID, directory }) => {
    try {
      const response = await client.session.messages({ sessionID, directory, limit: 20 });
      const records = Array.isArray(response?.data) ? response.data : [];
      for (let index = records.length - 1; index >= 0; index -= 1) {
        const info = records[index]?.info;
        if (info?.role !== 'user') continue;
        const providerID = asNonEmptyString(info.model?.providerID);
        const modelID = asNonEmptyString(info.model?.modelID);
        if (!providerID || !modelID) continue;
        return {
          model: { providerID, modelID },
          agent: asNonEmptyString(info.agent),
          variant: asNonEmptyString(info.model?.variant),
        };
      }
    } catch {
    }
    return null;
  };

  // Explicit model/agent/variant are never checked by `prompt_async`: an unknown
  // agent makes the forked run fail silently, leaving a session with no message.
  // Reject them before any session, worktree, or goal side effect happens.
  // 中文补充：显式 model/agent/variant 从不被 prompt_async 校验：未知的
  // agent 会让分叉出的运行静默失败、留下一个没有消息的会话。这里在
  // 任何会话、worktree 或 goal 副作用发生之前先把它们拒掉。
  const validateRequestedSelection = async ({ directory, requestedModel, requestedAgent, requestedVariant }) => {
    if (!requestedModel && !requestedAgent && !requestedVariant) return;
    const authHeaders = getOpenCodeAuthHeaders();
    const { providers, agents } = await fetchSelectionInputs({
      buildOpenCodeUrl,
      authHeaders,
      directory,
      readSettingsFromDiskMigrated,
    });

    // An empty list means the lookup failed or returned nothing authoritative;
    // it must not turn a valid selection into a rejection.
    if (requestedAgent && agents.length > 0) {
      const agent = agents.find((entry) => entry?.name === requestedAgent) || null;
      if (!agent) {
        throw new OMPChamberControlError(`Unknown agent '${requestedAgent}' for ${directory}`, 400);
      }
      if (!isPrimaryAgentMode(agent.mode)) {
        throw new OMPChamberControlError(`Agent '${requestedAgent}' is a subagent and cannot receive a prompt directly`, 400);
      }
    }

    if (requestedModel && providers.length > 0) {
      if (!hasProviderModel(providers, requestedModel.providerID, requestedModel.modelID)) {
        throw new OMPChamberControlError(
          `Unknown model '${requestedModel.providerID}/${requestedModel.modelID}' for ${directory}`,
          400,
        );
      }
      if (requestedVariant
        && !resolveVariant(providers, requestedModel.providerID, requestedModel.modelID, requestedVariant)) {
        throw new OMPChamberControlError(
          `Unknown variant '${requestedVariant}' for model '${requestedModel.providerID}/${requestedModel.modelID}'`,
          400,
        );
      }
    }
  };

  // prompt 派发管线：补齐选择（reuseSessionSelection 时优先复用会话上次
  // 的模型/agent/variant，缺项再走缺省解析，模型仍无则 400）→ 展开片段
  // → 识别 slash 命令 → goal 启用时先创建 goal 元数据（命令则用展开后的
  // 模板为目标）→ 命令走 session.command，普通 prompt 走 prompt_async
  // （前置项目背景知识、goal 引言），随后确认 prompt 落盘；goal 已配置
  // 时的派发失败会打上 goalConfigured 标记。
  const dispatchPrompt = async ({
    client,
    baseUrl,
    authHeaders,
    sessionID,
    directory,
    prompt,
    goalInput,
    requestedModel,
    requestedAgent,
    requestedVariant,
    reuseSessionSelection = false,
  }) => {
    let model = requestedModel;
    let agent = requestedAgent;
    let variant = requestedVariant;
    if (reuseSessionSelection && (!model || !agent)) {
      const previous = await fetchLastUserSelection({ client, sessionID, directory });
      if (previous) {
        if (!model && previous.model) {
          model = previous.model;
          if (variant == null) variant = previous.variant ?? undefined;
        }
        if (!agent && previous.agent) agent = previous.agent;
      }
    }
    if (!model || !agent) {
      const inputs = await fetchSelectionInputs({
        buildOpenCodeUrl,
        authHeaders,
        directory,
        readSettingsFromDiskMigrated,
      });
      const defaults = resolveDefaultSelection(inputs);
      if (!model) {
        model = defaults.model;
        if (variant == null) variant = defaults.variant;
      }
      agent = agent || defaults.agent;
    }
    if (!model) {
      const error = new Error('No model is configured or available for the requested directory');
      error.statusCode = 400;
      throw error;
    }

    const expandedPrompt = expandSnippets(prompt, directory);
    const parsedCommand = parseScheduledCommandPrompt(prompt);
    let resolvedCommand = null;
    if (parsedCommand) {
      try {
        const response = await client.command.list({ directory });
        const commands = Array.isArray(response?.data) ? response.data : [];
        const command = commands.find((candidate) => candidate?.name === parsedCommand.command);
        if (command) resolvedCommand = { ...parsedCommand, template: command.template };
      } catch {
      }
    }
    if (goalInput.enabled) {
      const commandObjective = resolvedCommand
        ? expandCommandGoalObjective(resolvedCommand.template, resolvedCommand.arguments)
        : null;
      await (createSessionGoalOverride || createSessionGoal)({
        baseUrl,
        authHeaders,
        sessionID,
        directory,
        objective: commandObjective ?? expandedPrompt,
        tokenBudget: goalInput.tokenBudget,
        providerID: model.providerID,
        modelID: model.modelID,
        onWarning: (message, error) => console.warn(`[OMPChamberSessions] ${message}:`, error?.message || error),
      });
    }

    // 给派发失败打 goalConfigured 标记：goal 元数据已建，调用方需披露。
    const markGoalPartial = (error) => {
      if (goalInput.enabled && error && typeof error === 'object') error.goalConfigured = true;
      return error;
    };

    if (resolvedCommand) {
      try {
        await client.session.command({
          sessionID,
          directory,
          command: resolvedCommand.command,
          arguments: resolvedCommand.arguments,
          ...(agent ? { agent } : {}),
          model: `${model.providerID}/${model.modelID}`,
          ...(variant ? { variant } : {}),
        });
      } catch (error) {
        throw markGoalPartial(error);
      }
    } else {
      const baseline = await latestUserMessageID({ client, sessionID, directory });
      // A session the agent dispatched has no UI to attach the project's
      // standing context, so it is asked for here. Never fails the dispatch:
      // a session that runs without its background beats one that never runs.
      const knowledge = sessionKnowledgeRuntime
        ? await sessionKnowledgeRuntime.resolvePendingForSession(sessionID, directory)
          .catch(() => ({ text: '', signature: '' }))
        : { text: '', signature: '' };
      try {
        await runPromptAsync({
          baseUrl,
          authHeaders,
          sessionID,
          directory,
          payload: {
            model,
            ...(agent ? { agent } : {}),
            ...(variant ? { variant } : {}),
            parts: [
              ...(knowledge.text ? [{ type: 'text', text: knowledge.text, synthetic: true }] : []),
              { type: 'text', text: expandedPrompt },
              ...(goalInput.enabled
                ? [{ type: 'text', text: buildGoalIntroText(goalInput.tokenBudget), synthetic: true }]
                : []),
            ],
          },
        });
      } catch (error) {
        throw markGoalPartial(error);
      }
      if (knowledge.text && sessionKnowledgeRuntime) {
        // After the prompt is accepted, so a rejected dispatch carries it again.
        await sessionKnowledgeRuntime.recordDelivered(sessionID, directory, knowledge.signature)
          .catch(() => undefined);
      }
      const landed = await waitForPromptLanded({
        client,
        sessionID,
        directory,
        baselineUserMessageID: baseline.messageID,
      });
      if (!landed) {
        return {
          model,
          agent,
          variant,
          promptDispatched: false,
          dispatchedAsCommand: false,
          promptError: 'OpenCode accepted the prompt but it never appeared in the session',
        };
      }
    }

    return { model, agent, variant, promptDispatched: true, dispatchedAsCommand: Boolean(resolvedCommand) };
  };

  // 新建会话：校验 goal 组合与目录/项目（worktree 缺 name 报 400）→ 等
  // 引擎就绪 → 有 prompt 时先校验显式选择（避免无谓副作用）→ 可选建立
  // worktree 并等引导完成 → 创建会话 → 派发 prompt → 组装结果并尽力广
  // 播 session-created 事件（广播失败不影响结果）。
  const create = async (payload = {}) => {
    const title = asNonEmptyString(payload.title);
    const prompt = asNonEmptyString(payload.prompt);
    const goalInput = resolveGoalInput(payload, prompt);
    if (!goalInput.ok) {
      throw new OMPChamberControlError(goalInput.error, 400);
    }
    const model = resolveRequestedModel(payload);
    const agent = asNonEmptyString(payload.agent);
    const variant = asNonEmptyString(payload.variant);

    const resolvedDirectory = await resolveRequestedDirectory({
      payload,
      readSettingsFromDiskMigrated,
      sanitizeProjects,
      validateDirectoryPath,
    });
    if (!resolvedDirectory.ok) {
      throw new OMPChamberControlError(resolvedDirectory.error, resolvedDirectory.status || 400);
    }

    const worktreeInput = resolveWorktreeInput(payload);
    let worktree = null;
    let sessionDirectory = resolvedDirectory.directory;
    if (payload?.worktree && !worktreeInput) {
      throw new OMPChamberControlError('worktree.name is required when worktree is provided', 400);
    }

    if (typeof waitForOpenCodeReady === 'function') await waitForOpenCodeReady(10_000, 250);

    if (prompt) {
      await validateRequestedSelection({
        directory: resolvedDirectory.directory,
        requestedModel: model,
        requestedAgent: agent,
        requestedVariant: variant,
      });
    }

    if (worktreeInput) {
      worktree = await createWorktree(resolvedDirectory.directory, worktreeInput);
      sessionDirectory = worktree.path;
      await waitForWorktreeBootstrapReady({ directory: sessionDirectory });
    }

    const baseUrl = buildOpenCodeUrl('/', '').replace(/\/$/, '');
    const authHeaders = getOpenCodeAuthHeaders();
    const client = createLocalEngineClient({ baseUrl, headers: authHeaders });
    const sessionID = await createSession({
      client,
      baseUrl,
      authHeaders,
      directory: sessionDirectory,
      ...(title ? { title } : {}),
    });

    let dispatch = { model, agent, variant, promptDispatched: false, dispatchedAsCommand: false };
    if (prompt) {
      dispatch = await dispatchPrompt({
        client,
        baseUrl,
        authHeaders,
        sessionID,
        directory: sessionDirectory,
        prompt,
        goalInput,
        requestedModel: model,
        requestedAgent: agent,
        requestedVariant: variant,
      });
    }

    const result = {
      sessionId: sessionID,
      directory: sessionDirectory,
      ...(resolvedDirectory.projectId ? { projectId: resolvedDirectory.projectId } : {}),
      ...(title ? { title } : {}),
      ...(worktree ? { worktree } : {}),
      ...(prompt && dispatch.model ? { model: dispatch.model } : {}),
      ...(prompt && dispatch.agent ? { agent: dispatch.agent } : {}),
      ...(prompt && dispatch.variant ? { variant: dispatch.variant } : {}),
      promptDispatched: dispatch.promptDispatched,
      ...(dispatch.promptError ? { promptError: dispatch.promptError } : {}),
      dispatchedAsCommand: dispatch.dispatchedAsCommand,
      ...(goalInput.enabled ? { goalEnabled: true } : {}),
      ...(goalInput.tokenBudget ? { goalTokenBudget: goalInput.tokenBudget } : {}),
    };

    try {
      emitSessionCreatedEvent?.({
        sessionID,
        directory: sessionDirectory,
        ...(resolvedDirectory.projectId ? { projectID: resolvedDirectory.projectId } : {}),
        ...(title ? { title } : {}),
        ...(worktree ? { worktree } : {}),
        ...(prompt && dispatch.model ? { model: dispatch.model } : {}),
        ...(prompt && dispatch.agent ? { agent: dispatch.agent } : {}),
        ...(prompt && dispatch.variant ? { variant: dispatch.variant } : {}),
        promptDispatched: dispatch.promptDispatched,
        dispatchedAsCommand: dispatch.dispatchedAsCommand,
        ...(goalInput.enabled ? { goalEnabled: true } : {}),
        ...(goalInput.tokenBudget ? { goalTokenBudget: goalInput.tokenBudget } : {}),
        createdAt: Date.now(),
      });
    } catch {
    }

    return result;
  };

  // 对既有会话执行 send/fork：要求 sessionId 与 prompt（400），先解析目
  // 录、校验选择，fork 时先分叉（messageId 为边界）并记录基线 assistant
  // 消息，再复用会话既有选择派发；fork 成功后广播 session-created。任
  // 何失败统一转 OMPChamberControlError，且 fork 已建或 goal 已配置时附
  // partial 详情，让调用方知道留下了什么。
  const runExisting = async (action, sourceSessionId, payload = {}) => {
    const sourceSessionID = asNonEmptyString(sourceSessionId);
    const prompt = asNonEmptyString(payload.prompt);
    if (!sourceSessionID) throw new OMPChamberControlError('sessionId is required', 400);
    if (!prompt) throw new OMPChamberControlError('prompt is required', 400);
    const goalInput = resolveGoalInput(payload, prompt);
    if (!goalInput.ok) throw new OMPChamberControlError(goalInput.error, 400);
    const requestedModel = resolveRequestedModel(payload);

    let targetSessionID = sourceSessionID;
    let targetSession = null;
    let directory = null;
    try {
      const resolvedDirectory = await resolveRequestedDirectory({
        payload,
        readSettingsFromDiskMigrated,
        sanitizeProjects,
        validateDirectoryPath,
      });
      if (!resolvedDirectory.ok) {
        throw new OMPChamberControlError(resolvedDirectory.error, resolvedDirectory.status || 400);
      }
      directory = resolvedDirectory.directory;
      if (typeof waitForOpenCodeReady === 'function') await waitForOpenCodeReady(10_000, 250);

      await validateRequestedSelection({
        directory,
        requestedModel,
        requestedAgent: asNonEmptyString(payload.agent),
        requestedVariant: asNonEmptyString(payload.variant),
      });

      const baseUrl = buildOpenCodeUrl('/', '').replace(/\/$/, '');
      const authHeaders = getOpenCodeAuthHeaders();
      const client = createLocalEngineClient({ baseUrl, headers: authHeaders });
      if (action === 'fork') {
        targetSession = await forkSession({
          client,
          sessionID: sourceSessionID,
          directory,
          messageID: asNonEmptyString(payload.messageId) || undefined,
        });
        targetSessionID = targetSession.id;
      }

      const baselineAssistantMessageId = await latestCompletedAssistantMessageID({
        client,
        sessionID: targetSessionID,
        directory,
      });

      const dispatch = await dispatchPrompt({
        client,
        baseUrl,
        authHeaders,
        sessionID: targetSessionID,
        directory,
        prompt,
        goalInput,
        requestedModel,
        requestedAgent: asNonEmptyString(payload.agent),
        requestedVariant: asNonEmptyString(payload.variant),
        reuseSessionSelection: true,
      });
      const result = {
        action,
        sessionId: targetSessionID,
        directory,
        ...(action === 'fork' ? { sourceSessionId: sourceSessionID } : {}),
        ...(targetSession?.title ? { title: targetSession.title } : {}),
        ...(baselineAssistantMessageId ? { baselineAssistantMessageId } : {}),
        model: dispatch.model,
        ...(dispatch.agent ? { agent: dispatch.agent } : {}),
        ...(dispatch.variant ? { variant: dispatch.variant } : {}),
        promptDispatched: dispatch.promptDispatched,
        ...(dispatch.promptError ? { promptError: dispatch.promptError } : {}),
        dispatchedAsCommand: dispatch.dispatchedAsCommand,
        ...(goalInput.enabled ? { goalEnabled: true } : {}),
        ...(goalInput.tokenBudget ? { goalTokenBudget: goalInput.tokenBudget } : {}),
      };

      if (action === 'fork') {
        try {
          emitSessionCreatedEvent?.({
            sessionID: targetSessionID,
            directory,
            sourceSessionID,
            ...(targetSession?.title ? { title: targetSession.title } : {}),
            model: dispatch.model,
            ...(dispatch.agent ? { agent: dispatch.agent } : {}),
            ...(dispatch.variant ? { variant: dispatch.variant } : {}),
            promptDispatched: dispatch.promptDispatched,
            dispatchedAsCommand: dispatch.dispatchedAsCommand,
            ...(goalInput.enabled ? { goalEnabled: true } : {}),
            ...(goalInput.tokenBudget ? { goalTokenBudget: goalInput.tokenBudget } : {}),
            createdAt: Date.now(),
          });
        } catch {
        }
      }
      return result;
    } catch (error) {
      const statusCode = Number(error?.statusCode) || 500;
      const forkCreated = action === 'fork' && targetSessionID !== sourceSessionID;
      const goalConfigured = error?.goalConfigured === true;
      throw new OMPChamberControlError(
        error instanceof Error ? error.message : `Failed to ${action} session`,
        statusCode,
        {
        ...(forkCreated || goalConfigured
          ? {
            partial: true,
            partialAction: forkCreated ? 'fork-created' : 'goal-configured',
            sessionId: targetSessionID,
            directory,
          }
          : {}),
        },
      );
    }
  };

  return {
    create,
    // 对既有会话发送新 prompt（复用会话上次的选择）。
    send: (sessionID, payload) => runExisting('send', sessionID, payload),
    // 从 messageId（缺省为末尾）分叉会话并发送新 prompt。
    fork: (sessionID, payload) => runExisting('fork', sessionID, payload),
  };
};

/** 把任意错误写入统一 JSON 错误响应：statusCode + 文案，partial 时附部分结果详情。 */
const sendServiceError = (res, error, fallback) => {
  const controlError = asControlError(error, fallback);
  return res.status(controlError.statusCode).json({
    error: controlError.message,
    ...(controlError.partial === true ? {
      partial: true,
      partialAction: controlError.partialAction,
      sessionId: controlError.sessionId,
      directory: controlError.directory,
    } : {}),
  });
};

/**
 * 在 express app 上注册会话路由：POST /api/ompchamber/sessions（创建）、
 * /:sessionId/send 与 /:sessionId/fork（body 限 1MB）。dependencies 可
 * 直接注入 sessionService（测试），否则现场用其余依赖构造。
 */
export const registerOMPChamberSessionRoutes = (app, dependencies) => {
  // 优先使用注入的服务（测试桩）；缺省现场装配。
  const service = dependencies.sessionService || createOMPChamberSessionService(dependencies);

  // 创建会话：body 非对象时按空 payload 处理；失败记日志并走统一错误响应。
  app.post('/api/ompchamber/sessions', express.json({ limit: '1mb' }), async (req, res) => {
    try {
      return res.json(await service.create(req.body && typeof req.body === 'object' ? req.body : {}));
    } catch (error) {
      console.error('[OMPChamberSessions] failed to create session:', error);
      return sendServiceError(res, error, 'Failed to create session');
    }
  });

  // 向既有会话发送 prompt；:sessionId 取自路径参数。
  app.post(
    '/api/ompchamber/sessions/:sessionId/send',
    express.json({ limit: '1mb' }),
    async (req, res) => {
      try {
        return res.json(await service.send(req.params.sessionId, req.body));
      } catch (error) {
        console.error('[OMPChamberSessions] failed to send session:', error);
        return sendServiceError(res, error, 'Failed to send session');
      }
    },
  );
  // 分叉既有会话并发送 prompt；:sessionId 取自路径参数。
  app.post(
    '/api/ompchamber/sessions/:sessionId/fork',
    express.json({ limit: '1mb' }),
    async (req, res) => {
      try {
        return res.json(await service.fork(req.params.sessionId, req.body));
      } catch (error) {
        console.error('[OMPChamberSessions] failed to fork session:', error);
        return sendServiceError(res, error, 'Failed to fork session');
      }
    },
  );
};
