import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * getPullRequestDiff 的测试套件：mock 掉 octokit 与远端解析，验证成功
 * 路径的请求参数与返回值，以及三类错误（未连接 GitHub、无远端、空 diff）
 * 的错误码、状态码和检查顺序。
 */
vi.mock('../github/octokit.js', () => ({ getOctokitOrNull: vi.fn() }));
vi.mock('../github/repo/index.js', () => ({ resolveGitHubRepoFromDirectory: vi.fn() }));

// vi.mock 会被提升，必须用动态 import 在 mock 生效后取被测模块与两个 mock 的引用。
const { getPullRequestDiff } = await import('./pull-request.js');
const { getOctokitOrNull } = await import('../github/octokit.js');
const { resolveGitHubRepoFromDirectory } = await import('../github/repo/index.js');

// 模拟 GitHub 返回的最小合法 diff 文本。
const PATCH = `diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,1 +1,2 @@
+const added = true;
`;

// 默认前置为"已连接 + 已解析出 ompchamber/ompchamber 远端"的成功路径。
describe('getPullRequestDiff', () => {
  // 每个用例重新装配的 octokit.request mock。
  let request;

  // 装配成功路径：octokit 返回固定 diff，远端解析返回仓库包装对象。
  beforeEach(() => {
    request = vi.fn().mockResolvedValue({ data: PATCH });
    getOctokitOrNull.mockReturnValue({ request });
    // The resolver hands back a wrapper, not the repo. Reading `.owner` off the
    // wrapper made every repository look remote-less, which is what this suite
    // exists to prevent.
    resolveGitHubRepoFromDirectory.mockResolvedValue({
      repo: { owner: 'ompchamber', repo: 'ompchamber' },
      remoteUrl: 'git@github.com:openchamber/openchamber.git',
    });
  });

  // 清空 mock 调用记录，避免用例间串扰。
  afterEach(() => {
    vi.clearAllMocks();
  });

  it('requests the diff for the resolved repository', async () => {
    const result = await getPullRequestDiff('/repo', 2122);

    expect(result.patch).toBe(PATCH);
    expect(result.meta).toEqual({ owner: 'ompchamber', repo: 'ompchamber', number: 2122 });
    expect(request).toHaveBeenCalledWith('GET /repos/{owner}/{repo}/pulls/{pull_number}', {
      owner: 'ompchamber',
      repo: 'ompchamber',
      pull_number: 2122,
      headers: { accept: 'application/vnd.github.v3.diff' },
    });
  });

  it('reports a missing GitHub remote only when there really is none', async () => {
    resolveGitHubRepoFromDirectory.mockResolvedValue({ repo: null, remoteUrl: null });

    await expect(getPullRequestDiff('/repo', 2122)).rejects.toMatchObject({
      code: 'no-github-remote',
      statusCode: 400,
    });
    expect(request).not.toHaveBeenCalled();
  });

  it('asks the user to connect GitHub before anything else', async () => {
    getOctokitOrNull.mockReturnValue(null);

    await expect(getPullRequestDiff('/repo', 2122)).rejects.toMatchObject({
      code: 'github-not-connected',
      statusCode: 401,
    });
    expect(resolveGitHubRepoFromDirectory).not.toHaveBeenCalled();
  });

  it('treats an empty diff as a missing pull request rather than an empty review', async () => {
    request.mockResolvedValue({ data: '   ' });

    await expect(getPullRequestDiff('/repo', 2122)).rejects.toMatchObject({
      code: 'empty-diff',
      statusCode: 404,
    });
  });
});
