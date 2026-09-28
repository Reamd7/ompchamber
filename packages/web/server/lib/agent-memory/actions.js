/**
 * Dispatch for the `memory.*` actions the `ompchamber_memory` tool calls.
 *
 * Kept beside the store rather than inside the control service, because the
 * control service already owns sessions, schedules and the browser; memory
 * shares none of that machinery and only needs the same envelope.
 *
 * Project scope is derived from the session's directory, never from the model.
 * Letting the agent name a project id would let a memory learned in one
 * checkout be filed against another, which the user would have no way to
 * notice.
 *
 * The directory is resolved to the project first. A session running in a
 * worktree has the worktree's own path, and keying memory by that path filed it
 * under a project the panel never looks at — the memory was written, stored,
 * and invisible. Every worktree of a repository shares one project memory,
 * which is also what the user means by "this project".
 */
/**
 * ompchamber_memory 工具所调 memory.* 动作的分发层（中文说明）。
 *
 * 放在存储旁边而不是塞进 control service，因为后者已经管着会话、定时
 * 任务和浏览器；记忆不共享那些机制，只需要同样的信封格式。
 *
 * 项目 scope 由会话目录推导，绝不采信模型提供的项目 ID：否则 agent 可
 * 以把在一个 checkout 里学到的记忆记到另一个项目名下，用户无从察觉。
 *
 * 目录先解析到项目。运行在 worktree 里的会话拿到的是 worktree 自己的
 * 路径，直接按它归档会落进面板永远不读的项目——记忆写了、存了、却看
 * 不见。同一仓库的所有 worktree 共享一份项目记忆，这也正是用户所说的
 * "这个项目"。
 */

/** 记忆类型白名单：fact（事实）、preference（偏好）、reference（指引）。 */
const MEMORY_TYPES = new Set(['fact', 'preference', 'reference']);

/** 把值规整为 trim 后的非空字符串；非字符串或空白返回 null。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** Everything the agent is told about an entry it has not opened yet. */
/** 条目摘要（中文补充）：agent 打开条目前能看到的全部——id、标题、类型、scope，不含正文。 */
const toSummary = (entry, scope) => ({
  memoryId: entry.id,
  title: entry.title,
  type: entry.type,
  scope,
});

/** 在摘要之上附加正文 body，构成完整条目，用于 memory.read 的返回。 */
const toFullEntry = (entry, scope) => ({ ...toSummary(entry, scope), body: entry.body });

/**
 * 创建 agent memory 动作集。依赖：agentMemoryRuntime（存储运行时）、
 * createError（构造带状态码的错误）、onMemoryChanged（写入后的变更通
 * 知，可省略）、resolveProjectId（目录到项目 ID 的解析）、
 * isAgentMemoryEnabled（设置开关，可省略）。返回 { execute }。
 */
export const createAgentMemoryActions = (dependencies) => {
  const {
    agentMemoryRuntime,
    createError,
    onMemoryChanged,
    resolveProjectId: resolveProjectIdForDirectory,
    isAgentMemoryEnabled,
  } = dependencies;

  /**
   * Announce a write so an open panel shows it without being reopened. The
   * agent writes here on its own initiative, so without this the user only
   * learns what was stored the next time something else happens to reload.
   *
   * Never allowed to fail the action: the memory is already on disk, and a
   * broken notification must not report the write as failed.
   */
  /**
   * 写入后广播变更，让打开着的面板无需重开就能看到（中文补充）。绝不能
   * 让通知失败连累动作本身：记忆已经落盘，坏掉的通知不能把写入报成失
   * 败。
   */
  const announce = (scope, projectId) => {
    if (typeof onMemoryChanged !== 'function') return;
    try {
      onMemoryChanged({ scope, ...(projectId ? { projectId } : {}) });
    } catch {
      // A listener that throws must not take the write down with it.
    }
  };

  /** 抛出 createError 构造的错误，默认 400；所有输入校验失败的统一出口。 */
  const fail = (message, status = 400) => {
    throw createError(message, status);
  };

  /**
   * 由会话目录解析项目 ID；目录为空或解析不出结果时以 400 失败——项目
   * 记忆必须有明确归属，宁可不写也不能猜。
   */
  const resolveProjectId = async (contextDirectory) => {
    const directory = asNonEmptyString(contextDirectory);
    const projectId = directory ? await resolveProjectIdForDirectory(directory) : '';
    if (!projectId) {
      fail('Project memory needs a session directory, and this session has none', 400);
    }
    return projectId;
  };

  /**
   * 把输入中的 scope 规整为目标：global 原样通过；project 先经
   * resolveProjectId 从会话目录推导项目 ID（不采信模型给的）；其余取值
   * 一律 400。
   */
  const resolveTarget = async (input, contextDirectory) => {
    const scope = asNonEmptyString(input.scope);
    if (scope === 'global') return { scope: 'global' };
    if (scope === 'project') {
      return { scope: 'project', projectId: await resolveProjectId(contextDirectory) };
    }
    return fail('scope must be global or project', 400);
  };

  /**
   * 列出两个 scope 的全部摘要。加载失败的 scope 带
   * globalUnavailable/projectUnavailable 标记而不是伪装成空列表——被告
   * 知"没有记忆"的 agent 会把它们全部再存一遍。
   */
  const listBothScopes = async (contextDirectory) => {
    const directory = asNonEmptyString(contextDirectory);
    const projectId = directory ? await resolveProjectIdForDirectory(directory) : null;
    const result = await agentMemoryRuntime.readAll(projectId);

    // A scope that failed to load is reported, never rendered as empty: an
    // agent told it has no memories will happily store them all again.
    return {
      memories: [
        ...result.global.map((entry) => toSummary(entry, 'global')),
        ...result.project.map((entry) => toSummary(entry, 'project')),
      ],
      ...(result.globalFailed ? { globalUnavailable: true } : {}),
      ...(result.projectFailed ? { projectUnavailable: true } : {}),
    };
  };

  /**
   * memory.list：scope 缺省或为 both 时列两个 scope，否则列出指定
   * scope 的摘要。返回 { memories }，永不携带正文。
   */
  const list = async (input, contextDirectory) => {
    const scope = asNonEmptyString(input.scope);
    if (!scope || scope === 'both') {
      return listBothScopes(contextDirectory);
    }
    const target = await resolveTarget(input, contextDirectory);
    const { entries } = await agentMemoryRuntime.read(target);
    return { memories: entries.map((entry) => toSummary(entry, target.scope)) };
  };

  /**
   * Reading by title as well as by id is deliberate: the session index lists
   * titles only, so requiring an id would force a list call before every read
   * just to translate what the agent can already see.
   *
   * Scope is optional here. It decides everything for a write — a fact filed
   * globally reaches every project — but for a read it is only which drawer to
   * open, and demanding it turned a legible request into an error the model had
   * to recover from. Omitted, both stores are searched.
   */
  /**
   * memory.read（中文补充）：按 memoryId 或 title 读取完整条目。允许只
   * 给 title 是刻意的——会话索引只列 title，强制 id 会让每次读取前都多
   * 一次 list 调用。scope 在这里可省：对写它决定一切，对读只是开哪个抽
   * 屉；省略时先查项目存储、再查全局存储。存储读不到时回 503 而不是报
   * 告"没有这条记忆"。
   */
  const read = async (input, contextDirectory) => {
    const memoryId = asNonEmptyString(input.memoryId);
    const title = asNonEmptyString(input.title);
    if (!memoryId && !title) {
      fail('memory.read requires memoryId or title', 400);
    }

    // 匹配谓词：给了 memoryId 就按 id 精确匹配，否则按 title 忽略大小写匹配。
    const matches = (entry) => (memoryId
      ? entry.id === memoryId
      : entry.title.toLowerCase() === title.toLowerCase());

    const requestedScope = asNonEmptyString(input.scope);
    if (requestedScope === 'global' || requestedScope === 'project') {
      const target = await resolveTarget(input, contextDirectory);
      const { entries } = await agentMemoryRuntime.read(target);
      const found = entries.find(matches);
      if (!found) {
        fail('No memory matches that id or title in this scope', 404);
      }
      return { memory: toFullEntry(found, target.scope) };
    }

    const directory = asNonEmptyString(contextDirectory);
    const projectId = directory ? await resolveProjectIdForDirectory(directory) : null;
    const result = await agentMemoryRuntime.readAll(projectId);

    const projectMatch = result.project.find(matches);
    if (projectMatch) {
      // Project first: when both stores hold the same title, the one about this
      // codebase is the one being asked about.
      return { memory: toFullEntry(projectMatch, 'project') };
    }
    const globalMatch = result.global.find(matches);
    if (globalMatch) {
      return { memory: toFullEntry(globalMatch, 'global') };
    }
    if (result.globalFailed || result.projectFailed) {
      // Never reported as "no such memory": a store that failed to load may well
      // hold it, and the agent would go on to store it a second time.
      fail('Stored memory could not be read; try again before assuming it is absent', 503);
    }
    fail('No memory matches that id or title', 404);
  };

  /**
   * memory.save：写入记忆。title 与 body 必填，type 可选（须在白名单
   * 内）。写入后调用 announce 通知面板；返回摘要而非回显正文（防止模
   * 型读回后反复"改进"重存），并明确告知 replaced 与注入警告（若有）。
   */
  const save = async (input, contextDirectory) => {
    const target = await resolveTarget(input, contextDirectory);
    const title = asNonEmptyString(input.title);
    const body = asNonEmptyString(input.body);
    if (!title) fail('title is required for memory.save', 400);
    if (!body) fail('body is required for memory.save', 400);
    if (input.type !== undefined && !MEMORY_TYPES.has(input.type)) {
      fail('type must be fact, preference, or reference', 400);
    }

    const result = await agentMemoryRuntime.create(target, {
      title,
      body,
      type: input.type,
      sessionId: asNonEmptyString(input.sessionId),
    });
    announce(target.scope, target.projectId);
    // Deliberately does not echo the text back. Handing the model what it just
    // wrote invites it to find something to improve and re-save, and the store
    // is not the place to discover that a save worked — the confirmation is.
    return {
      saved: true,
      memory: toSummary(result.entry, target.scope),
      // Told plainly so the agent does not report storing a second memory when
      // it actually corrected one it had already written.
      replaced: result.replaced,
      ...(result.entry.flagged
        ? { warning: 'Stored, but held back from future sessions: this text reads as an instruction to the model rather than a fact. The user can see it in the Memory panel.' }
        : {}),
    };
  };

  /**
   * memory.delete：按 memoryId 删除指定 scope 的一条记忆。缺 id 回
   * 400，该 scope 下无此 id 回 404；成功后调用 announce。
   */
  const remove = async (input, contextDirectory) => {
    const target = await resolveTarget(input, contextDirectory);
    const memoryId = asNonEmptyString(input.memoryId);
    if (!memoryId) fail('memoryId is required for memory.delete', 400);

    const result = await agentMemoryRuntime.remove(target, memoryId);
    if (!result.deleted) {
      fail('No memory has that id in this scope', 404);
    }
    announce(target.scope, target.projectId);
    return { deleted: true, memoryId };
  };

  /**
   * 动作分发入口：先过 isAgentMemoryEnabled 闸门（工具活在 OpenCode 子
   * 进程里，开关关闭到子进程重启之间 agent 仍可能调用；设置读不出时按
   * 关闭处理），再把 memory.list/read/save/delete 分发给对应实现，未知
   * 动作回 400。
   */
  const execute = async (action, input = {}, contextDirectory) => {
    /**
     * The tool lives in the managed OpenCode child and only disappears when
     * that child restarts, so between switching memory off and restarting it
     * the agent can still call this. Ungated, those writes would land on disk
     * while the panel that shows them is hidden and the index that carries
     * them is suppressed — memory accumulating where nobody can see it.
     */
    if (typeof isAgentMemoryEnabled === 'function') {
      let enabled = false;
      try {
        enabled = await isAgentMemoryEnabled();
      } catch {
        // An unreadable setting closes the surface rather than opening it.
        enabled = false;
      }
      if (!enabled) {
        return fail('Agent memory is switched off in OMPChamber settings', 403);
      }
    }

    switch (action) {
      case 'memory.list': return list(input, contextDirectory);
      case 'memory.read': return read(input, contextDirectory);
      case 'memory.save': return save(input, contextDirectory);
      case 'memory.delete': return remove(input, contextDirectory);
      default: return fail(`Unsupported memory action: ${action || 'missing'}`, 400);
    }
  };

  return { execute };
};
