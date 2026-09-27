/**
 * Magic prompts（内置提示词覆盖）的运行时模块。
 *
 * 以 magic-prompts.json 持久化"提示词 id → 覆盖文本"的状态：读取时对非法条目
 * 静默过滤（文件缺失或损坏回退为空状态），写入经串行化写锁排队避免并发覆盖；
 * 对外提供读取、设置单条、重置单条与全部重置四个操作。
 */
/** 状态文件格式版本号。 */
const FILE_VERSION = 1;
/** 单条提示词覆盖文本的最大长度（200k 字符）。 */
const MAX_PROMPT_TEXT_LENGTH = 200_000;
/** 合法提示词 id 的形态：小写字母/数字/点/下划线/连字符，长度 1-160。 */
const PROMPT_ID_PATTERN = /^[a-z0-9._-]{1,160}$/;
/** 判断 id 是否为"可见提示词"（.visible 结尾），这类提示词不允许把文本置空。 */
const isVisiblePromptID = (id) => typeof id === 'string' && id.endsWith('.visible');

/** 安全的 hasOwnProperty 封装，只认对象自身的属性、不受原型链污染。 */
const hasOwn = (input, key) => Object.prototype.hasOwnProperty.call(input, key);

/**
 * 清洗 overrides 对象：仅保留 id 符合 PROMPT_ID_PATTERN 且值为字符串的条目，
 * 其余条目直接丢弃，返回干净的新对象。
 */
const sanitizeOverrides = (value) => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return {};
  }

  const next = {};
  for (const [key, entry] of Object.entries(value)) {
    if (!PROMPT_ID_PATTERN.test(key) || typeof entry !== 'string') {
      continue;
    }
    next[key] = entry;
  }
  return next;
};

/**
 * 创建一个绑定到具体文件路径的 magic prompts 运行时。
 * fsPromises 与 path 通过依赖注入传入（便于测试替身）；返回的各方法
 * 均经 persist 的串行写锁排队执行，保证并发修改不会相互覆盖。
 * @param {{ fsPromises: object, path: object, filePath: string }} dependencies
 *   注入的 fs promises API、path 模块与状态文件路径
 * @returns {{ readPromptState: Function, setOverride: Function, resetOverride: Function, resetAllOverrides: Function }}
 */
export const createMagicPromptRuntime = (dependencies) => {
  const {
    fsPromises,
    path,
    filePath,
  } = dependencies;

  // 串行写锁：所有持久化操作按顺序排队执行，避免并发读-改-写竞态。
  let writeLock = Promise.resolve();

  // 读取并解析状态文件：文件缺失（ENOENT）或解析失败时回退为空 overrides 并告警。
  const readPromptState = async () => {
    try {
      const raw = await fsPromises.readFile(filePath, 'utf8');
      const parsed = JSON.parse(raw);
      const overrides = sanitizeOverrides(parsed?.overrides);
      return {
        version: FILE_VERSION,
        overrides,
      };
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return { version: FILE_VERSION, overrides: {} };
      }
      console.warn('Failed to read magic prompts file:', error);
      return { version: FILE_VERSION, overrides: {} };
    }
  };

  // 将状态以两个空格缩进的 JSON 写入文件（目录不存在时递归创建）。
  const writePromptState = async (state) => {
    await fsPromises.mkdir(path.dirname(filePath), { recursive: true });
    await fsPromises.writeFile(filePath, JSON.stringify(state, null, 2), 'utf8');
  };

  // 串行化持久化：把"读-改-写"闭包排队到写锁上执行，返回排队后的最终状态。
  const persist = async (mutator) => {
    const run = async () => {
      const current = await readPromptState();
      const next = await mutator(current);
      await writePromptState(next);
      return next;
    };
    writeLock = writeLock.then(run, run);
    return writeLock;
  };

  // 设置单条覆盖：校验 id 与文本（可见提示词不可为空、长度受上限约束），返回新状态。
  const setOverride = async (id, text) => {
    const normalizedId = typeof id === 'string' ? id.trim() : '';
    if (!PROMPT_ID_PATTERN.test(normalizedId)) {
      throw new Error('Invalid prompt id');
    }
    if (typeof text !== 'string') {
      throw new Error('Prompt text must be a string');
    }
    if (isVisiblePromptID(normalizedId) && text.trim().length === 0) {
      throw new Error('Visible prompt text cannot be empty');
    }
    if (text.length > MAX_PROMPT_TEXT_LENGTH) {
      throw new Error('Prompt text is too long');
    }

    return persist(async (state) => {
      const nextOverrides = { ...state.overrides, [normalizedId]: text };
      return {
        version: FILE_VERSION,
        overrides: nextOverrides,
      };
    });
  };

  // 重置单条覆盖：id 不存在时原样返回，否则删除该条并落盘。
  const resetOverride = async (id) => {
    const normalizedId = typeof id === 'string' ? id.trim() : '';
    if (!PROMPT_ID_PATTERN.test(normalizedId)) {
      throw new Error('Invalid prompt id');
    }

    return persist(async (state) => {
      if (!hasOwn(state.overrides, normalizedId)) {
        return state;
      }
      const nextOverrides = { ...state.overrides };
      delete nextOverrides[normalizedId];
      return {
        version: FILE_VERSION,
        overrides: nextOverrides,
      };
    });
  };

  // 重置全部覆盖：清空 overrides 并落盘。
  const resetAllOverrides = async () => {
    return persist(async () => ({ version: FILE_VERSION, overrides: {} }));
  };

  // 对外暴露的运行时 API。
  return {
    readPromptState,
    setOverride,
    resetOverride,
    resetAllOverrides,
  };
};
