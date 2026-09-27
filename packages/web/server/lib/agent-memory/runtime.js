/**
 * Agent memory storage.
 *
 * What the agent has learned and chose to keep, in two scopes:
 *
 * - **project** — `<projectsDir>/<projectId>/memory.json`. How this codebase
 *   works, what was decided, where things live.
 * - **global** — `<userConfigRoot>/memory.json`. Who the user is and how they
 *   want to be worked with. It belongs to no project, so it cannot live under
 *   one.
 *
 * The split is not cosmetic. A wrong project fact costs one project and is
 * noticed quickly; a wrong global fact quietly shapes every session in every
 * project, and the user has no code to check it against. Global memory is
 * therefore deliberately narrower: fewer entries, and only the types that
 * genuinely have no other home.
 *
 * This is NOT the notes surface. Notes are what the user writes for themselves
 * and hands to the agent by pinning; memory is what the agent writes for
 * itself. Keeping them apart keeps an agent mistake out of the user's notes.
 *
 * Because the agent writes here unprompted, two invariants guard the store:
 *
 * - **Restatements replace.** A memory the agent phrases differently the second
 *   time supersedes the first rather than sitting beside it, so the store
 *   cannot fill with variants of one fact that later disagree.
 * - **Timestamps are the record of change.** The panel derives "new" and
 *   "changed" from `createdAt` and `updatedAt` against when the user last
 *   looked, so what the agent stored without asking stays visible without the
 *   store carrying any review state of its own.
 */
/**
 * agent 记忆存储运行时。project 与 global 两个作用域各自对应一个
 * memory.json；写入走"复述即替换"的去重合并（标题精确匹配或词元重叠达标即视为
 * 同一条），有容量上限，按 target key 串行加写锁并原子落盘；读取时逐条清洗并
 * 重新做威胁模式扫描，损坏的文件显式报错而非当作空存储，防止 agent 误覆写。
 */

/** memory.json 的结构版本号；读取时统一按此版本重建输出。 */
const MEMORY_VERSION = 1;

/**
 * Titles are what every session carries, so their combined length is the
 * standing cost of memory. Short enough to keep a full store's index modest,
 * long enough to say what an entry is about.
 */
/** 单条记忆标题的最大长度；标题随每个会话加载，是记忆的常驻成本。 */
const MEMORY_TITLE_MAX_LENGTH = 60;
/** 单条记忆正文的最大长度，超长直接截断。 */
const MEMORY_BODY_MAX_LENGTH = 2000;

/** Global memory stays small on purpose: it is the highest-blast-radius store. */
/** global 作用域的条目上限（影响面最大，刻意保持小）。 */
const GLOBAL_MEMORY_MAX_ITEMS = 60;
/** project 作用域的条目上限。 */
const PROJECT_MEMORY_MAX_ITEMS = 200;

/**
 * `fact` — something true about the project or the user.
 * `preference` — how the user wants work done.
 * `reference` — a pointer to a resource that is hard to rediscover.
 */
/** 允许的记忆类型集合；非法类型在清洗与写入时回退为 'fact'。 */
const MEMORY_TYPES = new Set(['fact', 'preference', 'reference']);

import { findThreatPattern } from './threat-patterns.js';

/** projectId 的合法字符白名单（字母数字与 . _ : -），不匹配即拒绝，防止路径穿越。 */
const PROJECT_ID_PATTERN = /^[a-zA-Z0-9._:-]+$/;

/**
 * Two entries are the same memory when this much of the incoming one is already
 * in the stored one. Set high on purpose: merging two genuinely different
 * memories destroys one of them silently, which is far worse than keeping a
 * near-duplicate the user can see and delete.
 */
/** 判定为"同一条记忆"的词元重叠率阈值（0.75）。 */
const DUPLICATE_OVERLAP_THRESHOLD = 0.75;

/**
 * Below this many meaningful words, overlap is noise — "use bun" and "use npm"
 * share half their tokens. Short entries fall back to exact-title matching.
 */
/** 触发重叠比对的最低词元数；低于该值只用标题精确匹配。 */
const DUPLICATE_MIN_TOKENS = 4;

/**
 * Words carried by almost every sentence, so their overlap says nothing about
 * whether two memories mean the same thing.
 */
/** 停用词表：几乎每句话都有的词，其重叠不构成"同一条记忆"的证据。 */
const STOP_WORDS = new Set([
  'a', 'an', 'and', 'are', 'as', 'at', 'be', 'but', 'by', 'for', 'from', 'has',
  'have', 'in', 'into', 'is', 'it', 'its', 'not', 'of', 'on', 'or', 'that',
  'the', 'their', 'them', 'they', 'this', 'to', 'was', 'were', 'when', 'with',
]);

/** 分词：转小写、按非字母数字字符切分，丢弃长度小于 3 与停用词，返回去重 Set。 */
const tokenize = (value) => {
  const tokens = new Set();
  for (const raw of String(value).toLowerCase().split(/[^\p{L}\p{N}]+/u)) {
    if (raw.length < 3 || STOP_WORDS.has(raw)) continue;
    tokens.add(raw);
  }
  return tokens;
};

/** How much of `incoming` is already present in `existing`, in `[0, 1]`. */
/** incoming 词元出现在 existing 中的占比，取值 [0,1]；incoming 为空时为 0。 */
const overlapFraction = (incoming, existing) => {
  if (incoming.size === 0) return 0;
  let shared = 0;
  for (const token of incoming) {
    if (existing.has(token)) shared += 1;
  }
  return shared / incoming.size;
};

/**
 * The stored entry a new one should replace, or null for a genuinely new
 * memory.
 *
 * Exact title match alone is not enough: an agent that re-learns the same fact
 * phrases it differently each time ("run UI tests per file" / "UI tests must be
 * run one file at a time"), and storing both leaves the two free to drift apart
 * until they contradict each other. Comparing the wording catches the restated
 * duplicate that the title check misses.
 */
/** 找出应被新记忆替换的既有条目：先按标题精确匹配，再按词元重叠率（达阈值）择优；都不是则 null。 */
const findSupersededEntry = (entries, title, body) => {
  const lowerTitle = title.toLowerCase();
  const exact = entries.find((entry) => entry.title.toLowerCase() === lowerTitle);
  if (exact) return exact;

  const incoming = tokenize(`${title} ${body}`);
  if (incoming.size < DUPLICATE_MIN_TOKENS) return null;

  let best = null;
  let bestScore = 0;
  for (const entry of entries) {
    const score = overlapFraction(incoming, tokenize(`${entry.title} ${entry.body}`));
    if (score >= DUPLICATE_OVERLAP_THRESHOLD && score > bestScore) {
      best = entry;
      bestScore = score;
    }
  }
  return best;
};

/** 校验非空字符串：trim 后非空返回 trim 结果，否则返回 null。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 字符串超过 maxLength 时截断；非字符串返回空串。 */
const clampLength = (value, maxLength) => {
  if (typeof value !== 'string') return '';
  return value.length > maxLength ? value.slice(0, maxLength) : value;
};

/** 判断是否为非数组的纯对象（JSON 解析结果的合法容器）。 */
const isObjectRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 按作用域取条目上限：global 为 60，project 为 200。 */
const limitForScope = (scope) => (scope === 'global' ? GLOBAL_MEMORY_MAX_ITEMS : PROJECT_MEMORY_MAX_ITEMS);

/**
 * 清洗从磁盘读出的条目数组：丢弃非对象、缺 id/标题/正文或 id 重复的条目，并按
 * 作用域上限截断；标题/正文截断到上限，非法 type 回退 'fact'，createdAt/
 * updatedAt 缺失或非法时分别以 now、createdAt 兜底；每条都重新执行
 * findThreatPattern 决定 flagged（不信任文件中的旧标记）。结果按 updatedAt 降序。
 */
const sanitizeEntries = (value, now, scope) => {
  if (!Array.isArray(value)) return [];

  const result = [];
  const seen = new Set();
  for (const entry of value) {
    if (result.length >= limitForScope(scope)) break;
    if (!isObjectRecord(entry)) continue;

    const id = asNonEmptyString(entry.id);
    const title = clampLength(asNonEmptyString(entry.title) || '', MEMORY_TITLE_MAX_LENGTH);
    const body = clampLength(typeof entry.body === 'string' ? entry.body : '', MEMORY_BODY_MAX_LENGTH).trim();
    if (!id || !title || !body || seen.has(id)) continue;
    seen.add(id);

    const createdAt = Number.isFinite(entry.createdAt) && entry.createdAt >= 0 ? entry.createdAt : now;
    const sessionId = asNonEmptyString(entry.sessionId);
    result.push({
      id,
      title,
      body,
      type: MEMORY_TYPES.has(entry.type) ? entry.type : 'fact',
      createdAt,
      updatedAt: Number.isFinite(entry.updatedAt) && entry.updatedAt >= 0 ? entry.updatedAt : createdAt,
      // Re-checked on every read, not trusted from the file: an entry written
      // before a pattern existed, or edited on disk since, is judged now.
      ...(findThreatPattern(`${title}\n${body}`) ? { flagged: true } : {}),
      ...(sessionId ? { sessionId } : {}),
    });
  }

  return result.sort((a, b) => b.updatedAt - a.updatedAt);
};

/** 构造空存储结构：当前版本号加空条目列表。 */
const createEmptyMemory = () => ({ version: MEMORY_VERSION, entries: [] });

/**
 * 创建记忆运行时实例。依赖（fsPromises/path/projectsDirPath/userConfigRoot/
 * createId）全部注入，便于测试替换。返回 { read, readAll, create, update,
 * remove, resolveTarget }；所有写操作经 withWriteLock 按 target key 串行，
 * 落盘统一走 writeJsonAtomic 原子替换。
 */
export const createAgentMemoryRuntime = (deps) => {
  const { fsPromises, path, projectsDirPath, userConfigRoot, createId } = deps;

  /** 条目 ID 生成器：优先注入的 createId，退到 crypto.randomUUID，再退到时间戳加随机串。 */
  const idFactory = typeof createId === 'function'
    ? createId
    : () => (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
      ? crypto.randomUUID()
      : `mem_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`);

  /** 按 target key（'global' 或 'project:<id>'）串行化写操作的 Promise 链表。 */
  const writeLocks = new Map();

  /** 校验 projectId：必须非空且仅含白名单字符，否则抛错；通过后原样返回。 */
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

  /** `target` is `{ scope: 'global' }` or `{ scope: 'project', projectId }`. */
  /** 把 target 描述解析为 { scope, key, filePath }；scope 缺失或非法时抛错。 */
  const resolveTarget = (target) => {
    if (target?.scope === 'global') {
      return { scope: 'global', key: 'global', filePath: path.join(userConfigRoot, 'memory.json') };
    }
    if (target?.scope === 'project') {
      const projectId = sanitizeProjectId(target.projectId);
      return {
        scope: 'project',
        key: `project:${projectId}`,
        filePath: path.join(projectsDirPath, projectId, 'memory.json'),
      };
    }
    throw new Error('scope is required');
  };

  /** 读 JSON 文件：ENOENT 视为缺失（missing:true）；能读到但解析失败或非对象按 value:null 返回；其它 IO 错误上抛。 */
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

  /** 原子写：先写同目录临时文件再 rename 覆盖目标；失败时清理临时文件后重抛原错误。 */
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
   * 以 key 为粒度的写锁：把 mutate 排到该 key 的 Promise 链末尾，保证同一存储的
   * 读-改-写互不交错；结束后若链尾仍是自己则删除锁条目，避免 Map 无限增长。
   */
  const withWriteLock = async (key, mutate) => {
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
   * Missing is authoritative empty; malformed is a failure. An agent that reads
   * "no memory" from a corrupt file would cheerfully rewrite everything it
   * thought it had lost.
   */
  /** 读取一个作用域的存储：文件缺失返回空存储；文件存在但损坏（解析失败/非对象）抛"malformed"错误，防止把坏文件当空存储覆写。条目逐条 sanitize。 */
  const read = async (target) => {
    const resolved = resolveTarget(target);
    const stored = await readJson(resolved.filePath);

    if (!stored.missing && !stored.value) {
      throw new Error('Stored agent memory is malformed');
    }
    if (stored.missing) {
      return createEmptyMemory();
    }

    return {
      version: MEMORY_VERSION,
      entries: sanitizeEntries(stored.value.entries, Date.now(), resolved.scope),
    };
  };

  /** 把条目数组以当前版本号原子写入 resolved.filePath。 */
  const write = async (resolved, entries) => {
    await writeJsonAtomic(resolved.filePath, { version: MEMORY_VERSION, entries });
  };

  /**
   * 新增一条记忆。title/body 必填（截断到上限）；若命中既有条目（标题精确或词元
   * 重叠达标）则原位替换并刷新 updatedAt（返回 replaced:true）——替换不受容量
   * 限制，满存储仍可自我修正。真正新增时受作用域上限约束，超限抛出附带全部现有
   * 标题、指示先合并/删除再重试的错误。
   */
  const create = async (target, value) => {
    const resolved = resolveTarget(target);
    const title = clampLength(asNonEmptyString(value?.title) || '', MEMORY_TITLE_MAX_LENGTH);
    const body = clampLength(typeof value?.body === 'string' ? value.body : '', MEMORY_BODY_MAX_LENGTH).trim();
    if (!title) throw new Error('title is required');
    if (!body) throw new Error('body is required');

    return withWriteLock(resolved.key, async () => {
      const now = Date.now();
      const current = await read(target);

      // A restatement of something already stored is an update, not a second
      // copy: an agent re-learning a fact each session would otherwise fill the
      // store with near-duplicates and contradict itself.
      //
      // Checked before the capacity limit, because replacing an entry does not
      // grow the store — a full store must still be able to correct itself.
      const existing = findSupersededEntry(current.entries, title, body);
      if (existing) {
        const updated = {
          ...existing,
          title,
          body,
          updatedAt: now,
          ...(MEMORY_TYPES.has(value?.type) ? { type: value.type } : {}),
        };
        const entries = current.entries.map((entry) => (entry.id === existing.id ? updated : entry));
        await write(resolved, entries);
        return { entry: updated, entries, replaced: true };
      }

      const limit = limitForScope(resolved.scope);
      if (current.entries.length >= limit) {
        // Handed its own titles and told what to do with them. A bare "full"
        // leaves the agent with a dead end, when the useful move — merge the
        // overlapping entries, drop the stale ones, then retry — is something
        // only it can judge.
        const titles = current.entries.map((entry) => `- ${entry.title}`).join('\n');
        throw new Error(
          `${resolved.scope} memory is full (${current.entries.length}/${limit} entries). `
          + 'Consolidate before saving anything else: merge overlapping entries by saving one '
          + 'under an existing title, and delete what is stale or wrong. Then retry this save, '
          + `all in this turn. Current entries:\n${titles}`,
        );
      }

      const sessionId = asNonEmptyString(value?.sessionId);
      const entry = {
        id: idFactory(),
        title,
        body,
        type: MEMORY_TYPES.has(value?.type) ? value.type : 'fact',
        createdAt: now,
        updatedAt: now,
        ...(findThreatPattern(`${title}\n${body}`) ? { flagged: true } : {}),
        ...(sessionId ? { sessionId } : {}),
      };
      const entries = [entry, ...current.entries];
      await write(resolved, entries);
      return { entry, entries, replaced: false };
    });
  };

  /**
   * A user correction. The agent rewrites by saving the same memory again, so
   * this exists for the panel: a memory worded badly enough to mislead should
   * be fixable where it is read, not only deletable.
   */
  /** 面板用的局部修改：按 id 定位条目，patch 至少含 title/body/type 之一（空串视为非法），命中则更新 updatedAt；条目不存在返回 null。 */
  const update = async (target, memoryId, patch) => {
    const resolved = resolveTarget(target);
    const id = asNonEmptyString(memoryId);
    if (!id) throw new Error('memoryId is required');

    const hasTitle = typeof patch?.title === 'string';
    const hasBody = typeof patch?.body === 'string';
    const hasType = MEMORY_TYPES.has(patch?.type);
    if (!hasTitle && !hasBody && !hasType) {
      throw new Error('title, body or type is required');
    }
    const title = hasTitle ? clampLength(patch.title, MEMORY_TITLE_MAX_LENGTH).trim() : null;
    const body = hasBody ? clampLength(patch.body, MEMORY_BODY_MAX_LENGTH).trim() : null;
    if (hasTitle && !title) throw new Error('title is required');
    if (hasBody && !body) throw new Error('body is required');

    return withWriteLock(resolved.key, async () => {
      const current = await read(target);
      const existing = current.entries.find((entry) => entry.id === id);
      if (!existing) {
        return null;
      }

      const updated = {
        ...existing,
        ...(hasTitle ? { title } : {}),
        ...(hasBody ? { body } : {}),
        ...(hasType ? { type: patch.type } : {}),
        updatedAt: Date.now(),
      };
      const entries = current.entries.map((entry) => (entry.id === id ? updated : entry));
      await write(resolved, entries);
      return { entry: updated, entries };
    });
  };

  /** 按 id 删除条目：存在则过滤后落盘并返回 deleted:true，不存在返回 deleted:false（均不抛错）。 */
  const remove = async (target, memoryId) => {
    const resolved = resolveTarget(target);
    const id = asNonEmptyString(memoryId);
    if (!id) throw new Error('memoryId is required');

    return withWriteLock(resolved.key, async () => {
      const current = await read(target);
      if (!current.entries.some((entry) => entry.id === id)) {
        return { deleted: false, entries: current.entries };
      }
      const entries = current.entries.filter((entry) => entry.id !== id);
      await write(resolved, entries);
      return { deleted: true, entries };
    });
  };

  /**
   * Both scopes at once, for the session index. A failure in one scope must not
   * hide the other: losing the project half should not also erase what the
   * agent knows about the user.
   */
  /** 一次读 global 与（可选）project 两个作用域：allSettled 隔离失败，单侧损坏仅置对应 *Failed 标记并返回空数组，不影响另一侧。 */
  const readAll = async (projectId) => {
    const settled = await Promise.allSettled([
      read({ scope: 'global' }),
      projectId ? read({ scope: 'project', projectId }) : Promise.resolve(createEmptyMemory()),
    ]);

    return {
      global: settled[0].status === 'fulfilled' ? settled[0].value.entries : [],
      project: settled[1].status === 'fulfilled' ? settled[1].value.entries : [],
      globalFailed: settled[0].status === 'rejected',
      projectFailed: settled[1].status === 'rejected',
    };
  };

  return { read, readAll, create, update, remove, resolveTarget };
};
