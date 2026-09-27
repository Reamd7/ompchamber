/**
 * 终端 WebSocket 协议工具：固定升级路径常量、二进制控制帧编解码
 * （首字节 0x01 标记 + JSON 载荷）、ws 消息载荷归一化，
 * 以及 rebind 频控所需的窗口裁剪与阈值判断。
 */
/** 终端 WS 的固定升级路径。 */
export const TERMINAL_WS_PATH = '/api/terminal/ws';
/** 控制帧类型标记：首字节 0x01 表示 JSON 控制帧（区别于其他帧类型）。 */
export const TERMINAL_WS_CONTROL_TAG_JSON = 0x01;
/** 单帧最大载荷 64 KiB，防止滥用。 */
export const TERMINAL_WS_MAX_PAYLOAD_BYTES = 64 * 1024;

/** 从请求 URL（相对或绝对）解析 pathname；非法输入返回空串。 */
export const parseRequestPathname = (requestUrl) => {
  if (typeof requestUrl !== 'string' || requestUrl.length === 0) {
    return '';
  }

  try {
    return new URL(requestUrl, 'http://localhost').pathname;
  } catch {
    return '';
  }
};

/** 判断 pathname 是否为终端 WS 路径。 */
export const isTerminalWsPathname = (pathname) => pathname === TERMINAL_WS_PATH;

/** 把 ws 消息载荷（Buffer/chunk 数组/其他）统一归一化为 Buffer。 */
export const normalizeTerminalWsMessageToBuffer = (rawData) => {
  if (Buffer.isBuffer(rawData)) {
    return rawData;
  }

  if (Array.isArray(rawData)) {
    return Buffer.concat(rawData.map((chunk) => (Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))));
  }

  return Buffer.from(rawData);
};

/** 把 ws 消息载荷统一归一化为 UTF-8 文本。 */
export const normalizeTerminalWsMessageToText = (rawData) => {
  if (typeof rawData === 'string') {
    return rawData;
  }

  return normalizeTerminalWsMessageToBuffer(rawData).toString('utf8');
};

/** 解析二进制控制帧为 JSON 对象；载荷缺失、缺协议标记、长度不足或 JSON 非对象一律返回 null。 */
export const readTerminalWsControlFrame = (rawData) => {
  if (!rawData) {
    return null;
  }

  const buffer = normalizeTerminalWsMessageToBuffer(rawData);
  if (buffer.length < 2 || buffer[0] !== TERMINAL_WS_CONTROL_TAG_JSON) {
    return null;
  }

  try {
    const parsed = JSON.parse(buffer.subarray(1).toString('utf8'));
    if (!parsed || typeof parsed !== 'object') {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
};

/** 把任意对象编码为 0x01 前缀 + JSON 的二进制控制帧。 */
export const createTerminalWsControlFrame = (payload) => {
  const jsonBytes = Buffer.from(JSON.stringify(payload), 'utf8');
  return Buffer.concat([Buffer.from([TERMINAL_WS_CONTROL_TAG_JSON]), jsonBytes]);
};

/** 裁掉超出窗口（now - timestamp >= windowMs）的 rebind 时间戳。 */
export const pruneRebindTimestamps = (timestamps, now, windowMs) =>
  timestamps.filter((timestamp) => now - timestamp < windowMs);

/** 判断窗口内 rebind 次数是否已达上限（>= maxPerWindow 即限流）。 */
export const isRebindRateLimited = (timestamps, maxPerWindow) => timestamps.length >= maxPerWindow;
