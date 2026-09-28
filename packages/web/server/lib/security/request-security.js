/**
 * 请求安全运行时：为 HTTP 与 WebSocket 提供三条安全原语——从 Cookie 提取
 * UI 会话 token、以携带正确状态码的完整 HTTP 响应拒绝 WebSocket 升级、
 * 以及基于 Origin / Host 的同源校验（覆盖反向代理与打包客户端场景）。
 */
export const createRequestSecurityRuntime = (deps) => {
  const { readSettingsFromDiskMigrated } = deps;
  // Origins of packaged (non-browser) clients whose WebView origin never
  // matches the server host: the desktop shell, the iOS Capacitor WebView
  // (capacitor://localhost), and the Android Capacitor WebView, which uses
  // androidScheme 'https' and therefore reports 'https://localhost'. Missing
  // the Android origin 403'd every WebSocket upgrade from the Android app
  // (message stream, terminal, dictation) while SSE kept working.
  // 中文补充：打包（非浏览器）客户端的 Origin 白名单——桌面壳与 iOS /
  // Android 的 Capacitor WebView，它们的 Origin 永远不会等于服务器 host。
  const packagedClientOrigins = new Set([
    'ompchamber-ui://app',
    'capacitor://localhost',
    'https://localhost',
  ]);

  /**
   * 从请求 Cookie 头解析 `oc_ui_session` 的值（等号后的内容按 URL 解码，
   * 解码失败退回原始值）。无 Cookie 头或无该 cookie 时返回 null，绝不抛出。
   */
  const getUiSessionTokenFromRequest = (req) => {
    const cookieHeader = req?.headers?.cookie;
    if (!cookieHeader || typeof cookieHeader !== 'string') {
      return null;
    }
    const segments = cookieHeader.split(';');
    for (const segment of segments) {
      const [rawName, ...rest] = segment.split('=');
      const name = rawName?.trim();
      if (!name) continue;
      if (name !== 'oc_ui_session') continue;
      const value = rest.join('=').trim();
      try {
        return decodeURIComponent(value || '');
      } catch {
        return value || null;
      }
    }
    return null;
  };

  /**
   * 以一段完整的 HTTP/1.1 错误响应拒绝 WebSocket 升级请求：先 socket.end()
   * 冲刷响应字节再销毁连接（1 秒兜底销毁 + unref），避免浏览器只看到裸
   * “连接失败”。已销毁的 socket 直接忽略；reason 缺省为 'Bad Request'。
   */
  const rejectWebSocketUpgrade = (socket, statusCode, reason) => {
    if (!socket || socket.destroyed) {
      return;
    }

    const message = typeof reason === 'string' && reason.trim().length > 0 ? reason.trim() : 'Bad Request';
    const body = Buffer.from(message, 'utf8');
    const statusText = {
      400: 'Bad Request',
      401: 'Unauthorized',
      403: 'Forbidden',
      404: 'Not Found',
      500: 'Internal Server Error',
    }[statusCode] || 'Bad Request';

    // Write+destroy races the socket's write queue and usually discards the
    // response bytes, leaving browsers a bare "WebSocket connection failed"
    // with no status. end() flushes first; destroy is the backstop for a
    // client that never reads.
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(destroyTimer);
      try {
        socket.destroy();
      } catch {
      }
    };
    const destroyTimer = setTimeout(finish, 1000);
    destroyTimer.unref?.();

    try {
      socket.once('error', finish);
      socket.end(
        `HTTP/1.1 ${statusCode} ${statusText}\r\n` +
        'Connection: close\r\n' +
        'Content-Type: text/plain; charset=utf-8\r\n' +
        `Content-Length: ${body.length}\r\n\r\n` +
        body,
        finish
      );
    } catch {
      finish();
    }
  };

  /**
   * 收集请求可能对应的合法 origin / host 候选：Host 头（尊重
   * x-forwarded-host / x-forwarded-proto 的首个值，缺省按 socket 是否
   * 加密推断协议）、localhost 与 127.0.0.1 / [::1] 之间的互推，以及设置
   * 里的 publicOrigin。设置读取失败静默忽略。
   */
  const getRequestOriginCandidates = async (req) => {
    const origins = new Set();
    const hosts = new Set();
    const forwardedProto = typeof req.headers['x-forwarded-proto'] === 'string'
      ? req.headers['x-forwarded-proto'].split(',')[0].trim().toLowerCase()
      : '';
    const protocol = forwardedProto || (req.socket?.encrypted ? 'https' : 'http');

    const forwardedHost = typeof req.headers['x-forwarded-host'] === 'string'
      ? req.headers['x-forwarded-host'].split(',')[0].trim()
      : '';
    const host = forwardedHost || (typeof req.headers.host === 'string' ? req.headers.host.trim() : '');

    if (host) {
      hosts.add(host.toLowerCase());
      origins.add(`${protocol}://${host}`);
      const [hostname, port] = host.split(':');
      const normalizedHost = typeof hostname === 'string' ? hostname.toLowerCase() : '';
      const portSuffix = typeof port === 'string' && port.length > 0 ? `:${port}` : '';
      if (normalizedHost === 'localhost') {
        origins.add(`${protocol}://127.0.0.1${portSuffix}`);
        origins.add(`${protocol}://[::1]${portSuffix}`);
      } else if (normalizedHost === '127.0.0.1' || normalizedHost === '[::1]') {
        origins.add(`${protocol}://localhost${portSuffix}`);
      }
    }

    try {
      const settings = await readSettingsFromDiskMigrated();
      if (typeof settings?.publicOrigin === 'string' && settings.publicOrigin.trim().length > 0) {
        origins.add(new URL(settings.publicOrigin.trim()).origin);
      }
    } catch {
    }

    return { origins, hosts };
  };

  /**
   * 判断请求 Origin 是否可信：打包客户端 Origin 白名单直接放行；否则先
   * 与候选 origins 精确比对，再退化为仅比对 host（TLS 在云边缘终止、回源
   * 为 HTTP 的拓扑下协议不可信但外部 host 仍权威）。无 Origin 头或 Origin
   * 无法解析为 URL 时返回 false。
   */
  const isRequestOriginAllowed = async (req) => {
    const originHeader = typeof req.headers.origin === 'string' ? req.headers.origin.trim() : '';
    if (!originHeader) {
      return false;
    }

    if (packagedClientOrigins.has(originHeader)) {
      return true;
    }

    let origin;
    try {
      origin = new URL(originHeader);
    } catch {
      return false;
    }

    const candidates = await getRequestOriginCandidates(req);
    if (candidates.origins.has(origin.origin)) return true;

    // TLS commonly ends at a cloud edge before an HTTP hop to OpenChamber.
    // In that setup the browser's Origin is https while a generic reverse
    // proxy reports the upstream request as http. The external host remains
    // authoritative, so compare it directly instead of requiring the proxy to
    // preserve the browser-facing protocol.
    return candidates.hosts.has(origin.host.toLowerCase());
  };

  // 运行时对外 API：Cookie 会话 token 提取、WebSocket 升级拒绝、Origin 放行判定。
  return {
    getUiSessionTokenFromRequest,
    rejectWebSocketUpgrade,
    isRequestOriginAllowed,
  };
};
