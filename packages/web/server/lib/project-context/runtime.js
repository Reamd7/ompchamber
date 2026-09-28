/**
 * Project context storage: notes, todos, and plan files.
 *
 * The server is the sole writer of `<projectsDir>/<projectId>/context.json`.
 * The sibling `<projectsDir>/<projectId>.json` stays client-owned (worktree
 * setup, draft starters, project actions) and server-owned only for
 * `version`/`scheduledTasks`; keeping the two apart is what removes the
 * cross-process read-modify-write race that a shared file would create.
 *
 * Plan bodies live as markdown at `<projectsDir>/<projectId>/plans/<file>.md`
 * and are referenced by base name only, so moving the project storage
 * directory never invalidates a reference.
 */
/**
 * （中文说明）项目上下文存储（备注、待办、计划文件）的服务端实现。
 *
 * context.json 由服务端独占写入；计划正文以 markdown 存于 plans/ 目录，
 * 清单只保存文件名，项目存储目录迁移不会导致引用失效。所有写入走
 * “临时文件 + rename”的原子写，并按 projectId 串行化，消除跨进程的
 * 读-改-写竞争（详见上方英文说明）。
 */

/** context.json 的当前版本号。 */
const PROJECT_CONTEXT_VERSION = 2;
/** 单条备注正文的最大长度（超出截断）。 */
const PROJECT_NOTE_BODY_MAX_LENGTH = 3000;
/** 单个项目允许的备注条数上限。 */
const PROJECT_NOTE_MAX_ITEMS = 200;
/** 单条待办文本的最大长度（超出截断）。 */
const PROJECT_TODO_TEXT_MAX_LENGTH = 120;
/** 计划标题的最大长度（超出截断）。 */
const PROJECT_PLAN_TITLE_MAX_LENGTH = 160;
/** 计划正文（或整份 markdown）的最大长度。 */
const PROJECT_PLAN_BODY_MAX_LENGTH = 200_000;
/** 单个项目允许的待办条数上限。 */
const PROJECT_TODO_MAX_ITEMS = 500;
/** 单个项目允许的计划条数上限。 */
const PROJECT_PLAN_MAX_ITEMS = 500;

/** 合法 projectId 字符集（字母数字与 . _ : -；不含路径分隔符，防目录穿越）。 */
const PROJECT_ID_PATTERN = /^[a-zA-Z0-9._:-]+$/;
/** 合法计划文件名模式（base name + .md，不允许任何路径分隔符）。 */
const PLAN_FILE_PATTERN = /^[a-zA-Z0-9._-]+\.md$/;

/** 值为非空（trim 后）字符串时返回 trim 结果，否则返回 null。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 把字符串截断到最大长度；非字符串输入返回空串。 */
const clampLength = (value, maxLength) => {
  if (typeof value !== 'string') return '';
  return value.length > maxLength ? value.slice(0, maxLength) : value;
};

/** 判断值是否为非数组的普通对象（逐条清洗前的形状过滤）。 */
const isObjectRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 备注来源白名单：手动输入、聊天选区蒸馏、代理写入。 */
const NOTE_SOURCES = new Set(['manual', 'selection', 'agent']);

/**
 * 清洗备注来源（origin）：必须是对象且 sessionId 非空（trim 后），
 * messageId 可选；没有 sessionId 时整体返回 null（来源被丢弃）。
 */
const sanitizeNoteOrigin = (value) => {
  if (!isObjectRecord(value)) return null;
  const sessionId = asNonEmptyString(value.sessionId);
  const messageId = asNonEmptyString(value.messageId);
  if (!sessionId) return null;
  return messageId ? { sessionId, messageId } : { sessionId };
};

/**
 * Notes are a list of entries.
 *
 * Version 1 stored a single string. It is converted here rather than in a
 * separate migration pass so that any read — including one that races another
 * writer — sees the same shape.
 */
/**
 * （中文）把任意输入清洗为合法的备注列表：逐条校验 id/body、按 id 去重、
 * 截断长度、补默认时间戳与来源，最终按 createdAt 降序，最多保留条数上限。
 * 版本 1 的“单个字符串备注”在这里就地转换成单元素列表（见上方英文说明）。
 */
const sanitizeNotes = (value, now) => {
  if (typeof value === 'string') {
    const body = clampLength(value, PROJECT_NOTE_BODY_MAX_LENGTH).trim();
    if (!body) return [];
    return [{
      id: `note_legacy_${now}`,
      body,
      createdAt: now,
      updatedAt: now,
      source: 'manual',
      pinned: false,
    }];
  }

  if (!Array.isArray(value)) return [];

  const result = [];
  const seen = new Set();
  for (const entry of value) {
    if (result.length >= PROJECT_NOTE_MAX_ITEMS) break;
    if (!isObjectRecord(entry)) continue;
    const id = asNonEmptyString(entry.id);
    const body = clampLength(typeof entry.body === 'string' ? entry.body : '', PROJECT_NOTE_BODY_MAX_LENGTH).trim();
    if (!id || !body || seen.has(id)) continue;
    seen.add(id);

    const createdAt = Number.isFinite(entry.createdAt) && entry.createdAt >= 0 ? entry.createdAt : now;
    const origin = sanitizeNoteOrigin(entry.origin);
    result.push({
      id,
      body,
      createdAt,
      updatedAt: Number.isFinite(entry.updatedAt) && entry.updatedAt >= 0 ? entry.updatedAt : createdAt,
      source: NOTE_SOURCES.has(entry.source) ? entry.source : 'manual',
      pinned: entry.pinned === true,
      ...(origin ? { origin } : {}),
    });
  }

  return result.sort((a, b) => b.createdAt - a.createdAt);
};

/**
 * 清洗待办列表：逐条校验 id/text 并去重、截断文本长度，completed 仅接受
 * true，时间戳非法时回退 now；最多保留条数上限，非数组输入返回空数组。
 */
const sanitizeTodos = (value, now) => {
  if (!Array.isArray(value)) return [];
  const result = [];
  const seen = new Set();
  for (const entry of value) {
    if (result.length >= PROJECT_TODO_MAX_ITEMS) break;
    if (!isObjectRecord(entry)) continue;
    const id = asNonEmptyString(entry.id);
    const text = clampLength(asNonEmptyString(entry.text) || '', PROJECT_TODO_TEXT_MAX_LENGTH);
    if (!id || !text || seen.has(id)) continue;
    seen.add(id);
    result.push({
      id,
      text,
      completed: entry.completed === true,
      createdAt: Number.isFinite(entry.createdAt) && entry.createdAt >= 0 ? entry.createdAt : now,
    });
  }
  return result;
};

/** 清洗计划标题：trim 后截断到上限；空值得到空串（由调用方兜底为 'Plan'）。 */
const sanitizePlanTitle = (value) => clampLength(asNonEmptyString(value) || '', PROJECT_PLAN_TITLE_MAX_LENGTH);

/**
 * 解析计划 markdown：统一换行为 \n 后，若首行是一级标题则拆出标题与正文；
 * 否则取第一个非空行（剥掉前导 #）作为标题、整篇作为正文；标题兜底 'Plan'。
 * @param {string} raw - 原始 markdown 文本。
 * @returns {{ title: string, body: string }}
 */
export const parsePlanMarkdown = (raw) => {
  const normalized = (typeof raw === 'string' ? raw : '').replace(/\r\n?/g, '\n');
  const match = normalized.match(/^\s*#\s+(.+?)\s*(?:\n+|$)/);
  if (match) {
    return {
      title: sanitizePlanTitle(match[1]) || 'Plan',
      body: normalized.slice(match[0].length).replace(/^\n+/, ''),
    };
  }
  const firstLine = normalized.split('\n').map((line) => line.trim()).find(Boolean) || 'Plan';
  return {
    title: sanitizePlanTitle(firstLine.replace(/^#+\s*/, '')) || 'Plan',
    body: normalized.trim(),
  };
};

/** 把标题与正文格式化为规范 markdown（“# 标题” + 空行 + 正文）。 */
const formatPlanMarkdown = (title, body) => {
  const normalizedTitle = sanitizePlanTitle(title) || 'Plan';
  const normalizedBody = typeof body === 'string' ? body.trim() : '';
  return normalizedBody ? `# ${normalizedTitle}\n\n${normalizedBody}` : `# ${normalizedTitle}\n`;
};

/**
 * 把标题转成文件名友好的 slug：小写、剥离 markdown 符号、空白折叠为 '-'、
 * 其余字符替换为 '-' 并压缩连续 '-'；结果为空时兜底 'plan'。
 */
const slugifyPlanTitle = (value) => {
  const normalized = value
    .trim()
    .toLowerCase()
    .replace(/[`*_#>[\](){}.!?,:;"']/g, '')
    .replace(/\s+/g, '-')
    .replace(/[^a-z0-9-]/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-+|-+$/g, '');
  return normalized || 'plan';
};

/**
 * 清洗计划清单（plans links）：id 与 file 必填且 file 必须匹配安全文件名
 * 模式；按 id 与 file 双重去重，标题清洗后兜底 'Plan'，按 createdAt 降序，
 * 最多保留条数上限。
 */
const sanitizePlanLinks = (value, now) => {
  if (!Array.isArray(value)) return [];
  const result = [];
  const seenIds = new Set();
  const seenFiles = new Set();
  for (const entry of value) {
    if (result.length >= PROJECT_PLAN_MAX_ITEMS) break;
    if (!isObjectRecord(entry)) continue;
    const id = asNonEmptyString(entry.id);
    const file = asNonEmptyString(entry.file);
    if (!id || !file || !PLAN_FILE_PATTERN.test(file)) continue;
    if (seenIds.has(id) || seenFiles.has(file)) continue;
    seenIds.add(id);
    seenFiles.add(file);
    result.push({
      id,
      file,
      title: sanitizePlanTitle(entry.title) || 'Plan',
      createdAt: Number.isFinite(entry.createdAt) && entry.createdAt >= 0 ? entry.createdAt : now,
      pinned: entry.pinned === true,
    });
  }
  return result.sort((a, b) => b.createdAt - a.createdAt);
};

/** 构造权威空上下文（version 2 + 三个空列表）。 */
const createEmptyContext = () => ({
  version: PROJECT_CONTEXT_VERSION,
  notes: [],
  todos: [],
  plans: [],
});

/**
 * 创建项目上下文运行时。
 * @param {object} deps - fsPromises、path、projectsDirPath（项目存储根目录）
 *   与可选的 createId（ID 工厂；缺省用 crypto.randomUUID，旧环境回退到
 *   时间戳 + 随机串）。
 * @returns 上下文读取与待办/备注/计划的增删改查；所有变更经 withWriteLock
 *   串行化 + 原子写，读取不持锁（见 readContext 说明）。
 */
export const createProjectContextRuntime = (deps) => {
  const { fsPromises, path, projectsDirPath, createId } = deps;

  // ID 工厂：优先用注入的 createId（测试需要确定性 ID），否则用
  // crypto.randomUUID（极旧环境回退时间戳 + 随机串）
  const idFactory = typeof createId === 'function'
    ? createId
    : () => (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
      ? crypto.randomUUID()
      : `plan_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`);

  // 每个 projectId 一条写锁（promise 链）：同项目的写操作排队串行执行
  const writeLocks = new Map();

  /** 校验 projectId：必填且只含白名单字符，否则抛错（防路径穿越）。 */
  const sanitizeProjectId = (projectId) => {
    const value = asNonEmptyString(projectId);
    if (!value) {
      throw new Error('projectId is required');
    }
    if (!PROJECT_ID_PATTERN.test(value)) {
      throw new Error('projectId contains unsupported characters');
    }
    return value;
  };

  /** 项目存储子目录 <projectsDir>/<projectId> 的完整路径。 */
  const storageDirFor = (projectId) => path.join(projectsDirPath, sanitizeProjectId(projectId));
  /** 项目上下文文件 context.json 的完整路径。 */
  const contextPathFor = (projectId) => path.join(storageDirFor(projectId), 'context.json');
  /** 项目计划 markdown 目录（<projectId>/plans）的完整路径。 */
  const plansDirFor = (projectId) => path.join(storageDirFor(projectId), 'plans');
  /** 旧版客户端自有配置 <projectId>.json 的路径（迁移的数据源）。 */
  const legacyConfigPathFor = (projectId) => path.join(projectsDirPath, `${sanitizeProjectId(projectId)}.json`);

  /**
   * 读取并解析 JSON 文件。文件不存在返回 { missing: true, value: null }；
   * 内容不是普通对象（含 JSON 解析失败）返回 { missing: false, value: null }；
   * 其它 I/O 错误原样抛出。注意 missing（缺失）与 value 为 null（损坏）
   * 是调用方必须区分的两个状态。
   */
  const readJson = async (filePath) => {
    let raw;
    try {
      raw = await fsPromises.readFile(filePath, 'utf8');
    } catch (error) {
      if (error && error.code === 'ENOENT') return { missing: true, value: null };
      throw error;
    }
    try {
      const parsed = JSON.parse(raw);
      return { missing: false, value: isObjectRecord(parsed) ? parsed : null };
    } catch {
      return { missing: false, value: null };
    }
  };

  /**
   * 原子写 JSON：先写同目录临时文件（文件名带 pid/时间戳/随机串避免并发
   * 冲突），再 rename 覆盖目标；任何失败都尽力删除临时文件后抛出错误。
   */
  const writeJsonAtomic = async (filePath, value) => {
    const temporaryPath = `${filePath}.tmp-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    await fsPromises.mkdir(path.dirname(filePath), { recursive: true });
    try {
      await fsPromises.writeFile(temporaryPath, JSON.stringify(value, null, 2), 'utf8');
      await fsPromises.rename(temporaryPath, filePath);
    } catch (error) {
      await fsPromises.rm(temporaryPath, { force: true }).catch(() => {});
      throw error;
    }
  };

  /**
   * 按 projectId 串行执行 mutate：等待链尾的前一个写完成后执行，结束后
   * 放行下一个写；若自己仍是链尾则从 writeLocks 删除，防止映射无限增长。
   * @param {Function} mutate - 实际的读-改-写操作（通常内部先 readContext）。
   */
  const withWriteLock = async (projectId, mutate) => {
    const key = sanitizeProjectId(projectId);
    const previous = writeLocks.get(key) || Promise.resolve();
    let release;
    const next = new Promise((resolve) => { release = resolve; });
    const chained = previous.finally(() => next);
    writeLocks.set(key, chained);

    await previous;
    try {
      return await mutate();
    } finally {
      release();
      if (writeLocks.get(key) === chained) {
        writeLocks.delete(key);
      }
    }
  };

  /**
   * One-time migration of `projectNotes` / `projectTodos` / `projectPlanFiles`
   * out of the client-owned `<projectId>.json`.
   *
   * Plan links carried absolute paths; those are converted to base names. A
   * referenced file that is not already inside the plans directory is moved
   * there so a stale absolute path from an earlier project id is recovered
   * rather than dropped. A link whose file cannot be located at all is kept
   * out of the result — the markdown is gone, so the link is dead either way.
   *
   * The legacy keys are removed only after `context.json` is durably written.
   * A failure at any point leaves the legacy keys in place, so the migration
   * simply runs again on the next read.
   */
  /**
   * （中文）一次性迁移：把客户端自有配置里的 projectNotes / projectTodos /
   * projectPlanFiles 搬进 context.json。计划链接携带的绝对路径转为文件名；
   * 文件尚不在 plans 目录时从记录路径找回并搬运过去；markdown 彻底找不到
   * 的死链直接丢弃。只有 context.json 落盘成功后才删除旧键，中途失败则
   * 下次读取会重新执行迁移（详见上方英文说明）。
   * @returns {object|null} 迁移结果；无旧键或旧文件不可读时返回 null。
   */
  const migrateFromLegacyConfig = async (projectId, now) => {
    const legacyPath = legacyConfigPathFor(projectId);
    const legacy = await readJson(legacyPath);
    if (!legacy.value) {
      return null;
    }

    const hasLegacyKeys = legacy.value.projectNotes !== undefined
      || legacy.value.projectTodos !== undefined
      || legacy.value.projectPlanFiles !== undefined;
    if (!hasLegacyKeys) {
      return null;
    }

    const plansDir = plansDirFor(projectId);
    const links = [];
    const rawLinks = Array.isArray(legacy.value.projectPlanFiles) ? legacy.value.projectPlanFiles : [];
    for (const entry of rawLinks) {
      if (!isObjectRecord(entry)) continue;
      const id = asNonEmptyString(entry.id);
      const absolutePath = asNonEmptyString(entry.path);
      if (!id || !absolutePath) continue;

      const file = path.basename(absolutePath);
      if (!PLAN_FILE_PATTERN.test(file)) continue;
      const targetPath = path.join(plansDir, file);

      let raw = null;
      try {
        raw = await fsPromises.readFile(targetPath, 'utf8');
      } catch (error) {
        if (!error || error.code !== 'ENOENT') throw error;
        // Not in the plans directory yet — recover it from the recorded path.
        try {
          raw = await fsPromises.readFile(absolutePath, 'utf8');
        } catch (recoverError) {
          if (!recoverError || recoverError.code !== 'ENOENT') throw recoverError;
          continue;
        }
        await fsPromises.mkdir(plansDir, { recursive: true });
        await fsPromises.writeFile(targetPath, raw, 'utf8');
      }

      links.push({
        id,
        file,
        title: parsePlanMarkdown(raw).title,
        createdAt: Number.isFinite(entry.createdAt) && entry.createdAt >= 0 ? entry.createdAt : now,
      });
    }

    const migrated = {
      version: PROJECT_CONTEXT_VERSION,
      notes: sanitizeNotes(legacy.value.projectNotes, now),
      todos: sanitizeTodos(legacy.value.projectTodos, now),
      plans: sanitizePlanLinks(links, now),
    };

    await writeJsonAtomic(contextPathFor(projectId), migrated);

    const remaining = { ...legacy.value };
    delete remaining.projectNotes;
    delete remaining.projectTodos;
    delete remaining.projectPlanFiles;
    await writeJsonAtomic(legacyPath, remaining);

    return migrated;
  };

  /**
   * Read the stored context.
   *
   * Distinguishes the three states the caller must not conflate: a missing
   * file is authoritative empty, malformed JSON is a failure, and an I/O
   * error propagates. Never returns an empty context to paper over a read
   * that did not succeed.
   *
   * Deliberately does NOT take the write lock: every mutator calls this while
   * already holding it, so locking here would deadlock. The legacy migration
   * it can trigger is safe unlocked — both of its writes are atomic renames
   * of identical content, so concurrent migrations converge instead of
   * interleaving.
   */
  /**
   * （中文）读取存储的上下文：文件缺失是“权威空”（此时尝试一次旧格式
   * 迁移），JSON 损坏是失败（抛错而不是返回空），字段逐层清洗后返回。
   * 刻意不取写锁，避免与持锁调用它的各变更方法死锁（详见上方英文说明）。
   * @returns {{ version, notes, todos, plans }}
   */
  const readContext = async (projectId) => {
    const now = Date.now();
    const stored = await readJson(contextPathFor(projectId));

    if (!stored.missing && !stored.value) {
      throw new Error('Stored project context is malformed');
    }

    if (stored.missing) {
      const migrated = await migrateFromLegacyConfig(projectId, now);
      if (migrated) {
        return {
          version: PROJECT_CONTEXT_VERSION,
          notes: sanitizeNotes(migrated.notes, now),
          todos: sanitizeTodos(migrated.todos, now),
          plans: sanitizePlanLinks(migrated.plans, now),
        };
      }
      return createEmptyContext();
    }

    return {
      version: PROJECT_CONTEXT_VERSION,
      notes: sanitizeNotes(stored.value.notes, now),
      todos: sanitizeTodos(stored.value.todos, now),
      plans: sanitizePlanLinks(stored.value.plans, now),
    };
  };

  /** 原子写入完整上下文（固定 version 2 + 备注列表 + 待办列表 + 计划清单）。 */
  const writeContext = async (projectId, context) => {
    await writeJsonAtomic(contextPathFor(projectId), {
      version: PROJECT_CONTEXT_VERSION,
      notes: context.notes,
      todos: context.todos,
      plans: context.plans,
    });
  };

  /**
   * 整体替换待办列表（持锁读-改-写），备注与计划不受影响。
   * @param {Array} todos - 新的待办数组（写入前经 sanitizeTodos 清洗）。
   * @returns 写入后的完整上下文。
   */
  const saveTodos = async (projectId, todos) => {
    return withWriteLock(projectId, async () => {
      const now = Date.now();
      const current = await readContext(projectId);
      const next = { ...current, todos: sanitizeTodos(todos, now) };
      await writeContext(projectId, next);
      return next;
    });
  };

  /**
   * Notes are addressed individually.
   *
   * Splitting them from todos is what lets the panel stop writing both fields
   * on every keystroke-driven save: a todo toggle can no longer clobber notes
   * the user is still typing, and an agent-authored note can no longer lose a
   * concurrent todo change.
   */
  /**
   * （中文）新增一条备注：body 必填（trim 后非空）并截断到上限，条数达到
   * 上限时抛错；新备注前插到列表头部，随完整上下文一起返回
   * （与待办分开寻址的理由见上方英文说明）。
   * @returns {{ note: object, context: object }}
   */
  const createNote = async (projectId, value) => {
    const body = clampLength(typeof value?.body === 'string' ? value.body : '', PROJECT_NOTE_BODY_MAX_LENGTH).trim();
    if (!body) {
      throw new Error('body is required');
    }

    return withWriteLock(projectId, async () => {
      const now = Date.now();
      const current = await readContext(projectId);
      if (current.notes.length >= PROJECT_NOTE_MAX_ITEMS) {
        throw new Error(`A project can hold at most ${PROJECT_NOTE_MAX_ITEMS} notes`);
      }

      const note = {
        id: idFactory(),
        body,
        createdAt: now,
        updatedAt: now,
        source: NOTE_SOURCES.has(value?.source) ? value.source : 'manual',
        pinned: false,
        ...(sanitizeNoteOrigin(value?.origin) ? { origin: sanitizeNoteOrigin(value.origin) } : {}),
      };

      const next = { ...current, notes: [note, ...current.notes] };
      await writeContext(projectId, next);
      return { note, context: next };
    });
  };

  /**
   * Patch one note. Omitted fields are left alone, so pinning a note cannot
   * roll back an edit that landed between the two requests.
   */
  /**
   * （中文）补丁式更新单条备注：只改传入的字段（body 更新时同时刷新
   * updatedAt），避免覆盖两次请求之间落地的其它编辑；body 与 pinned 至少
   * 传一个，备注不存在返回 null（详见上方英文说明）。
   * @returns {{ note: object, context: object } | null}
   */
  const updateNote = async (projectId, noteId, patch) => {
    const id = asNonEmptyString(noteId);
    if (!id) {
      throw new Error('noteId is required');
    }
    const hasBody = typeof patch?.body === 'string';
    const hasPinned = typeof patch?.pinned === 'boolean';
    if (!hasBody && !hasPinned) {
      throw new Error('body or pinned is required');
    }
    const body = hasBody ? clampLength(patch.body, PROJECT_NOTE_BODY_MAX_LENGTH).trim() : null;
    if (hasBody && !body) {
      throw new Error('body is required');
    }

    return withWriteLock(projectId, async () => {
      const now = Date.now();
      const current = await readContext(projectId);
      const existing = current.notes.find((note) => note.id === id);
      if (!existing) {
        return null;
      }

      const note = {
        ...existing,
        ...(hasBody ? { body, updatedAt: now } : {}),
        ...(hasPinned ? { pinned: patch.pinned } : {}),
      };
      const next = { ...current, notes: current.notes.map((entry) => (entry.id === id ? note : entry)) };
      await writeContext(projectId, next);
      return { note, context: next };
    });
  };

  /**
   * 删除单条备注：备注不存在时返回 { deleted: false } 且不写盘。
   * @returns {{ deleted: boolean, context: object }}
   */
  const deleteNote = async (projectId, noteId) => {
    const id = asNonEmptyString(noteId);
    if (!id) {
      throw new Error('noteId is required');
    }

    return withWriteLock(projectId, async () => {
      const current = await readContext(projectId);
      if (!current.notes.some((note) => note.id === id)) {
        return { deleted: false, context: current };
      }
      const next = { ...current, notes: current.notes.filter((note) => note.id !== id) };
      await writeContext(projectId, next);
      return { deleted: true, context: next };
    });
  };

  /**
   * 读取单个计划：先在清单中定位文件名，再读取并解析 markdown；计划不
   * 存在或 markdown 文件已被删除都返回 null，其它 I/O 错误原样抛出。
   * @returns {{ id, file, createdAt, title, body, raw } | null}
   */
  const readPlan = async (projectId, planId) => {
    const id = asNonEmptyString(planId);
    if (!id) {
      throw new Error('planId is required');
    }
    const context = await readContext(projectId);
    const link = context.plans.find((entry) => entry.id === id);
    if (!link) {
      return null;
    }

    let raw;
    try {
      raw = await fsPromises.readFile(path.join(plansDirFor(projectId), link.file), 'utf8');
    } catch (error) {
      if (error && error.code === 'ENOENT') return null;
      throw error;
    }

    const parsed = parsePlanMarkdown(raw);
    return { id: link.id, file: link.file, createdAt: link.createdAt, title: parsed.title, body: parsed.body, raw };
  };

  /**
   * Overwrite a plan's markdown in place.
   *
   * Takes the whole raw document, because the editor surface owns the file
   * verbatim — round-tripping through title + body would rewrite the heading
   * and silently reformat what the user typed. The manifest title is
   * re-derived from the saved content so the list never drifts from the file.
   *
   * The file name is deliberately not regenerated on a title change: it is the
   * stable identity behind the link, and renaming it would strand the markdown
   * if the manifest write failed afterwards.
   */
  /**
   * （中文）整份覆写计划的 markdown：编辑器持有文件原文，因此按 raw 整体
   * 写入而不是 title+body 重组；标题从新内容重新推导以免清单漂移；文件
   * 名不随标题变化（清单身份稳定），底层文件已被删除时拒绝复活并返回
   * null（详见上方英文说明）。
   * @returns {{ plan, context, title, body, raw } | null}
   */
  const updatePlan = async (projectId, planId, value) => {
    const id = asNonEmptyString(planId);
    if (!id) {
      throw new Error('planId is required');
    }
    if (typeof value?.raw !== 'string') {
      throw new Error('raw is required');
    }
    const raw = clampLength(value.raw, PROJECT_PLAN_BODY_MAX_LENGTH);

    return withWriteLock(projectId, async () => {
      const current = await readContext(projectId);
      const link = current.plans.find((entry) => entry.id === id);
      if (!link) {
        return null;
      }

      const filePath = path.join(plansDirFor(projectId), link.file);
      // Refuse to recreate a file that was deleted underneath us: the link is
      // already dead, and writing here would resurrect it with editor content
      // the user believed was discarded.
      try {
        await fsPromises.access(filePath);
      } catch (error) {
        if (error && error.code === 'ENOENT') return null;
        throw error;
      }

      await fsPromises.writeFile(filePath, raw, 'utf8');

      const parsed = parsePlanMarkdown(raw);
      const nextLink = { ...link, title: parsed.title };
      const next = {
        ...current,
        plans: current.plans.map((entry) => (entry.id === id ? nextLink : entry)),
      };
      await writeContext(projectId, next);

      return { plan: nextLink, context: next, title: parsed.title, body: parsed.body, raw };
    });
  };

  /**
   * Create a plan from title + body.
   *
   * The markdown file is written before the manifest entry. A failure after
   * the file write leaves an unreferenced markdown file rather than a
   * manifest entry pointing at nothing — the orphan is inert, a dangling
   * entry would surface as a broken row in the UI.
   */
  /**
   * （中文）从标题 + 正文创建计划：先写 markdown 文件再写清单条目，失败时
   * 最多留下一个无引用的孤儿文件而不是指向空处的死链；同毫秒同名冲突用
   * 序号后缀规避（详见上方英文说明）。
   * @returns {{ plan: object, context: object }}
   */
  const createPlan = async (projectId, value) => {
    const title = sanitizePlanTitle(value?.title) || 'Plan';
    const body = clampLength(typeof value?.body === 'string' ? value.body : '', PROJECT_PLAN_BODY_MAX_LENGTH);

    return withWriteLock(projectId, async () => {
      const current = await readContext(projectId);
      const createdAt = Date.now();
      const plansDir = plansDirFor(projectId);
      await fsPromises.mkdir(plansDir, { recursive: true });

      const baseName = `${createdAt}-${slugifyPlanTitle(title)}`;
      let file = `${baseName}.md`;
      let attempt = 1;
      while (current.plans.some((entry) => entry.file === file)) {
        file = `${baseName}-${attempt}.md`;
        attempt += 1;
      }

      await fsPromises.writeFile(path.join(plansDir, file), formatPlanMarkdown(title, body), 'utf8');

      const link = { id: idFactory(), file, title, createdAt, pinned: false };
      const next = { ...current, plans: [link, ...current.plans] };
      await writeContext(projectId, next);
      return { plan: link, context: next };
    });
  };

  /**
   * Delete a plan.
   *
   * The manifest entry is removed first so a failed file unlink cannot leave
   * the UI showing a plan that no longer opens. The leftover markdown is
   * unreferenced and harmless.
   */
  /** Pin state is patched on its own so it cannot roll back a concurrent edit. */
  /**
   * （中文）单独补丁计划的置顶状态：只改 pinned 字段，不触碰标题与文件，
   * 计划不存在返回 null。
   * @returns {{ plan: object, context: object } | null}
   */
  const setPlanPinned = async (projectId, planId, pinned) => {
    const id = asNonEmptyString(planId);
    if (!id) {
      throw new Error('planId is required');
    }

    return withWriteLock(projectId, async () => {
      const current = await readContext(projectId);
      const existing = current.plans.find((entry) => entry.id === id);
      if (!existing) {
        return null;
      }
      const plan = { ...existing, pinned: pinned === true };
      const next = { ...current, plans: current.plans.map((entry) => (entry.id === id ? plan : entry)) };
      await writeContext(projectId, next);
      return { plan, context: next };
    });
  };

  /**
   * 删除计划：先移除清单条目（UI 立即不可见），再删除 markdown 文件；
   * 计划不存在时返回 { deleted: false } 且不写盘。残留的 markdown 无害
   * （顺序保证见上方英文说明）。
   * @returns {{ deleted: boolean, context: object }}
   */
  const deletePlan = async (projectId, planId) => {
    const id = asNonEmptyString(planId);
    if (!id) {
      throw new Error('planId is required');
    }

    return withWriteLock(projectId, async () => {
      const current = await readContext(projectId);
      const link = current.plans.find((entry) => entry.id === id);
      if (!link) {
        return { deleted: false, context: current };
      }

      const next = { ...current, plans: current.plans.filter((entry) => entry.id !== id) };
      await writeContext(projectId, next);
      await fsPromises.rm(path.join(plansDirFor(projectId), link.file), { force: true });
      return { deleted: true, context: next };
    });
  };

  // 导出：上下文读取、待办整存、备注与计划的增删改查、两个路径工具
  return {
    readContext,
    saveTodos,
    createNote,
    updateNote,
    deleteNote,
    readPlan,
    updatePlan,
    createPlan,
    setPlanPinned,
    deletePlan,
    contextPathFor,
    plansDirFor,
  };
};
