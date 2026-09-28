/**
 * OMPChamber agent memory routes.
 *
 * The scope is a query parameter rather than part of the path, because global
 * and project memory are the same resource with two homes: one set of handlers
 * that resolve `?scope=global` or `?scope=project&projectId=...`. Getting the
 * scope wrong must fail loudly, never silently write the user's global memory
 * from a project-scoped call.
 *
 * Memory is created by the agent through the `ompchamber_memory` tool, so
 * there is no create route here; the panel reads, corrects, and deletes.
 *
 * The body parser is attached per route rather than globally: the generic
 * OpenCode proxy needs an unread request stream, so `core-routes` parses only
 * an explicit allowlist of path prefixes. A route that forgets this sees
 * `req.body` as undefined and rejects every write as a malformed body.
 */
/**
 * OMPChamber agent memory 的 HTTP 路由（中文说明）。
 *
 * scope 用查询参数而非路径表达，因为全局与项目记忆是同一资源的两个归
 * 处：一组 handler 统一解析 ?scope=global 或 ?scope=project&projectId=…
 * 。scope 解析错误必须响亮失败，绝不能把项目范围的调用静默写进用户的
 * 全局记忆。
 *
 * 记忆由 agent 通过 ompchamber_memory 工具创建，因此这里没有 create 路
 * 由；面板只做读取、修正与删除。
 *
 * body 解析器按路由单独挂载而不是全局挂载：通用的 OpenCode proxy 需要
 * 未被读取的请求流，core-routes 只解析白名单里的路径前缀。忘了挂解
 * 析器的路由会看到 req.body 是 undefined，并把每个写请求都当作畸形请
 * 求拒掉。
 */

import express from 'express';

/** 单独挂到写路由上的 JSON body 解析器（上限 1mb），原因见文件头。 */
const parseJsonBody = express.json({ limit: '1mb' });

/** 记忆类型白名单：fact（事实）、preference（偏好）、reference（指引）。 */
const MEMORY_TYPES = new Set(['fact', 'preference', 'reference']);

/** 判断值是否为非数组的普通对象（PATCH body 的形状检查）。 */
const isObjectRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 依据错误消息判断是否为输入校验类错误（应回 400 而不是 500）。 */
const isValidationError = (error) => {
  const message = error instanceof Error ? error.message : '';
  return message.includes('is required')
    || message.includes('unsupported characters')
    || message.includes('holds at most');
};

/**
 * 统一错误出口：校验类错误回 400，其余回 500，响应体固定为
 * { error: message }；error 不是 Error 实例时用 fallbackMessage 兜底。
 */
const respondWithError = (res, error, fallbackMessage) => {
  const message = error instanceof Error ? error.message : fallbackMessage;
  if (isValidationError(error)) {
    return res.status(400).json({ error: message });
  }
  return res.status(500).json({ error: message || fallbackMessage });
};

/**
 * Resolves the target scope, or returns the reason it could not be resolved.
 * A project request without an id is rejected here rather than quietly falling
 * back to global, which would write project facts into every other project.
 */
/**
 * 解析查询参数得到目标 scope，失败时返回原因而不是悄悄回退到全局。
 * 项目 scope 缺 projectId 在这里就被拒绝——否则项目事实会被写进其它每
 * 一个项目。返回 { target } 或 { error } 二者之一。
 */
const resolveScope = (query) => {
  if (query.scope === 'global') {
    return { target: { scope: 'global' } };
  }
  if (query.scope === 'project') {
    if (typeof query.projectId !== 'string' || query.projectId.trim().length === 0) {
      return { error: 'projectId is required for project scope' };
    }
    return { target: { scope: 'project', projectId: query.projectId } };
  }
  return { error: 'scope must be global or project' };
};

/**
 * 在 express 应用上注册 agent memory 的读、改、删路由。
 * 依赖：agentMemoryRuntime（存储运行时）与 isAgentMemoryEnabled（异步
 * 设置开关，可省略）。所有路由都先过 requireEnabled 闸门。
 */
export const registerAgentMemoryRoutes = (app, dependencies) => {
  const { agentMemoryRuntime, isAgentMemoryEnabled } = dependencies;

  /**
   * One gate for the whole surface. The settings toggle disables the feature,
   * not just its UI: with memory off, these routes must not read or write the
   * store at all, or a stale client would keep editing memory the user believes
   * is turned off.
   */
  /**
   * 整个路由面的统一闸门（中文补充）。设置开关关掉的是功能本身而不仅是
   * 它的 UI：记忆关闭后这些路由不得读写存储，否则残留的旧客户端会继续
   * 编辑用户以为已经关掉的记忆。设置读不出来时按关闭处理（503）。
   */
  const requireEnabled = async (_req, res, next) => {
    if (!isAgentMemoryEnabled) {
      return next();
    }
    try {
      // Awaited: the setting is read from disk, and testing the returned
      // promise for truthiness would leave the gate permanently open.
      if (!(await isAgentMemoryEnabled())) {
        // Flagged, not merely 404: a missing entry answers 404 too, and a
        // client that could not tell them apart would report a deleted memory
        // as the whole feature being switched off.
        return res.status(404).json({ error: 'Agent memory is disabled', disabled: true });
      }
    } catch {
      // An unreadable settings file must not silently expose a surface the
      // user may have turned off.
      return res.status(503).json({ error: 'Agent memory availability is unknown' });
    }
    return next();
  };

  /** 列出某一 scope 的记忆：scope 无效回 400，读取失败走统一错误出口。 */
  app.get('/api/agent-memory', requireEnabled, async (req, res) => {
    const { target, error } = resolveScope(req.query);
    if (error) {
      return res.status(400).json({ error });
    }
    try {
      return res.json(await agentMemoryRuntime.read(target));
    } catch (caught) {
      return respondWithError(res, caught, 'Failed to read agent memory');
    }
  });

  /**
   * Both scopes in one response. The panel always shows them together, and two
   * separate requests would let one scope render while the other is still
   * loading, which reads as memory that has gone missing.
   */
  /**
   * 一次返回两个 scope（中文补充）。面板总是并排展示它们，拆成两个请求
   * 会让一边先渲染完、看起来像记忆消失了。projectId 可省略（无项目打开
   * 时只读全局）。
   */
  app.get('/api/agent-memory/all', requireEnabled, async (req, res) => {
    const projectId = typeof req.query.projectId === 'string' && req.query.projectId.trim().length > 0
      ? req.query.projectId
      : null;
    try {
      return res.json(await agentMemoryRuntime.readAll(projectId));
    } catch (caught) {
      return respondWithError(res, caught, 'Failed to read agent memory');
    }
  });

  /**
   * 修正一条记忆：body 必须是对象，title/body 若出现必须是字符串，type
   * 必须属于 MEMORY_TYPES；只把显式给出的字段传给 runtime.update。更新
   * 未命中（返回 null）时回 404。本组路由中唯一带 body 的写路由，因此
   * 单独挂 parseJsonBody。
   */
  app.patch('/api/agent-memory/:memoryId', requireEnabled, parseJsonBody, async (req, res) => {
    const { target, error } = resolveScope(req.query);
    if (error) {
      return res.status(400).json({ error });
    }

    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (body.title !== undefined && typeof body.title !== 'string') {
      return res.status(400).json({ error: 'title must be a string' });
    }
    if (body.body !== undefined && typeof body.body !== 'string') {
      return res.status(400).json({ error: 'body must be a string' });
    }
    if (body.type !== undefined && !MEMORY_TYPES.has(body.type)) {
      return res.status(400).json({ error: 'type must be fact, preference, or reference' });
    }

    try {
      const result = await agentMemoryRuntime.update(target, req.params.memoryId, {
        ...(body.title !== undefined ? { title: body.title } : {}),
        ...(body.body !== undefined ? { body: body.body } : {}),
        ...(body.type !== undefined ? { type: body.type } : {}),
      });
      if (!result) {
        return res.status(404).json({ error: 'Memory not found' });
      }
      return res.json(result);
    } catch (caught) {
      return respondWithError(res, caught, 'Failed to save memory');
    }
  });

  /** 删除指定 scope 下某 memoryId 的记忆；runtime 报告未删除时回 404。 */
  app.delete('/api/agent-memory/:memoryId', requireEnabled, async (req, res) => {
    const { target, error } = resolveScope(req.query);
    if (error) {
      return res.status(400).json({ error });
    }

    try {
      const result = await agentMemoryRuntime.remove(target, req.params.memoryId);
      if (!result.deleted) {
        return res.status(404).json({ error: 'Memory not found' });
      }
      return res.json(result);
    } catch (caught) {
      return respondWithError(res, caught, 'Failed to delete memory');
    }
  });
};
