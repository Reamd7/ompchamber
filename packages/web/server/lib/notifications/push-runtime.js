/**
 * Web Push（VAPID）订阅与发送运行时。
 *
 * 订阅按 UI 会话 token 分组持久化到磁盘文件（版本化 JSON），发送时跨会话按
 * endpoint 去重；VAPID 密钥存放在设置中，缺失时自动生成。
 * 另维护一份带 TTL 的 UI 可见性心跳表，用于在已有客户端可见时抑制原生 push，
 * 避免与页面内通知重复打扰。移动端 PWA 订阅与 APNs 共用同一套可见性门控。
 */
/** 订阅文件格式版本；version 不匹配的旧文件按空数据丢弃。 */
const PUSH_SUBSCRIPTIONS_VERSION = 1;
/** UI 可见性心跳的有效期（30 秒），超时未更新即视为不可见。 */
const UI_VISIBILITY_TTL_MS = 30_000;

/** 判断 origin 是否为 loopback（localhost / 127.0.0.1 / [::1]）的明文 http 地址。 */
const isLoopbackHttpOrigin = (value) => {
  if (typeof value !== 'string') {
    return false;
  }

  return value.startsWith('http://localhost')
    || value.startsWith('http://127.0.0.1')
    || value.startsWith('http://[::1]');
};

/**
 * 创建 push 运行时。
 * deps 注入 fsPromises/path、webPush 库、订阅文件路径 PUSH_SUBSCRIPTIONS_FILE_PATH，
 * 以及设置读写 readSettingsFromDiskMigrated / writeSettingsToDisk
 * （存放 VAPID 密钥与 publicOrigin）。
 */
export const createPushRuntime = (deps) => {
  const {
    fsPromises,
    path,
    webPush,
    PUSH_SUBSCRIPTIONS_FILE_PATH,
    readSettingsFromDiskMigrated,
    writeSettingsToDisk,
  } = deps;

  /** 串行化订阅文件读改写的 Promise 链锁，避免并发更新互相覆盖。 */
  let persistPushSubscriptionsLock = Promise.resolve();
  /** webPush 是否已完成 VAPID 初始化（懒加载，一次即成）。 */
  let pushInitialized = false;

  /** UI 会话 token -> { visible, updatedAt, platform } 的内存可见性心跳表。 */
  const uiVisibilityByToken = new Map();
  /** 清理过期或损坏的可见性心跳记录（超过 UI_VISIBILITY_TTL_MS 未更新即删除）。 */
  const pruneUiVisibility = (now = Date.now()) => {
    for (const [token, state] of uiVisibilityByToken) {
      if (!state || now - state.updatedAt > UI_VISIBILITY_TTL_MS) {
        uiVisibilityByToken.delete(token);
      }
    }
  };

  /**
   * 读取并校验磁盘上的订阅文件。
   * 文件不存在（ENOENT）、解析失败、version 不符或结构非法时一律返回
   * 空的版本化结构，绝不抛错（仅告警）。
   */
  const readPushSubscriptionsFromDisk = async () => {
    try {
      const raw = await fsPromises.readFile(PUSH_SUBSCRIPTIONS_FILE_PATH, 'utf8');
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object') {
        return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: {} };
      }
      if (typeof parsed.version !== 'number' || parsed.version !== PUSH_SUBSCRIPTIONS_VERSION) {
        return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: {} };
      }

      const subscriptionsBySession =
        parsed.subscriptionsBySession && typeof parsed.subscriptionsBySession === 'object'
          ? parsed.subscriptionsBySession
          : {};

      return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession };
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: {} };
      }
      console.warn('Failed to read push subscriptions file:', error);
      return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: {} };
    }
  };

  /** 把订阅数据以两空格缩进的 JSON 写回磁盘，先确保父目录存在。 */
  const writePushSubscriptionsToDisk = async (data) => {
    await fsPromises.mkdir(path.dirname(PUSH_SUBSCRIPTIONS_FILE_PATH), { recursive: true });
    await fsPromises.writeFile(PUSH_SUBSCRIPTIONS_FILE_PATH, JSON.stringify(data, null, 2), 'utf8');
  };

  /**
   * 串行执行一次订阅文件的读改写：经 persistPushSubscriptionsLock 排队，
   * 读取当前数据、应用 mutate 得到新结构后写盘；返回本次更新后的数据。
   */
  const persistPushSubscriptionUpdate = async (mutate) => {
    persistPushSubscriptionsLock = persistPushSubscriptionsLock.then(async () => {
      await fsPromises.mkdir(path.dirname(PUSH_SUBSCRIPTIONS_FILE_PATH), { recursive: true });
      const current = await readPushSubscriptionsFromDisk();
      const next = mutate({
        version: PUSH_SUBSCRIPTIONS_VERSION,
        subscriptionsBySession: current.subscriptionsBySession || {},
      });
      await writePushSubscriptionsToDisk(next);
      return next;
    });

    return persistPushSubscriptionsLock;
  };

  /**
   * 获取 VAPID 密钥对：设置中已有合法密钥直接复用；
   * 否则用 webPush 生成新密钥对并写回设置。返回 { publicKey, privateKey }。
   */
  const getOrCreateVapidKeys = async () => {
    const settings = await readSettingsFromDiskMigrated();
    const existing = settings?.vapidKeys;
    if (existing && typeof existing.publicKey === 'string' && typeof existing.privateKey === 'string') {
      return { publicKey: existing.publicKey, privateKey: existing.privateKey };
    }

    const generated = webPush.generateVAPIDKeys();
    const next = {
      ...settings,
      vapidKeys: {
        publicKey: generated.publicKey,
        privateKey: generated.privateKey,
      },
    };

    await writeSettingsToDisk(next);
    return { publicKey: generated.publicKey, privateKey: generated.privateKey };
  };

  /**
   * 规整某会话的订阅记录数组：剔除缺 endpoint/p256dh/auth 的非法项，
   * 保留 createdAt 与可选的 platform。非数组输入返回空数组。
   */
  const normalizePushSubscriptions = (record) => {
    if (!Array.isArray(record)) return [];
    return record
      .map((entry) => {
        if (!entry || typeof entry !== 'object') return null;
        const endpoint = entry.endpoint;
        const p256dh = entry.p256dh;
        const auth = entry.auth;
        if (typeof endpoint !== 'string' || typeof p256dh !== 'string' || typeof auth !== 'string') {
          return null;
        }
        return {
          endpoint,
          p256dh,
          auth,
          createdAt: typeof entry.createdAt === 'number' ? entry.createdAt : null,
          platform: typeof entry.platform === 'string' ? entry.platform : undefined,
        };
      })
      .filter(Boolean);
  };

  /**
   * 为指定 UI 会话新增或更新一条订阅（按 endpoint 去重，新条目置顶），
   * 记录 createdAt/lastSeenAt/userAgent；platform 未提供时沿用旧值
   * （供移动端 PWA 走与 APNs 相同的可见性抑制门控）。
   * 每会话最多保留 10 条，超出截断。
   */
  const addOrUpdatePushSubscription = async (uiSessionToken, subscription, userAgent, platform) => {
    if (!uiSessionToken) {
      return;
    }

    await ensurePushInitialized();

    const now = Date.now();

    await persistPushSubscriptionUpdate((current) => {
      const subsBySession = { ...(current.subscriptionsBySession || {}) };
      const existing = Array.isArray(subsBySession[uiSessionToken]) ? subsBySession[uiSessionToken] : [];

      const filtered = existing.filter((entry) => entry && typeof entry.endpoint === 'string' && entry.endpoint !== subscription.endpoint);

      const previous = existing.find((entry) => entry && entry.endpoint === subscription.endpoint);
      filtered.unshift({
        endpoint: subscription.endpoint,
        p256dh: subscription.p256dh,
        auth: subscription.auth,
        createdAt: now,
        lastSeenAt: now,
        userAgent: typeof userAgent === 'string' && userAgent.length > 0 ? userAgent : undefined,
        // Platform lets the sender route mobile PWA push through the same presence gate as APNs.
        platform:
          typeof platform === 'string' && platform
            ? platform
            : typeof previous?.platform === 'string'
              ? previous.platform
              : undefined,
      });

      subsBySession[uiSessionToken] = filtered.slice(0, 10);

      return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: subsBySession };
    });
  };

  /** 删除指定会话中某 endpoint 的订阅；会话订阅清空时连同该会话键一并移除。 */
  const removePushSubscription = async (uiSessionToken, endpoint) => {
    if (!uiSessionToken || !endpoint) return;

    await ensurePushInitialized();

    await persistPushSubscriptionUpdate((current) => {
      const subsBySession = { ...(current.subscriptionsBySession || {}) };
      const existing = Array.isArray(subsBySession[uiSessionToken]) ? subsBySession[uiSessionToken] : [];
      const filtered = existing.filter((entry) => entry && typeof entry.endpoint === 'string' && entry.endpoint !== endpoint);
      if (filtered.length === 0) {
        delete subsBySession[uiSessionToken];
      } else {
        subsBySession[uiSessionToken] = filtered;
      }
      return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: subsBySession };
    });
  };

  /** 从所有会话中删除某 endpoint 的订阅（推送服务报告订阅失效时调用）。 */
  const removePushSubscriptionFromAllSessions = async (endpoint) => {
    if (!endpoint) return;

    await ensurePushInitialized();

    await persistPushSubscriptionUpdate((current) => {
      const subsBySession = { ...(current.subscriptionsBySession || {}) };
      for (const [token, entries] of Object.entries(subsBySession)) {
        if (!Array.isArray(entries)) continue;
        const filtered = entries.filter((entry) => entry && typeof entry.endpoint === 'string' && entry.endpoint !== endpoint);
        if (filtered.length === 0) {
          delete subsBySession[token];
        } else {
          subsBySession[token] = filtered;
        }
      }
      return { version: PUSH_SUBSCRIPTIONS_VERSION, subscriptionsBySession: subsBySession };
    });
  };

  /**
   * 向单条订阅发送 push。收到 410/404（订阅已失效）时自动从所有会话
   * 移除该订阅；其它失败仅告警，不向外抛出。
   */
  const sendPushToSubscription = async (sub, payload) => {
    await ensurePushInitialized();
    const body = JSON.stringify(payload);

    const pushSubscription = {
      endpoint: sub.endpoint,
      keys: {
        p256dh: sub.p256dh,
        auth: sub.auth,
      },
    };

    try {
      await webPush.sendNotification(pushSubscription, body);
    } catch (error) {
      const statusCode = typeof error?.statusCode === 'number' ? error.statusCode : null;
      if (statusCode === 410 || statusCode === 404) {
        await removePushSubscriptionFromAllSessions(sub.endpoint);
        return;
      }
      console.warn('[Push] Failed to send notification:', error);
    }
  };

  /**
   * 向所有会话的全部订阅广播 push（跨会话按 endpoint 去重，并发发送）。
   * options.requireNoSse 为 true 时按可见性门控抑制：移动端 PWA 订阅仅在
   * 无可见交互式（桌面/web）客户端时发送；非移动订阅在任意客户端可见时
   * 即抑制——避免与页面内通知重复打扰。
   */
  const sendPushToAllUiSessions = async (payload, options = {}) => {
    const requireNoSse = options.requireNoSse === true;
    const store = await readPushSubscriptionsFromDisk();
    const sessions = store.subscriptionsBySession || {};
    const subscriptionsByEndpoint = new Map();

    for (const record of Object.values(sessions)) {
      const subscriptions = normalizePushSubscriptions(record);
      if (subscriptions.length === 0) continue;

      for (const sub of subscriptions) {
        if (!subscriptionsByEndpoint.has(sub.endpoint)) {
          subscriptionsByEndpoint.set(sub.endpoint, sub);
        }
      }
    }

    await Promise.all(Array.from(subscriptionsByEndpoint.values()).map(async (sub) => {
      if (requireNoSse) {
        // Mobile PWA subscriptions follow the same presence model as native push: suppress only
        // when an interactive (desktop/web) client is visible. The phone PWA's own foreground is
        // handled in the service worker (focused-client check), so it won't double-notify.
        // Non-mobile (desktop/web) subscriptions keep the existing any-visible gate.
        const suppressed = isMobilePlatform(sub.platform) ? isAnyInteractiveClientVisible() : isAnyUiVisible();
        if (suppressed) return;
      }
      await sendPushToSubscription(sub, payload);
    }));
  };

  // A client is "mobile" if it reports a native mobile platform. Anything else (web, desktop,
  // vscode, or an older client that doesn't report a platform) is treated as interactive — i.e.
  // a surface where the user would actually see the in-app notification.
  /** 视为移动端的原生平台标识集合。 */
  const MOBILE_PLATFORMS = new Set(['ios', 'android']);
  /** 判断平台是否为移动端（ios/android）；未上报平台的旧客户端视为非移动端。 */
  const isMobilePlatform = (platform) => typeof platform === 'string' && MOBILE_PLATFORMS.has(platform);

  /**
   * 更新某 UI 会话的可见性心跳（带平台标识）。
   * 本次心跳未携带 platform 时沿用上次记录（如纯心跳包）。
   */
  const updateUiVisibility = (token, visible, platform) => {
    if (!token) return;
    const now = Date.now();
    const nextVisible = Boolean(visible);
    const existing = uiVisibilityByToken.get(token);
    // Keep the last known platform if this beacon didn't carry one (e.g. a heartbeat).
    const nextPlatform = typeof platform === 'string' && platform ? platform : existing?.platform;
    uiVisibilityByToken.set(token, { visible: nextVisible, updatedAt: now, platform: nextPlatform });
  };

  /** 是否存在任一可见（且心跳未过期）的 UI 客户端；查询前先清理过期心跳。 */
  const isAnyUiVisible = () => {
    const now = Date.now();
    pruneUiVisibility(now);
    for (const state of uiVisibilityByToken.values()) {
      if (state.visible === true && now - state.updatedAt <= UI_VISIBILITY_TTL_MS) {
        return true;
      }
    }
    return false;
  };

  // True when at least one NON-mobile client (desktop/web/vscode) is currently visible. Used to
  // suppress native push to the phone: an active desktop already shows the notification, so the
  // phone doesn't need it. Deliberately based on the desktop's visibility (reliable), never the
  // phone's own (a backgrounded WKWebView can't report "hidden" before iOS suspends it).
  /** 是否存在至少一个可见的非移动（桌面/web/vscode）客户端；用于抑制发往手机的原生 push。 */
  const isAnyInteractiveClientVisible = () => {
    const now = Date.now();
    pruneUiVisibility(now);
    for (const state of uiVisibilityByToken.values()) {
      if (
        state.visible === true &&
        now - state.updatedAt <= UI_VISIBILITY_TTL_MS &&
        !isMobilePlatform(state.platform)
      ) {
        return true;
      }
    }
    return false;
  };

  /** 判断指定会话当前是否可见（心跳未过期且 visible 为 true）。 */
  const isUiVisible = (token) => {
    const now = Date.now();
    pruneUiVisibility(now);
    const state = uiVisibilityByToken.get(token);
    return state?.visible === true && now - state.updatedAt <= UI_VISIBILITY_TTL_MS;
  };

  /**
   * 解析 VAPID subject：依次取 OMPCHAMBER_VAPID_SUBJECT 环境变量、
   * OMPCHAMBER_PUBLIC_ORIGIN、设置中的 publicOrigin；
   * loopback http 地址替换为 'mailto:ompchamber@localhost'
   * （push 服务不接受内网地址），全部缺失时也兜底到该 mailto。
   */
  const resolveVapidSubject = async () => {
    const configured = process.env.OMPCHAMBER_VAPID_SUBJECT;
    if (typeof configured === 'string' && configured.trim().length > 0) {
      return configured.trim();
    }

    const originEnv = process.env.OMPCHAMBER_PUBLIC_ORIGIN;
    if (typeof originEnv === 'string' && originEnv.trim().length > 0) {
      const trimmed = originEnv.trim();
      if (isLoopbackHttpOrigin(trimmed)) {
        return 'mailto:ompchamber@localhost';
      }
      return trimmed;
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      const stored = settings?.publicOrigin;
      if (typeof stored === 'string' && stored.trim().length > 0) {
        const trimmed = stored.trim();
        if (isLoopbackHttpOrigin(trimmed)) {
          return 'mailto:ompchamber@localhost';
        }
        return trimmed;
      }
    } catch {
    }

    return 'mailto:ompchamber@localhost';
  };

  /**
   * 懒初始化 webPush：获取/生成 VAPID 密钥并解析 subject 后调用
   * setVapidDetails；subject 为兜底 mailto 时告警提示未配置公开 origin。
   * 幂等，成功后置 pushInitialized。
   */
  const ensurePushInitialized = async () => {
    if (pushInitialized) return;
    const keys = await getOrCreateVapidKeys();
    const subject = await resolveVapidSubject();

    if (subject === 'mailto:ompchamber@localhost') {
      console.warn('[Push] No public origin configured for VAPID; set OMPCHAMBER_VAPID_SUBJECT or enable push once from a real origin.');
    }

    webPush.setVapidDetails(subject, keys.publicKey, keys.privateKey);
    pushInitialized = true;
  };

  /** 手动设置初始化标志（供测试重置）。 */
  const setPushInitialized = (value) => {
    pushInitialized = value === true;
  };

  return {
    getOrCreateVapidKeys,
    addOrUpdatePushSubscription,
    removePushSubscription,
    sendPushToAllUiSessions,
    updateUiVisibility,
    isAnyUiVisible,
    isAnyInteractiveClientVisible,
    isUiVisible,
    ensurePushInitialized,
    setPushInitialized,
  };
};
