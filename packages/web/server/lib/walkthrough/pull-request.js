import { getOctokitOrNull } from '../github/octokit.js';
import { resolveGitHubRepoFromDirectory } from '../github/repo/index.js';

/**
 * walkthrough 的 PR 来源模块：借助已连接的 GitHub 账号（octokit）拉取指定
 * pull request 的原始 unified diff，交给 digest 管线解析成导览素材。
 */
/**
 * Raw unified diff for a pull request.
 *
 * GitHub already returns the merge-base diff for a PR, so this matches the
 * three-dot semantics used for local branch reviews: work merged in from the
 * base branch is not part of it.
 *
 * 中文补充：directory 用于解析仓库远端（owner/repo），number 为 PR 编号。
 * 错误路径：未连接 GitHub -> 401 github-not-connected；目录无 GitHub 远端
 * -> 400 no-github-remote；PR 无 diff -> 404 empty-diff。成功返回
 * { patch, meta: { owner, repo, number } }。
 */
export async function getPullRequestDiff(directory, number) {
  const octokit = getOctokitOrNull();
  if (!octokit) {
    throw Object.assign(new Error('Connect a GitHub account to review pull requests'), {
      statusCode: 401,
      code: 'github-not-connected',
    });
  }

  // The resolver returns `{ repo, remoteUrl }`, not the repo itself. Reading
  // `.owner` off the wrapper made this check fail for every repository.
  const { repo } = await resolveGitHubRepoFromDirectory(directory);
  if (!repo?.owner || !repo?.repo) {
    throw Object.assign(new Error('This directory has no GitHub remote'), {
      statusCode: 400,
      code: 'no-github-remote',
    });
  }

  // 通过 Accept 头直接向 pulls 接口请求 diff 媒体类型，拿到纯文本而非 JSON 元数据。
  const response = await octokit.request('GET /repos/{owner}/{repo}/pulls/{pull_number}', {
    owner: repo.owner,
    repo: repo.repo,
    pull_number: number,
    headers: { accept: 'application/vnd.github.v3.diff' },
  });

  // 非 JSON 响应时 octokit 把正文放在 data 字符串里；纯空白的 diff 视为
  // "PR 无变更"，按 404 处理而不是返回一篇空导览。
  const patch = typeof response?.data === 'string' ? response.data : '';
  if (!patch.trim()) {
    throw Object.assign(new Error(`Pull request #${number} has no diff`), {
      statusCode: 404,
      code: 'empty-diff',
    });
  }

  return { patch, meta: { owner: repo.owner, repo: repo.repo, number } };
}
