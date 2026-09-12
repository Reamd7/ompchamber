import { parseSseEventEnvelope } from './protocol.js';

export const DEFAULT_UPSTREAM_STALL_TIMEOUT_MS = 20_000;
export const UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS = DEFAULT_UPSTREAM_STALL_TIMEOUT_MS * 3;
export const DEFAULT_UPSTREAM_RECONNECT_DELAY_MS = 250;
// Parser guard (docs/plan.md §5.3.1): a block that exceeds this is not
// dropped silently — the connection aborts and reconnects so the host's
// hole→gap→resync contract disavows the range explicitly. The cap must sit
// ABOVE the largest event the host's replay ring can retain (the wire bus
// keeps events up to a 2 MiB serialized estimate whose JSON form can be
// roughly double), or an oversized-but-retained event would reconnect-loop
// this reader forever.
export const DEFAULT_UPSTREAM_MAX_BLOCK_BYTES = 8 * 1024 * 1024;
/** Header the omp host echoes for boot-identity resume (plan §5.2.1). */
export const UPSTREAM_EPOCH_HEADER = 'x-omp-epoch';

/** Non-empty-string arm check for untrusted boundary values (no `typeof`). */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

function resolveTimeoutMs(value, fallback) {
  const resolved = value instanceof Function ? value() : value;
  return Number.isFinite(resolved) ? resolved : fallback;
}

function waitForReconnectDelay(ms, signal) {
  if (signal?.aborted) {
    return Promise.resolve();
  }

  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener('abort', onAbort);
      resolve();
    };
    const timeout = setTimeout(finish, Math.max(0, ms));
    const onAbort = () => {
      clearTimeout(timeout);
      finish();
    };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

function normalizeHeaders(headers) {
  if (!(headers instanceof Object)) {
    return {};
  }

  return { ...headers };
}

async function cancelResponseBody(response) {
  if (response?.body && response.body.cancel instanceof Function) {
    await response.body.cancel().catch(() => {});
  }
}

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
  let running = null;
  let stopped = false;
  let activeController = null;
  let lastEventId = stringOrNull(initialLastEventId) ?? '';
  /** Last upstream boot identity; echoed on reconnects to detect restarts.
   *  `initialEpoch` lets a proxying reader (directory WS bridge) forward the
   *  downstream client's learned epoch so the first connect can still prove
   *  an `ok` same-boot resume. */
  let lastEpoch = stringOrNull(initialEpoch);
  let stopListenerAttached = false;
  const stats = { droppedBlocks: 0, droppedBytes: 0, epochChanges: 0 };

  function detachStopListener() {
    if (!stopListenerAttached) return;
    signal?.removeEventListener('abort', stop);
    stopListenerAttached = false;
  }

  function attachStopListener() {
    if (!signal || signal.aborted || stopListenerAttached) return;
    signal.addEventListener('abort', stop, { once: true });
    stopListenerAttached = true;
  }

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
   * reconnects do not re-request the disavowed range (plan §5.4).
   */
  const handleBlock = (block) => {
    const envelope = parseBlock(block);
    if (!envelope) return;
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
        const abortActive = () => controller.abort();
        signal?.addEventListener('abort', abortActive, { once: true });

        let abortReason = null;
        let stallTimer = null;
        const clearStallTimer = () => {
          if (stallTimer) {
            clearTimeout(stallTimer);
            stallTimer = null;
          }
        };
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
            }
            lastEpoch = upstreamEpoch;
            onEpochChange?.({ epoch: upstreamEpoch, changed });
          }

          onConnect?.({ response, lastEventId });

          const decoder = new TextDecoder();
          const reader = response.body.getReader();

          const consumeBuffer = () => {
            let separatorIndex = buffer.indexOf('\n\n');
            while (separatorIndex !== -1 && !stopped && !signal?.aborted) {
              const block = buffer.slice(0, separatorIndex);
              buffer = buffer.slice(separatorIndex + 2);
              if (block.length <= maxBlockBytes) {
                handleBlock(block);
              } else {
                // Never drop silently: abort → reconnect → the cursor we
                // did NOT advance makes the host verdict this range
                // hole→gap→resync instead of a silent loss.
                stats.droppedBlocks += 1;
                stats.droppedBytes += block.length;
                throw new Error(`upstream SSE block exceeded ${maxBlockBytes} bytes`);
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
    getLastEventId() {
      return lastEventId;
    },
    getStats() {
      return { ...stats };
    },
  };
}
