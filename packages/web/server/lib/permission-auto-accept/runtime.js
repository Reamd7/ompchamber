/**
 * 权限自动放行（permission auto-accept）运行时。
 *
 * 允许用户按会话开启“自动同意工具权限请求”。策略（sessions 映射 + 单调
 * 递增的 revision）持久化在设置键 permissionAutoAccept 下；子代理会话沿
 * parentID 链向上继承最近的显式祖先策略。运行时订阅全局事件总线：收到
 * permission.asked 事件（或重连后对账）时，若所属会话开启自动放行，则以
 * reply 'once' 调用 OpenCode HTTP 接口放行，自带重试与并发去重。
 */
/** 策略在 settings.json 中的存储键。 */
const SETTINGS_KEY = 'permissionAutoAccept';
/** 一次放行请求失败后的重试延迟序列（首个 0 表示立即首次尝试）。 */
const RETRY_DELAYS_MS = [0, 250, 1000];
/** 调用 OpenCode HTTP 接口的单请求超时时间（毫秒）。 */
const REQUEST_TIMEOUT_MS = 5000;
/** 会话信息缓存的最大条数（超出时淘汰最旧条目）。 */
const SESSION_CACHE_LIMIT = 10000;

/**
 * 把任意输入归一为合法策略：sessions 只保留 sessionId -> boolean 的条目，
 * revision 必须是非负安全整数（默认 0）；其余形状一律回退为空策略。
 * @returns {{ sessions: Object<string, boolean>, revision: number }}
 */
const normalizePolicy = (value) => {
  const source = value && typeof value === 'object' && !Array.isArray(value) ? value : {};
  const sessions = {};
  const entries = source.sessions && typeof source.sessions === 'object' && !Array.isArray(source.sessions)
    ? Object.entries(source.sessions)
    : [];
  for (const [sessionId, enabled] of entries) {
    if (sessionId && typeof enabled === 'boolean') sessions[sessionId] = enabled;
  }
  const revision = Number.isSafeInteger(source.revision) && source.revision >= 0 ? source.revision : 0;
  return { sessions, revision };
};

/** 等待指定毫秒数的 Promise 包装。 */
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * 创建权限自动放行运行时。
 * @param {object} deps - globalEventHub（事件与连接状态订阅）、
 *   buildOpenCodeUrl、getOpenCodeAuthHeaders、settings 的读取与持久化、
 *   broadcastGlobalUiEvent（UI 事件广播）、fetchImpl 及可覆盖的重试/超时参数。
 * @returns 暴露 snapshot / load / setSessionPolicy / isSessionAutoAccepting /
 *   processPermission / reconcilePending / start；策略写入经 writePromise
 *   串行化，相同 permission id 的放行与相同目录集合的对账均会去重合并。
 */
export function createPermissionAutoAcceptRuntime({
  globalEventHub,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  readSettingsFromDiskMigrated,
  persistSettings,
  broadcastGlobalUiEvent,
  fetchImpl = fetch,
  retryDelaysMs = RETRY_DELAYS_MS,
  requestTimeoutMs = REQUEST_TIMEOUT_MS,
}) {
  // 闭包内的运行时状态：
  let policy = normalizePolicy();
  // 当前策略（内存权威副本，首次 load 时被磁盘值覆盖）
  // 磁盘策略是否已加载完成
  let loaded = false;
  // 进行中的加载 promise（并发 load 共享；失败后清空以便重试）
  let loadPromise = null;
  // 策略写入串行链（读-改-写按顺序排队）
  let writePromise = Promise.resolve();
  // 会话元信息缓存：sessionId -> { parentID, directory }
  const sessions = new Map();
  // 进行中的放行任务：permission id -> promise（并发去重）
  const inFlight = new Map();
  // 进行中的对账任务：目录集合键 -> promise（并发去重）
  const reconcilePromises = new Map();

  /** 当前策略的浅拷贝（sessions 复制 + revision），供持久化与广播使用。 */
  const snapshot = () => ({
    sessions: { ...policy.sessions },
    revision: policy.revision,
  });

  /**
   * 惰性从磁盘加载策略：并发调用共享同一个 promise，成功后标记 loaded，
   * 此后直接返回内存快照；失败时清空 loadPromise，下次调用会重试。
   * @returns {Promise<object>} 策略快照。
   */
  const load = async () => {
    if (loaded) return snapshot();
    if (!loadPromise) {
      loadPromise = readSettingsFromDiskMigrated()
        .then((settings) => {
          policy = normalizePolicy(settings?.[SETTINGS_KEY]);
          loaded = true;
          return snapshot();
        })
        .finally(() => { loadPromise = null; });
    }
    return loadPromise;
  };

  /**
   * 串行执行一次策略更新：把 update(policy) 的结果写入设置文件、更新内存
   * 策略并广播 ompchamber:permission-auto-accept.updated 事件，返回新快照。
   * 通过链到 writePromise 保证读-改-写不会交错。
   * @param {Function} update - 以当前策略为入参、返回新策略的纯函数。
   */
  const persistUpdate = (update) => {
    writePromise = writePromise.then(async () => {
      const next = update(policy);
      await persistSettings({ [SETTINGS_KEY]: next });
      policy = next;
      loaded = true;
      broadcastGlobalUiEvent?.({
        type: 'ompchamber:permission-auto-accept.updated',
        properties: snapshot(),
      });
      return snapshot();
    });
    return writePromise;
  };

  /**
   * 设置某会话的自动放行开关：校验入参（非法抛 TypeError），加载并持久化
   * 策略（revision 自增），开启时立刻对该会话目录对账存量待放行权限。
   * @param {string} sessionId - 会话 ID（trim 后非空）。
   * @param {boolean} enabled - 是否自动放行。
   * @param {string} [directory] - 会话所属目录（对账范围）。
   * @returns {Promise<object>} 更新后的策略快照。
   */
  const setSessionPolicy = async (sessionId, enabled, directory) => {
    if (typeof sessionId !== 'string' || !sessionId.trim()) throw new TypeError('sessionId is required');
    if (typeof enabled !== 'boolean') throw new TypeError('enabled must be a boolean');
    await load();
    const result = await persistUpdate((current) => ({
      ...current,
      sessions: { ...current.sessions, [sessionId.trim()]: enabled },
      revision: current.revision + 1,
    }));
    if (enabled) await reconcilePending({ directories: [directory] });
    return result;
  };

  /**
   * 缓存会话元信息（parentID 与 directory），让子代理继承判定免于反复请求
   * /session/:id；缓存超过上限时淘汰最旧条目，无效输入直接忽略。
   * @param {object} info - 会话信息（须有非空 id）。
   * @param {string} [directoryHint] - 目录兜底值。
   */
  const rememberSession = (info, directoryHint) => {
    if (!info || typeof info.id !== 'string' || !info.id) return;
    sessions.set(info.id, {
      parentID: typeof info.parentID === 'string' && info.parentID ? info.parentID : null,
      directory: typeof info.directory === 'string' && info.directory ? info.directory : directoryHint,
    });
    if (sessions.size > SESSION_CACHE_LIMIT) {
      sessions.delete(sessions.keys().next().value);
    }
  };

  /**
   * 调用 OpenCode HTTP 接口：自动拼接认证头与可选 directory 查询参数，
   * 并施加请求级超时。非 2xx 抛出带 status 的 Error；响应体非法 JSON 时
   * 返回 null。
   * @param {string} path - 相对路径（如 /permission）。
   * @param {object} [options] - directory、method（默认 GET）、body。
   */
  const request = async (path, { directory, method = 'GET', body } = {}) => {
    const url = new URL(buildOpenCodeUrl(path, ''));
    if (directory) url.searchParams.set('directory', directory);
    const response = await fetchImpl(url, {
      method,
      headers: {
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        ...getOpenCodeAuthHeaders(),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
      signal: AbortSignal.timeout(requestTimeoutMs),
    });
    if (!response.ok) {
      const error = new Error(`OpenCode request failed (${response.status})`);
      error.status = response.status;
      throw error;
    }
    return response.json().catch(() => null);
  };

  /**
   * 获取会话信息：优先命中缓存，否则请求 /session/:id 并写回缓存；
   * 返回 { parentID, directory } 形状的记录，未知会话为 null。
   * @param {string} sessionId - 会话 ID。
   * @param {string} [directory] - 目录参数（透传给请求）。
   */
  const getSession = async (sessionId, directory) => {
    const cached = sessions.get(sessionId);
    if (cached) return cached;
    const info = await request(`/session/${encodeURIComponent(sessionId)}`, { directory });
    rememberSession(info?.data ?? info, directory);
    return sessions.get(sessionId) ?? null;
  };

  /**
   * 判断会话是否开启自动放行：沿 parentID 链向上查找，命中策略映射即返回
   * 其布尔值（“最近的显式祖先策略”）；seen 集合防止父链成环；链上信息
   * 获取失败按未开启处理；走到链尽头仍无显式策略则返回 false。
   * @returns {Promise<boolean>}
   */
  const isSessionAutoAccepting = async (sessionId, directory) => {
    await load();
    const seen = new Set();
    let current = sessionId;
    let currentDirectory = directory;
    while (current && !seen.has(current)) {
      if (Object.hasOwn(policy.sessions, current)) return policy.sessions[current] === true;
      seen.add(current);
      let info;
      try {
        info = await getSession(current, currentDirectory);
      } catch {
        return false;
      }
      current = info?.parentID ?? null;
      currentDirectory = info?.directory ?? currentDirectory;
    }
    return false;
  };

  /**
   * 对单个权限请求放行一次：校验 id/sessionID，确认所属会话开启自动放行
   * 后 POST /permission/:id/reply（reply: 'once'）。
   * @param {object} permission - 权限事件负载（须有 id 与 sessionID）。
   * @returns {Promise<boolean>} 是否真正发起放行；字段缺失或未开启返回 false。
   */
  const replyOnce = async (permission, directory) => {
    if (!permission?.id || !permission?.sessionID) return false;
    await load();
    if (!(await isSessionAutoAccepting(permission.sessionID, directory))) return false;
    await request(`/permission/${encodeURIComponent(permission.id)}/reply`, {
      directory,
      method: 'POST',
      body: { reply: 'once' },
    });
    return true;
  };

  /**
   * 处理一个权限事件：同一 permission id 的并发调用复用同一个 in-flight
   * promise；按 retryDelaysMs 序列重试，404 视为已被处理（返回 true），
   * 全部尝试失败返回 false。结束后从 inFlight 移除，允许后续事件重试。
   * @returns {Promise<boolean>}
   */
  const processPermission = (permission, directory) => {
    if (!permission?.id) return Promise.resolve(false);
    const key = permission.id;
    const existing = inFlight.get(key);
    if (existing) return existing;
    const task = (async () => {
      for (const delay of retryDelaysMs) {
        if (delay > 0) await wait(delay);
        try {
          return await replyOnce(permission, directory);
        } catch (error) {
          if (error?.status === 404) return true;
        }
      }
      return false;
    })().finally(() => inFlight.delete(key));
    inFlight.set(key, task);
    return task;
  };

  /**
   * 对账存量待放行权限：拉取全局与各指定目录的 /permission 列表，按 id
   * 去重后逐个尝试放行。相同目录集合的并发调用共享同一 promise；单个
   * 目录请求失败仅跳过该目录，不影响其余范围。
   * @param {object} [options] - directories 为目录数组（空数组表示全量对账）。
   */
  async function reconcilePending({ directories = [] } = {}) {
    const normalizedDirectories = Array.from(new Set(
      directories.filter((directory) => typeof directory === 'string' && directory.trim()).map((directory) => directory.trim()),
    ));
    const key = normalizedDirectories.length > 0 ? normalizedDirectories.join('\n') : 'all';
    const existing = reconcilePromises.get(key);
    if (existing) return existing;
    const task = (async () => {
      await load();
      const scopes = [undefined, ...normalizedDirectories];
      const pendingById = new Map();
      for (const directory of scopes) {
        let payload;
        try {
          payload = await request('/permission', { directory });
        } catch {
          continue;
        }
        const pending = Array.isArray(payload) ? payload : Array.isArray(payload?.data) ? payload.data : null;
        if (!pending) continue;
        for (const permission of pending) {
          if (!permission?.id) continue;
          pendingById.set(permission.id, { permission, directory: permission.directory ?? directory });
        }
      }
      await Promise.all(Array.from(pendingById.values()).map(({ permission, directory }) =>
        processPermission(permission, directory)));
    })().finally(() => { reconcilePromises.delete(key); });
    reconcilePromises.set(key, task);
    return task;
  }

  /**
   * 全局事件分发：session.created / session.updated 时缓存会话信息；
   * permission.asked 时触发一次放行尝试（fire-and-forget）。directory 为
   * 'global' 时视为无目录；负载兼容裸事件与嵌套 payload 两种形状。
   */
  const processEvent = (event) => {
    const raw = event?.payload;
    const payload = raw?.payload && typeof raw.payload === 'object' ? raw.payload : raw;
    const directory = typeof event?.directory === 'string' && event.directory !== 'global' ? event.directory : undefined;
    if (payload?.type === 'session.created' || payload?.type === 'session.updated') {
      rememberSession(payload.properties?.info, directory);
      return;
    }
    if (payload?.type === 'permission.asked') {
      void processPermission(payload.properties, directory);
    }
  };

  /**
   * 启动运行时：订阅事件与连接状态（重连即全量对账），预加载策略后再对账
   * 一次；策略加载失败仅告警。返回反初始化函数用于退订两个订阅。
   * @returns {Function} 停止函数。
   */
  const start = () => {
    const unsubscribeEvent = globalEventHub.subscribeEvent(processEvent);
    const unsubscribeStatus = globalEventHub.subscribeStatus((status) => {
      if (status?.type === 'connect') void reconcilePending();
    });
    void load().then(() => reconcilePending()).catch((error) => {
      console.warn('[permission-auto-accept] failed to load policy:', error?.message ?? error);
    });
    return () => {
      unsubscribeEvent();
      unsubscribeStatus();
    };
  };

  // 导出：策略读取与写入、会话判定、权限处理与对账、生命周期
  return {
    snapshot,
    load,
    setSessionPolicy,
    isSessionAutoAccepting,
    processPermission,
    reconcilePending,
    start,
  };
}

/**
 * 在 Express app 上注册权限自动放行的 HTTP 路由：
 * GET /api/permission-auto-accept 返回策略快照（读取失败 500）；
 * PUT /api/permission-auto-accept/sessions/:sessionId 更新会话开关，
 * 入参非法返回 400（TypeError），其余错误返回 500。
 * @param {object} app - Express 应用实例。
 * @param {object} runtime - 由 createPermissionAutoAcceptRuntime 创建的运行时。
 */
export function registerPermissionAutoAcceptRoutes(app, runtime) {
  app.get('/api/permission-auto-accept', async (_req, res) => {
    try {
      res.json(await runtime.load());
    } catch (error) {
      res.status(500).json({ error: error?.message ?? 'Failed to load permission auto-accept policy' });
    }
  });

  app.put('/api/permission-auto-accept/sessions/:sessionId', async (req, res) => {
    try {
      const directory = typeof req.body?.directory === 'string' ? req.body.directory : undefined;
      res.json(await runtime.setSessionPolicy(req.params.sessionId, req.body?.enabled, directory));
    } catch (error) {
      res.status(error instanceof TypeError ? 400 : 500).json({ error: error?.message });
    }
  });
}
