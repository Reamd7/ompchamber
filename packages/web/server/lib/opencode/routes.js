/**
 * OpenCode 引擎相关 HTTP 路由的注册模块。
 *
 * registerOpenCodeRoutes 把设置读写、引擎健康/版本探测、MCP OAuth 回调
 * 闭环、provider 配置增删、工作目录切换与全局 AGENTS.md 行为提示词等端点
 * 挂到 express 应用上。所有引擎交互都通过注入的 buildOpenCodeUrl /
 * getOpenCodeAuthHeaders 走 omp-host 代理——本服务跑在 Node 下，omp SDK
 * 是 Bun/TS 专用的，不能（也不应）在这里直接 import。
 */
import express from 'express';
import { createProjectIdFromPath } from '../projects/project-id.js';
import fs from 'fs';
import os from 'os';
import path from 'path';
import {
  buildDeferredRestartResponse,
} from './config-mutation-response.js';
// NOTE: the omp SDK cannot be imported here — this server runs under Node
// and the SDK is Bun/TS-only (pi-natives fails to load). The omp-host owns
// SDK-side resolution; `resolveAgentDir` below fetches it once and caches.
import { getClaudeCliAuthStatus } from './claude-cli-auth.js';

/**
 * 注册全部 OpenCode 相关路由。
 *
 * @param app express 应用实例
 * @param dependencies 注入依赖：设置读取/迁移/持久化与格式化（readSettings*、
 * persistSettings、formatSettingsResponse）、项目目录校验与解析、provider
 * 配置增删（upsertProviderConfig / removeProviderConfig / getProviderSources）、
 * 引擎 URL 与鉴权头构造（buildOpenCodeUrl / getOpenCodeAuthHeaders）、解析
 * 快照与配置变更重启编排等；fsPromises 可选，默认 fs.promises（测试可注入桩）。
 * @returns 无返回值，副作用是把路由挂到 app 上
 */
export const registerOpenCodeRoutes = (app, dependencies) => {
  const {
    crypto,
    getOpenCodeResolutionSnapshot,
    formatSettingsResponse,
    readSettingsFromDisk,
    readSettingsFromDiskMigrated,
    persistSettings,
    sanitizeProjects,
    validateDirectoryPath,
    resolveProjectDirectory,
    getProviderSources,
    removeProviderConfig,
    upsertProviderConfig,
    refreshOpenCodeAfterConfigChange,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    fsPromises = fs.promises,
  } = dependencies;

  // auth.js 模块的懒加载缓存：首次用到认证能力时才动态 import。
  let authLibrary = null;
  // state → 待完成 MCP OAuth 上下文的内存映射（不落盘，服务重启即失效）。
  const pendingMcpAuthContextByState = new Map();
  // 暂存 MCP OAuth 上下文的有效期：30 分钟。
  const PENDING_MCP_AUTH_TTL_MS = 30 * 60 * 1000;
  /**
   * 懒加载并缓存 ./auth.js（提供 getProviderAuth / removeProviderAuth）。
   * 首次调用动态 import，之后复用同一实例，避免在启动路径上加载认证依赖。
   */
  const getAuthLibrary = async () => {
    if (!authLibrary) {
      authLibrary = await import('./auth.js');
    }
    return authLibrary;
  };

  /** 把值规整为 trim 后的非空字符串；非字符串或纯空白返回 null。 */
  const normalizePendingString = (value) => {
    if (typeof value !== 'string') {
      return null;
    }

    const trimmed = value.trim();
    return trimmed || null;
  };

  /** 转义 & < > " ' 五个字符，用于把动态值安全嵌入回调 HTML 页面。 */
  const escapeHtml = (value) => String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');

  // Self-contained page for the OAuth return leg: the system browser has no UI
  // session, so it cannot load the SPA behind the auth gate — everything it
  // needs ships inline. `ompchamber://focus/mcp-auth` raises the desktop app;
  // the link stays visible because some browsers only follow custom-protocol
  // URLs from a user gesture.
  /**
   * 渲染自包含的 OAuth 回调结果页（中文补充）：系统浏览器没有 UI 会话，
   * 加载不了鉴权门后的 SPA，所以样式与内容全部内联。desktopReturn 为真
   * 时输出 ompchamber://focus/mcp-auth 深链——脚本自动跳转，另留一个可见
   * 链接兜底（部分浏览器只允许在用户手势里打开自定义协议）。
   */
  const renderMcpOAuthCallbackPage = ({ title, message, desktopReturn }) => `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(title)} — OMPChamber</title>
<style>
  :root { color-scheme: light dark; }
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
         font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
         background: Canvas; color: CanvasText; }
  main { max-width: 34rem; padding: 2.5rem 2rem; text-align: center; }
  h1 { font-size: 1.25rem; margin: 0 0 0.75rem; }
  p { margin: 0; line-height: 1.5; opacity: 0.85; }
  a.return { display: inline-block; margin-top: 1.5rem; padding: 0.5rem 1.25rem; border-radius: 0.5rem;
             border: 1px solid color-mix(in srgb, CanvasText 25%, transparent); color: inherit; text-decoration: none; }
</style>
</head>
<body>
<main>
<h1>${escapeHtml(title)}</h1>
<p>${escapeHtml(message)}</p>
${desktopReturn ? `<a class="return" href="ompchamber://focus/mcp-auth">Return to OMPChamber</a>
<script>window.location.href = 'ompchamber://focus/mcp-auth';</script>` : ''}
</main>
</body>
</html>`;


  /** 清理已过期的待完成 MCP OAuth 上下文（expiresAt 已过或非法的条目）。 */
  const pruneExpiredPendingMcpAuthContexts = () => {
    const now = Date.now();
    for (const [state, entry] of pendingMcpAuthContextByState.entries()) {
      if (!entry || typeof entry.expiresAt !== 'number' || entry.expiresAt <= now) {
        pendingMcpAuthContextByState.delete(state);
      }
    }
  };

  /** 读取应用设置：先走磁盘设置的迁移逻辑再格式化返回；失败回 500。 */
  app.get('/api/config/settings', async (_req, res) => {
    try {
      const settings = await readSettingsFromDiskMigrated();
      res.json(formatSettingsResponse(settings));
    } catch (error) {
      console.error('Failed to read settings:', error);
      res.status(500).json({ error: 'Failed to read settings' });
    }
  });

  /** 返回当前设置下 OpenCode 引擎的解析快照（可执行来源、版本等）；失败回 500。 */
  app.get('/api/config/opencode-resolution', async (_req, res) => {
    try {
      const settings = await readSettingsFromDiskMigrated();
      const resolution = await getOpenCodeResolutionSnapshot(settings);
      res.json(resolution);
    } catch (error) {
      console.error('Failed to resolve engine runtime:', error);
      res.status(500).json({ error: 'Failed to resolve engine runtime' });
    }
  });



  /**
   * 探测 OpenCode 引擎健康状态（代理引擎的 /global/health）。引擎返回非 2xx
   * 时透传其状态码并附 healthy: false；本服务侧异常回 503；正常时回
   * { healthy } 布尔值。
   */
  app.get('/api/opencode/health', async (_req, res) => {
    try {
      const healthResponse = await fetch(buildOpenCodeUrl('/global/health', ''), {
        method: 'GET',
        headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
      });
      const health = await healthResponse.json().catch(() => null);
      if (!healthResponse.ok) {
        return res.status(healthResponse.status).json({
          healthy: false,
          error: health?.error || healthResponse.statusText || 'OpenCode health check failed',
        });
      }
      return res.json({ healthy: health?.healthy === true });
    } catch (error) {
      return res.status(503).json({
        healthy: false,
        error: error instanceof Error ? error.message : 'OpenCode health check failed',
      });
    }
  });

  /**
   * 读取引擎版本：同样代理 /global/health，取 version 字段并去掉前导 v。
   * 失败时 version 为 null 并附错误信息。
   */
  app.get('/api/opencode/version', async (_req, res) => {
    try {
      const healthResponse = await fetch(buildOpenCodeUrl('/global/health', ''), {
        method: 'GET',
        headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
      });
      const health = await healthResponse.json().catch(() => null);
      if (!healthResponse.ok) {
        return res.status(healthResponse.status).json({
          version: null,
          error: health?.error || healthResponse.statusText || 'Failed to read engine version',
        });
      }
      const version = typeof health?.version === 'string' ? health.version.replace(/^v/, '') : null;
      return res.json({ version });
    } catch (error) {
      return res.status(500).json({
        version: null,
        error: error instanceof Error ? error.message : 'Failed to read engine version',
      });
    }
  });

  /** 保存应用设置：请求体整体交给 persistSettings 持久化，返回更新后的设置；失败回 500。 */
  app.put('/api/config/settings', async (req, res) => {
    try {
      const updated = await persistSettings(req.body ?? {});
      res.json(updated);
    } catch (error) {
      console.error('[API:PUT /api/config/settings] Failed to save settings:', error);
      console.error('[API:PUT /api/config/settings] Error stack:', error.stack);
      res.status(500).json({ error: 'Failed to save settings' });
    }
  });

  // The body parser is per-route on this server; without it req.body is
  // undefined here, the state read as absent, and the "parked" context was
  // silently never stored — the callback then always failed as unknown.
  /**
   * 暂存一条待完成的 MCP OAuth 上下文（中文补充）：以 state 为键记录 MCP
   * 服务名、目录与发起方 origin，30 分钟后过期。state 缺失视为无需暂存
   * （成功返回 context: null）；name 缺失回 400。origin 记在这里而不是
   * redirect URI 里，因为 URI 一旦写进服务配置就不再改写。
   */
  app.post('/api/mcp/auth/pending', express.json({ limit: '16kb' }), async (req, res) => {
    try {
      pruneExpiredPendingMcpAuthContexts();

      const state = normalizePendingString(req.body?.state);
      if (!state) {
        return res.json({ success: true, context: null });
      }

      const name = normalizePendingString(req.body?.name);
      if (!name) {
        return res.status(400).json({ error: 'MCP server name is required' });
      }

      const entry = {
        name,
        directory: normalizePendingString(req.body?.directory),
        // Which surface started the flow. It belongs here rather than in the
        // redirect URI: that URI is written into the server's config once and
        // deliberately never rewritten, so anything encoded in it would be
        // frozen at whatever runtime authorised first.
        origin: normalizePendingString(req.body?.origin),
        expiresAt: Date.now() + PENDING_MCP_AUTH_TTL_MS,
      };
      pendingMcpAuthContextByState.set(state, entry);

      return res.json({
        success: true,
        context: {
          name: entry.name,
          directory: entry.directory,
          origin: entry.origin,
        },
      });
    } catch (error) {
      console.error('Failed to store pending MCP auth context:', error);
      return res.status(500).json({ error: error.message || 'Failed to store pending MCP auth context' });
    }
  });

  /** 按 state 查询待完成的 MCP OAuth 上下文；无 state 回 null，未知或已过期回 404。 */
  app.get('/api/mcp/auth/pending', async (req, res) => {
    try {
      pruneExpiredPendingMcpAuthContexts();

      const state = normalizePendingString(Array.isArray(req.query?.state) ? req.query.state[0] : req.query?.state);
      if (!state) {
        return res.json(null);
      }

      const pendingMcpAuthContext = pendingMcpAuthContextByState.get(state) ?? null;
      if (!pendingMcpAuthContext) {
        return res.status(404).json({ error: 'No pending MCP auth context' });
      }

      return res.json(pendingMcpAuthContext);
    } catch (error) {
      console.error('Failed to read pending MCP auth context:', error);
      return res.status(500).json({ error: error.message || 'Failed to read pending MCP auth context' });
    }
  });

  /** 按 state 清除待完成的 MCP OAuth 上下文（授权结束或放弃后调用）；幂等成功。 */
  app.delete('/api/mcp/auth/pending', async (req, res) => {
    try {
      const state = normalizePendingString(Array.isArray(req.query?.state) ? req.query.state[0] : req.query?.state);
      if (!state) {
        return res.json({ success: true });
      }

      pendingMcpAuthContextByState.delete(state);
      return res.json({ success: true });
    } catch (error) {
      console.error('Failed to clear pending MCP auth context:', error);
      return res.status(500).json({ error: error.message || 'Failed to clear pending MCP auth context' });
    }
  });

  // Browser return leg of the MCP OAuth flow, completed entirely server-side.
  //
  // The provider redirects the SYSTEM browser here, and that browser has no
  // OMPChamber UI session — the SPA route this path used to land on sits
  // behind the client-side auth gate, so the user saw a login page instead of
  // a finished authorization. No session can be required on this path.
  //
  // Safe without auth because it acts only on a code+state pair whose `state`
  // matches a context parked by an authenticated start call: `state` is the
  // OAuth CSRF secret, generated per flow and known only to the initiating
  // client and the provider. Without a match the code is NOT forwarded, so an
  // unauthenticated caller cannot bind this server's MCP entry to a foreign
  // account by fabricating a callback. The endpoint reads nothing and mutates
  // nothing else.
  /**
   * MCP OAuth 浏览器回调（中文补充）：code 换 token 全程在服务端完成。
   * state 必须匹配先前由已认证的 start 调用暂存的上下文，否则 code 不会被
   * 转发给引擎——伪造的回调无法把本服务的 MCP 条目绑到外部账号。结果用
   * 自包含 HTML 页面呈现；桌面端发起的流程附带 ompchamber://focus/mcp-auth
   * 深链拉起应用。此路径不得要求 UI 会话（系统浏览器没有）。
   */
  app.get('/mcp/oauth/callback', async (req, res) => {
    // 从查询串取单个值：数组取首个，规整为非空字符串否则 null。
    const queryValue = (key) => normalizePendingString(Array.isArray(req.query?.[key]) ? req.query[key][0] : req.query?.[key]);
    const state = queryValue('state');
    const code = queryValue('code');
    const providerError = queryValue('error');
    const providerErrorDescription = queryValue('error_description');

    pruneExpiredPendingMcpAuthContexts();
    const context = state ? pendingMcpAuthContextByState.get(state) ?? null : null;
    const startedFromDesktop = context?.origin === 'desktop';

    // 统一出口：清掉暂存上下文并以 HTML 渲染结果页（桌面端附带回跳深链）。
    const finish = (status, { title, message }) => {
      if (state) pendingMcpAuthContextByState.delete(state);
      res.status(status).type('html').send(renderMcpOAuthCallbackPage({
        title,
        message,
        // Browsers only follow custom-protocol links from a user gesture in
        // some configurations, so the page both tries the jump and keeps a
        // visible link as the fallback.
        desktopReturn: startedFromDesktop,
      }));
    };

    if (providerError) {
      return finish(400, {
        title: 'Authorization Failed',
        message: providerErrorDescription || providerError,
      });
    }
    if (!code) {
      return finish(400, {
        title: 'Authorization Failed',
        message: 'The provider did not return an authorization code. Start authorization again from MCP Settings.',
      });
    }
    if (!context?.name) {
      return finish(400, {
        title: 'Authorization Failed',
        message: 'This authorization session has expired or is unknown to the running app. Return to OMPChamber and click Authorize again.',
      });
    }

    try {
      const callbackUrl = new URL(buildOpenCodeUrl(`/mcp/${encodeURIComponent(context.name)}/auth/callback`, ''));
      if (context.directory) callbackUrl.searchParams.set('directory', context.directory);
      const upstream = await fetch(callbackUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json', ...getOpenCodeAuthHeaders() },
        body: JSON.stringify({ code }),
      });
      if (!upstream.ok) {
        const payload = await upstream.json().catch(() => null);
        return finish(502, {
          title: 'Authorization Failed',
          message: payload?.error || payload?.message || `OpenCode rejected the authorization code (${upstream.status}). Start authorization again from MCP Settings.`,
        });
      }
      return finish(200, {
        title: 'Authorization Complete',
        message: 'You can close this tab and return to OMPChamber.',
      });
    } catch (error) {
      return finish(502, {
        title: 'Authorization Failed',
        message: error?.message || 'Failed to complete MCP authorization.',
      });
    }
  });

  /**
   * 查询某 provider 的配置来源与认证状态。目录优先取 x-opencode-directory
   * 头，其次 directory 查询参数；显式要求了目录但解析失败时回 400。
   * claude-code 的认证状态走 CLI 凭据检测，其余 provider 走 auth 库存储的凭据。
   */
  app.get('/api/provider/:providerId/source', async (req, res) => {
    try {
      const { providerId } = req.params;
      if (!providerId) {
        return res.status(400).json({ error: 'Provider ID is required' });
      }

      const headerDirectory = typeof req.get === 'function' ? req.get('x-opencode-directory') : null;
      const queryDirectory = Array.isArray(req.query?.directory)
        ? req.query.directory[0]
        : req.query?.directory;
      const requestedDirectory = headerDirectory || queryDirectory || null;

      let directory = null;
      const resolved = await resolveProjectDirectory(req);
      if (resolved.directory) {
        directory = resolved.directory;
      } else if (requestedDirectory) {
        return res.status(400).json({ error: resolved.error });
      }

      const sources = getProviderSources(providerId, directory);
      const { getProviderAuth } = await getAuthLibrary();
      const auth = getProviderAuth(providerId);
      sources.sources.auth.exists = providerId === 'claude-code'
        ? getClaudeCliAuthStatus().connected
        : Boolean(auth);

      return res.json({
        providerId,
        sources: sources.sources,
      });
    } catch (error) {
      console.error('Failed to get provider sources:', error);
      return res.status(500).json({ error: error.message || 'Failed to get provider sources' });
    }
  });

  /**
   * 新增或更新 provider 配置（providerID + config + scope：user/project/custom，
   * 缺省 user）。project scope 或显式指定目录时必须有可解析的工作目录，否则
   * 回 400。写入成功返回延迟重启响应（需重启引擎生效），并附最终 providerId、
   * 写入路径与配置；auth 库里已有凭据时会作为上下文传给 upsert。
   */
  app.put('/api/provider', async (req, res) => {
    try {
      const providerID = typeof req.body?.providerID === 'string'
        ? req.body.providerID.trim()
        : (typeof req.body?.providerId === 'string' ? req.body.providerId.trim() : '');
      const config = req.body?.config;
      const scope = typeof req.body?.scope === 'string' ? req.body.scope : 'user';

      if (!providerID) {
        return res.status(400).json({ error: 'Provider ID is required' });
      }
      if (!config || typeof config !== 'object' || Array.isArray(config)) {
        return res.status(400).json({ error: 'Provider config is required' });
      }
      if (scope !== 'user' && scope !== 'project' && scope !== 'custom') {
        return res.status(400).json({ error: 'Invalid scope' });
      }

      const headerDirectory = typeof req.get === 'function' ? req.get('x-opencode-directory') : null;
      const queryDirectory = Array.isArray(req.query?.directory)
        ? req.query.directory[0]
        : req.query?.directory;
      const requestedDirectory = headerDirectory || queryDirectory || null;

      let directory = null;
      if (scope === 'project' || requestedDirectory) {
        const resolved = await resolveProjectDirectory(req);
        if (!resolved.directory) {
          return res.status(400).json({ error: resolved.error || 'Working directory is required' });
        }
        directory = resolved.directory;
      } else {
        const resolved = await resolveProjectDirectory(req);
        if (resolved.directory) {
          directory = resolved.directory;
        }
      }

      const { getProviderAuth } = await getAuthLibrary();
      const hasStoredAuth = Boolean(getProviderAuth(providerID));
      const upsertResult = upsertProviderConfig(providerID, config, directory, scope, { hasStoredAuth });

      return res.json({
        ...buildDeferredRestartResponse(
          `Provider ${providerID} saved. Restart the engine to apply.`,
        ),
        providerId: upsertResult.providerId,
        path: upsertResult.path,
        config: upsertResult.config,
      });
    } catch (error) {
      const status = typeof error?.statusCode === 'number' ? error.statusCode : 500;
      console.error('Failed to upsert provider config:', error);
      return res.status(status).json({ error: error.message || 'Failed to save provider config' });
    }
  });

  /**
   * 断开 provider。scope 决定删除范围：auth 只删存储凭据；user/project/custom
   * 删对应层的配置；all 全部删除。返回 removed 标明是否真删掉了东西；删过
   * 则附延迟重启提示，没删过也回 success（幂等）。
   */
  app.delete('/api/provider/:providerId/auth', async (req, res) => {
    try {
      const { providerId } = req.params;
      if (!providerId) {
        return res.status(400).json({ error: 'Provider ID is required' });
      }

      const scope = typeof req.query?.scope === 'string' ? req.query.scope : 'auth';
      const headerDirectory = typeof req.get === 'function' ? req.get('x-opencode-directory') : null;
      const queryDirectory = Array.isArray(req.query?.directory)
        ? req.query.directory[0]
        : req.query?.directory;
      const requestedDirectory = headerDirectory || queryDirectory || null;
      let directory = null;

      if (scope === 'project' || requestedDirectory) {
        const resolved = await resolveProjectDirectory(req);
        if (!resolved.directory) {
          return res.status(400).json({ error: resolved.error });
        }
        directory = resolved.directory;
      } else {
        const resolved = await resolveProjectDirectory(req);
        if (resolved.directory) {
          directory = resolved.directory;
        }
      }

      let removed = false;
      if (scope === 'auth') {
        const { removeProviderAuth } = await getAuthLibrary();
        removed = removeProviderAuth(providerId);
      } else if (scope === 'user' || scope === 'project' || scope === 'custom') {
        removed = removeProviderConfig(providerId, directory, scope);
      } else if (scope === 'all') {
        const { removeProviderAuth } = await getAuthLibrary();
        const authRemoved = removeProviderAuth(providerId);
        const userRemoved = removeProviderConfig(providerId, directory, 'user');
        const projectRemoved = directory ? removeProviderConfig(providerId, directory, 'project') : false;
        const customRemoved = removeProviderConfig(providerId, directory, 'custom');
        removed = authRemoved || userRemoved || projectRemoved || customRemoved;
      } else {
        return res.status(400).json({ error: 'Invalid scope' });
      }

      if (removed) {
        return res.json({
          success: true,
          removed,
          ...buildDeferredRestartResponse('Provider disconnected successfully. Restart the engine to apply.'),
        });
      }

      return res.json({
        success: true,
        removed,
        requiresReload: false,
        message: 'Provider was not connected',
      });
    } catch (error) {
      console.error('Failed to disconnect provider:', error);
      return res.status(500).json({ error: error.message || 'Failed to disconnect provider' });
    }
  });

  /**
   * 切换 OpenCode 工作目录：校验路径合法性（create 为真时先递归创建目录），
   * 未登记的项目追加进设置并设为活跃项目，同时更新 lastDirectory；已登记的
   * 项目复用原 id。失败回 400/500。
   */
  app.post('/api/opencode/directory', async (req, res) => {
    try {
      const requestedPath = typeof req.body?.path === 'string' ? req.body.path.trim() : '';
      if (!requestedPath) {
        return res.status(400).json({ error: 'Path is required' });
      }

      if (req.body?.create === true) {
        await fsPromises.mkdir(path.resolve(requestedPath), { recursive: true });
      }

      const validated = await validateDirectoryPath(requestedPath);
      if (!validated.ok) {
        return res.status(400).json({ error: validated.error });
      }

      const resolvedPath = validated.directory;
      const currentSettings = await readSettingsFromDisk();
      const existingProjects = sanitizeProjects(currentSettings.projects) || [];
      const existing = existingProjects.find((project) => project.path === resolvedPath) || null;

      const nextProjects = existing
        ? existingProjects
        : [
            ...existingProjects,
            {
              id: createProjectIdFromPath(resolvedPath),
              path: resolvedPath,
              addedAt: Date.now(),
              lastOpenedAt: Date.now(),
            },
          ];

      const activeProjectId = existing ? existing.id : nextProjects[nextProjects.length - 1].id;

      const updated = await persistSettings({
        projects: nextProjects,
        activeProjectId,
        lastDirectory: resolvedPath,
      });

      return res.json({
        success: true,
        restarted: false,
        path: resolvedPath,
        settings: updated,
      });
    } catch (error) {
      console.error('Failed to update OpenCode working directory:', error);
      return res.status(500).json({ error: error.message || 'Failed to update working directory' });
    }
  });

  // Behavior / Global AGENTS.md endpoints.
  //
  // The edit target is the omp-native user-level file (spec 07 §5.13): the
  // highest-priority (100) context provider loads `getAgentDir()/AGENTS.md`,
  // resolved server-side because the agent dir is profile-scoped
  // (~/.omp/agent, or ~/.omp/profiles/<name>/agent while a profile is
  // active). The legacy OpenCode-compat file (~/.config/opencode/AGENTS.md,
  // priority 55) is still read by the engine's discovery provider, so it is
  // reported read-only for the UI notice; when the native file exists it
  /**
   * 旧版 OpenCode 兼容的 AGENTS.md 路径（~/.config/opencode/AGENTS.md）。
   * 引擎的发现 provider 仍会读它，因此只读上报给 UI 做提示，编辑目标永远是
   * omp 原生的 agent 目录内文件。
   */
  const legacyAgentsMdPath = () => path.join(os.homedir(), '.config', 'opencode', 'AGENTS.md');
  // The agent dir is profile-scoped and can change while the server runs
  // (profile activation), so it is resolved per request — the loopback fetch
  // is cheap, and caching here would pin writes to a stale profile. The
  // static default is only used while the omp-host is unreachable and is
  // never cached, so a later request can pick up the real directory.
  /**
   * 解析当前生效的 agent 目录（中文补充）：优先问 omp-host 的 /agent-dir；
   * agent 目录随 profile 激活而变，所以逐请求解析、绝不缓存。omp-host 尚未
   * 就绪时回退静态默认 ~/.omp/agent（同样不缓存，后续请求能拿到真实目录）。
   */
  const resolveAgentDir = async () => {
    try {
      const base = buildOpenCodeUrl('/agent-dir');
      const res = await fetch(base, { headers: getOpenCodeAuthHeaders() ?? {} });
      if (res.ok) {
        const body = await res.json();
        if (typeof body?.agentDir === 'string' && body.agentDir) {
          return body.agentDir;
        }
      }
    } catch {
      // omp-host not reachable yet: fall through to the static default.
    }
    return path.join(os.homedir(), '.omp', 'agent');
  };
  /** 拼出当前 agent 目录下 AGENTS.md 的完整路径（每次调用都重新解析 agent 目录）。 */
  const agentsMdPath = async () => path.join(await resolveAgentDir(), 'AGENTS.md');
  // 行为提示词的大小上限（1 MB），超出回 413。
  const MAX_BEHAVIOR_PROMPT_SIZE = 1024 * 1024; // 1 MB

  /**
   * 读取全局 AGENTS.md：返回内容、是否存在与实际路径，并附带旧版兼容文件的
   * 路径及是否仍有内容（供 UI 提示）。原生文件缺失是合法状态（空编辑器），
   * 旧版文件缺失同样不算错误。
   */
  app.get('/api/behavior/agents-md', async (_req, res) => {
    try {
      const targetPath = await agentsMdPath();
      let content = '';
      let exists = false;
      try {
        content = await fs.promises.readFile(targetPath, 'utf8');
        exists = true;
      } catch {
        // Missing native file is a valid state (empty editor), not an error.
      }

      const legacyPath = legacyAgentsMdPath();
      let legacyHasContent = false;
      try {
        const legacyContent = await fs.promises.readFile(legacyPath, 'utf8');
        legacyHasContent = legacyContent.trim().length > 0;
      } catch {
        // No legacy file: nothing to notice.
      }

      return res.json({
        content,
        exists,
        path: targetPath,
        legacy: { path: legacyPath, hasContent: legacyHasContent },
      });
    } catch (error) {
      console.error('Failed to read AGENTS.md:', error);
      return res.status(500).json({ error: 'Failed to read AGENTS.md' });
    }
  });

  /**
   * 保存全局 AGENTS.md：内容超过 1 MB 回 413；父目录缺失时递归创建；成功后
   * 返回延迟重启响应（需重启引擎才生效）。
   */
  app.put('/api/behavior/agents-md', async (req, res) => {
    try {
      const content = typeof req.body?.content === 'string' ? req.body.content : '';

      if (content.length > MAX_BEHAVIOR_PROMPT_SIZE) {
        return res.status(413).json({ error: `Content exceeds maximum size of ${MAX_BEHAVIOR_PROMPT_SIZE} bytes` });
      }

      const targetPath = await agentsMdPath();
      // Ensure parent directory exists
      const parentDir = path.dirname(targetPath);
      try {
        await fs.promises.access(parentDir);
      } catch {
        await fs.promises.mkdir(parentDir, { recursive: true });
      }

      await fs.promises.writeFile(targetPath, content, 'utf8');

      return res.json(buildDeferredRestartResponse(
        'AGENTS.md saved. Restart the engine to apply.',
      ));
    } catch (error) {
      console.error('Failed to write AGENTS.md:', error);
      return res.status(500).json({ error: error.message || 'Failed to write AGENTS.md' });
    }
  });
};
