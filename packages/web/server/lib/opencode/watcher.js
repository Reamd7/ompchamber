import { createUpstreamSseReader } from '../event-stream/upstream-reader.js';

/**
 * OpenCode 全局事件监听（PushWatcher）：把引擎的 /global/event 事件流
 * 接入服务端。支持两种传输：注入了 globalEventHub（多路复用的全局事件
 * 中枢）时订阅它；否则用 createUpstreamSseReader 建立独立的 SSE 长连接
 * （含失速超时与自动重连）。事件负载经解包后统一交给 onPayload 分发。
 */

/**
 * 创建 watcher 运行时实例。
 *
 * @param {object} deps
 * @param {Function} deps.waitForOpenCodePort 等待引擎端口就绪
 * @param {Function} deps.buildOpenCodeUrl 构造引擎事件端点 URL
 * @param {Function} deps.getOpenCodeAuthHeaders 引擎请求的认证头
 * @param {Function} deps.onPayload 事件负载回调（服务端分发入口）
 * @param {Function} [deps.fetchImpl] 可注入的 fetch（默认全局 fetch）
 * @param {number} [deps.upstreamStallTimeoutMs] SSE 失速超时
 * @param {number} [deps.upstreamReconnectDelayMs] 断线重连间隔（默认 1s）
 * @param {object|null} [deps.globalEventHub] 全局事件中枢（提供时优先订阅而非自建 SSE）
 * @returns {{ start: Function, stop: Function }}
 */
export const createOpenCodeWatcherRuntime = (deps) => {
  const {
    waitForOpenCodePort,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    onPayload,
    fetchImpl = fetch,
    upstreamStallTimeoutMs,
    upstreamReconnectDelayMs = 1000,
    globalEventHub = null,
  } = deps;

  // 运行时句柄：abortController 控制生命周期；reader 为 SSE 读取器；
  // 两个 unsubscribe 为 globalEventHub 的退订函数。stop() 后全部复位为 null。
  let abortController = null;
  let reader = null;
  let unsubscribeEvent = null;
  let unsubscribeStatus = null;

  /**
   * 解包全局事件负载：若 eventData 自带 .payload 对象则取内层（事件中枢
   * 的转发格式），否则视为已是裸负载；非对象输入返回 null（调用方跳过）。
   */
  const unwrapGlobalEventPayload = (eventData) => {
    if (!eventData || typeof eventData !== 'object') {
      return null;
    }

    if (eventData.payload && typeof eventData.payload === 'object') {
      return eventData.payload;
    }

    return eventData;
  };

  /**
   * 启动监听（幂等：已在运行时直接返回）。先等引擎端口就绪，再按是否
   * 提供 globalEventHub 二选一：订阅中枢的事件 / 状态通道，或创建带
   * 重连能力的 SSE 读取器。SSE 的启动不 await（void），错误经 onError
   * 记录日志后由读取器自行重连。
   */
  const start = async () => {
    if (abortController) {
      return;
    }

    await waitForOpenCodePort();

    abortController = new AbortController();
    const signal = abortController.signal;

    if (globalEventHub) {
      unsubscribeEvent = globalEventHub.subscribeEvent((event) => {
        const payload = unwrapGlobalEventPayload(event.payload);
        if (!payload || typeof payload !== 'object') {
          return;
        }
        onPayload(payload);
      });
      unsubscribeStatus = globalEventHub.subscribeStatus((status) => {
        if (signal.aborted) {
          return;
        }
        if (status.type === 'connect') {
          console.log('[PushWatcher] connected');
          return;
        }
        if (status.type === 'error' || status.type === 'initial-error') {
          console.warn('[PushWatcher] disconnected', status.error?.error?.message ?? status.error?.message ?? status.error);
        }
      });
      globalEventHub.start();
      return;
    }

    reader = createUpstreamSseReader({
      signal,
      buildUrl: () => buildOpenCodeUrl('/global/event', ''),
      getHeaders: getOpenCodeAuthHeaders,
      fetchImpl,
      stallTimeoutMs: upstreamStallTimeoutMs,
      reconnectDelayMs: upstreamReconnectDelayMs,
      onConnect() {
        console.log('[PushWatcher] connected');
      },
      onEvent(event) {
        const payload = unwrapGlobalEventPayload(event.payload);
        if (!payload || typeof payload !== 'object') {
          return;
        }
        onPayload(payload);
      },
      onError(error) {
        if (signal.aborted) {
          return;
        }
        console.warn('[PushWatcher] disconnected', error?.error?.message ?? error?.message ?? error);
      },
    });

    void reader.start();
  };

  /**
   * 停止监听并释放全部资源：中止 AbortController、停掉 SSE 读取器、退订
   * 中枢回调，然后把句柄复位为 null 以允许再次 start()。未运行时为
   * no-op；清理过程中的异常被吞掉（进程往往正在退出）。
   */
  const stop = () => {
    if (!abortController) {
      return;
    }
    try {
      abortController.abort();
      reader?.stop();
      unsubscribeEvent?.();
      unsubscribeStatus?.();
    } catch {
    }
    reader = null;
    unsubscribeEvent = null;
    unsubscribeStatus = null;
    abortController = null;
  };

  return {
    start,
    stop,
  };
};
