/**
 * 定时任务（scheduled tasks）服务层：在 loop markdown 文件（./loops.js）
 * 与项目配置存储（projectConfigRuntime）之上封装领域逻辑，供 routes.js
 * 的 HTTP 处理器调用。业务错误统一以 OMPChamberControlError（含
 * statusCode）抛出，由路由层映射为 HTTP 状态码。
 */
import fs from 'node:fs';
import path from 'node:path';
import { OMPChamberControlError } from '../openchamber-control/error.js';
import { setLoopFileEnabled } from './loops.js';

/** 归一化为非空字符串：非字符串、空串或纯空白返回 null，否则返回 trim 后的值。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/**
 * 创建定时任务服务实例（依赖注入风格，便于路由层与测试替换实现）。
 *
 * @param {object} dependencies 注入依赖
 * @param {Function} dependencies.readSettingsFromDiskMigrated 读取（并迁移）磁盘设置
 * @param {Function} dependencies.sanitizeProjects 清洗设置中的项目列表
 * @param {object} dependencies.projectConfigRuntime 项目配置运行时（定时任务的 JSON 存储）
 * @param {object} dependencies.scheduledTasksRuntime 定时任务运行时（调度、立即执行、状态汇总）
 * @returns {object} 定时任务服务：listProjects / resolveProjectID / list /
 *   upsert / remove / run / setEnabled / setLoopEnabled / removeLoopFile / status
 */
export const createScheduledTaskService = (dependencies) => {
  const {
    readSettingsFromDiskMigrated,
    sanitizeProjects,
    projectConfigRuntime,
    scheduledTasksRuntime,
  } = dependencies;

  /** 读取磁盘设置并返回清洗后的项目列表（sanitizeProjects 过滤无效条目）。 */
  const listProjects = async () => {
    const settings = await readSettingsFromDiskMigrated();
    return sanitizeProjects(settings?.projects || []);
  };

  /**
   * 按 id 查找项目：projectID 缺失抛 400，找不到抛 404。所有需要项目
   * 上下文的操作都以它做前置校验。
   */
  const findProjectByID = async (projectID) => {
    const normalized = asNonEmptyString(projectID);
    if (!normalized) throw new OMPChamberControlError('projectId is required', 400);
    const projects = await listProjects();
    const project = projects.find((entry) => entry.id === normalized) || null;
    if (!project) throw new OMPChamberControlError('Project not found', 404);
    return project;
  };

  /**
   * 把 `{ projectId }` 或 `{ directory }`（二选一）解析成项目 id：projectId
   * 路径先验证项目存在；directory 路径按 resolve 后的绝对路径精确匹配
   * 项目的 path。两者同时给出、都缺省或匹配不到分别抛 400 / 400 / 404。
   */
  const resolveProjectID = async ({ projectId, directory } = {}) => {
    const requestedProjectID = asNonEmptyString(projectId);
    const requestedDirectory = asNonEmptyString(directory);
    if (requestedProjectID && requestedDirectory) {
      throw new OMPChamberControlError('Provide only one of projectId or directory', 400);
    }
    if (requestedProjectID) {
      await findProjectByID(requestedProjectID);
      return requestedProjectID;
    }
    if (!requestedDirectory) throw new OMPChamberControlError('projectId or directory is required', 400);
    const resolvedDirectory = path.resolve(requestedDirectory);
    const projects = await listProjects();
    const project = projects.find((entry) => path.resolve(entry.path) === resolvedDirectory);
    if (!project) throw new OMPChamberControlError(`Project not found for directory: ${resolvedDirectory}`, 404);
    return project.id;
  };

  /** 列出项目全部定时任务：验证项目存在后先触发一次运行时同步（把 loop 文件对账进存储）再返回。 */
  const list = async (projectID) => {
    await findProjectByID(projectID);
    return scheduledTasksRuntime.syncProject(projectID);
  };

  /**
   * 定位由 loop markdown 文件管理的任务并校验其文件仍在磁盘上：taskId
   * 缺失抛 400，任务不存在抛 404，任务并非 loop 管理抛 400，loop 文件
   * 已缺失抛 404。是 setLoopEnabled / removeLoopFile 的共用前置。
   */
  const findLoopTask = async (projectID, taskID) => {
    await findProjectByID(projectID);
    const normalizedTaskID = asNonEmptyString(taskID);
    if (!normalizedTaskID) throw new OMPChamberControlError('taskId is required', 400);
    const tasks = await scheduledTasksRuntime.syncProject(projectID);
    const task = tasks.find((entry) => entry?.id === normalizedTaskID) || null;
    if (!task) throw new OMPChamberControlError('Task not found', 404);
    if (!task.loopFile) throw new OMPChamberControlError('Task is not managed by a loop file', 400);
    if (!fs.existsSync(task.loopFile)) throw new OMPChamberControlError('Loop file not found', 404);
    return task;
  };

  /**
   * 切换 loop 任务的 enabled 状态：写入 markdown frontmatter 而非 JSON
   * 存储。enabled 必须为布尔（否则 400）；文件当前非法（解析不出定义）
   * 时拒绝修改（400）；写文件失败包装为 500。成功后重新同步并返回更新
   * 后的任务（找不到时返回 null）。
   */
  const setLoopEnabled = async (projectID, taskID, enabled) => {
    if (typeof enabled !== 'boolean') {
      throw new OMPChamberControlError('enabled must be a boolean', 400);
    }
    const task = await findLoopTask(projectID, taskID);
    try {
      if (!setLoopFileEnabled(task.loopFile, enabled)) {
        throw new OMPChamberControlError('Loop file must be valid before changing its enabled state', 400);
      }
    } catch (error) {
      if (error instanceof OMPChamberControlError) throw error;
      const message = error instanceof Error ? error.message : 'Failed to update loop file';
      throw new OMPChamberControlError(message, 500);
    }
    const tasks = await scheduledTasksRuntime.syncProject(projectID);
    return tasks.find((entry) => entry.id === taskID) || null;
  };

  /** 删除 loop 任务背后的 markdown 文件（unlink）并重新同步，返回剩余任务列表；删除失败包装为 500。 */
  const removeLoopFile = async (projectID, taskID) => {
    const task = await findLoopTask(projectID, taskID);
    try {
      fs.unlinkSync(task.loopFile);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to delete loop file';
      throw new OMPChamberControlError(message, 500);
    }
    return scheduledTasksRuntime.syncProject(projectID);
  };

  /**
   * 新增或更新一条定时任务（写入项目配置存储）。taskInput 必须为对象
   * （否则 400）；存储层报错按消息内容映射 400 / 500。成功后同步运行时
   * 并返回 `{ tasks, task, created }`（task 优先取同步后的最新值）。
   */
  const upsert = async (projectID, taskInput) => {
    await findProjectByID(projectID);
    if (!taskInput || typeof taskInput !== 'object') {
      throw new OMPChamberControlError('task payload is required', 400);
    }
    let upserted;
    try {
      upserted = await projectConfigRuntime.upsertScheduledTask(projectID, taskInput);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to save scheduled task';
      const invalid = message.toLowerCase().includes('required') || message.toLowerCase().includes('invalid');
      throw new OMPChamberControlError(message, invalid ? 400 : 500);
    }
    await scheduledTasksRuntime.syncProject(projectID);
    const tasks = await projectConfigRuntime.listScheduledTasks(projectID);
    return {
      tasks,
      task: tasks.find((task) => task.id === upserted.task.id) || upserted.task,
      created: upserted.created,
    };
  };

  /**
   * 删除定时任务。loop 文件仍存在的任务拒绝删除（400）——文件才是删除
   * 入口，只删 JSON 行会被下一次对账“复活”；文件已消失的孤儿行允许直接
   * 删。taskId 缺失抛 400、任务不存在抛 404；删除后同步并返回剩余任务。
   */
  const remove = async (projectID, taskID) => {
    await findProjectByID(projectID);
    const normalizedTaskID = asNonEmptyString(taskID);
    if (!normalizedTaskID) throw new OMPChamberControlError('taskId is required', 400);
    const current = await projectConfigRuntime.listScheduledTasks(projectID);
    const existing = current.find((task) => task.id === normalizedTaskID) || null;
    if (existing?.loopFile && fs.existsSync(existing.loopFile)) {
      // Loop tasks are owned by their `.agents/loops` markdown file: deleting
      // the JSON row would be silently undone by the next reconcile while the
      // file exists. The file itself is the removal surface. Once the file is
      // gone (the task is an orphan that the next sync would remove anyway),
      // deleting the row is safe and allowed.
      throw new OMPChamberControlError(
        'Loop task is managed by its .agents/loops markdown file; delete the file to remove the task',
        400,
      );
    }
    const result = await projectConfigRuntime.deleteScheduledTask(projectID, normalizedTaskID);
    if (!result.deleted) throw new OMPChamberControlError('Task not found', 404);
    await scheduledTasksRuntime.syncProject(projectID);
    return projectConfigRuntime.listScheduledTasks(projectID);
  };

  /**
   * 立即触发一次任务执行。任务运行中或已排队抛 409；任务不存在或未启用
   * 抛 404；执行失败抛 500（附带 task）。成功返回
   * `{ task, sessionId, persistError? }`（persistError 为状态持久化的非致命警告）。
   */
  const run = async (projectID, taskID) => {
    await findProjectByID(projectID);
    const normalizedTaskID = asNonEmptyString(taskID);
    if (!normalizedTaskID) throw new OMPChamberControlError('taskId is required', 400);
    const result = await scheduledTasksRuntime.runNow(projectID, normalizedTaskID);
    if (result.running || result.queued) {
      throw new OMPChamberControlError(result.error || 'Task already running', 409);
    }
    if (result.skipped) throw new OMPChamberControlError('Task not found or disabled', 404);
    if (!result.ok) {
      throw new OMPChamberControlError(result.error || 'Task run failed', 500, { task: result.task });
    }
    return {
      task: result.task,
      sessionId: result.sessionID,
      ...(typeof result.persistError === 'string' && result.persistError.trim()
        ? { persistError: result.persistError.trim() }
        : {}),
    };
  };

  /** 切换任意任务的 enabled 状态（loop 与 JSON 任务统一经 upsert 路径落盘），返回更新后的任务；任务不存在抛 404。 */
  const setEnabled = async (projectID, taskID, enabled) => {
    const tasks = await list(projectID);
    const task = tasks.find((entry) => entry?.id === taskID);
    if (!task) throw new OMPChamberControlError('Task not found', 404);
    const result = await upsert(projectID, { ...task, enabled });
    return result.task;
  };

  /**
   * 汇总全部项目的定时任务状态。运行时提供 getStatus 时直接透传；否则
   * 逐项目扫描存储统计 enabled / running 数量，返回
   * `{ hasEnabledScheduledTasks, hasRunningScheduledTasks,
   *    enabledScheduledTasksCount, runningScheduledTasksCount }`。
   * 单个项目读取失败被静默忽略（空 catch）。
   */
  const status = async () => {
    if (typeof scheduledTasksRuntime.getStatus === 'function') {
      return scheduledTasksRuntime.getStatus();
    }
    const projects = await listProjects();
    let enabledCount = 0;
    let runningCount = 0;
    for (const project of projects) {
      try {
        const tasks = await projectConfigRuntime.listScheduledTasks(project.id);
        for (const task of tasks) {
          if (task?.enabled) enabledCount += 1;
          if (task?.state?.lastStatus === 'running') runningCount += 1;
        }
      } catch {
      }
    }
    return {
      hasEnabledScheduledTasks: enabledCount > 0,
      hasRunningScheduledTasks: runningCount > 0,
      enabledScheduledTasksCount: enabledCount,
      runningScheduledTasksCount: runningCount,
    };
  };

  return {
    listProjects,
    resolveProjectID,
    list,
    upsert,
    remove,
    run,
    setEnabled,
    setLoopEnabled,
    removeLoopFile,
    status,
  };
};
