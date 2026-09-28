/**
 * 项目级配置文件（projects/<projectId>.json）的读写 runtime。
 *
 * 负责 scheduled tasks 的规范化（schedule/execution/state 的校验、
 * 截断与归一）、双重写锁（进程内链式 Promise + 跨进程锁文件）保护下的
 * 原子读改写、向前兼容的未知字段保留，以及 `.agents/loops` 发现结果
 * 与 JSON 任务列表的对账（reconcile）。
 */

import { DateTime, IANAZone } from 'luxon';
import parser from 'cron-parser';

/** 配置文件格式版本号；每次写入都会重置为当前版本。 */
const PROJECT_CONFIG_VERSION = 1;
/** 任务名称最大长度（超出截断）；导出供 UI 侧做同规则校验。 */
export const MAX_TASK_NAME_LENGTH = 80;
/** 任务 prompt 最大长度（超出截断）。 */
const MAX_TASK_PROMPT_LENGTH = 20_000;
/** cron 表达式最大长度（超出截断后为空即报错）。 */
const MAX_CRON_LENGTH = 200;
/** 持久化的最近一次错误消息最大长度，防止异常堆栈撑爆配置文件。 */
const MAX_LAST_ERROR_LENGTH = 2_000;

/** 输入为非空字符串时返回 trim 后的值，否则返回 null（统一的「可选字符串」规范化入口）。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 把字符串截断到 maxLength；非字符串返回空串（只裁剪不报错，用于 name/lastError 等展示字段）。 */
const clampLength = (value, maxLength) => {
  if (typeof value !== 'string') {
    return '';
  }
  return value.length > maxLength ? value.slice(0, maxLength) : value;
};

/** 把 lastStatus 归一到合法枚举 running/success/error/idle，未知或缺失一律落回 idle。 */
const normalizeStatus = (value) => {
  if (value === 'running' || value === 'success' || value === 'error' || value === 'idle') {
    return value;
  }
  return 'idle';
};

/** 校验 24 小时制 HH:mm 时间字符串，非法或非字符串返回 null。 */
const normalizeTimeValue = (value) => {
  const time = asNonEmptyString(value);
  if (!time) {
    return null;
  }
  if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(time)) {
    return null;
  }
  return time;
};

/**
 * 校验 YYYY-MM-DD 日期字符串：正则限定格式后用 luxon 按 UTC 解析并
 * 回格式化比对，排除「2 月 30 日」这类幻影日期；非法返回 null。
 */
const normalizeDateValue = (value) => {
  const date = asNonEmptyString(value);
  if (!date) {
    return null;
  }
  if (!/^\d{4}-\d{2}-\d{2}$/.test(date)) {
    return null;
  }
  const parsed = DateTime.fromISO(date, { zone: 'UTC' });
  if (!parsed.isValid || parsed.toFormat('yyyy-LL-dd') !== date) {
    return null;
  }
  return date;
};

/**
 * 校验星期数组（0=周日 … 6=周六）：元素必须是 0-6 的整数；去重后
 * 升序返回。空数组、非数组或含非法值返回 null。
 */
const normalizeWeekdays = (value) => {
  if (!Array.isArray(value)) {
    return null;
  }

  const unique = new Set();
  for (const entry of value) {
    if (!Number.isInteger(entry)) {
      return null;
    }
    if (entry < 0 || entry > 6) {
      return null;
    }
    unique.add(entry);
  }

  if (unique.size === 0) {
    return null;
  }

  return Array.from(unique).sort((a, b) => a - b);
};

/**
 * 合并出 schedule 最终生效的 times 列表：优先取新值 value.times（数组，
 * 任一非法 HH:mm 直接抛错）与遗留单值 value.time，两者皆空时沿用
 * existingSchedule.times；结果去重并按字典序排序，完全为空返回 null。
 */
const resolveScheduleTimes = (value, existingSchedule) => {
  const times = [];

  if (Array.isArray(value?.times)) {
    for (const item of value.times) {
      const normalized = normalizeTimeValue(item);
      if (!normalized) {
        throw new Error('schedule.times must contain HH:mm values');
      }
      times.push(normalized);
    }
  }

  const legacySingleTime = normalizeTimeValue(value?.time);
  if (legacySingleTime) {
    times.push(legacySingleTime);
  }

  if (times.length === 0 && Array.isArray(existingSchedule?.times)) {
    for (const item of existingSchedule.times) {
      const normalized = normalizeTimeValue(item);
      if (normalized) {
        times.push(normalized);
      }
    }
  }

  const uniqueSorted = Array.from(new Set(times)).sort((a, b) => a.localeCompare(b));
  if (uniqueSorted.length === 0) {
    return null;
  }
  return uniqueSorted;
};

/** 取系统本地 IANA 时区名作为 schedule.timezone 的默认值，取不到或无效时回退 UTC。 */
const resolveDefaultTimezone = () => {
  const resolved = DateTime.local().zoneName;
  if (resolved && IANAZone.isValidZone(resolved)) {
    return resolved;
  }
  return 'UTC';
};

/**
 * 校验时区：值为空返回 fallback（默认取系统时区）；非空的字符串必须是
 * 合法 IANA 时区名，否则返回 null 交由调用方报错。
 */
const normalizeTimezone = (value, fallback = resolveDefaultTimezone()) => {
  const timezone = asNonEmptyString(value);
  if (!timezone) {
    return fallback;
  }
  return IANAZone.isValidZone(timezone) ? timezone : null;
};

/**
 * 用 cron-parser 在指定时区试解析表达式并推进一次，验证其可执行；
 * 任何解析/迭代异常都视为表达式非法，返回 false。
 */
const validateCronExpression = (expression, timezone) => {
  try {
    const iterator = parser.parseExpression(expression, {
      tz: timezone,
      currentDate: new Date(),
    });
    iterator.next();
    return true;
  } catch {
    return false;
  }
};

/**
 * 校验并归一 schedule 对象（失败抛带字段名的 Error）：
 * kind 必须是 daily/weekly/once/cron 之一；timezone 必须是合法 IANA 名
 * （缺省回填 existingSchedule.timezone 或系统时区）；daily/weekly 需要
 * 至少一个合法 times（weekly 另需合法 weekdays），once 需要合法的
 * date + time，cron 需要能通过校验的 cron 表达式。
 */
const normalizeSchedule = (value, existingSchedule) => {
  if (!value || typeof value !== 'object') {
    throw new Error('schedule is required');
  }

  const kind = asNonEmptyString(value.kind);
  if (kind !== 'daily' && kind !== 'weekly' && kind !== 'once' && kind !== 'cron') {
    throw new Error('schedule.kind must be daily, weekly, once, or cron');
  }

  const fallbackTimezone = existingSchedule?.timezone || resolveDefaultTimezone();
  const timezone = normalizeTimezone(value.timezone, fallbackTimezone);
  if (!timezone) {
    throw new Error('schedule.timezone must be a valid IANA timezone');
  }

  if (kind === 'daily') {
    const times = resolveScheduleTimes(value, existingSchedule);
    if (!times) {
      throw new Error('schedule.times must include at least one HH:mm value for daily schedule');
    }
    return { kind, times, timezone };
  }

  if (kind === 'weekly') {
    const times = resolveScheduleTimes(value, existingSchedule);
    if (!times) {
      throw new Error('schedule.times must include at least one HH:mm value for weekly schedule');
    }
    const weekdays = normalizeWeekdays(value.weekdays);
    if (!weekdays) {
      throw new Error('schedule.weekdays must include values from 0 to 6 for weekly schedule');
    }
    return { kind, times, weekdays, timezone };
  }

  if (kind === 'once') {
    const date = normalizeDateValue(value.date);
    if (!date) {
      throw new Error('schedule.date must be YYYY-MM-DD for once schedule');
    }

    const time = normalizeTimeValue(value.time);
    if (!time) {
      throw new Error('schedule.time must be HH:mm for once schedule');
    }

    return { kind, date, time, timezone };
  }

  const cron = clampLength(asNonEmptyString(value.cron) || '', MAX_CRON_LENGTH);
  if (!cron) {
    throw new Error('schedule.cron is required for cron schedule');
  }

  if (!validateCronExpression(cron, timezone)) {
    throw new Error('schedule.cron is invalid');
  }

  return { kind, cron, timezone };
};

/**
 * 校验并归一 execution（失败抛错）：prompt 必填（超长截断）；必须要么
 * 显式指定 providerID + modelID，要么声明 modelRole: 'default' 交由
 * 引擎选默认模型；goalEnabled/permissionAutoAccept 仅在显式为 true 时
 * 保留，goalTokenBudget 必须为正的有限数（且仅随 goalEnabled 保留）。
 */
const normalizeExecution = (value) => {
  if (!value || typeof value !== 'object') {
    throw new Error('execution is required');
  }

  const prompt = clampLength(asNonEmptyString(value.prompt) || '', MAX_TASK_PROMPT_LENGTH);
  const providerID = asNonEmptyString(value.providerID);
  const modelID = asNonEmptyString(value.modelID);
  const modelRole = value.modelRole === 'default' ? 'default' : undefined;
  const variant = asNonEmptyString(value.variant);
  const agent = asNonEmptyString(value.agent);
  const goalEnabled = value.goalEnabled === true;
  const permissionAutoAccept = value.permissionAutoAccept === true;
  const goalTokenBudget = typeof value.goalTokenBudget === 'number'
    && Number.isFinite(value.goalTokenBudget)
    && value.goalTokenBudget > 0
    ? Math.floor(value.goalTokenBudget)
    : undefined;

  if (!prompt) {
    throw new Error('execution.prompt is required');
  }
  // A task may either pin an explicit model (legacy contract, provider/model
  // required) or follow the engine's default model role (`modelRole:
  // 'default'`), in which case identifiers are omitted and the engine picks.
  if (!providerID && modelRole !== 'default') {
    throw new Error('execution.providerID is required');
  }
  if (!modelID && modelRole !== 'default') {
    throw new Error('execution.modelID is required');
  }

  return {
    prompt,
    ...(providerID ? { providerID } : {}),
    ...(modelID ? { modelID } : {}),
    ...(modelRole ? { modelRole } : {}),
    ...(variant ? { variant } : {}),
    ...(agent ? { agent } : {}),
    ...(goalEnabled ? { goalEnabled: true } : {}),
    ...(goalEnabled && goalTokenBudget ? { goalTokenBudget } : {}),
    ...(permissionAutoAccept ? { permissionAutoAccept: true } : {}),
  };
};

/**
 * 归一任务运行时 state：数值时间戳取整并压到非负，lastStatus 归一，
 * lastError 截断，createdAt/updatedAt 缺省补当前时间；只输出存在的
 * 字段。value 优先、fallback（磁盘上的既有 state）兜底。
 */
const normalizeState = (value, fallback) => {
  const source = value && typeof value === 'object' ? value : fallback || {};
  const lastRunAt = typeof source.lastRunAt === 'number' && Number.isFinite(source.lastRunAt)
    ? Math.max(0, Math.round(source.lastRunAt))
    : undefined;
  const lastDurationMs = typeof source.lastDurationMs === 'number' && Number.isFinite(source.lastDurationMs)
    ? Math.max(0, Math.round(source.lastDurationMs))
    : undefined;
  const nextRunAt = typeof source.nextRunAt === 'number' && Number.isFinite(source.nextRunAt)
    ? Math.max(0, Math.round(source.nextRunAt))
    : undefined;
  // Absolute ms of the schedule occurrence last claimed for dispatch. Used so
  // two OMPChamber server instances sharing this config cannot both start a
  // run for the same daily/weekly/cron/once slot (see issue #2710).
  const lastScheduledFor = typeof source.lastScheduledFor === 'number' && Number.isFinite(source.lastScheduledFor)
    ? Math.max(0, Math.round(source.lastScheduledFor))
    : undefined;
  const lastSessionId = asNonEmptyString(source.lastSessionId);
  const lastErrorRaw = asNonEmptyString(source.lastError);
  const lastError = lastErrorRaw ? clampLength(lastErrorRaw, MAX_LAST_ERROR_LENGTH) : undefined;

  return {
    createdAt: typeof source.createdAt === 'number' && Number.isFinite(source.createdAt)
      ? Math.max(0, Math.round(source.createdAt))
      : Date.now(),
    updatedAt: typeof source.updatedAt === 'number' && Number.isFinite(source.updatedAt)
      ? Math.max(0, Math.round(source.updatedAt))
      : Date.now(),
    lastStatus: normalizeStatus(source.lastStatus),
    ...(typeof lastRunAt === 'number' ? { lastRunAt } : {}),
    ...(typeof lastDurationMs === 'number' ? { lastDurationMs } : {}),
    ...(typeof nextRunAt === 'number' ? { nextRunAt } : {}),
    ...(typeof lastScheduledFor === 'number' ? { lastScheduledFor } : {}),
    ...(lastSessionId ? { lastSessionId } : {}),
    ...(lastError ? { lastError } : {}),
  };
};

/**
 * 把外部输入的任务规范化为存储形状：id 与既有任务不一致即抛错
 * （id 不可变）、name 必填（截断至上限）、schedule/execution 全量重校验、
 * state 继承既有值（refreshUpdatedAt 控制是否刷新 updatedAt）。
 * loopFile 是 loop 来源标记，优先取新值、否则保留既有值，供对账时
 * 检测 loop 文件删除；allowCreate=false 时拒绝创建带未知 id 的任务。
 */
const normalizeTaskForStorage = (value, options) => {
  const {
    now,
    createId,
    existingTask,
    allowCreate,
    refreshUpdatedAt = true,
  } = options;

  if (!value || typeof value !== 'object') {
    throw new Error('task is required');
  }

  const incomingId = asNonEmptyString(value.id);
  const existingId = asNonEmptyString(existingTask?.id);

  if (existingTask) {
    if (incomingId && incomingId !== existingId) {
      throw new Error('task.id is immutable');
    }
  }

  if (!existingTask && incomingId && !allowCreate) {
    throw new Error('task.id does not exist');
  }

  const id = existingId || incomingId || createId();
  const name = clampLength(asNonEmptyString(value.name) || '', MAX_TASK_NAME_LENGTH);
  if (!name) {
    throw new Error('task.name is required');
  }

  const enabled = typeof value.enabled === 'boolean'
    ? value.enabled
    : (existingTask?.enabled ?? true);

  const schedule = normalizeSchedule(value.schedule, existingTask?.schedule);
  const execution = normalizeExecution(value.execution);

  // Loop provenance: absolute path of the `.agents/loops/*.md` file driving
  // this task, when any. Preserved on every write so the scheduler can detect
  // removed loop files across restarts. Unknown to the UI model.
  const loopFile = asNonEmptyString(value.loopFile) ?? asNonEmptyString(existingTask?.loopFile);

  const nowMs = Math.max(0, Math.round(now));
  const baseState = normalizeState(value.state, existingTask?.state);
  const state = {
    ...baseState,
    createdAt: existingTask?.state?.createdAt ?? baseState.createdAt ?? nowMs,
    updatedAt: refreshUpdatedAt ? nowMs : baseState.updatedAt ?? nowMs,
  };

  return {
    id,
    name,
    enabled,
    schedule,
    execution,
    state,
    ...(loopFile ? { loopFile } : {}),
  };
};

/** 新项目的空配置骨架：仅版本号与空任务列表。 */
const createEmptyProjectConfig = () => ({
  version: PROJECT_CONFIG_VERSION,
  scheduledTasks: [],
});

/**
 * 创建作用于指定 projects 目录的配置 runtime。
 *
 * @param deps.fsPromises 文件系统 Promise API（测试可注入沙盒实现）
 * @param deps.path 路径工具（与 fsPromises 配套注入）
 * @param deps.projectsDirPath 各项目 `<projectId>.json` 所在目录
 * @param deps.createTaskID 新任务 id 生成器（缺省用 crypto.randomUUID
 *   或「时间戳 + 随机串」兜底）
 * @returns 暴露 list/upsert/delete、两个 state 更新入口、loop 对账与
 *   配置路径解析的 API 对象
 */
export const createProjectConfigRuntime = (deps) => {
  const {
    fsPromises,
    path,
    projectsDirPath,
    createTaskID,
  } = deps;

  /** 任务 id 生成器：优先使用注入的 createTaskID，否则退回随机方案。 */
  const taskIDFactory = typeof createTaskID === 'function'
    ? createTaskID
    : (() => {
      if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
        return crypto.randomUUID();
      }
      return `task_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`;
    });

  /** 项目 id → 链式 Promise 的进程内写锁表，串行化同一项目的并发写。 */
  const writeLocks = new Map();
  /** 获取跨进程文件锁的最长等待时间，超时抛错。 */
  const PROJECT_FILE_LOCK_WAIT_MS = 10_000;
  /** 锁文件年龄超过该阈值即视为陈旧，可被抢占删除。 */
  const PROJECT_FILE_LOCK_STALE_MS = 60_000;
  /** 抢锁失败后的重试间隔。 */
  const PROJECT_FILE_LOCK_RETRY_MS = 20;

  /** 校验 projectId 非空且仅含 [a-zA-Z0-9._:-]，防止路径穿越与怪异文件名；非法即抛错。 */
  const sanitizeProjectID = (projectID) => {
    const value = asNonEmptyString(projectID);
    if (!value) {
      throw new Error('projectId is required');
    }
    if (!/^[a-zA-Z0-9._:-]+$/.test(value)) {
      throw new Error('projectId contains unsupported characters');
    }
    return value;
  };

  /** 返回 projectId 对应的配置文件绝对路径（内部先做 id 消毒）。 */
  const resolveProjectConfigPath = (projectID) => {
    const safeProjectID = sanitizeProjectID(projectID);
    return path.join(projectsDirPath, `${safeProjectID}.json`);
  };

  /** 用 kill(pid, 0) 探测进程是否存活；EPERM 表示进程存在但属其它用户，同样视为存活。 */
  const isProcessAlive = (pid) => {
    if (!Number.isInteger(pid) || pid <= 0) {
      return false;
    }
    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      // EPERM: process exists but belongs to another user — treat as alive.
      return error?.code === 'EPERM';
    }
  };

  /**
   * Cross-process exclusive lock for a project config file.
   * In-process chaining alone cannot serialize Electron (port 57123) and CLI
   * serve (port 3000) writers that share the same on-disk projects dir.
   */
  /**
   * 中文说明：以 open(lockPath, 'wx') 独占创建 `<config>.json.lock`
   * 实现跨进程互斥（Electron 与 CLI serve 两个进程可能写同一目录），
   * 锁内容为 {pid, at}。持锁进程已死或锁龄超阈值时删除陈旧锁重试；
   * 等待超时抛 timeout 错误。release 只在锁文件仍属本进程时 unlink，
   * 避免误删被抢占后新持有者的锁。
   */
  const acquireProjectFileLock = async (projectID) => {
    const configPath = resolveProjectConfigPath(projectID);
    const lockPath = `${configPath}.lock`;
    const startedAt = Date.now();

    await fsPromises.mkdir(path.dirname(configPath), { recursive: true });

    while (Date.now() - startedAt < PROJECT_FILE_LOCK_WAIT_MS) {
      let handle;
      try {
        handle = await fsPromises.open(lockPath, 'wx');
        const lockPayload = {
          pid: process.pid,
          at: Date.now(),
        };
        await handle.writeFile(JSON.stringify(lockPayload));
        return {
          release: async () => {
            try {
              await handle.close();
            } catch {
            }
            // Only unlink if we still own the lock. A stale-recovery steal can
            // replace the file; unlinking blindly would drop the new owner's lock.
            try {
              const raw = await fsPromises.readFile(lockPath, 'utf8');
              const parsed = JSON.parse(raw);
              if (Number(parsed?.pid) !== process.pid) {
                return;
              }
              await fsPromises.unlink(lockPath);
            } catch {
            }
          },
        };
      } catch (error) {
        if (handle) {
          try {
            await handle.close();
          } catch {
          }
        }
        if (error?.code !== 'EEXIST') {
          throw error;
        }

        try {
          const raw = await fsPromises.readFile(lockPath, 'utf8');
          const parsed = JSON.parse(raw);
          const lockPid = Number(parsed?.pid);
          const lockAt = Number(parsed?.at);
          const staleByPid = Number.isInteger(lockPid) && lockPid > 0 && !isProcessAlive(lockPid);
          const staleByAge = Number.isFinite(lockAt) && (Date.now() - lockAt) > PROJECT_FILE_LOCK_STALE_MS;
          if (staleByPid || staleByAge || !Number.isInteger(lockPid)) {
            await fsPromises.unlink(lockPath).catch(() => {});
            continue;
          }
        } catch {
          // Crash between open(wx) and writeFile (or a partial write) leaves an
          // unparseable lock. Fall back to mtime age so recovery is not wedged.
          try {
            const stat = await fsPromises.stat(lockPath);
            const mtimeMs = Number(stat?.mtimeMs);
            if (Number.isFinite(mtimeMs) && (Date.now() - mtimeMs) > PROJECT_FILE_LOCK_STALE_MS) {
              await fsPromises.unlink(lockPath).catch(() => {});
              continue;
            }
          } catch {
          }
        }

        await new Promise((resolve) => {
          setTimeout(resolve, PROJECT_FILE_LOCK_RETRY_MS);
        });
      }
    }

    throw new Error(`timeout acquiring project config lock for ${projectID}`);
  };

  /** 读取并解析磁盘上的原始配置 JSON；文件不存在返回空对象，其余读取/解析错误原样上抛。 */
  const readRawProjectConfigFromDisk = async (projectID) => {
    const filePath = resolveProjectConfigPath(projectID);
    try {
      const raw = await fsPromises.readFile(filePath, 'utf8');
      const parsed = JSON.parse(raw);
      return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : {};
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return {};
      }
      throw error;
    }
  };

  // Normalized tasks for reading, plus the raw on-disk record of each one for
  // writing back. Normalization only keeps the fields THIS build knows, so a
  // write that re-serialized normalized tasks would strip every field added
  // by a newer build (or a newer UI) the moment an older server touched the
  // file — a goal or auto-accept setting silently lost after a task ran.
  // Writers therefore persist untouched tasks from `rawTasksByID` verbatim and
  // only serialize a normalized task where the task itself was deliberately
  // replaced.
  /**
   * 中文说明：读取配置并返回双层视图——scheduledTasks 是本版本能识别
   * 的规范化任务，rawTasksByID 保留每个任务的原始磁盘记录，写入时
   * 原样回写以保住未知字段。单个任务规范化失败（数据损坏）仅跳过该
   * 任务，不影响其余任务加载。
   */
  const readProjectConfigFromDisk = async (projectID) => {
    const parsed = await readRawProjectConfigFromDisk(projectID);
    const tasksRaw = Array.isArray(parsed.scheduledTasks) ? parsed.scheduledTasks : [];
    const now = Date.now();
    const scheduledTasks = [];
    const rawTasksByID = new Map();
    for (const task of tasksRaw) {
      try {
        const normalized = normalizeTaskForStorage(task, {
          now,
          createId: taskIDFactory,
          existingTask: null,
          allowCreate: true,
          refreshUpdatedAt: false,
        });
        scheduledTasks.push(normalized);
        rawTasksByID.set(normalized.id, task);
      } catch {
      }
    }
    return {
      version: PROJECT_CONFIG_VERSION,
      scheduledTasks,
      rawTasksByID,
    };
  };

  // The list to write: tasks this write replaced go out normalized; every
  // other task goes out exactly as stored, fields unknown to this build
  // included. A state-only update counts as untouched — only its `state` is
  // swapped onto the stored record. Callers keep working with (and returning)
  // the normalized tasks; only the bytes on disk differ.
  /**
   * 中文说明：计算实际落盘的任务列表——本次被整体替换的任务
   * （replacedIDs）写规范化结果；其余任务沿用 rawTasksByID 里的原始
   * 记录；stateUpdatedID 指定的任务只在原始记录上替换 state 字段。
   */
  const toStoredTasks = (config, tasks, { replacedIDs = new Set(), stateUpdatedID = null } = {}) => (
    tasks.map((task) => {
      if (replacedIDs.has(task.id)) return task;
      const stored = config.rawTasksByID.get(task.id);
      if (!stored) return task;
      return task.id === stateUpdatedID ? { ...stored, state: task.state } : stored;
    })
  );

  /**
   * 原子写盘：先读旧文件以合并保留顶层未知键，再写唯一命名的临时
   * 文件并 rename 覆盖目标（读者不会看到半截 JSON）；失败时清理
   * 临时文件并上抛。调用方须已持有该项目的写锁。
   */
  const writeProjectConfigToDisk = async (projectID, config) => {
    const filePath = resolveProjectConfigPath(projectID);
    const parentDirectory = path.dirname(filePath);
    const temporaryPath = `${filePath}.tmp-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;

    const existing = await readRawProjectConfigFromDisk(projectID);
    const merged = {
      ...existing,
      version: PROJECT_CONFIG_VERSION,
      scheduledTasks: Array.isArray(config?.scheduledTasks) ? config.scheduledTasks : [],
    };

    await fsPromises.mkdir(parentDirectory, { recursive: true });
    try {
      await fsPromises.writeFile(temporaryPath, JSON.stringify(merged, null, 2), 'utf8');
      await fsPromises.rename(temporaryPath, filePath);
    } catch (error) {
      await fsPromises.rm(temporaryPath, { force: true }).catch(() => {});
      throw error;
    }
  };

  /**
   * 在「进程内链式锁 + 跨进程文件锁」双重保护下执行 mutate（读-改-写）。
   * 文件锁获取失败也必须先释放进程内链，否则该项目的后续写入会
   * 永远卡在 await previous 上。
   */
  const withProjectWriteLock = async (projectID, mutate) => {
    const key = sanitizeProjectID(projectID);
    const previous = writeLocks.get(key) || Promise.resolve();
    let release;
    const next = new Promise((resolve) => {
      release = resolve;
    });
    const chained = previous.finally(() => next);
    writeLocks.set(key, chained);

    await previous;
    // Acquire sits outside the mutate try — if it throws (10s lock timeout),
    // we must still release the in-process chain or every later write for this
    // project hangs forever on await previous.
    try {
      const fileLock = await acquireProjectFileLock(projectID);
      try {
        return await mutate();
      } finally {
        await fileLock.release();
      }
    } finally {
      release();
      const current = writeLocks.get(key);
      if (current === chained) {
        writeLocks.delete(key);
      }
    }
  };

  /** 列出项目的全部规范化 scheduled tasks（只读，不加锁）。 */
  const listScheduledTasks = async (projectID) => {
    const config = await readProjectConfigFromDisk(projectID);
    return config.scheduledTasks;
  };

  /**
   * 创建或更新任务：按传入 id 匹配既有任务（id 不可变，name 等字段
   * 全量替换），规范化后在校验+写锁内落盘；返回 {task, tasks, created}，
   * created 区分新建与覆盖。
   */
  const upsertScheduledTask = async (projectID, taskInput) => {
    return withProjectWriteLock(projectID, async () => {
      const now = Date.now();
      const current = await readProjectConfigFromDisk(projectID);
      const incomingID = asNonEmptyString(taskInput?.id);
      const existingIndex = incomingID
        ? current.scheduledTasks.findIndex((task) => task.id === incomingID)
        : -1;
      const existingTask = existingIndex >= 0 ? current.scheduledTasks[existingIndex] : null;

      const normalizedTask = normalizeTaskForStorage(taskInput, {
        now,
        createId: taskIDFactory,
        existingTask,
        allowCreate: true,
      });

      const nextTasks = current.scheduledTasks.slice();
      const created = !existingTask;
      if (existingIndex >= 0) {
        nextTasks[existingIndex] = normalizedTask;
      } else {
        nextTasks.push(normalizedTask);
      }

      const nextConfig = {
        version: PROJECT_CONFIG_VERSION,
        scheduledTasks: toStoredTasks(current, nextTasks, { replacedIDs: new Set([normalizedTask.id]) }),
      };
      await writeProjectConfigToDisk(projectID, nextConfig);

      return {
        task: normalizedTask,
        tasks: nextTasks,
        created,
      };
    });
  };

  /**
   * 删除指定 id 的任务；任务不存在时 deleted 为 false 且不写盘。
   * 返回删除后的任务列表。
   */
  const deleteScheduledTask = async (projectID, taskID) => {
    return withProjectWriteLock(projectID, async () => {
      const normalizedTaskID = asNonEmptyString(taskID);
      if (!normalizedTaskID) {
        throw new Error('taskId is required');
      }

      const current = await readProjectConfigFromDisk(projectID);
      const nextTasks = current.scheduledTasks.filter((task) => task.id !== normalizedTaskID);
      const deleted = nextTasks.length !== current.scheduledTasks.length;

      if (deleted) {
        await writeProjectConfigToDisk(projectID, {
          version: PROJECT_CONFIG_VERSION,
          scheduledTasks: toStoredTasks(current, nextTasks),
        });
      }

      return {
        deleted,
        tasks: nextTasks,
      };
    });
  };

  /**
   * 无条件把 statePatch 合并进任务 state（经 normalizeState 归一后写盘），
   * 用于一次运行结束后回写结果。任务不存在返回 updated:false 且不写盘。
   */
  const updateScheduledTaskState = async (projectID, taskID, statePatch) => {
    return withProjectWriteLock(projectID, async () => {
      const normalizedTaskID = asNonEmptyString(taskID);
      if (!normalizedTaskID) {
        throw new Error('taskId is required');
      }

      const current = await readProjectConfigFromDisk(projectID);
      const taskIndex = current.scheduledTasks.findIndex((task) => task.id === normalizedTaskID);
      if (taskIndex === -1) {
        return { task: null, tasks: current.scheduledTasks, updated: false };
      }

      const currentTask = current.scheduledTasks[taskIndex];
      const patchObject = statePatch && typeof statePatch === 'object' ? statePatch : {};
      const nextTask = {
        ...currentTask,
        state: normalizeState(
          {
            ...currentTask.state,
            ...patchObject,
            updatedAt: Date.now(),
          },
          currentTask.state,
        ),
      };

      const nextTasks = current.scheduledTasks.slice();
      nextTasks[taskIndex] = nextTask;

      await writeProjectConfigToDisk(projectID, {
        version: PROJECT_CONFIG_VERSION,
        scheduledTasks: toStoredTasks(current, nextTasks, { stateUpdatedID: nextTask.id }),
      });

      return {
        task: nextTask,
        tasks: nextTasks,
        updated: true,
      };
    });
  };

  /**
   * Conditionally patch task runtime state under the project write lock.
   * `predicate(currentTask)` is evaluated after the latest on-disk read; when
   * it returns false the write is skipped and `{ updated: false }` is returned.
   * Used by the scheduled-tasks runtime to claim a single schedule occurrence
   * across concurrent OMPChamber server instances.
   */
  /**
   * 中文说明：state 更新的条件版本——先读取最新任务，predicate 通过
   * 才应用 statePatch，否则跳过写盘并返回 updated:false。调度器用它
   * 认领一次调度触发（写入 lastScheduledFor），保证多个共享同一配置
   * 文件的 OMPChamber 实例不会就同一时隙各跑一次。
   */
  const updateScheduledTaskStateIf = async (projectID, taskID, predicate, statePatch) => {
    return withProjectWriteLock(projectID, async () => {
      const normalizedTaskID = asNonEmptyString(taskID);
      if (!normalizedTaskID) {
        throw new Error('taskId is required');
      }
      if (typeof predicate !== 'function') {
        throw new Error('predicate is required');
      }

      const current = await readProjectConfigFromDisk(projectID);
      const taskIndex = current.scheduledTasks.findIndex((task) => task.id === normalizedTaskID);
      if (taskIndex === -1) {
        return { task: null, tasks: current.scheduledTasks, updated: false };
      }

      const currentTask = current.scheduledTasks[taskIndex];
      if (!predicate(currentTask)) {
        return {
          task: currentTask,
          tasks: current.scheduledTasks,
          updated: false,
        };
      }

      const patchObject = statePatch && typeof statePatch === 'object' ? statePatch : {};
      const nextTask = {
        ...currentTask,
        state: normalizeState(
          {
            ...currentTask.state,
            ...patchObject,
            updatedAt: Date.now(),
          },
          currentTask.state,
        ),
      };

      const nextTasks = current.scheduledTasks.slice();
      nextTasks[taskIndex] = nextTask;

      await writeProjectConfigToDisk(projectID, {
        version: PROJECT_CONFIG_VERSION,
        scheduledTasks: toStoredTasks(current, nextTasks, { stateUpdatedID: nextTask.id }),
      });

      return {
        task: nextTask,
        tasks: nextTasks,
        updated: true,
      };
    });
  };

  /**
   * Reconcile discovered `.agents/loops` definitions with the persisted JSON
   * task list.
   *
   * Rules (documented in scheduled-tasks/DOCUMENTATION.md):
   * - For loop-owned tasks (carrying the `loopFile` marker) identity is the
   *   LOOP FILE PATH: a loop takes its task over regardless of the task's
   *   current name, so renaming the loop (`name` field or a UI edit) renames
   *   the task in place instead of leaving a stale duplicate behind.
   * - A loop whose name matches a JSON task (no `loopFile`) takes that task
   *   over: its schedule/execution/enabled are overwritten from the file while
   *   its id and runtime state are preserved (markdown wins on conflict).
   *   Execution fields the file format does not define (goalEnabled,
   *   goalTokenBudget, permissionAutoAccept, variant) are preserved.
   * - A task whose loopFile no longer matches any discovered loop file is
   *   unscheduled (removed). JSON-configured tasks (no loopFile) are never
   *   removed.
   * - A task whose loop file still exists but is currently unparseable is
   *   KEPT with its last good definition: only a genuinely removed file
   *   unschedules a task, so transiently malformed files (mid-edit, bad
   *   merge) never delete tasks or their runtime state.
   * - Loops with no matching task are created under a deterministic
   *   `loop:<scope>:<name>` id, so runtime state survives restarts.
   * - Malformed definitions are skipped with a warning and never block valid
   *   loops; the scheduler passes them as `definition: null` entries, and
   *   normalization failures here are isolated per loop.
   */
  /**
   * 中文说明：对账实现——按 loop 文件路径认领既有 loop 任务（重命名
   * 不留副本）、按名称认领同名 JSON 任务（保留 id/state 与文件格式未
   * 覆盖的 UI 字段）、为无主 loop 创建确定性 `loop:<scope>:<name>` id、
   * 清理文件已删除的任务与同名孤儿副本；全程持有写锁，返回对账后的
   * 完整任务列表。
   */
  const reconcileLoopTasks = async (projectID, loops) => {
    return withProjectWriteLock(projectID, async () => {
      const now = Date.now();
      const current = await readProjectConfigFromDisk(projectID);
      const tasks = current.scheduledTasks;

      const activeLoopFilePaths = new Set();
      const pendingLoops = new Map();
      const loopsByPath = new Map();
      for (const loop of loops) {
        if (!loop || typeof loop.filePath !== 'string' || !loop.filePath) {
          continue;
        }
        activeLoopFilePaths.add(loop.filePath);
        if (loop.definition && typeof loop.definition === 'object') {
          pendingLoops.set(loop.definition.name, loop);
          loopsByPath.set(loop.filePath, loop);
        }
      }

      const consumedLoopPaths = new Set();
      const nextTasks = [];
      const replacedIDs = new Set();
      for (const task of tasks) {
        if (task.loopFile && !activeLoopFilePaths.has(task.loopFile)) {
          // The driving loop file was removed (or renamed) — unschedule.
          continue;
        }

        // Loop-owned tasks adopt by file path (covers renames of the `name`
        // field); JSON tasks adopt by name.
        const loop = task.loopFile
          ? loopsByPath.get(task.loopFile) || null
          : pendingLoops.get(task.name) || null;
        if (loop) {
          try {
            const adopted = normalizeTaskForStorage(
              {
                ...task,
                ...loop.definition,
                // File-defined execution fields win; UI-only fields the file
                // format does not define are preserved from the task.
                execution: { ...task.execution, ...loop.definition.execution },
                loopFile: loop.filePath,
              },
              {
                now,
                createId: taskIDFactory,
                existingTask: task,
                allowCreate: false,
                refreshUpdatedAt: false,
              },
            );
            nextTasks.push(adopted);
            replacedIDs.add(adopted.id);
            pendingLoops.delete(loop.definition.name);
            if (task.loopFile) {
              consumedLoopPaths.add(task.loopFile);
              loopsByPath.delete(task.loopFile);
            }
          } catch (error) {
            console.warn(`[scheduled-tasks] skipped loop ${loop.filePath} for task "${task.name}":`, error?.message ?? error);
            nextTasks.push(task);
          }
          continue;
        }

        if (task.loopFile && consumedLoopPaths.has(task.loopFile)) {
          // Orphan duplicate: another task already adopted this loop file
          // (left over from a rename) — unschedule it.
          continue;
        }

        nextTasks.push(task);
      }

      for (const loop of pendingLoops.values()) {
        try {
          const id = `loop:${loop.scope}:${loop.definition.name}`;
          const created = normalizeTaskForStorage(
            { id, ...loop.definition, loopFile: loop.filePath },
            {
              now,
              createId: taskIDFactory,
              existingTask: null,
              allowCreate: true,
              refreshUpdatedAt: false,
            },
          );
          nextTasks.push(created);
          replacedIDs.add(created.id);
        } catch (error) {
          console.warn(`[scheduled-tasks] skipped loop ${loop.filePath}:`, error?.message ?? error);
        }
      }

      await writeProjectConfigToDisk(projectID, {
        version: PROJECT_CONFIG_VERSION,
        scheduledTasks: toStoredTasks(current, nextTasks, { replacedIDs }),
      });

      return nextTasks;
    });
  };

  return {
    listScheduledTasks,
    upsertScheduledTask,
    deleteScheduledTask,
    updateScheduledTaskState,
    updateScheduledTaskStateIf,
    reconcileLoopTasks,
    resolveProjectConfigPath,
  };
};
