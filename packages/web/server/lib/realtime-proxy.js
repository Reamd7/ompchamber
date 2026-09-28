/**
 * 实时通道反向代理。桌面端 UI（origin 受限的 WebView，无法直接跨域访问运行时）
 * 的 SSE 与 WebSocket 请求经本 web server 中转到当前激活的 desktop runtime。
 * 目标 URL 由 ?url= 查询参数传入，但必须同时满足：协议与代理类型匹配、路径在
 * 实时事件白名单内、origin 与 runtime 的 apiBaseUrl 一致，且 runtime 配置了
 * requestHeaders——任一不满足即拒绝，避免沦为任意目标的开放 proxy。SSE 与 WS
 * 两条路径都先过 UI 会话认证与 origin 校验。
 */
import { WebSocket, WebSocketServer } from 'ws';

/** 本地 SSE 代理端点路径。 */
const PROXY_SSE_PATH = '/api/ompchamber/realtime-proxy/sse';
/** 本地 WebSocket 代理端点路径。 */
const PROXY_WS_PATH = '/api/ompchamber/realtime-proxy/ws';

/** SSE 目标路径白名单：仅事件流相关端点允许被代理。 */
const isAllowedSsePath = (pathname) => {
  return pathname === '/api/event'
    || pathname === '/api/global/event'
    || pathname === '/api/ompchamber/events'
    || pathname === '/api/notifications/stream';
};

/** WebSocket 目标路径白名单：仅事件与终端 WS 端点允许被代理。 */
const isAllowedWebSocketPath = (pathname) => {
  return pathname === '/api/event/ws'
    || pathname === '/api/global/event/ws'
    || pathname === '/api/terminal/ws';
};

/** 去除 base URL 的首尾空白与末尾斜杠；非字符串返回空串。 */
const normalizeBaseUrl = (value) => {
  if (typeof value !== 'string') return '';
  return value.trim().replace(/\/+$/, '');
};

/**
 * 清洗要转发给 runtime 的请求头：丢弃空值、名称含 CR/LF/冒号（头部注入风险）
 * 的项以及 authorization（认证凭据由 runtime 配置自行提供），其余 trim 后保留。
 */
const sanitizeHeaders = (headers) => {
  if (!headers || typeof headers !== 'object') return {};
  const next = {};
  for (const [rawName, rawValue] of Object.entries(headers)) {
    const name = typeof rawName === 'string' ? rawName.trim() : '';
    const value = typeof rawValue === 'string' ? rawValue.trim() : '';
    if (!name || !value || /[\r\n:]/.test(name) || /[\r\n]/.test(value)) continue;
    if (name.toLowerCase() === 'authorization') continue;
    next[name] = value;
  }
  return next;
};

/** 判断清洗后的头对象是否非空。 */
const hasHeaders = (headers) => Object.keys(headers).length > 0;

/**
 * 从请求中解析 ?url= 目标参数：优先 express 的 req.query.url，缺失时再从原始
 * req.url（WebSocket upgrade 场景）解析；参数缺失或不是合法 URL 返回 null。
 */
const getTargetParam = (req) => {
  let raw = typeof req.query?.url === 'string' ? req.query.url : '';
  if (!raw) {
    try {
      raw = new URL(req.url || '/', 'http://127.0.0.1').searchParams.get('url') || '';
    } catch {
      raw = '';
    }
  }
  if (!raw) return null;
  try {
    return new URL(raw);
  } catch {
    return null;
  }
};

/**
 * 目标与 runtime apiBaseUrl 的 origin 是否一致：比较前把 ws/wss 规整为
 * http/https；解析失败按不匹配处理。
 */
const urlsMatchRuntime = (target, apiBaseUrl) => {
  const base = normalizeBaseUrl(apiBaseUrl);
  if (!base) return false;
  try {
    const baseUrl = new URL(base);
    const targetForCompare = new URL(target.toString());
    if (targetForCompare.protocol === 'ws:') targetForCompare.protocol = 'http:';
    if (targetForCompare.protocol === 'wss:') targetForCompare.protocol = 'https:';
    return targetForCompare.origin === baseUrl.origin;
  } catch {
    return false;
  }
};

/** 目标协议是否与代理类型匹配：ws 代理要求 ws/wss，sse 代理要求 http/https。 */
const protocolMatchesProxyType = (target, type) => {
  if (type === 'ws') return target.protocol === 'ws:' || target.protocol === 'wss:';
  return target.protocol === 'http:' || target.protocol === 'https:';
};

/** 目标路径是否落在对应代理类型（ws/sse）的白名单内。 */
const pathMatchesProxyType = (target, type) => {
  return type === 'ws' ? isAllowedWebSocketPath(target.pathname) : isAllowedSsePath(target.pathname);
};

/**
 * 综合校验并解析代理目标：runtime 配置（apiBaseUrl 与 requestHeaders）齐备、
 * ?url= 可解析、协议与路径匹配类型、origin 与 runtime 一致，全部通过才返回
 * { target, requestHeaders }；否则返回 null，调用方统一以 404 拒绝。
 */
const resolveProxyTarget = (req, getDesktopRuntimeConfig, type) => {
  const config = typeof getDesktopRuntimeConfig === 'function' ? getDesktopRuntimeConfig() : null;
  const requestHeaders = sanitizeHeaders(config?.requestHeaders);
  const apiBaseUrl = normalizeBaseUrl(config?.apiBaseUrl);
  const target = getTargetParam(req);
  if (!target || !apiBaseUrl || !hasHeaders(requestHeaders)) return null;
  if (!protocolMatchesProxyType(target, type)) return null;
  if (!pathMatchesProxyType(target, type)) return null;
  if (!urlsMatchRuntime(target, apiBaseUrl)) return null;
  return { target, requestHeaders };
};

/** 从 headers 中取单一字符串头值：数组取第一个非空项，统一 trim，缺失返回空串。 */
const safeHeader = (headers, name) => {
  const value = headers?.[name.toLowerCase()];
  if (Array.isArray(value)) return value.find((item) => typeof item === 'string' && item.trim()) || '';
  return typeof value === 'string' ? value.trim() : '';
};

/**
 * 组装转发给 runtime 的 SSE 请求头：透传 Accept 与 Last-Event-ID（断线续传
 * 必需），再叠加 runtime 配置的 requestHeaders（同名时配置优先）。
 */
const buildSseRequestHeaders = (req, requestHeaders) => {
  const headers = {};
  const accept = safeHeader(req.headers, 'accept');
  const lastEventId = safeHeader(req.headers, 'last-event-id');
  if (accept) headers.Accept = accept;
  if (lastEventId) headers['Last-Event-ID'] = lastEventId;
  return { ...headers, ...requestHeaders };
};

/** 对未通过校验的 WS upgrade 请求：手写 HTTP 错误响应行并立即销毁 socket。 */
const rejectWebSocketUpgrade = (socket, statusCode, message) => {
  socket.write(`HTTP/1.1 ${statusCode} ${message}\r\nConnection: close\r\n\r\n`);
  socket.destroy();
};

/** 构造指向本地 SSE 代理端点的 URL，目标 URL 编码后放入 ?url= 查询参数。 */
export const buildRealtimeProxySseUrl = (localOrigin, targetUrl) => {
  const url = new URL(PROXY_SSE_PATH, localOrigin);
  url.searchParams.set('url', targetUrl);
  return url.toString();
};

/** 构造指向本地 WS 代理端点的 URL：目标放入 ?url=，协议按本地 origin 转为 wss/ws。 */
export const buildRealtimeProxyWsUrl = (localOrigin, targetUrl) => {
  const url = new URL(PROXY_WS_PATH, localOrigin);
  url.searchParams.set('url', targetUrl);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
};

/**
 * 把实时代理挂载到 express app 与 http server 上，返回 { stop }。
 * 注册两条通道：
 * - SSE GET 路由：会话认证（失败 401）→ origin 校验（失败 403）→ 目标解析
 *   （失败 404）→ fetch 上游并流式回写；客户端断开即 abort 上游请求，上游
 *   非 2xx 时透传状态码。
 * - WS upgrade 处理器：同样先认证与 origin 校验（失败时向原始 socket 写
 *   401/403 后销毁），通过后交给 WebSocketServer；连接建立后与上游双向转发，
 *   上游握手完成前客户端消息先入 pending 队列，任一侧关闭/出错按对应 close
 *   code 收尾。
 * 缺少 app/server 或 getDesktopRuntimeConfig 时返回无操作的 stop。
 * stop() 解绑 upgrade 监听并关闭 WS server。
 */
export const attachRealtimeProxy = ({ app, server, getDesktopRuntimeConfig, getUiAuthController, isRequestOriginAllowed }) => {
  if (!app || !server || typeof getDesktopRuntimeConfig !== 'function') {
    return { stop: () => {} };
  }

  /** 请求 origin 是否被放行：委托 isRequestOriginAllowed；未提供该回调或抛错时一律拒绝。 */
  const originAllowed = async (req) => {
    if (typeof isRequestOriginAllowed !== 'function') return false;
    try {
      return await isRequestOriginAllowed(req);
    } catch {
      return false;
    }
  };

  /** UI 会话认证：经 uiAuthController.ensureSessionToken 换取会话 token；无控制器或拿不到 token 返回 false。 */
  const ensureAuthenticated = async (req, res) => {
    const controller = typeof getUiAuthController === 'function' ? getUiAuthController() : null;
    if (typeof controller?.ensureSessionToken !== 'function') return false;
    const response = res || { setHeader: () => {} };
    const token = await controller.ensureSessionToken(req, response);
    return Boolean(token);
  };

  app.get(PROXY_SSE_PATH, async (req, res) => {
    if (!await ensureAuthenticated(req, res)) {
      res.status(401).json({ error: 'UI authentication required' });
      return;
    }
    if (!await originAllowed(req)) {
      res.status(403).json({ error: 'Realtime proxy origin is not allowed' });
      return;
    }
    const resolved = resolveProxyTarget(req, getDesktopRuntimeConfig, 'sse');
    if (!resolved) {
      res.status(404).json({ error: 'Realtime proxy is unavailable' });
      return;
    }

    const abort = new AbortController();
    req.on('close', () => abort.abort());
    try {
      const response = await fetch(resolved.target.toString(), {
        headers: buildSseRequestHeaders(req, resolved.requestHeaders),
        signal: abort.signal,
      });
      if (!response.ok || !response.body) {
        res.status(response.status || 502).end();
        return;
      }

      res.status(response.status);
      res.setHeader('Content-Type', response.headers.get('content-type') || 'text/event-stream');
      res.setHeader('Cache-Control', response.headers.get('cache-control') || 'no-cache');
      res.setHeader('Connection', 'keep-alive');

      for await (const chunk of response.body) {
        if (abort.signal.aborted) break;
        res.write(chunk);
      }
      res.end();
    } catch (error) {
      if (!abort.signal.aborted && !res.headersSent) {
        res.status(502).json({ error: error instanceof Error ? error.message : 'Realtime proxy failed' });
      } else if (!res.destroyed) {
        res.end();
      }
    }
  });

  const wsServer = new WebSocketServer({ noServer: true });

  wsServer.on('connection', (client, request) => {
    const resolved = resolveProxyTarget(request, getDesktopRuntimeConfig, 'ws');
    if (!resolved) {
      client.close(1008, 'Realtime proxy is unavailable');
      return;
    }

    const upstream = new WebSocket(resolved.target.toString(), {
      headers: resolved.requestHeaders,
    });
    const pending = [];

    /** 上游连接打开后，把 pending 队列中缓存的客户端消息按序发出。 */
    const flush = () => {
      while (pending.length > 0 && upstream.readyState === WebSocket.OPEN) {
        const [data, isBinary] = pending.shift();
        upstream.send(data, { binary: isBinary });
      }
    };

    client.on('message', (data, isBinary) => {
      if (upstream.readyState === WebSocket.OPEN) {
        upstream.send(data, { binary: isBinary });
        return;
      }
      if (upstream.readyState === WebSocket.CONNECTING) {
        pending.push([data, isBinary]);
      }
    });
    upstream.on('open', flush);
    upstream.on('message', (data, isBinary) => {
      if (client.readyState === WebSocket.OPEN) {
        client.send(data, { binary: isBinary });
      }
    });
    upstream.on('close', (code, reason) => {
      if (client.readyState === WebSocket.OPEN || client.readyState === WebSocket.CONNECTING) {
        client.close(code || 1000, reason);
      }
    });
    upstream.on('error', () => {
      if (client.readyState === WebSocket.OPEN || client.readyState === WebSocket.CONNECTING) {
        client.close(1011, 'Realtime proxy upstream error');
      }
    });
    client.on('close', () => {
      if (upstream.readyState === WebSocket.OPEN || upstream.readyState === WebSocket.CONNECTING) {
        upstream.close();
      }
    });
  });

  /** server 的 upgrade 监听：仅处理代理 WS 路径；认证与 origin 校验通过后交给 wsServer 完成握手。 */
  const upgradeHandler = (req, socket, head) => {
    // 解析 upgrade 请求的 pathname（解析失败按空串处理）。
    const pathname = (() => {
      try { return new URL(req.url || '/', 'http://127.0.0.1').pathname; } catch { return ''; }
    })();
    if (pathname !== PROXY_WS_PATH) return;
    void ensureAuthenticated(req, null).then((authenticated) => {
      if (!authenticated) {
        rejectWebSocketUpgrade(socket, 401, 'Unauthorized');
        return;
      }
      void originAllowed(req).then((allowed) => {
        if (!allowed) {
          rejectWebSocketUpgrade(socket, 403, 'Forbidden');
          return;
        }
        wsServer.handleUpgrade(req, socket, head, (ws) => {
          wsServer.emit('connection', ws, req);
        });
      }).catch(() => {
        rejectWebSocketUpgrade(socket, 403, 'Forbidden');
      });
    }).catch(() => {
      rejectWebSocketUpgrade(socket, 401, 'Unauthorized');
    });
  };

  server.on('upgrade', upgradeHandler);
  return {
    stop: () => {
      server.off('upgrade', upgradeHandler);
      wsServer.close();
    },
  };
};
