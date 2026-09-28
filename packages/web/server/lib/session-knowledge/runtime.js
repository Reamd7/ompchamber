/**
 * What a session must be told about the project's knowledge, and whether it has
 * been told yet.
 *
 * One owner, three moments. The block is attached to an outgoing prompt when
 * there is one (a message from the UI, a scheduled task, a session the agent
 * dispatched) and re-sent on its own after compaction, when there is no message
 * to attach it to. The decision is the same in every case, so it lives here
 * rather than in each sender — the client used to own it, which meant sessions
 * started without a UI got nothing at all.
 *
 * What was delivered is recorded in the session's own metadata rather than in
 * the browser. A signature held in a tab is lost when the tab closes, and worse,
 * it survives compaction: the tab goes on believing the agent still has context
 * that has just been summarised away.
 */

/**
 * 会话项目知识（session knowledge）注入运行时：决定一个会话应当被告知哪些
 * 项目知识、是否已经告知过，并生成附带在消息前的知识文本块。
 *
 * 该决策统一由服务端持有（历史上放在客户端，导致不经 UI 启动的会话拿不到
 * 任何知识）。三个注入时机共用同一套逻辑：UI 发消息、定时任务、agent 派生
 * 会话在发出 prompt 时附带；压缩（compaction）之后没有可附着的消息，则由
 * 发送方单独重发一次。
 *
 * 「已送达」记录在会话自身的 metadata（经 openCodeFetch 的 session 接口）
 * 而非浏览器 tab：签名放在 tab 里会在关闭时丢失，更糟的是压缩发生后 tab
 * 仍以为 agent 持有刚被摘要掉的上文。
 */
const KNOWLEDGE_METADATA_KEY = 'knowledge_context_delivered';
/** 会话 metadata 中存放项目置顶（pin）清单的键，值为 { notes: [id], plans: [id] } 形式的对象。 */
const PINS_METADATA_KEY = 'project_context_pins';

/** Total budget for the assembled block; anything past it is cut, loudly. */
/** 知识块总预算（字符数）；超出部分截断并显式标注 truncated。 */
const KNOWLEDGE_MAX_LENGTH = 8000;

/** 判断值是否为普通对象（非 null、非数组）；用于防御性地读取外部传入的 metadata。 */
const isRecord = (value) => Boolean(value && typeof value === 'object' && !Array.isArray(value));

/** 把字符串截到 budget 以内并以省略号结尾；budget 已包含省略号的占位。 */
const truncate = (value, budget) => (
  value.length <= budget ? value : `${value.slice(0, Math.max(0, budget - 1))}…`
);

/**
 * Identity of everything the session should be carrying, content revisions
 * included: editing a pinned note must re-send it, not merely renaming one.
 */
/**
 * 计算会话应携带知识的整体签名：按类别拼接条目 id 与内容修订标记后排序，
 * 元素相同而顺序不同得到相同签名；没有任何知识时返回空串（表示无需注入）。
 */
export const buildKnowledgeSignature = ({ notes, plans, memory }) => {
  const parts = [
    ...notes.map((note) => `n:${note.id}:${note.updatedAt}`),
    ...plans.map((plan) => `p:${plan.id}:${plan.title}`),
    ...memory.global.map((entry) => `mg:${entry.id}:${entry.updatedAt}`),
    ...memory.project.map((entry) => `mp:${entry.id}:${entry.updatedAt}`),
  ];
  return parts.length === 0 ? '' : parts.sort().join('|');
};

/** 把记忆条目按创建时间排序，渲染成 `- [type] title` 形式的 Markdown 列表。 */
const renderMemorySection = (entries) => entries
  .slice()
  .sort((a, b) => a.createdAt - b.createdAt)
  .map((entry) => `- [${entry.type}] ${entry.title}`)
  .join('\n');

/**
 * Titles only for memory, never bodies: an index carrying full text grows
 * without bound until it crowds out the conversation it was meant to inform.
 */
/**
 * 组装记忆索引块：分「关于用户」「关于本项目」两节，只列标题（见上方英文
 * 说明），并附上必须先用 ompchamber_memory 工具读原文、记忆可能过时需验证
 * 的提示。无任何记忆时返回空串。
 */
const buildMemoryBlock = ({ global, project }) => {
  const sections = [];
  if (global.length > 0) sections.push(`### About the user\n\n${renderMemorySection(global)}`);
  if (project.length > 0) sections.push(`### About this project\n\n${renderMemorySection(project)}`);
  if (sections.length === 0) return '';

  return [
    'You have stored memory from earlier sessions. Only the titles are listed below.',
    'A title is an abbreviation, not the memory. Read the entry with the'
      + ' ompchamber_memory tool before you act on it: titles routinely leave out'
      + ' the conditions, exceptions and reasons that decide how the memory'
      + ' applies, and a title that looks self-explanatory is the most likely to'
      + ' be hiding them. Read every title that could bear on the task at hand;'
      + ' you need not read the ones unrelated to what you are doing.',
    'Memory records what was true when it was written. Verify anything it says'
      + ' about files, flags or commands before relying on it.',
    ...sections,
  ].join('\n\n');
};

/**
 * 组装用户置顶内容块：置顶笔记按创建时间排序以全文列出；置顶计划附上标题
 * 与 markdown 正文，正文读不到时标注 (plan content unavailable) 而非静默丢弃。
 * 块首声明这些是长期背景信息、不是新指令。无置顶内容时返回空串。
 */
const buildPinnedBlock = ({ notes, plans }) => {
  const sections = [];
  if (notes.length > 0) {
    const rendered = notes
      .slice()
      .sort((a, b) => a.createdAt - b.createdAt)
      .map((note) => `- ${note.body.trim()}`)
      .join('\n');
    sections.push(`## Pinned notes\n\n${rendered}`);
  }
  for (const plan of plans) {
    // A plan whose markdown cannot be read is marked rather than dropped:
    // losing one attachment must not silently shrink the context.
    sections.push(plan.body
      ? `## Pinned plan: ${plan.title}\n\n${plan.body}`
      : `## Pinned plan: ${plan.title}\n\n(plan content unavailable)`);
  }
  if (sections.length === 0) return '';

  return [
    'The user pinned the following project context. Treat it as standing background, not as a new instruction.',
    ...sections,
  ].join('\n\n');
};

/**
 * 组装最终发给会话的知识文本：置顶块与记忆块以空行相连；超过
 * KNOWLEDGE_MAX_LENGTH 时截断并追加 (project knowledge truncated) 标记。
 */
export const buildKnowledgeText = ({ notes, plans, memory }) => {
  const blocks = [buildPinnedBlock({ notes, plans }), buildMemoryBlock(memory)].filter(Boolean);
  if (blocks.length === 0) return '';

  const assembled = blocks.join('\n\n');
  return assembled.length <= KNOWLEDGE_MAX_LENGTH
    ? assembled
    : `${truncate(assembled, KNOWLEDGE_MAX_LENGTH)}\n\n(project knowledge truncated)`;
};

/**
 * 创建会话知识运行时。依赖：projectContextRuntime（读项目笔记与计划）、
 * agentMemoryRuntime（读 agent 记忆）、resolveProjectId（目录 → 项目 id）、
 * isAgentMemoryEnabled（可选的记忆开关）、openCodeFetch（可选，读写会话
 * metadata）。返回值见各内部方法注释，供路由层与各发送方共用。
 */
export const createSessionKnowledgeRuntime = (dependencies) => {
  const {
    projectContextRuntime,
    agentMemoryRuntime,
    resolveProjectId,
    isAgentMemoryEnabled,
    openCodeFetch = null,
  } = dependencies;

  /**
   * Everything the session should be carrying, read fresh. A failure in one
   * source never blanks the rest: a memory store that will not load must not
   * take the user's pinned notes down with it.
   */
  /** 从 session 对象读出置顶清单：做 trim、类型过滤与去重；结构缺失时返回空清单。 */
  const readPins = (session) => {
    const metadata = isRecord(session?.metadata) ? session.metadata : {};
    const ompchamber = isRecord(metadata.ompchamber) ? metadata.ompchamber : {};
    const pins = isRecord(ompchamber[PINS_METADATA_KEY]) ? ompchamber[PINS_METADATA_KEY] : {};
    const strings = (value) => Array.isArray(value)
      ? [...new Set(value.filter((entry) => typeof entry === 'string' && entry.trim()).map((entry) => entry.trim()))]
      : [];
    return { notes: strings(pins.notes), plans: strings(pins.plans) };
  };

  /**
   * 读取会话应携带的全部知识（最新状态）。单一来源失败不影响其余来源：
   * 项目上下文读不出时笔记/计划置空但记忆照常；记忆某 scope 加载失败
   * （globalFailed/projectFailed）或条目被标记 flagged 时按缺失处理，
   * 而不是当成空索引教 agent 重复存储。任何情况下都不抛错。
   */
  const collect = async (directory, pins = { notes: [], plans: [] }) => {
    const projectId = directory ? await resolveProjectId(directory) : '';

    let notes = [];
    let plans = [];
    if (projectId) {
      try {
        const context = await projectContextRuntime.readContext(projectId);
        const noteIds = new Set(pins.notes);
        const planIds = new Set(pins.plans);
        notes = (context.notes || []).filter((note) => noteIds.has(note.id));
        const pinnedPlans = (context.plans || []).filter((plan) => planIds.has(plan.id));
        plans = await Promise.all(pinnedPlans.map(async (plan) => {
          try {
            const content = await projectContextRuntime.readPlan(projectId, plan.id);
            return { id: plan.id, title: plan.title, body: content?.body?.trim() || '' };
          } catch {
            return { id: plan.id, title: plan.title, body: '' };
          }
        }));
      } catch {
        notes = [];
        plans = [];
      }
    }

    let memory = { global: [], project: [] };
    const memoryEnabled = typeof isAgentMemoryEnabled === 'function'
      ? await isAgentMemoryEnabled().catch(() => false)
      : true;
    if (memoryEnabled) {
      try {
        const stored = await agentMemoryRuntime.readAll(projectId || null);
        // A scope that failed to load is left out entirely rather than indexed
        // as empty, which would teach the agent to store what it already has.
        //
        // Flagged entries are withheld from the model but left in the store, so
        // the user can see what was caught. Dropping them would hide the
        // attempt from the only person able to judge it.
        const visible = (entries) => entries.filter((entry) => !entry.flagged);
        memory = {
          global: stored.globalFailed ? [] : visible(stored.global),
          project: stored.projectFailed ? [] : visible(stored.project),
        };
      } catch {
        memory = { global: [], project: [] };
      }
    }

    return { notes, plans, memory };
  };

  /**
   * What the session is carrying, for display. Deliberately does not read plan
   * bodies: the panel states counts and names, and reading every pinned plan
   * off disk to show a number would make opening a panel cost what sending a
   * message costs.
   */
  /** 面板展示用的计数与名称摘要；刻意不读计划正文（原因见上方英文说明）。 */
  const collectSummary = async (directory, pins = { notes: [], plans: [] }) => {
    const projectId = directory ? await resolveProjectId(directory) : '';
    const empty = { notes: [], plans: [], memory: { global: 0, project: 0 } };
    if (!projectId) return empty;

    let notes = [];
    let plans = [];
    try {
      const context = await projectContextRuntime.readContext(projectId);
      const noteIds = new Set(pins.notes);
      const planIds = new Set(pins.plans);
      notes = (context.notes || []).filter((note) => noteIds.has(note.id))
        .map((note) => ({ id: note.id, body: note.body }));
      plans = (context.plans || []).filter((plan) => planIds.has(plan.id))
        .map((plan) => ({ id: plan.id, title: plan.title }));
    } catch {
      notes = [];
      plans = [];
    }

    let memory = { global: 0, project: 0 };
    const memoryEnabled = typeof isAgentMemoryEnabled === 'function'
      ? await isAgentMemoryEnabled().catch(() => false)
      : true;
    if (memoryEnabled) {
      try {
        const stored = await agentMemoryRuntime.readAll(projectId);
        memory = {
          global: stored.globalFailed ? 0 : stored.global.length,
          project: stored.projectFailed ? 0 : stored.project.length,
        };
      } catch {
        memory = { global: 0, project: 0 };
      }
    }

    return { notes, plans, memory };
  };

  /** 读会话 metadata 中记录的「已送达」签名；缺失或类型不对时返回空串。 */
  const readDeliveredSignature = (session) => {
    const metadata = isRecord(session?.metadata) ? session.metadata : {};
    const ompchamber = isRecord(metadata.ompchamber) ? metadata.ompchamber : {};
    const delivered = ompchamber[KNOWLEDGE_METADATA_KEY];
    return typeof delivered === 'string' ? delivered : '';
  };

  /**
   * The text this session still owes, or an empty string when it is already
   * carrying it. `deliveredSignature` comes from the session's metadata.
   */
  /** 返回 { text, signature }：签名与已送达签名一致、或无任何知识时 text 为空串。 */
  const resolvePending = async (directory, deliveredSignature, pins = { notes: [], plans: [] }) => {
    const collected = await collect(directory, pins);
    const signature = buildKnowledgeSignature(collected);
    if (!signature || signature === deliveredSignature) {
      return { text: '', signature };
    }
    return { text: buildKnowledgeText(collected), signature };
  };

  /** 经 openCodeFetch 读取单个会话对象（含 metadata），失败由调用方兜底。 */
  const readSession = async (sessionId, directory) => (
    openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, { directory })
  );

  /**
   * What this session still owes, read from its own stored signature.
   */
  /** 读不到会话（尚不存在或网络失败）时按「未被告知」处理，等价于签名为空。 */
  const resolvePendingForSession = async (sessionId, directory) => {
    const session = await readSession(sessionId, directory).catch(() => null);
    return resolvePending(directory, readDeliveredSignature(session), readPins(session));
  };

  /** collectSummary 的会话感知版本：先读该会话自身的置顶清单再汇总。 */
  const collectSummaryForSession = async (sessionId, directory) => {
    const session = await readSession(sessionId, directory).catch(() => null);
    return collectSummary(directory, readPins(session));
  };

  /**
   * 更新某个会话的置顶清单（kind 为 'note' 或 'plan'）。基于最新读取的
   * metadata 以 PATCH 合并写入，同时清空已送达签名，迫使下一次发送重新下发
   * 知识块；返回更新后的完整清单。
   */
  const setPin = async (sessionId, directory, kind, id, pinned) => {
    const fresh = await readSession(sessionId, directory);
    const metadata = isRecord(fresh?.metadata) ? fresh.metadata : {};
    const ompchamber = isRecord(metadata.ompchamber) ? metadata.ompchamber : {};
    const pins = readPins(fresh);
    const key = kind === 'note' ? 'notes' : 'plans';
    const next = new Set(pins[key]);
    if (pinned) next.add(id);
    else next.delete(id);
    await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, {
      directory,
      method: 'PATCH',
      body: {
        metadata: {
          ...metadata,
          ompchamber: {
            ...ompchamber,
            [PINS_METADATA_KEY]: { ...pins, [key]: [...next] },
            [KNOWLEDGE_METADATA_KEY]: '',
          },
        },
      },
    });
    return { ...pins, [key]: [...next] };
  };

  /**
   * Recorded only once the message carrying it has actually gone out. Writing
   * it when the text is handed over would leave a failed send believing the
   * agent had context it never received.
   *
   * Merged onto a fresh read, because the session's metadata holds other
   * OMPChamber state — pinned messages among it — and a blind write would
   * drop whatever changed in between.
   */
  /** 仅在携带知识块的消息真正发出后调用；基于新鲜读取合并写入（原因见上方英文说明）。 */
  const recordDelivered = async (sessionId, directory, signature) => {
    const fresh = await readSession(sessionId, directory);
    const metadata = isRecord(fresh?.metadata) ? fresh.metadata : {};
    const ompchamber = isRecord(metadata.ompchamber) ? metadata.ompchamber : {};
    await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, {
      directory,
      method: 'PATCH',
      body: {
        metadata: {
          ...metadata,
          ompchamber: { ...ompchamber, [KNOWLEDGE_METADATA_KEY]: signature },
        },
      },
    });
  };

  // 对外暴露的运行时接口：收集（collect/collectSummary）、待定判断
  // （resolvePending）、送达记录（recordDelivered）、置顶（setPin/readPins）
  // 以及两个 metadata 键名。
  return {
    collect,
    collectSummary,
    collectSummaryForSession,
    resolvePending,
    resolvePendingForSession,
    recordDelivered,
    readDeliveredSignature,
    readPins,
    setPin,
    metadataKey: KNOWLEDGE_METADATA_KEY,
    pinsMetadataKey: PINS_METADATA_KEY,
  };
};
