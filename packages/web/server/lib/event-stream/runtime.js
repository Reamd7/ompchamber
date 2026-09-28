/**
 * 消息流 WS 运行时装配层：在既有 HTTP server 上挂载
 * `/api/global/event/ws` 与 `/api/event/ws` 两个 WebSocket 升级端点，
 * 鉴权通过后把连接分发给全局桥（共享 hub）或目录桥（每连接一条上游）。
 *
 * 另导出 createGlobalUiEventBroadcaster：把服务端自发事件同时广播给
 * SSE 与 WS 两类客户端。
 */

import { WebSocketServer } from 'ws';

import { parseRequestPathname } from '../terminal/terminal-ws-protocol.js';
import {
  MESSAGE_STREAM_DIRECTORY_WS_PATH,
  MESSAGE_STREAM_GLOBAL_WS_PATH,
  MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS,
  sendMessageStreamWsEvent,
} from './protocol.js';
import { createGlobalMessageStreamHub } from './global-hub.js';
import { createGlobalMessageStreamWsBridge } from './global-ws-bridge.js';
import { acceptDirectoryMessageStreamWsConnection } from './directory-ws-bridge.js';
import {
  DEFAULT_UPSTREAM_RECONNECT_DELAY_MS,
  DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
} from './upstream-reader.js';

// wsServer.close() waits for every client socket to report close. A socket
// whose close event never arrives (observed under the Bun dev runtime) would
// pin gracefulShutdown open forever with the upgrade listener already
// removed; the terminal runtime races the same close with a timeout.
/** 关闭 wsServer 的兜底超时（毫秒）：close 事件迟迟不来也不再阻塞关停。 */
const MESSAGE_STREAM_CLOSE_TIMEOUT_MS = 1000;

/** Non-empty-string arm check for untrusted boundary values (no `typeof`). */
/** 不可信边界值收敛：非字符串或空串返回 null（不依赖 typeof 窄化）。 */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

/**
 * 创建服务端自发事件的广播器（SSE + WS 双通道）。
 * 返回 (payload, options) => void：无任何客户端时直接返回；SSE 逐个
 * 写入（异常吞掉），WS 走 sendMessageStreamWsEvent 的背压语义，
 * 发送失败的 socket 从 wsClients 移除。options.directory 缺省
 * 'global'，options.eventId 可选携带续传游标。
 */
export function createGlobalUiEventBroadcaster({
  sseClients,
  wsClients,
  writeSseEvent,
}) {
  return (payload, options = {}) => {
    const hasSseClients = sseClients.size > 0;
    const hasWsClients = wsClients.size > 0;
    if (!hasSseClients && !hasWsClients) {
      return;
    }

    if (hasSseClients) {
      for (const res of sseClients) {
        try {
          writeSseEvent(res, payload);
        } catch {
        }
      }
    }

    if (hasWsClients) {
      for (const socket of Array.from(wsClients)) {
        const sent = sendMessageStreamWsEvent(socket, payload, {
          directory: stringOrNull(options.directory) ?? 'global',
          eventId: stringOrNull(options.eventId) ?? undefined,
        });
        if (!sent) {
          wsClients.delete(socket);
        }
      }
    }
  };
}

/**
 * 创建消息流 WS 运行时并挂接到 server 的 upgrade 事件。
 *
 * 参数：server 为 HTTP server；uiAuthController/isRequestOriginAllowed/
 * rejectWebSocketUpgrade 负责升级鉴权（未启用鉴权时直通）；
 * buildOpenCodeUrl/getOpenCodeAuthHeaders/fetchImpl 描述上游 OpenCode；
 * processForwardedEventPayload 为转发派生钩子；wsClients 为跨模块
 * 活跃客户端集合；triggerHealthCheck 在上游首连失败时触发；
 * heartbeatIntervalMs 与 upstreamStallTimeoutMs/upstreamReconnectDelayMs
 * 为定时与重连参数；globalEventHub 可注入共享 hub（缺省自建并独占）。
 *
 * 返回 { wsServer, rebindUpstream, close }：rebindUpstream 在 OpenCode
 * 托管重启后重拨上游（见其注释）；close 摘除监听并限时关闭。
 */
export function createMessageStreamWsRuntime({
  server,
  uiAuthController,
  isRequestOriginAllowed,
  rejectWebSocketUpgrade,
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  processForwardedEventPayload,
  wsClients,
  triggerHealthCheck,
  heartbeatIntervalMs = MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS,
  upstreamStallTimeoutMs = DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
  upstreamReconnectDelayMs = DEFAULT_UPSTREAM_RECONNECT_DELAY_MS,
  fetchImpl = fetch,
  globalEventHub = null,
}) {
  // 显式升级模式的 WS 服务：upgrade 事件完成鉴权后再 handleUpgrade。
  const wsServer = new WebSocketServer({
    noServer: true,
  });

  // Directory-scoped streams create one upstream reader per client
  // connection. Track those sockets so a managed OpenCode restart can close
  // them: each reader is pinned to the port it connected at and would
  // otherwise keep streaming from an orphaned process on the old port (#2638).
  const directorySockets = new Set();

  // 未注入外部 hub 时由本运行时独占创建并持有（决定无人时能否 stop）。
  const ownsGlobalHub = !globalEventHub;
  // 全局共享 hub：注入优先，否则自建；目录级流不经过它。
  const globalHub = globalEventHub ?? createGlobalMessageStreamHub({
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    fetchImpl,
    upstreamStallTimeoutMs,
    upstreamReconnectDelayMs,
  });

  // 全局桥：hub 与 /api/global/event/ws 客户端之间的转发层。
  const globalBridge = createGlobalMessageStreamWsBridge({
    globalHub,
    ownsGlobalHub,
    wsClients,
    processForwardedEventPayload,
    triggerHealthCheck,
    heartbeatIntervalMs,
  });

  // 连接分发：全局路径走共享桥，否则按 directory 参数建立独立上游流。
  wsServer.on('connection', (socket, req) => {
    const rawUrl = stringOrNull(req?.url) ?? MESSAGE_STREAM_GLOBAL_WS_PATH;
    const pathname = parseRequestPathname(rawUrl);
    const requestUrl = new URL(rawUrl, 'http://127.0.0.1');
    const isGlobalStream = pathname === MESSAGE_STREAM_GLOBAL_WS_PATH;
    const requestedLastEventId = requestUrl.searchParams.get('lastEventId')?.trim() || '';
    const requestedDirectory = requestUrl.searchParams.get('directory')?.trim() || '';
    // Boot identity of the client's cursor (plan §5.4): the bridge can only
    // prove an `ok` resume when the epoch still matches the upstream's.
    const requestedEpoch = requestUrl.searchParams.get('epoch')?.trim() || '';

    if (isGlobalStream) {
      globalBridge.accept(socket, {
        requestedLastEventId,
        requestedEpoch,
      });
      return;
    }

    directorySockets.add(socket);
    socket.on('close', () => {
      directorySockets.delete(socket);
    });

    acceptDirectoryMessageStreamWsConnection({
      socket,
      requestedLastEventId,
      requestedEpoch,
      requestedDirectory,
      buildOpenCodeUrl,
      getOpenCodeAuthHeaders,
      processForwardedEventPayload,
      wsClients,
      triggerHealthCheck,
      heartbeatIntervalMs,
      upstreamStallTimeoutMs,
      upstreamReconnectDelayMs,
      fetchImpl,
    });
  });

/**
 * server 的 upgrade 处理器：只认两个消息流路径，其余请求不处理、
 * 交给后续监听器。鉴权启用时先换取会话 token（失败 401）再校验
 * Origin（失败 403），通过后交给 wsServer.handleUpgrade 并转发
 * connection 事件；任何异常以 500 拒绝升级。
 */
  const upgradeHandler = (req, socket, head) => {
    const pathname = parseRequestPathname(req.url);
    if (pathname !== MESSAGE_STREAM_GLOBAL_WS_PATH && pathname !== MESSAGE_STREAM_DIRECTORY_WS_PATH) {
      return;
    }

    /** 异步完成鉴权后再执行升级；异常统一拒绝为 500。 */
    const handleUpgrade = async () => {
      try {
        if (uiAuthController?.enabled) {
          const sessionToken = await uiAuthController?.ensureSessionToken?.(req, null);
          if (!sessionToken) {
            rejectWebSocketUpgrade(socket, 401, 'UI authentication required');
            return;
          }

          const originAllowed = await isRequestOriginAllowed(req);
          if (!originAllowed) {
            rejectWebSocketUpgrade(socket, 403, 'Invalid origin');
            return;
          }
        }

        wsServer.handleUpgrade(req, socket, head, (ws) => {
          wsServer.emit('connection', ws, req);
        });
      } catch {
        rejectWebSocketUpgrade(socket, 500, 'Upgrade failed');
      }
    };

    void handleUpgrade();
  };

  server.on('upgrade', upgradeHandler);

  return {
    wsServer,
    /**
     * Rebind all upstream readers to the current OpenCode port. Called after
     * a managed process restart: the restart can land on a NEW port while
     * the old process (or an orphaned survivor of it) still holds the
     * previous one, and a healthy-but-pinned SSE connection never notices —
     * so the UI would stop receiving events until the app restarts (#2638).
     * Restarting the shared hub re-dials `buildOpenCodeUrl` (which reads the
     * current port) on its next attempt; directory-scoped readers are
     * rebuilt by closing their client sockets, which reconnect with
     * `Last-Event-ID` and re-establish the stream against the new port.
     */
    /**
     * 把全部上游读取器重绑到当前 OpenCode 端口（中文摘要）：
     * 重启共享 hub（下次连接重新解析 buildOpenCodeUrl 得到新端口），
     * 并关闭目录级 socket，促使客户端带 Last-Event-ID 重连新端口。
     */
    rebindUpstream() {
      globalHub.stop();
      globalHub.start();
      for (const socket of Array.from(directorySockets)) {
        try {
          socket.close(1012, 'OpenCode upstream restarted');
        } catch {
        }
      }
    },
    /**
     * 关停运行时：摘除 upgrade 监听、关闭全局桥、terminate 全部客户端，
     * 以 MESSAGE_STREAM_CLOSE_TIMEOUT_MS 兜底等待 wsServer.close 完成，
     * 最后清空 wsClients 集合。
     */
    async close() {
      server.off('upgrade', upgradeHandler);
      globalBridge.close();

      try {
        for (const client of wsServer.clients) {
          try {
            client.terminate();
          } catch {
          }
        }

        await Promise.race([
          new Promise((resolve) => {
            wsServer.close(() => resolve());
          }),
          new Promise((resolve) => {
            setTimeout(resolve, MESSAGE_STREAM_CLOSE_TIMEOUT_MS).unref?.();
          }),
        ]);
      } catch {
      } finally {
        wsClients.clear();
      }
    },
  };
}
