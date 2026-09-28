/**
 * OMPChamber 控制服务的动作分发核心（openchamber-control/service）。
 *
 * createOMPChamberControlService 把 settings 读取、OpenCode 本地引擎客
 * 户端、会话服务、计划任务服务、浏览器控制与 agent 记忆动作装配成一
 * 个 execute(action, input, contextDirectory, options) 入口：校验入参、
 * 解析目录/项目作用域、按动作前缀路由（memory./browser./schedule./
 * session./projects./models.），并统一把异常归一为 OMPChamberControlError。
 * 等待类参数（wait/timeout/lastAssistant）在此归一，浏览器截图在服务
 * 端写入项目目录后只回传可引用的路径。
 */
import path from 'node:path';
import { createLocalEngineClient } from '../opencode/local-engine-client.js';
import { OMPChamberControlError, asControlError } from './error.js';
import { OMPCHAMBER_ALL_ACTIONS } from './actions.js';
import { writeScreenshot } from './screenshots.js';

/** wait 模式的默认超时（秒）。 */
const DEFAULT_WAIT_TIMEOUT_SECONDS = 600;
/** wait 模式允许的最大超时（秒）——24 小时。 */
const MAX_WAIT_TIMEOUT_SECONDS = 86_400;
/** 等待会话空闲时的轮询间隔（毫秒）。 */
const WAIT_POLL_INTERVAL_MS = 500;
// One service, both capabilities: which tool asked is the caller's concern.
// 中文补充：一个服务同时承载全部能力；"是哪个工具在问"由调用方自理，
// 这里只认动作名本身。
const CONTROL_ACTIONS = new Set(OMPCHAMBER_ALL_ACTIONS);
/** 必须携带 taskId 的计划任务动作集合（run/delete/toggle）。 */
const SCHEDULE_TASK_ID_ACTIONS = new Set([
  'schedule.run',
  'schedule.delete',
  'schedule.toggle',
]);

/** 规整可选字符串：trim 后非空返回 trim 结果，否则返回 null。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/**
 * 解析正整数参数：undefined/null 回退 fallback；非安全整数或小于 1
 * 抛 OMPChamberControlError（400，文案点名 field）。
 */
const positiveInteger = (value, fallback, field) => {
  if (value === undefined || value === null) return fallback;
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 1) {
    throw new OMPChamberControlError(`${field} must be a positive integer`, 400);
  }
  return number;
};

/**
 * 把秒级 timeout 归一为毫秒：缺省用 DEFAULT_WAIT_TIMEOUT_SECONDS；
 * 非整数、小于 1 或超过 MAX_WAIT_TIMEOUT_SECONDS 抛 400。
 */
const normalizeWaitTimeoutMs = (value) => {
  const seconds = value === undefined || value === null ? DEFAULT_WAIT_TIMEOUT_SECONDS : Number(value);
  if (!Number.isSafeInteger(seconds) || seconds < 1 || seconds > MAX_WAIT_TIMEOUT_SECONDS) {
    throw new OMPChamberControlError(`timeout must be from 1 to ${MAX_WAIT_TIMEOUT_SECONDS} seconds`, 400);
  }
  return seconds * 1000;
};

/**
 * 把 OpenCode 消息响应投影为纯文本消息数组：只保留 user/assistant 角色
 * （可按 role 过滤），拼接 parts 里的有序 text 块并 trim，丢弃无文本的
 * 行；附带 id/时间/model（provider/model 形式），按 createdAt 升序返回。
 */
const extractTextMessages = (messages, role = 'all') => {
  const result = [];
  for (const record of Array.isArray(messages) ? messages : []) {
    const info = record?.info;
    const messageRole = info?.role;
    if ((messageRole !== 'user' && messageRole !== 'assistant') || (role !== 'all' && role !== messageRole)) continue;
    const text = Array.isArray(record?.parts)
      ? record.parts.filter((part) => part?.type === 'text' && typeof part.text === 'string').map((part) => part.text).join('').trim()
      : '';
    if (!text) continue;
    const providerID = asNonEmptyString(info.providerID);
    const modelID = asNonEmptyString(info.modelID);
    result.push({
      id: asNonEmptyString(info.id) || '',
      role: messageRole,
      createdAt: Number.isFinite(info?.time?.created) ? info.time.created : null,
      completedAt: Number.isFinite(info?.time?.completed) ? info.time.completed : null,
      model: providerID && modelID ? `${providerID}/${modelID}` : null,
      text,
    });
  }
  return result.sort((left, right) => (left.createdAt || 0) - (right.createdAt || 0));
};

/** 解析 "provider/model" 字符串；缺省或格式非法抛 400，成功返回 {providerID, modelID}。 */
const parseModel = (value) => {
  const model = asNonEmptyString(value);
  if (!model) throw new OMPChamberControlError('model is required', 400);
  const slashIndex = model.indexOf('/');
  if (slashIndex <= 0 || slashIndex === model.length - 1) {
    throw new OMPChamberControlError('model must be in provider/model format', 400);
  }
  return { providerID: model.slice(0, slashIndex), modelID: model.slice(slashIndex + 1) };
};

/**
 * 解析 weekly 选择器（逗号分隔的 0-6 星期数字）；缺省或越界抛 400，
 * 返回去重升序数组。
 */
const parseWeekdays = (value) => {
  const raw = asNonEmptyString(value);
  if (!raw) throw new OMPChamberControlError('weekly is required', 400);
  const weekdays = raw.split(',').map((entry) => Number.parseInt(entry.trim(), 10));
  if (weekdays.some((entry) => !Number.isInteger(entry) || entry < 0 || entry > 6)) {
    throw new OMPChamberControlError('weekly must contain weekdays from 0 to 6', 400);
  }
  return Array.from(new Set(weekdays)).sort((a, b) => a - b);
};

/**
 * 从输入构建计划任务调度对象：daily/weekly/once/cron 四选一（多了或
 * 都没有抛 400）；weekly/once 还要求 time，timezone 可选附带。
 */
const buildSchedule = (input) => {
  const daily = asNonEmptyString(input.daily);
  const weekly = asNonEmptyString(input.weekly);
  const once = asNonEmptyString(input.once);
  const cron = asNonEmptyString(input.cron);
  const selectors = [daily, weekly, once, cron].filter(Boolean);
  if (selectors.length !== 1) {
    throw new OMPChamberControlError('Provide exactly one of daily, weekly, once, or cron', 400);
  }
  const timezone = asNonEmptyString(input.timezone);
  if (daily) return { kind: 'daily', times: [daily], ...(timezone ? { timezone } : {}) };
  if (weekly) {
    const time = asNonEmptyString(input.time);
    if (!time) throw new OMPChamberControlError('time is required with weekly', 400);
    return { kind: 'weekly', weekdays: parseWeekdays(weekly), times: [time], ...(timezone ? { timezone } : {}) };
  }
  if (once) {
    const time = asNonEmptyString(input.time);
    if (!time) throw new OMPChamberControlError('time is required with once', 400);
    return { kind: 'once', date: once, time, ...(timezone ? { timezone } : {}) };
  }
  return { kind: 'cron', cron, ...(timezone ? { timezone } : {}) };
};

/**
 * 从输入构建完整计划任务：要求 name 与 prompt（400），model 经
 * parseModel 解析；goalTokenBudget 必须搭配 goal 且在 1000 到
 * 100_000_000 之间；disabled 取反为 enabled，产出 schedule + execution。
 */
const buildScheduledTask = (input) => {
  const name = asNonEmptyString(input.name);
  const prompt = asNonEmptyString(input.prompt);
  if (!name) throw new OMPChamberControlError('name is required', 400);
  if (!prompt) throw new OMPChamberControlError('prompt is required', 400);
  const model = parseModel(input.model);
  const goalTokenBudget = input.goalTokenBudget;
  if (goalTokenBudget !== undefined && input.goal !== true) {
    throw new OMPChamberControlError('goalTokenBudget requires goal', 400);
  }
  if (goalTokenBudget !== undefined && (!Number.isSafeInteger(goalTokenBudget) || goalTokenBudget < 1000 || goalTokenBudget > 100_000_000)) {
    throw new OMPChamberControlError('goalTokenBudget must be from 1000 to 100000000', 400);
  }
  return {
    name,
    enabled: input.disabled !== true,
    schedule: buildSchedule(input),
    execution: {
      prompt,
      ...model,
      ...(asNonEmptyString(input.agent) ? { agent: input.agent.trim() } : {}),
      ...(asNonEmptyString(input.variant) ? { variant: input.variant.trim() } : {}),
      ...(input.goal === true ? { goalEnabled: true } : {}),
      ...(goalTokenBudget !== undefined ? { goalTokenBudget } : {}),
    },
  };
};

/**
 * 创建 OMPChamber 控制服务。dependencies 注入运行环境（settings 读取、
 * 项目清洗、OpenCode URL/认证头/就绪等待、sessionService、
 * scheduledTaskService、可选 browserControl 与 agentMemoryActions）；测
 * 试可替换 createClient/sleep/now。返回 { execute }。
 */
export const createOMPChamberControlService = (dependencies) => {
  const {
    readSettingsFromDiskMigrated,
    sanitizeProjects,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    waitForOpenCodeReady,
    sessionService,
    scheduledTaskService,
    browserControl = null,
    agentMemoryActions = null,
    createClient = createLocalEngineClient,
    sleep = (duration) => new Promise((resolve) => setTimeout(resolve, duration)),
    now = Date.now,
  } = dependencies;

  // 可中断的 sleep：signal 已中止或中止时立刻以 499（客户端取消）拒绝，
  // 否则等到 duration 结束；监听器在两条出路都会被摘除。
  const wait = (duration, signal) => {
    if (!signal) return sleep(duration);
    if (signal.aborted) return Promise.reject(new OMPChamberControlError('OMPChamber action was cancelled', 499));
    return new Promise((resolve, reject) => {
      const onAbort = () => {
        signal.removeEventListener('abort', onAbort);
        reject(new OMPChamberControlError('OMPChamber action was cancelled', 499));
      };
      signal.addEventListener('abort', onAbort, { once: true });
      sleep(duration).then(() => {
        signal.removeEventListener('abort', onAbort);
        resolve();
      }, (error) => {
        signal.removeEventListener('abort', onAbort);
        reject(error);
      });
    });
  };

  // 取得指向本地 OpenCode 引擎的客户端：先等引擎就绪（10s 上限、
  // 250ms 轮询），再用基础 URL 与认证头构造。
  const getClient = async () => {
    if (typeof waitForOpenCodeReady === 'function') await waitForOpenCodeReady(10_000, 250);
    return createClient({
      baseUrl: buildOpenCodeUrl('/', '').replace(/\/$/, ''),
      headers: getOpenCodeAuthHeaders(),
    });
  };

  // projects.list 的数据源：读取磁盘 settings、清洗项目列表，投影为
  // id/绝对路径/label（缺省回退路径 basename）。
  const projects = async () => {
    const settings = await readSettingsFromDiskMigrated();
    return sanitizeProjects(settings?.projects || []).map((project) => ({
      id: project.id,
      path: path.resolve(project.path),
      label: asNonEmptyString(project.label) || path.basename(project.path) || project.path,
    }));
  };

  // models.list 的数据源：读取 settings 中的默认模型/变体/agent、收藏
  // 与最近模型，缺字段回退空值或空数组。
  const models = async () => {
    const settings = await readSettingsFromDiskMigrated();
    return {
      defaultModel: asNonEmptyString(settings?.defaultModel),
      defaultVariant: asNonEmptyString(settings?.defaultVariant),
      defaultAgent: asNonEmptyString(settings?.defaultAgent),
      favoriteModels: Array.isArray(settings?.favoriteModels) ? settings.favoriteModels : [],
      recentModels: Array.isArray(settings?.recentModels) ? settings.recentModels : [],
    };
  };

  // 查询单个会话状态：取 directory 作用域的状态表，找不到该会话时回
  // 退 {type:'idle'}；响应形状非法抛 500。
  const sessionStatus = async (client, sessionID, directory) => {
    const response = await client.session.status({ directory });
    const statuses = response?.data;
    if (!statuses || typeof statuses !== 'object' || Array.isArray(statuses)) {
      throw new OMPChamberControlError('Invalid session status response', 500);
    }
    return statuses[sessionID] || { type: 'idle' };
  };

  // 读取会话文本消息：limit 存在时先按 limit*4（下限 100）拉取，若提
  // 取后仍不足 limit 且原始行也已到拉取上限，则无上限重拉一次再截尾。
  const sessionMessages = async (client, sessionID, directory, role, limit) => {
    const fetchLimit = limit === undefined ? undefined : Math.max(100, limit * 4);
    let response = await client.session.messages({ sessionID, directory, ...(fetchLimit ? { limit: fetchLimit } : {}) });
    let raw = Array.isArray(response?.data) ? response.data : [];
    let messages = extractTextMessages(raw, role);
    if (limit !== undefined && messages.length < limit && raw.length >= fetchLimit) {
      response = await client.session.messages({ sessionID, directory });
      raw = Array.isArray(response?.data) ? response.data : [];
      messages = extractTextMessages(raw, role);
    }
    return limit === undefined ? messages : messages.slice(-limit);
  };

  // 轮询等待会话离开 busy/retry：requireActivity 时必须先观察到活动或
  // 出现新的已完成 assistant 消息（相对 baselineMessageID 或 startedAt）
  // 才算空闲；超时抛 500，signal 中止抛 499。
  const waitForIdle = async ({ client, sessionID, directory, timeoutMs, requireActivity, baselineMessageID, startedAt, signal }) => {
    const deadline = now() + timeoutMs;
    let observedActivity = false;
    while (true) {
      if (signal?.aborted) throw new OMPChamberControlError('OMPChamber action was cancelled', 499);
      const status = await sessionStatus(client, sessionID, directory);
      if (status.type === 'busy' || status.type === 'retry') {
        observedActivity = true;
      } else if (!requireActivity || observedActivity) {
        return status;
      } else {
        const messages = await sessionMessages(client, sessionID, directory, 'assistant', 1);
        const message = messages[0];
        if (message?.completedAt && (baselineMessageID ? message.id !== baselineMessageID : message.completedAt >= startedAt)) {
          return status;
        }
      }
      const remaining = deadline - now();
      if (remaining <= 0) {
        throw new OMPChamberControlError(`Session did not become idle within ${Math.ceil(timeoutMs / 1000)} seconds`, 500);
      }
      await wait(Math.min(WAIT_POLL_INTERVAL_MS, remaining), signal);
    }
  };

  // session.send/fork default the directory to the caller's context directory,
  // which is wrong for sessions living in other worktrees: prompt_async then
  // targets an instance that does not hold the session and the run dies with
  // UnknownError. Resolve the target session's directory from the global
  // session list when the caller did not scope explicitly.
  // 中文补充：session.send/fork 缺省用调用方的上下文目录，但对住在
  // 其它 worktree 里的会话是错的：prompt_async 会打到不持有该会话的
  // 实例上，运行以 UnknownError 告终。调用方未显式指定作用域时，从
  // 全局会话列表解析目标会话自己的目录。
  const resolveSessionDirectory = async (sessionID) => {
    try {
      const client = await getClient();
      const response = await client.experimental?.session?.list?.({});
      const sessions = Array.isArray(response?.data) ? response.data : [];
      const session = sessions.find((item) => item?.id === sessionID);
      return asNonEmptyString(session?.directory) || null;
    } catch {
      return null;
    }
  };

  // session.create/send/fork 的公共执行路径：先校验 wait 修饰参数组合，
  // 再确定 directory（显式 > 上下文 > 从全局会话列表解析），组装 payload
  // 后委托 sessionService；wait:true 时等待空闲并可选带回 lastAssistant，
  // 对外结果一律剥掉内部的 baselineAssistantMessageId。
  const executeSessionAction = async (action, input, contextDirectory, signal) => {
    if (input.timeout !== undefined && input.wait !== true) throw new OMPChamberControlError('timeout requires wait', 400);
    if (input.lastAssistant === true && input.wait !== true) throw new OMPChamberControlError('lastAssistant requires wait', 400);
    const sessionID = asNonEmptyString(input.sessionId);
    let directory = asNonEmptyString(input.directory) || (!input.projectId ? asNonEmptyString(contextDirectory) : null);
    if (sessionID && action !== 'session.create' && !asNonEmptyString(input.directory) && !input.projectId) {
      const resolvedSessionDirectory = await resolveSessionDirectory(sessionID);
      if (resolvedSessionDirectory) directory = resolvedSessionDirectory;
    }
    const payload = {
      ...(directory ? { directory } : {}),
      ...(asNonEmptyString(input.projectId) ? { projectId: input.projectId.trim() } : {}),
      ...(asNonEmptyString(input.title) ? { title: input.title.trim() } : {}),
      ...(asNonEmptyString(input.prompt) ? { prompt: input.prompt.trim() } : {}),
      ...(asNonEmptyString(input.model) ? { model: input.model.trim() } : {}),
      ...(asNonEmptyString(input.agent) ? { agent: input.agent.trim() } : {}),
      ...(asNonEmptyString(input.variant) ? { variant: input.variant.trim() } : {}),
      ...(input.goal === true ? { goal: true } : {}),
      ...(input.goalTokenBudget !== undefined ? { goalTokenBudget: input.goalTokenBudget } : {}),
      ...(asNonEmptyString(input.worktree) ? { worktree: {
        name: input.worktree.trim(),
        ...(asNonEmptyString(input.branch) ? { branchName: input.branch.trim() } : {}),
        ...(asNonEmptyString(input.startRef) ? { startRef: input.startRef.trim() } : {}),
      } } : {}),
      ...(typeof input.setUpstream === 'boolean' ? { setUpstream: input.setUpstream } : {}),
      ...(asNonEmptyString(input.messageId) ? { messageId: input.messageId.trim() } : {}),
    };
    const startedAt = now();
    let result;
    if (action === 'session.create') {
      result = await sessionService.create(payload);
    } else {
      if (!sessionID) throw new OMPChamberControlError('sessionId is required', 400);
      if (action === 'session.send') {
        result = await sessionService.send(sessionID, payload);
      } else {
        result = await sessionService.fork(sessionID, payload);
      }
    }
    if (input.wait !== true) {
      const publicResult = { ...result };
      delete publicResult.baselineAssistantMessageId;
      return publicResult;
    }
    const client = await getClient();
    const status = await waitForIdle({
      client,
      sessionID: result.sessionId,
      directory: result.directory,
      timeoutMs: normalizeWaitTimeoutMs(input.timeout),
      requireActivity: result.promptDispatched === true,
      baselineMessageID: result.baselineAssistantMessageId,
      startedAt,
      signal,
    });
    const publicResult = { ...result, sessionStatus: status };
    delete publicResult.baselineAssistantMessageId;
    if (input.lastAssistant === true) {
      publicResult.lastAssistantMessage = (await sessionMessages(client, result.sessionId, result.directory, 'assistant', 1))[0] || null;
    }
    return publicResult;
  };

  /**
   * Validates browser inputs here rather than in the renderer: an invalid call
   * should come back as a usage error the agent can correct, without waking a
   * client or waiting for a round trip.
   */
  // 中文补充：浏览器输入在服务端而非渲染器校验：非法调用应当作为
  // agent 可自行修正的用法错误立刻返回，而不是唤醒客户端再等一个
  // 往返。各动作要求的参数见下方逐段校验；超时上 browser.open 独享
  // 45s（要等导航落定），其余 20s。
  const browserAction = async (action, input, signal, contextDirectory) => {
    const parameters = {};

    // 校验 viewport 值：required 为真时缺省报错；合法值固定为
    // mobile/tablet/desktop/fill。
    const readViewport = (required) => {
      const viewport = asNonEmptyString(input.viewport);
      if (!viewport) {
        if (required) throw new OMPChamberControlError('viewport is required for browser.resize', 400);
        return;
      }
      if (!['mobile', 'tablet', 'desktop', 'fill'].includes(viewport)) {
        throw new OMPChamberControlError('viewport must be mobile, tablet, desktop, or fill', 400);
      }
      parameters.viewport = viewport;
    };

    if (action === 'browser.resize') readViewport(true);

    if (action === 'browser.capture') {
      const label = asNonEmptyString(input.label);
      if (label) parameters.label = label;
    }

    if (action === 'browser.open') {
      readViewport(false);
      const url = asNonEmptyString(input.url);
      if (!url) throw new OMPChamberControlError('url is required for browser.open', 400);
      let parsed;
      try {
        parsed = new URL(url);
      } catch {
        throw new OMPChamberControlError('url must be an absolute http(s) URL', 400);
      }
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
        throw new OMPChamberControlError('url must use http or https', 400);
      }
      parameters.url = parsed.toString();
    }


    if (action === 'browser.click') {
      const selector = asNonEmptyString(input.selector);
      const text = asNonEmptyString(input.text);
      if (!selector && !text) {
        throw new OMPChamberControlError('browser.click requires selector or text', 400);
      }
      if (selector) parameters.selector = selector;
      if (text) parameters.text = text;
    }

    if (action === 'browser.snapshot') {
      const selector = asNonEmptyString(input.selector);
      if (selector) parameters.selector = selector;
    }

    if (action === 'browser.inspect') {
      const selector = asNonEmptyString(input.selector);
      if (!selector) throw new OMPChamberControlError('selector is required for browser.inspect', 400);
      parameters.selector = selector;
    }

    if (action === 'browser.type') {
      const selector = asNonEmptyString(input.selector);
      if (!selector) throw new OMPChamberControlError('selector is required for browser.type', 400);
      if (typeof input.value !== 'string') {
        throw new OMPChamberControlError('value is required for browser.type', 400);
      }
      parameters.selector = selector;
      parameters.value = input.value;
      parameters.submit = input.submit === true;
    }

    if (action === 'browser.scroll') {
      const selector = asNonEmptyString(input.selector);
      const direction = asNonEmptyString(input.direction);
      if (!selector && !direction) {
        throw new OMPChamberControlError('browser.scroll requires direction or selector', 400);
      }
      if (direction && !['up', 'down', 'top', 'bottom'].includes(direction)) {
        throw new OMPChamberControlError('direction must be up, down, top, or bottom', 400);
      }
      if (selector) parameters.selector = selector;
      if (direction) parameters.direction = direction;
    }

    // Opening a page waits for the navigation to settle, so its budget has to
    // exceed the client's own wait; sharing one timeout with the quick actions
    // made a slow page indistinguishable from an unreachable browser.
    const timeoutMs = action === 'browser.open' ? 45_000 : 20_000;
    const result = await browserControl.request(action, parameters, { signal, timeoutMs });

    // The image is written here rather than in the renderer: the file belongs
    // beside the code it documents, and the client that took it may be on a
    // different machine than the repository.
    if (action === 'browser.capture') {
      const directory = asNonEmptyString(input.directory) || asNonEmptyString(contextDirectory);
      if (!directory) {
        throw new OMPChamberControlError('directory is required to save a screenshot', 400);
      }
      const capture = result && typeof result === 'object' ? result : {};
      const saved = await writeScreenshot({
        directory,
        base64: capture.base64,
        mime: capture.mime,
        label: input.label,
      });
      // The base64 never goes back to the caller: it is large, and the path is
      // what an answer, a commit, or a review can actually use.
      return {
        path: saved.path,
        // Saving the file is only half of showing it. Chat collects the image
        // paths written in a finished answer and renders them below it, so the
        // agent is told the one thing it cannot infer: that writing the path is
        // what puts the picture in front of the user.
        hint: `Write ![](${saved.path}) in your reply to show this image to the user; it is rendered under your message.`,
        url: capture.url ?? null,
        title: capture.title ?? null,
        viewport: capture.viewport ?? null,
        width: capture.width ?? null,
        height: capture.height ?? null,
      };
    }

    return result;
  };

  // 总分发：白名单校验 → memory./browser. 前缀委托（未配置时 503）→
  // projects/models/schedule 系列直连各服务 → session 系列走引擎客户端
  // 或 executeSessionAction；任何异常统一经 asControlError 归一。
  const execute = async (action, input = {}, contextDirectory, options = {}) => {
    try {
      if (!CONTROL_ACTIONS.has(action)) {
        throw new OMPChamberControlError(`Unsupported OMPChamber action: ${action || 'missing'}`, 400);
      }
      if (action.startsWith('memory.')) {
        if (!agentMemoryActions) {
          throw new OMPChamberControlError('Agent memory is not available on this server', 503);
        }
        return agentMemoryActions.execute(action, input, contextDirectory);
      }
      if (action.startsWith('browser.')) {
        if (!browserControl) {
          throw new OMPChamberControlError('The in-app browser is not available on this server', 503);
        }
        return browserAction(action, input, options.signal, contextDirectory);
      }
      if (action === 'projects.list') return { projects: await projects() };
      if (action === 'models.list') return models();
      if (action === 'schedule.status') return scheduledTaskService.status();
      if (action.startsWith('schedule.')) {
        const taskID = asNonEmptyString(input.taskId);
        if (SCHEDULE_TASK_ID_ACTIONS.has(action) && !taskID) {
          throw new OMPChamberControlError('taskId is required', 400);
        }
        const explicitProjectID = asNonEmptyString(input.projectId);
        const explicitDirectory = asNonEmptyString(input.directory);
        const contextDirectoryFallback = explicitProjectID
          ? undefined
          : asNonEmptyString(contextDirectory) || undefined;
        const projectID = await scheduledTaskService.resolveProjectID({
          projectId: explicitProjectID || undefined,
          directory: explicitDirectory || contextDirectoryFallback,
        });
        switch (action) {
          case 'schedule.list':
            return { scheduler: await scheduledTaskService.status(), tasks: await scheduledTaskService.list(projectID) };
          case 'schedule.create': {
            const result = await scheduledTaskService.upsert(projectID, buildScheduledTask(input));
            return { task: result.task, created: result.created };
          }
          case 'schedule.run':
            return scheduledTaskService.run(projectID, taskID);
          case 'schedule.delete':
            return { deleted: true, tasks: await scheduledTaskService.remove(projectID, taskID) };
          case 'schedule.toggle': {
            if (typeof input.disabled !== 'boolean') {
              throw new OMPChamberControlError('disabled is required for schedule.toggle', 400);
            }
            const enabled = input.disabled === false;
            return { task: await scheduledTaskService.setEnabled(projectID, taskID, enabled), enabled };
          }
        }
      }
      if (action === 'session.create' || action === 'session.send' || action === 'session.fork') {
        return executeSessionAction(action, input, contextDirectory, options.signal);
      }
      if (action.startsWith('session.')) {
        const directory = asNonEmptyString(input.directory) || asNonEmptyString(contextDirectory);
        const sessionID = asNonEmptyString(input.sessionId);
        const client = await getClient();
        if (action === 'session.list') {
          const limit = positiveInteger(input.limit, 10, 'limit');
          const response = await client.session.list(directory ? { directory } : {});
          let sessions = Array.isArray(response?.data) ? response.data : [];
          if (input.all !== true) sessions = sessions.filter((session) => !session?.time?.archived);
          sessions = sessions.slice(0, limit);
          if (input.withStatus === true) {
            const cache = new Map();
            sessions = await Promise.all(sessions.map(async (session) => {
              const sessionDirectory = asNonEmptyString(session?.directory);
              if (!sessionDirectory) return { ...session, status: { type: 'unknown' } };
              if (!cache.has(sessionDirectory)) {
                const statusRequest = client.session.status({ directory: sessionDirectory }).catch(() => null);
                cache.set(sessionDirectory, statusRequest);
              }
              const statusResponse = await cache.get(sessionDirectory);
              return { ...session, status: statusResponse?.data?.[session.id] || (statusResponse ? { type: 'idle' } : { type: 'unknown' }) };
            }));
          }
          return { sessions, limit, directory, archived: input.all === true ? 'included' : 'excluded' };
        }
        if (!sessionID) throw new OMPChamberControlError('sessionId is required', 400);
        if (!directory) throw new OMPChamberControlError('directory is required', 400);
        if (action === 'session.status') {
          return { sessionId: sessionID, directory, sessionStatus: await sessionStatus(client, sessionID, directory) };
        }
        if (action === 'session.messages') {
          if (input.timeout !== undefined && input.wait !== true) throw new OMPChamberControlError('timeout requires wait', 400);
          const role = input.lastAssistant === true ? 'assistant' : (asNonEmptyString(input.role) || 'all');
          if (!['all', 'user', 'assistant'].includes(role)) throw new OMPChamberControlError('role must be all, user, or assistant', 400);
          const last = input.last === true || input.lastAssistant === true;
          if (input.all === true && (last || input.limit !== undefined)) throw new OMPChamberControlError('all cannot be combined with last or limit', 400);
          if (last && input.limit !== undefined) throw new OMPChamberControlError('last cannot be combined with limit', 400);
          const currentStatus = input.wait === true
            ? await waitForIdle({ client, sessionID, directory, timeoutMs: normalizeWaitTimeoutMs(input.timeout), requireActivity: false, startedAt: now(), signal: options.signal })
            : await sessionStatus(client, sessionID, directory);
          const limit = input.all === true ? undefined : (last ? 1 : positiveInteger(input.limit, 10, 'limit'));
          return { sessionId: sessionID, directory, role, sessionStatus: currentStatus, messages: await sessionMessages(client, sessionID, directory, role, limit) };
        }
      }
      throw new OMPChamberControlError(`Unsupported OMPChamber action: ${action || 'missing'}`, 400);
    } catch (error) {
      throw asControlError(error, `Failed to execute ${action || 'OMPChamber action'}`);
    }
  };

  return { execute };
};
