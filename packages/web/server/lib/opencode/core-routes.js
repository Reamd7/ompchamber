/**
 * OpenCode web server 的核心路由与通用中间件注册。
 *
 * 按功能拆分为四组：服务器状态/系统路由（registerServerStatusRoutes）、
 * 认证与设备访问路由（registerAuthAndAccessRoutes）、设置工具路由
 * （registerSettingsUtilityRoutes）以及通用请求中间件
 * （registerCommonRequestMiddleware）。
 */
import { buildExternalManualRestartResponse } from './config-mutation-response.js';

/**
 * 解析并校验一个回环（loopback）URL：仅接受 http/https 协议且 hostname
 * 为 localhost/127.0.0.1/::1/0.0.0.0。供 /api/system/probe-url 使用，
 * 防止把服务端探测请求发往任意地址（SSRF 防护）。
 * @param {*} rawUrl 原始输入
 * @returns {URL|null} 合法的回环 URL；否则 null
 */
const parseLoopbackUrl = (rawUrl) => {
  if (typeof rawUrl !== 'string') {
    return null;
  }

  let url;
  try {
    url = new URL(rawUrl);
  } catch {
    return null;
  }

  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    return null;
  }

  const host = url.hostname;
  if (host !== 'localhost' && host !== '127.0.0.1' && host !== '::1' && host !== '0.0.0.0') {
    return null;
  }

  return url;
};

/**
 * 注册服务器状态与系统管理路由：/health、/api/version、
 * /api/system/shutdown、/api/system/dev-shutdown、/api/system/info、
 * /api/system/free-port。
 *
 * @param {object} app express 应用实例
 * @param {object} dependencies 依赖注入集合
 * @param {object} dependencies.express express 模块（用于 json 中间件）
 * @param {object} dependencies.process 宿主进程对象（读 env/pid/ppid）
 * @param {string} dependencies.ompchamberVersion 当前版本号
 * @param {string} dependencies.runtimeName 运行时名称（web/desktop/ssh-remote 等）
 * @param {string} dependencies.serverStartedAt 启动时间戳
 * @param {Function} dependencies.gracefulShutdown 优雅停机函数
 * @param {Function} dependencies.getHealthSnapshot 返回健康快照字段
 * @param {Function} [dependencies.getServerPort] 返回服务端口；旧接线缺省为 null
 * @param {Function} [dependencies.getTunnelUrl] 返回隧道公网 URL；未启用为 null
 * @param {Function} [dependencies.getServerId] 返回稳定服务器身份（签名公钥哈希，非机密）
 * @param {object} [dependencies.tunnelAuthController] 隧道会话认证控制器
 * @param {object} [dependencies.uiAuthController] UI 会话认证控制器
 */
export const registerServerStatusRoutes = (app, dependencies) => {
  const {
    express,
    process,
    ompchamberVersion,
    runtimeName,
    serverStartedAt,
    gracefulShutdown,
    getHealthSnapshot,
    // Port this OMPChamber instance serves on and the tunnel public URL (if
    // a tunnel is active). Exposed on /api/system/info so the UI can surface
    // the active instance's service URLs. Optional: older wiring omits them
    // and the endpoint reports null.
    getServerPort = () => null,
    getTunnelUrl = () => null,
    // Stable server identity (hash of the public signing key — not a secret).
    // Exposed on /health and /api/version so a client can verify that a
    // learned/probed address belongs to the expected server BEFORE sending its
    // bearer token there. Optional: older wiring omits it.
    getServerId = async () => null,
    tunnelAuthController = null,
    uiAuthController = null,
  } = dependencies;

  // The identity is immutable for the process lifetime; resolve once, and never
  // let an identity failure break health reporting.
  // 进程生命周期内不变的 serverId 缓存；解析失败固定为 null，不影响健康上报。
  let cachedServerId = null;
  /**
   * 解析（并缓存）稳定服务器身份；获取失败返回 null 而非抛错。
   * @returns {Promise<string|null>}
   */
  const resolveServerId = async () => {
    if (cachedServerId) return cachedServerId;
    try {
      const value = await getServerId();
      cachedServerId = typeof value === 'string' && value.trim() ? value.trim() : null;
    } catch {
      cachedServerId = null;
    }
    return cachedServerId;
  };

  /**
   * 在 127.0.0.1 上临时 listen(0) 获取一个空闲端口后立即释放并返回
   * （尽力而为的端口提示，返回前可能被其它进程抢占）。
   * @returns {Promise<number>} 分配到的端口号
   */
  const allocateLoopbackPort = async () => {
    const net = await import('node:net');
    return await new Promise((resolve, reject) => {
      const server = net.createServer();
      server.on('error', reject);
      server.listen(0, '127.0.0.1', () => {
        try {
          const address = server.address();
          const port = address && typeof address === 'object' ? address.port : 0;
          server.close(() => {
            resolve(port);
          });
        } catch (error) {
          try {
            server.close();
          } catch {
          }
          reject(error);
        }
      });
    });
  };

  // 客户端兼容性声明：API 版本与能力位列表（健康检查、runtime URL、
  // 原始文件、SSE、全局事件 WebSocket、terminal WebSocket 等）。
  const compatibility = {
    apiVersion: 1,
    minClientApiVersion: 1,
    capabilities: [
      'api.health.v1',
      'api.runtime-url.v1',
      'api.raw-file.v1',
      'realtime.sse.v1',
      'realtime.websocket.global-events.v1',
      'terminal.websocket.v1',
    ],
  };

  /**
   * 是否允许 dev-only 的整组进程关闭逃生口（OMPCHAMBER_DEV_SHUTDOWN
   * 显式开启）；生产运行时永远为 false。
   * @returns {boolean}
   */
  const isDevShutdownAllowed = () => {
    // Dev-only escape hatch: allow terminating the whole dev process group.
    // This should never be enabled in production runtimes.
    return process.env.OMPCHAMBER_DEV_SHUTDOWN === 'true';
  };

  /**
   * 校验请求 Origin 头与 Host 一致（防跨站站点触发 dev-shutdown）。
   * @param {object} req express 请求
   * @returns {boolean} 是否同源
   */
  const isSameOriginRequest = (req) => {
    const rawOrigin = typeof req.get === 'function' ? req.get('origin') : '';
    const rawHost = typeof req.get === 'function' ? req.get('host') : '';
    if (!rawOrigin || !rawHost) {
      return false;
    }
    try {
      const origin = new URL(rawOrigin);
      return origin.host === rawHost;
    } catch {
      return false;
    }
  };

  /**
   * 用 ps 查询进程所属的进程组 id（pgid）；Windows 或查询失败返回 null。
   * @param {number} pid 目标进程 pid
   * @returns {Promise<number|null>}
   */
  const resolveProcessGroupId = async (pid) => {
    if (!pid || typeof pid !== 'number' || !Number.isFinite(pid) || pid <= 0) {
      return null;
    }
    if (process.platform === 'win32') {
      return null;
    }

    try {
      const { execFile } = await import('node:child_process');
      const { promisify } = await import('node:util');
      const execFileAsync = promisify(execFile);
      const result = await execFileAsync('ps', ['-o', 'pgid=', '-p', String(pid)]);
      const raw = String(result.stdout || '').trim();
      const pgid = Number.parseInt(raw, 10);
      return Number.isFinite(pgid) && pgid > 0 ? pgid : null;
    } catch {
      return null;
    }
  };

  /**
   * 从 URL 提取回环端口号：复用 parseLoopbackUrl 的协议/回环校验，
   * 缺省端口按协议补 443/80，越界返回 null。
   * @param {*} rawUrl 原始输入
   * @returns {number|null}
   */
  const parseLoopbackPort = (rawUrl) => {
    if (typeof rawUrl !== 'string') {
      return null;
    }
    let url;
    try {
      url = new URL(rawUrl);
    } catch {
      return null;
    }
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      return null;
    }
    const host = url.hostname;
    if (host !== 'localhost' && host !== '127.0.0.1' && host !== '::1' && host !== '0.0.0.0') {
      return null;
    }
    const port = url.port ? Number.parseInt(url.port, 10) : (url.protocol === 'https:' ? 443 : 80);
    if (!Number.isFinite(port) || port <= 0 || port > 65535) {
      return null;
    }
    return port;
  };

  /**
   * 终止占用指定回环端口的监听进程（仅非 Windows）：lsof 找到 pid 后
   * 先 SIGTERM，1.2 秒后对仍存活者补 SIGKILL；失败静默忽略。
   * @param {number} port 目标端口
   * @returns {Promise<void>}
   */
  const killListenPort = async (port) => {
    if (!Number.isFinite(port) || port <= 0) {
      return;
    }
    if (process.platform === 'win32') {
      return;
    }

    try {
      const { execFile } = await import('node:child_process');
      const { promisify } = await import('node:util');
      const execFileAsync = promisify(execFile);
      const result = await execFileAsync('lsof', ['-nP', '-t', `-iTCP:${Math.trunc(port)}`, '-sTCP:LISTEN'], {
        timeout: 2500,
      });
      const pids = String(result.stdout || '')
        .split(/\s+/)
        .map((value) => Number.parseInt(value, 10))
        .filter((pid) => Number.isFinite(pid) && pid > 0 && pid !== process.pid);

      for (const pid of pids) {
        try {
          process.kill(pid, 'SIGTERM');
        } catch {
        }
      }
      if (pids.length > 0) {
        setTimeout(() => {
          for (const pid of pids) {
            try {
              process.kill(pid, 'SIGKILL');
            } catch {
            }
          }
        }, 1200).unref?.();
      }
    } catch {
      // ignore (no lsof, no permission, etc.)
    }
  };

  // GET /health：无鉴权健康检查，返回状态、版本、运行时、兼容性声明、
  // 健康快照与可选 serverId（供客户端在发送 bearer token 前核对身份）。
  app.get('/health', async (_req, res) => {
    const serverId = await resolveServerId();
    res.json({
      status: 'ok',
      timestamp: new Date().toISOString(),
      ompchamberVersion,
      runtime: runtimeName,
      compatibility,
      ...(serverId ? { serverId } : {}),
      ...getHealthSnapshot(),
    });
  });

  // GET /api/version：版本与启动信息，附兼容性声明与可选 serverId。
  app.get('/api/version', async (_req, res) => {
    const serverId = await resolveServerId();
    res.json({
      status: 'ok',
      ompchamberVersion,
      runtime: runtimeName,
      startedAt: serverStartedAt,
      compatibility,
      ...(serverId ? { serverId } : {}),
    });
  });

  /**
   * shutdown 路由的鉴权中间件：隧道/公网未知来源走隧道会话校验，其余
   * 走 UI 会话校验；未配置控制器时直接放行（本地开发场景）。
   */
  const requireShutdownAuth = async (req, res, next) => {
    if (!uiAuthController || typeof uiAuthController.requireAuth !== 'function') {
      return next();
    }
    const requestScope = typeof tunnelAuthController?.classifyRequestScope === 'function'
      ? tunnelAuthController.classifyRequestScope(req)
      : 'local';
    if (
      (requestScope === 'tunnel' || requestScope === 'unknown-public')
      && typeof tunnelAuthController?.requireTunnelSession === 'function'
    ) {
      return tunnelAuthController.requireTunnelSession(req, res, next);
    }
    return uiAuthController.requireAuth(req, res, next);
  };

  // POST /api/system/shutdown：鉴权通过后先应答 { ok: true }，再异步执行
  // 优雅停机（exitProcess）；停机失败只记录日志。
  app.post('/api/system/shutdown', async (req, res, next) => {
    try {
      await requireShutdownAuth(req, res, () => {
        res.json({ ok: true });
        gracefulShutdown({ exitProcess: true }).catch((error) => {
          console.error('Shutdown request failed:', error?.message || error);
        });
      });
    } catch (error) {
      next(error);
    }
  });

  // POST /api/system/dev-shutdown：dev 专用逃生口。校验开关与同源后先应答，
  // 再清理 UI 提供的回环预览端口进程，并对自身/父进程所在进程组先
  // SIGTERM 后 SIGKILL 整组终止，兜底强制退出，保证 bun run dev 不留孤儿。
  app.post('/api/system/dev-shutdown', express.json({ limit: '64kb' }), async (req, res) => {
    if (!isDevShutdownAllowed()) {
      return res.status(403).json({ ok: false, error: 'Dev shutdown is disabled' });
    }
    if (!isSameOriginRequest(req)) {
      return res.status(403).json({ ok: false, error: 'Invalid origin' });
    }

    res.json({ ok: true });

    // Terminate the entire dev process group so `bun run dev` leaves no orphans.
    // We still run graceful shutdown to clean up OpenCode, terminals, websockets.
    try {
      const rawPreviewUrls = Array.isArray(req.body?.previewUrls) ? req.body.previewUrls : [];
      const previewPorts = Array.from(new Set(
        rawPreviewUrls
          .map((value) => parseLoopbackPort(value))
          .filter((port) => typeof port === 'number')
      ));
      // Attempt to stop preview servers that may have daemonized away from the PTY.
      // This is dev-only and limited to loopback ports supplied by the UI.
      await Promise.all(previewPorts.map((port) => killListenPort(port)));

      const pgid = await resolveProcessGroupId(process.pid);
      const ppid = typeof process.ppid === 'number' ? process.ppid : null;
      const parentPgid = ppid ? await resolveProcessGroupId(ppid) : null;

      // Kick off shutdown cleanup first.
      void gracefulShutdown({ exitProcess: false });

      const pgidsToKill = Array.from(new Set([pgid, parentPgid].filter(Boolean)));
      for (const id of pgidsToKill) {
        try {
          process.kill(-id, 'SIGTERM');
        } catch {
        }
      }

      setTimeout(() => {
        for (const id of pgidsToKill) {
          try {
            process.kill(-id, 'SIGKILL');
          } catch {
          }
        }
      }, 1500).unref?.();

      // Ensure the server process itself exits even if the group kill fails.
      setTimeout(() => {
        try {
          process.exit(0);
        } catch {
        }
      }, 2500).unref?.();
    } catch (error) {
      console.error('Dev shutdown request failed:', error?.message || error);
      // As a last resort, exit.
      try {
        process.exit(0);
      } catch {
      }
    }
  });

  // GET /api/system/info：当前实例的服务信息（版本、运行时、pid、启动时间、
  // 服务端口与隧道公网 URL），供 UI 展示实际可达的服务地址。
  app.get('/api/system/info', (_req, res) => {
    res.json({
      ompchamberVersion,
      runtime: runtimeName,
      pid: process.pid,
      startedAt: serverStartedAt,
      port: getServerPort(),
      tunnelUrl: getTunnelUrl(),
    });
  });

  // Allocates a best-effort free TCP port hint on 127.0.0.1.
  // Another process can still claim it before the preview server binds.
  // GET /api/system/free-port：返回一个尽力分配的空闲回环端口提示。
  app.get('/api/system/free-port', async (_req, res) => {
    try {
      const port = await allocateLoopbackPort();
      if (!Number.isFinite(port) || port <= 0) {
        return res.status(500).json({ error: 'Failed to allocate port' });
      }
      return res.json({ port });
    } catch (error) {
      return res.status(500).json({ error: (error && error.message) || 'Failed to allocate port' });
    }
  });

};

/**
 * 注册认证与设备访问路由：会话/密码/URL token/passkey 登录、远程客户端
 * （client token）管理、设备配对（创建/列表/取消/兑换）、连接候选刷新、
 * /connect 引导链接，以及挂在 /api 前缀上的统一鉴权中间件。
 *
 * @param {object} app express 应用实例
 * @param {object} dependencies 依赖注入集合
 * @param {object} dependencies.express express 模块
 * @param {object} dependencies.tunnelAuthController 隧道会话认证控制器
 * @param {object} dependencies.uiAuthController UI 会话认证控制器
 * @param {object} dependencies.remoteClientAuthRuntime 远程客户端管理运行时
 * @param {object} dependencies.clientPairingRuntime 设备配对运行时
 * @param {Function} dependencies.readSettingsFromDiskMigrated 读取（含迁移的）设置
 * @param {Function} dependencies.normalizeTunnelSessionTtlMs 规范化隧道会话 TTL
 * @param {Function} [dependencies.getRelayPairingCandidate] 返回 relay 配对候选（未启用为 null）
 * @param {Function} [dependencies.reconcileRelay] 配对/设备变化后重估 relay 生命周期
 * @param {Function} [dependencies.getPairingTransports] 返回直连传输 URL（local/lan/relayAvailable）
 * @param {Function} [dependencies.getDirectCandidateUrls] 返回当前全部可达的直连 LAN URL
 * @param {Function} [dependencies.getServerId] 返回稳定服务器身份
 * @param {Function} [dependencies.getServerLabel] 配对设备显示的本服务器名称
 */
export const registerAuthAndAccessRoutes = (app, dependencies) => {
  const {
    express,
    tunnelAuthController,
    uiAuthController,
    remoteClientAuthRuntime,
    clientPairingRuntime,
    readSettingsFromDiskMigrated,
    normalizeTunnelSessionTtlMs,
    // Returns the relay pairing candidate ({ type:'relay', relayUrl, serverId,
    // hostEncPubJwk, priority }) when the host relay is enabled, else null.
    // Injected lazily because the relay service is constructed after these routes.
    getRelayPairingCandidate = async () => null,
    // Re-evaluate the relay lifecycle after pairing/device changes.
    reconcileRelay = async () => {},
    // Returns { local, lan, relayAvailable } — the direct transport URLs the
    // server can actually be reached on (LAN derived from the server bind, not
    // the UI origin), for the create-device dialog.
    getPairingTransports = () => ({ local: null, lan: null, relayAvailable: true }),
    // Returns ALL direct LAN URLs the server is currently reachable on (client-
    // reached address first, then interface scan) for the candidates-refresh
    // endpoint. Empty when the server is loopback-only.
    getDirectCandidateUrls = () => [],
    // Stable server identity for client-side verification of learned addresses.
    getServerId = async () => null,
    // Display name a paired device shows for THIS server (issuing machine's
    // hostname), distinct from the per-device pairing label typed by the operator.
    getServerLabel = () => 'OMPChamber',
  } = dependencies;
  // 配对兑换限流：滑动窗口时长与窗口内最大尝试次数。
  const PAIRING_REDEEM_RATE_LIMIT_WINDOW_MS = 5 * 60 * 1000;
  // 窗口内允许的最大兑换尝试次数。
  const PAIRING_REDEEM_RATE_LIMIT_MAX_ATTEMPTS = 10;
  // 「IP:pairingId」到尝试计数的限流状态表。
  const pairingRedeemAttempts = new Map();

  /**
   * 在 UI 会话鉴权之后执行 handler；sessionOnly 为 true 时改用
   * requireSessionAuth（不接受 URL token）。错误交给 next(error)。
   * @param {object} req express 请求
   * @param {object} res express 响应
   * @param {Function} next 下一个中间件
   * @param {Function} handler 业务处理函数
   * @param {{sessionOnly?: boolean}} [options] sessionOnly 强制仅会话鉴权
   */
  const runWithUiAuth = async (req, res, next, handler, options = {}) => {
    try {
      const requireAuth = options.sessionOnly === true && typeof uiAuthController.requireSessionAuth === 'function'
        ? uiAuthController.requireSessionAuth
        : uiAuthController.requireAuth;
      await requireAuth(req, res, async () => {
        await handler();
      });
    } catch (error) {
      next(error);
    }
  };

  /**
   * 设备管理类接口的鉴权包装：接受 UI 会话或 client bearer（不允许
   * URL token）；两者都无法解析时回落到仅会话鉴权。handler 收到
   * authContext（type 为 session 或 client）。
   */
  const runWithClientManagementAuth = async (req, res, next, handler) => {
    try {
      if (typeof uiAuthController.resolveAuthContext === 'function') {
        const context = await uiAuthController.resolveAuthContext(req, res, {
          allowClientAuth: true,
          allowUrlToken: false,
        });
        if (context?.type === 'session' || context?.type === 'client') {
          await handler(context);
          return;
        }
      }

      await runWithUiAuth(req, res, next, async () => {
        await handler({ type: 'session' });
      }, { sessionOnly: true });
    } catch (error) {
      next(error);
    }
  };

  /**
   * 创建类接口的鉴权包装：UI 会话或 desktop-local 客户端可用；其它
   * client token 一律 403（客户端令牌不能再创建远程客户端）。
   */
  const runWithClientCreateAuth = async (req, res, next, handler) => {
    try {
      if (typeof uiAuthController.resolveAuthContext === 'function') {
        const context = await uiAuthController.resolveAuthContext(req, res, {
          allowClientAuth: true,
          allowUrlToken: false,
        });
        if (context?.type === 'session') {
          await handler(context);
          return;
        }
        if (context?.type === 'client') {
          const client = await clientRecordFromAuthContext(context);
          if (client?.clientKind === 'desktop-local') {
            await handler({ ...context, client });
            return;
          }
          return res.status(403).json({ error: 'Client tokens cannot create remote clients' });
        }
      }

      await runWithUiAuth(req, res, next, async () => {
        await handler({ type: 'session' });
      }, { sessionOnly: true });
    } catch (error) {
      next(error);
    }
  };

  /**
   * 从鉴权上下文提取 client id；缺失返回 null。
   * @param {object} context 鉴权上下文
   * @returns {string|null}
   */
  const clientIdFromAuthContext = (context) => {
    const raw = context?.client?.id || context?.clientId;
    return typeof raw === 'string' && raw.length > 0 ? raw : null;
  };

  /**
   * 从鉴权上下文解析完整 client 记录：上下文已内嵌记录直接使用，否则
   * 按 id 从远程客户端列表查找；查不到返回 null。
   * @param {object} context 鉴权上下文
   * @returns {Promise<object|null>}
   */
  const clientRecordFromAuthContext = async (context) => {
    if (context?.client && typeof context.client === 'object') {
      return context.client;
    }
    const clientId = clientIdFromAuthContext(context);
    if (!clientId) return null;
    const clients = await remoteClientAuthRuntime.listClients();
    return clients.find((client) => client.id === clientId) || null;
  };

  /**
   * 计算请求对外呈现的 origin（优先 X-Forwarded-Proto，其次 socket 是否
   * 加密，再拼上 Host 头）；缺 Host 返回 null。
   * @param {object} req express 请求
   * @returns {string|null}
   */
  const requestOrigin = (req) => {
    const forwardedProto = typeof req.headers?.['x-forwarded-proto'] === 'string'
      ? req.headers['x-forwarded-proto'].split(',')[0].trim()
      : '';
    const protocol = forwardedProto || (req.socket?.encrypted ? 'https' : 'http');
    const host = typeof req.headers?.host === 'string' ? req.headers.host.trim() : '';
    if (!host) return null;
    return `${protocol}://${host}`;
  };

  /**
   * 取客户端 IP：直接读 socket 地址而非 req.ip（trust proxy 下 express
   * 会用 X-Forwarded-For 改写 req.ip，而兑换限流发生在鉴权之前）。
   * @param {object} req express 请求
   * @returns {string}
   */
  const requestIp = (req) => {
    // Do not use req.ip here: Express rewrites it from X-Forwarded-For when
    // trust proxy is enabled, and redeem is unauthenticated before this limit.
    return req.socket?.remoteAddress || req.connection?.remoteAddress || 'unknown';
  };

  /**
   * 从请求体取 pairingId（缺失记为 'missing'），作为限流 key 的一部分。
   * @param {object} req express 请求
   * @returns {string}
   */
  const pairingIdFromRequest = (req) => {
    const raw = typeof req.body?.pairingId === 'string' ? req.body.pairingId.trim() : '';
    return raw || 'missing';
  };

  /**
   * 检查并累计配对兑换限流：按「IP:pairingId」在滑动窗口内最多 N 次，
   * 顺带清理过期表项。
   * @param {object} req express 请求
   * @returns {{allowed: boolean, remaining: number, reset: number, retryAfter?: number}}
   */
  const checkPairingRedeemRateLimit = (req) => {
    const now = Date.now();
    const key = `${requestIp(req)}:${pairingIdFromRequest(req)}`;
    for (const [entryKey, entry] of pairingRedeemAttempts.entries()) {
      if (!entry || now - entry.firstAttemptAt >= PAIRING_REDEEM_RATE_LIMIT_WINDOW_MS) {
        pairingRedeemAttempts.delete(entryKey);
      }
    }
    const entry = pairingRedeemAttempts.get(key);
    if (!entry) {
      pairingRedeemAttempts.set(key, { count: 1, firstAttemptAt: now });
      return { allowed: true, remaining: PAIRING_REDEEM_RATE_LIMIT_MAX_ATTEMPTS - 1, reset: Math.ceil((now + PAIRING_REDEEM_RATE_LIMIT_WINDOW_MS) / 1000) };
    }
    const reset = Math.ceil((entry.firstAttemptAt + PAIRING_REDEEM_RATE_LIMIT_WINDOW_MS) / 1000);
    if (entry.count >= PAIRING_REDEEM_RATE_LIMIT_MAX_ATTEMPTS) {
      return {
        allowed: false,
        remaining: 0,
        reset,
        retryAfter: Math.max(1, Math.ceil((entry.firstAttemptAt + PAIRING_REDEEM_RATE_LIMIT_WINDOW_MS - now) / 1000)),
      };
    }
    entry.count += 1;
    return { allowed: true, remaining: PAIRING_REDEEM_RATE_LIMIT_MAX_ATTEMPTS - entry.count, reset };
  };

  /** 兑换成功后清除对应的限流表项。 */
  const clearPairingRedeemRateLimit = (req) => {
    pairingRedeemAttempts.delete(`${requestIp(req)}:${pairingIdFromRequest(req)}`);
  };

  /**
   * 规范化候选 URL：仅接受 http/https，去掉 hash、查询串与尾部斜杠；
   * 非法输入返回 null。
   * @param {*} value 原始输入
   * @returns {string|null}
   */
  const normalizeCandidateUrl = (value) => {
    if (typeof value !== 'string' || !value.trim()) return null;
    try {
      const parsed = new URL(value.trim());
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
      parsed.hash = '';
      parsed.search = '';
      return parsed.toString().replace(/\/+$/, '');
    } catch {
      return null;
    }
  };

  /**
   * 判断候选 URL 类型：https 视为 tunnel，其余按 lan 处理。
   * @param {string} url 候选 URL
   * @returns {string} 'tunnel' 或 'lan'
   */
  const candidateUrlType = (url) => {
    try {
      return new URL(url).protocol === 'https:' ? 'tunnel' : 'lan';
    } catch {
      return 'lan';
    }
  };

  /**
   * 判断候选 URL 是否为回环地址；解析失败按回环处理（保守跳过）。
   * @param {string} url 候选 URL
   * @returns {boolean}
   */
  const isLoopbackCandidateUrl = (url) => {
    try {
      const hostname = new URL(url).hostname.toLowerCase();
      return hostname === 'localhost' || hostname === '127.0.0.1' || hostname === '::1' || hostname === '[::1]';
    } catch {
      return true;
    }
  };

  // `preferredServerUrl` is the caller-supplied externally reachable URL (the
  // desktop UI reaches its own server over loopback, so the request origin is not
  // scannable — it passes the LAN URL instead). Falls back to the request origin
  // for remote callers where the Host header IS the reachable address.
  //
  // `includeRelay` is the per-link transport choice from the create-link dialog:
  //   true  → add the relay candidate, enabling the relay host on demand;
  //   false → direct only, never relay;
  //   undefined → legacy: advertise relay only if it is already enabled.
  // `includeDirect === false` produces a relay-only link (no direct candidate).
  /**
   * 组装配对链接的服务器候选列表：直连候选（preferredServerUrl 或请求
   * origin，非回环的额外 origin 一并带上）加可选的 relay 候选；客户端
   * 按 priority 竞速，直连不通才落到 relay（relay priority 数值更大）。
   * @param {object} req express 请求
   * @param {{preferredServerUrl?: string, includeRelay?: boolean, includeDirect?: boolean}} [options] 传输选择
   * @returns {Promise<Array<{type: string, url: string, priority: number}>>}
   */
  const pairingServerCandidates = async (req, { preferredServerUrl, includeRelay, includeDirect = true } = {}) => {
    const candidates = [];
    if (includeDirect) {
      const preferred = normalizeCandidateUrl(preferredServerUrl);
      const origin = normalizeCandidateUrl(requestOrigin(req));
      const direct = preferred || origin;
      if (direct) {
        candidates.push({ type: candidateUrlType(direct), url: direct, priority: 10 });
      }
      // The origin the creator is browsing over (e.g. a public https domain in
      // front of a reverse proxy) is a reachable address the server cannot
      // discover from its own interfaces. Carry it as an additional direct
      // candidate so the paired device can keep using that same domain instead
      // of depending on LAN hairpin behavior or relay availability. Loopback
      // origins (desktop shell, localhost dev) are unreachable from another
      // device and are skipped.
      if (origin && direct && origin !== direct && !isLoopbackCandidateUrl(origin)) {
        candidates.push({ type: candidateUrlType(origin), url: origin, priority: 20 });
      }
    }
    // The client races candidates and falls back to relay only if the direct URL
    // is unreachable (relay carries a higher priority number).
    if (includeRelay !== false) {
      try {
        const relayCandidate = await getRelayPairingCandidate({ ensureEnabled: includeRelay === true });
        if (relayCandidate) candidates.push(relayCandidate);
      } catch {
        // A relay enable/status failure must not break direct pairing.
      }
    }
    return candidates;
  };

  /**
   * 以 error.statusCode（缺省 400）返回统一的「配对会话无效或过期」错误，
   * 不泄露具体失败原因（兑换端点在鉴权之前，必须防枚举）。
   * @param {object} res express 响应
   * @param {object} error 捕获的错误
   */
  const sendPairingRedeemError = (res, error) => {
    const statusCode = typeof error?.statusCode === 'number' ? error.statusCode : 400;
    res.status(statusCode).json({ error: 'Invalid or expired pairing session' });
  };

  /**
   * /api 统一鉴权中间件：隧道/公网未知来源走隧道会话校验，其余走 UI
   * 会话校验。挂在全部具体 /api 路由之后，为后续注册的 API 兜底。
   */
  const requireApiAuth = async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return tunnelAuthController.requireTunnelSession(req, res, next);
    }
    return uiAuthController.requireAuth(req, res, next);
  };

  // GET /auth/session：查询会话状态；隧道范围内只认隧道会话（无效即清除
  // cookie 并 401 tunnelLocked），否则委托 uiAuthController.handleSessionStatus。
  app.get('/auth/session', async (req, res) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      const tunnelSession = tunnelAuthController.getTunnelSessionFromRequest(req);
      if (tunnelSession) {
        return res.json({ authenticated: true, scope: 'tunnel' });
      }
      tunnelAuthController.clearTunnelSessionCookie(req, res);
      return res.status(401).json({ authenticated: false, locked: true, tunnelLocked: true });
    }

    try {
      await uiAuthController.handleSessionStatus(req, res);
    } catch {
      res.status(500).json({ error: 'Internal server error' });
    }
  });

  // POST /auth/session：密码登录；隧道/公网未知范围内一律 403（禁用密码登录）。
  app.post('/auth/session', (req, res) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Password login is disabled for tunnel scope', tunnelLocked: true });
    }
    return uiAuthController.handleSessionCreate(req, res);
  });

  // POST /auth/url-token：用一次性 URL token 换取会话。
  app.post('/auth/url-token', async (req, res, next) => {
    try {
      await uiAuthController.handleUrlAuthToken(req, res);
    } catch (error) {
      next(error);
    }
  });

  // GET /auth/passkey/status：passkey 可用状态；隧道范围内固定为禁用。
  app.get('/auth/passkey/status', (req, res) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.json({ enabled: false, hasPasskeys: false, passkeyCount: 0, rpID: null, tunnelLocked: true });
    }
    return uiAuthController.handlePasskeyStatus(req, res);
  });

  // POST /auth/passkey/authenticate/options：生成 passkey 登录挑战；隧道范围内 403。
  app.post('/auth/passkey/authenticate/options', (req, res) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey login is disabled for tunnel scope', tunnelLocked: true });
    }
    return uiAuthController.handlePasskeyAuthenticationOptions(req, res);
  });

  // POST /auth/passkey/authenticate/verify：校验 passkey 登录断言；隧道范围内 403。
  app.post('/auth/passkey/authenticate/verify', (req, res) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey login is disabled for tunnel scope', tunnelLocked: true });
    }
    return uiAuthController.handlePasskeyAuthenticationVerify(req, res);
  });

  // POST /auth/passkey/register/options：需已登录会话，生成 passkey 注册挑战；隧道范围内 403。
  app.post('/auth/passkey/register/options', async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey setup is disabled for tunnel scope', tunnelLocked: true });
    }
    try {
      await uiAuthController.requireSessionAuth(req, res, async () => {
        await uiAuthController.handlePasskeyRegistrationOptions(req, res);
      });
    } catch (error) {
      next(error);
    }
  });

  // POST /auth/passkey/register/verify：需已登录会话，完成 passkey 注册；隧道范围内 403。
  app.post('/auth/passkey/register/verify', async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey setup is disabled for tunnel scope', tunnelLocked: true });
    }
    try {
      await uiAuthController.requireSessionAuth(req, res, async () => {
        await uiAuthController.handlePasskeyRegistrationVerify(req, res);
      });
    } catch (error) {
      next(error);
    }
  });

  // GET /api/passkeys：列出已注册 passkey（需会话；隧道范围内 403）。
  app.get('/api/passkeys', async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey management is disabled for tunnel scope', tunnelLocked: true });
    }
    try {
      await uiAuthController.requireSessionAuth(req, res, async () => {
        await uiAuthController.handlePasskeyList(req, res);
      });
    } catch (error) {
      next(error);
    }
  });

  // DELETE /api/passkeys/:id：吊销指定 passkey（需会话；隧道范围内 403）。
  app.delete('/api/passkeys/:id', async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Passkey management is disabled for tunnel scope', tunnelLocked: true });
    }
    try {
      await uiAuthController.requireSessionAuth(req, res, async () => {
        await uiAuthController.handlePasskeyRevoke(req, res);
      });
    } catch (error) {
      next(error);
    }
  });

  // POST /api/auth/reset：全局登出（需会话；隧道范围内 403）。
  app.post('/api/auth/reset', async (req, res, next) => {
    const requestScope = tunnelAuthController.classifyRequestScope(req);
    if (requestScope === 'tunnel' || requestScope === 'unknown-public') {
      return res.status(403).json({ error: 'Global sign-out is disabled for tunnel scope', tunnelLocked: true });
    }
    try {
      await uiAuthController.requireSessionAuth(req, res, async () => {
        await uiAuthController.handleResetAuth(req, res);
      });
    } catch (error) {
      next(error);
    }
  });

  // GET /api/client-auth/clients：列出远程客户端；desktop-local 客户端与
  // UI 会话可见全部，其它 client token 只能看到自己的记录。
  app.get('/api/client-auth/clients', async (req, res, next) => {
    await runWithClientManagementAuth(req, res, next, async (authContext) => {
      if (authContext.type === 'client') {
        const client = await clientRecordFromAuthContext(authContext);
        // The desktop shell's local client is the trusted operator of this
        // server; it manages devices just like a browser UI session. Every
        // other client token is scoped to its own record.
        if (client?.clientKind !== 'desktop-local') {
          return res.json({ clients: client ? [client] : [] });
        }
      }
      const clients = await remoteClientAuthRuntime.listClients();
      res.json({ clients });
    });
  });

  // POST /api/client-auth/clients：创建远程客户端（UI 会话或 desktop-local
  // 客户端；响应 no-store 缓存，201 返回新客户端凭据）。
  app.post('/api/client-auth/clients', express.json({ limit: '64kb' }), async (req, res, next) => {
    await runWithClientCreateAuth(req, res, next, async () => {
      const result = await remoteClientAuthRuntime.createClient({
        label: req.body?.label,
        clientKind: req.body?.clientKind,
        dedupeKey: req.body?.dedupeKey,
      });
      res.setHeader('Cache-Control', 'no-store');
      res.status(201).json(result);
    });
  });

  // DELETE /api/client-auth/clients/:id：吊销指定客户端；desktop-local 或
  // UI 会话可吊销任意，其它 client token 只能吊销自己（403/404）。
  app.delete('/api/client-auth/clients/:id', async (req, res, next) => {
    await runWithClientManagementAuth(req, res, next, async (authContext) => {
      if (authContext.type === 'client') {
        const actingClient = await clientRecordFromAuthContext(authContext);
        // The desktop shell's local client manages every device; other client
        // tokens may only revoke themselves.
        if (actingClient?.clientKind !== 'desktop-local') {
          const clientId = clientIdFromAuthContext(authContext);
          if (!clientId || clientId !== req.params?.id) {
            return res.status(403).json({ revoked: false, error: 'Client tokens can only revoke themselves' });
          }
        }
      }
      const result = await remoteClientAuthRuntime.revokeClient(req.params?.id);
      if (!result.revoked) {
        return res.status(404).json({ revoked: false, error: 'Client not found' });
      }
      void reconcileRelay();
      res.json(result);
    });
  });

  // DELETE /api/client-auth/clients：清除全部已吊销的客户端（仅 UI 会话
  // 或 desktop-local；成功后重估 relay 需求）。
  app.delete('/api/client-auth/clients', async (req, res, next) => {
    await runWithClientManagementAuth(req, res, next, async (authContext) => {
      if (authContext.type === 'client') {
        const actingClient = await clientRecordFromAuthContext(authContext);
        // Purging revoked devices is a whole-server management action; only the
        // trusted desktop shell client (or a UI session) may do it.
        if (actingClient?.clientKind !== 'desktop-local') {
          return res.status(403).json({ purged: 0, error: 'Client tokens cannot purge revoked devices' });
        }
      }
      const result = await remoteClientAuthRuntime.purgeRevokedClients();
      void reconcileRelay();
      res.json(result);
    });
  });

  // POST /api/client-auth/pairing/sessions：创建配对会话；请求体可指定
  // serverUrl/includeRelay/includeDirect 决定候选传输，响应附候选服务器
  // 列表（含 relay）与配对密钥（no-store，201）。
  app.post('/api/client-auth/pairing/sessions', express.json({ limit: '64kb' }), async (req, res, next) => {
    await runWithClientCreateAuth(req, res, next, async (authContext) => {
      const candidates = await pairingServerCandidates(req, {
        preferredServerUrl: req.body?.serverUrl,
        includeRelay: typeof req.body?.includeRelay === 'boolean' ? req.body.includeRelay : undefined,
        includeDirect: req.body?.includeDirect !== false,
      });
      const usesRelay = candidates.some((candidate) => candidate.type === 'relay');
      const result = await clientPairingRuntime.createPairingSession({
        label: req.body?.label,
        allowedClientKinds: req.body?.allowedClientKinds,
        createdByClientId: clientIdFromAuthContext(authContext),
        usesRelay,
      });
      void reconcileRelay();
      res.setHeader('Cache-Control', 'no-store');
      res.status(201).json({
        ...result,
        server: { label: getServerLabel(), candidates },
      });
    });
  });

  // Current reachable transports for an ALREADY-PAIRED device. Pairing-payload
  // candidates are a snapshot: when DHCP hands this machine a new address, the
  // device's saved LAN candidate goes stale and it is stuck on the relay forever.
  // A client that connected over any live transport calls this to learn the
  // server's present LAN URLs (plus the relay candidate when enabled) and update
  // its saved candidate set. `serverId` lets the client bind the response — and
  // later /health probes of the learned addresses — to this server's identity
  // before trusting them with its bearer token.
  // Auth: UI session or client bearer; never the short-lived URL token.
  // GET /api/client-auth/connection/candidates：已配对设备刷新当前可达的
  // 直连 LAN URL（加可选 relay 候选）与服务器身份，用于更新保存的候选集。
  app.get('/api/client-auth/connection/candidates', async (req, res, next) => {
    await runWithClientManagementAuth(req, res, next, async () => {
      const candidates = [];
      // 读取当前可达的直连 URL 列表（接口抛错时按空列表处理）。
      const directUrls = (() => {
        try {
          const urls = getDirectCandidateUrls(req);
          return Array.isArray(urls) ? urls : [];
        } catch {
          return [];
        }
      })();
      for (const url of directUrls) {
        const normalized = normalizeCandidateUrl(url);
        if (normalized) candidates.push({ type: 'lan', url: normalized, priority: 10 });
      }
      try {
        const relayCandidate = await getRelayPairingCandidate({ ensureEnabled: false });
        if (relayCandidate) candidates.push(relayCandidate);
      } catch {
        // Relay status failure must not break the direct-candidate refresh.
      }
      let serverId = null;
      try {
        const value = await getServerId();
        serverId = typeof value === 'string' && value.trim() ? value.trim() : null;
      } catch {
        serverId = null;
      }
      res.setHeader('Cache-Control', 'no-store');
      res.json({ label: getServerLabel(), ...(serverId ? { serverId } : {}), candidates });
    });
  });

  // Direct transports the server can be reached on (for the create-device dialog).
  // GET /api/client-auth/pairing/transports：创建设备对话框使用的直连传输信息。
  app.get('/api/client-auth/pairing/transports', async (req, res, next) => {
    await runWithClientCreateAuth(req, res, next, async () => {
      res.setHeader('Cache-Control', 'no-store');
      res.json(getPairingTransports(req));
    });
  });

  // Pending pairing sessions (link created, device not yet connected) for the
  // "pending devices" list. Secrets are never included.
  // GET /api/client-auth/pairing/sessions：列出待配对会话（绝不含密钥）。
  app.get('/api/client-auth/pairing/sessions', async (req, res, next) => {
    await runWithClientCreateAuth(req, res, next, async () => {
      const pending = await clientPairingRuntime.listPendingSessions();
      res.setHeader('Cache-Control', 'no-store');
      res.json({ pending });
    });
  });

  // DELETE /api/client-auth/pairing/sessions/:id：取消指定配对会话；不存在 404。
  app.delete('/api/client-auth/pairing/sessions/:id', async (req, res, next) => {
    await runWithClientCreateAuth(req, res, next, async () => {
      const result = await clientPairingRuntime.cancelPairingSession(req.params?.id);
      if (!result.cancelled) {
        return res.status(404).json({ cancelled: false, error: 'Pairing session not found' });
      }
      void reconcileRelay();
      res.json(result);
    });
  });

  // POST /api/client-auth/pairing/redeem：设备端用配对码兑换 client token；
  // 按「IP:pairingId」限流（429），无效/过期统一 400/401，成功返回客户端
  // 凭据、服务器地址与指纹，并重估 relay 需求。
  app.post('/api/client-auth/pairing/redeem', express.json({ limit: '64kb' }), async (req, res, next) => {
    try {
      const rateLimit = checkPairingRedeemRateLimit(req);
      res.setHeader('X-RateLimit-Limit', PAIRING_REDEEM_RATE_LIMIT_MAX_ATTEMPTS);
      res.setHeader('X-RateLimit-Remaining', rateLimit.remaining);
      res.setHeader('X-RateLimit-Reset', rateLimit.reset);
      if (!rateLimit.allowed) {
        res.setHeader('Retry-After', rateLimit.retryAfter);
        return res.status(429).json({ error: 'Invalid or expired pairing session' });
      }
      const result = await clientPairingRuntime.redeemPairingSession({
        pairingId: req.body?.pairingId,
        secret: req.body?.secret,
        clientLabel: req.body?.clientLabel,
        clientKind: req.body?.clientKind,
        deviceName: req.body?.deviceName,
        devicePlatform: req.body?.devicePlatform,
        deviceModel: req.body?.deviceModel,
        appVersion: req.body?.appVersion,
        dedupeKey: req.body?.dedupeKey,
      });
      clearPairingRedeemRateLimit(req);
      // The session became a device: relay demand may have moved from the pending
      // session to the paired device (or a non-relay redeem may drop it).
      void reconcileRelay();
      res.setHeader('Cache-Control', 'no-store');
      res.json({
        ok: true,
        server: {
          label: getServerLabel(),
          url: requestOrigin(req),
          fingerprint: result.pairing?.fingerprint || null,
        },
        client: result.client,
        clientToken: result.token,
      });
    } catch (error) {
      if (error?.message === 'Invalid or expired pairing session') {
        sendPairingRedeemError(res, error);
        return;
      }
      next(error);
    }
  });

  // GET /connect?t=...：引导链接入口；用一次性 bootstrap token 换隧道
  // 会话 cookie 后 302 跳转首页。限流 429、无效/过期 401，异常 500。
  app.get('/connect', async (req, res) => {
    try {
      const token = typeof req.query?.t === 'string' ? req.query.t : '';
      const settings = await readSettingsFromDiskMigrated();
      const tunnelSessionTtlMs = normalizeTunnelSessionTtlMs(settings?.tunnelSessionTtlMs);

      const exchange = tunnelAuthController.exchangeBootstrapToken({
        req,
        res,
        token,
        sessionTtlMs: tunnelSessionTtlMs,
      });

      res.setHeader('Cache-Control', 'no-store');

      if (!exchange.ok) {
        if (exchange.reason === 'rate-limited') {
          res.setHeader('Retry-After', String(exchange.retryAfter || 60));
          return res.status(429).type('text/plain').send('Too many attempts. Please try again later.');
        }
        return res.status(401).type('text/plain').send('Connection link is invalid or expired.');
      }

      return res.redirect(302, '/');
    } catch {
      return res.status(500).type('text/plain').send('Failed to process connect request.');
    }
  });

  // POST /api/system/probe-url：服务端代为探测一个回环 URL 的可达性
  // （仅限回环地址、1.5s 超时、不跟随重定向；2xx-5xx 均算可达）。
  app.post('/api/system/probe-url', express.json({ limit: '16kb' }), async (req, res, next) => {
    try {
      await requireApiAuth(req, res, async () => {
        const url = parseLoopbackUrl(req.body?.url);
        if (!url) {
          return res.status(400).json({ ok: false, error: 'Invalid loopback URL' });
        }

        try {
          const response = await fetch(url.toString(), {
            method: 'GET',
            redirect: 'manual',
            signal: AbortSignal.timeout(1500),
          });
          return res.json({ ok: response.status >= 200 && response.status < 600, status: response.status });
        } catch (error) {
          return res.json({ ok: false, error: error?.message || 'Probe failed' });
        }
      });
    } catch (error) {
      next(error);
    }
  });

  // 挂在 /api 前缀上的统一鉴权中间件（为其后注册的全部 API 路由兜底）。
  app.use('/api', async (req, res, next) => {
    try {
      await requireApiAuth(req, res, next);
    } catch (err) {
      next(err);
    }
  });
};

/**
 * 注册设置相关的工具路由：自定义主题列表与手动配置重载。
 * @param {object} app express 应用实例
 * @param {object} dependencies 依赖注入集合
 * @param {Function} dependencies.readCustomThemesFromDisk 从磁盘读取自定义主题
 * @param {Function} dependencies.refreshOpenCodeAfterConfigChange 配置变更后刷新 OpenCode
 * @param {number} dependencies.clientReloadDelayMs 客户端重载前的延迟毫秒数
 */
export const registerSettingsUtilityRoutes = (app, dependencies) => {
  const {
    readCustomThemesFromDisk,
    refreshOpenCodeAfterConfigChange,
    clientReloadDelayMs,
  } = dependencies;

  // GET /api/config/themes：读取并返回自定义主题列表；读取失败 500。
  app.get('/api/config/themes', async (_req, res) => {
    try {
      const customThemes = await readCustomThemesFromDisk();
      res.json({ themes: customThemes });
    } catch (error) {
      console.error('Failed to load custom themes:', error);
      res.status(500).json({ error: error instanceof Error ? error.message : 'Failed to load custom themes' });
    }
  });

  // POST /api/config/reload：手动重载配置并刷新 OpenCode；外部服务器模式
  // 返回提示用户自行重启的响应，受管模式则要求客户端延时后刷新界面。
  app.post('/api/config/reload', async (_req, res) => {
    try {
      console.log('[Server] Manual configuration reload requested');

      const refreshResult = await refreshOpenCodeAfterConfigChange('manual configuration reload');

      if (refreshResult?.external) {
        return res.json(buildExternalManualRestartResponse(
          'Configuration is saved on disk. Restart your connected OpenCode server to apply the changes.',
        ));
      }

      res.json({
        success: true,
        requiresReload: true,
        message: 'Configuration reloaded successfully. Refreshing interface…',
        reloadDelayMs: clientReloadDelayMs,
      });
    } catch (error) {
      console.error('[Server] Failed to reload configuration:', error);
      res.status(500).json({
        error: error.message || 'Failed to reload configuration',
        success: false,
      });
    }
  });
};

/**
 * 注册通用请求中间件：按路径前缀选择 JSON body 大小限制（/api/behavior
 * 1MB、配置/文件类 50MB、其余 50MB）、urlencoded 解析与可选的请求日志。
 * @param {object} app express 应用实例
 * @param {object} dependencies 依赖注入集合
 * @param {object} dependencies.express express 模块
 * @param {boolean} [dependencies.verboseRequestLogs] 是否打印每个请求的日志
 */
export const registerCommonRequestMiddleware = (app, dependencies) => {
  const { express, verboseRequestLogs = false } = dependencies;

  // 按路径前缀分派 JSON 解析器与大小限制：/api/behavior 1MB；配置/文件类
  // 路径 50MB；其余 /api 路径跳过（交给各自路由的解析器）；非 API 路径 50MB。
  app.use((req, res, next) => {
    if (req.path.startsWith('/api/behavior')) {
      const contentLength = parseInt(req.headers['content-length'] || '0', 10);
      if (contentLength > 1024 * 1024) {
        return res.status(413).json({ error: 'Content exceeds maximum size of 1048576 bytes' });
      }
      express.json({ limit: '1mb' })(req, res, next);
    } else if (
      req.path.startsWith('/api/config/agents') ||
      req.path.startsWith('/api/config/commands') ||
      req.path.startsWith('/api/config/mcp') ||
      req.path.startsWith('/api/config/snippets') ||
      req.path.startsWith('/api/config/settings') ||
      req.path.startsWith('/api/config/skills') ||
      req.path.startsWith('/api/config/plugins') ||
      req.path.startsWith('/api/projects') ||
      req.path.startsWith('/api/fs') ||
      req.path.startsWith('/api/git') ||
      req.path.startsWith('/api/magic-prompts') ||
      req.path.startsWith('/api/prompts') ||
      req.path.startsWith('/api/terminal') ||
      req.path.startsWith('/api/opencode') ||
      req.path.startsWith('/api/push') ||
      req.path.startsWith('/api/notifications') ||
      req.path.startsWith('/api/permission-auto-accept') ||
      req.path.startsWith('/api/provider') ||
      req.path.startsWith('/api/session-folders') ||
      req.path.startsWith('/api/small-model') ||
      req.path.startsWith('/api/walkthrough') ||
      req.path.startsWith('/api/goals') ||
      req.path.startsWith('/api/text') ||
      req.path.startsWith('/api/voice') ||
      req.path.startsWith('/api/tts') ||
      req.path.startsWith('/api/ompchamber/tunnel')
    ) {
      express.json({ limit: '50mb' })(req, res, next);
    } else if (req.path.startsWith('/api')) {
      next();
    } else {
      express.json({ limit: '50mb' })(req, res, next);
    }
  });

  // 解析 urlencoded 请求体（extended 模式，上限 50MB）。
  app.use(express.urlencoded({ extended: true, limit: '50mb' }));

  // verboseRequestLogs 开启时打印每个请求的方法与路径。
  app.use((req, _res, next) => {
    if (verboseRequestLogs) {
      console.log(`${new Date().toISOString()} - ${req.method} ${req.path}`);
    }
    next();
  });
};
