/**
 * 通知相关 HTTP 路由：Web Push/APNs 订阅注册与注销、UI 可见性心跳、
 * SSE 通知流，以及会话状态/注意力快照与已读标记。
 *
 * 各项能力经 registerNotificationRoutes 注入（push/APNs 运行时方法、
 * 会话快照与标记函数、UI 鉴权控制器等）；本模块只做参数校验与编排。
 */
/**
 * 校验并规整 push 订阅请求体：需要非空 endpoint 与 keys.p256dh/auth。
 * 返回 trim 后的 { endpoint, keys }，结构非法返回 null。
 */
const parsePushSubscribeBody = (body) => {
  if (!body || typeof body !== 'object') return null;
  const endpoint = body.endpoint;
  const keys = body.keys;
  const p256dh = keys?.p256dh;
  const auth = keys?.auth;

  if (typeof endpoint !== 'string' || endpoint.trim().length === 0) return null;
  if (typeof p256dh !== 'string' || p256dh.trim().length === 0) return null;
  if (typeof auth !== 'string' || auth.trim().length === 0) return null;

  return {
    endpoint: endpoint.trim(),
    keys: { p256dh: p256dh.trim(), auth: auth.trim() },
  };
};

/** 校验注销请求体：需要非空 endpoint；返回 trim 后的 { endpoint }，非法返回 null。 */
const parsePushUnsubscribeBody = (body) => {
  if (!body || typeof body !== 'object') return null;
  const endpoint = body.endpoint;
  if (typeof endpoint !== 'string' || endpoint.trim().length === 0) return null;
  return { endpoint: endpoint.trim() };
};

/** SSE 通知流的心跳间隔（20 秒），用于保活与探测断连。 */
export const NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS = 20_000;

/**
 * 在 express 应用上注册通知相关路由。
 * dependencies 注入：UI 鉴权（uiAuthController / getUiSessionTokenFromRequest）、
 * push 与 APNs 运行时操作、可见性与角标、SSE 客户端集合与事件写入、
 * 会话状态/注意力快照与标记函数等；可选依赖缺失时安全跳过。
 */
export const registerNotificationRoutes = (app, dependencies) => {
  const {
    uiAuthController,
    ensurePushInitialized,
    ensureGlobalWatcherStarted,
    getOrCreateVapidKeys,
    getUiSessionTokenFromRequest,
    readSettingsFromDiskMigrated,
    writeSettingsToDisk,
    addOrUpdatePushSubscription,
    removePushSubscription,
    addOrUpdateApnsToken,
    removeApnsToken,
    updateUiVisibility,
    clearPendingPushBadge,
    isUiVisible,
    getUiNotificationClients,
    writeSseEvent,
    getSessionActivitySnapshot,
    getSessionStateSnapshot,
    getSessionAttentionSnapshot,
    getSessionState,
    getSessionAttentionState,
    markSessionViewed,
    markSessionUnviewed,
    markUserMessageSent,
    setPushInitialized,
    setAutoAcceptSession,
  } = dependencies;

  /** 懒启动全局会话 watcher；未注入或启动失败仅告警，不阻塞请求。 */
  const ensureSessionWatcher = async () => {
    if (typeof ensureGlobalWatcherStarted !== 'function') {
      return;
    }
    try {
      await ensureGlobalWatcherStarted();
    } catch (error) {
      console.warn('[OpenCodeWatcher] lazy start failed:', error?.message ?? error);
    }
  };

  // GET /api/push/vapid-public-key：返回 Web Push 公钥。
  // 先懒初始化 push（生成/加载 VAPID 密钥），失败返回 500。
  app.get('/api/push/vapid-public-key', async (_req, res) => {
    try {
      await ensurePushInitialized();
      const keys = await getOrCreateVapidKeys();
      res.json({ publicKey: keys.publicKey });
    } catch (error) {
      console.warn('[Push] Failed to load VAPID key:', error);
      res.status(500).json({ error: 'Failed to load push key' });
    }
  });

  // POST /api/push/subscribe：注册 web push 订阅。
  // 确保 UI 会话（缺失 401）并校验 body（非法 400）；若请求携带 http(s) origin
  // 且设置中尚无 publicOrigin，则记录该 origin 并重置 push 初始化，
  // 使 VAPID subject 使用真实公开地址；最后按 endpoint 持久化订阅（含 UA 与平台）。
  app.post('/api/push/subscribe', async (req, res) => {
    await ensurePushInitialized();
    await ensureSessionWatcher();

    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    const parsed = parsePushSubscribeBody(req.body);
    if (!parsed) {
      return res.status(400).json({ error: 'Invalid body' });
    }

    const { endpoint, keys } = parsed;

    const origin = typeof req.body?.origin === 'string' ? req.body.origin.trim() : '';
    if (origin.startsWith('http://') || origin.startsWith('https://')) {
      try {
        const settings = await readSettingsFromDiskMigrated();
        if (typeof settings?.publicOrigin !== 'string' || settings.publicOrigin.trim().length === 0) {
          await writeSettingsToDisk({
            ...settings,
            publicOrigin: origin,
          });
          setPushInitialized(false);
        }
      } catch {
      }
    }

    const platform = typeof req.body?.platform === 'string' ? req.body.platform : undefined;
    await addOrUpdatePushSubscription(
      uiToken,
      {
        endpoint,
        p256dh: keys.p256dh,
        auth: keys.auth,
      },
      req.headers['user-agent'],
      platform
    );

    return res.json({ ok: true });
  });

  // DELETE /api/push/subscribe：注销订阅。确保 UI 会话、校验 endpoint 后
  // 删除该会话下的对应订阅。
  app.delete('/api/push/subscribe', async (req, res) => {
    await ensurePushInitialized();

    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    const parsed = parsePushUnsubscribeBody(req.body);
    if (!parsed) {
      return res.status(400).json({ error: 'Invalid body' });
    }

    await removePushSubscription(uiToken, parsed.endpoint);
    return res.json({ ok: true });
  });

  // Native iOS APNs device token registration (mirrors /api/push/subscribe). The token
  // is a hex APNs device token from @capacitor/push-notifications, scoped to the UI
  // session like web-push subscriptions.
  // 注册原生 iOS APNs 设备 token（与 /api/push/subscribe 镜像），与会话绑定；
  // platform 缺省 ios，environment 缺省 production。
  app.post('/api/push/apns-token', async (req, res) => {
    await ensureSessionWatcher();

    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    const deviceToken = typeof req.body?.token === 'string' ? req.body.token.trim() : '';
    if (!deviceToken) {
      return res.status(400).json({ error: 'Invalid body' });
    }

    const platform = req.body?.platform === 'android' ? 'android' : 'ios';
    // APNs environment the token belongs to: Xcode/dev-signed installs report 'sandbox',
    // TestFlight/App Store report 'production'. Absent (older clients, Android) → production.
    const environment = req.body?.environment === 'sandbox' ? 'sandbox' : 'production';
    if (typeof addOrUpdateApnsToken === 'function') {
      await addOrUpdateApnsToken(uiToken, deviceToken, req.headers['user-agent'], platform, environment);
    }
    return res.json({ ok: true });
  });

  // DELETE /api/push/apns-token：注销该 UI 会话下的 APNs 设备 token。
  app.delete('/api/push/apns-token', async (req, res) => {
    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    const deviceToken = typeof req.body?.token === 'string' ? req.body.token.trim() : '';
    if (!deviceToken) {
      return res.status(400).json({ error: 'Invalid body' });
    }

    if (typeof removeApnsToken === 'function') {
      await removeApnsToken(uiToken, deviceToken);
    }
    return res.json({ ok: true });
  });

  // POST /api/push/visibility：上报 UI 可见性心跳（visible + platform），
  // 供 push/APNs 的抑制门控使用。
  app.post('/api/push/visibility', async (req, res) => {
    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    const body = req.body && typeof req.body === 'object' ? req.body : {};
    const platform = typeof body.platform === 'string' ? body.platform : undefined;
    updateUiVisibility(uiToken, body.visible === true, platform);
    return res.json({ ok: true });
  });

  // GET /api/push/visibility：查询当前会话心跳记录的可见状态（无需 401 以外的副作用）。
  app.get('/api/push/visibility', (req, res) => {
    const uiToken = getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return res.status(401).json({ error: 'UI session missing' });
    }

    return res.json({
      ok: true,
      visible: isUiVisible(uiToken),
    });
  });

  // GET /api/notifications/stream：SSE 通知流。写入 SSE 响应头并把连接注册进
  // 客户端集合；按 NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS 发送 ':heartbeat' 注释帧
  // 保活并探测断连，close/error 时清理定时器与注册；建立成功先下发
  // notification-stream-ready 事件（携带 uiToken）。
  app.get('/api/notifications/stream', async (req, res) => {
    await ensureSessionWatcher();

    const uiToken = uiAuthController?.ensureSessionToken
      ? await uiAuthController.ensureSessionToken(req, res)
      : getUiSessionTokenFromRequest(req);
    if (!uiToken) {
      return;
    }

    res.setHeader('Content-Type', 'text/event-stream; charset=utf-8');
    res.setHeader('Cache-Control', 'no-cache, no-transform');
    res.setHeader('Connection', 'keep-alive');
    res.setHeader('X-Accel-Buffering', 'no');
    res.flushHeaders?.();

    const clients = getUiNotificationClients();
    clients.add(res);

    let closed = false;
    let heartbeatTimer = null;

    const cleanup = () => {
      if (closed) {
        return;
      }
      closed = true;
      if (heartbeatTimer) {
        clearInterval(heartbeatTimer);
        heartbeatTimer = null;
      }
      clients.delete(res);
    };

    req.on('close', cleanup);
    res.on('error', cleanup);

    const flushSse = () => {
      res.flush?.();
    };

    heartbeatTimer = setInterval(() => {
      if (closed || res.writableEnded || res.destroyed) {
        cleanup();
        return;
      }
      try {
        res.write(':heartbeat\n\n');
        flushSse();
      } catch {
        cleanup();
      }
    }, NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS);

    try {
      writeSseEvent(res, {
        type: 'ompchamber:notification-stream-ready',
        properties: { uiToken },
      });
      flushSse();
    } catch {
      cleanup();
    }
  });

  // GET /api/session-activity：返回会话活动快照；异步触发 watcher 启动，不等待完成。
  app.get('/api/session-activity', (_req, res) => {
    void ensureSessionWatcher();
    res.json(getSessionActivitySnapshot());
  });

  // GET /api/sessions/snapshot：返回会话状态与注意力双快照，附带服务器时间。
  app.get('/api/sessions/snapshot', async (_req, res) => {
    await ensureSessionWatcher();
    res.json({
      statusSessions: getSessionStateSnapshot(),
      attentionSessions: getSessionAttentionSnapshot(),
      serverTime: Date.now(),
    });
  });

  // GET /api/sessions/status：返回全部会话状态快照与服务器时间。
  app.get('/api/sessions/status', async (_req, res) => {
    await ensureSessionWatcher();
    const snapshot = getSessionStateSnapshot();
    res.json({
      sessions: snapshot,
      serverTime: Date.now(),
    });
  });

  // GET /api/sessions/:id/status：返回单个会话状态；无可用状态时 404。
  app.get('/api/sessions/:id/status', async (req, res) => {
    await ensureSessionWatcher();
    const sessionId = req.params.id;
    const state = getSessionState(sessionId);

    if (!state) {
      return res.status(404).json({
        error: 'Session not found or no state available',
        sessionId,
      });
    }

    return res.json({
      sessionId,
      ...state,
    });
  });

  // GET /api/sessions/attention：返回全部会话注意力快照与服务器时间。
  app.get('/api/sessions/attention', async (_req, res) => {
    await ensureSessionWatcher();
    const snapshot = getSessionAttentionSnapshot();
    res.json({
      sessions: snapshot,
      serverTime: Date.now(),
    });
  });

  // GET /api/sessions/:id/attention：返回单个会话注意力状态；无则 404。
  app.get('/api/sessions/:id/attention', async (req, res) => {
    await ensureSessionWatcher();
    const sessionId = req.params.id;
    const state = getSessionAttentionState(sessionId);

    if (!state) {
      return res.status(404).json({
        error: 'Session not found or no attention state available',
        sessionId,
      });
    }

    return res.json({
      sessionId,
      ...state,
    });
  });

  // POST /api/sessions/:id/view：标记会话已查看（clientId 取 x-client-id 头或 IP），
  // 同时清空原生 push 角标（用户正在使用应用，角标不再适用）。
  app.post('/api/sessions/:id/view', (req, res) => {
    const sessionId = req.params.id;
    const clientId = req.headers['x-client-id'] || req.ip || 'anonymous';

    markSessionViewed(sessionId, clientId);
    // The user is engaging with the app, so the native push badge no longer
    // applies — reset it here too (not only on the visibility beacon), since
    // opening the app reliably marks the opened session viewed.
    if (typeof clearPendingPushBadge === 'function') clearPendingPushBadge();

    return res.json({
      success: true,
      sessionId,
      viewed: true,
    });
  });

  // POST /api/sessions/:id/unview：标记会话回到未查看状态。
  app.post('/api/sessions/:id/unview', (req, res) => {
    const sessionId = req.params.id;
    const clientId = req.headers['x-client-id'] || req.ip || 'anonymous';

    markSessionUnviewed(sessionId, clientId);

    return res.json({
      success: true,
      sessionId,
      viewed: false,
    });
  });

  // POST /api/sessions/:id/message-sent：标记用户已发送消息，
  // 并清空原生 push 角标（角标只统计此后的通知）。
  app.post('/api/sessions/:id/message-sent', (req, res) => {
    const sessionId = req.params.id;

    markUserMessageSent(sessionId);
    // Sending a message means the user is active in the app; reset the native
    // push badge so it counts only notifications since this engagement.
    if (typeof clearPendingPushBadge === 'function') clearPendingPushBadge();

    return res.json({
      success: true,
      sessionId,
      messageSent: true,
    });
  });

  // Mirror client-side Permission Auto-Accept state to the server so it can
  // suppress permission notifications at the source (the 500ms debounce race
  // otherwise leaks notifications for auto-accepted permissions).
  // 同步客户端的 Permission Auto-Accept 开关到服务端，使其能在源头
  // 抑制权限通知（否则 500ms 去抖竞态会泄漏自动接受产生的通知）。
  app.post('/api/notifications/auto-accept', (req, res) => {
    const body = req.body && typeof req.body === 'object' ? req.body : {};
    const sessionId = typeof body.sessionId === 'string' ? body.sessionId.trim() : '';
    const enabled = body.enabled === true;
    if (!sessionId) {
      return res.status(400).json({ error: 'sessionId required' });
    }
    if (typeof setAutoAcceptSession === 'function') {
      setAutoAcceptSession(sessionId, enabled);
    }
    return res.json({ success: true, sessionId, enabled });
  });
};
