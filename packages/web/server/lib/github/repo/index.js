/**
 * GitHub remote URL 解析模块。
 *
 * 把 git remote 配置中的 SSH/HTTPS 形态地址解析为 { owner, repo, url }
 * 结构，并组合 git/index.js 的 getRemoteUrl 提供目录级的仓库解析。
 */
import { getRemoteUrl } from '../../git/index.js';

/**
 * 解析 GitHub remote URL 为 { owner, repo, url }：支持
 * git@github.com:、ssh://git@github.com/ 与 https://github.com/ 三种
 * 形态并容忍 .git 后缀，url 统一规整为 https 页面地址；非 GitHub
 * 地址或路径缺少 owner/repo 时返回 null。
 */
export const parseGitHubRemoteUrl = (raw) => {
  if (typeof raw !== 'string') {
    return null;
  }
  const value = raw.trim();
  if (!value) {
    return null;
  }

  // git@github.com:OWNER/REPO.git
  if (value.startsWith('git@github.com:')) {
    const rest = value.slice('git@github.com:'.length);
    const cleaned = rest.endsWith('.git') ? rest.slice(0, -4) : rest;
    const [owner, repo] = cleaned.split('/');
    if (!owner || !repo) return null;
    return { owner, repo, url: `https://github.com/${owner}/${repo}` };
  }

  // ssh://git@github.com/OWNER/REPO.git
  if (value.startsWith('ssh://git@github.com/')) {
    const rest = value.slice('ssh://git@github.com/'.length);
    const cleaned = rest.endsWith('.git') ? rest.slice(0, -4) : rest;
    const [owner, repo] = cleaned.split('/');
    if (!owner || !repo) return null;
    return { owner, repo, url: `https://github.com/${owner}/${repo}` };
  }

  // https://github.com/OWNER/REPO(.git)
  try {
    const url = new URL(value);
    if (url.hostname !== 'github.com') {
      return null;
    }
    const path = url.pathname.replace(/^\/+/, '').replace(/\/+$/, '');
    const cleaned = path.endsWith('.git') ? path.slice(0, -4) : path;
    const [owner, repo] = cleaned.split('/');
    if (!owner || !repo) return null;
    return { owner, repo, url: `https://github.com/${owner}/${repo}` };
  } catch {
    return null;
  }
};

/**
 * 读取目录某 remote 的 URL 并解析出 GitHub 仓库；remote 不存在或不是
 * GitHub 仓库时 repo 为 null（remoteUrl 尽量返回原始值，便于诊断）。
 */
export async function resolveGitHubRepoFromDirectory(directory, remoteName = 'origin') {
  const remoteUrl = await getRemoteUrl(directory, remoteName).catch(() => null);
  if (!remoteUrl) {
    return { repo: null, remoteUrl: null };
  }
  return {
    repo: parseGitHubRemoteUrl(remoteUrl),
    remoteUrl,
  };
}
