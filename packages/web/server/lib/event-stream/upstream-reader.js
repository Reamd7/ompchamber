/**
 * 上游 SSE 读取器：向 OpenCode 服务发起 text/event-stream 长连接，
 * 流式解析事件块、维护 Last-Event-ID 游标与 boot epoch（x-omp-epoch），
 * 并在失速、断流或错误时按策略自动重连。
 *
 * 该模块是 global hub 与 directory WS bridge 共用的上游传输核心；
 * 游标推进语义（malformed 块不推进、控制帧推进、超预算块的处置）
 * 集中在 handleBlock 与 DEFAULT_UPSTREAM_MAX_BLOCK_BYTES 处。
 */

import { parseSseEventEnvelope } from './protocol.js';

/** 默认上游失速超时（毫秒）：一个读窗口内无任何新数据即中止连接并重连。 */
export const DEFAULT_UPSTREAM_STALL_TIMEOUT_MS = 20_000;
/** 并发多条上游读取器时的放大失速超时（默认值的 3 倍），避免同机重连风暴。 */
export const UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS = DEFAULT_UPSTREAM_STALL_TIMEOUT_MS * 3;
/** 每次重连之间的基础退避延迟（毫秒）。 */
export const DEFAULT_UPSTREAM_RECONNECT_DELAY_MS = 250;
// Parser guard (docs/plan.md §5.3.1): a block that exceeds this is not
// dropped silently — complete blocks get their `id:` line extracted and the
// range is disavowed through a synthesized resync control; an unfinished
// oversize buffer aborts the connection instead. The cap must sit ABOVE the
// largest event the host's replay ring can retain (the wire bus keeps events
// up to a 2 MiB serialized estimate, and JSON.stringify's \uXXXX escaping can
// expand control/escape-heavy payloads ~6x beyond that estimate), or an
// oversized-but-retained event would reconnect-loop this reader forever.
/** 单个 SSE 块的默认字节预算（16 MiB）；超限块的处置策略见上方注释。 */
export const DEFAULT_UPSTREAM_MAX_BLOCK_BYTES = 16 * 1024 * 1024;
/** Header the omp host echoes for boot-identity resume (plan §5.2.1). */
/** 上游声明/客户端回显 boot 身份的 header 名（断线续传的关键，plan §5.2.1）。 */
export const UPSTREAM_EPOCH_HEADER = 'x-omp-epoch';

/** Non-empty-string arm check for untrusted boundary values (no `typeof`). */
/** 不可信边界值收敛：非字符串或空串返回 null（repo 惯用法，不依赖 typeof）。 */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

/**
 * 解析超时配置：接受数值或返回数值的函数（每个读窗口重新求值，
 * 支持运行时调整），结果非有限数值时回落到 fallback。
 */
function resolveTimeoutMs(value, fallback) {
  const resolved = value instanceof Function ? value() : value;
  return Number.isFinite(resolved) ? resolved : fallback;
}

/**
 * 可中断的重连退避等待：至少等 ms 毫秒，signal 中止（stop）时立即返回。
 * 只 resolve 不 reject；结束时总是解绑 abort 监听。
 */
function waitForReconnectDelay(ms, signal) {
  if (signal?.aborted) {
    return Promise.resolve();
  }

  return new Promise((resolve) => {
    let settled = false;
    // 幂等收尾：只结算一次，并解绑 abort 监听。
    const finish = () => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener('abort', onAbort);
      resolve();
    };
    const timeout = setTimeout(finish, Math.max(0, ms));
    // 提前中止：清除计时器并立即结束等待。
    const onAbort = () => {
      clearTimeout(timeout);
      finish();
    };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * 复制调用方提供的附加请求头；非对象输入返回空对象，
 * 防止外部对象被读取器长期持有或意外污染。
 */
function normalizeHeaders(headers) {
  if (!(headers instanceof Object)) {
    return {};
  }

  return { ...headers };
}

/**
 * 释放响应体以归还底层连接：body 缺失或无 cancel 方法时静默跳过，
 * 取消失败也吞掉——清理路径绝不抛出。
 */
async function cancelResponseBody(response) {
  if (response?.body && response.body.cancel instanceof Function) {
    await response.body.cancel().catch(() => {});
  }
}

/**
 * 创建可重入的上游 SSE 读取器（global hub 与目录桥共用）。
 *
 * 配置：buildUrl 为每次连接尝试调用的 URL 工厂（读取当前端口，
 * 支持 rebindUpstream）；getHeaders 附加认证头；fetchImpl/parseBlock
 * 为测试注入点；initialLastEventId/initialEpoch 为续传游标及其
 * boot 身份（代理场景透传下游游标）；signal 为外部中止信号；
 * stallTimeoutMs 支持数值或函数；reconnectDelayMs 为重连退避；
 * maxBlockBytes 为单块字节预算；onEvent/onConnect/onDisconnect/
 * onError/onEpochChange 为生命周期回调。
 *
 * 返回 { start, stop, getLastEventId, getEpoch, getStats }：
 * start 幂等并返回运行 Promise；stop 幂等中止当前连接；
 * 两个 getter 暴露游标与 boot 身份；getStats 返回丢弃统计的浅拷贝。
 */
export function createUpstreamSseReader({
  buildUrl,
  getHeaders = () => ({}),
  fetchImpl = fetch,
  parseBlock = parseSseEventEnvelope,
  initialLastEventId = '',
  initialEpoch = '',
  signal,
  stallTimeoutMs = DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
  reconnectDelayMs = DEFAULT_UPSTREAM_RECONNECT_DELAY_MS,
  maxBlockBytes = DEFAULT_UPSTREAM_MAX_BLOCK_BYTES,
  onEvent,
  onConnect,
  onDisconnect,
  onError,
  onEpochChange,
}) {
  // 运行中循环的 Promise（null 表示未启动/已结束），幂等 start 的判据。
  let running = null;
  // 置位后循环退出且不再重连。
  let stopped = false;
  // 当前连接尝试的 AbortController：stop 或失速时中止以打断 fetch/read。
  let activeController = null;
  // Last-Event-ID 游标：仅在确认处理块后推进，随重连请求回传上游。
  let lastEventId = stringOrNull(initialLastEventId) ?? '';
  /** Last upstream boot identity; echoed on reconnects to detect restarts.
   *  `initialEpoch` lets a proxying reader (directory WS bridge) forward the
   *  downstream client's learned epoch so the first connect can still prove
   *  an `ok` same-boot resume. */
  let lastEpoch = stringOrNull(initialEpoch);
  // 外部 signal 的 abort→stop 监听是否处于挂载状态。
  let stopListenerAttached = false;
  // 累计统计：丢弃的块数/字节数与 epoch 变更次数。
  const stats = { droppedBlocks: 0, droppedBytes: 0, epochChanges: 0 };

  /** 解绑外部 signal 上的 stop 监听：stop 之后不再持有外部引用。 */
  function detachStopListener() {
    if (!stopListenerAttached) return;
    signal?.removeEventListener('abort', stop);
    stopListenerAttached = false;
  }

  /** 幂等挂载外部 signal 的 abort→stop 监听；signal 已中止时不挂载。 */
  function attachStopListener() {
    if (!signal || signal.aborted || stopListenerAttached) return;
    signal.addEventListener('abort', stop, { once: true });
    stopListenerAttached = true;
  }

  /**
   * 停止读取器：置停止位、解绑监听并中止当前活动连接。
   * 幂等；可在外部 signal 的 abort 事件中重复触发。
   */
  function stop() {
    stopped = true;
    detachStopListener();
    if (activeController && !activeController.signal.aborted) {
      activeController.abort();
    }
  }

  /**
   * Handle one parsed block. Control frames (no payload) advance the cursor
   * and surface the event name; their ids must move `lastEventId` so
   * reconnects do not re-request the disavowed range (plan §5.4). Malformed
   * blocks are the opposite case: data was present and lost, so the cursor
   * must NOT advance — a reconnect re-requests the range instead of a
   * silent skip.
   */
  /**
   * 处理一个已完整到达的 SSE 块（中文摘要）：解析信封、推进游标、
   * 分发回调。malformed 块只记账不推进游标（重连后重新请求该范围）；
   * 控制帧虽无 payload 也推进游标，防止重连重播已废弃区间。
   */
  const handleBlock = (block) => {
    const envelope = parseBlock(block);
    if (!envelope) return;
    if (envelope.malformed === true) {
      stats.droppedBlocks += 1;
      stats.droppedBytes += block.length;
      return;
    }
    const blockEventId = stringOrNull(envelope.eventId);
    if (blockEventId !== null) {
      lastEventId = blockEventId;
    }
    onEvent?.({
      block,
      envelope,
      payload: envelope.payload ?? null,
      eventId: envelope.eventId ?? null,
      eventName: envelope.eventName ?? null,
      directory: envelope.directory ?? null,
    });
  };

  /**
   * 启动读取主循环（幂等，重复调用返回同一运行 Promise）。
   * 每轮：构造连接 → 校验/回显 epoch → 流式读取并按块分发 →
   * finally 中统一清理（释放 body、丢弃半块、上报断开原因）→
   * 退避后重连，直至 stop 或外部 signal 中止。
   */
  const start = () => {
    if (running) {
      return running;
    }

    attachStopListener();
    stopped = false;
    running = (async () => {
      while (!stopped && !signal?.aborted) {
        const controller = new AbortController();
        activeController = controller;
        // 外部 signal 中止时联动中止当前连接的 controller。
        const abortActive = () => controller.abort();
        signal?.addEventListener('abort', abortActive, { once: true });

        let abortReason = null;
        let stallTimer = null;
        /** 清除当前失速计时器（读到新数据或连接结束时调用）。 */
        const clearStallTimer = () => {
          if (stallTimer) {
            clearTimeout(stallTimer);
            stallTimer = null;
          }
        };
        /** 重置失速计时器：每个读窗口重新解析 stallTimeoutMs；到期以 upstream_stalled 中止。 */
        const resetStallTimer = () => {
          clearStallTimer();
          const currentStallTimeoutMs = resolveTimeoutMs(stallTimeoutMs, DEFAULT_UPSTREAM_STALL_TIMEOUT_MS);
          if (currentStallTimeoutMs <= 0) {
            return;
          }

          stallTimer = setTimeout(() => {
            abortReason = 'upstream_stalled';
            controller.abort();
          }, currentStallTimeoutMs);
        };

        let buffer = '';
        let currentResponse = null;

        try {
          const url = buildUrl();
          const headers = {
            Accept: 'text/event-stream',
            'Cache-Control': 'no-cache',
            Connection: 'keep-alive',
            ...normalizeHeaders(getHeaders()),
          };
          if (lastEventId) {
            headers['Last-Event-ID'] = lastEventId;
          }
          if (lastEpoch) {
            // Boot-identity echo: lets the upstream distinguish a same-boot
            // resume from a stale cross-boot cursor (plan §5.2.1).
            headers[UPSTREAM_EPOCH_HEADER] = lastEpoch;
          }

          const response = await fetchImpl(url.toString(), {
            headers,
            signal: controller.signal,
          });
          currentResponse = response;

          if (!response?.ok || !response.body) {
            onError?.({
              type: 'upstream_unavailable',
              status: response?.status ?? 0,
              response,
            });
            await cancelResponseBody(response);
            await waitForReconnectDelay(reconnectDelayMs, signal);
            continue;
          }

          const upstreamEpoch = response.headers?.get?.(UPSTREAM_EPOCH_HEADER) ?? null;
          if (upstreamEpoch && upstreamEpoch !== lastEpoch) {
            const changed = lastEpoch !== null;
            if (changed) {
              stats.epochChanges += 1;
              // The cursor belongs to the boot that issued it: echoing the
              // NEW epoch with the OLD id would let a numeric collision
              // verdict a wrong suffix `ok` (plan §5.2.1). If this
              // connection dies before the resync frame heals the cursor,
              // the next connect must go in fresh rather than attest a boot
              // it never consumed.
              lastEventId = '';
            }
            lastEpoch = upstreamEpoch;
            onEpochChange?.({ epoch: upstreamEpoch, changed });
          }

          onConnect?.({ response, lastEventId });

          const decoder = new TextDecoder();
          const reader = response.body.getReader();

          /**
           * 从缓冲区按空行切出完整块并逐块处理。超预算块不静默丢弃：
           * 能读到 id: 行则推进游标并合成 resync 控制帧（synthesized: true），
           * 无 id: 行则抛错中止连接；无分隔符且已超预算的尾部半块同样抛错。
           */
          const consumeBuffer = () => {
            let separatorIndex = buffer.indexOf('\n\n');
            while (separatorIndex !== -1 && !stopped && !signal?.aborted) {
              const block = buffer.slice(0, separatorIndex);
              buffer = buffer.slice(separatorIndex + 2);
              if (block.length <= maxBlockBytes) {
                handleBlock(block);
              } else {
                // Over-budget block: never drop silently and never
                // reconnect-loop on a retained replay entry. The `id:` line
                // is readable without a full parse — advancing the cursor
                // past it and surfacing a synthesized resync lets the
                // consumer reconcile the disavowed range explicitly
                // (plan §5.4). An id-less oversize block still aborts.
                stats.droppedBlocks += 1;
                stats.droppedBytes += block.length;
                const idLine = block.split('\n').find((line) => line.startsWith('id:'));
                const droppedId = stringOrNull(idLine?.slice(3).trim());
                if (droppedId === null) {
                  throw new Error(`upstream SSE block exceeded ${maxBlockBytes} bytes`);
                }
                lastEventId = droppedId;
                onEvent?.({
                  block: null,
                  envelope: null,
                  payload: null,
                  eventId: droppedId,
                  eventName: 'omp.stream.resync',
                  directory: null,
                  // Not an upstream frame — consumers must not dedupe it
                  // against a connect-time epoch restart.
                  synthesized: true,
                });
              }
              separatorIndex = buffer.indexOf('\n\n');
            }
            if (buffer.length > maxBlockBytes) {
              // An unfinished block already past the budget can only grow.
              stats.droppedBlocks += 1;
              stats.droppedBytes += buffer.length;
              throw new Error(`upstream SSE block exceeded ${maxBlockBytes} bytes without a separator`);
            }
          };

          resetStallTimer();

          while (!stopped && !signal?.aborted) {
            const { value, done } = await reader.read();
            if (done) {
              break;
            }

            resetStallTimer();
            buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, '\n');
            consumeBuffer();
          }

          if (!stopped && !signal?.aborted && buffer.trim().length > 0 && buffer.length <= maxBlockBytes) {
            handleBlock(buffer.trim());
          }
        } catch (error) {
          if (!stopped && !signal?.aborted && abortReason !== 'upstream_stalled') {
            onError?.({
              type: 'stream_error',
              error,
            });
          }
        } finally {
          clearStallTimer();
          // Release the body reader and drop the parser remainder: a
          // reconnect never inherits half a block (plan §5.4).
          await cancelResponseBody(currentResponse);
          buffer = '';
          signal?.removeEventListener('abort', abortActive);
          if (activeController === controller) {
            activeController = null;
          }
          onDisconnect?.({ reason: abortReason ?? (stopped || signal?.aborted ? 'stopped' : 'closed') });
        }

        if (!stopped && !signal?.aborted) {
          await waitForReconnectDelay(reconnectDelayMs, signal);
        }
      }
    })().finally(() => {
      detachStopListener();
      running = null;
    });

    return running;
  };

  return {
    start,
    stop,
    /** 最近一次确认处理的块 id；空串表示尚无游标。 */
    getLastEventId() {
      return lastEventId;
    },
    /** Last upstream boot identity learned from `x-omp-epoch`, or null. */
    /** 最近一次从 x-omp-epoch 学到的上游 boot 身份；null 表示尚未学到。 */
    getEpoch() {
      return lastEpoch;
    },
    /** 丢弃块/字节与 epoch 变更计数的快照（浅拷贝，调用方可安全持有）。 */
    getStats() {
      return { ...stats };
    },
  };
}
