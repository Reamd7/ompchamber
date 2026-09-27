/**
 * Raw byte tunnel to a dev server running on the OMPChamber host.
 *
 * This is what lets a desktop client preview a dev server that lives on another
 * machine without rewriting anything. The client binds its own local port and
 * pipes it here; the page is then served from a real origin at the root of its
 * own host, so absolute URLs, cookies, HMR sockets, and DevTools all behave
 * exactly as they do locally. No HTML is inspected or modified.
 *
 * Security posture: the reachable set is the same list dev-server discovery
 * offers the user, not "any loopback port". Without that restriction an
 * authenticated client could dial arbitrary local services on the host —
 * databases, admin panels, the OpenCode API — through this socket.
 *
 * Authentication differs from the browser-facing sockets on purpose. Those
 * demand an allowed `Origin`, which is a CSRF defence: a hostile page can make
 * a browser open a WebSocket carrying the user's ambient cookies, and the
 * origin is what exposes it. This tunnel's client is the desktop shell, not a
 * browser, and it authenticates with an explicit bearer token. So:
 *
 * - With an `Origin` header, the request came from a browser context and the
 *   usual origin check applies unchanged.
 * - With no `Origin`, the request must carry client-token auth. A browser
 *   cannot reach this path: the WebSocket API always sends an origin and never
 *   lets a page set an `Authorization` header.
 */
/**
 * 到 OMPChamber 宿主上 dev server 的原始字节隧道（中文说明）。
 *
 * 安全姿态：可达集合就是 dev-server 发现展示给用户的那份列表，而不是
 * "任意 loopback 端口"——否则已认证客户端可以借此拨打宿主上的任意本地
 * 服务（数据库、管理面板、OpenCode API）。认证与浏览器侧 socket 不同：
 * 带 Origin 头则照常做 origin 检查；不带 Origin 的请求必须持客户端
 * bearer token（浏览器 WebSocket 一定会带 origin 且无法自定义
 * Authorization 头，故可区分）。
 */
import net from 'node:net';
import { WebSocketServer } from 'ws';

/** dev tunnel 的 WebSocket upgrade 路径。 */
const DEV_TUNNEL_WS_PATH = '/api/dev-tunnel';
/** One page load opens many sockets; the cap is per host, not per page. */
/** 单宿主并发的隧道 socket 上限（一次页面加载会开很多连接）。 */
const MAX_CONCURRENT_SOCKETS = 64;
/** 连接 dev server（127.0.0.1）的超时。 */
const CONNECT_TIMEOUT_MS = 5_000;

/** 从 upgrade URL 里解析目标端口；路径不符或端口非法（不在 1..65535）时返回 null。 */
const parseRequestedPort = (url) => {
  try {
    const parsed = new URL(String(url || ''), 'http://localhost');
    if (parsed.pathname !== DEV_TUNNEL_WS_PATH) return null;
    const port = Number.parseInt(parsed.searchParams.get('port') || '', 10);
    return Number.isInteger(port) && port > 0 && port <= 65535 ? port : null;
  } catch {
    return null;
  }
};

/** 判断某 URL 是否是 dev tunnel 的 upgrade 路径（供宿主服务器分发 upgrade 事件）。 */
export const isDevTunnelPath = (url) => {
  try {
    return new URL(String(url || ''), 'http://localhost').pathname === DEV_TUNNEL_WS_PATH;
  } catch {
    return false;
  }
};

/**
 * 创建 dev tunnel 运行时：挂到已有 HTTP 服务器的 upgrade 事件上，
 * 校验认证/origin/端口白名单/并发上限后，把 WebSocket 与到
 * 127.0.0.1:port 的 TCP 连接做原始字节对拷。
 * @param {object} server 宿主 HTTP 服务器
 * @param {() => Promise<{ok:boolean,servers:Array<{port:number}>}>} discoverDevServers dev-server 发现
 * @param {object} uiAuthController UI 认证控制器（enabled 时逐请求解析认证上下文）
 * @param {(req: object) => Promise<boolean>} isRequestOriginAllowed Origin 白名单判定
 * @param {(socket: object, status: number, message: string) => void} rejectWebSocketUpgrade 拒绝 upgrade 的统一出口
 * @param {object} [logger] 日志对象
 */
export function createDevTunnelRuntime({
  server,
  discoverDevServers,
  uiAuthController,
  isRequestOriginAllowed,
  rejectWebSocketUpgrade,
  logger = console,
}) {
  // noServer 模式：由 upgradeHandler 手动分发。
  const wsServer = new WebSocketServer({ noServer: true });
  // 当前活跃的隧道 socket 计数（上限 MAX_CONCURRENT_SOCKETS）。
  let openSockets = 0;

  /**
   * A port is reachable only while discovery still reports it. Re-checked on
   * every upgrade rather than cached, so a dev server that stops listening
   * stops being reachable.
   */
  /** 端口只有仍被 dev-server 发现报告时才可达；每次 upgrade 都重查，不缓存。 */
  const isAllowedPort = async (port) => {
    const result = await discoverDevServers();
    if (!result?.ok) return false;
    return result.servers.some((entry) => entry.port === port);
  };

  // 每条隧道连接：解析端口 → 连 127.0.0.1 → 双向搬运字节，任一侧断开即收尾。
  wsServer.on('connection', (socket, req) => {
    const port = parseRequestedPort(req.url);
    if (port === null) {
      socket.close(1008, 'Invalid port');
      return;
    }

    openSockets += 1;
    const upstream = net.connect({ host: '127.0.0.1', port });
    upstream.setNoDelay(true);

    let settled = false;
    /** 收尾（幂等，只结算一次）：递减计数并双向销毁 upstream 与 WebSocket。 */
    const teardown = () => {
      if (settled) return;
      settled = true;
      openSockets -= 1;
      try { upstream.destroy(); } catch { /* already gone */ }
      try { socket.close(); } catch { /* already closing */ }
    };

    const connectTimer = setTimeout(() => {
      if (!upstream.connecting) return;
      logger.warn?.(`[dev-tunnel] timed out connecting to 127.0.0.1:${port}`);
      teardown();
    }, CONNECT_TIMEOUT_MS);

    upstream.on('connect', () => clearTimeout(connectTimer));
    upstream.on('data', (chunk) => {
      if (socket.readyState !== socket.OPEN) return;
      socket.send(chunk);
      // Stop reading from the dev server while the socket drains, otherwise a
      // fast response against a slow client buffers the whole body in memory.
      if (socket.bufferedAmount > 1_000_000) {
        upstream.pause();
        /** 轮询等待 WebSocket 缓冲排空后恢复读取 upstream。 */
        const resume = () => {
          if (socket.bufferedAmount > 1_000_000) {
            setTimeout(resume, 20);
            return;
          }
          upstream.resume();
        };
        setTimeout(resume, 20);
      }
    });
    upstream.on('error', () => { clearTimeout(connectTimer); teardown(); });
    upstream.on('close', () => { clearTimeout(connectTimer); teardown(); });

    socket.on('message', (data) => {
      if (upstream.destroyed) return;
      upstream.write(data);
    });
    socket.on('close', teardown);
    socket.on('error', teardown);
  });

  /**
   * upgrade 事件处理：只认领 dev tunnel 路径；依次校验 UI 认证、
   * Origin（有则查白名单，无则必须是 client token 认证）、端口格式、
   * 并发上限、端口白名单，全部通过才交给 wsServer 完成握手。
   */
  const upgradeHandler = (req, socket, head) => {
    if (!isDevTunnelPath(req.url)) return;
    void (async () => {
      try {
        if (uiAuthController?.enabled) {
          const auth = await uiAuthController.resolveAuthContext(req, null, { allowUrlToken: false });
          if (!auth) {
            rejectWebSocketUpgrade(socket, 401, 'UI authentication required');
            return;
          }
          const hasOrigin = typeof req.headers?.origin === 'string' && req.headers.origin.trim() !== '';
          if (hasOrigin) {
            if (!await isRequestOriginAllowed(req)) {
              rejectWebSocketUpgrade(socket, 403, 'Invalid origin');
              return;
            }
          } else if (auth.type !== 'client') {
            rejectWebSocketUpgrade(socket, 403, 'Client authentication required');
            return;
          }
        }

        const port = parseRequestedPort(req.url);
        if (port === null) {
          rejectWebSocketUpgrade(socket, 400, 'Invalid port');
          return;
        }
        if (openSockets >= MAX_CONCURRENT_SOCKETS) {
          rejectWebSocketUpgrade(socket, 503, 'Too many tunnel connections');
          return;
        }
        if (!await isAllowedPort(port)) {
          // Says which port, because the alternative is an empty response in
          // the panel with nothing anywhere explaining why.
          logger.warn?.(`[dev-tunnel] refused port ${port}: not reported by dev-server discovery`);
          rejectWebSocketUpgrade(socket, 403, 'That port is not an available dev server');
          return;
        }

        wsServer.handleUpgrade(req, socket, head, (ws) => wsServer.emit('connection', ws, req));
      } catch {
        rejectWebSocketUpgrade(socket, 500, 'Upgrade failed');
      }
    })();
  };

  // 挂到宿主服务器的 upgrade 事件上。
  server.on('upgrade', upgradeHandler);

  return {
    path: DEV_TUNNEL_WS_PATH,
    /** 当前活跃的隧道 socket 数（监控/测试用）。 */
    get openSocketCount() {
      return openSockets;
    },
    /** 摘除 upgrade 监听并关闭 WebSocket 服务器。 */
    dispose() {
      server.off('upgrade', upgradeHandler);
      wsServer.close();
    },
  };
}
