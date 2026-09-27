/**
 * 项目自定义图标路由模块。
 *
 * 挂载 /api/projects/:projectId/icon 系列端点：GET 读取图标（SVG 可
 * 按 theme / iconColor 查询参数注入主题色）、PUT 上传（data URL 形式，
 * 仅 PNG/JPEG/SVG，上限 5MB）、DELETE 清除、POST discover 在项目目录
 * 自动发现 favicon 并落为 auto 图标。图标文件统一存放在 OMPChamber
 * 数据目录的 project-icons 下（文件名用 projectId 的 SHA-1 哈希，避免
 * 任意 id 直接进入文件路径），元数据（mime / updatedAt / source）写在
 * settings.json 对应 projects[].iconImage 上。全部依赖经注入以便测试。
 */
/**
 * 在 express app 上注册项目图标路由。
 *
 * @param app express 应用实例
 * @param dependencies 注入依赖：fsPromises / path / crypto 为文件与
 *   哈希工具；ompchamberDataDir 为图标存储根目录；sanitizeProjects
 *   为 settings.projects 清洗函数（复用 settings-normalization-runtime）；
 *   readSettingsFromDiskMigrated / persistSettings 为 settings 读写；
 *   createFsSearchRuntime 为 favicon 搜索运行时工厂；spawn 与
 *   resolveGitBinaryForSpawn 供文件搜索调用 git。
 */
export const registerProjectIconRoutes = (app, dependencies) => {
  const {
    fsPromises,
    path,
    crypto,
    ompchamberDataDir,
    sanitizeProjects,
    readSettingsFromDiskMigrated,
    persistSettings,
    createFsSearchRuntime,
    spawn,
    resolveGitBinaryForSpawn,
  } = dependencies;

  // 图标文件存放目录：<OMPChamber 数据目录>/project-icons。
  const projectIconsDirPath = path.join(ompchamberDataDir, 'project-icons');
  // 支持的图标 MIME 到文件扩展名的映射（决定落盘文件名与读取候选）。
  const projectIconMimeToExtension = {
    'image/png': 'png',
    'image/jpeg': 'jpg',
    'image/svg+xml': 'svg',
    'image/webp': 'webp',
    'image/x-icon': 'ico',
  };
  // 反向映射：扩展名 -> MIME，用于从磁盘文件名推断 Content-Type。
  const projectIconExtensionToMime = Object.fromEntries(
    Object.entries(projectIconMimeToExtension).map(([mime, ext]) => [ext, mime])
  );
  // 支持的 MIME 集合（normalizeProjectIconMime 的白名单）。
  const projectIconSupportedMimes = new Set(Object.keys(projectIconMimeToExtension));
  // 图标字节上限：5MB（上传与自动发现共用同一限制）。
  const projectIconMaxBytes = 5 * 1024 * 1024;
  // SVG 主题化默认颜色：light / dark 两档。
  const projectIconThemeColors = {
    light: '#111111',
    dark: '#f5f5f5',
  };
  // 自定义 iconColor 的合法格式：#RGB / #RGBA / #RRGGBB / #RRGGBBAA。
  const projectIconHexColorPattern = /^#(?:[\da-fA-F]{3}|[\da-fA-F]{4}|[\da-fA-F]{6}|[\da-fA-F]{8})$/;

  // 规范化 MIME：小写化并把 image/jpg 别名归一为 image/jpeg；不在
  // 白名单内（或非字符串）返回 null。
  const normalizeProjectIconMime = (value) => {
    if (typeof value !== 'string') {
      return null;
    }

    const normalized = value.trim().toLowerCase();
    if (normalized === 'image/jpg') {
      return 'image/jpeg';
    }
    if (projectIconSupportedMimes.has(normalized)) {
      return normalized;
    }
    return null;
  };

  // 生成图标文件基础名：project- 加 projectId 的 SHA-1 十六进制摘要，
  // 避免把任意 id 直接拼进文件系统路径。
  const projectIconBaseName = (projectId) => {
    const hash = crypto.createHash('sha1').update(projectId).digest('hex');
    return `project-${hash}`;
  };

  // 按 projectId + MIME 拼出图标文件的绝对路径；MIME 非法返回 null。
  const projectIconPathForMime = (projectId, mime) => {
    const normalizedMime = normalizeProjectIconMime(mime);
    if (!normalizedMime) {
      return null;
    }
    const ext = projectIconMimeToExtension[normalizedMime];
    return path.join(projectIconsDirPath, `${projectIconBaseName(projectId)}.${ext}`);
  };

  // 列出某项目全部可能的图标文件路径（五种扩展名），供读取时的候选
  // 回退与删除替换时的全量清理使用。
  const projectIconPathCandidates = (projectId) => {
    const base = projectIconBaseName(projectId);
    return Object.values(projectIconMimeToExtension).map((ext) => path.join(projectIconsDirPath, `${base}.${ext}`));
  };

  // 删除项目所有扩展名的图标文件（keepPath 指定的除外，上传替换时
  // 保留新文件）；ENOENT 静默忽略，其它错误向上抛出。
  const removeProjectIconFiles = async (projectId, keepPath) => {
    const candidates = projectIconPathCandidates(projectId);
    await Promise.all(candidates.map(async (candidatePath) => {
      if (keepPath && candidatePath === keepPath) {
        return;
      }
      try {
        await fsPromises.unlink(candidatePath);
      } catch (error) {
        if (!error || typeof error !== 'object' || error.code !== 'ENOENT') {
          throw error;
        }
      }
    }));
  };

  // 解析并校验上传的 data URL 图标：必须匹配 data:<mime>;base64,<体>
  // 且 MIME 属于 PNG/JPEG/SVG；解码后须非空且不超过 5MB。返回
  // { ok: true, mime, bytes } 或 { ok: false, error }（error 直接作为
  // 400 响应文案）。
  const parseProjectIconDataUrl = (value) => {
    if (typeof value !== 'string') {
      return { ok: false, error: 'dataUrl is required' };
    }

    const trimmed = value.trim();
    const match = trimmed.match(/^data:([^;,]+);base64,([A-Za-z0-9+/=\s]+)$/i);
    if (!match) {
      return { ok: false, error: 'Invalid dataUrl format' };
    }

    const mime = normalizeProjectIconMime(match[1]);
    if (!mime || !['image/png', 'image/jpeg', 'image/svg+xml'].includes(mime)) {
      return { ok: false, error: 'Icon must be PNG, JPEG, or SVG' };
    }

    try {
      const base64 = match[2].replace(/\s+/g, '');
      const bytes = Buffer.from(base64, 'base64');
      if (bytes.length === 0) {
        return { ok: false, error: 'Icon content is empty' };
      }
      if (bytes.length > projectIconMaxBytes) {
        return { ok: false, error: 'Icon exceeds size limit (5 MB)' };
      }
      return { ok: true, mime, bytes };
    } catch {
      return { ok: false, error: 'Failed to decode icon data' };
    }
  };

  // 规范化 theme 查询参数：仅接受 light / dark（大小写不敏感），
  // 其余返回 null。
  const normalizeProjectIconThemeVariant = (value) => {
    if (typeof value !== 'string') {
      return null;
    }

    const normalized = value.trim().toLowerCase();
    if (normalized === 'light' || normalized === 'dark') {
      return normalized;
    }
    return null;
  };

  // 规范化 iconColor 查询参数：必须匹配 hex 颜色正则，其余返回 null。
  const normalizeProjectIconColor = (value) => {
    if (typeof value !== 'string') {
      return null;
    }

    const normalized = value.trim();
    if (!projectIconHexColorPattern.test(normalized)) {
      return null;
    }
    return normalized;
  };

  // 给 SVG 注入主题色：在 <svg> 开标签后插入一段覆盖 :root 颜色的
  // <style>，颜色取显式 iconColor，否则取主题档位默认色；非字符串
  // 输入、无可用颜色或找不到 <svg> 标签时原样返回。只做字符串拼接、
  // 不解析 XML，因此不会重排原文档结构。
  const applyProjectIconSvgTheme = (svgMarkup, themeVariant, iconColor) => {
    if (typeof svgMarkup !== 'string') {
      return svgMarkup;
    }

    const color = iconColor || projectIconThemeColors[themeVariant];
    if (!color) {
      return svgMarkup;
    }

    const svgTagIndex = svgMarkup.search(/<svg\b/i);
    if (svgTagIndex === -1) {
      return svgMarkup;
    }

    const svgOpenTagEndIndex = svgMarkup.indexOf('>', svgTagIndex);
    if (svgOpenTagEndIndex === -1) {
      return svgMarkup;
    }

    const overrideStyle = `<style data-ompchamber-theme-icon="1">:root{color:${color}!important;}</style>`;
    return `${svgMarkup.slice(0, svgOpenTagEndIndex + 1)}${overrideStyle}${svgMarkup.slice(svgOpenTagEndIndex + 1)}`;
  };

  // 在 settings 中按 id 定位项目：先用 sanitizeProjects 规范化再查找，
  // 返回 { projects, index, project }；找不到时 index 为 -1、project
  // 为 null。
  const findProjectById = (settings, projectId) => {
    const projects = sanitizeProjects(settings?.projects) || [];
    const index = projects.findIndex((project) => project.id === projectId);
    if (index === -1) {
      return { projects, index: -1, project: null };
    }
    return { projects, index, project: projects[index] };
  };

  // 构建 favicon 自动发现用的文件搜索运行时（复用注入的 fs/spawn 依赖）。
  const fsSearchRuntime = createFsSearchRuntime({
    fsPromises,
    path,
    spawn,
    resolveGitBinaryForSpawn,
  });

  // GET /api/projects/:projectId/icon — 返回项目图标文件：优先读
  // iconImage.mime 指定的路径，读不到再按扩展名候选回退；SVG 且带
  // theme / iconColor 查询参数时注入主题色后返回；响应带一年
  // immutable 缓存头。projectId 缺失 400、项目或图标不存在 404、
  // 读失败 500。
  app.get('/api/projects/:projectId/icon', async (req, res) => {
    const projectId = typeof req.params.projectId === 'string' ? req.params.projectId.trim() : '';
    if (!projectId) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      const { project } = findProjectById(settings, projectId);
      if (!project) {
        return res.status(404).json({ error: 'Project not found' });
      }

      const metadataMime = normalizeProjectIconMime(project.iconImage?.mime);
      const preferredPath = metadataMime ? projectIconPathForMime(projectId, metadataMime) : null;
      const candidates = preferredPath
        ? [preferredPath, ...projectIconPathCandidates(projectId).filter((candidate) => candidate !== preferredPath)]
        : projectIconPathCandidates(projectId);

      const themeQuery = Array.isArray(req.query?.theme) ? req.query.theme[0] : req.query?.theme;
      const requestedThemeVariant = normalizeProjectIconThemeVariant(themeQuery);
      const iconColorQuery = Array.isArray(req.query?.iconColor) ? req.query.iconColor[0] : req.query?.iconColor;
      const requestedIconColor = normalizeProjectIconColor(iconColorQuery);

      for (const iconPath of candidates) {
        try {
          const data = await fsPromises.readFile(iconPath);
          const ext = path.extname(iconPath).slice(1).toLowerCase();
          const resolvedMime = iconPath === preferredPath && metadataMime
            ? metadataMime
            : projectIconExtensionToMime[ext] || 'application/octet-stream';
          const contentType = resolvedMime === 'image/svg+xml' ? 'image/svg+xml; charset=utf-8' : resolvedMime;

          if (resolvedMime === 'image/svg+xml' && requestedThemeVariant) {
            const svgMarkup = data.toString('utf8');
            const themedSvgMarkup = applyProjectIconSvgTheme(svgMarkup, requestedThemeVariant, requestedIconColor);
            res.setHeader('Content-Type', contentType);
            res.setHeader('Cache-Control', 'public, max-age=31536000, immutable');
            return res.send(themedSvgMarkup);
          }

          if (resolvedMime === 'image/svg+xml' && requestedIconColor) {
            const svgMarkup = data.toString('utf8');
            const themedSvgMarkup = applyProjectIconSvgTheme(svgMarkup, requestedThemeVariant, requestedIconColor);
            res.setHeader('Content-Type', contentType);
            res.setHeader('Cache-Control', 'public, max-age=31536000, immutable');
            return res.send(themedSvgMarkup);
          }

          res.setHeader('Content-Type', contentType);
          res.setHeader('Cache-Control', 'public, max-age=31536000, immutable');
          return res.send(data);
        } catch (error) {
          if (!error || typeof error !== 'object' || error.code !== 'ENOENT') {
            console.warn('Failed to read project icon:', error);
            return res.status(500).json({ error: 'Failed to read project icon' });
          }
        }
      }

      return res.status(404).json({ error: 'Project icon not found' });
    } catch (error) {
      console.warn('Failed to load project icon:', error);
      return res.status(500).json({ error: 'Failed to load project icon' });
    }
  });

  // PUT /api/projects/:projectId/icon — 上传自定义图标：解析 data URL
  // 后写入图标文件、清掉其它格式副本，并把 iconImage 元数据更新为
  // { mime, updatedAt, source: 'custom' }；返回更新后的 project 与
  // settings。projectId 缺失或 data URL 非法 400、项目不存在 404、
  // 写入失败 500。
  app.put('/api/projects/:projectId/icon', async (req, res) => {
    const projectId = typeof req.params.projectId === 'string' ? req.params.projectId.trim() : '';
    if (!projectId) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    const parsed = parseProjectIconDataUrl(req.body?.dataUrl);
    if (!parsed.ok) {
      return res.status(400).json({ error: parsed.error });
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      const { projects, project } = findProjectById(settings, projectId);
      if (!project) {
        return res.status(404).json({ error: 'Project not found' });
      }

      const iconPath = projectIconPathForMime(projectId, parsed.mime);
      if (!iconPath) {
        return res.status(400).json({ error: 'Unsupported icon format' });
      }

      await fsPromises.mkdir(projectIconsDirPath, { recursive: true });
      await fsPromises.writeFile(iconPath, parsed.bytes);
      await removeProjectIconFiles(projectId, iconPath);

      const updatedAt = Date.now();
      const nextProjects = projects.map((entry) => (
        entry.id === projectId
          ? { ...entry, iconImage: { mime: parsed.mime, updatedAt, source: 'custom' } }
          : entry
      ));
      const updatedSettings = await persistSettings({ projects: nextProjects });
      const updatedProject = (updatedSettings.projects || []).find((entry) => entry.id === projectId) || null;

      return res.json({ project: updatedProject, settings: updatedSettings });
    } catch (error) {
      console.warn('Failed to upload project icon:', error);
      return res.status(500).json({ error: 'Failed to upload project icon' });
    }
  });

  // DELETE /api/projects/:projectId/icon — 删除全部图标文件并把
  // iconImage 置为 null；返回更新后的 project 与 settings。projectId
  // 缺失 400、项目不存在 404、删除失败 500。
  app.delete('/api/projects/:projectId/icon', async (req, res) => {
    const projectId = typeof req.params.projectId === 'string' ? req.params.projectId.trim() : '';
    if (!projectId) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      const { projects, project } = findProjectById(settings, projectId);
      if (!project) {
        return res.status(404).json({ error: 'Project not found' });
      }

      await removeProjectIconFiles(projectId);

      const nextProjects = projects.map((entry) => (
        entry.id === projectId
          ? { ...entry, iconImage: null }
          : entry
      ));
      const updatedSettings = await persistSettings({ projects: nextProjects });
      const updatedProject = (updatedSettings.projects || []).find((entry) => entry.id === projectId) || null;

      return res.json({ project: updatedProject, settings: updatedSettings });
    } catch (error) {
      console.warn('Failed to remove project icon:', error);
      return res.status(500).json({ error: 'Failed to remove project icon' });
    }
  });

  // POST /api/projects/:projectId/icon/discover — 在项目目录搜索
  // favicon.* 并落为自动图标（source 为 'auto'）：已有自定义图标且
  // 未 force 时跳过并返回 skipped；无 favicon 404、格式不支持 415、
  // 空文件或超限 400；成功返回 project / settings / discoveredPath。
  app.post('/api/projects/:projectId/icon/discover', async (req, res) => {
    const projectId = typeof req.params.projectId === 'string' ? req.params.projectId.trim() : '';
    if (!projectId) {
      return res.status(400).json({ error: 'projectId is required' });
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      const { projects, project } = findProjectById(settings, projectId);
      if (!project) {
        return res.status(404).json({ error: 'Project not found' });
      }

      const force = req.body?.force === true;
      if (project.iconImage?.source === 'custom' && !force) {
        return res.json({
          project,
          skipped: true,
          reason: 'custom-icon-present',
        });
      }

      const faviconCandidates = await fsSearchRuntime.searchFilesystemFiles(project.path, {
        limit: 200,
        query: 'favicon',
        includeHidden: true,
        respectGitignore: false,
      });

      const filtered = faviconCandidates
        .filter((entry) => /(^|\/)favicon\.(ico|png|svg|jpg|jpeg|webp)$/i.test(entry.path))
        .sort((a, b) => a.path.length - b.path.length);

      const selected = filtered[0];
      if (!selected) {
        return res.status(404).json({ error: 'No favicon found in project' });
      }

      const ext = path.extname(selected.path).slice(1).toLowerCase();
      const mime = projectIconExtensionToMime[ext] || null;
      if (!mime) {
        return res.status(415).json({ error: 'Unsupported favicon format' });
      }

      const bytes = await fsPromises.readFile(selected.path);
      if (bytes.length === 0) {
        return res.status(400).json({ error: 'Discovered icon is empty' });
      }
      if (bytes.length > projectIconMaxBytes) {
        return res.status(400).json({ error: 'Discovered icon exceeds size limit (5 MB)' });
      }

      const iconPath = projectIconPathForMime(projectId, mime);
      if (!iconPath) {
        return res.status(415).json({ error: 'Unsupported favicon format' });
      }

      await fsPromises.mkdir(projectIconsDirPath, { recursive: true });
      await fsPromises.writeFile(iconPath, bytes);
      await removeProjectIconFiles(projectId, iconPath);

      const updatedAt = Date.now();
      const nextProjects = projects.map((entry) => (
        entry.id === projectId
          ? { ...entry, iconImage: { mime, updatedAt, source: 'auto' } }
          : entry
      ));
      const updatedSettings = await persistSettings({ projects: nextProjects });
      const updatedProject = (updatedSettings.projects || []).find((entry) => entry.id === projectId) || null;

      return res.json({
        project: updatedProject,
        settings: updatedSettings,
        discoveredPath: selected.path,
      });
    } catch (error) {
      console.warn('Failed to discover project icon:', error);
      return res.status(500).json({ error: 'Failed to discover project icon' });
    }
  });
};
