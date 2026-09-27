/**
 * OMPChamber project context routes: notes, todos, and plan files.
 *
 * These replace the shared UI's direct `/api/fs/*` access to
 * `~/.config/ompchamber/projects/*`. The client no longer resolves the home
 * directory or composes storage paths, and plan markdown is addressed by id
 * rather than by an absolute path supplied by the caller.
 *
 * Body parsing is attached per route. There is no global JSON parser: the
 * generic OpenCode proxy needs an unread request stream, so `core-routes`
 * parses only an explicit allowlist of path prefixes and leaves every other
 * `/api` request untouched. A route that forgets this sees `req.body` as
 * undefined and rejects every write as a malformed body.
 */
/**
 * project-context 路由（中文说明）：notes、todos 与 plan 文件的读写 API。
 *
 * 取代共享 UI 直接访问 `/api/fs/*`（`~/.config/ompchamber/projects/*`）
 * 的旧方案：客户端不再解析 home 目录或拼接存储路径，plan markdown 也
 * 改为按 id 寻址而非调用方传入的绝对路径。
 *
 * body 解析必须按路由单独挂载——服务器没有全局 JSON parser（OpenCode
 * proxy 需要保持请求流未读，core-routes 只解析显式白名单前缀），
 * 漏挂的写路由会看到 `req.body` 为 undefined，把所有写请求当非法
 * body 拒绝。
 */

import express from 'express';

/** 按路由挂载的 express JSON body parser（1mb 上限）；服务器没有全局 parser，见模块说明。 */
const parseJsonBody = express.json({ limit: '1mb' });

/** 判断值是否为非数组、非 null 的普通对象（所有请求体形状检查的基础）。 */
const isObjectRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 依据错误消息判断是否为调用方输入问题（缺少必填字段 / projectId 含非法字符），决定走 400 而非 500。 */
const isValidationError = (error) => {
  const message = error instanceof Error ? error.message : '';
  return message.includes('is required') || message.includes('unsupported characters');
};

/** 统一错误响应：校验类错误回 400，其余回 500；非 Error 值回退到 fallbackMessage。 */
const respondWithError = (res, error, fallbackMessage) => {
  const message = error instanceof Error ? error.message : fallbackMessage;
  if (isValidationError(error)) {
    return res.status(400).json({ error: message });
  }
  return res.status(500).json({ error: message || fallbackMessage });
};

/** note 的合法来源枚举：manual（手动）、selection（编辑器选中文本）、agent（代理会话）。 */
const isValidNoteSource = (value) => value === 'manual' || value === 'selection' || value === 'agent';

/**
 * 校验 todos 数组形状：每项必须是对象且 id/text 为字符串，
 * 可选的 completed 为 boolean、createdAt 为有限数字；任何一项不合规即整体拒绝。
 */
const hasValidTodosShape = (value) => (
  Array.isArray(value)
  && value.every((todo) => (
    isObjectRecord(todo)
    && typeof todo.id === 'string'
    && typeof todo.text === 'string'
    && (todo.completed === undefined || typeof todo.completed === 'boolean')
    && (todo.createdAt === undefined || (typeof todo.createdAt === 'number' && Number.isFinite(todo.createdAt)))
  ))
);

/**
 * 在 express app 上注册 project-context 的全部路由：context 整读、
 * todos 整体保存、notes 的创建/更新/删除、plans 的读取/保存/创建/
 * 删除与置顶。所有写路由各自挂 parseJsonBody（见模块说明）。
 *
 * @param dependencies.projectContextRuntime 注入的实际读写实现
 *   （projectId 校验、路径解析与文件 IO），便于测试替换。
 */
export const registerProjectContextRoutes = (app, dependencies) => {
  const { projectContextRuntime } = dependencies;

  // 读取整个 project context（notes/todos/plans 元数据）。
  app.get('/api/project-context/:projectId', async (req, res) => {
    try {
      return res.json(await projectContextRuntime.readContext(req.params.projectId));
    } catch (error) {
      return respondWithError(res, error, 'Failed to read project context');
    }
  });

  // 整体覆盖保存 todos 列表。
  app.put('/api/project-context/:projectId/todos', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (!hasValidTodosShape(body.todos)) {
      return res.status(400).json({ error: 'todos must be an array of todo items' });
    }

    try {
      return res.json(await projectContextRuntime.saveTodos(req.params.projectId, body.todos));
    } catch (error) {
      return respondWithError(res, error, 'Failed to save project todos');
    }
  });

  // 创建 note，成功返回 201 与新建 note 及最新 context。
  app.post('/api/project-context/:projectId/notes', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (typeof body.body !== 'string') {
      return res.status(400).json({ error: 'body must be a string' });
    }
    if (body.source !== undefined && !isValidNoteSource(body.source)) {
      return res.status(400).json({ error: 'source must be manual, selection, or agent' });
    }
    if (body.origin !== undefined && !isObjectRecord(body.origin)) {
      return res.status(400).json({ error: 'origin must be an object' });
    }

    try {
      const { note, context } = await projectContextRuntime.createNote(req.params.projectId, {
        body: body.body,
        source: body.source,
        origin: body.origin,
      });
      return res.status(201).json({ note, context });
    } catch (error) {
      return respondWithError(res, error, 'Failed to create note');
    }
  });

  // 部分更新 note 的 body/pinned；目标不存在由 runtime 返回 null 后映射为 404。
  app.patch('/api/project-context/:projectId/notes/:noteId', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (body.body !== undefined && typeof body.body !== 'string') {
      return res.status(400).json({ error: 'body must be a string' });
    }
    if (body.pinned !== undefined && typeof body.pinned !== 'boolean') {
      return res.status(400).json({ error: 'pinned must be a boolean' });
    }

    try {
      const result = await projectContextRuntime.updateNote(req.params.projectId, req.params.noteId, {
        ...(body.body !== undefined ? { body: body.body } : {}),
        ...(body.pinned !== undefined ? { pinned: body.pinned } : {}),
      });
      if (!result) {
        return res.status(404).json({ error: 'Note not found' });
      }
      return res.json(result);
    } catch (error) {
      return respondWithError(res, error, 'Failed to save note');
    }
  });

  // 删除 note，成功返回删除后的 context。
  app.delete('/api/project-context/:projectId/notes/:noteId', async (req, res) => {
    try {
      const { deleted, context } = await projectContextRuntime.deleteNote(
        req.params.projectId,
        req.params.noteId,
      );
      if (!deleted) {
        return res.status(404).json({ error: 'Note not found' });
      }
      return res.json(context);
    } catch (error) {
      return respondWithError(res, error, 'Failed to delete note');
    }
  });

  // 设置 plan 的置顶状态（仅接受布尔 pinned）。
  app.patch('/api/project-context/:projectId/plans/:planId', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body) || typeof body.pinned !== 'boolean') {
      return res.status(400).json({ error: 'pinned must be a boolean' });
    }

    try {
      const result = await projectContextRuntime.setPlanPinned(
        req.params.projectId,
        req.params.planId,
        body.pinned,
      );
      if (!result) {
        return res.status(404).json({ error: 'Plan not found' });
      }
      return res.json(result);
    } catch (error) {
      return respondWithError(res, error, 'Failed to update plan');
    }
  });

  // 读取单个 plan 的完整内容（含 markdown 正文）。
  app.get('/api/project-context/:projectId/plans/:planId', async (req, res) => {
    try {
      const plan = await projectContextRuntime.readPlan(req.params.projectId, req.params.planId);
      if (!plan) {
        return res.status(404).json({ error: 'Plan not found' });
      }
      return res.json(plan);
    } catch (error) {
      return respondWithError(res, error, 'Failed to read plan');
    }
  });

  // 以 raw markdown 整体保存 plan。
  app.put('/api/project-context/:projectId/plans/:planId', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (typeof body.raw !== 'string') {
      return res.status(400).json({ error: 'raw must be a string' });
    }

    try {
      const result = await projectContextRuntime.updatePlan(
        req.params.projectId,
        req.params.planId,
        { raw: body.raw },
      );
      if (!result) {
        return res.status(404).json({ error: 'Plan not found' });
      }
      return res.json(result);
    } catch (error) {
      return respondWithError(res, error, 'Failed to save plan');
    }
  });

  // 创建 plan，成功返回 201。
  app.post('/api/project-context/:projectId/plans', parseJsonBody, async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (typeof body.body !== 'string') {
      return res.status(400).json({ error: 'body must be a string' });
    }
    if (body.title !== undefined && typeof body.title !== 'string') {
      return res.status(400).json({ error: 'title must be a string' });
    }

    try {
      const { plan, context } = await projectContextRuntime.createPlan(req.params.projectId, {
        title: body.title ?? '',
        body: body.body,
      });
      return res.status(201).json({ plan, context });
    } catch (error) {
      return respondWithError(res, error, 'Failed to create plan');
    }
  });

  // 删除 plan，成功返回删除后的 context。
  app.delete('/api/project-context/:projectId/plans/:planId', async (req, res) => {
    try {
      const { deleted, context } = await projectContextRuntime.deletePlan(
        req.params.projectId,
        req.params.planId,
      );
      if (!deleted) {
        return res.status(404).json({ error: 'Plan not found' });
      }
      return res.json(context);
    } catch (error) {
      return respondWithError(res, error, 'Failed to delete plan');
    }
  });
};
