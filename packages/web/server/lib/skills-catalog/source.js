/**
 * 技能仓库源解析：把用户输入的仓库标识（HTTPS URL、SSH URL 或
 * owner/repo 简写，简写可再带 /subpath）解析为统一的仓库描述（owner、
 * repo、host、SSH/HTTPS 克隆地址、normalizedRepo、effectiveSubpath），
 * 供扫描（scan.js）与安装（install.js）共用。
 */

/** 简写格式默认按 GitHub 解析时使用的主机名。 */
const GITHUB_HOST = 'github.com';


/** 归一化 owner/repo：trim、去掉 repo 的 .git 后缀；任一为空返回 null。 */
function normalizeGitOwnerRepo(owner, repo) {
  const normalizedOwner = String(owner || '').trim();
  const normalizedRepo = String(repo || '').trim().replace(/\.git$/i, '');
  if (!normalizedOwner || !normalizedRepo) {
    return null;
  }
  return { owner: normalizedOwner, repo: normalizedRepo };
}


/**
 * 解析仓库源字符串。支持三种形式：https://host/owner/repo(.git)、
 * git@host:owner/repo(.git)、owner/repo[/subpath...]（默认 host 为 github.com）。
 * subpath 可经 options.subpath 显式传入；简写形式的路径尾部也作为 subpath。
 * 解析成功返回含克隆地址与 normalizedRepo 的对象，失败返回
 * { ok: false, error: { kind: 'invalidSource', message } }。
 */
export function parseSkillRepoSource(input, options = {}) {
  const raw = typeof input === 'string' ? input.trim() : '';
  if (!raw) {
    return { ok: false, error: { kind: 'invalidSource', message: 'Repository source is required' } };
  }
  const explicitSubpath = typeof options.subpath === 'string' && options.subpath.trim() ? options.subpath.trim() : null;

  const urlFormat = raw.startsWith('https://') ? 'https' : raw.startsWith('git@') ? 'ssh' : 'shorthand';
  const gitHost = urlFormat === 'https' ? raw.split('/')[2] : urlFormat === 'ssh' ? raw.split('@')[1].split(':')[0] : null;

  if (gitHost === null && urlFormat !== 'shorthand') {
    return { ok: false, error: { kind: 'invalidSource', message: 'Invalid repository URL format' } };
  }

  const pathSegments = urlFormat === 'https'
    ? raw.split('/').slice(3).filter(Boolean)
    : urlFormat === 'ssh'
      ? (raw.split('@')[1].split(':')[1] ?? '').split('/').filter(Boolean)
      : null;

  const repoName = pathSegments && pathSegments.length > 0
    ? pathSegments[pathSegments.length - 1].replace(/\.git$/i, '')
    : null;

  const gitOwner = pathSegments && pathSegments.length > 1
    ? pathSegments.slice(0, -1).join('/')
    : (pathSegments && pathSegments.length === 1 ? pathSegments[0] : null);


  // SSH git@host:owner/repo(.git) or HTTPS https://host/owner/repo(.git)
  if (urlFormat === 'ssh' || urlFormat === 'https') {
    const parsed = normalizeGitOwnerRepo(gitOwner, repoName);
    if (!parsed) {
      return { ok: false, error: { kind: 'invalidSource', message: `Invalid ${urlFormat} repository URL` } };
    }

    return {
      ok: true,
      host: gitHost,
      owner: parsed.owner,
      repo: parsed.repo,
      cloneUrlSsh: `git@${gitHost}:${parsed.owner}/${parsed.repo}.git`,
      cloneUrlHttps: `https://${gitHost}/${parsed.owner}/${parsed.repo}.git`,
      // For SSH URLs, subpath is only accepted via options.subpath
      effectiveSubpath: explicitSubpath,
      normalizedRepo: `${parsed.owner}/${parsed.repo}`,
    };
  }

  // Shorthand: owner/repo[/subpath...]
  const shorthandMatch = raw.match(/^([^/\s]+)\/([^/\s]+)(?:\/(.+))?$/);
  if (shorthandMatch) {
    const parsed = normalizeGitOwnerRepo(shorthandMatch[1], shorthandMatch[2]);
    if (!parsed) {
      return { ok: false, error: { kind: 'invalidSource', message: 'Invalid repository source' } };
    }

    const shorthandSubpath = typeof shorthandMatch[3] === 'string' && shorthandMatch[3].trim() ? shorthandMatch[3].trim() : null;
    const effectiveSubpath = explicitSubpath || shorthandSubpath;

    return {
      ok: true,
      host: GITHUB_HOST,
      owner: parsed.owner,
      repo: parsed.repo,
      cloneUrlSsh: `git@github.com:${parsed.owner}/${parsed.repo}.git`,
      cloneUrlHttps: `https://github.com/${parsed.owner}/${parsed.repo}.git`,
      effectiveSubpath,
      normalizedRepo: `${parsed.owner}/${parsed.repo}`,
    };
  }

  return { ok: false, error: { kind: 'invalidSource', message: 'Unsupported repository source format' } };
}
