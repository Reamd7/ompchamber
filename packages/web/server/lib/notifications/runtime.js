/**
 * 通知触发运行时：把 OpenCode 会话/消息事件转换为各渠道通知。
 *
 * 处理 session.idle / session.error / message.updated / question.asked /
 * permission.asked / permission.replied 等事件，按设置过滤与节流
 * （ready/error 冷却、question/permission 去抖、auto-accept 抑制、窗口焦点），
 * 渲染通知模板后经 emitter（桌面 + UI 广播）与 web push / APNs 扇出；
 * 另提供 goal 落定推送与原生角标（collapse tag 去重计数）管理。
 * 模板渲染与文本提取能力由 template-runtime 注入。
 */
/**
 * 创建通知触发运行时。
 * deps 注入设置读取、模板/文本能力（template-runtime）、发射与推送通道
 * （emitter-runtime、push-runtime、apns-runtime）、交互式客户端可见性查询、
 * OpenCode URL 构建与鉴权头，以及可晚绑定的焦点/auto-accept 解析器。
 */
export const createNotificationTriggerRuntime = (deps) => {
  const {
    readSettingsFromDisk,
    prepareNotificationLastMessage,
    buildTemplateVariables,
    extractLastMessageText,
    fetchLastAssistantMessageText,
    resolveNotificationTemplate,
    shouldApplyResolvedTemplateMessage,
    emitDesktopNotification,
    broadcastUiNotification,
    sendPushToAllUiSessions,
    sendApnsToAllUiSessions,
    isAnyInteractiveClientVisible,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
  } = deps;
  /** 可晚绑定的「会话是否处于 Permission Auto-Accept」解析器（见 setGetIsSessionAutoAccepting）。 */
  let getIsSessionAutoAccepting = deps.getIsSessionAutoAccepting;
  /** 注册/替换外部注入的 auto-accept 解析器；非函数则清空，回退到内部实现。 */
  const setGetIsSessionAutoAccepting = (resolver) => {
    getIsSessionAutoAccepting = typeof resolver === 'function' ? resolver : undefined;
  };

  // App-icon badge for native push: the set of DISTINCT collapse-ids (the push
  // `tag`, e.g. `ready-<sessionId>` / `permission-<requestKey>`) we've sent since
  // the app was last foregrounded. The badge is the absolute APNs `aps.badge`.
  //
  // We key by `tag`, not sessionId, because the tag IS the banner identity: iOS
  // uses it as `apns-collapse-id`, so same-tag pushes REPLACE one banner while
  // different tags are distinct banners. One session can raise several banners
  // (ready + question + permission are different tags), so counting sessionIds
  // both over- and under-counts the lock-screen stack; counting tags mirrors it.
  //
  // We deliberately do NOT derive this from the live attention snapshot
  // (needsAttention/isViewed): that machinery is for in-app indicators on
  // connected clients — a backgrounded client stays "viewing", and needsAttention
  // is set by a separate session.status event that races the push trigger. The
  // set is cleared when a UI client reports visible (`clearPendingPushBadge`),
  // the same moment the device zeroes its icon badge on becomeActive.
  /** 已发送且尚未被前台消费的去重 tag 集合；其大小即原生推送角标数。 */
  const pendingPushTags = new Set();
  /** 清空待办角标集合（UI 客户端上报可见时调用，与设备端清零图标角标同步）。 */
  const clearPendingPushBadge = () => {
    pendingPushTags.clear();
  };
  /** 记录一次推送的 tag 并返回去重后的计数，作为 APNs 的绝对 aps.badge。 */
  const trackPushAndCountBadge = (tag) => {
    if (typeof tag === 'string' && tag.length > 0) {
      pendingPushTags.add(tag);
    }
    return pendingPushTags.size;
  };

  // Generic notification for native push (per the mobile design): a fixed, scenario-based
  // title + the session name as the body. No model/project/message content crosses the relay.
  /** 原生推送的固定标题映射（按场景类型）；正文一律是会话名，不含模型/项目/消息内容。 */
  const APNS_TITLE_BY_TYPE = {
    ready: 'Agent response is ready',
    error: 'Agent hit an error',
    question: 'Agent needs your input',
    permission: 'Agent needs permission',
    goal_complete: 'Goal complete',
    goal_blocked: 'Goal blocked',
    goal_budget: 'Goal reached its token budget',
  };

  /**
   * 把完整通知 payload 转成原生推送使用的通用文案：标题按 data.type 查
   * 上表（缺省 'Agent update'），正文为会话名（缺省 'Session'），角标取自
   * tag 去重计数，并仅透传 sessionId（供点击深链；是 opaque id，不是内容）。
   */
  const toApnsGenericPayload = (payload) => {
    const data = payload?.data && typeof payload.data === 'object' ? payload.data : {};
    const sessionName = typeof data.sessionName === 'string' && data.sessionName.trim().length > 0
      ? data.sessionName.trim()
      : 'Session';
    return {
      title: APNS_TITLE_BY_TYPE[data.type] || 'Agent update',
      body: sessionName,
      badge: trackPushAndCountBadge(typeof payload?.tag === 'string' ? payload.tag : undefined),
      tag: payload?.tag,
      // sessionId is forwarded so a tapped push can deep-link; it is an opaque id, not content.
      data: typeof data.sessionId === 'string' ? { sessionId: data.sessionId } : undefined,
    };
  };

  // Fan a notification out to every delivery channel: browser web-push (full templated
  // payload) and native iOS APNs (generic model-based text). Both share the dedup tag and
  // `requireNoSse` focus gate; a failure in one channel must not block the other.
  /**
   * 把一条通知扇出到全部投递通道：浏览器 web-push（完整模板文案）与原生
   * APNs（通用文案）。两通道共用去重 tag 与 requireNoSse 焦点门控，
   * 任一通道失败只告警、不阻塞另一通道。存在可见的交互式（非移动）客户端时
   * 跳过原生推送，也不再构造通用文案——避免未投递的推送虚增角标。
   */
  const fanoutPush = (payload, options) => {
    // Presence-aware routing: if any interactive (non-mobile) client — desktop/web/vscode — is
    // currently visible, it already shows the in-app notification, so skip the native push to the
    // phone. Gated on the desktop's visibility (reliable), never the phone's own. When we skip we
    // also skip toApnsGenericPayload, so the badge isn't incremented for an undelivered push.
    const interactiveVisible = isAnyInteractiveClientVisible?.() === true;
    return Promise.all([
      Promise.resolve(sendPushToAllUiSessions?.(payload, options)).catch((error) => {
        console.warn('[Push] web-push fanout failed:', error?.message ?? error);
      }),
      interactiveVisible
        ? Promise.resolve()
        : Promise.resolve(sendApnsToAllUiSessions?.(toApnsGenericPayload(payload), options)).catch((error) => {
            console.warn('[APNs] fanout failed:', error?.message ?? error);
          }),
    ]);
  };

  /** 可晚绑定的窗口焦点查询回调（见 setGetIsWindowFocused）。 */
  let getIsWindowFocused = typeof deps.getIsWindowFocused === 'function'
    ? deps.getIsWindowFocused
    : null;

  /** 注册/替换窗口焦点查询回调；非函数则清空。 */
  const setGetIsWindowFocused = (cb) => {
    getIsWindowFocused = typeof cb === 'function' ? cb : null;
  };

  /** 同一会话两次 ready/error 通知的最小间隔（5 秒冷却）。 */
  const PUSH_READY_COOLDOWN_MS = 5000;
  /** question 通知的去抖等待时间。 */
  const PUSH_QUESTION_DEBOUNCE_MS = 500;
  /** permission 通知的去抖等待时间。 */
  const PUSH_PERMISSION_DEBOUNCE_MS = 500;
  /** 会话 ID -> 待触发 question 通知的去抖定时器。 */
  const pushQuestionDebounceTimers = new Map();
  /** 会话 ID -> { timer, requestKey } 的 permission 去抖定时器。 */
  const pushPermissionDebounceTimers = new Map();
  /** 已通知过的权限请求键（sessionId:requestId），避免同一请求重复提醒。 */
  const notifiedPermissionRequests = new Set();
  /** 会话 ID -> 上次 ready 通知时间戳，用于冷却。 */
  const lastReadyNotificationAt = new Map();
  /** 会话 ID -> 上次 error 通知时间戳，用于冷却。 */
  const lastErrorNotificationAt = new Map();

  /** 「目录+会话」复合键 -> { parentID, at } 的父会话缓存。 */
  const sessionParentIdCache = new Map();
  /** 父会话缓存的存活时间（60 秒）。 */
  const SESSION_PARENT_CACHE_TTL_MS = 60 * 1000;

  // Sessions where the client has enabled Permission Auto-Accept. Mirrored
  // from the client-side permissionStore via POST /api/notifications/auto-accept
  // so the server can suppress permission notifications BEFORE dispatch (the
  // 500ms debounce race otherwise leaks notifications for auto-accepted
  // permissions when the replied round-trip is slower than the debounce).
  /** 客户端开启了 Permission Auto-Accept 的会话集合（经 /api/notifications/auto-accept 镜像到服务端）。 */
  const autoAcceptingSessions = new Set();
  /** 更新某会话的 auto-accept 标记；sessionId 非法时忽略。 */
  const setAutoAcceptSession = (sessionId, enabled) => {
    if (typeof sessionId !== 'string' || sessionId.length === 0) return;
    if (enabled) {
      autoAcceptingSessions.add(sessionId);
    } else {
      autoAcceptingSessions.delete(sessionId);
    }
  };

  /** 构造点击通知后跳转的会话深链（/?session=<id>）；无会话 id 返回根路径。 */
  const buildSessionDeepLinkUrl = (sessionId) => {
    if (!sessionId || typeof sessionId !== 'string') {
      return '/';
    }
    return `/?session=${encodeURIComponent(sessionId)}`;
  };

  /** 生成父会话缓存键：目录与会话 ID 以 NUL 字符分隔（目录可为空）。 */
  const getSessionParentCacheKey = (sessionId, directory) => `${directory || ''}\0${sessionId}`;

  /**
   * 读取父会话缓存；过期或未命中返回 undefined（过期项顺带清理）。
   * 注意返回 null 表示「已确认无父会话」，与 undefined（未知）语义不同。
   */
  const getCachedSessionParentId = (sessionId, directory) => {
    const cacheKey = getSessionParentCacheKey(sessionId, directory);
    const entry = sessionParentIdCache.get(cacheKey);
    if (!entry) return undefined;
    if (Date.now() - entry.at > SESSION_PARENT_CACHE_TTL_MS) {
      sessionParentIdCache.delete(cacheKey);
      return undefined;
    }
    return entry.parentID;
  };

  /** 写入父会话缓存（parentID 为 null 表示已确认无父会话）。 */
  const setCachedSessionParentId = (sessionId, directory, parentID) => {
    sessionParentIdCache.set(getSessionParentCacheKey(sessionId, directory), { parentID: parentID ?? null, at: Date.now() });
  };

  /**
   * 从 session.created / session.updated 事件提取 parentID；
   * 非此类事件返回 undefined，字段缺失或为空返回 null。
   */
  const getParentIdFromPayload = (payload) => {
    if (!payload || typeof payload !== 'object') return undefined;
    if (payload.type !== 'session.created' && payload.type !== 'session.updated') return undefined;
    const parentID = payload.properties?.info?.parentID ?? null;
    return typeof parentID === 'string' && parentID.length > 0 ? parentID : null;
  };

  /** 顺带从会话事件缓存 parentID，让子任务判断免去额外拉取。 */
  const maybeCacheSessionParentFromPayload = (payload) => {
    const sessionId = extractSessionIdFromPayload(payload);
    if (typeof sessionId !== 'string' || sessionId.length === 0) return;
    const directory = extractDirectoryFromPayload(payload);
    const parentID = getParentIdFromPayload(payload);
    if (parentID === undefined) return;
    setCachedSessionParentId(sessionId, directory, parentID);
  };

  /**
   * 查询会话的父会话 ID：先查缓存，未命中则请求 OpenCode 会话接口
   * （带 directory query，2 秒超时）并写缓存。任何失败返回 undefined（视为未知）。
   */
  const fetchSessionParentId = async (sessionId, directory) => {
    if (!sessionId) return undefined;

    const cached = getCachedSessionParentId(sessionId, directory);
    if (cached !== undefined) return cached;

    try {
      const base = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}`, '');
      const url = directory ? `${base}?directory=${encodeURIComponent(directory)}` : base;
      const response = await fetch(url, {
        method: 'GET',
        headers: {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
        },
        signal: AbortSignal.timeout(2000),
      });
      if (!response.ok) {
        return undefined;
      }
      const session = await response.json().catch(() => null);
      if (!session || typeof session !== 'object') {
        return undefined;
      }

      const parentID = typeof session.parentID === 'string' && session.parentID.length > 0
        ? session.parentID
        : null;
      setCachedSessionParentId(sessionId, directory, parentID);
      return parentID;
    } catch {
      return undefined;
    }
  };

  // Mirrors client-side autoRespondsPermission: a session auto-accepts if it
  // OR any ancestor is flagged. Walks the parent chain via fetchSessionParentId.
  /**
   * 判断会话是否处于 auto-accept：会话自身或任一祖先被标记即成立
   * （与客户端 autoRespondsPermission 语义一致），沿 parentID 链上溯并防环。
   */
  const isSessionAutoAccepting = async (sessionId, directory) => {
    if (!sessionId || autoAcceptingSessions.size === 0) return false;
    let current = sessionId;
    const seen = new Set();
    while (current && !seen.has(current)) {
      if (autoAcceptingSessions.has(current)) return true;
      seen.add(current);
      const parent = await fetchSessionParentId(current, directory);
      if (!parent) return false;
      current = parent;
    }
    return false;
  };

  /** 从事件 payload 的多个候选位置（info/props）提取会话 ID；找不到返回 null。 */
  const extractSessionIdFromPayload = (payload) => {
    if (!payload || typeof payload !== 'object') return null;
    const props = payload.properties;
    const info = props?.info;
    const sessionId =
      info?.sessionID ??
      info?.sessionId ??
      props?.sessionID ??
      props?.sessionId ??
      props?.session ??
      null;
    return typeof sessionId === 'string' && sessionId.length > 0 ? sessionId : null;
  };

  /** 提取事件 payload 的工作区目录；trim 后为空返回 undefined。 */
  const extractDirectoryFromPayload = (payload) => {
    if (!payload || typeof payload !== 'object') return undefined;
    const props = payload.properties;
    const directory = props?.directory ?? props?.info?.directory;
    if (typeof directory !== 'string') return undefined;
    const trimmed = directory.trim();
    return trimmed.length > 0 ? trimmed : undefined;
  };

  /** 把 agent/mode 标识格式化为标题形式：按 -_ 与空白分词、首字母大写；缺省 'Agent'。 */
  const formatMode = (raw) => {
    const value = typeof raw === 'string' ? raw.trim() : '';
    const normalized = value.length > 0 ? value : 'agent';
    return normalized
      .split(/[-_\s]+/)
      .filter(Boolean)
      .map((token) => token.charAt(0).toUpperCase() + token.slice(1))
      .join(' ');
  };

  /**
   * 把模型 ID 格式化为可读名称：按 -_ 分词、相邻纯数字段合并为小数点
   * （如 4 与 5 合成 4.5）、每段首字母大写；空值返回 'Assistant'。
   */
  const formatModelId = (raw) => {
    const value = typeof raw === 'string' ? raw.trim() : '';
    if (!value) {
      return 'Assistant';
    }

    const tokens = value.split(/[-_]+/).filter(Boolean);
    const result = [];
    for (let i = 0; i < tokens.length; i += 1) {
      const current = tokens[i];
      const next = tokens[i + 1];
      if (/^\d+$/.test(current) && next && /^\d+$/.test(next)) {
        result.push(`${current}.${next}`);
        i += 1;
        continue;
      }
      result.push(current);
    }

    return result
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(' ');
  };

  // A session with an ACTIVE goal suppresses per-turn ready notifications;
  // the session-goal runtime sends its own notification when the goal
  // settles. Fetch failures fall through to normal notification behavior.
  /**
   * 判断会话是否有进行中的 goal（读会话 metadata.ompchamber.goal.status）。
   * 有活跃 goal 时抑制逐轮 ready 通知，由 goal 落定通知收尾；
   * 请求失败按无 goal 处理，回落到普通通知行为。
   */
  const hasActiveSessionGoal = async (sessionId, directory) => {
    if (!sessionId) return false;
    try {
      const base = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}`, '');
      const url = directory ? `${base}?directory=${encodeURIComponent(directory)}` : base;
      const response = await fetch(url, {
        method: 'GET',
        headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
        signal: AbortSignal.timeout(2000),
      });
      if (!response.ok) return false;
      const session = await response.json().catch(() => null);
      const goal = session?.metadata?.ompchamber?.goal;
      return Boolean(goal && typeof goal === 'object' && goal.status === 'active');
    } catch {
      return false;
    }
  };

  /**
   * 事件入口：按 payload.type 决定是否发通知与发什么。
   * session.idle / session.error 先被改写为等价的 message.updated 事件统一处理；
   * message.updated 且 assistant finish=stop 走完成通知（依次经 notifyOnSubtasks、
   * notifyOnCompletion、活跃 goal、窗口焦点、5 秒冷却过滤，渲染模板后先桌面 +
   * UI 广播再 push/APNs 扇出）；finish=error 走错误通知（notifyOnError、冷却、焦点）；
   * question.asked 以 500ms 去抖合并（plan/build 模式切换有专属标题）；
   * permission.replied 取消同会话挂起的 permission 去抖定时器；
   * permission.asked 以 500ms 去抖发送（auto-accept 会话整体跳过），
   * 定时器触发时会再查一次 auto-accept，同一请求只提醒一次。
   */
  const maybeSendPushForTrigger = async (payload) => {
    if (!payload || typeof payload !== 'object') {
      return;
    }

    maybeCacheSessionParentFromPayload(payload);

    const sessionId = extractSessionIdFromPayload(payload);
    const notificationDirectory = extractDirectoryFromPayload(payload);
    // session.idle / session.error：改写为带 finish 的 message.updated 事件后统一处理。
    if ((payload.type === 'session.idle' || payload.type === 'session.error') && sessionId) {
      const error = payload.properties?.error;
      const errorText = typeof error?.message === 'string'
        ? error.message
        : typeof error === 'string' ? error : '';
      await maybeSendPushForTrigger({
        ...payload,
        type: 'message.updated',
        properties: {
          ...payload.properties,
          info: {
            sessionID: sessionId,
            role: 'assistant',
            finish: payload.type === 'session.error' ? 'error' : 'stop',
            ...(errorText ? { parts: [{ type: 'text', text: errorText }] } : {}),
          },
        },
      });
      return;
    }

    // message.updated：完成（finish=stop）与出错（finish=error）两类通知都在此处理。
    if (payload.type === 'message.updated') {
      const info = payload.properties?.info;
      // 完成通知：助手回合结束。
      if (info?.role === 'assistant' && info?.finish === 'stop' && sessionId) {
        const settings = await readSettingsFromDisk();

        if (settings.notifyOnSubtasks === false) {
          const parentIDFromPayload = getParentIdFromPayload(payload);
          const parentID = parentIDFromPayload
            ? parentIDFromPayload
            : await fetchSessionParentId(sessionId, notificationDirectory);

          if (parentID !== null) {
            return;
          }
        }

        if (settings.notifyOnCompletion === false) {
          return;
        }

        // While a goal drives the session, per-turn "ready" notifications are
        // noise produced by the goal loop itself — the goal's own settle
        // notification (complete/blocked/budget) is the final word instead.
        if (await hasActiveSessionGoal(sessionId, notificationDirectory)) {
          return;
        }

        if (settings.notificationMode !== 'always' && getIsWindowFocused?.()) {
          return;
        }

        const now = Date.now();
        const lastAt = lastReadyNotificationAt.get(sessionId) ?? 0;
        if (now - lastAt < PUSH_READY_COOLDOWN_MS) {
          return;
        }
        lastReadyNotificationAt.set(sessionId, now);

        let title = `${formatMode(info?.mode)} agent is ready`;
        let body = `${formatModelId(info?.modelID)} completed the task`;
        let sessionName = '';

        try {
          const templates = settings.notificationTemplates || {};
          const isSubtask = await fetchSessionParentId(sessionId, notificationDirectory);
          const completionTemplate = isSubtask && settings.notifyOnSubtasks !== false
            ? (templates.subtask || templates.completion || { title: '{agent_name} is ready', message: '{model_name} completed the task' })
            : (templates.completion || { title: '{agent_name} is ready', message: '{model_name} completed the task' });

          const variables = await buildTemplateVariables(payload, sessionId);
          sessionName = typeof variables.session_name === 'string' ? variables.session_name : sessionName;

          const messageId = info?.id;
          let lastMessage = extractLastMessageText(payload);
          if (!lastMessage) {
            lastMessage = await fetchLastAssistantMessageText(sessionId, messageId);
          }

          variables.last_message = await prepareNotificationLastMessage({
            message: lastMessage,
            settings,
          });

          const resolvedTitle = resolveNotificationTemplate(completionTemplate.title, variables);
          const resolvedBody = resolveNotificationTemplate(completionTemplate.message, variables);
          if (resolvedTitle) title = resolvedTitle;
          if (shouldApplyResolvedTemplateMessage(completionTemplate.message, resolvedBody, variables)) body = resolvedBody;
        } catch (error) {
          console.warn('[Notification] Template resolution failed, using defaults:', error?.message || error);
        }

        if (settings.nativeNotificationsEnabled) {
          const notificationPayload = {
            title,
            body,
            tag: `ready-${sessionId}`,
            kind: 'ready',
            sessionId,
            directory: notificationDirectory,
            requireHidden: settings.notificationMode !== 'always',
          };
          const desktopNotificationDelivered = emitDesktopNotification(notificationPayload);
          broadcastUiNotification(notificationPayload, { desktopNotificationDelivered });
        }

        await fanoutPush(
          {
            title,
            body,
            tag: `ready-${sessionId}`,
            data: {
              url: buildSessionDeepLinkUrl(sessionId),
              sessionId,
              sessionName,
              type: 'ready',
            },
          },
          { requireNoSse: true },
        );
      }

      // 错误通知：助手回合以错误收尾。
      if (info?.role === 'assistant' && info?.finish === 'error' && sessionId) {
        const settings = await readSettingsFromDisk();
        if (settings.notifyOnError === false) return;

        const now = Date.now();
        const lastAt = lastErrorNotificationAt.get(sessionId) ?? 0;
        if (now - lastAt < PUSH_READY_COOLDOWN_MS) return;
        lastErrorNotificationAt.set(sessionId, now);

        if (settings.notificationMode !== 'always' && getIsWindowFocused?.()) {
          return;
        }

        let title = 'Tool error';
        let body = 'An error occurred';
        let sessionName = '';

        try {
          const variables = await buildTemplateVariables(payload, sessionId);
          sessionName = typeof variables.session_name === 'string' ? variables.session_name : sessionName;
          const errorMessageId = info?.id;
          let lastMessage = extractLastMessageText(payload);
          if (!lastMessage) {
            lastMessage = await fetchLastAssistantMessageText(sessionId, errorMessageId);
          }

          variables.last_message = await prepareNotificationLastMessage({
            message: lastMessage,
            settings,
          });

          const errorTemplate = (settings.notificationTemplates || {}).error || { title: 'Tool error', message: '{last_message}' };
          const resolvedTitle = resolveNotificationTemplate(errorTemplate.title, variables);
          const resolvedBody = resolveNotificationTemplate(errorTemplate.message, variables);
          if (resolvedTitle) title = resolvedTitle;
          if (shouldApplyResolvedTemplateMessage(errorTemplate.message, resolvedBody, variables)) body = resolvedBody;
        } catch (error) {
          console.warn('[Notification] Error template resolution failed, using defaults:', error?.message || error);
        }

        if (settings.nativeNotificationsEnabled) {
          const notificationPayload = {
            title,
            body,
            tag: `error-${sessionId}`,
            kind: 'error',
            sessionId,
            directory: notificationDirectory,
            requireHidden: settings.notificationMode !== 'always',
          };
          const desktopNotificationDelivered = emitDesktopNotification(notificationPayload);
          broadcastUiNotification(notificationPayload, { desktopNotificationDelivered });
        }

        await fanoutPush(
          {
            title,
            body,
            tag: `error-${sessionId}`,
            data: {
              url: buildSessionDeepLinkUrl(sessionId),
              sessionId,
              sessionName,
              type: 'error',
            },
          },
          { requireNoSse: true },
        );
      }

      return;
    }

    // question.asked：去抖合并后发送输入提醒。
    if (payload.type === 'question.asked' && sessionId) {
      const existingTimer = pushQuestionDebounceTimers.get(sessionId);
      if (existingTimer) {
        clearTimeout(existingTimer);
      }

      const timer = setTimeout(async () => {
        pushQuestionDebounceTimers.delete(sessionId);

        const settings = await readSettingsFromDisk();
        if (settings.notifyOnQuestion === false) {
          return;
        }

        if (settings.notificationMode !== 'always' && getIsWindowFocused?.()) {
          return;
        }

        const firstQuestion = payload.properties?.questions?.[0];
        const header = typeof firstQuestion?.header === 'string' ? firstQuestion.header.trim() : '';
        const questionText = typeof firstQuestion?.question === 'string' ? firstQuestion.question.trim() : '';

        let title = /plan\s*mode/i.test(header)
          ? 'Switch to plan mode'
          : /build\s*agent/i.test(header)
            ? 'Switch to build mode'
            : header || 'Input needed';
        let body = questionText || 'Agent is waiting for your response';
        let sessionName = '';

        try {
          const variables = await buildTemplateVariables(payload, sessionId);
          sessionName = typeof variables.session_name === 'string' ? variables.session_name : sessionName;
          variables.last_message = questionText || header || '';

          const templates = settings.notificationTemplates || {};
          const questionTemplate = templates.question || { title: 'Input needed', message: '{last_message}' };

          const resolvedTitle = resolveNotificationTemplate(questionTemplate.title, variables);
          const resolvedBody = resolveNotificationTemplate(questionTemplate.message, variables);
          if (resolvedTitle) title = resolvedTitle;
          if (shouldApplyResolvedTemplateMessage(questionTemplate.message, resolvedBody, variables)) body = resolvedBody;
        } catch (error) {
          console.warn('[Notification] Question template resolution failed, using defaults:', error?.message || error);
        }

        if (settings.nativeNotificationsEnabled) {
          const notificationPayload = {
            kind: 'question',
            title,
            body,
            tag: `question-${sessionId}`,
            sessionId,
            directory: notificationDirectory,
            requireHidden: settings.notificationMode !== 'always',
          };
          const desktopNotificationDelivered = emitDesktopNotification(notificationPayload);
          broadcastUiNotification(notificationPayload, { desktopNotificationDelivered });
        }

        void fanoutPush(
          {
            title,
            body,
            tag: `question-${sessionId}`,
            data: {
              url: buildSessionDeepLinkUrl(sessionId),
              sessionId,
              sessionName,
              type: 'question',
            },
          },
          { requireNoSse: true },
        );
      }, PUSH_QUESTION_DEBOUNCE_MS);

      pushQuestionDebounceTimers.set(sessionId, timer);
      return;
    }

    // permission.replied：取消同会话挂起的 permission 通知（requestID 缺失时按会话整体取消）。
    if (payload.type === 'permission.replied' && sessionId) {
      const requestId = payload.properties?.requestID ?? payload.properties?.requestId ?? payload.properties?.id;
      const requestKey = typeof requestId === 'string' ? `${sessionId}:${requestId}` : null;
      const pendingNotification = pushPermissionDebounceTimers.get(sessionId);
      if (!pendingNotification) {
        return;
      }

      // Some runtimes may omit requestID on permission.replied.
      // When request ID is missing, clear session debounce to avoid
      // showing stale permission notifications for auto-approved prompts.
      if (!requestKey || !pendingNotification.requestKey || pendingNotification.requestKey === requestKey) {
        clearTimeout(pendingNotification.timer);
        pushPermissionDebounceTimers.delete(sessionId);
      }
      return;
    }

    // permission.asked：去抖后发送权限提醒；auto-accept 会话（含祖先）整体跳过。
    if (payload.type === 'permission.asked' && sessionId) {
      const requestId = payload.properties?.id ?? payload.properties?.requestID ?? payload.properties?.requestId;
      const permission = payload.properties?.permission;
      const requestKey = typeof requestId === 'string' ? `${sessionId}:${requestId}` : null;
      if (requestKey && notifiedPermissionRequests.has(requestKey)) {
        return;
      }

      // Client may be in Permission Auto-Accept for this session (or any
      // ancestor). Skip the whole notification path — the client responds
      // directly and the user has opted out of approval prompts.
      if (await (getIsSessionAutoAccepting?.(sessionId, notificationDirectory)
        ?? isSessionAutoAccepting(sessionId, notificationDirectory))) {
        if (requestKey) notifiedPermissionRequests.add(requestKey);
        return;
      }

      const existingTimer = pushPermissionDebounceTimers.get(sessionId);
      if (existingTimer) {
        clearTimeout(existingTimer.timer);
      }

      const timer = setTimeout(async () => {
        pushPermissionDebounceTimers.delete(sessionId);

        if (await (getIsSessionAutoAccepting?.(sessionId, notificationDirectory)
          ?? isSessionAutoAccepting(sessionId, notificationDirectory))) {
          if (requestKey) notifiedPermissionRequests.add(requestKey);
          return;
        }

        const settings = await readSettingsFromDisk();

        if (settings.notifyOnQuestion === false) {
          return;
        }

        if (settings.notificationMode !== 'always' && getIsWindowFocused?.()) {
          return;
        }

        const sessionTitle = payload.properties?.sessionTitle;
        const permissionText = typeof permission === 'string' && permission.length > 0 ? permission : '';
        const fallbackMessage = typeof sessionTitle === 'string' && sessionTitle.trim().length > 0
          ? sessionTitle.trim()
          : permissionText || 'Agent is waiting for your approval';

        let title = 'Permission required';
        let body = fallbackMessage;
        let sessionName = '';

        try {
          const variables = await buildTemplateVariables(payload, sessionId);
          sessionName = typeof variables.session_name === 'string' ? variables.session_name : sessionName;
          variables.last_message = fallbackMessage;

          const templates = settings.notificationTemplates || {};
          const questionTemplate = templates.question || { title: 'Permission required', message: '{last_message}' };

          const resolvedTitle = resolveNotificationTemplate(questionTemplate.title, variables);
          const resolvedBody = resolveNotificationTemplate(questionTemplate.message, variables);
          if (resolvedTitle) title = resolvedTitle;
          if (shouldApplyResolvedTemplateMessage(questionTemplate.message, resolvedBody, variables)) body = resolvedBody;
        } catch (error) {
          console.warn('[Notification] Permission template resolution failed, using defaults:', error?.message || error);
        }

        if (settings.nativeNotificationsEnabled) {
          const notificationPayload = {
            kind: 'permission',
            title,
            body,
            tag: requestKey ? `permission-${requestKey}` : `permission-${sessionId}`,
            sessionId,
            directory: notificationDirectory,
            requireHidden: settings.notificationMode !== 'always',
          };
          const desktopNotificationDelivered = emitDesktopNotification(notificationPayload);
          broadcastUiNotification(notificationPayload, { desktopNotificationDelivered });
        }

        if (requestKey) {
          notifiedPermissionRequests.add(requestKey);
        }

        void fanoutPush(
          {
            title,
            body,
            tag: `permission-${sessionId}`,
            data: {
              url: buildSessionDeepLinkUrl(sessionId),
              sessionId,
              sessionName,
              type: 'permission',
            },
          },
          { requireNoSse: true },
        );
      }, PUSH_PERMISSION_DEBOUNCE_MS);

      pushPermissionDebounceTimers.set(sessionId, { timer, requestKey });
    }
  };

  // Goal settle push: same fanout as the trigger paths (web-push with the
  // full text; APNs with the generic per-type title and the session name as
  // body, so the relay never sees content).
  /**
   * 目标（goal）落定推送：与触发路径同一扇出（web-push 带全文；APNs 用按类型的
   * 通用标题 + 会话名，relay 看不到内容）。先尽力取会话名作展示点缀，失败不阻塞；
   * status 映射为 goal_complete（complete）/ goal_budget（budgetLimited）/ goal_blocked。
   */
  const sendGoalSettlePush = async ({ sessionId, directory, status, title, body }) => {
    let sessionName = '';
    try {
      const base = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}`, '');
      const url = directory ? `${base}?directory=${encodeURIComponent(directory)}` : base;
      const response = await fetch(url, {
        method: 'GET',
        headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
        signal: AbortSignal.timeout(2000),
      });
      if (response.ok) {
        const session = await response.json().catch(() => null);
        if (typeof session?.title === 'string') sessionName = session.title.trim();
      }
    } catch {
      // Session name is presentation sugar for the mobile push — never block on it.
    }
    const type = status === 'complete' ? 'goal_complete' : (status === 'budgetLimited' ? 'goal_budget' : 'goal_blocked');
    await fanoutPush(
      {
        title,
        body,
        tag: `goal-${sessionId}`,
        data: {
          url: buildSessionDeepLinkUrl(sessionId),
          sessionId,
          sessionName,
          type,
        },
      },
      { requireNoSse: true },
    );
  };

  return {
    maybeSendPushForTrigger,
    setAutoAcceptSession,
    setGetIsWindowFocused,
    setGetIsSessionAutoAccepting,
    clearPendingPushBadge,
    sendGoalSettlePush,
  };
};
