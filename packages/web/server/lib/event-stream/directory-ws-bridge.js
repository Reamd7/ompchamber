/**
 * 目录级消息流 WebSocket 桥：为 `/api/event/ws` 的每个客户端连接
 * 独立建立一条到 OpenCode `/event?directory=...` 的上游 SSE 读取器，
 * 并把事件转发给该客户端（含 replay 续传、resync 透传与心跳）。
 *
 * 与全局桥的差异：无共享 hub、无 replay 环——游标由客户端的
 * Last-Event-ID/epoch 直接与上游协商。
 */

import { sendMessageStreamWsEvent, sendMessageStreamWsFrame, sendWsResyncFrame } from './protocol.js';
import { createUpstreamSseReader } from './upstream-reader.js';

/** Non-empty-string arm check for untrusted boundary values (no `typeof`). */
/** 不可信边界值收敛：非字符串或空串返回 null（不依赖 typeof 窄化）。 */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

/**
 * 判断一次失败的上游响应是否应触发 OpenCode 健康检查：
 * 无 body 时 ok 失败或 5xx 触发；有 body（可 drain）时仅 5xx 触发——
 * 4xx 类失败（如鉴权问题）重启进程也无济于事。
 */
function shouldTriggerUpstreamHealthCheck(upstream) {
  if (!upstream) {
    return true;
  }

  if (!upstream.body) {
    return upstream.ok || upstream.status >= 500;
  }

  return upstream.status >= 500;
}

/**
 * 接入一个目录级 WS 连接并启动其专属上游读取器。
 *
 * 参数：socket 为已升级的客户端；requestedLastEventId/requestedEpoch
 * 为客户端游标及其 boot 身份；requestedDirectory 为目标项目目录；
 * buildOpenCodeUrl/getOpenCodeAuthHeaders/fetchImpl 描述上游；
 * processForwardedEventPayload 为转发派生钩子；wsClients 为跨模块
 * 活跃客户端集合；triggerHealthCheck 在首连失败时按需触发；
 * heartbeatIntervalMs 同时驱动 ping 与 heartbeat；其余参数透传读取器。
 *
 * 函数立即返回（后台异步 run 驱动代理）；客户端关闭或上游终止时
 * 经 cleanup 释放定时器、读取器与登记。
 */
export function acceptDirectoryMessageStreamWsConnection({
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
}) {
  // 桥接生命周期的 AbortSignal：客户端关闭时中止上游读取。
  const controller = new AbortController();
  // 上游是否处于连接状态（决定 heartbeat 是否发送）。
  let upstreamConnected = false;
  // 是否已向客户端发送过 ready 帧（每次连接只握手一次）。
  let streamReady = false;
  // 专属上游读取器实例（run 中创建）。
  let reader = null;

  /** 统一清理：中止 controller、停止读取器、从 wsClients 摘除 socket。 */
  const cleanup = () => {
    if (!controller.signal.aborted) {
      controller.abort();
    }
    reader?.stop();
    wsClients.delete(socket);
  };

  // 协议层保活 ping，周期 heartbeatIntervalMs；异常吞掉。
  const pingInterval = setInterval(() => {
    if (socket.readyState !== 1) {
      return;
    }

    try {
      socket.ping();
    } catch {
    }
  }, heartbeatIntervalMs);

  // 应用层 heartbeat 事件：仅在上游连通时发送，客户端据此判定流活性。
  const heartbeatInterval = setInterval(() => {
    if (!upstreamConnected) {
      return;
    }

    sendMessageStreamWsEvent(socket, { type: 'ompchamber:heartbeat', timestamp: Date.now() }, { directory: 'global' });
  }, heartbeatIntervalMs);

  // 客户端断开：停掉两个定时器并触发统一清理。
  socket.on('close', () => {
    clearInterval(pingInterval);
    clearInterval(heartbeatInterval);
    upstreamConnected = false;
    cleanup();
  });

  // 仅为防止 unhandled 'error' 事件崩溃；清理由 close 处理器负责。
  socket.on('error', () => {
    void 0;
  });

  /**
   * 上游代理主流程：构建读取器（URL 带 directory 参数、游标带
   * epoch），转发事件与控制帧；首连失败以 1011 关闭客户端并按需
   * 触发健康检查；任何路径最终都 cleanup 并尝试关闭 socket。
   */
  const run = async () => {
    /**
     * 读取器 onEvent 回调：boot 帧丢弃；data-less 的 resync 控制帧
     * 透传为客户端 resync（带当前 epoch）；业务事件按目录路由转发，
     * 并经 processForwardedEventPayload 派生合成事件再次发送。
     */
    const forwardEvent = ({ envelope, payload, eventId, eventName }) => {
      if (eventName === 'omp.stream.boot') {
        // Transport identity metadata: never forwarded as a business event.
        return;
      }
      if (payload === null || payload === undefined) {
        // Data-less upstream control: relay as an explicit resync control so
        // the client reconciles instead of missing the gap silently
        // (docs/plan.md §5.4).
        if (eventName === 'omp.stream.resync') {
          const epoch = reader?.getEpoch?.();
          const resyncFrame = { eventId: stringOrNull(eventId) ?? undefined };
          if (epoch) resyncFrame.epoch = epoch;
          sendWsResyncFrame(socket, resyncFrame);
        }
        return;
      }
      const directory = requestedDirectory || envelope?.directory || 'global';

      sendMessageStreamWsEvent(socket, payload, {
        directory,
        eventId: stringOrNull(eventId) ?? undefined,
      });

      processForwardedEventPayload(payload, (syntheticPayload) => {
        sendMessageStreamWsEvent(socket, syntheticPayload, { directory: 'global' });
      });
    };

    try {
      let buildUrlFailed = false;
      /**
       * 首连失败收尾：发送 error 帧、以 1011 关闭 socket、按需触发
       * 健康检查（判定见 shouldTriggerUpstreamHealthCheck），
       * 并停止读取器、执行统一清理。
       */
      const closeWithInitialError = ({ message, closeReason = message, triggerHealthCheckFor = null }) => {
        sendMessageStreamWsFrame(socket, { type: 'error', message });
        socket.close(1011, closeReason);
        if (triggerHealthCheckFor === true || (triggerHealthCheckFor && shouldTriggerUpstreamHealthCheck(triggerHealthCheckFor))) {
          triggerHealthCheck?.();
        }
        reader?.stop();
        cleanup();
      };

      reader = createUpstreamSseReader({
        initialLastEventId: requestedLastEventId,
        // The client's cursor only proves a resume under the boot it
        // learned it on — echo the epoch so the host can verdict the first
        // connect `ok` instead of forcing a resync (plan §5.2.1).
        initialEpoch: requestedEpoch,
        signal: controller.signal,
        stallTimeoutMs: upstreamStallTimeoutMs,
        reconnectDelayMs: upstreamReconnectDelayMs,
        fetchImpl,
        buildUrl: () => {
          buildUrlFailed = false;
          let targetUrl;
          try {
            targetUrl = new URL(buildOpenCodeUrl('/event', ''));
          } catch {
            buildUrlFailed = true;
            throw new Error('OpenCode service unavailable');
          }

          if (requestedDirectory) {
            targetUrl.searchParams.set('directory', requestedDirectory);
          }

          return targetUrl;
        },
        getHeaders: getOpenCodeAuthHeaders,
        onConnect() {
          if (!streamReady) {
            const epoch = reader?.getEpoch?.();
            // Same boot-identity handoff as the global bridge: without it a
            // WS-only client cannot echo `epoch` on reconnect and the resume
            // verdict degrades to cursor-only.
            const readyFrame = { type: 'ready', scope: 'directory' };
            if (epoch) readyFrame.epoch = epoch;
            sendMessageStreamWsFrame(socket, readyFrame);
            streamReady = true;
          }

          upstreamConnected = true;
        },
        onDisconnect() {
          upstreamConnected = false;
        },
        onEvent: forwardEvent,
        onError(error) {
          if (controller.signal.aborted) {
            return;
          }

          if (!streamReady) {
            if (error?.type === 'upstream_unavailable') {
              closeWithInitialError({
                message: `OpenCode event stream unavailable (${error.status})`,
                closeReason: 'OpenCode event stream unavailable',
                triggerHealthCheckFor: error.response,
              });
              return;
            }

            closeWithInitialError({
              message: buildUrlFailed ? 'OpenCode service unavailable' : 'Failed to connect to OpenCode event stream',
              closeReason: buildUrlFailed ? 'OpenCode service unavailable' : 'Failed to connect to OpenCode event stream',
              triggerHealthCheckFor: !buildUrlFailed,
            });
            return;
          }

          if (error?.type === 'stream_error') {
            console.warn('Message stream WS proxy error:', error.error);
          }
        },
      });

      await reader.start();
    } catch (error) {
      if (!controller.signal.aborted) {
        console.warn('Message stream WS proxy error:', error);
        sendMessageStreamWsFrame(socket, { type: 'error', message: 'Message stream proxy error' });
        socket.close(1011, 'Message stream proxy error');
      }
    } finally {
      cleanup();
      try {
        if (socket.readyState === 1 || socket.readyState === 0) {
          socket.close();
        }
      } catch {
      }
    }
  };

  void run();
}
