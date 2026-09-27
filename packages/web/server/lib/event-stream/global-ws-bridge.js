/**
 * 全局消息流 WebSocket 桥：把 global hub 的扇出事件转发给
 * `/api/global/event/ws` 上的浏览器客户端，并处理接入握手、
 * replay 续传、resync 通知与双向心跳。
 *
 * 每个客户端维护独立的 Last-Event-ID 游标与 boot epoch；
 * 慢客户端由 sendMessageStreamWsFrame 的背压逻辑断开；
 * 无人使用时可按需停止独占持有的 hub。
 */

import { sendMessageStreamWsEvent, sendMessageStreamWsFrame, sendWsResyncFrame } from './protocol.js';

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
 * 创建全局 WS 桥（与 global hub 一一配合）。
 *
 * 参数：globalHub 为事件源；ownsGlobalHub 标记是否独占持有 hub
 * （决定无人时能否 stop）；wsClients 为跨模块的活跃客户端集合
 * （global broadcaster 依赖它判断是否有听众）；
 * processForwardedEventPayload 为转发派生钩子（可从上游事件合成
 * 额外事件）；triggerHealthCheck 在上游首连失败时按需触发；
 * heartbeatIntervalMs 同时驱动 ping 与 heartbeat 事件。
 *
 * 返回 { accept, close }：accept 接入一个已升级的 socket 并按需
 * 启动 hub；close 退订 hub 并清理全部客户端。
 */
export function createGlobalMessageStreamWsBridge({
  globalHub,
  ownsGlobalHub,
  wsClients,
  processForwardedEventPayload,
  triggerHealthCheck,
  heartbeatIntervalMs,
}) {
  // 已接入的客户端 socket 集合。
  const clients = new Set();
  // 每个客户端的 Last-Event-ID 游标（replay 补发与 resync 后更新）。
  const clientLastEventIds = new Map();
  // Boot epoch each client's cursor belongs to — numeric upstream ids
  // restart per boot, so a cursor without its epoch is unprovable.
  const clientEpochs = new Map();
  // 已完成 ready 握手的客户端；仅它们接收实时扇出。
  const readyClients = new Set();

  /** 移除客户端的全部登记：桥内三张表与外部 wsClients 集合。 */
  const removeClient = (socket) => {
    clients.delete(socket);
    clientLastEventIds.delete(socket);
    clientEpochs.delete(socket);
    readyClients.delete(socket);
    wsClients.delete(socket);
  };

  /**
   * 向客户端补发其游标之后的 replay 段。epoch 不匹配直接判 gap；
   * gap（游标被淘汰/清空或跨 boot）时发送 resync 控制帧、让客户端
   * 从当前尾部重新开始——绝不静默续传看似连续的后缀（plan §5.4）。
   * 任一帧发送失败即移除该客户端。
   */
  const replayEvents = (socket, requestedLastEventId) => {
    const upstreamEpoch = globalHub.getStats().upstreamEpoch;
    const clientEpoch = clientEpochs.get(socket) ?? '';
    // A cross-boot cursor can still numerically match a new-boot replay
    // entry — only the epoch disproves it. Epoch-less clients keep the old
    // cursor-only verdict (documented degraded path; our own pipeline
    // always sends one).
    const epochMismatch = Boolean(requestedLastEventId && clientEpoch && upstreamEpoch && clientEpoch !== upstreamEpoch);
    const { status, events } = epochMismatch ? { status: 'gap', events: [] } : globalHub.replayAfter(requestedLastEventId);
    if (status === 'gap') {
      // Requested id was evicted or cleared: send an explicit resync
      // control with the reconnectable tail — never a silent suffix
      // (docs/plan.md §5.4).
      const tail = globalHub.tailEventId() ?? '';
      const sent = sendWsResyncFrame(socket, { eventId: tail, epoch: upstreamEpoch });
      if (!sent) {
        removeClient(socket);
        return;
      }
      clientLastEventIds.set(socket, tail);
      // The client adopts the resync tail under the CURRENT upstream epoch.
      clientEpochs.set(socket, upstreamEpoch ?? '');
      return;
    }
    for (const entry of events) {
      const sent = sendMessageStreamWsEvent(socket, entry.payload, {
        directory: entry.directory,
        eventId: entry.eventId,
      });
      if (!sent) {
        removeClient(socket);
        return;
      }
    }
  };

  /**
   * 完成首次 ready 握手：发送 ready 帧（携带当前上游 epoch，让
   * WS-only 客户端也能学到 boot 身份）、登记进 wsClients，
   * 随后立即补发 replay。
   */
  const markReady = (socket, requestedLastEventId) => {
    if (socket.readyState !== 1) {
      return;
    }

    const upstreamEpoch = globalHub.getStats().upstreamEpoch;
    // A WS client that never saw a resync otherwise has no way to learn the
    // boot identity its cursor belongs to — without it the epoch verdict in
    // replayEvents stays degraded for exactly the clients it exists to
    // protect.
    const readyFrame = { type: 'ready', scope: 'global' };
    if (upstreamEpoch) readyFrame.epoch = upstreamEpoch;
    const sent = sendMessageStreamWsFrame(socket, readyFrame);
    if (!sent) {
      removeClient(socket);
      return;
    }

    readyClients.add(socket);
    wsClients.add(socket);
    replayEvents(socket, requestedLastEventId);
  };

  /** 独占持有 hub 且已无任何客户端时停止上游连接（按需启停）。 */
  const stopHubIfUnused = () => {
    if (ownsGlobalHub && clients.size === 0) {
      globalHub.stop();
    }
  };

  /**
   * 首连失败路径：向所有客户端发送 error 帧并以 1011 关闭；
   * triggerHealthCheckFor 为 true 或经 shouldTriggerUpstreamHealthCheck
   * 判定后触发健康检查；独占持有 hub 时顺带停止它。
   */
  const closeClientsWithInitialError = ({ message, closeReason = message, triggerHealthCheckFor = null }) => {
    for (const socket of Array.from(clients)) {
      sendMessageStreamWsFrame(socket, { type: 'error', message });
      try {
        socket.close(1011, closeReason);
      } catch {
      }
      removeClient(socket);
    }

    if (triggerHealthCheckFor === true || (triggerHealthCheckFor && shouldTriggerUpstreamHealthCheck(triggerHealthCheckFor))) {
      triggerHealthCheck?.();
    }

    if (ownsGlobalHub) {
      globalHub.stop();
    }
  };

  /**
   * hub 事件订阅：向全部 ready 客户端转发事件；再经
   * processForwardedEventPayload 派生的合成事件以 directory 'global'
   * 二次广播。发送失败的客户端被移除（背压断开语义）。
   */
  const unsubscribeEvent = globalHub.subscribeEvent(({ payload, directory, eventId }) => {
    for (const socket of Array.from(clients)) {
      if (!readyClients.has(socket)) {
        continue;
      }
      const sent = sendMessageStreamWsEvent(socket, payload, {
        directory,
        eventId,
      });
      if (!sent) {
        removeClient(socket);
      }
    }

    processForwardedEventPayload(payload, (syntheticPayload) => {
      for (const socket of Array.from(clients)) {
        if (!readyClients.has(socket)) {
          continue;
        }
        const sent = sendMessageStreamWsEvent(socket, syntheticPayload, { directory: 'global' });
        if (!sent) {
          removeClient(socket);
        }
      }
    });
  });

  /**
   * hub 状态订阅：
   * - connect：未 ready 的客户端补握手；已 ready 的重发 ready 帧
   *   （携带重连后的 epoch，向客户端证明游标仍属当前 boot）。
   * - restart：上游重启或 resync——所有 ready 客户端收到 resync 帧，
   *   游标重置为当前尾部并改挂新 epoch。
   * - initial-error：按错误类型关闭全部客户端（文案区分服务不可用
   *   与连接失败），并按需触发健康检查。
   * - error(stream_error)：仅告警，等待读取器自动重连。
   */
  const unsubscribeStatus = globalHub.subscribeStatus((status) => {
    if (status.type === 'connect') {
      for (const socket of Array.from(clients)) {
        if (!readyClients.has(socket)) {
          markReady(socket, clientLastEventIds.get(socket) ?? '');
          continue;
        }

        if (status.wasReady) {
          const reconnectEpoch = globalHub.getStats().upstreamEpoch;
          const readyFrame = { type: 'ready', scope: 'global' };
          if (reconnectEpoch) readyFrame.epoch = reconnectEpoch;
          const sent = sendMessageStreamWsFrame(socket, readyFrame);
          if (!sent) {
            removeClient(socket);
          }
        }
      }
      return;
    }

    if (status.type === 'restart') {
      // Upstream rebooted or resynced: every client cursor is untrustworthy.
      for (const socket of Array.from(clients)) {
        if (!readyClients.has(socket)) {
          continue;
        }
        const tail = globalHub.tailEventId() ?? '';
        const sent = sendWsResyncFrame(socket, { eventId: tail, epoch: status.epoch });
        if (!sent) {
          removeClient(socket);
          continue;
        }
        clientLastEventIds.set(socket, tail);
        clientEpochs.set(socket, status.epoch ?? '');
      }
      return;
    }

    if (status.type === 'initial-error') {
      const error = status.error;
      if (error?.type === 'upstream_unavailable') {
        closeClientsWithInitialError({
          message: `OpenCode event stream unavailable (${error.status})`,
          closeReason: 'OpenCode event stream unavailable',
          triggerHealthCheckFor: error.response,
        });
        return;
      }

      closeClientsWithInitialError({
        message: status.buildUrlFailed ? 'OpenCode service unavailable' : 'Failed to connect to OpenCode event stream',
        closeReason: status.buildUrlFailed ? 'OpenCode service unavailable' : 'Failed to connect to OpenCode event stream',
        triggerHealthCheckFor: !status.buildUrlFailed,
      });
      return;
    }

    if (status.type === 'error' && status.error?.type === 'stream_error') {
      console.warn('Message stream WS proxy error:', status.error.error);
    }
  });

  /**
   * 接入一个已升级的客户端 socket：启动协议层 ping 与应用层
   * heartbeat 两个定时器，登记游标/epoch，按需启动 hub；上游已连接
   * 则立即完成 ready 握手，否则等待下一次 connect 状态。close 事件
   * 触发清理，并在无人使用时停掉 hub。
   */
  const accept = (socket, { requestedLastEventId = '', requestedEpoch = '' } = {}) => {
    const pingInterval = setInterval(() => {
      if (socket.readyState !== 1) {
        return;
      }

      try {
        socket.ping();
      } catch {
      }
    }, heartbeatIntervalMs);

    const heartbeatInterval = setInterval(() => {
      if (!globalHub.isConnected()) {
        return;
      }

      sendMessageStreamWsEvent(socket, { type: 'ompchamber:heartbeat', timestamp: Date.now() }, { directory: 'global' });
    }, heartbeatIntervalMs);

    socket.on('close', () => {
      clearInterval(pingInterval);
      clearInterval(heartbeatInterval);
      removeClient(socket);
      stopHubIfUnused();
    });

    socket.on('error', () => {
      void 0;
    });

    clients.add(socket);
    clientLastEventIds.set(socket, requestedLastEventId);
    clientEpochs.set(socket, requestedEpoch);
    globalHub.start();
    if (globalHub.isConnected()) {
      markReady(socket, requestedLastEventId);
    }
  };

  /** 关闭桥：退订 hub 的事件与状态，独占时停止 hub，移除全部客户端。 */
  const close = () => {
    unsubscribeEvent();
    unsubscribeStatus();
    if (ownsGlobalHub) {
      globalHub.stop();
    }
    for (const socket of Array.from(clients)) {
      removeClient(socket);
    }
  };

  return {
    accept,
    close,
  };
}
