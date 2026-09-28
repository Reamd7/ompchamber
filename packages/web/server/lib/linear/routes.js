/**
 * Linear 集成的 Express 路由注册模块。
 *
 * 提供 OAuth 回调页面、授权状态/发起/激活/断开等认证端点，以及 issue 查询与更新、
 * 团队与状态映射、会话状态评论、偏好设置等 API。业务逻辑统一通过动态
 * import('./index.js') 延迟加载并缓存，避免 server 启动时强制加载 Linear 相关模块。
 */
import express from 'express';
import { readTrimmedString } from './parse.js';

/** 各 POST/PUT 端点 JSON body 的大小上限，防止超大请求体。 */
const PENDING_JSON_LIMIT = '16kb';
/** 共享的 JSON body 解析中间件（受 PENDING_JSON_LIMIT 限制）。 */
const parseJsonBody = express.json({ limit: PENDING_JSON_LIMIT });

/**
 * 从 req.query 读取指定键的字符串值：值为数组时取第一个元素，
 * 再经 readTrimmedString trim 与类型校验，返回字符串或 null。
 * @param {import('express').Request} req 请求对象
 * @param {string} key 查询参数键名
 * @returns {string | null} 规范化后的值
 */
function queryValue(req, key) {
  const raw = req.query?.[key];
  const value = Array.isArray(raw) ? raw[0] : raw;
  return readTrimmedString(value);
}

/**
 * 判断错误是否为用户输入导致的业务错误（code 为 INVALID 或带 userError 标记），
 * 用于把这类错误映射为 400 而不是 500。
 */
function isLinearUserError(error) {
  return error?.code === 'INVALID' || error?.userError === true;
}

/** 转义 HTML 特殊字符（&、<、>、"、'），防止回调页面出现内容注入。 */
function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/**
 * 渲染 OAuth 回调结果页（自包含静态 HTML，跟随系统浅色/深色模式）。
 * title 与 message 均先经 escapeHtml 转义；desktopReturn 为 true 时额外渲染
 * "Return to OpenChamber" 深链按钮，并用脚本自动通过 openchamber://focus/linear-auth
 * 唤起桌面端返回前台。
 * @param {{ title: string, message: string, desktopReturn?: boolean }} params 页面文案与行为开关
 * @returns {string} 完整 HTML 字符串
 */
function renderLinearOAuthCallbackPage({ title, message, desktopReturn }) {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(title)} — OpenChamber</title>
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
${desktopReturn ? `<a class="return" href="openchamber://focus/linear-auth">Return to OpenChamber</a>
<script>window.location.href = 'openchamber://focus/linear-auth';</script>` : ''}
</main>
</body>
</html>`;
}

/**
 * 持久化一次 OAuth 授权结果：先用 access token 拉取 Linear 用户与组织身份
 * （失败仅记录日志、以 null 身份继续，不阻断授权流程），再调用 setLinearAuth 写入本地。
 * @param {object} libraries linear 聚合模块（需含 setLinearAuth 与 fetchLinearIdentity）
 * @param {object} result OAuth 令牌交换结果
 * @returns {object} 写入后的授权条目
 */
async function storeAuthorizationResult(libraries, result) {
  const { setLinearAuth, fetchLinearIdentity } = libraries;
  let user = null;
  let organization = null;
  try {
    const identity = await fetchLinearIdentity(result.accessToken);
    user = identity.user;
    organization = identity.organization;
  } catch (error) {
    console.error('Failed to load Linear identity after OAuth:', error);
  }
  return setLinearAuth({
    accessToken: result.accessToken,
    refreshToken: result.refreshToken,
    tokenType: result.tokenType,
    expiresAt: result.expiresAt,
    scope: result.scope,
    user,
    organization,
  });
}

/**
 * 在 Express 应用上注册全部 Linear 相关路由。
 *
 * linear 库函数通过 getLinearLibraries 懒加载动态 import 获取并缓存；
 * 各 API 端点统一返回 JSON，可预期的输入错误映射为 400/404，
 * 其余异常经 console.error 记录后返回 500。
 * @param {import('express').Express} app Express 应用实例
 */
export function registerLinearRoutes(app) {
  let linearLibraries = null;
  // 懒加载并缓存 linear 聚合模块（./index.js），避免 server 启动时加载全部 Linear 依赖。
  const getLinearLibraries = async () => {
    if (!linearLibraries) {
      linearLibraries = await import('./index.js');
    }
    return linearLibraries;
  };

  // Linear OAuth 授权回调：消费 code/state 完成令牌交换与持久化，渲染成功/失败 HTML 页面。
  app.get('/linear/oauth/callback', async (req, res) => {
    const finish = (status, { title, message, desktopReturn = false }) => {
      res.status(status).type('html').send(renderLinearOAuthCallbackPage({ title, message, desktopReturn }));
    };

    try {
      const libraries = await getLinearLibraries();
      const { consumeAuthorizationCallback } = libraries;
      const result = await consumeAuthorizationCallback({
        code: queryValue(req, 'code'),
        state: queryValue(req, 'state'),
        error: queryValue(req, 'error'),
        errorDescription: queryValue(req, 'error_description'),
      });

      await storeAuthorizationResult(libraries, result);

      return finish(200, {
        title: 'Authorization Complete',
        message: 'You can close this tab and return to OpenChamber.',
        desktopReturn: result.origin === 'desktop',
      });
    } catch (error) {
      const code = error instanceof Error ? error.code : '';
      const status = code === 'UNKNOWN_STATE' || code === 'MISSING_CODE' || code === 'ACCESS_DENIED'
        ? 400
        : 502;
      return finish(status, {
        title: 'Authorization Failed',
        message: error instanceof Error ? error.message : 'Linear authorization failed. Return to OpenChamber and click Connect again.',
        desktopReturn: error?.origin === 'desktop',
      });
    }
  });

  // 授权状态查询：先尝试收取 broker 中转的授权结果，再校验/刷新 token 并同步最新身份；
  // 身份接口返回 401 时清除失效凭据，最终返回不含 token 的安全状态 JSON。
  app.get('/api/linear/auth/status', async (_req, res) => {
    try {
      const libraries = await getLinearLibraries();
      const {
        getLinearAuth,
        getLinearAuthWorkspaces,
        getValidLinearAccessToken,
        fetchLinearIdentity,
        setLinearAuth,
        clearLinearAuth,
        toLinearPublicStatus,
        pollAuthorizationBroker,
        completeAuthorizationBroker,
      } = libraries;

      try {
        const result = await pollAuthorizationBroker();
        if (result) {
          await storeAuthorizationResult(libraries, result);
          await completeAuthorizationBroker(result.brokerReceipt).catch((error) => {
            console.warn('Failed to acknowledge Linear authorization broker result:', error);
          });
        }
      } catch (error) {
        console.error('Failed to complete Linear authorization through broker:', error);
      }

      const accessToken = await getValidLinearAccessToken();
      if (!accessToken) {
        return res.json({ connected: false });
      }

      const auth = getLinearAuth();
      try {
        const identity = await fetchLinearIdentity(accessToken);
        const next = setLinearAuth({
          accessToken,
          refreshToken: auth?.refreshToken,
          tokenType: auth?.tokenType,
          expiresAt: auth?.expiresAt,
          scope: auth?.scope,
          user: identity.user,
          organization: identity.organization,
          workspaceId: auth?.workspaceId,
        }, { activate: false });
        return res.json(toLinearPublicStatus(next, getLinearAuthWorkspaces()));
      } catch (error) {
        if (error?.status === 401) {
          clearLinearAuth(auth?.workspaceId);
          const remaining = getLinearAuth();
          if (!remaining) {
            return res.json({ connected: false });
          }
          return res.json(toLinearPublicStatus(remaining, getLinearAuthWorkspaces()));
        }
        if (auth) {
          return res.json(toLinearPublicStatus(auth, getLinearAuthWorkspaces()));
        }
        throw error;
      }
    } catch (error) {
      console.error('Failed to get Linear auth status:', error);
      return res.status(500).json({ error: error.message || 'Failed to get Linear auth status' });
    }
  });

  // 发起 OAuth 授权：按 body.origin（desktop/web）生成授权 URL 与 PKCE 等参数。
  app.post('/api/linear/auth/start', parseJsonBody, async (req, res) => {
    try {
      const { startAuthorization } = await getLinearLibraries();
      const origin = req.body?.origin === 'desktop' ? 'desktop' : 'web';
      const payload = await startAuthorization({ origin });
      return res.json(payload);
    } catch (error) {
      const status = error?.code === 'LINEAR_CLIENT_ID_MISSING' ? 400 : 500;
      console.error('Failed to start Linear authorization:', error);
      return res.status(status).json({ error: error.message || 'Failed to start Linear authorization' });
    }
  });

  // 按 query/cursor/status/assignee/teamId/priority 组合条件分页查询 Linear issue 列表。
  app.get('/api/linear/issues/list', async (req, res) => {
    try {
      const { listLinearIssues } = await getLinearLibraries();
      const result = await listLinearIssues({
        query: queryValue(req, 'query'),
        cursor: queryValue(req, 'cursor'),
        status: queryValue(req, 'status'),
        assignee: queryValue(req, 'assignee'),
        teamId: queryValue(req, 'teamId'),
        priority: queryValue(req, 'priority'),
      });
      return res.json(result);
    } catch (error) {
      console.error('Failed to list Linear issues:', error);
      return res.status(500).json({ error: error.message || 'Failed to list Linear issues' });
    }
  });

  // 按查询参数 id 获取单个 Linear issue 详情；缺少 id 返回 400。
  app.get('/api/linear/issues/get', async (req, res) => {
    try {
      const id = queryValue(req, 'id');
      if (!id) {
        return res.status(400).json({ error: 'id is required' });
      }
      const { getLinearIssue } = await getLinearLibraries();
      const result = await getLinearIssue(id);
      return res.json(result);
    } catch (error) {
      console.error('Failed to load Linear issue:', error);
      return res.status(500).json({ error: error.message || 'Failed to load Linear issue' });
    }
  });

  // 查询指定团队的工作流状态列表；缺少 teamId 返回 400，Linear 业务错误（INVALID）同样返回 400。
  app.get('/api/linear/issues/states', async (req, res) => {
    try {
      const teamId = queryValue(req, 'teamId');
      if (!teamId) {
        return res.status(400).json({ error: 'teamId is required' });
      }
      const { listLinearIssueStates } = await getLinearLibraries();
      const result = await listLinearIssueStates(teamId);
      return res.json(result);
    } catch (error) {
      if (isLinearUserError(error)) {
        return res.status(400).json({ error: error.message });
      }
      console.error('Failed to load Linear workflow states:', error);
      return res.status(500).json({ error: error.message || 'Failed to load Linear workflow states' });
    }
  });

  // 更新 Linear issue 状态（body 传 id 与 stateId）；用户输入错误返回 400，其余错误返回 500。
  app.post('/api/linear/issues/update', parseJsonBody, async (req, res) => {
    try {
      const { updateLinearIssue } = await getLinearLibraries();
      const result = await updateLinearIssue({
        id: req.body?.id,
        stateId: req.body?.stateId,
      });
      return res.json(result);
    } catch (error) {
      if (isLinearUserError(error)) {
        return res.status(400).json({ error: error.message });
      }
      console.error('Failed to update Linear issue:', error);
      return res.status(500).json({ error: error.message || 'Failed to update Linear issue' });
    }
  });

  // 读取团队与状态映射视图：合并本地存储的映射配置和 Linear 实时团队/状态数据。
  app.get('/api/linear/mapping', async (_req, res) => {
    try {
      const {
        listLinearTeams,
        readStoredLinearMapping,
        mergeLinearMappingView,
        LinearMappingError,
      } = await getLinearLibraries();
      const teamsResult = await listLinearTeams();
      if (teamsResult.connected === false) {
        return res.json({ connected: false });
      }
      let stored;
      try {
        stored = readStoredLinearMapping();
      } catch (error) {
        if (error instanceof LinearMappingError && error.code === 'MALFORMED') {
          return res.status(500).json({ error: error.message });
        }
        throw error;
      }
      return res.json({
        connected: true,
        ...mergeLinearMappingView(stored, teamsResult.teams),
      });
    } catch (error) {
      console.error('Failed to load Linear mapping:', error);
      return res.status(500).json({ error: error.message || 'Failed to load Linear mapping' });
    }
  });

  // 保存团队与状态映射配置：先校验写入本地存储（非法输入返回 400），再合并最新团队数据返回视图。
  app.put('/api/linear/mapping', parseJsonBody, async (req, res) => {
    try {
      const {
        getValidLinearAccessToken,
        listLinearTeams,
        setStoredLinearMapping,
        mergeLinearMappingView,
        LinearMappingError,
      } = await getLinearLibraries();
      const accessToken = await getValidLinearAccessToken();
      if (!accessToken) {
        return res.json({ connected: false });
      }
      let stored;
      try {
        stored = setStoredLinearMapping(req.body);
      } catch (error) {
        if (error instanceof LinearMappingError && error.code === 'INVALID') {
          return res.status(400).json({ error: error.message });
        }
        throw error;
      }
      const teamsResult = await listLinearTeams();
      if (teamsResult.connected === false) {
        return res.json({
          connected: true,
          ...mergeLinearMappingView(stored, []),
        });
      }
      return res.json({
        connected: true,
        ...mergeLinearMappingView(stored, teamsResult.teams),
      });
    } catch (error) {
      console.error('Failed to save Linear mapping:', error);
      return res.status(500).json({ error: error.message || 'Failed to save Linear mapping' });
    }
  });

  // 在 Linear issue 上发布会话状态评论；INVALID（输入问题）返回 400，MALFORMED 返回 500。
  app.post('/api/linear/session-status', parseJsonBody, async (req, res) => {
    try {
      const { postLinearSessionStatus, LinearSessionStatusError } = await getLinearLibraries();
      try {
        const result = await postLinearSessionStatus({
          kind: req.body?.kind,
          sessionId: req.body?.sessionId,
          issueIdentifier: req.body?.issueIdentifier,
          sessionOrigin: req.body?.sessionOrigin,
        });
        return res.json(result);
      } catch (error) {
        if (error instanceof LinearSessionStatusError && error.code === 'INVALID') {
          return res.status(400).json({ error: error.message });
        }
        if (error instanceof LinearSessionStatusError && error.code === 'MALFORMED') {
          return res.status(500).json({ error: error.message });
        }
        throw error;
      }
    } catch (error) {
      console.error('Failed to post Linear session status:', error);
      return res.status(500).json({ error: error.message || 'Failed to post Linear session status' });
    }
  });

  // 读取 Linear 偏好设置（目前仅有"会话状态评论"开关）。
  app.get('/api/linear/preferences', async (_req, res) => {
    try {
      const { getLinearSessionCommentsEnabled } = await getLinearLibraries();
      return res.json({ sessionComments: getLinearSessionCommentsEnabled() });
    } catch (error) {
      console.error('Failed to load Linear preferences:', error);
      return res.status(500).json({ error: error.message || 'Failed to load Linear preferences' });
    }
  });

  // 更新"会话状态评论"开关；body.sessionComments 必须为布尔值，否则返回 400。
  app.put('/api/linear/preferences', parseJsonBody, async (req, res) => {
    try {
      const sessionComments = req.body?.sessionComments;
      if (sessionComments !== true && sessionComments !== false) {
        return res.status(400).json({ error: 'sessionComments must be a boolean' });
      }
      const { setLinearSessionCommentsEnabled } = await getLinearLibraries();
      return res.json({ sessionComments: setLinearSessionCommentsEnabled(sessionComments) });
    } catch (error) {
      console.error('Failed to save Linear preferences:', error);
      return res.status(500).json({ error: error.message || 'Failed to save Linear preferences' });
    }
  });

  // 切换当前激活的 Linear workspace（body.organizationId）；缺少参数返回 400，找不到返回 404。
  app.post('/api/linear/auth/activate', parseJsonBody, async (req, res) => {
    try {
      const {
        activateLinearAuth,
        getLinearAuth,
        getLinearAuthWorkspaces,
        toLinearPublicStatus,
      } = await getLinearLibraries();
      const organizationId = readTrimmedString(req.body?.organizationId);
      if (!organizationId) {
        return res.status(400).json({ error: 'organizationId is required' });
      }
      const activated = activateLinearAuth(organizationId);
      if (!activated) {
        return res.status(404).json({ error: 'Linear workspace not found' });
      }
      const auth = getLinearAuth();
      if (!auth) {
        return res.json({ connected: false });
      }
      return res.json(toLinearPublicStatus(auth, getLinearAuthWorkspaces()));
    } catch (error) {
      console.error('Failed to switch Linear workspace:', error);
      return res.status(500).json({ error: error.message || 'Failed to switch Linear workspace' });
    }
  });

  // 断开 Linear 连接：先向 Linear 撤销 refresh/access token（尽力而为），再清除本地凭据。
  app.delete('/api/linear/auth', async (_req, res) => {
    try {
      const { getLinearAuth, clearLinearAuth, revokeToken } = await getLinearLibraries();
      const auth = getLinearAuth();
      if (auth?.refreshToken) {
        await revokeToken(auth.refreshToken, 'refresh_token');
      } else if (auth?.accessToken) {
        await revokeToken(auth.accessToken, 'access_token');
      }
      const removed = clearLinearAuth(auth?.workspaceId);
      return res.json({ success: true, removed });
    } catch (error) {
      console.error('Failed to disconnect Linear:', error);
      return res.status(500).json({ error: error.message || 'Failed to disconnect Linear' });
    }
  });
}
