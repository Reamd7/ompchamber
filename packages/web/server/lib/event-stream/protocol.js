/**
 * 消息流（message stream）传输协议层：定义 WebSocket 端点路径、帧发送
 * 与 SSE 块解析，是 global hub、global/directory WS bridge 与
 * upstream reader 共用的基础模块。
 *
 * 上游 SSE 块经 parseSseEventEnvelope 归一化为事件信封（envelope）；
 * 下游帧统一经 sendMessageStreamWsFrame 序列化发送，并在客户端
 * 积压超限（背压）时主动断开，保护服务端内存。
 */

/** 全局消息流 WebSocket 端点路径（跨目录共享的单条聚合流）。 */
export const MESSAGE_STREAM_GLOBAL_WS_PATH = '/api/global/event/ws';
/** 目录级消息流 WebSocket 端点路径（每个客户端独享一条上游流）。 */
export const MESSAGE_STREAM_DIRECTORY_WS_PATH = '/api/event/ws';
/** WS 心跳间隔（毫秒）：协议层 ping 与 ompchamber:heartbeat 事件共用该周期。 */
export const MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS = 15 * 1000;
// Per-client pending outbound WS buffer, not a payload or stream-size limit.
// Healthy clients stay near 0; this only trips when a client is far behind.
// Raised from 4 MB → 16 MB to tolerate bursts during long agent sessions
// (e.g. ultrawork / multi-tool loops) where the browser briefly falls behind.
/** 单客户端待发送缓冲的硬上限（字节）：超过即以 1013 断开慢客户端。 */
export const MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES = 16 * 1024 * 1024;

// Threshold at which we emit a backpressure warning frame so the client can
// proactively start shedding low-priority updates before the hard disconnect.
/** 背压预警阈值（字节）：超过后发送一次性 warning 帧，提示客户端主动降载。 */
export const MESSAGE_STREAM_WS_BACKPRESSURE_WARN_BYTES = 12 * 1024 * 1024;

/**
 * Parse one SSE block into `{ eventId, eventName, directory, payload }`.
 * Data-less blocks carrying an `event:` name or `id:` are control frames
 * (payload null) — dropping them would hide resync/restart controls
 * (docs/plan.md §5.4). Blocks with none of the three are comments → null.
 * A block with `data:` that fails JSON.parse is NOT a control frame: it
 * returns `malformed: true` so the caller can refuse to advance the cursor
 * instead of silently skipping a lost business event.
 */
/**
 * 解析单个 SSE 块（已按空行切分、不含分隔符）为事件信封。
 * 返回 null 表示纯注释块（无任何可用信息）；控制帧与 malformed
 * 块的判定规则见上方英文说明（plan §5.4）。
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
    const control = { eventId, eventName, directory: null, payload: null };
    // Non-empty but unparseable data marks a malformed business block, not
    // a control frame; absent data stays a clean control.
    if (payloadText.length > 0) control.malformed = true;
    return control;
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

/**
 * 序列化并发送一帧 WebSocket 消息（JSON.stringify）。
 *
 * 返回 boolean 表示是否成功入队：socket 缺失/未打开或 send 抛错为
 * false；发送前后 bufferedAmount 任一超过 MESSAGE_STREAM_WS_MAX_BUFFERED_BYTES
 * 即以 1013 关闭连接并返回 false（宁可断开慢客户端也不无限堆积）。
 * 超过预警阈值时补发一次 backpressure 帧（socket 上的
 * _ocBackpressureWarned 标志防重复；缓冲排空后复位，可再次告警）。
 */
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
/** 构造并发送 resync 控制帧：eventId/epoch 仅在为非空字符串时写入帧字段。 */
export function sendWsResyncFrame(socket, { eventId, epoch }) {
  return sendMessageStreamWsFrame(socket, {
    type: 'resync',
    ...(typeof eventId === 'string' && eventId.length > 0 ? { eventId } : {}),
    ...(typeof epoch === 'string' && epoch.length > 0 ? { epoch } : {}),
  });
}

/**
 * 发送业务事件帧：{ type: 'event', payload, eventId?, directory? }。
 * eventId（续传游标）与 directory（路由）为可选元数据，仅在为
 * 非空字符串时携带；返回值与背压断开语义同 sendMessageStreamWsFrame。
 */
export function sendMessageStreamWsEvent(socket, payload, options = {}) {
  return sendMessageStreamWsFrame(socket, {
    type: 'event',
    payload,
    ...(typeof options.eventId === 'string' && options.eventId.length > 0 ? { eventId: options.eventId } : {}),
    ...(typeof options.directory === 'string' && options.directory.length > 0 ? { directory: options.directory } : {}),
  });
}
