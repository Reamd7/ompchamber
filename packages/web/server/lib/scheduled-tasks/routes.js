/**
 * 定时任务的 HTTP 路由注册：把 /api/projects/:projectId/scheduled-tasks
 * 系列 REST 端点与 /api/ompchamber 全局端点挂到 express app 上；参数做
 * 最小校验后委托给 createScheduledTaskService，服务层错误按 statusCode
 * 透传，其余包装为 500。此外还挂载全局 SSE 事件流 /api/ompchamber/events
 * （客户端注册、就绪事件与 25 秒心跳）。
 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 从 req.params 提取非空 projectId；缺失返回 null（由各路由转成 400）。 */
const parseProjectID = (req) => asNonEmptyString(req?.params?.projectId);
/** 从 req.params 提取非空 taskId；缺失返回 null（由各路由转成 400）。 */
const parseTaskID = (req) => asNonEmptyString(req?.params?.taskId);

/**
 * 在 express app 上注册定时任务相关路由。
 *
 * @param {object} app express 应用实例
 * @param {object} dependencies 注入依赖（设置读取、项目清洗、项目配置与
 *   定时任务运行时、SSE 客户端注册表 getOMPChamberEventClients 与写入器
 *   writeSseEvent）；scheduledTaskService 缺省时用这些依赖现场构建
 */
export const registerScheduledTaskRoutes = (app, dependencies) => {
  const {
    readSettingsFromDiskMigrated,
    sanitizeProjects,
    projectConfigRuntime,
    scheduledTasksRuntime,
    getOMPChamberEventClients,
    writeSseEvent,
    scheduledTaskService = createScheduledTaskService(dependencies),
  } = dependencies;

  // 列出项目全部定时任务；projectId 缺失 400，服务层错误按 statusCode 透传。
  app.get('/api/projects/:projectId/scheduled-tasks', async (req, res) => {
    const projectID = parseProjectID(req);
    if (!projectID) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    try {
      const tasks = await scheduledTaskService.list(projectID);
      return res.json({ tasks });
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message });
      console.error('[ScheduledTasks] failed to load tasks:', error);
      return res.status(500).json({ error: 'Failed to load scheduled tasks' });
    }
  });

  // 新增 / 更新任务（body.task 必须为对象，否则 400）；错误消息含
  // required / invalid 时映射 400，其余 500。
  app.put('/api/projects/:projectId/scheduled-tasks', async (req, res) => {
    const projectID = parseProjectID(req);
    if (!projectID) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    const taskInput = req.body && typeof req.body === 'object' ? req.body.task : null;
    if (!taskInput || typeof taskInput !== 'object') {
      return res.status(400).json({ error: 'task payload is required' });
    }

    try {
      return res.json(await scheduledTaskService.upsert(projectID, taskInput));
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message });
      const message = error instanceof Error ? error.message : 'Failed to save scheduled task';
      const statusCode = message.toLowerCase().includes('required') || message.toLowerCase().includes('invalid')
        ? 400
        : 500;
      if (statusCode === 500) {
        console.error('[ScheduledTasks] failed to save task:', error);
      }
      return res.status(statusCode).json({ error: message });
    }
  });

  // 删除任务（loop 文件管理的任务会被服务层拒绝，需走 loop-file 删除端点）。
  app.delete('/api/projects/:projectId/scheduled-tasks/:taskId', async (req, res) => {
    const projectID = parseProjectID(req);
    const taskID = parseTaskID(req);
    if (!projectID) {
      return res.status(400).json({ error: 'projectId is required' });
    }
    if (!taskID) {
      return res.status(400).json({ error: 'taskId is required' });
    }

    try {
      return res.json({ tasks: await scheduledTaskService.remove(projectID, taskID) });
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message });
      console.error('[ScheduledTasks] failed to delete task:', error);
      return res.status(500).json({ error: 'Failed to delete scheduled task' });
    }
  });

  // 切换 loop 文件管理任务的 enabled（写入 markdown frontmatter 而非 JSON 存储）。
  app.patch('/api/projects/:projectId/scheduled-tasks/:taskId/loop-file', async (req, res) => {
    const projectID = parseProjectID(req);
    const taskID = parseTaskID(req);
    if (!projectID) return res.status(400).json({ error: 'projectId is required' });
    if (!taskID) return res.status(400).json({ error: 'taskId is required' });
    try {
      const task = await scheduledTaskService.setLoopEnabled(projectID, taskID, req.body?.enabled);
      return res.json({ task });
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message });
      console.error('[ScheduledTasks] failed to update loop file:', error);
      return res.status(500).json({ error: 'Failed to update loop file' });
    }
  });

  // 删除 loop 任务的 markdown 文件本体，返回剩余任务列表。
  app.delete('/api/projects/:projectId/scheduled-tasks/:taskId/loop-file', async (req, res) => {
    const projectID = parseProjectID(req);
    const taskID = parseTaskID(req);
    if (!projectID) return res.status(400).json({ error: 'projectId is required' });
    if (!taskID) return res.status(400).json({ error: 'taskId is required' });
    try {
      return res.json({ tasks: await scheduledTaskService.removeLoopFile(projectID, taskID) });
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message });
      console.error('[ScheduledTasks] failed to delete loop file:', error);
      return res.status(500).json({ error: 'Failed to delete loop file' });
    }
  });

  // 立即执行一次任务；409 / 404 / 500 由服务层错误码透传，成功附带 ok:true。
  app.post('/api/projects/:projectId/scheduled-tasks/:taskId/run', async (req, res) => {
    const projectID = parseProjectID(req);
    const taskID = parseTaskID(req);
    if (!projectID) {
      return res.status(400).json({ error: 'projectId is required' });
    }
    if (!taskID) {
      return res.status(400).json({ error: 'taskId is required' });
    }

    try {
      return res.json({ ok: true, ...await scheduledTaskService.run(projectID, taskID) });
    } catch (error) {
      if (error?.statusCode) return res.status(error.statusCode).json({ error: error.message, ...(error.task ? { task: error.task } : {}) });
      console.error('[ScheduledTasks] failed to run task:', error);
      return res.status(500).json({ error: 'Failed to run scheduled task' });
    }
  });

  // 全局定时任务状态汇总（是否存在启用 / 运行中的任务），供关机阻断等逻辑使用。
  app.get('/api/ompchamber/scheduled-tasks/status', async (_req, res) => {
    try {
      return res.json(await scheduledTaskService.status());
    } catch (error) {
      console.error('[ScheduledTasks] failed to resolve scheduled task status:', error);
      return res.status(500).json({ error: 'Failed to resolve scheduled task status' });
    }
  });

  // 全局 SSE 事件流：设置 SSE 响应头并冲刷、按 ?browser=1 标记客户端能力、
  // 注册到客户端集合、发送就绪事件，之后每 25 秒心跳；写入失败或连接关闭
  // 时清理心跳并从集合移除。
  app.get('/api/ompchamber/events', (req, res) => {
    res.setHeader('Content-Type', 'text/event-stream; charset=utf-8');
    res.setHeader('Cache-Control', 'no-cache, no-transform');
    res.setHeader('Connection', 'keep-alive');
    res.setHeader('X-Accel-Buffering', 'no');
    res.flushHeaders?.();
    // 中文补充：把“能否驱动浏览器视图”记录在连接上（res.ompchamberBrowserCapable），
    // 桌面壳与浏览器标签可同时连着同一台服务器，连接级标记不会像全局开关那样过期。

    // Whether a client can drive a browser view is a property of that client,
    // not of this server: a desktop shell and a browser tab can be connected to
    // the same server at once. Recording it on the connection keeps the answer
    // current without any enable/disable setting to go stale.
    res.ompchamberBrowserCapable = req.query?.browser === '1';

    const clients = getOMPChamberEventClients();
    clients.add(res);

    try {
      writeSseEvent(res, {
        type: 'ompchamber:event-stream-ready',
        properties: {
          connectedAt: Date.now(),
        },
      });
    } catch {
    }

    const heartbeat = setInterval(() => {
      try {
        writeSseEvent(res, {
          type: 'ompchamber:heartbeat',
          properties: {
            timestamp: Date.now(),
          },
        });
      } catch {
        clearInterval(heartbeat);
        clients.delete(res);
      }
    }, 25_000);

    req.on('close', () => {
      clearInterval(heartbeat);
      clients.delete(res);
    });
  });
};
import { createScheduledTaskService } from './service.js';
