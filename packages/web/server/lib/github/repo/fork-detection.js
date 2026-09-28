/**
 * fork 网络解析模块。
 *
 * 以目录某 remote 对应的仓库为起点，查询其 GitHub 元数据的 parent/source
 * 字段判断是否为 fork；是则把 upstream 仓库一并纳入返回的查询目标列表，
 * 供 PR 状态解析在 fork 与 upstream 两侧查找。元数据带 5 分钟缓存与
 * 200 条上限。
 */
import { resolveGitHubRepoFromDirectory } from './index.js';

/** 仓库元数据缓存的 TTL：5 分钟。 */
const REPO_METADATA_TTL_MS = 5 * 60_000;
/** 元数据缓存的最大条目数，超出后淘汰最旧条目。 */
const REPO_METADATA_CACHE_MAX_ENTRIES = 200;
/** 元数据缓存：repoKey → { data: repos.get 响应, fetchedAt }。 */
const repoMetadataCache = new Map();

/** 写入元数据缓存；容量已满且为新键时先淘汰最旧（插入序）条目。 */
const setRepoMetadataCache = (repoKey, data) => {
  if (repoMetadataCache.size >= REPO_METADATA_CACHE_MAX_ENTRIES && !repoMetadataCache.has(repoKey)) {
    const oldest = repoMetadataCache.entries().next().value;
    if (oldest) {
      repoMetadataCache.delete(oldest[0]);
    }
  }
  repoMetadataCache.set(repoKey, { data, fetchedAt: Date.now() });
};

/** 生成小写归一化的 `owner/repo` 键；任一侧为空返回空串。 */
const normalizeRepoKey = (owner, repo) => {
  const o = typeof owner === 'string' ? owner.trim().toLowerCase() : '';
  const r = typeof repo === 'string' ? repo.trim().toLowerCase() : '';
  if (!o || !r) return '';
  return `${o}/${r}`;
};

/**
 * 查询仓库元数据（带 TTL 缓存）；403/404 视为确认不可得并缓存 null，
 * 其它错误原样抛出。
 */
const getRepoMetadata = async (octokit, repo) => {
  const repoKey = normalizeRepoKey(repo?.owner, repo?.repo);
  if (!repoKey) return null;

  const cached = repoMetadataCache.get(repoKey);
  if (cached && Date.now() - cached.fetchedAt < REPO_METADATA_TTL_MS) {
    return cached.data;
  }

  try {
    const response = await octokit.rest.repos.get({
      owner: repo.owner,
      repo: repo.repo,
    });
    const data = response?.data ?? null;
    setRepoMetadataCache(repoKey, data);
    return data;
  } catch (error) {
    if (error?.status === 403 || error?.status === 404) {
      setRepoMetadataCache(repoKey, null);
      return null;
    }
    throw error;
  }
};

/**
 * 解析目录所属的仓库网络（中文说明）：origin 仓库若是 fork，则连同
 * parent/source 指向的 upstream 仓库一起返回（origin 在前、source 去重）；
 * 元数据不可得时退化为仅 origin，非 fork 时返回 null。
 */
/**
 * Resolve the repo network for a directory. If the origin repo is a fork,
 * includes the parent/source (upstream) repo in the result.
 *
 * @param {import('@octokit/rest').Octokit} octokit
 * @param {string} directory
 * @param {string} [remoteName='origin']
 * @returns {Promise<Array<{ owner: string, repo: string, url: string, source: string }> | null>}
 *   Array of repos to query (origin first, then upstream), or null if not a fork.
 */
export async function resolveRepoNetwork(octokit, directory, remoteName = 'origin') {
  const { repo } = await resolveGitHubRepoFromDirectory(directory, remoteName).catch(() => ({ repo: null }));
  if (!repo) return null;

  const metadata = await getRepoMetadata(octokit, repo);
  if (!metadata) return [{ ...repo, source: 'origin' }];

  const result = [{ ...repo, source: 'origin' }];
  const seenKeys = new Set([normalizeRepoKey(repo.owner, repo.repo)]);

  const parent = metadata?.parent;
  if (parent?.owner?.login && parent?.name) {
    const key = normalizeRepoKey(parent.owner.login, parent.name);
    if (!seenKeys.has(key)) {
      seenKeys.add(key);
      result.push({
        owner: parent.owner.login,
        repo: parent.name,
        url: parent.html_url || `https://github.com/${parent.owner.login}/${parent.name}`,
        source: 'upstream',
      });
    }
  }

  const source = metadata?.source;
  if (source?.owner?.login && source?.name) {
    const key = normalizeRepoKey(source.owner.login, source.name);
    if (!seenKeys.has(key)) {
      seenKeys.add(key);
      result.push({
        owner: source.owner.login,
        repo: source.name,
        url: source.html_url || `https://github.com/${source.owner.login}/${source.name}`,
        source: 'upstream',
      });
    }
  }

  // If no parent/source found, repo is not a fork
  if (result.length === 1) return null;

  return result;
}
