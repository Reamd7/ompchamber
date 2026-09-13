export const MESSAGE_STREAM_GLOBAL_WS_PATH = '/api/global/event/ws';
export const MESSAGE_STREAM_DIRECTORY_WS_PATH = '/api/event/ws';
export const MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS = 15 * 1000;
// Per-client pending outbound WS buffer, not a payload or stream-size limit.
// Healthy clients stay near 0; this only trips when a client is far behind.
// Raised from 4 MB → 16 MB to tolerate bursts during long agent sessions
// (e.g. ultrawork / multi-tool loops) where the browser briefly falls behind.
export const MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES = 16 * 1024 * 1024;

// Threshold at which we emit a backpressure warning frame so the client can
// proactively start shedding low-priority updates before the hard disconnect.
export const MESSAGE_STREAM_WS_BACKPRESSURE_WARN_BYTES = 12 * 1024 * 1024;

/**
 * Parse one SSE block into `{ eventId, eventName, directory, payload }`.
 * Data-less blocks carrying an `event:` name or `id:` are control frames
 * (payload null) — dropping them would hide resync/restart controls
 * (docs/plan.md §5.4). Blocks with none of the three are comments → null.
 */
export function parseSseEventEnvelope(block) {
  if (!block || typeof block !== 'string') {
    return null;
  }

  const lines = block.split('\n');
  let eventId = null;
  let eventName = null;
  const dataLines = [];
  for (const line of lines) {
    if (line.startsWith('id:')) {
      eventId = line.slice(3).trim() || null;
    } else if (line.startsWith('event:')) {
      eventName = line.slice(6).trim() || null;
    } else if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).replace(/^\s/, ''));
    }
  }

  const payloadText = dataLines.join('\n').trim();
  let parsed;
  if (payloadText) {
    try {
      parsed = JSON.parse(payloadText);
    } catch {
      parsed = undefined;
    }
  }
  if (parsed === undefined) {
    if (eventId === null && eventName === null) {
      return null;
    }
    return { eventId, eventName, directory: null, payload: null };
  }

  if (
    parsed &&
    typeof parsed === 'object' &&
    typeof parsed.payload === 'object' &&
    parsed.payload !== null
  ) {
    return {
      eventId,
      eventName,
      directory: typeof parsed.directory === 'string' && parsed.directory.length > 0 ? parsed.directory : null,
      payload: parsed.payload,
    };
  }

  const directory =
    typeof parsed?.directory === 'string' && parsed.directory.length > 0
      ? parsed.directory
      : typeof parsed?.properties?.directory === 'string' && parsed.properties.directory.length > 0
        ? parsed.properties.directory
        : typeof parsed?.properties?.info?.directory === 'string' && parsed.properties.info.directory.length > 0
          ? parsed.properties.info.directory
          : null;

  return {
    eventId,
    eventName,
    directory,
    payload: parsed,
  };
}

export function sendMessageStreamWsFrame(socket, payload) {
  if (!socket || socket.readyState !== 1) {
    return false;
  }

  const buffered = typeof socket.bufferedAmount === 'number' ? socket.bufferedAmount : 0;

  if (buffered > MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES) {
    try {
      socket.close(1013, 'Message stream client is too slow');
    } catch {
    }
    return false;
  }

  try {
    socket.send(JSON.stringify(payload));
    const bufferedAfter = typeof socket.bufferedAmount === 'number' ? socket.bufferedAmount : 0;
    if (bufferedAfter > MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES) {
      try {
        socket.close(1013, 'Message stream client is too slow');
      } catch {
      }
      return false;
    }

    // Emit a one-shot backpressure warning when the buffer is building up.
    // The flag prevents sending repeated warnings that would themselves
    // increase the buffer.  It resets once the buffer drains below the
    // threshold.
    if (bufferedAfter > MESSAGE_STREAM_WS_BACKPRESSURE_WARN_BYTES) {
      if (!socket._ocBackpressureWarned) {
        socket._ocBackpressureWarned = true;
        try {
          socket.send(JSON.stringify({
            type: 'backpressure',
            bufferedBytes: bufferedAfter,
            maxBytes: MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES,
          }));
        } catch {
          // Best-effort warning — ignore send failures.
        }
      }
    } else if (socket._ocBackpressureWarned) {
      socket._ocBackpressureWarned = false;
    }

    return true;
  } catch {
    return false;
  }
}

/** Transport-level resync control frame (docs/plan.md §5.2): cursor + epoch. */
export function sendWsResyncFrame(socket, { eventId, epoch }) {
  return sendMessageStreamWsFrame(socket, {
    type: 'resync',
    ...(typeof eventId === 'string' && eventId.length > 0 ? { eventId } : {}),
    ...(typeof epoch === 'string' && epoch.length > 0 ? { epoch } : {}),
  });
}

export function sendMessageStreamWsEvent(socket, payload, options = {}) {
  return sendMessageStreamWsFrame(socket, {
    type: 'event',
    payload,
    ...(typeof options.eventId === 'string' && options.eventId.length > 0 ? { eventId: options.eventId } : {}),
    ...(typeof options.directory === 'string' && options.directory.length > 0 ? { directory: options.directory } : {}),
  });
}
