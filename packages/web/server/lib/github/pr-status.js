/**
 * GitHub PR 状态解析模块。
 *
 * 负责把本地 worktree 分支解析为对应的 GitHub PR：从目录的 git remotes 出发，
 * 沿 fork 网络（parent/source）扩展候选仓库，按优先级在其中查找该分支的
 * open PR 或最近一条 closed/merged 历史 PR，并附带仓库默认分支等元信息。
 * 内部维护多级 TTL 缓存（repo 级 PR 列表、默认分支、repo 元数据、历史 PR、
 * 搜索未命中）以摊平轮询之间的 GitHub API 调用；遇到 rate limit 时经
 * rate-limit.js 的全局冷却门避让。模块入口为 resolveGitHubPrStatus。
 */
import { stat } from 'node:fs/promises';
import { getRemotes, getStatus } from '../git/index.js';
import { resolveGitHubRepoFromDirectory } from './repo/index.js';
import { noteIfGitHubRateLimit } from './rate-limit.js';

/**
 * 判断目录是否仍然存在（stat 探测）。
 * 已删除的 worktree 仍可能被侧栏会话轮询 PR 状态，先在此拦截，避免对
 * 已消失的路径浪费 git 与 GitHub 调用；任何 stat 错误都按 false 处理。
 */
const directoryExists = async (dir) => {
  if (!dir) return false;
  try {
    await stat(dir);
    return true;
  } catch {
    return false;
  }
};

/** 仓库默认分支缓存的 TTL：5 分钟（见 defaultBranchCache）。 */
const REPO_DEFAULT_BRANCH_TTL_MS = 5 * 60_000;
/** 默认分支缓存：repoKey（小写 owner/repo）→ { defaultBranch, fetchedAt }。 */
const defaultBranchCache = new Map();
/** 仓库完整元数据缓存：repoKey → { data: repos.get 响应, fetchedAt }，TTL 同上。 */
const repoMetadataCache = new Map();

/** 把任意值规整为去除首尾空白的字符串；非字符串一律返回空串。 */
const normalizeText = (value) => typeof value === 'string' ? value.trim() : '';
/** 在 normalizeText 基础上再转小写，用于大小写不敏感的 owner/repo/分支比较。 */
const normalizeLower = (value) => normalizeText(value).toLowerCase();
/**
 * 生成归一化的仓库键 `owner/repo`（两侧 trim + 小写）。
 * 任一侧为空时返回空串，调用方据此跳过无效条目。
 */
const normalizeRepoKey = (owner, repo) => {
  const normalizedOwner = normalizeLower(owner);
  const normalizedRepo = normalizeLower(repo);
  if (!normalizedOwner || !normalizedRepo) {
    return '';
  }
  return `${normalizedOwner}/${normalizedRepo}`;
};
/**
 * 从 git 的 upstream 跟踪引用（如 `origin/feature`）中提取 remote 名。
 * 无 `/` 或 `/` 位于首位时返回空串，表示没有可用的跟踪信息。
 */
const parseTrackingRemoteName = (trackingBranch) => {
  const normalized = normalizeText(trackingBranch);
  if (!normalized) {
    return '';
  }
  const slashIndex = normalized.indexOf('/');
  if (slashIndex <= 0) {
    return '';
  }
  return normalized.slice(0, slashIndex).trim();
};

/**
 * 从 git 的 upstream 跟踪引用（如 `origin/feature`）中提取分支名部分。
 * 格式不合法（无分隔符、以分隔符结尾）时返回空串。
 */
const parseTrackingBranchName = (trackingBranch) => {
  const normalized = normalizeText(trackingBranch);
  if (!normalized) {
    return '';
  }
  const slashIndex = normalized.indexOf('/');
  if (slashIndex <= 0 || slashIndex >= normalized.length - 1) {
    return '';
  }
  return normalized.slice(slashIndex + 1).trim();
};

/**
 * 按 keyFn 归一化键去重后把 value 追加进 collection（原地修改）。
 * 空值或归一化键为空时忽略；已存在相同键的元素时跳过，保证列表无重复。
 */
const pushUnique = (collection, value, keyFn = normalizeLower) => {
  const normalizedValue = normalizeText(value);
  if (!normalizedValue) {
    return;
  }
  const nextKey = keyFn(normalizedValue);
  if (!nextKey) {
    return;
  }
  if (collection.some((item) => keyFn(item) === nextKey)) {
    return;
  }
  collection.push(normalizedValue);
};

/**
 * 为候选 remote 名排序：显式指定的 remote 最先，其次是分支的跟踪 remote，
 * 再是约定俗成的 origin/upstream，最后按 git 配置顺序补齐其余 remote。
 */
const rankRemoteNames = (remoteNames, explicitRemoteName, trackingRemoteName) => {
  const ranked = [];
  pushUnique(ranked, explicitRemoteName);

  if (trackingRemoteName) {
    pushUnique(ranked, trackingRemoteName);
  }

  pushUnique(ranked, 'origin');
  pushUnique(ranked, 'upstream');
  remoteNames.forEach((name) => pushUnique(ranked, name));
  return ranked;
};

/**
 * 提取 PR head 侧的 owner 登录名。
 * 依次尝试 head.repo.owner.login → head.user.login → head.label 冒号前缀，
 * 全部缺失时返回空串。
 */
const getHeadOwner = (pr) => {
  const repoOwner = normalizeText(pr?.head?.repo?.owner?.login);
  if (repoOwner) {
    return repoOwner;
  }
  const userOwner = normalizeText(pr?.head?.user?.login);
  if (userOwner) {
    return userOwner;
  }
  const headLabel = normalizeText(pr?.head?.label);
  const separatorIndex = headLabel.indexOf(':');
  if (separatorIndex > 0) {
    return headLabel.slice(0, separatorIndex).trim();
  }
  return '';
};
/**
 * 提取 PR head 侧的归一化仓库键。
 * 优先 head.repo 的 owner+name；否则用 head.label 的 owner 段配合
 * fallbackRepoName（目标仓库的名）拼出键；无法确定时返回空串。
 */

/** 提取 PR head 侧的归一化仓库键（owner/repo 形式），供来源匹配与排序使用。 */
const getHeadRepoKey = (pr, fallbackRepoName) => {
  const repoOwner = normalizeText(pr?.head?.repo?.owner?.login);
  const repoName = normalizeText(pr?.head?.repo?.name);
  if (repoOwner && repoName) {
    return normalizeRepoKey(repoOwner, repoName);
  }
  const headLabel = normalizeText(pr?.head?.label);
  const separatorIndex = headLabel.indexOf(':');
  if (separatorIndex > 0) {
    const labelOwner = headLabel.slice(0, separatorIndex).trim();
    if (labelOwner && fallbackRepoName) {
      return normalizeRepoKey(labelOwner, fallbackRepoName);
    }
  }
  return '';
};

/**
 * 根据分支真正的来源仓库候选（sourceCandidates，按优先级排序）构建匹配器。
 * matches 判断某个 PR 的 head 是否来自这些来源仓库（先精确匹配 repo 键，
 * 再退化为仅匹配 owner）；compare 按同一优先级给 PR 排序，来源排名越靠前
 * 得分越小，不属于任何来源的 PR 排最后（Number.POSITIVE_INFINITY）。
 */
const buildSourceMatcher = (sourceCandidates) => {
  const repoRank = new Map();
  const ownerRank = new Map();

  sourceCandidates.forEach((candidate, index) => {
    const repoKey = normalizeRepoKey(candidate.repo?.owner, candidate.repo?.repo);
    if (repoKey && !repoRank.has(repoKey)) {
      repoRank.set(repoKey, index);
    }
    const owner = normalizeLower(candidate.repo?.owner);
    if (owner && !ownerRank.has(owner)) {
      ownerRank.set(owner, index);
    }
  });

  const matches = (pr, fallbackRepoName) => {
    const repoKey = getHeadRepoKey(pr, fallbackRepoName);
    if (repoKey && repoRank.has(repoKey)) {
      return true;
    }
    const owner = normalizeLower(getHeadOwner(pr));
    return Boolean(owner) && ownerRank.has(owner);
  };

  const compare = (left, right, fallbackRepoName) => {
    const leftRepoRank = repoRank.get(getHeadRepoKey(left, fallbackRepoName));
    const rightRepoRank = repoRank.get(getHeadRepoKey(right, fallbackRepoName));
    const leftRepoScore = typeof leftRepoRank === 'number' ? leftRepoRank : Number.POSITIVE_INFINITY;
    const rightRepoScore = typeof rightRepoRank === 'number' ? rightRepoRank : Number.POSITIVE_INFINITY;
    if (leftRepoScore !== rightRepoScore) {
      return leftRepoScore - rightRepoScore;
    }

    const leftOwnerRank = ownerRank.get(normalizeLower(getHeadOwner(left)));
    const rightOwnerRank = ownerRank.get(normalizeLower(getHeadOwner(right)));
    const leftOwnerScore = typeof leftOwnerRank === 'number' ? leftOwnerRank : Number.POSITIVE_INFINITY;
    const rightOwnerScore = typeof rightOwnerRank === 'number' ? rightOwnerRank : Number.POSITIVE_INFINITY;
    if (leftOwnerScore !== rightOwnerScore) {
      return leftOwnerScore - rightOwnerScore;
    }

    return 0;
  };

  return { matches, compare };
};

/**
 * 查询仓库默认分支（main/master 等），带 5 分钟 TTL 缓存。
 * 命中 repoMetadataCache 时直接复用元数据，省去一次重复的 repos.get；
 * 请求失败时上报 rate limit 并返回 null（默认分支缺失只影响过滤，不致命）。
 */
const getRepoDefaultBranch = async (octokit, repo) => {
  const repoKey = normalizeRepoKey(repo?.owner, repo?.repo);
  if (!repoKey) {
    return null;
  }

  const cached = defaultBranchCache.get(repoKey);
  if (cached && Date.now() - cached.fetchedAt < REPO_DEFAULT_BRANCH_TTL_MS) {
    return cached.defaultBranch;
  }

  // Reuse the full repo metadata if it was already fetched (expandRepoNetwork
  // calls getRepoMetadata for every candidate before the default-branch loop).
  // This avoids a redundant repos.get per repo — fewer serial GitHub calls means
  // less exposure to secondary-rate-limiting that makes PR status slow.
  const metaCached = repoMetadataCache.get(repoKey);
  if (metaCached && Date.now() - metaCached.fetchedAt < REPO_DEFAULT_BRANCH_TTL_MS) {
    const defaultBranch = normalizeText(metaCached.data?.default_branch) || null;
    defaultBranchCache.set(repoKey, { defaultBranch, fetchedAt: Date.now() });
    return defaultBranch;
  }

  try {
    const response = await octokit.rest.repos.get({
      owner: repo.owner,
      repo: repo.repo,
    });
    const defaultBranch = normalizeText(response?.data?.default_branch) || null;
    defaultBranchCache.set(repoKey, {
      defaultBranch,
      fetchedAt: Date.now(),
    });
    return defaultBranch;
  } catch (error) {
    noteIfGitHubRateLimit(error);
    return null;
  }
};

/**
 * 查询仓库完整元数据（含 fork 的 parent/source），带 TTL 缓存。
 * 403/404 视为"确认拿不到"并缓存 null，短 TTL 内不再重试；其它错误在
 * 上报 rate limit 后原样抛出，由调用方决定是否继续。
 */
const getRepoMetadata = async (octokit, repo) => {
  const repoKey = normalizeRepoKey(repo?.owner, repo?.repo);
  if (!repoKey) {
    return null;
  }

  const cached = repoMetadataCache.get(repoKey);
  if (cached && Date.now() - cached.fetchedAt < REPO_DEFAULT_BRANCH_TTL_MS) {
    return cached.data;
  }

  try {
    const response = await octokit.rest.repos.get({
      owner: repo.owner,
      repo: repo.repo,
    });
    const data = response?.data ?? null;
    repoMetadataCache.set(repoKey, {
      data,
      fetchedAt: Date.now(),
    });
    return data;
  } catch (error) {
    noteIfGitHubRateLimit(error);
    if (error?.status === 403 || error?.status === 404) {
      repoMetadataCache.set(repoKey, {
        data: null,
        fetchedAt: Date.now(),
      });
      return null;
    }
    throw error;
  }
};

/**
 * 并发解析每个候选 remote 对应的 GitHub 仓库，再按传入的排名顺序以
 * 归一化 repoKey 去重。任一 remote 解析失败只贡献 null 并被过滤，
 * 不影响其余候选。
 */
const resolveRemoteCandidates = async (directory, rankedRemoteNames) => {
  // Resolve every ranked remote concurrently — they're independent git lookups.
  // Dedup afterwards in rank order so the result is identical to the previous
  // sequential pass, just without paying each lookup's latency back-to-back.
  const resolvedRemotes = await Promise.all(
    rankedRemoteNames.map((remoteName) =>
      resolveGitHubRepoFromDirectory(directory, remoteName)
        .then((resolved) => ({ remoteName, repo: resolved?.repo || null }))
        .catch(() => ({ remoteName, repo: null })),
    ),
  );

  const results = [];
  const seenRepoKeys = new Set();
  for (const { remoteName, repo } of resolvedRemotes) {
    const repoKey = normalizeRepoKey(repo?.owner, repo?.repo);
    if (!repo || !repoKey || seenRepoKeys.has(repoKey)) {
      continue;
    }
    seenRepoKeys.add(repoKey);
    results.push({ remoteName, repo });
  }

  return results;
};

/**
 * 把 remote 解析出的仓库候选扩展为完整查询目标集：并发拉取各候选的
 * 元数据，把每个仓库连同其 fork parent（priority +0.1）与 source
 * （priority +0.2）一并纳入，按 repoKey 去重后按 priority 升序返回，
 * 使 fork 检出也能查到 upstream 上的 PR。
 */
const expandRepoNetwork = async (octokit, candidates) => {
  const expanded = [];
  const seenRepoKeys = new Set();

  const pushCandidate = (repo, remoteName, priority) => {
    const repoKey = normalizeRepoKey(repo?.owner, repo?.repo);
    if (!repoKey || seenRepoKeys.has(repoKey)) {
      return;
    }
    seenRepoKeys.add(repoKey);
    expanded.push({ repo, remoteName, priority });
  };

  // Fetch repo metadata for all candidates concurrently (independent GET
  // /repos calls), then fold them in candidate order so dedup/priority is
  // unchanged from the sequential version.
  const metadatas = await Promise.all(
    candidates.map((candidate) =>
      getRepoMetadata(octokit, candidate.repo).then((metadata) => ({ candidate, metadata })),
    ),
  );

  for (const { candidate, metadata } of metadatas) {
    if (!metadata) {
      continue;
    }

    pushCandidate(candidate.repo, candidate.remoteName, candidate.priority);

    const parent = metadata?.parent;
    if (parent?.owner?.login && parent?.name) {
      pushCandidate({
        owner: parent.owner.login,
        repo: parent.name,
        url: parent.html_url || `https://github.com/${parent.owner.login}/${parent.name}`,
      }, candidate.remoteName, candidate.priority + 0.1);
    }

    const source = metadata?.source;
    if (source?.owner?.login && source?.name) {
      pushCandidate({
        owner: source.owner.login,
        repo: source.name,
        url: source.html_url || `https://github.com/${source.owner.login}/${source.name}`,
      }, candidate.remoteName, candidate.priority + 0.2);
    }
  }

  return expanded.sort((left, right) => left.priority - right.priority);
};

/**
 * 安全包装 octokit.rest.pulls.list 并返回 data 数组。
 * 404/403（仓库不存在、无权限、rate limit）吞掉并返回空数组——列表缺失
 * 只是减少候选，不应中断整个解析；其它错误继续抛出。
 */
const safeListPulls = async (octokit, options) => {
  try {
    const response = await octokit.rest.pulls.list(options);
    return Array.isArray(response?.data) ? response.data : [];
  } catch (error) {
    noteIfGitHubRateLimit(error);
    if (error?.status === 404 || error?.status === 403) {
      return [];
    }
    throw error;
  }
};

/** repo 级 PR 列表缓存的 TTL：45 秒（见 repoPullsCache）。 */
// Repo-level pull list, shared across every branch resolution. Ten worktree
// branches of one repo need ONE pulls.list per state per TTL window, not ten
// per-branch query fans. In-flight requests coalesce so concurrent branch
// resolutions share a single GitHub call.
const REPO_PULLS_CACHE_TTL_MS = 45_000;
/**
 * repo 级 pull 列表缓存：`owner/repo::state` → 在途 promise 或
 * { fetchedAt, prs, complete } 结果。complete 表示第一页已覆盖全部 PR，
 * 列表未命中即可视为权威结论。
 */
const repoPullsCache = new Map();

/** 命中历史 PR（closed/merged）记录的缓存 TTL：6 小时，此类记录几乎不再变化。 */
// Remembered answer to "what is the newest closed/merged PR for this head?",
// so discovery polls do not re-ask GitHub every few minutes.
//
// A found record barely ever changes: it would take a second PR on the same
// head, and while that one is open the open-PR path wins and never reads this
// cache at all. "No history yet" is the volatile answer, since closing or
// merging a PR elsewhere flips it, so it expires far sooner. Either way, doing
// it from OMPChamber invalidates the entry immediately.
const HISTORICAL_PR_FOUND_TTL_MS = 6 * 60 * 60 * 1000;
/** "该 head 尚无历史 PR"这一易变结论的缓存 TTL：10 分钟。 */
const HISTORICAL_PR_ABSENT_TTL_MS = 10 * 60 * 1000;
/** 历史 PR 缓存的最大条目数，超出后按插入序淘汰最旧条目。 */
const HISTORICAL_PR_CACHE_MAX_ENTRIES = 500;
/** 历史 PR 记忆缓存：`owner/repo::branch` → { pr, fetchedAt }，pr 为 null 表示查过且无历史。 */
const _historicalPrCache = new Map();

/**
 * 判断历史 PR 缓存条目是否仍新鲜：有记录用长 TTL，无记录（pr 为 null）
 * 用短 TTL，与两种结论的稳定性差异对应。
 */
const isHistoricalPrCacheFresh = (entry) => {
  if (!entry) {
    return false;
  }
  const ttl = entry.pr ? HISTORICAL_PR_FOUND_TTL_MS : HISTORICAL_PR_ABSENT_TTL_MS;
  return Date.now() - entry.fetchedAt < ttl;
};

/**
 * 写入历史 PR 缓存（pr 可为 null 表示"无历史"），先 delete 再 set 刷新
 * LRU 位置，超限时淘汰最旧条目。
 */
const rememberHistoricalPr = (key, pr) => {
  _historicalPrCache.delete(key);
  _historicalPrCache.set(key, { pr, fetchedAt: Date.now() });
  if (_historicalPrCache.size > HISTORICAL_PR_CACHE_MAX_ENTRIES) {
    const oldest = _historicalPrCache.keys().next().value;
    if (oldest !== undefined) {
      _historicalPrCache.delete(oldest);
    }
  }
};

/**
 * 作废指定仓库的 PR 相关缓存（在 OMPChamber 内创建 PR、合并、关闭后调用）：
 * repo 级 pull 列表、该仓库的搜索未命中记录、以及历史 PR 记忆，
 * 确保下一轮轮询立即看到最新状态。
 */
export const invalidateRepoPullsCache = (owner, repo) => {
  const prefix = `${normalizeText(owner)}/${normalizeText(repo)}::`;
  for (const key of repoPullsCache.keys()) {
    if (key.startsWith(prefix)) {
      repoPullsCache.delete(key);
    }
  }
  // A just-created PR must also clear remembered search misses for this repo.
  const repoNameLower = normalizeText(repo).toLowerCase();
  for (const key of _searchMissCache.keys()) {
    const [repoPart] = key.split('::');
    if (repoPart && repoPart.split(',').includes(repoNameLower)) {
      _searchMissCache.delete(key);
    }
  }
  // A merge or close changes the branch's PR history, so drop it too.
  const historicalPrefix = `${normalizeRepoKey(owner, repo)}::`;
  for (const key of _historicalPrCache.keys()) {
    if (key.startsWith(historicalPrefix)) {
      _historicalPrCache.delete(key);
    }
  }
};

/**
 * 获取某仓库某 state 的 PR 列表（repo 级共享缓存）。
 * 在途请求会被合并：并发调用共享同一个 promise；命中未过期缓存直接返回；
 * force 为 true 时绕过 TTL 强制刷新。返回 { fetchedAt, prs, complete }，
 * 失败时删除缓存条目并把错误抛给所有共享者。
 */
const getRepoPulls = (octokit, repo, state, { force = false } = {}) => {
  const key = `${normalizeText(repo.owner)}/${normalizeText(repo.repo)}::${state}`;
  const cached = repoPullsCache.get(key);
  if (cached?.promise) {
    return cached.promise;
  }
  if (!force && cached && Date.now() - cached.fetchedAt < REPO_PULLS_CACHE_TTL_MS) {
    return Promise.resolve(cached);
  }

  const promise = safeListPulls(octokit, {
    owner: repo.owner,
    repo: repo.repo,
    state,
    per_page: 100,
  }).then((prs) => {
    // `complete` means the first page held everything, so a miss is
    // authoritative: this repo has no PR in this state for any branch.
    const entry = { fetchedAt: Date.now(), prs, complete: prs.length < 100 };
    repoPullsCache.set(key, entry);
    return entry;
  }).catch((error) => {
    repoPullsCache.delete(key);
    throw error;
  });
  repoPullsCache.set(key, { promise });
  return promise;
};

/**
 * 从 GitHub API URL（如 https://api.github.com/repos/OWNER/REPO）解析出
 * { owner, repo }；路径不是 /repos/OWNER/REPO 结构或 URL 非法时返回 null。
 */
const parseRepoFromApiUrl = (value) => {
  const normalized = normalizeText(value);
  if (!normalized) {
    return null;
  }
  try {
    const url = new URL(normalized);
    const parts = url.pathname.replace(/^\/+/, '').split('/').filter(Boolean);
    if (parts.length < 2 || parts[0] !== 'repos') {
      return null;
    }
    const owner = parts[1];
    const repo = parts[2];
    if (!owner || !repo) {
      return null;
    }
    return { owner, repo };
  } catch {
    return null;
  }
};

/** 记录 Search API 返回 403 的 repo 集合（token 缺少对应 org 权限），键为排序后的 repo 名集合。 */
// Track repos where the GitHub Search API returned 403 (token lacks scope for that org)
const _searchApiDisabledRepos = new Map();
/** Search API 对某 repo 集合被禁用后的重试间隔：5 分钟。 */
const SEARCH_API_RETRY_MS = 5 * 60 * 1000; // retry after 5 minutes

/** 搜索未命中的退避间隔：10 分钟内同一 repo 集合加分支不再重复搜索。 */
// The Search API has its own tiny quota (30/min). A branch that has no PR
// would otherwise re-search on every poll; a miss is extremely unlikely to
// change within minutes, so remember it per repo+branch and back off.
const SEARCH_MISS_RETRY_MS = 10 * 60 * 1000;
/** 搜索未命中缓存的最大条目数，超出后淘汰最旧条目。 */
const SEARCH_MISS_CACHE_MAX_ENTRIES = 500;
/** 搜索未命中缓存：`repoKey::branch` → 上次未命中的时间戳。 */
const _searchMissCache = new Map();

/** 记录一次搜索未命中（delete + set 刷新 LRU 位置并淘汰超限最旧条目）。 */
const rememberSearchMiss = (key) => {
  _searchMissCache.delete(key);
  _searchMissCache.set(key, Date.now());
  if (_searchMissCache.size > SEARCH_MISS_CACHE_MAX_ENTRIES) {
    const oldest = _searchMissCache.keys().next().value;
    if (oldest !== undefined) {
      _searchMissCache.delete(oldest);
    }
  }
};

/**
 * 列表查询全部落空后的 Search API 兜底：按 `is:pr state:open head:branch`
 * 全局搜索，过滤出候选 repo 名内的结果，再逐条 pulls.get 验证 head ref
 * 确为该分支，返回 { repo, pr } 或 null。
 * 为保护 Search API 的小配额（30 次/分钟）：403 的 repo 集合 5 分钟内
 * 不再尝试，404 与完全未命中记入 miss 缓存退避 10 分钟；rate limit
 * 会上报全局冷却门。
 */
const searchFallbackPr = async ({ octokit, branch, repoNames }) => {
  // Build a repo key to check/store 403 status per-repo
  const repoKey = [...repoNames].sort().join(',').toLowerCase();

  // Skip if this repo set returned 403 recently
  const disabledAt = _searchApiDisabledRepos.get(repoKey);
  if (disabledAt && Date.now() - disabledAt < SEARCH_API_RETRY_MS) {
    return null;
  }

  const missKey = `${repoKey}::${normalizeText(branch)}`;
  const missedAt = _searchMissCache.get(missKey);
  if (missedAt && Date.now() - missedAt < SEARCH_MISS_RETRY_MS) {
    return null;
  }

  const normalizedRepoNames = new Set(repoNames.map((name) => normalizeLower(name)).filter(Boolean));

  // The Search API has a tiny quota, so it is only spent on live branch status.
  // Closed/merged history is resolved by the cheaper per-head repo queries.
  let response;
  try {
    response = await octokit.rest.search.issuesAndPullRequests({
      q: `is:pr state:open head:${branch}`,
      per_page: 20,
    });
    // If we get here, search API works for this repo — clear the disabled flag
    _searchApiDisabledRepos.delete(repoKey);
  } catch (error) {
    noteIfGitHubRateLimit(error);
    if (error?.status === 403) {
      _searchApiDisabledRepos.set(repoKey, Date.now());
      return null;
    }
    if (error?.status === 404) {
      rememberSearchMiss(missKey);
      return null;
    }
    throw error;
  }

  const items = Array.isArray(response?.data?.items) ? response.data.items : [];
  for (const item of items) {
    const repo = parseRepoFromApiUrl(item?.repository_url);
    if (!repo) {
      continue;
    }
    if (normalizedRepoNames.size > 0 && !normalizedRepoNames.has(normalizeLower(repo.repo))) {
      continue;
    }
    try {
      const prResponse = await octokit.rest.pulls.get({
        owner: repo.owner,
        repo: repo.repo,
        pull_number: item.number,
      });
      const pr = prResponse?.data;
      if (!pr || normalizeText(pr.head?.ref) !== branch) {
        continue;
      }
      return {
        repo: {
          owner: repo.owner,
          repo: repo.repo,
          url: `https://github.com/${repo.owner}/${repo.repo}`,
        },
        pr,
      };
    } catch (error) {
      if (error?.status === 403 || error?.status === 404) {
        continue;
      }
      throw error;
    }
  }

  rememberSearchMiss(missKey);
  return null;
};

/** 判断 PR 是否已终局（closed 或已 merged），用于区分实时状态与历史记录。 */
const isTerminalPr = (pr) => Boolean(pr) && (pr.state === 'closed' || Boolean(pr.merged_at));

/**
 * 在单个目标仓库中解析分支关联的 PR，返回 { open, historical }：
 * open 是该分支的实时状态，historical 是同一 head 最近一条 closed/merged
 * 的 PR。调用方必须让"任意目标的 open PR"优先于"历史 PR"，否则已合并的
 * fork PR 会遮住 upstream 上仍 open 的 PR。includeHistory 默认关闭且仅
 * 应对主目标开启——实时状态值得搜索整个 fork 网络，历史不值得。
 */
/**
 * Resolve the PRs a branch is associated with in one repo target.
 *
 * Returns both candidates because they answer different questions:
 * `open` is live branch status, `historical` is the last closed/merged PR for
 * the same head. The caller must prefer an open PR from ANY target over a
 * historical one — otherwise a merged fork PR hides an open upstream PR.
 *
 * `includeHistory` is off by default and must stay that way for secondary
 * targets. Live status is worth searching the whole fork network for; history
 * is not, and doing it per target multiplied the serial GitHub calls until the
 * route hit its resolve timeout and reported no status at all.
 */
const findBranchPrCandidates = async ({ octokit, target, branch, sourceCandidates, force = false, coverage = null, includeHistory = false }) => {
  const matcher = buildSourceMatcher(sourceCandidates);
  const sourceOwners = [];
  sourceCandidates.forEach((candidate) => pushUnique(sourceOwners, candidate.repo?.owner));

  const pickPreferred = (prs) => prs
    .filter((pr) => normalizeText(pr?.head?.ref) === branch)
    .filter((pr) => matcher.matches(pr, target.repo.repo))
    .sort((left, right) => matcher.compare(left, right, target.repo.repo))[0] ?? null;

  // The shared repo-level open list answers every branch of the repo within the
  // TTL. A miss in a complete list is authoritative: no open PR exists here.
  let openListWasComplete = false;
  try {
    const listEntry = await getRepoPulls(octokit, target.repo, 'open', { force });
    const fromList = pickPreferred(listEntry.prs);
    if (fromList) {
      return { open: fromList, historical: null };
    }
    openListWasComplete = listEntry.complete;
  } catch {
    // fall through to the precise per-head queries
  }

  if (!openListWasComplete && coverage) {
    coverage.authoritative = false;
  }

  // A complete open list already proved there is no open PR in this repo. With
  // no history to look up there is nothing left to ask GitHub.
  if (openListWasComplete && !includeHistory) {
    return { open: null, historical: null };
  }

  const historicalKey = `${normalizeRepoKey(target.repo?.owner, target.repo?.repo)}::${branch}`;
  if (includeHistory && !force && openListWasComplete) {
    const cached = _historicalPrCache.get(historicalKey);
    if (isHistoricalPrCacheFresh(cached)) {
      return { open: null, historical: cached.pr };
    }
  }

  // One query per source owner. With history enabled `state: 'all'` answers
  // both questions at once, so asking for history never costs an extra call.
  let historical = null;
  for (const owner of sourceOwners) {
    const directCandidates = await safeListPulls(octokit, {
      owner: target.repo.owner,
      repo: target.repo.repo,
      state: includeHistory ? 'all' : 'open',
      head: `${owner}:${branch}`,
      per_page: 100,
    });
    const openMatch = pickPreferred(directCandidates.filter((pr) => !isTerminalPr(pr)));
    if (openMatch) {
      return { open: openMatch, historical: null };
    }
    if (includeHistory && !historical) {
      // Among past PRs for the same head the newest one is the relevant record.
      historical = directCandidates
        .filter((pr) => normalizeText(pr?.head?.ref) === branch)
        .filter((pr) => matcher.matches(pr, target.repo.repo))
        .filter(isTerminalPr)
        .sort((left, right) => (right?.number ?? 0) - (left?.number ?? 0))[0] ?? null;
    }
  }

  if (includeHistory) {
    rememberHistoricalPr(historicalKey, historical);
  }
  return { open: null, historical };
};

// Exported for focused unit tests of open-versus-historical branch matching.
export { findBranchPrCandidates };

/**
 * 解析某 worktree 目录某分支的 GitHub PR 状态（模块主入口）。
 *
 * 流程：确认目录存在 → 并发读取 git status/remotes → 由显式 remote、
 * 跟踪 remote、origin/upstream 与全部 remote 排出候选 → 解析并沿 fork
 * 网络扩展目标仓库 → 逐目标、逐分支候选（本地分支名与跟踪分支名）查找
 * PR，无跨仓库来源时跳过默认分支 → 列表结论不权威时再用 Search API 兜底。
 *
 * 返回 { repo, pr, defaultBranch, resolvedRemoteName }：pr 优先为 open PR，
 * 其次为首个找到的历史 PR，最后退化为无 PR 的仓库信息；目录不存在或
 * 无可解析目标时各字段为 null。
 */
export async function resolveGitHubPrStatus({ octokit, directory, branch, remoteName, force = false }) {
  // A deleted worktree can still have a session in the sidebar that keeps
  // requesting its PR status. Bail before touching git or GitHub for a
  // directory that no longer exists — otherwise every poll spends a git call
  // (and the remote/repo resolution that follows) on a path that's gone.
  if (!(await directoryExists(directory))) {
    return { repo: null, pr: null, defaultBranch: null, resolvedRemoteName: null };
  }

  const normalizedBranch = normalizeText(branch);
  const normalizedRemoteName = normalizeText(remoteName) || 'origin';

  const [status, remotes] = await Promise.all([
    getStatus(directory).catch(() => null),
    getRemotes(directory).catch(() => []),
  ]);

  const trackingRemoteName = parseTrackingRemoteName(status?.tracking);
  const trackingBranchName = parseTrackingBranchName(status?.tracking);
  const branchCandidates = [];
  pushUnique(branchCandidates, normalizedBranch);
  pushUnique(branchCandidates, trackingBranchName);
  const rankedRemoteNames = rankRemoteNames(
    Array.isArray(remotes) ? remotes.map((remote) => remote?.name).filter(Boolean) : [],
    normalizedRemoteName,
    trackingRemoteName,
  );

  const resolvedRemoteTargets = await resolveRemoteCandidates(directory, rankedRemoteNames);
  const resolvedTargets = await expandRepoNetwork(
    octokit,
    resolvedRemoteTargets.map((target, index) => ({ ...target, priority: index })),
  );
  if (resolvedTargets.length === 0) {
    return {
      repo: null,
      pr: null,
      defaultBranch: null,
      resolvedRemoteName: null,
    };
  }

  // Only the repo this branch actually pushes to (the ranked-first remote)
  // and its fork network can be the SOURCE of the branch's PRs. Other
  // configured remotes — a maintainer's checkout often carries contributor
  // forks — are places to look for an open PR, but their `owner:branch`
  // heads are unrelated branches that merely share a name; treating them as
  // sources made a fork's closed `main` PR show up on the local main.
  const primaryRemoteName = resolvedTargets[0]?.remoteName ?? null;
  const sourceCandidates = resolvedTargets.filter(
    (target) => target.remoteName === primaryRemoteName,
  );
  // When every consulted repo list was complete, a no-PR result is
  // authoritative and the expensive Search API fallback is pointless.
  const coverage = { authoritative: true };

  let fallbackRepo = resolvedTargets[0].repo;
  let fallbackRemoteName = resolvedTargets[0].remoteName;
  let fallbackDefaultBranch = await getRepoDefaultBranch(octokit, fallbackRepo);

  // The first closed/merged PR found, in target priority order. It is only
  // returned once every target has been checked for an open PR, so an open
  // upstream PR always wins over a merged fork PR for the same head.
  let historicalMatch = null;

  for (const target of resolvedTargets) {
    const defaultBranch = await getRepoDefaultBranch(octokit, target.repo);
    if (!fallbackRepo) {
      fallbackRepo = target.repo;
      fallbackRemoteName = target.remoteName;
      fallbackDefaultBranch = defaultBranch;
    }

    const hasCrossRepoSource = sourceCandidates.some((candidate) => normalizeRepoKey(candidate.repo?.owner, candidate.repo?.repo) !== normalizeRepoKey(target.repo?.owner, target.repo?.repo));
    for (const candidateBranch of branchCandidates) {
      if (defaultBranch && defaultBranch === candidateBranch && !hasCrossRepoSource) {
        continue;
      }

      // History is only asked of the branch's own repo and its own name: the
      // ranked-first target is the remote this branch actually pushes to.
      // Searching the rest of the fork network for history would multiply
      // serial GitHub calls for no additional user-visible information.
      const isPrimaryAssociation = target === resolvedTargets[0] && candidateBranch === branchCandidates[0];

      const { open, historical } = await findBranchPrCandidates({
        octokit,
        target,
        branch: candidateBranch,
        sourceCandidates,
        force,
        coverage,
        includeHistory: isPrimaryAssociation,
      });
      if (open) {
        return {
          repo: target.repo,
          pr: open,
          defaultBranch,
          resolvedRemoteName: target.remoteName,
        };
      }
      if (historical && !historicalMatch) {
        historicalMatch = {
          repo: target.repo,
          pr: historical,
          defaultBranch,
          resolvedRemoteName: target.remoteName,
        };
      }
    }
  }

  for (const candidateBranch of branchCandidates) {
    if (coverage.authoritative) {
      break;
    }
    const fallbackSearch = await searchFallbackPr({
      octokit,
      branch: candidateBranch,
      repoNames: resolvedTargets.map((target) => target.repo.repo),
    });
    if (fallbackSearch) {
      return {
        repo: fallbackSearch.repo,
        pr: fallbackSearch.pr,
        defaultBranch: await getRepoDefaultBranch(octokit, fallbackSearch.repo),
        resolvedRemoteName: null,
      };
    }
  }

  if (historicalMatch) {
    return historicalMatch;
  }

  return {
    repo: fallbackRepo,
    pr: null,
    defaultBranch: fallbackDefaultBranch,
    resolvedRemoteName: fallbackRemoteName,
  };
}
