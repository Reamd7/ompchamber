/**
 * 【模块】定时任务（scheduled tasks / loops）运行时：按项目加载并 reconcile
 * 任务定义（`.agents/loops` 文件为权威来源），为每个 occurrence 布防定时器，
 * 经共享项目配置的 lastScheduledFor 声明防止多实例（CLI serve + desktop）重复执行（#2710），
 * 再通过本地 OpenCode 引擎创建会话并以 prompt_async / 斜杠命令驱动执行，
 * 全程受全局/项目两级并发额度与 watchdog 超时约束。
 */
import { createLocalEngineClient } from '../opencode/local-engine-client.js';
import { DateTime } from 'luxon';
import parser from 'cron-parser';
import { expandSnippets } from '../opencode/snippets.js';
import { buildGoalIntroText, createSessionGoal } from '../session-goal/create.js';
import { discoverLoops } from './loops.js';

// 全局并发上限默认值：所有项目合计同时运行的任务数。
const DEFAULT_GLOBAL_CONCURRENCY = 4;
// 单项目并发上限默认值：同一项目同时运行的任务数。
const DEFAULT_PROJECT_CONCURRENCY = 2;
// 单次任务运行时长上限（默认 30 分钟），超时由 watchdog 竞速中止。
const DEFAULT_MAX_RUN_MS = 30 * 60 * 1000;
// 到点触发抖动上限（毫秒）：在精确到点之上叠加随机抖动，避免大量任务同毫秒齐发。
const JITTER_MAX_MS = 2_000;
// 生成的运行会话标题最大长度（任务名 + 时间戳后缀合计）。
const TASK_TITLE_MAX_LENGTH = 120;
// 到期宽限量（毫秒）：lastScheduledFor 落在该窗口内视为同一 occurrence，用于多实例去重。
const TASK_DUE_SLACK_MS = 5_000;
// setTimeout 的最大合法延迟（2^31-1 ms）；更长延迟需分段重排定时器。
const MAX_TIMER_DELAY_MS = 2_147_483_647;

/** 构造任务唯一键 `${projectID}:${taskID}`，用于定时器索引与运行中去重。 */
const buildTaskKey = (projectID, taskID) => `${projectID}:${taskID}`;

/** 将 "HH:mm" 字符串解析为 { hour, minute }；非字符串或不匹配 00:00–23:59 返回 null。 */
const parseTimeParts = (time) => {
  const match = /^([01]\d|2[0-3]):([0-5]\d)$/.exec(typeof time === 'string' ? time : '');
  if (!match) {
    return null;
  }
  return {
    hour: Number(match[1]),
    minute: Number(match[2]),
  };
};

/** 把 HH:mm 套用到给定 luxon DateTime（秒/毫秒清零）返回同日该时刻；time 非法返回 null。 */
const applyTimeToDate = (baseDateTime, time) => {
  const parsed = parseTimeParts(time);
  if (!parsed) {
    return null;
  }
  return baseDateTime.set({
    hour: parsed.hour,
    minute: parsed.minute,
    second: 0,
    millisecond: 0,
  });
};

/** 归一化调度时间列表：优先取 schedule.times 中的合法项，回退到单个 schedule.time；去重并按字典序（等价时间序）排序返回。 */
const resolveScheduleTimes = (schedule) => {
  const times = [];
  if (Array.isArray(schedule?.times)) {
    for (const candidate of schedule.times) {
      if (typeof candidate === 'string' && /^([01]\d|2[0-3]):([0-5]\d)$/.test(candidate)) {
        times.push(candidate);
      }
    }
  }
  if (times.length === 0 && typeof schedule?.time === 'string' && /^([01]\d|2[0-3]):([0-5]\d)$/.test(schedule.time)) {
    times.push(schedule.time);
  }
  return Array.from(new Set(times)).sort((a, b) => a.localeCompare(b));
};

/** 将 luxon 的 1–7（周一–周日）weekday 转为 0–6（周日=0）以匹配 schedule.weekdays 约定；无效输入返回 null。 */
const weekdayAsZeroBased = (dateTime) => {
  if (!dateTime || typeof dateTime.weekday !== 'number') {
    return null;
  }
  return dateTime.weekday % 7;
};

/** 从任意抛出值提取安全错误消息：Error 取 message，其余 String 化；trim、空值兜底 "Unknown error"、超长截断到 maxLength。 */
const safeErrorMessage = (error, maxLength = 2_000) => {
  const raw = error instanceof Error
    ? (error.message || String(error))
    : String(error ?? 'Unknown error');
  const trimmed = raw.trim();
  if (!trimmed) {
    return 'Unknown error';
  }
  return trimmed.length > maxLength ? trimmed.slice(0, maxLength) : trimmed;
};

/** 解析 "/命令 参数…" 形态的任务提示词：非 "/" 开头或首行无命令名返回 null，否则返回 { command, arguments }。 */
export const parseScheduledCommandPrompt = (prompt) => {
  if (typeof prompt !== 'string') {
    return null;
  }

  const trimmed = prompt.trim();
  if (!trimmed.startsWith('/')) {
    return null;
  }

  const firstLine = trimmed.split(/\r?\n/, 1)[0] || '';
  const [head, ...tail] = firstLine.split(/\s+/);
  const commandName = (head || '').slice(1).trim();
  if (!commandName) {
    return null;
  }

  return {
    command: commandName,
    arguments: tail.join(' ').trim(),
  };
};

/**
 * 展开命令模板的参数占位符生成 goal objective：`$ARGUMENTS` 全量替换；
 * `$1`/`$2`… 按引号/空白切分的位置参数（最后一个位置吞并余下全部参数）；
 * 模板无占位符且有参数时把参数追加为第二段；模板为空返回 null。
 */
export const expandCommandGoalObjective = (template, argumentsText) => {
  if (typeof template !== 'string' || !template.trim()) {
    return null;
  }

  const rawArguments = String(argumentsText ?? '');
  if (template.includes('$ARGUMENTS')) {
    return template.replaceAll('$ARGUMENTS', rawArguments);
  }

  const positions = [...template.matchAll(/\$(\d+)/g)].map((match) => Number(match[1]));
  if (positions.length > 0) {
    const parsedArguments = [...rawArguments.matchAll(/"([^"]*)"|'([^']*)'|(\S+)/g)]
      .map((match) => match[1] ?? match[2] ?? match[3] ?? '');
    const lastPosition = Math.max(...positions);
    return template.replace(/\$(\d+)/g, (_match, value) => {
      const position = Number(value);
      return position === lastPosition
        ? parsedArguments.slice(position - 1).join(' ')
        : (parsedArguments[position - 1] ?? '');
    });
  }

  return rawArguments ? `${template}\n\n${rawArguments}` : template;
};

/**
 * 计算任务下一次应运行时刻（毫秒时间戳）。禁用、无 schedule、时区无效或
 * 调度数据不完整一律返回 null。支持 daily / weekly / once / cron 四种 kind，
 * 均按 schedule.timezone（缺省本地时区）计算；候选时刻必须晚于
 * now + TASK_DUE_SLACK_MS 宽限量，weekly 最多向前看 14 天。
 */
export const computeNextRunAt = (task, nowMs = Date.now()) => {
  if (!task?.enabled) {
    return null;
  }

  const schedule = task.schedule;
  if (!schedule || typeof schedule !== 'object') {
    return null;
  }

  const zone = typeof schedule.timezone === 'string' && schedule.timezone.trim().length > 0
    ? schedule.timezone.trim()
    : DateTime.local().zoneName;

  const now = DateTime.fromMillis(nowMs, { zone });
  if (!now.isValid) {
    return null;
  }

  if (schedule.kind === 'daily') {
    const times = resolveScheduleTimes(schedule);
    if (times.length === 0) {
      return null;
    }
    const minAllowed = now.plus({ milliseconds: TASK_DUE_SLACK_MS });

    for (const time of times) {
      const candidateToday = applyTimeToDate(now, time);
      if (!candidateToday || !candidateToday.isValid) {
        continue;
      }
      if (candidateToday > minAllowed) {
        return candidateToday.toMillis();
      }
    }

    const tomorrow = now.plus({ days: 1 });
    const firstTomorrow = applyTimeToDate(tomorrow, times[0]);
    return firstTomorrow?.isValid ? firstTomorrow.toMillis() : null;
  }

  if (schedule.kind === 'weekly') {
    if (!Array.isArray(schedule.weekdays) || schedule.weekdays.length === 0) {
      return null;
    }
    const times = resolveScheduleTimes(schedule);
    if (times.length === 0) {
      return null;
    }
    const weekdaysSet = new Set(schedule.weekdays);
    const minAllowed = now.plus({ milliseconds: TASK_DUE_SLACK_MS });

    for (let dayOffset = 0; dayOffset <= 14; dayOffset += 1) {
      const dayCandidate = now.plus({ days: dayOffset });
      const zeroBasedWeekday = weekdayAsZeroBased(dayCandidate);
      if (zeroBasedWeekday === null || !weekdaysSet.has(zeroBasedWeekday)) {
        continue;
      }
      for (const time of times) {
        const withTime = applyTimeToDate(dayCandidate, time);
        if (!withTime || !withTime.isValid) {
          continue;
        }
        if (withTime > minAllowed) {
          return withTime.toMillis();
        }
      }
    }
    return null;
  }

  if (schedule.kind === 'once') {
    if (typeof schedule.date !== 'string' || typeof schedule.time !== 'string') {
      return null;
    }

    const parsed = DateTime.fromFormat(
      `${schedule.date} ${schedule.time}`,
      'yyyy-LL-dd HH:mm',
      { zone },
    );
    if (!parsed.isValid) {
      return null;
    }

    const minAllowed = now.plus({ milliseconds: TASK_DUE_SLACK_MS });
    if (parsed <= minAllowed) {
      return null;
    }

    return parsed.toMillis();
  }

  if (schedule.kind === 'cron') {
    try {
      const iterator = parser.parseExpression(schedule.cron, {
        tz: zone,
        currentDate: new Date(nowMs),
      });
      return iterator.next().getTime();
    } catch {
      return null;
    }
  }

  return null;
};

/** 生成运行会话标题 `任务名 yyyy-LL-dd HH:mm`（按任务时区）；任务名缺失用 "Scheduled task"，整体截到 TASK_TITLE_MAX_LENGTH。 */
export const formatScheduledSessionTitle = (task, nowMs = Date.now()) => {
  const timezone = typeof task?.schedule?.timezone === 'string' && task.schedule.timezone.trim().length > 0
    ? task.schedule.timezone.trim()
    : DateTime.local().zoneName;
  const stamp = DateTime.fromMillis(nowMs, { zone: timezone }).toFormat('yyyy-LL-dd HH:mm');
  const taskName = typeof task?.name === 'string' && task.name.trim().length > 0
    ? task.name.trim()
    : 'Scheduled task';
  const suffix = ` ${stamp}`;
  const maxTaskNameLength = Math.max(1, TASK_TITLE_MAX_LENGTH - suffix.length);
  const trimmedName = taskName.length > maxTaskNameLength
    ? taskName.slice(0, maxTaskNameLength)
    : taskName;
  return `${trimmedName}${suffix}`;
};

/**
 * 创建定时任务运行时。内部维护每项目任务表、每任务定时器、运行队列与
 * 全局/项目两级并发额度；返回 { start, stop, syncAllProjects, syncProject,
 * runNow, getStatus }。deps 注入项目配置运行时、OpenCode 引擎访问器、
 * 事件回调与并发/超时参数。
 */
export const createScheduledTasksRuntime = (deps) => {
  const {
    projectConfigRuntime,
    listProjects,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    waitForOpenCodeReady,
    emitTaskRunEvent,
    setSessionAutoAccept,
    sessionKnowledgeRuntime = null,
    logger = console,
    maxGlobalConcurrency = DEFAULT_GLOBAL_CONCURRENCY,
    maxProjectConcurrency = DEFAULT_PROJECT_CONCURRENCY,
    maxRunDurationMs = DEFAULT_MAX_RUN_MS,
  } = deps;

  // 运行时是否已 start；stop 后置回 false，未启动时 scheduleTask 直接跳过布防。
  let started = false;
  // projectID -> Map(taskID -> task) 的内存任务表（最新已知任务定义与状态）。
  const tasksByProject = new Map();
  // projectID -> 项目路径缓存（listProjects 结果）。
  const projectPathByID = new Map();
  // taskKey -> setTimeout 句柄：每个任务至多一个待触发定时器。
  const timersByTaskKey = new Map();
  // 已入队尚未开跑的 taskKey 集合（防重复入队）。
  const queuedTaskKeys = new Set();
  // 正在运行中的 taskKey 集合（占用并发额度）。
  const runningTaskKeys = new Set();
  // projectID -> 该项目运行中任务数（项目级并发额度计数）。
  const runningCountByProject = new Map();
  // 全局运行中任务数（全局并发额度计数）。
  let runningGlobalCount = 0;
  // 待运行队列（FIFO），元素为 { projectID, taskID, reason, scheduledFor? }。
  const queue = [];

  /** 取消并移除某 taskKey 的待触发定时器（无定时器则空操作）。 */
  const clearTimerForKey = (taskKey) => {
    const timer = timersByTaskKey.get(taskKey);
    if (timer) {
      clearTimeout(timer);
      timersByTaskKey.delete(taskKey);
    }
  };

  /** 清掉某项目全部任务的定时器并把他们移出待入队集合（项目任务表本身的清理由调用方决定）。 */
  const clearProjectTimers = (projectID) => {
    const tasks = tasksByProject.get(projectID);
    if (!tasks) {
      return;
    }
    for (const task of tasks.values()) {
      clearTimerForKey(buildTaskKey(projectID, task.id));
      queuedTaskKeys.delete(buildTaskKey(projectID, task.id));
    }
  };

  /** 用新任务列表整体替换某项目的内存任务表（先清旧定时器，任务以 id 为键去重）。 */
  const setProjectTasks = (projectID, tasks) => {
    clearProjectTimers(projectID);
    const taskMap = new Map();
    for (const task of tasks) {
      taskMap.set(task.id, task);
    }
    tasksByProject.set(projectID, taskMap);
  };

  /**
   * 为任务布防到 nextRunAt 的定时器：先清旧定时器；未启动、目标时刻非法则不布防。
   * 延迟 = 到点差值 + 随机抖动（0..JITTER_MAX_MS），超过 MAX_TIMER_DELAY_MS 时
   * 先挂一个分段定时器、到期后递归重排。到点回调里重新校验任务存在且 enabled，
   * 再入队 'scheduled' 运行并泵动队列。
   */
  const scheduleTask = (projectID, taskID, nextRunAt) => {
    const taskKey = buildTaskKey(projectID, taskID);
    clearTimerForKey(taskKey);

    if (!started) {
      return;
    }

    if (!Number.isFinite(nextRunAt) || nextRunAt <= 0) {
      return;
    }

    const delayBase = Math.max(0, Math.round(nextRunAt - Date.now()));
    const jitter = Math.floor(Math.random() * (JITTER_MAX_MS + 1));
    const delay = delayBase + jitter;
    const boundedDelay = Math.min(delay, MAX_TIMER_DELAY_MS);

    const timer = setTimeout(async () => {
      if (delay > MAX_TIMER_DELAY_MS) {
        scheduleTask(projectID, taskID, nextRunAt);
        return;
      }

      clearTimerForKey(taskKey);
      const taskMap = tasksByProject.get(projectID);
      const task = taskMap?.get(taskID);
      if (!task || !task.enabled) {
        return;
      }
      queueTaskRun(projectID, taskID, 'scheduled', nextRunAt);
      pumpQueue();
    }, boundedDelay);

    timersByTaskKey.set(taskKey, timer);
  };

  /** 用最新任务对象覆盖内存表中的同 id 项（项目或任务不存在则忽略）。 */
  const updateInMemoryTask = (projectID, nextTask) => {
    if (!nextTask) {
      return;
    }
    const taskMap = tasksByProject.get(projectID);
    if (!taskMap) {
      return;
    }
    taskMap.set(nextTask.id, nextTask);
  };

  /** 重算任务 nextRunAt 并写回项目配置（statePatch 含 updatedAt）；写回成功后更新内存并在任务启用时重新布防。 */
  const syncTaskSchedule = async (projectID, task) => {
    if (!task) {
      return;
    }
    const nextRunAt = computeNextRunAt(task, Date.now());
    const statePatch = {
      nextRunAt: Number.isFinite(nextRunAt) ? nextRunAt : undefined,
      updatedAt: Date.now(),
    };
    const result = await projectConfigRuntime.updateScheduledTaskState(projectID, task.id, statePatch);
    if (result.task) {
      updateInMemoryTask(projectID, result.task);
      if (result.task.enabled && Number.isFinite(result.task.state?.nextRunAt)) {
        scheduleTask(projectID, result.task.id, result.task.state.nextRunAt);
      }
    }
  };

  /** 解析 projectID 对应的项目路径：命中缓存直接返回，否则查 listProjects 并缓存；找不到或出错返回 null。 */
  const ensureProjectPath = async (projectID) => {
    if (projectPathByID.has(projectID)) {
      return projectPathByID.get(projectID) || null;
    }

    try {
      const projects = await listProjects();
      const project = projects.find((item) => item?.id === projectID && item?.path);
      if (project?.path) {
        projectPathByID.set(projectID, project.path);
        return project.path;
      }
    } catch {
    }

    return null;
  };

  /**
   * 同步单个项目：有路径则先以 `.agents/loops` 定义 reconcile 持久化任务列表
   *（loop 文件在场时为权威、删除即取消调度、运行状态保留），否则直接列出持久化任务；
   * 然后整体替换内存任务表并逐个重算/布防 nextRunAt。返回该项目的任务列表。
   */
  const syncProject = async (projectID) => {
    await ensureProjectPath(projectID);
    const projectPath = projectPathByID.get(projectID) || null;

    let tasks;
    if (projectPath) {
      // Reconcile `.agents/loops` definitions with the persisted task list:
      // loop files are authoritative while present, removed files unschedule
      // their task, and runtime state is preserved (see loops.js).
      const loops = await discoverLoops(projectPath);
      tasks = await projectConfigRuntime.reconcileLoopTasks(projectID, loops);
    } else {
      tasks = await projectConfigRuntime.listScheduledTasks(projectID);
    }

    setProjectTasks(projectID, tasks);

    for (const task of tasks) {
      await syncTaskSchedule(projectID, task);
    }

    return tasks;
  };

  /** 全量同步：刷新项目列表与路径缓存，清掉已消失项目的定时器与任务表，再逐项目 syncProject。 */
  const syncAllProjects = async () => {
    const projects = await listProjects();
    const activeProjectIDs = new Set();
    projectPathByID.clear();
    for (const project of projects) {
      if (!project?.id || !project?.path) {
        continue;
      }
      activeProjectIDs.add(project.id);
      projectPathByID.set(project.id, project.path);
    }

    for (const existingProjectID of Array.from(tasksByProject.keys())) {
      if (!activeProjectIDs.has(existingProjectID)) {
        clearProjectTimers(existingProjectID);
        tasksByProject.delete(existingProjectID);
      }
    }

    for (const projectID of activeProjectIDs) {
      await syncProject(projectID);
    }
  };

  /** 把一次运行加入待运行队列（reason 为 'scheduled'/'manual'，scheduledFor 为计划时刻）；已在队列或运行中则忽略。 */
  const queueTaskRun = (projectID, taskID, reason, scheduledFor) => {
    const taskKey = buildTaskKey(projectID, taskID);
    if (queuedTaskKeys.has(taskKey) || runningTaskKeys.has(taskKey)) {
      return;
    }
    queuedTaskKeys.add(taskKey);
    queue.push({
      projectID,
      taskID,
      reason,
      ...(Number.isFinite(scheduledFor) ? { scheduledFor } : {}),
    });
  };

  /** 并发闸门：全局运行数未达上限且该项目运行数未达项目上限时才允许开跑。 */
  const canRunTask = (projectID) => {
    if (runningGlobalCount >= maxGlobalConcurrency) {
      return false;
    }
    const projectRunning = runningCountByProject.get(projectID) || 0;
    return projectRunning < maxProjectConcurrency;
  };

  /**
   * 构造 prompt_async 请求体：按需附带 model（角色跟随任务省略以用引擎默认）、
   * agent、variant；parts 依次为常驻项目知识（synthetic）、展开 snippet 后的任务提示词、
   * goal 模式时的 goal 引导文本（synthetic）。
   */
  const buildPromptAsyncPayload = (task, projectPath, knowledgeText = '') => ({
    ...(task.execution.providerID && task.execution.modelID
      ? { model: { providerID: task.execution.providerID, modelID: task.execution.modelID } }
      // Role-follow tasks omit the model so the engine resolves its default role.
      : {}),
    ...(task.execution.agent ? { agent: task.execution.agent } : {}),
    ...(task.execution.variant ? { variant: task.execution.variant } : {}),
    parts: [
      // Standing project context first, so the prompt reads against it. A
      // scheduled run has no UI to attach this, which is why it is asked for
      // here rather than assembled by whoever is sending.
      ...(knowledgeText ? [{ type: 'text', text: knowledgeText, synthetic: true }] : []),
      {
        type: 'text',
        text: expandSnippets(task.execution.prompt, projectPath),
      },
      ...(task.execution.goalEnabled
        ? [{ type: 'text', text: buildGoalIntroText(task.execution.goalTokenBudget), synthetic: true }]
        : []),
    ],
  });

  /**
   * 向会话发送 prompt_async：先解析待投递的项目知识（失败静默降级为空，绝不因此挂掉运行），
   * POST 成功后才把该知识记为已投递——失败的派发下次运行会再次携带上下文。
   * 非 2xx 响应抛出带状态码与响应体的错误。
   */
  const runPromptAsync = async ({ baseUrl, authHeaders, sessionID, projectPath, task }) => {
    // Never allowed to fail the run: a task that executes without its
    // background is a lesser loss than a task that does not execute.
    const knowledge = sessionKnowledgeRuntime
      ? await sessionKnowledgeRuntime.resolvePendingForSession(sessionID, projectPath)
        .catch(() => ({ text: '', signature: '' }))
      : { text: '', signature: '' };

    const promptUrl = new URL(`${baseUrl}/session/${encodeURIComponent(sessionID)}/prompt_async`);
    promptUrl.searchParams.set('directory', projectPath);
    const response = await fetch(promptUrl.toString(), {
      method: 'POST',
      headers: {
        ...authHeaders,
        'content-type': 'application/json',
        accept: 'application/json',
      },
      body: JSON.stringify(buildPromptAsyncPayload(task, projectPath, knowledge.text)),
    });

    if (!response.ok) {
      const body = await response.text().catch(() => '');
      throw new Error(`prompt_async failed (${response.status})${body ? `: ${body}` : ''}`);
    }

    // Recorded only after the prompt is accepted, so a failed dispatch carries
    // the context again on the next run.
    if (knowledge.text && sessionKnowledgeRuntime) {
      await sessionKnowledgeRuntime.recordDelivered(sessionID, projectPath, knowledge.signature)
        .catch(() => undefined);
    }
  };

  /** 若任务提示词是 "/命令" 形态且引擎已注册同名命令，返回 { command, arguments, template }，否则 null（回落到 prompt_async）。 */
  const resolveScheduledCommand = async ({ client, projectPath, task }) => {
    const parsed = parseScheduledCommandPrompt(task?.execution?.prompt);
    if (!parsed) {
      return null;
    }

    let commands = [];
    try {
      const response = await client.command.list({ directory: projectPath });
      commands = Array.isArray(response?.data) ? response.data : [];
    } catch {
      return null;
    }

    const command = commands.find((candidate) => candidate?.name === parsed.command);
    return command ? { ...parsed, template: command.template } : null;
  };

  /** 在会话里执行已解析的斜杠命令，透传任务的 agent / model / variant 覆盖。 */
  const runScheduledCommand = async ({ client, projectPath, sessionID, task, command }) => {
    await client.session.command({
      sessionID,
      directory: projectPath,
      command: command.command,
      arguments: command.arguments,
      ...(task.execution.agent ? { agent: task.execution.agent } : {}),
      ...(task.execution.providerID && task.execution.modelID
        ? { model: `${task.execution.providerID}/${task.execution.modelID}` }
        : {}),
      ...(task.execution.variant ? { variant: task.execution.variant } : {}),
    });

  };

  /**
   * 执行一次任务并产出运行结果：等待引擎就绪、经本地引擎 client 创建带标题的会话、
   * 发出 running 事件、按需为会话开启权限自动放行（失败仅告警）、goal 模式先创建
   * session goal（objective 用命令模板展开或 snippet 展开），最后走斜杠命令或
   * prompt_async 派发。返回 { sessionID, durationMs, reason, startedAt, finishedAt }；
   * 超时由调用方 runTask 的 watchdog 竞速处理。
   */
  const runTaskWithWatchdog = async (projectID, task, reason) => {
    const startedAt = Date.now();
    const title = formatScheduledSessionTitle(task, startedAt);
    const projectPath = projectPathByID.get(projectID);
    if (!projectPath) {
      throw new Error('project path is unavailable');
    }

    if (typeof waitForOpenCodeReady === 'function') {
      await waitForOpenCodeReady(10_000, 250);
    }

    const baseUrl = buildOpenCodeUrl('/', '').replace(/\/$/, '');
    const authHeaders = getOpenCodeAuthHeaders();
    const client = createLocalEngineClient({
      baseUrl,
      headers: authHeaders,
    });

    const sessionResponse = await client.session.create({
      directory: projectPath,
      title,
    });
    const sessionID = sessionResponse?.data?.id;
    if (!sessionID) {
      throw new Error('failed to create session');
    }

    try {
      emitTaskRunEvent?.({
        projectID,
        taskID: task.id,
        ranAt: startedAt,
        status: 'running',
        sessionID,
      });
    } catch {
    }

    if (task.execution.permissionAutoAccept && typeof setSessionAutoAccept === 'function') {
      // Enroll before the prompt goes out so the very first permission request
      // is already auto-approved. Enrollment failure must not kill the run —
      // the task still executes, permissions just wait for the user.
      try {
        await setSessionAutoAccept(sessionID, true, projectPath);
      } catch (error) {
        logger.warn?.('[scheduled-tasks] failed to enable permission auto-accept for session', sessionID, error?.message ?? error);
      }
    }

    const scheduledCommand = await resolveScheduledCommand({ client, projectPath, task });

    if (task.execution.goalEnabled) {
      const commandObjective = scheduledCommand
        ? expandCommandGoalObjective(scheduledCommand.template, scheduledCommand.arguments)
        : null;
      await createSessionGoal({
        baseUrl,
        authHeaders,
        sessionID,
        directory: projectPath,
        objective: commandObjective ?? expandSnippets(task.execution.prompt, projectPath),
        tokenBudget: task.execution.goalTokenBudget,
        providerID: task.execution.providerID,
        modelID: task.execution.modelID,
        onWarning: (message, error) => console.warn(`[scheduled-tasks] ${message}:`, error?.message || error),
      });
    }

    if (scheduledCommand) {
      await runScheduledCommand({ client, projectPath, sessionID, task, command: scheduledCommand });
    } else {
      await runPromptAsync({
        baseUrl,
        authHeaders,
        sessionID,
        projectPath,
        task,
      });
    }

    const finishedAt = Date.now();
    return {
      sessionID,
      durationMs: Math.max(0, finishedAt - startedAt),
      reason,
      startedAt,
      finishedAt,
    };
  };

  /** 释放一次运行占用的并发额度：移出运行集合、全局计数减一、项目计数减一（归零则删除键）。 */
  const releaseRunningSlot = (projectID, taskKey) => {
    runningTaskKeys.delete(taskKey);
    runningGlobalCount = Math.max(0, runningGlobalCount - 1);
    const nextProjectCount = Math.max(0, (runningCountByProject.get(projectID) || 1) - 1);
    if (nextProjectCount === 0) {
      runningCountByProject.delete(projectID);
    } else {
      runningCountByProject.set(projectID, nextProjectCount);
    }
  };

  /** 只为"未来"的 occurrence 布防定时器；nextRunAt 非法或已过期（<= fromMs）返回 false 不布防。 */
  /**
   * Arm a timer only for a future occurrence. Scheduling a past nextRunAt
   * (delay 0 + jitter) re-enters the claim path immediately and can spin —
   * especially for once tasks where the claim cannot advance nextRunAt.
   */
  const scheduleFutureRun = (projectID, taskID, nextRunAt, fromMs = Date.now()) => {
    if (!Number.isFinite(nextRunAt)) {
      return false;
    }
    const base = Number.isFinite(fromMs) ? fromMs : Date.now();
    if (nextRunAt <= base) {
      return false;
    }
    scheduleTask(projectID, taskID, nextRunAt);
    return true;
  };

  /**
   * 重新布防任务：优先用内存/兜底任务里仍指向未来的持久化 nextRunAt；
   * 否则基于 fromMs 重算下一次时刻再布防。绝不重挂过去的 occurrence
   * （否则会制造 once 任务的静默败者循环与 claim 失败重试风暴）。
   */
  const rearmFromTaskOrCompute = (projectID, taskID, fallbackTask, fromMs) => {
    const latest = (tasksByProject.get(projectID)?.get(taskID)) || fallbackTask;
    if (!latest?.enabled) {
      return;
    }
    const base = Number.isFinite(fromMs) ? fromMs : Date.now();
    const persistedNext = latest.state?.nextRunAt;
    // Prefer a still-future persisted slot; never re-arm a past occurrence
    // (that created silent once-task loser loops and claim-failed retry spam).
    if (scheduleFutureRun(projectID, taskID, persistedNext, base)) {
      return;
    }
    const computedNext = computeNextRunAt(latest, base);
    scheduleFutureRun(projectID, taskID, computedNext, base);
  };

  /**
   * 执行一次任务（manual/scheduled 统一入口）：占用运行槽位后——
   * 定时派发先在共享项目配置里声明 occurrence（updateScheduledTaskStateIf +
   * lastScheduledFor 宽限量去重，#2710：双实例各自布防定时器，靠该声明只有一个真正运行），
   * 声明失败/落败时记录状态并按未来时刻重挂；手动派发仅写 running 状态——
   * 然后以 watchdog 超时竞速执行 runTaskWithWatchdog，成功后消费 once 任务、
   * 写终态（含 nextRunAt）并按未来时刻重挂；完成态写入失败走内存恢复 + 单次重试。
   * 返回 { ok, status, sessionID, task, error, ... }；finally 里必定释放运行槽位，
   * 保证锁超时/写入失败不会把任务卡死在 "running"。
   */
  const runTask = async (projectID, taskID, reason, scheduledFor) => {
    const taskMap = tasksByProject.get(projectID);
    const task = taskMap?.get(taskID);
    if (!task || !task.enabled) {
      return { ok: false, skipped: true };
    }

    const taskKey = buildTaskKey(projectID, taskID);
    if (runningTaskKeys.has(taskKey)) {
      return { ok: false, running: true };
    }

    runningTaskKeys.add(taskKey);
    runningGlobalCount += 1;
    runningCountByProject.set(projectID, (runningCountByProject.get(projectID) || 0) + 1);

    // Every path that holds the running slot must exit through this finally so
    // lock timeouts / fs errors on claim, manual-start, or completion writes
    // cannot permanently stuck-run the task in this process.
    try {
      const runStartedAt = Date.now();

      // Scheduled dispatches must claim the occurrence in shared project config
      // before creating a session. Two server instances (e.g. CLI serve + desktop)
      // each arm their own timer; without this claim both would run (#2710).
      if (reason === 'scheduled') {
        if (!Number.isFinite(scheduledFor)) {
          return { ok: false, skipped: true, reason: 'missing-scheduled-for' };
        }

        const nextAfterClaim = computeNextRunAt(task, Math.max(runStartedAt, scheduledFor + 1));
        const claimPatch = {
          lastScheduledFor: Math.round(scheduledFor),
          lastRunAt: runStartedAt,
          lastStatus: 'running',
          lastError: undefined,
          updatedAt: runStartedAt,
          // Always set nextRunAt so a past once-slot is cleared when there is
          // no following occurrence (omitting the key would leave the past value).
          nextRunAt: Number.isFinite(nextAfterClaim) ? nextAfterClaim : undefined,
        };

        // Duplicate protection is solely lastScheduledFor within slack of this
        // occurrence. Do not reject on advanced disk nextRunAt: lastScheduledFor
        // persists across days, so a second-instance sync inside TASK_DUE_SLACK_MS
        // would otherwise suppress every armed occurrence after the first.
        // occurrence 声明谓词：任务须启用，且 lastScheduledFor 不在本 occurrence
        // 的宽限窗口内（在窗口内说明已被本实例或另一实例声明过）。
        const canClaimOccurrence = (candidate) => {
          if (!candidate?.enabled) {
            return false;
          }
          const lastScheduledFor = candidate.state?.lastScheduledFor;
          if (
            Number.isFinite(lastScheduledFor)
            && Math.abs(lastScheduledFor - scheduledFor) <= TASK_DUE_SLACK_MS
          ) {
            return false;
          }
          return true;
        };

        let claimResult;
        try {
          if (typeof projectConfigRuntime.updateScheduledTaskStateIf === 'function') {
            claimResult = await projectConfigRuntime.updateScheduledTaskStateIf(
              projectID,
              taskID,
              canClaimOccurrence,
              claimPatch,
            );
          } else {
            // Fallback for older test doubles: unconditional update (single-instance only).
            claimResult = await projectConfigRuntime.updateScheduledTaskState(projectID, taskID, claimPatch);
            claimResult = { ...claimResult, updated: Boolean(claimResult?.task) };
          }
        } catch (claimError) {
          const message = safeErrorMessage(claimError);
          logger.warn?.('[ScheduledTasks] occurrence claim failed', {
            projectID,
            taskID,
            error: message,
          });
          rearmFromTaskOrCompute(projectID, taskID, task, Math.max(runStartedAt, scheduledFor + 1));

          // Best-effort record so once tasks are not left enabled-but-inert with
          // no UI signal. Do not clobber a winner that claimed this occurrence.
          const claimFailurePatch = {
            lastStatus: 'error',
            lastError: `Scheduled claim failed: ${message}`,
            updatedAt: Date.now(),
          };
          try {
            if (typeof projectConfigRuntime.updateScheduledTaskStateIf === 'function') {
              const recorded = await projectConfigRuntime.updateScheduledTaskStateIf(
                projectID,
                taskID,
                (candidate) => {
                  const lastScheduledFor = candidate.state?.lastScheduledFor;
                  if (
                    Number.isFinite(lastScheduledFor)
                    && Math.abs(lastScheduledFor - scheduledFor) <= TASK_DUE_SLACK_MS
                  ) {
                    return false;
                  }
                  return true;
                },
                claimFailurePatch,
              );
              if (recorded.task) {
                updateInMemoryTask(projectID, recorded.task);
              }
            } else {
              const recorded = await projectConfigRuntime.updateScheduledTaskState(
                projectID,
                taskID,
                claimFailurePatch,
              );
              if (recorded.task) {
                updateInMemoryTask(projectID, recorded.task);
              }
            }
          } catch {
            updateInMemoryTask(projectID, {
              ...task,
              state: {
                ...(task.state || {}),
                ...claimFailurePatch,
              },
            });
          }

          return { ok: false, skipped: true, reason: 'claim-failed', error: message };
        }

        if (!claimResult?.updated) {
          if (claimResult?.task) {
            updateInMemoryTask(projectID, claimResult.task);
            // Loser must not schedule a past nextRunAt (once-task spin).
            rearmFromTaskOrCompute(
              projectID,
              taskID,
              claimResult.task,
              Math.max(Date.now(), scheduledFor + 1),
            );
          }
          return { ok: false, skipped: true, reason: 'occurrence-claimed' };
        }

        if (claimResult.task) {
          updateInMemoryTask(projectID, claimResult.task);
        }
      } else {
        try {
          const startResult = await projectConfigRuntime.updateScheduledTaskState(projectID, taskID, {
            lastRunAt: runStartedAt,
            lastStatus: 'running',
            lastError: undefined,
            updatedAt: runStartedAt,
          });
          if (startResult.task) {
            updateInMemoryTask(projectID, startResult.task);
          }
        } catch (startError) {
          const message = safeErrorMessage(startError);
          logger.warn?.('[ScheduledTasks] manual start state write failed', {
            projectID,
            taskID,
            error: message,
          });
          return { ok: false, error: message, reason: 'start-state-failed' };
        }
      }

      let status = 'success';
      let sessionID;
      let durationMs = 0;
      let errorMessage;

      try {
        const runPromise = runTaskWithWatchdog(projectID, task, reason);
        let timeoutID;
        const timeoutPromise = new Promise((_, reject) => {
          timeoutID = setTimeout(() => {
            reject(new Error('scheduled task run timed out'));
          }, maxRunDurationMs);
        });

        const result = await Promise.race([runPromise, timeoutPromise]).finally(() => {
          if (timeoutID) {
            clearTimeout(timeoutID);
          }
        });
        sessionID = result.sessionID;
        durationMs = result.durationMs;
        status = 'success';
        logger.info?.(
          '[ScheduledTasks] run completed',
          { projectID, taskID, status, reason, sessionID, durationMs }
        );
      } catch (error) {
        status = 'error';
        errorMessage = safeErrorMessage(error);
        logger.warn?.('[ScheduledTasks] run failed', {
          projectID,
          taskID,
          reason,
          status,
          error: errorMessage,
        });
      }

      const finishedAt = Date.now();
      if (!durationMs) {
        durationMs = Math.max(0, finishedAt - runStartedAt);
      }
      let latestTask = (tasksByProject.get(projectID)?.get(taskID)) || task;
      const shouldConsumeOneTimeTask = latestTask?.schedule?.kind === 'once' && reason === 'scheduled';
      if (shouldConsumeOneTimeTask && latestTask?.enabled) {
        try {
          const consumed = await projectConfigRuntime.upsertScheduledTask(projectID, {
            ...latestTask,
            enabled: false,
          });
          latestTask = consumed.task || latestTask;
          updateInMemoryTask(projectID, latestTask);
        } catch (consumeError) {
          logger.warn?.('[ScheduledTasks] failed to consume one-time task', {
            projectID,
            taskID,
            error: safeErrorMessage(consumeError),
          });
        }
      }

      const nextRunAt = computeNextRunAt(latestTask, finishedAt);

      const statePatch = {
        lastStatus: status,
        lastDurationMs: durationMs,
        lastError: status === 'error' ? errorMessage : undefined,
        lastSessionId: status === 'success' ? sessionID : undefined,
        nextRunAt: Number.isFinite(nextRunAt) ? nextRunAt : undefined,
        updatedAt: finishedAt,
      };

      let stateResult = { task: null };
      try {
        stateResult = await projectConfigRuntime.updateScheduledTaskState(projectID, taskID, statePatch);
        if (stateResult.task) {
          updateInMemoryTask(projectID, stateResult.task);
          if (stateResult.task.enabled) {
            scheduleFutureRun(
              projectID,
              taskID,
              stateResult.task.state?.nextRunAt,
              finishedAt,
            );
          }
        }
      } catch (persistError) {
        const message = safeErrorMessage(persistError);
        logger.warn?.('[ScheduledTasks] run completion state write failed', {
          projectID,
          taskID,
          reason,
          error: message,
        });

        // Keep in-memory status terminal so this process does not advertise
        // a stuck "running" task after the session already finished.
        const recoveredTask = {
          ...latestTask,
          state: {
            ...(latestTask.state || {}),
            lastStatus: status,
            lastDurationMs: durationMs,
            lastError: status === 'error' ? errorMessage : undefined,
            lastSessionId: status === 'success' ? sessionID : undefined,
            nextRunAt: Number.isFinite(nextRunAt) ? nextRunAt : undefined,
            updatedAt: finishedAt,
          },
        };
        updateInMemoryTask(projectID, recoveredTask);

        // Best-effort single retry so persisted lastStatus does not stay 'running'.
        try {
          const retry = await projectConfigRuntime.updateScheduledTaskState(projectID, taskID, statePatch);
          if (retry.task) {
            updateInMemoryTask(projectID, retry.task);
            stateResult = retry;
            if (retry.task.enabled) {
              scheduleFutureRun(projectID, taskID, retry.task.state?.nextRunAt, finishedAt);
            }
          }
        } catch (retryError) {
          logger.warn?.('[ScheduledTasks] run completion state retry failed', {
            projectID,
            taskID,
            reason,
            error: safeErrorMessage(retryError),
          });
          stateResult = { task: recoveredTask };
          rearmFromTaskOrCompute(projectID, taskID, recoveredTask, finishedAt);
        }

        // The session already ran — surface persist failure without treating a
        // successful dispatch as a hard run failure (manual runNow would 500).
        return {
          ok: status === 'success',
          status,
          sessionID,
          task: stateResult.task || recoveredTask,
          error: status === 'error' ? errorMessage : undefined,
          persistError: message,
          reason: 'completion-state-failed',
        };
      }

      try {
        emitTaskRunEvent?.({
          projectID,
          taskID,
          ranAt: finishedAt,
          status,
          ...(sessionID ? { sessionID } : {}),
        });
      } catch {
      }

      return {
        ok: status === 'success',
        status,
        sessionID,
        task: stateResult.task || null,
        error: errorMessage,
      };
    } finally {
      releaseRunningSlot(projectID, taskKey);
    }
  };

  /** 泵动待运行队列：从头扫描，把并发额度允许的项出队开跑（fire-and-forget + catch 告警），每项完成后再次泵动。 */
  const pumpQueue = () => {
    if (!started) {
      return;
    }

    let consumed = false;
    for (let index = 0; index < queue.length; index += 1) {
      const item = queue[index];
      if (!canRunTask(item.projectID)) {
        continue;
      }

      queue.splice(index, 1);
      index -= 1;

      const taskKey = buildTaskKey(item.projectID, item.taskID);
      queuedTaskKeys.delete(taskKey);
      consumed = true;

      void runTask(item.projectID, item.taskID, item.reason, item.scheduledFor)
        .catch((error) => {
          logger.warn?.('[ScheduledTasks] queued run rejected', {
            projectID: item.projectID,
            taskID: item.taskID,
            reason: item.reason,
            error: safeErrorMessage(error),
          });
        })
        .finally(() => {
          pumpQueue();
        });
    }

    if (!consumed && queue.length > 0) {
      return;
    }
  };

  /** 手动立即运行：任务运行中/已入队直接返回相应错误，否则以 reason='manual' 走 runTask（不做 occurrence 声明）。 */
  const runNow = async (projectID, taskID) => {
    const taskKey = buildTaskKey(projectID, taskID);
    if (runningTaskKeys.has(taskKey)) {
      return {
        ok: false,
        running: true,
        error: 'task is already running',
      };
    }
    if (queuedTaskKeys.has(taskKey)) {
      return {
        ok: false,
        queued: true,
        error: 'task is already queued',
      };
    }

    return runTask(projectID, taskID, 'manual');
  };

  /** 启动运行时（幂等）：置 started 后全量同步所有项目并布防定时器。 */
  const start = async () => {
    if (started) {
      return;
    }
    started = true;
    await syncAllProjects();
  };

  /** 停止运行时：清空全部定时器与待运行队列，置回未启动状态。 */
  const stop = () => {
    if (!started) {
      return;
    }
    started = false;
    for (const timer of timersByTaskKey.values()) {
      clearTimeout(timer);
    }
    timersByTaskKey.clear();
    queuedTaskKeys.clear();
    queue.length = 0;
  };

  /** 汇总状态：是否存在启用的定时任务、是否有运行中的任务及其数量，供 UI/健康检查使用。 */
  const getStatus = () => {
    let enabledCount = 0;
    for (const taskMap of tasksByProject.values()) {
      for (const task of taskMap.values()) {
        if (task?.enabled) {
          enabledCount += 1;
        }
      }
    }

    const runningCount = runningTaskKeys.size;
    return {
      hasEnabledScheduledTasks: enabledCount > 0,
      hasRunningScheduledTasks: runningCount > 0,
      enabledScheduledTasksCount: enabledCount,
      runningScheduledTasksCount: runningCount,
    };
  };

  return {
    start,
    stop,
    syncAllProjects,
    syncProject,
    runNow,
    getStatus,
  };
};
