import fs from 'node:fs';
import path from 'node:path';

/**
 * Worktree topology watcher.
 *
 * Watches the git metadata of every registered project for linked-worktree
 * creation and removal, regardless of who performed it (OMPChamber's own
 * worktree routes, the CLI, a terminal `git worktree add`, or another client).
 * Each linked worktree is registered under the repository's common git dir as
 * `.git/worktrees/<name>`, so watching that one directory per repository
 * observes every worktree topology change for the repo. The watcher never
 * interprets the topology: it reports the affected registered project paths
 * and the client re-lists worktrees authoritatively via `git worktree list`.
 *
 * Deliberate boundaries:
 * - A worktree checkout directory deleted without `git worktree remove`/
 *   `prune` leaves the metadata entry in place, so it fires no event here.
 *   The existing per-worktree "missing folder" status covers that case.
 * - Watch failures (repo moved/deleted, FS watch limits) close that repo's
 *   watchers and retry a bounded number of times; discovery then falls back to
 *   the client's existing pull-based worktree listing.
 * - The waiting-mode registry watcher relies on fs.watch reporting event
 *   filenames; platforms that omit them degrade to pull-based discovery until
 *   the registry already exists (worktrees mode filters nothing).
 */
/**
 * @module git/worktree-watcher
 * worktree 拓扑 watcher（上方英文总览的中文说明）。
 *
 * 监视每个已注册项目的 git 元数据目录，捕捉 linked worktree 的创建与删除
 * ——无论操作来自 OMPChamber 自身的 worktree 路由、CLI、终端里的
 * `git worktree add` 还是其它客户端。每个 linked worktree 都登记在仓库
 * 公共 git 目录的 `.git/worktrees/<name>` 下，因此每个仓库只需 watch 这
 * 一个目录即可覆盖全部拓扑变化。watcher 不解释拓扑：只上报受影响的已
 * 注册项目路径，由客户端通过 `git worktree list` 重新拉取权威列表。
 *
 * 明确的边界（细节见上方英文说明）：不经 `git worktree remove`/`prune`
 * 直接删除检出目录不会触发事件，由既有的"目录缺失"状态兜底；watch 失败
 * 按有界次数重试后放弃，退化为客户端轮询；waiting 模式依赖 fs.watch 上报
 * 事件文件名，不支持的平台上同样退化为轮询。
 */

/** 从 linked worktree 的 `.git` 指针文件内容中提取 `gitdir: <path>` 目标路径的正则（跨多行取首个匹配）。 */
const PARSE_GITDIR_PATTERN = /^gitdir:\s*(.+?)\s*$/m;
/** 单个仓库 watcher 失败后的最大重新布防（re-arm）次数；超过即放弃，直到项目列表再次变化。 */
const MAX_REARM_ATTEMPTS = 5;

/** Extract the gitdir target from a linked-worktree `.git` file. */
/** 解析 `.git` 指针文件内容，返回其中 gitdir 目标路径；格式不符或无匹配返回 null。 */
const parseGitdirTarget = (content) => {
  const match = PARSE_GITDIR_PATTERN.exec(content);
  return match ? match[1] : null;
};

/**
 * Resolve the common git dir that owns a repository's worktree registry.
 * - main repository: `<project>/.git` (a directory)
 * - linked worktree: `<project>/.git` is a file pointing at
 *   `<main>/.git/worktrees/<name>`; the common dir is that path's grandparent
 * Returns null for non-repositories and unreadable layouts.
 */
/**
 * （中文）解析持有 worktree 注册表的公共 git 目录：
 * - 主仓库：直接返回 `<project>/.git` 目录本身；
 * - linked worktree：`.git` 是指向 `<主仓库>/.git/worktrees/<name>` 的
 *   文件，公共目录为该路径的上两级（即 `<主仓库>/.git`）。
 * 非仓库、`.git` 既非目录也非文件、内容不可读或 gitdir 指向的布局不认识时
 * 返回 null，调用方据此跳过对该项目的监视。
 */
export const resolveGitCommonDir = (projectPath) => {
  const dotGitPath = path.join(projectPath, '.git');
  let stat;
  try {
    stat = fs.statSync(dotGitPath);
  } catch {
    return null;
  }
  if (stat.isDirectory()) {
    return dotGitPath;
  }
  if (!stat.isFile()) {
    return null;
  }
  let content;
  try {
    content = fs.readFileSync(dotGitPath, 'utf8');
  } catch {
    return null;
  }
  const gitdir = parseGitdirTarget(content);
  if (!gitdir) {
    return null;
  }
  const gitdirPath = path.resolve(projectPath, gitdir);
  if (path.basename(path.dirname(gitdirPath)) === 'worktrees') {
    return path.dirname(path.dirname(gitdirPath));
  }
  return gitdirPath;
};

/**
 * 创建 worktree 拓扑 watcher 实例。
 *
 * 同时监视两类目标：每个仓库的公共 git 目录（以及存在时的 worktrees 注册
 * 表目录本身），和 settings 文件所在目录（项目列表增删触发重扫描）。事件
 * 经防抖合并后回调 onWorktreesChanged，参数为受影响的已注册项目路径数组；
 * watcher 自身不区分是谁改的拓扑。
 *
 * @param {() => Promise<Array<{path: string}>>} listProjects 获取当前已注册项目列表（路径取 project.path）
 * @param {string|null} settingsFilePath settings 文件绝对路径；null 时不监视项目列表变化
 * @param {(projectPaths: string[]) => void} onWorktreesChanged 拓扑变化回调（同一仓库多路径一次上报，已防抖）
 * @param {object} [logger] 日志对象（默认 console），仅使用 warn；logger 抛错会被吞掉
 * @param {number} [debounceMs] 变更上报的防抖窗口，默认 500ms
 * @param {number} [settingsRescanDelayMs] settings 变化后延迟重扫描的时间，默认 400ms（等待写入完成）
 * @param {number} [rearmDelayMs] watcher 失败后的重新布防间隔，默认 5s
 * @returns {{start: () => Promise, stop: () => void, rescan: () => Promise}} watcher 控制句柄
 */
export const createWorktreeWatcher = ({
  listProjects,
  settingsFilePath,
  onWorktreesChanged,
  logger = console,
  debounceMs = 500,
  settingsRescanDelayMs = 400,
  rearmDelayMs = 5_000,
}) => {
  // common git dir -> watch state for every registered project of that repo.
  // （中文）watcher 状态块：repoEntries 按公共 git 目录聚合各注册项目的
  // watch 状态；其余为 settings watcher、延迟重扫描/重挂定时器、串行化
  // 重扫描的 Promise 链与 dispose 标记。
  const repoEntries = new Map();
  let settingsWatcher = null;
  let settingsRescanTimer = null;
  let settingsRearmTimer = null;
  let rescanChain = Promise.resolve();
  let disposed = false;

  /** 输出一条带 [WorktreeWatcher] 前缀的警告日志；logger 自身抛错时静默，日志绝不影响监视流程。 */
  const logWarn = (message, detail) => {
    try {
      logger.warn(`[WorktreeWatcher] ${message}`, detail ?? '');
    } catch {
      // logging must never break watching
    }
  };

  /** 停用一个仓库条目：置 stopped 标记、清空 emit/rearm 定时器，并关闭其名下全部 fs.watch watcher。 */
  const disposeRepoEntry = (entry) => {
    entry.stopped = true;
    if (entry.emitTimer) {
      clearTimeout(entry.emitTimer);
      entry.emitTimer = null;
    }
    if (entry.rearmTimer) {
      clearTimeout(entry.rearmTimer);
      entry.rearmTimer = null;
    }
    for (const watcher of entry.watchers) {
      try {
        watcher.close();
      } catch {
        // already closed
      }
    }
    entry.watchers.clear();
  };

  /**
   * 为某仓库安排一次防抖上报：debounceMs 窗口内的多次事件只触发一次
   * onWorktreesChanged（携带该仓库全部受影响项目路径）。已 dispose、已停用
   * 或已无归属项目时直接跳过；回调抛错只记警告不上抛。定时器 unref，
   * 不阻止进程退出。
   */
  const emitFor = (entry) => {
    if (disposed || entry.stopped || entry.projectPaths.size === 0) return;
    if (entry.emitTimer) return; // burst already coalescing
    entry.emitTimer = setTimeout(() => {
      entry.emitTimer = null;
      if (disposed || entry.stopped) return;
      try {
        onWorktreesChanged([...entry.projectPaths]);
      } catch (error) {
        logWarn('change listener failed:', error?.message || error);
      }
    }, debounceMs);
    entry.emitTimer.unref?.();
  };

  /**
   * 对单个目录建立 fs.watch 并登记到条目：事件经可选 filenameFilter 过滤后
   * 上报；error 触发有界重试（scheduleRearm），close 时从条目移除。
   * filenameFilter 还承担"注册表本身出现/消失"的语义：匹配到该名字时先
   * 重新布防（armRepo）再上报。建立失败（目录不存在、权限等）返回 false。
   */
  const watchDirectory = (entry, dirPath, filenameFilter) => {
    try {
      const watcher = fs.watch(dirPath, (event, filename) => {
        if (disposed || entry.stopped) return;
        if (filenameFilter && (!filename || path.basename(filename) !== filenameFilter)) return;
        if (filenameFilter) {
          // The worktrees registry itself appeared or disappeared: re-arm so
          // the direct registry watcher tracks the new state, then report it.
          armRepo(entry);
        }
        emitFor(entry);
      });
      watcher.on('error', () => {
        if (disposed || entry.stopped) return;
        scheduleRearm(entry);
      });
      watcher.on('close', () => {
        entry.watchers.delete(watcher);
      });
      entry.watchers.add(watcher);
      return true;
    } catch {
      return false;
    }
  };

  /**
   * Arm one repository's watchers.
   * - `worktrees` metadata dir present: watch it directly for entry
   *   add/remove (the actual topology events).
   * - always watch the common git dir filtered to the `worktrees` name so the
   *   registry's appearance (first linked worktree) and disappearance (last
   *   one pruned) are observed too.
   * Returns false when any desired watcher could not be armed.
   */
  /**
   * （中文）为一个仓库布防 watcher：先关闭并清空旧 watcher；worktrees 注册
   * 表目录存在时进入 worktrees 模式直接监视它（真正的拓扑事件），否则进入
   * waiting 模式等待注册表出现。两种模式都额外保留一个过滤到 `worktrees`
   * 名字的公共目录 watcher——注册表整体删除只会在其父目录上表现为事件。
   * 任一期望的 watcher 建立失败返回 false（调用方据此重试）。
   */
  const armRepo = (entry) => {
    if (disposed || entry.stopped) return true;
    for (const watcher of entry.watchers) {
      try {
        watcher.close();
      } catch {
        // already closed
      }
    }
    entry.watchers.clear();
    const worktreesDir = path.join(entry.commonGitDir, 'worktrees');
    let failed = false;

    if (fs.existsSync(worktreesDir)) {
      entry.mode = 'worktrees';
      if (!watchDirectory(entry, worktreesDir, null)) failed = true;
    } else {
      entry.mode = 'waiting';
    }
    // The common-dir watcher is kept in both modes: deleting the whole
    // worktrees registry only surfaces as an event on its parent, and the
    // filter keeps the busy `.git` churn to one basename compare per event.
    if (!watchDirectory(entry, entry.commonGitDir, 'worktrees')) failed = true;
    return !failed;
  };

  /** watcher 失败后按 rearmDelayMs 间隔重新布防；累计失败超过 MAX_REARM_ATTEMPTS 次记日志放弃，直到项目列表变化重置计数。 */
  const scheduleRearm = (entry) => {
    if (disposed || entry.stopped || entry.rearmTimer) return;
    entry.failureCount = (entry.failureCount ?? 0) + 1;
    if (entry.failureCount > MAX_REARM_ATTEMPTS) {
      if (!entry.gaveUp) {
        entry.gaveUp = true;
        logWarn(`giving up watching ${entry.commonGitDir} until the project list changes`);
      }
      return;
    }
    entry.rearmTimer = setTimeout(() => {
      entry.rearmTimer = null;
      if (disposed || entry.stopped) return;
      if (armRepo(entry)) {
        entry.failureCount = 0;
        entry.gaveUp = false;
      } else {
        scheduleRearm(entry);
      }
    }, rearmDelayMs);
    entry.rearmTimer.unref?.();
  };

  /** 关闭 settings 目录 watcher 并置空（幂等；已关闭导致的异常忽略）。 */
  const closeSettingsWatcher = () => {
    if (!settingsWatcher) return;
    try {
      settingsWatcher.close();
    } catch {
      // already closed
    }
    settingsWatcher = null;
  };

  /** settings watcher 失效后的定时重挂：rearmDelayMs 后再试 armSettingsWatcher，同一时刻只排一个。 */
  const scheduleSettingsRearm = () => {
    if (disposed || settingsRearmTimer) return;
    settingsRearmTimer = setTimeout(() => {
      settingsRearmTimer = null;
      armSettingsWatcher();
    }, rearmDelayMs);
    settingsRearmTimer.unref?.();
  };

  /** settings 文件变化后延迟 settingsRescanDelayMs 再重扫描；窗口内的连续写入合并为一次。 */
  const scheduleSettingsRescan = () => {
    if (disposed || settingsRescanTimer) return;
    settingsRescanTimer = setTimeout(() => {
      settingsRescanTimer = null;
      rescan();
    }, settingsRescanDelayMs);
    settingsRescanTimer.unref?.();
  };

  /**
   * 挂载 settings 文件所在目录的 fs.watch：只响应该文件名的变化并触发
   * 延迟重扫描。error/close（目录被删、watch 上限等）时置空当前 watcher
   * 并安排重挂；fs.watch 直接抛错时同样走重挂重试。
   */
  const armSettingsWatcher = () => {
    if (disposed || !settingsFilePath || settingsWatcher) return;
    const settingsDir = path.dirname(settingsFilePath);
    const settingsFileName = path.basename(settingsFilePath);
    try {
      const watcher = fs.watch(settingsDir, (_event, filename) => {
        if (!filename || path.basename(filename) !== settingsFileName) return;
        scheduleSettingsRescan();
      });
      const onInactive = () => {
        if (settingsWatcher !== watcher) return;
        settingsWatcher = null;
        scheduleSettingsRearm();
      };
      watcher.on('error', onInactive);
      watcher.on('close', onInactive);
      settingsWatcher = watcher;
    } catch {
      scheduleSettingsRearm();
    }
  };

  /** Reconcile watchers with the current registered project list. */
  /**
   * （中文）把 watcher 状态与最新项目列表对账：按公共 git 目录给项目分组；
   * 整组消失的仓库条目直接释放；路径集合变化但目录未变的条目只更新归属
   * （watcher 留用并重置失败计数）；新条目登记后立即布防，失败转重试。
   * 全程串在 rescanChain Promise 链上避免并发重扫描互相踩踏；
   * listProjects 失败时保留现有 watcher 不动。
   */
  const rescan = () => {
    rescanChain = rescanChain
      .then(async () => {
        if (disposed) return;
        let projects = [];
        try {
          projects = await listProjects();
        } catch (error) {
          logWarn('listProjects failed; keeping current watchers:', error?.message || error);
          return;
        }

        // listProjects awaited; stop() may have run meanwhile.
        if (disposed) return;
        const projectPathsByCommonDir = new Map();
        for (const project of Array.isArray(projects) ? projects : []) {
          const rawPath = typeof project?.path === 'string' ? project.path.trim() : '';
          if (!rawPath) continue;
          const projectPath = path.resolve(rawPath);
          const commonGitDir = resolveGitCommonDir(projectPath);
          if (!commonGitDir) continue;
          if (!projectPathsByCommonDir.has(commonGitDir)) {
            projectPathsByCommonDir.set(commonGitDir, new Set());
          }
          projectPathsByCommonDir.get(commonGitDir).add(projectPath);
        }

        for (const [commonGitDir, entry] of repoEntries) {
          if (!projectPathsByCommonDir.has(commonGitDir)) {
            disposeRepoEntry(entry);
            repoEntries.delete(commonGitDir);
          }
        }
        for (const [commonGitDir, projectPaths] of projectPathsByCommonDir) {
          const entry = repoEntries.get(commonGitDir);
          if (entry) {
            const samePaths =
              entry.projectPaths.size === projectPaths.size &&
              [...projectPaths].every((projectPath) => entry.projectPaths.has(projectPath));
            if (samePaths) continue;
            entry.projectPaths = projectPaths;
            // Attribution changed but the watched directory did not; the
            // existing watchers stay armed and pick up the new paths.
            entry.gaveUp = false;
            entry.failureCount = 0;
            continue;
          }
          const nextEntry = {
            commonGitDir,
            projectPaths,
            watchers: new Set(),
            emitTimer: null,
            rearmTimer: null,
            mode: 'waiting',
            failureCount: 0,
            gaveUp: false,
            stopped: false,
          };
          repoEntries.set(commonGitDir, nextEntry);
          if (!armRepo(nextEntry)) {
            scheduleRearm(nextEntry);
          }
        }
      })
      .catch((error) => {
        logWarn('rescan failed:', error?.message || error);
      });
    return rescanChain;
  };

  return {
    // 启动：挂载 settings watcher 并执行一次重扫描（已 dispose 时直接返回空 Promise）。
    start: () => {
      if (disposed) return Promise.resolve();
      armSettingsWatcher();
      return rescan();
    },
    // 停止：置 disposed 标记，释放全部仓库条目、settings watcher 与两个 settings 定时器；幂等。
    stop: () => {
      disposed = true;
      for (const entry of repoEntries.values()) {
        disposeRepoEntry(entry);
      }
      repoEntries.clear();
      closeSettingsWatcher();
      if (settingsRescanTimer) {
        clearTimeout(settingsRescanTimer);
        settingsRescanTimer = null;
      }
      if (settingsRearmTimer) {
        clearTimeout(settingsRearmTimer);
        settingsRearmTimer = null;
      }
    },
    // 手动触发一次重扫描（与 settings 变化触发的路径相同）。
    rescan,
  };
};
