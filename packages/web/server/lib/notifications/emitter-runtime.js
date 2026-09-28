/**
 * 通知发射运行时：负责把一条通知投递到各个客户端表面。
 *
 * 三条通道：桌面原生通知（进程内回调或 stdout 单行 JSON 协议）、
 * 全局 UI 广播（SSE/WS），以及直连 SSE 客户端的兜底写入。
 * 由 createNotificationTriggerRuntime 组合使用。
 */
export const createNotificationEmitterRuntime = (dependencies) => {
  const {
    process,
    getDesktopNotifyEnabled,
    desktopNotifyPrefix,
    getUiNotificationClients,
    getBroadcastGlobalUiEvent,
    // Optional: in-process desktop shells (Electron main) inject a callback so
    // notifications are delivered as a direct function call instead of a stdout
    // stringly-typed IPC.
    onDesktopNotification: initialOnDesktopNotification,
  } = dependencies;

  // Late-bindable: main() in server/index.js may call setOnDesktopNotification
  // after runtime construction so the in-process shell can subscribe without
  // restructuring the module-level wiring.
  let onDesktopNotification = typeof initialOnDesktopNotification === 'function'
    ? initialOnDesktopNotification
    : null;

  /** 注册/替换进程内桌面通知回调；传入非函数则清空，回到 stdout 兜底。 */
  const setOnDesktopNotification = (cb) => {
    onDesktopNotification = typeof cb === 'function' ? cb : null;
  };

  /** 按 SSE `data: {json}` 帧格式把 payload 写入响应流。 */
  const writeSseEvent = (res, payload) => {
    res.write(`data: ${JSON.stringify(payload)}\n\n`);
  };

  /**
   * 发送桌面原生通知；返回是否成功投递。
   * 桌面通知未启用或 payload 非对象时返回 false。
   * 优先调用进程内回调（Electron main 注入），回调抛错视为失败并忽略；
   * 无回调时退回 stdout 单行 `${prefix}{json}` 协议，写入失败同样忽略。
   */
  const emitDesktopNotification = (payload) => {
    const desktopNotifyEnabled = getDesktopNotifyEnabled();
    if (!desktopNotifyEnabled) {
      return false;
    }

    if (!payload || typeof payload !== 'object') {
      return false;
    }

    if (onDesktopNotification) {
      try {
        onDesktopNotification(payload);
        return true;
      } catch {
        // ignore host-side throw
      }
      return false;
    }

    try {
      // stdout fallback for runtimes that parse the one-line `${prefix}{json}` protocol.
      process.stdout.write(`${desktopNotifyPrefix}${JSON.stringify(payload)}\n`);
      return true;
    } catch {
      // ignore
    }

    return false;
  };

  /**
   * 向所有已连接的 UI 客户端广播通知事件。
   * payload 会包装成 ompchamber:notification 事件，并附带
   * desktopNotificationDelivered（原生通道是否已接收，避免客户端重复弹 OS 通知）
   * 与 desktopStdoutActive（兼容旧客户端的 stdout 标记）。
   * 优先走全局广播函数（覆盖 SSE 与 WebSocket），否则逐个写入 SSE 客户端，
   * 单个客户端写入失败不影响其余客户端。
   */
  const broadcastUiNotification = (payload, options = {}) => {
    const desktopNotifyEnabled = getDesktopNotifyEnabled();
    if (!payload || typeof payload !== 'object') {
      return;
    }

    const desktopNotificationDelivered = options.desktopNotificationDelivered === true;

    const syntheticPayload = {
      type: 'ompchamber:notification',
      properties: {
        ...payload,
        // Tell local desktop UI whether a native channel already accepted this
        // notification. If so, the SSE/WS event is informational only and must
        // not create a second OS notification.
        desktopNotificationDelivered,
        // Legacy marker retained for older clients that only know about stdout.
        desktopStdoutActive: desktopNotifyEnabled,
      },
    };

    const broadcastGlobalUiEvent = typeof getBroadcastGlobalUiEvent === 'function'
      ? getBroadcastGlobalUiEvent()
      : null;
    if (broadcastGlobalUiEvent) {
      broadcastGlobalUiEvent(syntheticPayload);
      return;
    }

    const clients = getUiNotificationClients();
    if (clients.size === 0) {
      return;
    }

    for (const res of clients) {
      try {
        writeSseEvent(res, syntheticPayload);
      } catch {
        // ignore
      }
    }
  };

  return {
    writeSseEvent,
    emitDesktopNotification,
    broadcastUiNotification,
    setOnDesktopNotification,
  };
};
