import { sendMessageStreamWsEvent, sendMessageStreamWsFrame, sendWsResyncFrame } from './protocol.js';
import { createUpstreamSseReader } from './upstream-reader.js';

/** Non-empty-string arm check for untrusted boundary values (no `typeof`). */
const stringOrNull = (value) =>
  Object.prototype.toString.call(value) === '[object String]' && value.length > 0 ? value : null;

function shouldTriggerUpstreamHealthCheck(upstream) {
  if (!upstream) {
    return true;
  }

  if (!upstream.body) {
    return upstream.ok || upstream.status >= 500;
  }

  return upstream.status >= 500;
}

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
  const controller = new AbortController();
  let upstreamConnected = false;
  let streamReady = false;
  let reader = null;

  const cleanup = () => {
    if (!controller.signal.aborted) {
      controller.abort();
    }
    reader?.stop();
    wsClients.delete(socket);
  };

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
    if (!upstreamConnected) {
      return;
    }

    sendMessageStreamWsEvent(socket, { type: 'ompchamber:heartbeat', timestamp: Date.now() }, { directory: 'global' });
  }, heartbeatIntervalMs);

  socket.on('close', () => {
    clearInterval(pingInterval);
    clearInterval(heartbeatInterval);
    upstreamConnected = false;
    cleanup();
  });

  socket.on('error', () => {
    void 0;
  });

  const run = async () => {
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
          sendWsResyncFrame(socket, {
            eventId: stringOrNull(eventId) ?? undefined,
          });
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
            sendMessageStreamWsFrame(socket, {
              type: 'ready',
              scope: 'directory',
            });
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
