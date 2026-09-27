/**
 * OpenCode 技能配置的 HTTP 路由层。
 *
 * 注册 /api/config/skills 下的全部端点：技能列表与元数据、支撑文件读写、
 * 技能 CRUD 与改名、目录（catalog）源列表、仓库扫描与安装。文件系统操作全部
 * 委托给注入的 skills.js 函数与 shared.js 工具；引擎侧数据通过本地引擎客户端
 * 拉取后与本地扫描结果合并（引擎优先）。写操作成功后统一返回“延迟重启”响应
 * （buildDeferredRestartResponse），提示客户端重启引擎后生效。
 */
import { createLocalEngineClient } from './local-engine-client.js';
import { buildDeferredRestartResponse } from './config-mutation-response.js';

/**
 * Matches how OpenCode reads its own boolean env flags: any value other than
 * unset, empty, "0" or "false" enables the flag.
 */
/**
 * 按 OpenCode 自身的约定解析布尔环境变量：未设置、空串、"0" 或 "false"
 * （忽略大小写与首尾空白）视为关闭，其余任何取值都视为开启。
 */
const isEnvFlagEnabled = (value) => {
  if (typeof value !== 'string') return false;
  const normalized = value.trim().toLowerCase();
  return normalized.length > 0 && normalized !== '0' && normalized !== 'false';
};

/**
 * 向 Express app 注册技能相关的全部 REST 路由（/api/config/skills 及其子路径）。
 *
 * 所有外部能力经 dependencies 注入：文件系统与路径工具、技能 CRUD
 * （skills.js 的导出）、OpenCode 引擎客户端参数、catalog 扫描与安装、
 * git 身份、项目目录解析等，便于测试整体替换。注册路由前还会构造少量
 * 内部辅助函数（worktree 根查找、scope/source 推断、引擎技能拉取、
 * 请求目录解析兜底等）。
 * @param {import('express').Express} app Express 应用实例
 * @param {object} dependencies 上述依赖集合
 */
export const registerSkillRoutes = (app, dependencies) => {
  const {
    fs,
    path,
    os,
    resolveProjectDirectory,
    resolveOptionalProjectDirectory,
    readSettingsFromDisk,
    sanitizeSkillCatalogs,
    isUnsafeSkillRelativePath,
    buildOpenCodeUrl,

    getOpenCodeAuthHeaders,
    getOpenCodePort,
    getSkillSources,
    discoverSkills,
    mergeDiscoveredSkills,
    createSkill,
    updateSkill,
    deleteSkill,
    renameSkill,
    isManagedSkillPath,
    readSkillSupportingFile,
    writeSkillSupportingFile,
    deleteSkillSupportingFile,
    SKILL_SCOPE,
    SKILL_DIR,
    getCuratedSkillsSources,
    getCacheKey,
    scanWithCache,
    parseSkillRepoSource,
    scanSkillsRepository,
    installSkillsFromRepository,
    fetchGitHubRepoMetas,
    getProfiles,
    getProfile,
  } = dependencies;

  /**
   * 向上查找包含 .git 的最近祖先目录作为 worktree 根；找不到返回 null。
   * 项目级技能的扫描边界与 scope 推断都以它为上限。
   */
  const findWorktreeRootForSkills = (workingDirectory) => {
    if (!workingDirectory) return null;
    let current = path.resolve(workingDirectory);
    while (true) {
      if (fs.existsSync(path.join(current, '.git'))) {
        return current;
      }
      const parent = path.dirname(current);
      if (parent === current) {
        return null;
      }
      current = parent;
    }
  };

  /**
   * 列出从 workingDirectory 逐级向上直至 worktree 根（含自身）的所有目录；
   * 没有 worktree 根时仅包含工作目录本身。用于在多级仓库结构中查找项目级技能。
   */
  const getSkillProjectAncestors = (workingDirectory) => {
    if (!workingDirectory) return [];
    const result = [];
    let current = path.resolve(workingDirectory);
    const stop = findWorktreeRootForSkills(workingDirectory) || current;
    while (true) {
      result.push(current);
      if (current === stop) break;
      const parent = path.dirname(current);
      if (parent === current) break;
      current = parent;
    }
    return result;
  };

  /**
   * 判断解析后的 candidatePath 是否等于 parentPath 或位于其目录树内；
   * 前缀匹配强制带路径分隔符，避免同名前缀目录（/foo/bar 与 /foo/bar-baz）误判。
   */
  const isPathInside = (candidatePath, parentPath) => {
    if (!candidatePath || !parentPath) return false;
    const normalizedCandidate = path.resolve(candidatePath);
    const normalizedParent = path.resolve(parentPath);
    return normalizedCandidate === normalizedParent || normalizedCandidate.startsWith(`${normalizedParent}${path.sep}`);
  };

  /**
   * 从技能的绝对路径反推其 scope 与 source。
   * source 按路径片段判断：位于 `.agents/skills` 下为 'agents'，`.claude/skills`
   * 下为 'claude'，否则为 'opencode'。scope：路径落在任一项目祖先的
   * .opencode、.claude/skills、.agents/skills 或 .omp/skills 内为 project，
   * 落在用户级配置根（含 OPENCODE_CONFIG_DIR 覆盖值）内为 user；
   * 无法判定时保守地按 user 处理。
   */
  const inferSkillScopeAndSourceFromPath = (skillPath, workingDirectory) => {
    const resolvedPath = typeof skillPath === 'string' ? path.resolve(skillPath) : '';
    const home = os.homedir();
    const source = resolvedPath.includes(`${path.sep}.agents${path.sep}skills${path.sep}`)
      ? 'agents'
      : resolvedPath.includes(`${path.sep}.claude${path.sep}skills${path.sep}`)
        ? 'claude'
        : 'opencode';

    const projectAncestors = getSkillProjectAncestors(workingDirectory);
    const isProjectScoped = projectAncestors.some((ancestor) => {
      const candidates = [
        path.join(ancestor, '.opencode'),
        path.join(ancestor, '.claude', 'skills'),
        path.join(ancestor, '.agents', 'skills'),
        path.join(ancestor, '.omp', 'skills'),
      ];
      return candidates.some((candidate) => isPathInside(resolvedPath, candidate));
    });

    if (isProjectScoped) {
      return { scope: SKILL_SCOPE.PROJECT, source };
    }
    const userRoots = [
      path.join(home, '.config', 'opencode'),
      path.join(home, '.opencode'),
      path.join(home, '.omp', 'agent', 'skills'),
      path.join(home, '.claude', 'skills'),
      path.join(home, '.agents', 'skills'),
      process.env.OPENCODE_CONFIG_DIR ? path.resolve(process.env.OPENCODE_CONFIG_DIR) : null,
    ].filter(Boolean);

    if (userRoots.some((root) => isPathInside(resolvedPath, root))) {
      return { scope: SKILL_SCOPE.USER, source };
    }

    return { scope: SKILL_SCOPE.USER, source };
  };

  /**
   * 调用本地 OpenCode 引擎的 skills 接口，拉取引擎实际发现的技能（8 秒超时）。
   * 引擎未运行（无端口）、响应异常或请求失败时返回空数组（仅记录错误日志），
   * 保证本地文件系统扫描结果仍然可用。location 为 '<built-in>' 的条目映射为
   * 不可落盘读写的内置技能对象；其余条目按路径推断 scope/source，
   * 引擎附带 content 时原样透传。
   */
  const fetchOpenCodeDiscoveredSkills = async (workingDirectory) => {
    if (!getOpenCodePort()) {
      return [];
    }

    try {
      const client = createLocalEngineClient({
        baseUrl: buildOpenCodeUrl('/', '').replace(/\/$/, ''),
        directory: workingDirectory || undefined,
        headers: getOpenCodeAuthHeaders(),
        fetch: (request) => fetch(request, { signal: AbortSignal.timeout(8_000) }),
      });

      const response = await client.app.skills(
        workingDirectory ? { directory: workingDirectory } : undefined,
      );
      const payload = response?.data;
      if (!Array.isArray(payload)) {
        return [];
      }

      return payload
        .map((item) => {
          const name = typeof item?.name === 'string' ? item.name.trim() : '';
          const location = typeof item?.location === 'string' ? item.location : '';
          const description = typeof item?.description === 'string' ? item.description : '';
          const content = typeof item?.content === 'string' ? item.content : '';
          if (!name || !location) {
            return null;
          }
          if (location === '<built-in>') {
            return {
              name,
              path: location,
              scope: SKILL_SCOPE.USER,
              source: 'opencode',
              description,
              content,
            };
          }
          const inferred = inferSkillScopeAndSourceFromPath(location, workingDirectory);
          const skill = {
            name,
            path: location,
            scope: inferred.scope,
            source: inferred.source,
            description,
          };
          if (content) {
            skill.content = content;
          }
          return skill;
        })
        .filter(Boolean);
    } catch (error) {
      console.error('Failed to list OpenCode skills:', error);
      return [];
    }
  };

  /**
   * 列出可供选择的 git 身份（仅 id 与 name），附加在认证失败的响应里；
   * 读取 profiles 失败时返回空数组。
   */
  const listGitIdentitiesForResponse = () => {
    try {
      const profiles = getProfiles();
      return profiles.map((p) => ({ id: p.id, name: p.name }));
    } catch {
      return [];
    }
  };

  /**
   * 把 profileId 解析为仓库操作所需的 SSH 身份（取 profile 配置的 sshKey）；
   * 无 profileId、profile 不存在、未配置 sshKey 或读取失败时返回 null，
   * 走默认的无认证访问路径。
   */
  const resolveGitIdentity = (profileId) => {
    if (!profileId) {
      return null;
    }
    try {
      const profile = getProfile(profileId);
      const sshKey = profile?.sshKey;
      if (typeof sshKey === 'string' && sshKey.trim()) {
        return { sshKey: sshKey.trim() };
      }
    } catch {
      // ignore
    }
    return null;
  };

  // Prefer an explicit request directory, then soft-fallback to the active
  // project / lastDirectory so repository-local skills stay visible when the
  // client omits `directory` (create already used resolveProjectDirectory).
  /**
   * 解析请求关联的工作目录：优先采用请求显式携带的目录
   * （resolveOptionalProjectDirectory），为空时软兜底到当前激活项目或
   * lastDirectory。兜底解析抛错可容忍——仅浏览用户级技能时没有项目目录
   * 也是合法状态。返回 { directory, error }，error 非空表示目录参数非法。
   */
  const resolveSkillsDirectory = async (req) => {
    const optional = await resolveOptionalProjectDirectory(req);
    if (optional.error) {
      return optional;
    }
    if (optional.directory) {
      return optional;
    }

    try {
      const fallback = await resolveProjectDirectory(req);
      if (fallback.directory) {
        return { directory: fallback.directory, error: null };
      }
    } catch {
      // ignore — listing user-scoped skills without a project is valid
    }

    return { directory: null, error: null };
  };

  /**
   * GET /api/config/skills — 列出当前可用的全部技能。
   *
   * 合并引擎上报技能与本地扫描结果（引擎优先），并为每项附加 sources 元数据与
   * renamable 标记（路径位于托管目录内且非内置技能才可改名）。同时返回
   * OPENCODE_DISABLE_* 环境标志的生效状态（externalSkills），供客户端过滤掉
   * 引擎实际不会加载的外部技能。目录参数非法返回 400，扫描异常返回 500。
   */
  app.get('/api/config/skills', async (req, res) => {
    try {
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const openCodeSkills = await fetchOpenCodeDiscoveredSkills(directory);
      const localSkills = discoverSkills(directory);
      const skills = mergeDiscoveredSkills(openCodeSkills, localSkills);

      const enrichedSkills = skills.map((skill) => {
        const sources = getSkillSources(skill.name, directory, skill);
        const skillPath = typeof skill.path === 'string' ? skill.path : null;
        return {
          ...skill,
          sources,
          renamable: Boolean(
            skillPath
            && skillPath !== '<built-in>'
            && isManagedSkillPath(skillPath, directory)
          ),
        };
      });

      // OpenCode decides which external skill roots it loads from process
      // env, and the browser cannot read that. Report the flags alongside the
      // scan so the client can narrow its list to what the agent can actually
      // invoke.
      //
      // OpenCode's own skill-list endpoint is not usable for this: on 1.18.14
      // it returns only global and builtin skills, omitting the project
      // `.agents`/`.claude` skills the agent demonstrably has.
      res.json({
        skills: enrichedSkills,
        externalSkills: {
          // `OPENCODE_DISABLE_CLAUDE_CODE` is the broad switch; the specific
          // one wins independently — OpenCode ORs them.
          claudeDisabled: isEnvFlagEnabled(process.env.OPENCODE_DISABLE_CLAUDE_CODE)
            || isEnvFlagEnabled(process.env.OPENCODE_DISABLE_CLAUDE_CODE_SKILLS),
          allDisabled: isEnvFlagEnabled(process.env.OPENCODE_DISABLE_EXTERNAL_SKILLS),
        },
      });
    } catch (error) {
      console.error('Failed to list skills:', error);
      res.status(500).json({ error: 'Failed to list skills' });
    }
  });

  /**
   * GET /api/config/skills/catalog — 列出技能目录（catalog）的安装源。
   *
   * 合并内置精选源与用户设置里的自定义源（经 sanitizeSkillCatalogs 清洗），
   * 并为 github.com 源批量附加 star 数与仓库更新时间（fetchGitHubRepoMetas）。
   * 响应固定携带 itemsBySource 空对象；目录项改由 source 端点按需加载。
   * 目录参数非法返回 400，其余异常返回 500。
   */
  app.get('/api/config/skills/catalog', async (req, res) => {
    try {
      const { error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      const curatedSources = getCuratedSkillsSources();
      const settings = await readSettingsFromDisk();
      const customSourcesRaw = sanitizeSkillCatalogs(settings.skillCatalogs) || [];

      const customSources = customSourcesRaw.map((entry) => ({
        id: entry.id,
        label: entry.label,
        description: entry.source,
        source: entry.source,
        defaultSubpath: entry.subpath,
        gitIdentityId: entry.gitIdentityId,
      }));

      const sources = [...curatedSources, ...customSources];

      const githubRepos = sources
        .map((src) => parseSkillRepoSource(src.source))
        .filter((parsed) => parsed.ok && parsed.host === 'github.com')
        .map((parsed) => parsed.normalizedRepo);
      const repoMetas = await fetchGitHubRepoMetas(githubRepos);

      const sourcesForUi = sources.map(({ gitIdentityId, ...rest }) => {
        const parsed = parseSkillRepoSource(rest.source);
        const meta = parsed.ok && parsed.host === 'github.com'
          ? repoMetas[parsed.normalizedRepo] || {}
          : {};
        return {
          ...rest,
          stars: typeof meta.stars === 'number' ? meta.stars : null,
          repoUpdatedAt: typeof meta.repoUpdatedAt === 'string' ? meta.repoUpdatedAt : null,
        };
      });

      res.json({ ok: true, sources: sourcesForUi, itemsBySource: {} });
    } catch (error) {
      console.error('Failed to load skills catalog:', error);
      res.status(500).json({ ok: false, error: { kind: 'unknown', message: error.message || 'Failed to load catalog' } });
    }
  });

  /**
   * GET /api/config/skills/catalog/source?sourceId=&refresh= — 加载某个目录源中
   * 可安装的技能条目。
   *
   * 按 sourceId 匹配精选或自定义源，解析仓库坐标后经 scanWithCache 扫描
   * （refresh=true 绕过缓存强制重扫），再把每个条目与本地已安装技能按名称
   * 比对，标注 installed 及其 scope/source。缺 sourceId 或源坐标非法返回 400，
   * 未知 sourceId 返回 404，扫描失败返回 500。
   */
  app.get('/api/config/skills/catalog/source', async (req, res) => {
    try {
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ ok: false, error: { kind: 'invalidSource', message: error } });
      }

      const sourceId = typeof req.query.sourceId === 'string' ? req.query.sourceId : null;
      if (!sourceId) {
        return res.status(400).json({ ok: false, error: { kind: 'invalidSource', message: 'Missing sourceId' } });
      }

      const refresh = String(req.query.refresh || '').toLowerCase() === 'true';

      const curatedSources = getCuratedSkillsSources();
      const settings = await readSettingsFromDisk();
      const customSourcesRaw = sanitizeSkillCatalogs(settings.skillCatalogs) || [];

      const customSources = customSourcesRaw.map((entry) => ({
        id: entry.id,
        label: entry.label,
        description: entry.source,
        source: entry.source,
        defaultSubpath: entry.subpath,
        gitIdentityId: entry.gitIdentityId,
      }));

      const sources = [...curatedSources, ...customSources];
      const src = sources.find((entry) => entry.id === sourceId);

      if (!src) {
        return res.status(404).json({ ok: false, error: { kind: 'invalidSource', message: 'Unknown source' } });
      }

      const resolvedDiscovered = mergeDiscoveredSkills(
        await fetchOpenCodeDiscoveredSkills(directory),
        discoverSkills(directory),
      );
      const installedByName = new Map(resolvedDiscovered.map((s) => [s.name, s]));

      const parsed = parseSkillRepoSource(src.source);
      if (!parsed.ok) {
        return res.status(400).json({ ok: false, error: parsed.error });
      }

      const effectiveSubpath = src.defaultSubpath || parsed.effectiveSubpath || null;
      const cacheKey = getCacheKey({
        normalizedRepo: parsed.normalizedRepo,
        subpath: effectiveSubpath || '',
        identityId: src.gitIdentityId || '',
      });

      const scanResult = await scanWithCache(
        cacheKey,
        () => scanSkillsRepository({
          source: src.source,
          subpath: src.defaultSubpath,
          defaultSubpath: src.defaultSubpath,
          identity: resolveGitIdentity(src.gitIdentityId),
        }),
        { refresh },
      );

      if (!scanResult.ok) {
        return res.status(500).json({ ok: false, error: scanResult.error });
      }

      const items = (scanResult.items || []).map((item) => {
        const installed = installedByName.get(item.skillName);
        return {
          sourceId: src.id,
          ...item,
          gitIdentityId: src.gitIdentityId,
          installed: installed
            ? { isInstalled: true, scope: installed.scope, source: installed.source }
            : { isInstalled: false },
        };
      });

      return res.json({ ok: true, items });
    } catch (error) {
      console.error('Failed to load catalog source:', error);
      return res.status(500).json({
        ok: false,
        error: { kind: 'unknown', message: error.message || 'Failed to load catalog source' },
      });
    }
  });

  /**
   * POST /api/config/skills/scan — 按需扫描任意技能仓库（添加目录源流程使用）。
   *
   * 请求体为 { source, subpath, gitIdentityId }。仓库需要 SSH 认证时返回 401
   * 并附上可用 git 身份列表（identities）；其余扫描错误返回 400；
   * 成功返回条目列表。服务器异常返回 500。
   */
  app.post('/api/config/skills/scan', async (req, res) => {
    try {
      const { source, subpath, gitIdentityId } = req.body || {};
      const identity = resolveGitIdentity(gitIdentityId);

      const result = await scanSkillsRepository({
        source,
        subpath,
        identity,
      });

      if (!result.ok) {
        if (result.error?.kind === 'authRequired') {
          return res.status(401).json({
            ok: false,
            error: {
              ...result.error,
              identities: listGitIdentitiesForResponse(),
            },
          });
        }

        return res.status(400).json({ ok: false, error: result.error });
      }

      res.json({ ok: true, items: result.items });
    } catch (error) {
      console.error('Failed to scan skills repository:', error);
      res.status(500).json({ ok: false, error: { kind: 'unknown', message: error.message || 'Failed to scan repository' } });
    }
  });

  /**
   * POST /api/config/skills/install — 从技能仓库安装选中的技能。
   *
   * scope 为 project 时必须能解析出项目目录，否则 400。安装出现同名冲突时
   * 返回 409 并携带冲突详情（供 conflictDecisions 决策后重试）；仓库需要
   * 认证返回 401（附身份列表）；其他失败返回 400。只要有技能被安装，
   * 响应就附带延迟重启提示（buildDeferredRestartResponse），否则返回
   * “未安装任何技能”的普通消息。服务器异常返回 500。
   */
  app.post('/api/config/skills/install', async (req, res) => {
    try {
      const {
        source,
        subpath,
        gitIdentityId,
        scope,
        targetSource,
        selections,
        conflictPolicy,
        conflictDecisions,
      } = req.body || {};

      let workingDirectory = null;
      if (scope === 'project') {
        const resolved = await resolveProjectDirectory(req);
        if (!resolved.directory) {
          return res.status(400).json({
            ok: false,
            error: { kind: 'invalidSource', message: resolved.error || 'Project installs require a directory parameter' },
          });
        }
        workingDirectory = resolved.directory;
      }

      const identity = resolveGitIdentity(gitIdentityId);

      const result = await installSkillsFromRepository({
        source,
        subpath,
        identity,
        scope,
        targetSource,
        workingDirectory,
        userSkillDir: SKILL_DIR,
        selections,
        conflictPolicy,
        conflictDecisions,
      });

      if (!result.ok) {
        if (result.error?.kind === 'conflicts') {
          return res.status(409).json({ ok: false, error: result.error });
        }

        if (result.error?.kind === 'authRequired') {
          return res.status(401).json({
            ok: false,
            error: {
              ...result.error,
              identities: listGitIdentitiesForResponse(),
            },
          });
        }

        return res.status(400).json({ ok: false, error: result.error });
      }

      const installed = result.installed || [];
      const skipped = result.skipped || [];
      const requiresRestart = installed.length > 0;

      res.json({
        ok: true,
        installed,
        skipped,
        ...(requiresRestart
          ? buildDeferredRestartResponse('Skills installed successfully. Restart the engine to apply.')
          : {
            requiresReload: false,
            message: 'No skills were installed',
          }),
      });
    } catch (error) {
      console.error('Failed to install skills:', error);
      res.status(500).json({ ok: false, error: { kind: 'unknown', message: error.message || 'Failed to install skills' } });
    }
  });

  /**
   * GET /api/config/skills/:name — 读取单个技能的来源元数据。
   *
   * 返回 name、sources（各候选位置与当前生效来源的详情）、scope、source 与
   * exists；引擎发现结果优先，用于补全内置技能等无本地文件的条目。
   * 目录参数非法返回 400，读取异常返回 500。
   */
  app.get('/api/config/skills/:name', async (req, res) => {
    try {
      const skillName = req.params.name;
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const discoveredSkill = (await fetchOpenCodeDiscoveredSkills(directory))
        .find((skill) => skill.name === skillName) || null;
      const sources = getSkillSources(skillName, directory, discoveredSkill);

      res.json({
        name: skillName,
        sources: sources,
        scope: sources.md.scope,
        source: sources.md.source,
        exists: sources.md.exists
      });
    } catch (error) {
      console.error('Failed to get skill sources:', error);
      res.status(500).json({ error: 'Failed to get skill configuration metadata' });
    }
  });

  /**
   * GET /api/config/skills/:name/files/*filePath — 读取技能的支撑文件内容。
   *
   * 相对路径经 isUnsafeSkillRelativePath 校验（拒绝绝对路径与目录穿越）并
   * decodeURIComponent 解码后，从技能目录读取。技能或文件不存在返回 404；
   * EACCES/EPERM 返回 403；其余异常返回 500。
   */
  app.get('/api/config/skills/:name/files/*filePath', async (req, res) => {
    try {
      const skillName = req.params.name;
      const filePath = decodeURIComponent(req.params.filePath);
      if (isUnsafeSkillRelativePath(filePath)) {
        return res.status(400).json({ error: 'Invalid file path' });
      }
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      const discoveredSkill = (await fetchOpenCodeDiscoveredSkills(directory))
        .find((skill) => skill.name === skillName) || null;
      const sources = getSkillSources(skillName, directory, discoveredSkill);
      if (!sources.md.exists || !sources.md.dir) {
        return res.status(404).json({ error: 'Skill not found' });
      }

      const content = readSkillSupportingFile(sources.md.dir, filePath);
      if (content === null) {
        return res.status(404).json({ error: 'File not found' });
      }

      res.json({ path: filePath, content });
    } catch (error) {
      if (error && typeof error === 'object' && (error.code === 'EACCES' || error.code === 'EPERM')) {
        return res.status(403).json({ error: 'Access to file denied' });
      }
      console.error('Failed to read skill file:', error);
      res.status(500).json({ error: 'Failed to read skill file' });
    }
  });

  /**
   * POST /api/config/skills/:name — 创建技能。
   *
   * 请求体除技能内容（description、instructions、supportingFiles 及其余
   * frontmatter 字段）外可携带 scope 与 source。project scope 必须能解析出
   * 项目目录，否则 400。成功返回延迟重启提示（需重启引擎生效）；
   * 创建失败（重名、描述缺失、名称非法等）返回 500 与错误消息。
   */
  app.post('/api/config/skills/:name', async (req, res) => {
    try {
      const skillName = req.params.name;
      const { scope, source: skillSource, ...config } = req.body;
      const { directory, error } = scope === SKILL_SCOPE.PROJECT
        ? await resolveProjectDirectory(req)
        : await resolveSkillsDirectory(req);
      if (error || (scope === SKILL_SCOPE.PROJECT && !directory)) {
        return res.status(400).json({ error: error || 'Project skill creation requires a directory' });
      }

      console.log('[Server] Creating skill:', skillName);
      console.log('[Server] Scope:', scope, 'Working directory:', directory);

      createSkill(skillName, { ...config, source: skillSource }, directory, scope);
      res.json(buildDeferredRestartResponse(
        `Skill ${skillName} created successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to create skill:', error);
      res.status(500).json({ error: error.message || 'Failed to create skill' });
    }
  });

  /**
   * PATCH /api/config/skills/:name — 更新技能；updates.renameTo 非空时走改名分支。
   *
   * 改名分支：renameSkill 成功后调用 refreshOpenCodeAfterConfigChange 刷新引擎，
   * 返回 requiresReload 与 reloadDelayMs，让客户端延迟后自行刷新界面。
   * 普通更新分支：updateSkill 处理 frontmatter、正文与支撑文件，
   * 返回延迟重启提示。两个分支的失败均返回 500 与错误消息。
   */
  app.patch('/api/config/skills/:name', async (req, res) => {
    try {
      const skillName = req.params.name;
      const updates = req.body;
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      if (typeof updates?.renameTo === 'string') {
        const newName = updates.renameTo.trim();
        console.log(`[Server] Renaming skill: ${skillName} -> ${newName}`);
        console.log('[Server] Working directory:', directory);
        renameSkill(skillName, newName, directory);
        await refreshOpenCodeAfterConfigChange('skill rename');

        return res.json({
          success: true,
          name: newName,
          requiresReload: true,
          message: `Skill renamed to ${newName} successfully. Reloading interface…`,
          reloadDelayMs: clientReloadDelayMs,
        });
      }

      console.log(`[Server] Updating skill: ${skillName}`);
      console.log('[Server] Working directory:', directory);

      updateSkill(skillName, updates, directory, updates?.targetPath);
      res.json(buildDeferredRestartResponse(
        `Skill ${skillName} updated successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('[Server] Failed to update skill:', error);
      res.status(500).json({ error: error.message || 'Failed to update skill' });
    }
  });

  /**
   * PUT /api/config/skills/:name/files/*filePath — 新建或覆盖支撑文件。
   *
   * 相对路径安全校验同 GET 端点；技能不存在返回 404；content 缺省按空字符串
   * 写入（即清空文件）。EACCES/EPERM 返回 403，其余异常返回 500。
   */
  app.put('/api/config/skills/:name/files/*filePath', async (req, res) => {
    try {
      const skillName = req.params.name;
      const filePath = decodeURIComponent(req.params.filePath);
      if (isUnsafeSkillRelativePath(filePath)) {
        return res.status(400).json({ error: 'Invalid file path' });
      }
      const { content } = req.body;
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      const discoveredSkill = (await fetchOpenCodeDiscoveredSkills(directory))
        .find((skill) => skill.name === skillName) || null;
      const sources = getSkillSources(skillName, directory, discoveredSkill);
      if (!sources.md.exists || !sources.md.dir) {
        return res.status(404).json({ error: 'Skill not found' });
      }

      writeSkillSupportingFile(sources.md.dir, filePath, content || '');

      res.json({
        success: true,
        message: `File ${filePath} saved successfully`,
      });
    } catch (error) {
      if (error && typeof error === 'object' && (error.code === 'EACCES' || error.code === 'EPERM')) {
        return res.status(403).json({ error: 'Access to file denied' });
      }
      console.error('Failed to write skill file:', error);
      res.status(500).json({ error: error.message || 'Failed to write skill file' });
    }
  });

  /**
   * DELETE /api/config/skills/:name/files/*filePath — 删除支撑文件。
   *
   * 相对路径安全校验同 GET 端点；技能不存在返回 404；EACCES/EPERM 返回 403，
   * 其余异常返回 500。
   */
  app.delete('/api/config/skills/:name/files/*filePath', async (req, res) => {
    try {
      const skillName = req.params.name;
      const filePath = decodeURIComponent(req.params.filePath);
      if (isUnsafeSkillRelativePath(filePath)) {
        return res.status(400).json({ error: 'Invalid file path' });
      }
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      const discoveredSkill = (await fetchOpenCodeDiscoveredSkills(directory))
        .find((skill) => skill.name === skillName) || null;
      const sources = getSkillSources(skillName, directory, discoveredSkill);
      if (!sources.md.exists || !sources.md.dir) {
        return res.status(404).json({ error: 'Skill not found' });
      }

      deleteSkillSupportingFile(sources.md.dir, filePath);

      res.json({
        success: true,
        message: `File ${filePath} deleted successfully`,
      });
    } catch (error) {
      if (error && typeof error === 'object' && (error.code === 'EACCES' || error.code === 'EPERM')) {
        return res.status(403).json({ error: 'Access to file denied' });
      }
      console.error('Failed to delete skill file:', error);
      res.status(500).json({ error: error.message || 'Failed to delete skill file' });
    }
  });

  /**
   * DELETE /api/config/skills/:name — 删除技能。
   *
   * 移除各约定位置下同名技能的整个目录；成功返回延迟重启提示，
   * 技能不存在等原因抛错时返回 500 与错误消息。
   */
  app.delete('/api/config/skills/:name', async (req, res) => {
    try {
      const skillName = req.params.name;
      const { directory, error } = await resolveSkillsDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }

      deleteSkill(skillName, directory);
      res.json(buildDeferredRestartResponse(
        `Skill ${skillName} deleted successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to delete skill:', error);
      res.status(500).json({ error: error.message || 'Failed to delete skill' });
    }
  });
};
