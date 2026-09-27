/**
 * @module git/routes
 * Git 功能的 Express 路由模块。
 *
 * 在 app 上注册 /api/git 系列端点，覆盖：身份 profile 增删改查与全局/
 * 本地身份应用、status/diff/log 等只读查询、stage/unstage/apply-hunk/
 * revert 等索引与工作区操作、pull/push/fetch/rebase/merge/cherry-pick/
 * revert/reset 等仓库操作、分支与远端管理、stash 管理，以及 worktree 的
 * 校验/创建/预览/删除与 bootstrap 状态查询、integrate（worktree 提交
 * 整合回主仓库）流程。
 *
 * 所有 git 库依赖经 getGitLibraries 首次请求时动态 import（懒加载），
 * 路由注册阶段零副作用。handler 统一 try/catch：参数缺失或非法返回 400，
 * git 执行失败打日志并回 5xx；个别端点按语义降级——status 对非仓库目录
 * 返回 nonRepoStatusPayload、worktrees 列表失败返回空数组并附
 * X-OMPChamber-Warning header，避免客户端对可选功能反复重试。
 */
/**
 * 在 Express app 上注册全部 /api/git 路由（身份、状态/diff/日志、索引与
 * 工作区操作、远端同步、分支、stash、worktree 与 integrate 流程）。
 * @param {object} app Express 应用实例（使用其 get/post/put/delete 方法）
 */
export function registerGitRoutes(app) {
  let gitLibraries = null;
  /** 懒加载并 memoize ./index.js 导出的 git 库集合；避免路由注册阶段产生 import 副作用。 */
  const getGitLibraries = async () => {
    if (!gitLibraries) {
      gitLibraries = await import('./index.js');
    }
    return gitLibraries;
  };

  /** 解析 query 中的 directory 参数：兼容数组形式（取首项）并 trim；缺失或全空白返回 null。 */
  const resolveDirectoryQuery = (value) => {
    const raw = Array.isArray(value) ? value[0] : value;
    if (typeof raw !== 'string') {
      return null;
    }
    const trimmed = raw.trim();
    return trimmed || null;
  };

  /** 把 error 的 message/stderr/stdout 及其字符串形式合并为多行诊断文本，供日志与"非仓库"判定复用。 */
  const extractGitErrorText = (error) => {
    const message = typeof error?.message === 'string' ? error.message : '';
    const stderr = typeof error?.stderr === 'string' ? error.stderr : '';
    const stdout = typeof error?.stdout === 'string' ? error.stdout : '';
    const fallback = !message && error != null ? String(error) : '';
    return [message, stderr, stdout, fallback]
      .map((value) => String(value || '').trim())
      .filter(Boolean)
      .join('\n');
  };

  /** 判断错误是否为 git 的 "not a git repository" 类错误（用于把潜在 5xx 降级为非仓库语义）。 */
  const isNonRepoGitError = (error) => /not a git repository/i.test(extractGitErrorText(error));

  /** 非仓库目录的标准 status 降级载荷：isGitRepository=false、空文件列表、无分支、ahead/behind 归零。 */
  const nonRepoStatusPayload = () => ({
    isGitRepository: false,
    files: [],
    branch: null,
    ahead: 0,
    behind: 0,
  });

  // GET /api/git/identities：列出全部 git 身份 profile。
  app.get('/api/git/identities', async (req, res) => {
    const { getProfiles } = await getGitLibraries();
    try {
      const profiles = getProfiles();
      res.json(profiles);
    } catch (error) {
      console.error('Failed to list git identity profiles:', error);
      res.status(500).json({ error: 'Failed to list git identity profiles' });
    }
  });

  // POST /api/git/identities：新建身份 profile；载荷非法时 400。
  app.post('/api/git/identities', async (req, res) => {
    const { createProfile } = await getGitLibraries();
    try {
      const profile = createProfile(req.body);
      console.log(`Created git identity profile: ${profile.name} (${profile.id})`);
      res.json(profile);
    } catch (error) {
      console.error('Failed to create git identity profile:', error);
      res.status(400).json({ error: error.message || 'Failed to create git identity profile' });
    }
  });

  // PUT /api/git/identities/:id：更新指定 profile。
  app.put('/api/git/identities/:id', async (req, res) => {
    const { updateProfile } = await getGitLibraries();
    try {
      const profile = updateProfile(req.params.id, req.body);
      console.log(`Updated git identity profile: ${profile.name} (${profile.id})`);
      res.json(profile);
    } catch (error) {
      console.error('Failed to update git identity profile:', error);
      res.status(400).json({ error: error.message || 'Failed to update git identity profile' });
    }
  });

  // DELETE /api/git/identities/:id：删除指定 profile。
  app.delete('/api/git/identities/:id', async (req, res) => {
    const { deleteProfile } = await getGitLibraries();
    try {
      deleteProfile(req.params.id);
      console.log(`Deleted git identity profile: ${req.params.id}`);
      res.json({ success: true });
    } catch (error) {
      console.error('Failed to delete git identity profile:', error);
      res.status(400).json({ error: error.message || 'Failed to delete git identity profile' });
    }
  });

  // GET /api/git/global-identity：读取全局 git 身份（user.name/user.email/ssh）。
  app.get('/api/git/global-identity', async (req, res) => {
    const { getGlobalIdentity } = await getGitLibraries();
    try {
      const identity = await getGlobalIdentity();
      res.json(identity);
    } catch (error) {
      console.error('Failed to get global git identity:', error);
      res.status(500).json({ error: 'Failed to get global git identity' });
    }
  });

  // GET /api/git/discover-credentials：探测本机可用的 git 凭证（gh CLI、ssh key 等）。
  app.get('/api/git/discover-credentials', async (req, res) => {
    try {
      const { discoverGitCredentials } = await import('./index.js');
      const credentials = discoverGitCredentials();
      res.json(credentials);
    } catch (error) {
      console.error('Failed to discover git credentials:', error);
      res.status(500).json({ error: 'Failed to discover git credentials' });
    }
  });

  // GET /api/git/check：判断 directory 是否为 git 仓库；"not a git repository" 类错误按 false 返回而非 500。
  app.get('/api/git/check', async (req, res) => {
    const { isGitRepository } = await getGitLibraries();
    try {
      const directory = resolveDirectoryQuery(req.query.directory);
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const isRepo = await isGitRepository(directory);
      res.json({ isGitRepository: isRepo });
    } catch (error) {
      if (isNonRepoGitError(error)) {
        console.warn('Git check treated non-repository path as not a git repo:', extractGitErrorText(error));
        return res.json({ isGitRepository: false });
      }
      console.error('Failed to check git repository:', error);
      res.status(500).json({ error: 'Failed to check git repository' });
    }
  });

  // GET /api/git/remote-url：读取 directory 指定 remote（默认 origin）的 URL。
  app.get('/api/git/remote-url', async (req, res) => {
    const { getRemoteUrl } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const remote = req.query.remote || 'origin';

      const url = await getRemoteUrl(directory, remote);
      res.json({ url });
    } catch (error) {
      console.error('Failed to get remote url:', error);
      res.status(500).json({ error: 'Failed to get remote url' });
    }
  });

  // GET /api/git/current-identity：读取仓库当前生效的 git 身份。
  app.get('/api/git/current-identity', async (req, res) => {
    const { getCurrentIdentity } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const identity = await getCurrentIdentity(directory);
      res.json(identity);
    } catch (error) {
      console.error('Failed to get current git identity:', error);
      res.status(500).json({ error: 'Failed to get current git identity' });
    }
  });

  // GET /api/git/has-local-identity：仓库是否配置了 local 级身份。
  app.get('/api/git/has-local-identity', async (req, res) => {
    const { hasLocalIdentity } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const hasLocal = await hasLocalIdentity(directory);
      res.json({ hasLocalIdentity: hasLocal });
    } catch (error) {
      console.error('Failed to check local git identity:', error);
      res.status(500).json({ error: 'Failed to check local git identity' });
    }
  });

  // POST /api/git/set-identity：把指定 profile（或 'global' 全局身份）写入仓库 local 配置；全局身份未配置或 profile 不存在时 404。
  app.post('/api/git/set-identity', async (req, res) => {
    const { getProfile, setLocalIdentity, getGlobalIdentity } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { profileId } = req.body;
      if (!profileId) {
        return res.status(400).json({ error: 'profileId is required' });
      }

      let profile = null;

      if (profileId === 'global') {
        const globalIdentity = await getGlobalIdentity();
        if (!globalIdentity?.userName || !globalIdentity?.userEmail) {
          return res.status(404).json({ error: 'Global identity is not configured' });
        }
        profile = {
          id: 'global',
          name: 'Global Identity',
          userName: globalIdentity.userName,
          userEmail: globalIdentity.userEmail,
          sshKey: globalIdentity.sshCommand
            ? globalIdentity.sshCommand.replace('ssh -i ', '')
            : null,
        };
      } else {
        profile = getProfile(profileId);
        if (!profile) {
          return res.status(404).json({ error: 'Profile not found' });
        }
      }

      await setLocalIdentity(directory, profile);
      res.json({ success: true, profile });
    } catch (error) {
      console.error('Failed to set git identity:', error);
      res.status(500).json({ error: error.message || 'Failed to set git identity' });
    }
  });

  // GET /api/git/status：读取仓库状态；mode=light 走轻量模式，非仓库目录返回降级载荷而非 500（保护侧栏等项目枚举调用方）。
  app.get('/api/git/status', async (req, res) => {
    const { getStatus, isGitRepository } = await getGitLibraries();

    try {
      const directory = resolveDirectoryQuery(req.query.directory);
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const isRepo = await isGitRepository(directory);
      if (!isRepo) {
        return res.json(nonRepoStatusPayload());
      }

      const mode = req.query.mode === 'light' ? 'light' : undefined;
      const status = await getStatus(directory, { mode });
      res.json(status);
    } catch (error) {
      // Non-repo / GitError must not abort callers that enumerate projects or
      // sessions (e.g. sidebar discovery). Log a warning and continue.
      if (isNonRepoGitError(error)) {
        console.warn('Git status skipped for non-repository path:', extractGitErrorText(error));
        return res.json(nonRepoStatusPayload());
      }
      console.error('Failed to get git status:', error);
      res.status(500).json({ error: error.message || 'Failed to get git status' });
    }
  });

  // GET /api/git/primary-root：从任意位置（含 linked worktree）解析主 worktree 根。
  app.get('/api/git/primary-root', async (req, res) => {
    const { resolvePrimaryWorktreeRoot } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const result = await resolvePrimaryWorktreeRoot(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to resolve git primary root:', error);
      res.status(500).json({ error: error.message || 'Failed to resolve git primary root' });
    }
  });

  // GET /api/git/toplevel：解析 directory 所在仓库的 git toplevel（子目录会上溯到根）。
  app.get('/api/git/toplevel', async (req, res) => {
    const { resolveWorktreeTopLevel } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const result = await resolveWorktreeTopLevel(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to resolve git worktree toplevel:', error);
      res.status(500).json({ error: error.message || 'Failed to resolve git worktree toplevel' });
    }
  });

  // POST /api/git/commit-summaries：按 body.shas 批量获取提交摘要。
  app.post('/api/git/commit-summaries', async (req, res) => {
    const { getCommitSummaries } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const result = await getCommitSummaries(directory, req.body?.shas);
      res.json(result);
    } catch (error) {
      console.error('Failed to get git commit summaries:', error);
      res.status(400).json({ error: error.message || 'Failed to get git commit summaries' });
    }
  });

  /**
   * 注册一个 /api/git/integrate/:action 端点的通用包装：按需加载 handler，
   * 以请求 body 调用并回传结果；失败统一打日志并返回 400。
   * @param {string} action 动作名（拼进路由路径）
   * @param {() => Promise<(body: object) => unknown>} loadHandler 惰性返回实际处理函数
   */
  const handleIntegrateAction = (action, loadHandler) => {
    app.post(`/api/git/integrate/${action}`, async (req, res) => {
      try {
        const handler = await loadHandler();
        const result = await handler(req.body || {});
        res.json(result);
      } catch (error) {
        console.error(`Failed to run git integrate ${action}:`, error);
        res.status(400).json({ error: error.message || `Failed to run git integrate ${action}` });
      }
    });
  };

  // integrate/plan：计算把 worktree 提交整合回目标分支的执行计划。
  handleIntegrateAction('plan', async () => {
    const { computeIntegratePlan } = await getGitLibraries();
    return (body) => computeIntegratePlan(body);
  });

  // integrate/conflict-details：读取临时 worktree 中 cherry-pick 冲突的详情。
  handleIntegrateAction('conflict-details', async () => {
    const { getIntegrateConflictDetails } = await getGitLibraries();
    return (body) => getIntegrateConflictDetails(body?.tempWorktreePath);
  });

  // integrate/cherry-pick-status：查询临时 worktree 是否处于 cherry-pick 进行中。
  handleIntegrateAction('cherry-pick-status', async () => {
    const { isCherryPickInProgress } = await getGitLibraries();
    return (body) => isCherryPickInProgress(body?.tempWorktreePath);
  });

  // integrate/run：按 plan 在临时 worktree 内执行整合。
  handleIntegrateAction('run', async () => {
    const { integrateWorktreeCommits } = await getGitLibraries();
    return (body) => integrateWorktreeCommits(body?.plan);
  });

  // integrate/abort：按 state 中止整合并清理临时 worktree。
  handleIntegrateAction('abort', async () => {
    const { abortIntegrate } = await getGitLibraries();
    return (body) => abortIntegrate(body?.state);
  });

  // integrate/continue：冲突解决后继续整合流程。
  handleIntegrateAction('continue', async () => {
    const { continueIntegrate } = await getGitLibraries();
    return (body) => continueIntegrate(body?.state);
  });

  // GET /api/git/diff：取指定文件的 unified diff；staged=true 看暂存区，context 控制上下文行数（非法或缺省为 3）。
  app.get('/api/git/diff', async (req, res) => {
    const { getDiff } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const path = req.query.path;
      if (!path || typeof path !== 'string') {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      const staged = req.query.staged === 'true';
      const context = req.query.context ? parseInt(String(req.query.context), 10) : undefined;

      const diff = await getDiff(directory, {
        path,
        staged,
        contextLines: Number.isFinite(context) ? context : 3,
      });

      res.json({ diff });
    } catch (error) {
      console.error('Failed to get git diff:', error);
      res.status(500).json({ error: error.message || 'Failed to get git diff' });
    }
  });

  // GET /api/git/file-diff：取文件的 original/modified 两侧内容（split diff 用），并标注是否二进制。
  app.get('/api/git/file-diff', async (req, res) => {
    const { getFileDiff } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const pathParam = req.query.path;
      if (!pathParam || typeof pathParam !== 'string') {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      const staged = req.query.staged === 'true';

      const result = await getFileDiff(directory, {
        path: pathParam,
        staged,
      });

      res.json({
        original: result.original,
        modified: result.modified,
        path: result.path,
        isBinary: Boolean(result.isBinary),
      });
    } catch (error) {
      console.error('Failed to get git file diff:', error);
      res.status(500).json({ error: error.message || 'Failed to get git file diff' });
    }
  });

  // GET /api/git/range-diff：取 base..head 区间的 diff；可选 path 过滤与 context 上下文行数。
  app.get('/api/git/range-diff', async (req, res) => {
    const { getRangeDiff } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const base = req.query.base;
      const head = req.query.head;
      if (!base || typeof base !== 'string' || !head || typeof head !== 'string') {
        return res.status(400).json({ error: 'base and head parameters are required' });
      }

      const pathParam = typeof req.query.path === 'string' && req.query.path ? req.query.path : undefined;
      const context = req.query.context ? parseInt(String(req.query.context), 10) : undefined;

      const diff = await getRangeDiff(directory, {
        base,
        head,
        path: pathParam,
        contextLines: Number.isFinite(context) ? context : 3,
      });

      res.json({ diff });
    } catch (error) {
      console.error('Failed to get git range diff:', error);
      res.status(500).json({ error: error.message || 'Failed to get git range diff' });
    }
  });

  // GET /api/git/branch-base：解析指定分支的 base ref（日志/对比的默认基准）。
  app.get('/api/git/branch-base', async (req, res) => {
    const { getBranchBase } = await getGitLibraries();
    try {
      const directory = resolveDirectoryQuery(req.query.directory);
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const branch = resolveDirectoryQuery(req.query.branch);
      if (!branch) {
        return res.status(400).json({ error: 'branch parameter is required' });
      }

      const result = await getBranchBase(directory, branch);
      res.json(result);
    } catch (error) {
      console.error('Failed to get branch base:', error);
      res.status(500).json({ error: error.message || 'Failed to get branch base' });
    }
  });

  // GET /api/git/range-files：列出 base..head 区间内变更的文件。
  app.get('/api/git/range-files', async (req, res) => {
    const { getRangeFiles } = await getGitLibraries();
    try {
      const directory = resolveDirectoryQuery(req.query.directory);
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const base = resolveDirectoryQuery(req.query.base);
      const head = resolveDirectoryQuery(req.query.head);
      if (!base || !head) {
        return res.status(400).json({ error: 'base and head parameters are required' });
      }

      const files = await getRangeFiles(directory, { base, head });
      res.json({ files });
    } catch (error) {
      console.error('Failed to get git range files:', error);
      res.status(500).json({ error: error.message || 'Failed to get git range files' });
    }
  });

  // POST /api/git/revert：按 scope 丢弃指定文件的改动（工作区/暂存区）。
  app.post('/api/git/revert', async (req, res) => {
    const { revertFile } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { path, scope } = req.body || {};
      if (!path || typeof path !== 'string') {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      await revertFile(directory, path, { scope });
      res.json({ success: true });
    } catch (error) {
      console.error('Failed to revert git file:', error);
      res.status(500).json({ error: error.message || 'Failed to revert git file' });
    }
  });

  // POST /api/git/stage：把 body.path 或 body.paths（批量）加入暂存区。
  app.post('/api/git/stage', async (req, res) => {
    const { stageFiles } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { path, paths } = req.body || {};
      const filePaths = Array.isArray(paths) ? paths : [path];
      if (!filePaths.some((value) => typeof value === 'string' && value.trim())) {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      await stageFiles(directory, filePaths);
      res.json({ success: true });
    } catch (error) {
      console.error('Failed to stage git file:', error);
      res.status(500).json({ error: error.message || 'Failed to stage git file' });
    }
  });

  // POST /api/git/unstage：把 body.path 或 body.paths（批量）移出暂存区。
  app.post('/api/git/unstage', async (req, res) => {
    const { unstageFiles } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { path, paths } = req.body || {};
      const filePaths = Array.isArray(paths) ? paths : [path];
      if (!filePaths.some((value) => typeof value === 'string' && value.trim())) {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      await unstageFiles(directory, filePaths);
      res.json({ success: true });
    } catch (error) {
      console.error('Failed to unstage git file:', error);
      res.status(500).json({ error: error.message || 'Failed to unstage git file' });
    }
  });

  // POST /api/git/apply-hunk：对单个 hunk 执行 stage/unstage/discard；action 非法或 patch 缺失在触达 git 前返回 400。
  app.post('/api/git/apply-hunk', async (req, res) => {
    const { applyHunk } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { path: filePath, patch, action } = req.body || {};
      if (!filePath || typeof filePath !== 'string') {
        return res.status(400).json({ error: 'path parameter is required' });
      }
      if (typeof patch !== 'string' || !patch.trim()) {
        return res.status(400).json({ error: 'patch is required' });
      }
      if (action !== 'stage' && action !== 'unstage' && action !== 'discard') {
        return res.status(400).json({ error: 'action must be stage, unstage, or discard' });
      }

      await applyHunk(directory, filePath, { patch, action });
      res.json({ success: true });
    } catch (error) {
      console.error('Failed to apply git hunk:', error);
      res.status(500).json({ error: error.message || 'Failed to apply git hunk' });
    }
  });

  // POST /api/git/pull：拉取远端更新（body 透传 pull 选项）。
  app.post('/api/git/pull', async (req, res) => {
    const { pull } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await pull(directory, req.body);
      res.json(result);
    } catch (error) {
      console.error('Failed to pull:', error);
      res.status(500).json({ error: error.message || 'Failed to pull from remote' });
    }
  });

  // POST /api/git/push：推送提交到远端（body 透传 push 选项）。
  app.post('/api/git/push', async (req, res) => {
    const { push } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await push(directory, req.body);
      res.json(result);
    } catch (error) {
      console.error('Failed to push:', error);
      res.status(500).json({ error: error.message || 'Failed to push to remote' });
    }
  });

  // GET /api/git/stashes：列出全部 stash 条目。
  app.get('/api/git/stashes', async (req, res) => {
    const { listStashes } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json({ stashes: await listStashes(directory) });
    } catch (error) {
      console.error('Failed to list stashes:', error);
      res.status(500).json({ error: error.message || 'Failed to list stashes' });
    }
  });

  // POST /api/git/stashes/file-counts：统计各 stash 涉及的文件数量。
  app.post('/api/git/stashes/file-counts', async (req, res) => {
    const { countStashFiles } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json({ counts: await countStashFiles(directory, req.body?.refs) });
    } catch (error) {
      console.error('Failed to count stash files:', error);
      res.status(500).json({ error: error.message || 'Failed to count stash files' });
    }
  });

  // POST /api/git/stash：新建 stash（stashPush，body 透传选项）。
  app.post('/api/git/stash', async (req, res) => {
    const { stashPush } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json(await stashPush(directory, req.body));
    } catch (error) {
      console.error('Failed to stash changes:', error);
      res.status(500).json({ error: error.message || 'Failed to stash changes' });
    }
  });

  // POST /api/git/stash/apply：应用指定 stash 但保留条目。
  app.post('/api/git/stash/apply', async (req, res) => {
    const { stashApply } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json(await stashApply(directory, req.body));
    } catch (error) {
      console.error('Failed to apply stash:', error);
      res.status(500).json({ error: error.message || 'Failed to apply stash' });
    }
  });

  // POST /api/git/stash/pop：应用指定 stash 并删除条目。
  app.post('/api/git/stash/pop', async (req, res) => {
    const { stashPop } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json(await stashPop(directory, req.body));
    } catch (error) {
      console.error('Failed to pop stash:', error);
      res.status(500).json({ error: error.message || 'Failed to pop stash' });
    }
  });

  // POST /api/git/stash/drop：丢弃指定 stash。
  app.post('/api/git/stash/drop', async (req, res) => {
    const { stashDrop } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      res.json(await stashDrop(directory, req.body));
    } catch (error) {
      console.error('Failed to drop stash:', error);
      res.status(500).json({ error: error.message || 'Failed to drop stash' });
    }
  });

  // POST /api/git/fetch：从远端 fetch（body 透传 fetch 选项）。
  app.post('/api/git/fetch', async (req, res) => {
    const { fetch: gitFetch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await gitFetch(directory, req.body);
      res.json(result);
    } catch (error) {
      console.error('Failed to fetch:', error);
      res.status(500).json({ error: error.message || 'Failed to fetch from remote' });
    }
  });

  // GET /api/git/remotes：列出远端配置。
  app.get('/api/git/remotes', async (req, res) => {
    const { getRemotes } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const remotes = await getRemotes(directory);
      res.json(remotes);
    } catch (error) {
      console.error('Failed to get remotes:', error);
      res.status(500).json({ error: error.message || 'Failed to get remotes' });
    }
  });

  // DELETE /api/git/remotes：删除指定远端（body.remote）。
  app.delete('/api/git/remotes', async (req, res) => {
    const { removeRemote } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const remote = String(req.body?.remote || '').trim();
      if (!remote) {
        return res.status(400).json({ error: 'remote is required' });
      }

      const result = await removeRemote(directory, { remote });
      res.json(result);
    } catch (error) {
      console.error('Failed to remove remote:', error);
      res.status(500).json({ error: error.message || 'Failed to remove remote' });
    }
  });

  // POST /api/git/rebase：发起 rebase（body 透传选项）。
  app.post('/api/git/rebase', async (req, res) => {
    const { rebase } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await rebase(directory, req.body);
      res.json(result);
    } catch (error) {
      console.error('Failed to rebase:', error);
      res.status(500).json({ error: error.message || 'Failed to rebase' });
    }
  });

  // POST /api/git/rebase/abort：中止进行中的 rebase。
  app.post('/api/git/rebase/abort', async (req, res) => {
    const { abortRebase } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await abortRebase(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to abort rebase:', error);
      res.status(500).json({ error: error.message || 'Failed to abort rebase' });
    }
  });

  // POST /api/git/merge：发起 merge（body 透传选项）。
  app.post('/api/git/merge', async (req, res) => {
    const { merge } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await merge(directory, req.body);
      res.json(result);
    } catch (error) {
      console.error('Failed to merge:', error);
      res.status(500).json({ error: error.message || 'Failed to merge' });
    }
  });

  // POST /api/git/merge/abort：中止进行中的 merge。
  app.post('/api/git/merge/abort', async (req, res) => {
    const { abortMerge } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await abortMerge(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to abort merge:', error);
      res.status(500).json({ error: error.message || 'Failed to abort merge' });
    }
  });

  // POST /api/git/rebase/continue：冲突解决后继续 rebase。
  app.post('/api/git/rebase/continue', async (req, res) => {
    const { continueRebase } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await continueRebase(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to continue rebase:', error);
      res.status(500).json({ error: error.message || 'Failed to continue rebase' });
    }
  });

  // POST /api/git/merge/continue：冲突解决后继续 merge。
  app.post('/api/git/merge/continue', async (req, res) => {
    const { continueMerge } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await continueMerge(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to continue merge:', error);
      res.status(500).json({ error: error.message || 'Failed to continue merge' });
    }
  });

  // GET /api/git/conflict-details：读取 merge/rebase 冲突文件的详情。
  app.get('/api/git/conflict-details', async (req, res) => {
    const { getConflictDetails } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await getConflictDetails(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to get conflict details:', error);
      res.status(500).json({ error: error.message || 'Failed to get conflict details' });
    }
  });

  // POST /api/git/commit：创建提交；message 必填，可选 addAll / files / stageFiles 组合暂存策略。
  app.post('/api/git/commit', async (req, res) => {
    const { commit } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { message, addAll, files, stageFiles } = req.body;
      if (!message) {
        return res.status(400).json({ error: 'message is required' });
      }

      const result = await commit(directory, message, {
        addAll,
        files,
        stageFiles,
      });
      res.json(result);
    } catch (error) {
      console.error('Failed to commit:', error);
      res.status(500).json({ error: error.message || 'Failed to create commit' });
    }
  });

  // GET /api/git/branches：列出本地与远端分支（含默认分支推断）。
  app.get('/api/git/branches', async (req, res) => {
    const { getBranches } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const branches = await getBranches(directory);
      res.json(branches);
    } catch (error) {
      console.error('Failed to get branches:', error);
      res.status(500).json({ error: error.message || 'Failed to get branches' });
    }
  });

  // POST /api/git/branch-push-status：批量查询 branches 各自相对本地已知上游的未推送提交数。
  app.post('/api/git/branch-push-status', async (req, res) => {
    const { getUnpushedBranchCounts } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      const branches = req.body?.branches;
      if (!directory) return res.status(400).json({ error: 'directory parameter is required' });
      if (!Array.isArray(branches) || branches.some((branch) => typeof branch !== 'string')) {
        return res.status(400).json({ error: 'branches must be an array of branch names' });
      }
      res.json(await getUnpushedBranchCounts(directory, branches));
    } catch (error) {
      console.error('Failed to get branch push status:', error);
      res.status(500).json({ error: error.message || 'Failed to get branch push status' });
    }
  });

  // POST /api/git/branches：创建分支（可指定 startPoint 起点）。
  app.post('/api/git/branches', async (req, res) => {
    const { createBranch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { name, startPoint } = req.body;
      if (!name) {
        return res.status(400).json({ error: 'name is required' });
      }

      const result = await createBranch(directory, name, { startPoint });
      res.json(result);
    } catch (error) {
      console.error('Failed to create branch:', error);
      res.status(500).json({ error: error.message || 'Failed to create branch' });
    }
  });

  // DELETE /api/git/branches：删除本地分支（force=true 强删未合并分支）。
  app.delete('/api/git/branches', async (req, res) => {
    const { deleteBranch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { branch, force } = req.body;
      if (!branch) {
        return res.status(400).json({ error: 'branch is required' });
      }

      const result = await deleteBranch(directory, branch, { force });
      res.json(result);
    } catch (error) {
      console.error('Failed to delete branch:', error);
      res.status(500).json({ error: error.message || 'Failed to delete branch' });
    }
  });


  // PUT /api/git/branches/rename：重命名本地分支。
  app.put('/api/git/branches/rename', async (req, res) => {
    const { renameBranch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { oldName, newName } = req.body;
      if (!oldName) {
        return res.status(400).json({ error: 'oldName is required' });
      }
      if (!newName) {
        return res.status(400).json({ error: 'newName is required' });
      }

      const result = await renameBranch(directory, oldName, newName);
      res.json(result);
    } catch (error) {
      console.error('Failed to rename branch:', error);
      res.status(500).json({ error: error.message || 'Failed to rename branch' });
    }
  });
  // DELETE /api/git/remote-branches：删除远端分支（body.branch/remote）。
  app.delete('/api/git/remote-branches', async (req, res) => {
    const { deleteRemoteBranch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { branch, remote } = req.body;
      if (!branch) {
        return res.status(400).json({ error: 'branch is required' });
      }

      const result = await deleteRemoteBranch(directory, { branch, remote });
      res.json(result);
    } catch (error) {
      console.error('Failed to delete remote branch:', error);
      res.status(500).json({ error: error.message || 'Failed to delete remote branch' });
    }
  });

  // POST /api/git/checkout：切换分支；选中远端分支时建立跟踪本地分支而非 detach。
  app.post('/api/git/checkout', async (req, res) => {
    const { checkoutBranch } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { branch } = req.body;
      if (!branch) {
        return res.status(400).json({ error: 'branch is required' });
      }

      const result = await checkoutBranch(directory, branch);
      res.json(result);
    } catch (error) {
      console.error('Failed to checkout branch:', error);
      res.status(500).json({ error: error.message || 'Failed to checkout branch' });
    }
  });

  // POST /api/git/checkout-commit：detach HEAD 到指定提交；hash 先经 7-40 位十六进制格式校验。
  app.post('/api/git/checkout-commit', async (req, res) => {
    const { checkoutCommit } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const { hash } = req.body;
      if (!req.body.hash || typeof req.body.hash !== 'string' || !/^[0-9a-fA-F]{7,40}$/.test(req.body.hash)) {
        return res.status(400).json({ error: 'Invalid commit hash' });
      }
      const result = await checkoutCommit(directory, hash);
      res.json(result);
    } catch (error) {
      console.error('Failed to checkout commit:', error);
      res.status(500).json({ error: error.message || 'Failed to checkout commit' });
    }
  });

  // POST /api/git/cherry-pick：cherry-pick 指定提交；hash 先做同上格式校验。
  app.post('/api/git/cherry-pick', async (req, res) => {
    const { cherryPick } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const { hash } = req.body;
      if (!req.body.hash || typeof req.body.hash !== 'string' || !/^[0-9a-fA-F]{7,40}$/.test(req.body.hash)) {
        return res.status(400).json({ error: 'Invalid commit hash' });
      }
      const result = await cherryPick(directory, hash);
      res.json(result);
    } catch (error) {
      console.error('Failed to cherry-pick:', error);
      res.status(500).json({ error: error.message || 'Failed to cherry-pick' });
    }
  });

  // POST /api/git/revert-commit：生成指定提交的反向提交；hash 先做格式校验。
  app.post('/api/git/revert-commit', async (req, res) => {
    const { revertCommit } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const { hash } = req.body;
      if (!req.body.hash || typeof req.body.hash !== 'string' || !/^[0-9a-fA-F]{7,40}$/.test(req.body.hash)) {
        return res.status(400).json({ error: 'Invalid commit hash' });
      }
      const result = await revertCommit(directory, hash);
      res.json(result);
    } catch (error) {
      console.error('Failed to revert commit:', error);
      res.status(500).json({ error: error.message || 'Failed to revert commit' });
    }
  });

  // POST /api/git/reset-to-commit：soft/mixed/hard 重置到指定提交；脏工作区下 hard 需显式 force=true。
  app.post('/api/git/reset-to-commit', async (req, res) => {
    const { resetToCommit } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const { hash, mode, force } = req.body;
      if (!req.body.hash || typeof req.body.hash !== 'string' || !/^[0-9a-fA-F]{7,40}$/.test(req.body.hash)) {
        return res.status(400).json({ error: 'Invalid commit hash' });
      }
      if (!['soft', 'mixed', 'hard'].includes(mode)) {
        return res.status(400).json({ error: 'mode must be soft, mixed, or hard' });
      }
      const result = await resetToCommit(directory, hash, mode, force === true);
      res.json(result);
    } catch (error) {
      console.error('Failed to reset to commit:', error);
      res.status(500).json({ error: error.message || 'Failed to reset' });
    }
  });

  // GET /api/git/worktrees：列出 worktree；失败不回 500——记警告、附 X-OMPChamber-Warning header 并返回空数组（可选功能，避免客户端反复重试）。
  app.get('/api/git/worktrees', async (req, res) => {
    const { getWorktrees } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const worktrees = await getWorktrees(directory);
      res.json(worktrees);
    } catch (error) {
      // Worktrees are an optional feature. Avoid repeated 500s (and repeated client retries)
      // when the directory isn't a git repo or uses shell shorthand like "~/".
      console.warn('Failed to get worktrees, returning empty list:', error?.message || error);
      res.setHeader('X-OMPChamber-Warning', 'git worktrees unavailable');
      res.json([]);
    }
  });

  // POST /api/git/worktrees/validate：预校验 worktree 创建参数；git 库未导出该函数时 501。
  app.post('/api/git/worktrees/validate', async (req, res) => {
    const { validateWorktreeCreate } = await getGitLibraries();
    if (typeof validateWorktreeCreate !== 'function') {
      return res.status(501).json({ error: 'Worktree validation is not available' });
    }

    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const result = await validateWorktreeCreate(directory, req.body || {});
      res.json(result);
    } catch (error) {
      console.error('Failed to validate worktree creation:', error);
      res.status(500).json({ error: error.message || 'Failed to validate worktree creation' });
    }
  });

  // POST /api/git/worktrees：创建 worktree（含 bootstrap 流程）；git 库未导出时 501。
  app.post('/api/git/worktrees', async (req, res) => {
    const { createWorktree } = await getGitLibraries();
    if (typeof createWorktree !== 'function') {
      return res.status(501).json({ error: 'Worktree creation is not available' });
    }

    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const created = await createWorktree(directory, req.body || {});
      res.json(created);
    } catch (error) {
      console.error('Failed to create worktree:', error);
      res.status(500).json({ error: error.message || 'Failed to create worktree' });
    }
  });

  // POST /api/git/worktrees/preview：预览创建结果（目录、分支等）而不实际创建；git 库未导出时 501。
  app.post('/api/git/worktrees/preview', async (req, res) => {
    const { previewWorktreeCreate } = await getGitLibraries();
    if (typeof previewWorktreeCreate !== 'function') {
      return res.status(501).json({ error: 'Worktree preview is not available' });
    }

    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const preview = await previewWorktreeCreate(directory, req.body || {});
      res.json(preview);
    } catch (error) {
      console.error('Failed to preview worktree:', error);
      res.status(500).json({ error: error.message || 'Failed to preview worktree' });
    }
  });

  // GET /api/git/worktrees/bootstrap-status：查询 worktree 引导（目录/Git/setup）进度；git 库未导出时 501。
  app.get('/api/git/worktrees/bootstrap-status', async (req, res) => {
    const { getWorktreeBootstrapStatus } = await getGitLibraries();
    if (typeof getWorktreeBootstrapStatus !== 'function') {
      return res.status(501).json({ error: 'Worktree bootstrap status is not available' });
    }

    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const status = await getWorktreeBootstrapStatus(directory);
      res.json(status);
    } catch (error) {
      console.error('Failed to get worktree bootstrap status:', error);
      res.status(500).json({ error: error.message || 'Failed to get worktree bootstrap status' });
    }
  });

  // DELETE /api/git/worktrees：删除 worktree（可连带删本地分支）；git 库未导出时 501。
  app.delete('/api/git/worktrees', async (req, res) => {
    const { removeWorktree } = await getGitLibraries();
    if (typeof removeWorktree !== 'function') {
      return res.status(501).json({ error: 'Worktree removal is not available' });
    }

    try {
      const directory = req.query.directory;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const worktreeDirectory = typeof req.body?.directory === 'string' ? req.body.directory : '';
      if (!worktreeDirectory) {
        return res.status(400).json({ error: 'worktree directory is required' });
      }

      const result = await removeWorktree(directory, {
        directory: worktreeDirectory,
        deleteLocalBranch: req.body?.deleteLocalBranch === true,
      });
      res.json({ success: Boolean(result) });
    } catch (error) {
      console.error('Failed to remove worktree:', error);
      res.status(500).json({ error: error.message || 'Failed to remove worktree' });
    }
  });

  // GET /api/git/worktree-type：判断 directory 是否为 linked worktree。
  app.get('/api/git/worktree-type', async (req, res) => {
    const { isLinkedWorktree } = await getGitLibraries();
    try {
      const { directory } = req.query;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      const linked = await isLinkedWorktree(directory);
      res.json({ linked });
    } catch (error) {
      console.error('Failed to determine worktree type:', error);
      res.status(500).json({ error: error.message || 'Failed to determine worktree type' });
    }
  });

  // POST /api/git/validate-directory：校验 worktree 目录合法性（越权/占用等）；git 库未导出时 501。
  app.post('/api/git/validate-directory', async (req, res) => {
    const { validateWorktreeDirectory } = await getGitLibraries();
    if (typeof validateWorktreeDirectory !== 'function') {
      return res.status(501).json({ error: 'validateWorktreeDirectory is not available' });
    }
    try {
      const { directory, worktreeRoot } = req.body || {};
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory is required' });
      }
      if (!worktreeRoot || typeof worktreeRoot !== 'string') {
        return res.status(400).json({ error: 'worktreeRoot is required' });
      }
      const result = await validateWorktreeDirectory(directory, worktreeRoot);
      res.json(result);
    } catch (error) {
      console.error('Failed to validate worktree directory:', error);
      res.status(500).json({ error: error.message || 'Failed to validate worktree directory' });
    }
  });

  // POST /api/git/canonicalize-worktree-state：把 worktree 状态归一化（孤儿条目等）；git 库未导出时 501。
  app.post('/api/git/canonicalize-worktree-state', async (req, res) => {
    const { canonicalizeWorktreeState } = await getGitLibraries();
    if (typeof canonicalizeWorktreeState !== 'function') {
      return res.status(501).json({ error: 'canonicalizeWorktreeState is not available' });
    }
    try {
      const { directory } = req.body || {};
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory is required' });
      }
      const result = await canonicalizeWorktreeState(directory);
      res.json(result);
    } catch (error) {
      console.error('Failed to canonicalize worktree state:', error);
      res.status(500).json({ error: error.message || 'Failed to canonicalize worktree state' });
    }
  });

  // GET /api/git/log：查询提交历史；支持 maxCount/from/to/file 过滤与 all（含全部分支）。
  app.get('/api/git/log', async (req, res) => {
    const { getLog } = await getGitLibraries();
    try {
      const directory = req.query.directory;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }

      const { maxCount, from, to, file } = req.query;
      const all = req.query.all === 'true';
      const log = await getLog(directory, {
        maxCount: maxCount ? parseInt(maxCount) : undefined,
        from,
        to,
        file,
        all
      });
      res.json(log);
    } catch (error) {
      console.error('Failed to get log:', error);
      res.status(500).json({ error: error.message || 'Failed to get commit log' });
    }
  });

  // GET /api/git/commit-files：列出指定提交涉及的文件。
  app.get('/api/git/commit-files', async (req, res) => {
    const { getCommitFiles } = await getGitLibraries();
    try {
      const { directory, hash } = req.query;
      if (!directory) {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      if (!hash) {
        return res.status(400).json({ error: 'hash parameter is required' });
      }

      const result = await getCommitFiles(directory, hash);
      res.json(result);
    } catch (error) {
      console.error('Failed to get commit files:', error);
      res.status(500).json({ error: error.message || 'Failed to get commit files' });
    }
  });

  // GET /api/git/commit-file-diff：取指定提交内某文件的 diff；hash 经 7-40 位十六进制校验，binary=true 走二进制路径。
  app.get('/api/git/commit-file-diff', async (req, res) => {
    const { getCommitFileDiff } = await getGitLibraries();
    try {
      const { directory, hash, path: filePath } = req.query;
      if (!directory || typeof directory !== 'string') {
        return res.status(400).json({ error: 'directory parameter is required' });
      }
      if (!hash || typeof hash !== 'string') {
        return res.status(400).json({ error: 'hash parameter is required' });
      }
      if (!/^[0-9a-fA-F]{7,40}$/.test(hash)) {
        return res.status(400).json({ error: 'hash must be a valid commit SHA' });
      }
      if (!filePath || typeof filePath !== 'string') {
        return res.status(400).json({ error: 'path parameter is required' });
      }

      const isBinary = req.query.binary === 'true';
      const result = await getCommitFileDiff(directory, hash, filePath, isBinary);
      res.json(result);
    } catch (error) {
      console.error('Failed to get commit file diff:', error);
      res.status(500).json({ error: error.message || 'Failed to get commit file diff' });
    }
  });

}
