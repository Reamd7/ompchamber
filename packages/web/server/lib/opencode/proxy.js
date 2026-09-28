/**
 * OpenCode API 反向代理（express 中间件注册模块）。
 *
 * 把前端发往 /api/* 的请求转发给托管 OpenCode 服务（或外部 OPENCODE_HOST），
 * 核心能力：
 * - 连接池化的 http/https agent（按 scheme 记忆化，避免每请求新建连接耗尽
 *   主机临时端口）；
 * - SSE 事件流的手动转发（心跳注入、上游停滞检测、写背压、事件边界跟踪）；
 * - 会话列表响应脱敏（字段白名单过滤，剔除 summary.diffs 等大体量内容）；
 * - 按路由类别区分的超时策略（普通请求 / 交互式 OAuth / 会话 turn）；
 * - 就绪门控：OpenCode 启动/重启期间挂起请求轮询就绪，而不是立刻 503。
 */
import http from 'node:http';
import https from 'node:https';

import { createProxyMiddleware } from 'http-proxy-middleware';

import {
  applyForwardProxyResponseHeaders,
  collectForwardProxyHeaders,
  shouldForwardProxyResponseHeader,
} from '../../proxy-headers.js';
import { createRealpathCache } from '../path-realpath-cache.js';
import { DEFAULT_UPSTREAM_STALL_TIMEOUT_MS } from '../event-stream/upstream-reader.js';
import { recordStartupPerformance } from './startup-performance.js';

/** SSE 心跳注释事件的默认发送间隔（毫秒），可经 deps.SSE_HEARTBEAT_INTERVAL_MS 覆盖。 */
const DEFAULT_SSE_HEARTBEAT_INTERVAL_MS = 20_000;

/** keep-alive agent 的 TCP keep-alive 探测延迟（毫秒）。 */
const OPENCODE_AGENT_KEEP_ALIVE_MS = 30_000;
/** 空闲 socket 池上限：取 Node 自身默认值 256；调低会在并发下驱逐池内 socket，重新引入本 agent 要消除的连接churn。 */
// Node's own default. A lower cap evicts pooled sockets under concurrency,
// which reintroduces exactly the per-request connection churn this agent
// exists to prevent (measured: at 64 concurrent requests, a cap of 32 left
// 303 sockets in TIME_WAIT versus 0 at 256).
const OPENCODE_AGENT_MAX_FREE_SOCKETS = 256;
/** 从本方驱逐池内空闲 socket 的超时（毫秒）；与 keepAliveMsecs（TCP keep-alive 探测延迟）是两个概念。 */
// Evicts idle free sockets from our side. Without it the only thing that
// retires an idle pooled socket is the upstream closing it. Note this is
// distinct from `keepAliveMsecs`, which is the TCP keep-alive probe delay.
const OPENCODE_AGENT_IDLE_TIMEOUT_MS = 60_000;

/** 共享的 agent 构造参数：开启 keep-alive、maxSockets 不设限（保持 agent:false 的并发能力），只改连接复用、不影响请求吞吐。 */
const OPENCODE_AGENT_OPTIONS = {
  keepAlive: true,
  keepAliveMsecs: OPENCODE_AGENT_KEEP_ALIVE_MS,
  maxSockets: Infinity,
  maxFreeSockets: OPENCODE_AGENT_MAX_FREE_SOCKETS,
  timeout: OPENCODE_AGENT_IDLE_TIMEOUT_MS,
};

/**
 * 判断代理目标是否为 https: 协议。
 *
 * 先按 URL 解析取 protocol；解析失败退回大小写不敏感的正则匹配；
 * 非字符串输入一律视为非 https。
 */
const isHttpsProxyTarget = (target) => {
  if (typeof target !== 'string') {
    return false;
  }
  try {
    return new URL(target).protocol === 'https:';
  } catch {
    return /^https:/i.test(target.trim());
  }
};

/**
 * Agent for proxied OpenCode API requests.
 *
 * When no agent is supplied, `http-proxy` falls back to `agent: false`, which
 * both disables connection pooling and forces `Connection: close` on every
 * proxied request (http-proxy/lib/http-proxy/common.js). That consumes one
 * ephemeral port per request, and sustained traffic can exhaust the host's
 * ephemeral port range — after which every process on the machine fails to
 * open outbound connections with EADDRNOTAVAIL.
 *
 * The agent must match the target scheme: http-proxy dispatches through
 * `https.request` when `target.protocol === 'https:'`
 * (http-proxy/lib/http-proxy/passes/web-incoming.js), and an `http.Agent`
 * would open a plaintext socket to a TLS port. External servers may be
 * configured over https via `OPENCODE_HOST` (see env-config.js), so derive the
 * agent class from the resolved target.
 *
 * `maxSockets: Infinity` preserves the unbounded concurrency of `agent: false`,
 * so this changes connection reuse only, not request throughput.
 */
/**
 * 创建用于代理 OpenCode API 请求的 keep-alive agent。
 *
 * 依据目标 scheme 选择 https.Agent 或 http.Agent（类不匹配会把明文
 * socket 接到 TLS 端口）；未提供 agent 时 http-proxy 退化为 agent:false，
 * 每个请求消耗一个临时端口并强制 Connection: close，持续流量可能耗尽
 * 主机临时端口（EADDRNOTAVAIL）。
 */
export const createOpenCodeProxyAgent = (target) => (
  isHttpsProxyTarget(target)
    ? new https.Agent(OPENCODE_AGENT_OPTIONS)
    : new http.Agent(OPENCODE_AGENT_OPTIONS)
);

/**
 * Lazily resolves the proxy agent, memoized per scheme.
 *
 * The scheme cannot be decided at registration time: `setupProxy()` runs before
 * `bootstrapOpenCodeAtStartup()` (startup-pipeline-runtime.js), so on a cold
 * start `state.openCodePort` is still null, `buildOpenCodeUrl()` throws
 * (network-runtime.js) and `resolveProxyTarget()` falls back to the http
 * loopback default. An external server configured over https via
 * `OPENCODE_HOST` only becomes visible on `state.openCodeBaseUrl` after
 * bootstrap completes.
 *
 * http-proxy-middleware rebuilds its per-request options with
 * `Object.assign({}, this.proxyOptions)` inside `prepareProxyRequest`, which
 * invokes getters, so exposing `agent` as a getter defers resolution to request
 * time. Memoizing per scheme keeps a single shared pool per scheme rather than
 * allocating an agent per request.
 */
/**
 * 创建按 scheme 记忆化的惰性 agent 解析器。
 *
 * 注册时 scheme 无法确定（setupProxy 早于 bootstrap，openCodePort 尚为
 * null，外部 https 目标要等 bootstrap 完成才可见）；把 agent 暴露为
 * getter 可将解析推迟到请求时，每个 scheme 仅建一个共享连接池。
 */
const createOpenCodeProxyAgentResolver = (resolveTarget) => {
  /** scheme -> agent 的记忆化缓存，保证每个协议只有一个共享连接池。 */
  const agents = new Map();

    /**
     * 读取当前代理目标并按其 scheme 取或建共享 agent。
     * 目标在 http/https 间切换时会自动落到对应 scheme 的池上。
     */
  return () => {
    const target = resolveTarget();
    const scheme = isHttpsProxyTarget(target) ? 'https:' : 'http:';
    let agent = agents.get(scheme);
    if (!agent) {
      // Construct through the shared factory rather than inline, so both
      // schemes are built from OPENCODE_AGENT_OPTIONS by the same code path.
      agent = createOpenCodeProxyAgent(target);
      agents.set(scheme, agent);
    }
    return agent;
  };
};

/**
 * 创建"目录查询参数规范化器"：把 URL 中 directory= 查询参数解析为真实
 * 路径（realpath），使 symlink 路径与实际会话目录一致。
 *
 * realpath 经 createRealpathCache 缓存（fallbackOnError:true，解析失败原样
 * 返回），避免在每个目录级请求的热路径上阻塞。返回
 * async (requestUrl) => 规范化后的 pathname+search；无 directory 参数或
 * 规范化结果与原值相同时原样返回。
 */
export const createDirectoryQueryCanonicalizer = ({ realpath, ...cacheOptions } = {}) => {
  /** 带 LRU 缓存与容错的 realpath 解析器；realpath 可注入以便测试。 */
  const realpathCache = createRealpathCache({ fallbackOnError: true, realpath, ...cacheOptions });

    /** 单次规范化：无 directory 查询参数时直接透传；否则替换为真实路径并返回 pathname+search。 */
  return async (requestUrl) => {
    if (typeof requestUrl !== 'string' || !requestUrl.includes('directory=')) {
      return requestUrl;
    }

    const url = new URL(requestUrl, 'http://localhost');
    const directory = url.searchParams.get('directory');
    if (!directory) {
      return requestUrl;
    }

    const canonicalDirectory = await realpathCache.resolve(directory);
    if (!canonicalDirectory || canonicalDirectory === directory) {
      return requestUrl;
    }

    url.searchParams.set('directory', canonicalDirectory);
    return `${url.pathname}${url.search}`;
  };
};

/**
 * 规范化转发给上游的目录相关 header。
 *
 * 当 x-opencode-directory-encoding 为 'uri' 时，对 x-opencode-directory
 * 的值做 decodeURIComponent 并删除 encoding 标记 header；解码失败的畸形
 * 值保持原样交给上游拒绝。原地修改并返回同一 headers 对象。
 */
export const normalizeForwardedDirectoryHeaders = (headers) => {
  const rawDirectory = headers?.['x-opencode-directory'];
  if (typeof rawDirectory !== 'string') {
    return headers;
  }

  if (headers['x-opencode-directory-encoding'] !== 'uri') {
    return headers;
  }

  try {
    headers['x-opencode-directory'] = decodeURIComponent(rawDirectory);
  } catch {
    // Leave malformed values untouched; upstream will reject invalid paths.
  }
  delete headers['x-opencode-directory-encoding'];
  return headers;
};

/**
 * 等待 SSE 响应流排空（res 的 drain 事件），永不 reject。
 *
 * signal 已 abort、或 res 已结束/销毁时立即 resolve；否则同时监听
 * drain/close/error 与 abort 信号，任一触发即解绑全部监听并 resolve。
 */
const waitForSseDrain = (res, signal) => new Promise((resolve) => {
  if (signal?.aborted || res.writableEnded || res.destroyed) {
    resolve();
    return;
  }

  /** 解绑全部一次性监听，防止事件泄漏。 */
  const cleanup = () => {
    res.off?.('drain', onDone);
    res.off?.('close', onDone);
    res.off?.('error', onDone);
    signal?.removeEventListener?.('abort', onDone);
  };
  /** 任一终止事件（排空/关闭/出错/中止）到达：清理监听并结束等待。 */
  const onDone = () => {
    cleanup();
    resolve();
  };

  res.once?.('drain', onDone);
  res.once?.('close', onDone);
  res.once?.('error', onDone);
  signal?.addEventListener?.('abort', onDone, { once: true });
});

/**
 * 向 SSE 响应写入一个 chunk 并处理写背压。
 *
 * 空值、已 abort、已结束或已销毁时直接返回 false；res.write 返回 false
 * （内核发送缓冲已满）时等待 drain，再返回流当前是否仍可写。返回值表示
 * "是否还能继续写入"。
 */
export const writeSseChunkWithBackpressure = async (res, value, signal) => {
  if (!value || value.length === 0 || signal?.aborted || res.writableEnded || res.destroyed) {
    return false;
  }

  const flushed = res.write(value);
  if (flushed !== false) {
    return true;
  }

  await waitForSseDrain(res, signal);
  return !signal?.aborted && !res.writableEnded && !res.destroyed;
};

/**
 * 创建 SSE 事件边界跟踪器：维护最近流文本的尾部，判断当前是否停在完整的
 * 事件边界上。
 *
 * 心跳注释只有插在 "\n\n" 边界处才不会撕裂上游事件；observe() 每次喂入
 * chunk 刷新尾部（换行统一为 \n，最多保留 4096 字符），
 * isAtBoundary() 判断尾部为空或以空行结尾。
 */
export const createSseBoundaryTracker = () => {
  /** 把 Uint8Array chunk 增量解码为文本的 TextDecoder。 */
  const decoder = new TextDecoder();
  /** 最近观测到的流文本尾部（换行已统一为 \n），仅保留最后 4096 字符。 */
  let tail = '';

  /** 把 CRLF / 孤立 CR 统一为 \n，使边界判定不受换行风格影响。 */
  const normalize = (value) => value.replace(/\r\n/g, '\n').replace(/\r/g, '\n');

  return {
    /** 喂入一个 chunk（string 或 Uint8Array），追加到尾部并返回当前是否处于事件边界。 */
    observe(value) {
      const text = typeof value === 'string'
        ? value
        : decoder.decode(value, { stream: true });
      if (text.length > 0) {
        tail = `${tail}${normalize(text)}`;
        if (tail.length > 4096) {
          tail = tail.slice(-4096);
        }
      }
      return this.isAtBoundary();
    },
    /** 是否处于完整 SSE 事件边界：尾部为空（尚未收到数据）或以空行（\n\n）结尾。 */
    isAtBoundary() {
      return tail.length === 0 || tail.endsWith('\n\n');
    },
  };
};

/**
 * 会话列表响应允许透传给客户端的字段白名单；名单外字段一律剔除，
 * 防止上游新增的大体量/敏感字段直接进入列表载荷。
 */
const SESSION_LIST_ALLOWED_FIELDS = [
  'id',
  'slug',
  'projectID',
  'workspaceID',
  'directory',
  'path',
  'parentID',
  'title',
  'agent',
  'model',
  'version',
  'time',
  'cost',
  'tokens',
  'share',
  'metadata',
  'project',
];

/**
 * 清洗单条会话记录：仅保留白名单字段；summary 深拷贝后剔除 diffs；
 * revert 仅保留字符串型的 messageID/partID（一个都没有则整个丢弃）。
 * 非对象输入（含数组、null）原样返回。
 */
const sanitizeSessionListItem = (session) => {
  if (!session || typeof session !== 'object' || Array.isArray(session)) {
    return session;
  }

  const sanitized = {};
  for (const key of SESSION_LIST_ALLOWED_FIELDS) {
    if (key in session) {
      sanitized[key] = session[key];
    }
  }

  const summary = session.summary;
  if (summary && typeof summary === 'object' && !Array.isArray(summary)) {
    const summaryWithoutDiffs = { ...summary };
    delete summaryWithoutDiffs.diffs;
    sanitized.summary = summaryWithoutDiffs;
  }

  const revert = session.revert;
  if (revert && typeof revert === 'object' && !Array.isArray(revert)) {
    const revertMarker = {};
    if (typeof revert.messageID === 'string') {
      revertMarker.messageID = revert.messageID;
    }
    if (typeof revert.partID === 'string') {
      revertMarker.partID = revert.partID;
    }
    if (Object.keys(revertMarker).length > 0) {
      sanitized.revert = revertMarker;
    }
  }

  return sanitized;
};

/** 清洗会话列表载荷：数组逐条清洗，其它类型原样透传（语义交由上游决定）。 */
const sanitizeSessionListPayload = (payload) => {
  if (!Array.isArray(payload)) {
    return payload;
  }
  return payload.map((session) => sanitizeSessionListItem(session));
};

/**
 * 在 express app 上注册 OpenCode API 代理及全部配套中间件与路由。
 *
 * 按挂载顺序：
 * 1. /api 前缀确保中间件（ensureOpenCodeApiPrefix）；
 * 2. 就绪门控：OpenCode 启动/重启期间挂起请求轮询就绪（有限窗口），
 *    超时才回 503，避免客户端进入指数退避重试；
 * 3. Windows 会话合并路由及 /api/session、/api/experimental/session
 *    的脱敏列表路由；
 * 4. /api/event、/api/global/event 的 SSE 手动转发路由；
 * 5. directory= 查询规范化中间件与响应截止时间中间件；
 * 6. 交互式 OAuth、会话 turn 专用与通用三层 http-proxy 代理。
 *
 * deps 注入 fs/os/path、超时配置、runtime 状态访问器、鉴权头构造器、
 * URL 构造器等；重复注册由 app.set('opencodeProxyConfigured') 幂等挡住。
 */
export const registerOpenCodeProxy = (app, deps) => {
  // 注入依赖：fs/os/path、就绪宽限与长请求超时、runtime 状态访问器、
  // OpenCode 鉴权头构造器、上游 URL 构造器、API 前缀确保与 SSE 超时配置。
  const {
    fs,
    os,
    path,
    OPEN_CODE_READY_GRACE_MS,
    LONG_REQUEST_TIMEOUT_MS,
    getRuntime,
    getOpenCodeAuthHeaders,
    buildOpenCodeUrl,
    ensureOpenCodeApiPrefix,
    SSE_HEARTBEAT_INTERVAL_MS = DEFAULT_SSE_HEARTBEAT_INTERVAL_MS,
    SSE_UPSTREAM_STALL_TIMEOUT_MS = DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
    getSseUpstreamStallTimeoutMs = () => SSE_UPSTREAM_STALL_TIMEOUT_MS,
  } = deps;

  // 幂等保护：同一 app 重复注册时直接返回。
  if (app.get('opencodeProxyConfigured')) {
    return;
  }

  const runtime = getRuntime();
  if (runtime.openCodePort) {
    console.log(`Setting up proxy to OpenCode on port ${runtime.openCodePort}`);
  } else {
    console.log('Setting up OpenCode API gate (OpenCode not started yet)');
  }
  app.set('opencodeProxyConfigured', true);

  /** 判断错误是否为 AbortError（主动中止的正常中断，不按故障处理）。 */
  const isAbortError = (error) => error?.name === 'AbortError';
  /** 兜底代理目标：本机回环的 OpenCode 默认端口（端口未知时的最后退路）。 */
  const FALLBACK_PROXY_TARGET = 'http://127.0.0.1:3902';
  /** 目录查询规范化器实例（realpath 取自注入的 fs.promises，带缓存）。 */
  const canonicalizeDirectoryQuery = createDirectoryQueryCanonicalizer({
    realpath: fs?.promises?.realpath?.bind(fs.promises),
  });

  /** 判断 body-parser 产物是否携带有效载荷：Buffer/字符串/数组看长度，对象看自有键数；undefined/null 为 false。 */
  const hasParsedBodyValue = (body) => {
    if (body === undefined || body === null) return false;
    if (Buffer.isBuffer(body)) return body.length > 0;
    if (typeof body === 'string') return body.length > 0;
    if (Array.isArray(body)) return body.length > 0;
    if (typeof body === 'object') return Object.keys(body).length > 0;
    return true;
  };

  /** 读取代理请求的 content-type：优先 proxyReq 已设值，退回原始请求 header；数组取首项，最终兜底空串。 */
  const getContentType = (proxyReq, req) => {
    const value = proxyReq.getHeader?.('content-type') ?? req.headers?.['content-type'] ?? '';
    if (Array.isArray(value)) return value[0] || '';
    return String(value || '');
  };

  /** 把对象序列化为 x-www-form-urlencoded 字符串：跳过 null/undefined，数组逐项 append（形成重复键），非对象直接 String 化。 */
  const serializeUrlEncodedBody = (body) => {
    if (!body || typeof body !== 'object' || Buffer.isBuffer(body)) {
      return String(body ?? '');
    }

    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(body)) {
      if (value === undefined || value === null) continue;
      if (Array.isArray(value)) {
        for (const entry of value) {
          if (entry !== undefined && entry !== null) params.append(key, String(entry));
        }
        continue;
      }
      params.append(key, String(value));
    }
    return params.toString();
  };

  /** 把已解析的 req.body 重新序列化为 Buffer：GET/HEAD 或无 body 返回 null；按 content-type 分别走 JSON / urlencoded / 原字符串；其余返回 null（不重放）。 */
  const serializeParsedBody = (req, proxyReq) => {
    if (req.method === 'GET' || req.method === 'HEAD') return null;
    if (req.body === undefined || req.body === null) return null;
    const originalContentLength = Number.parseInt(req.headers?.['content-length'] || '0', 10) || 0;
    if (!hasParsedBodyValue(req.body) && originalContentLength <= 0) return null;

    const contentType = getContentType(proxyReq, req).toLowerCase();
    if (Buffer.isBuffer(req.body)) return req.body;
    if (contentType.includes('application/json')) return Buffer.from(JSON.stringify(req.body));
    if (contentType.includes('application/x-www-form-urlencoded')) return Buffer.from(serializeUrlEncodedBody(req.body));
    if (typeof req.body === 'string') return Buffer.from(req.body);
    return null;
  };

  /** 向 proxyReq 重放已解析的请求体：先按序列化结果更新 content-length 再 write；无需重放时跳过。 */
  const replayParsedBody = (proxyReq, req) => {
    const body = serializeParsedBody(req, proxyReq);
    if (!body) return;
    proxyReq.setHeader('content-length', String(body.length));
    proxyReq.write(body);
  };

  /** 规范化候选目标 URL：非字符串或 trim 后为空返回 null，否则去掉尾部斜杠。 */
  const normalizeProxyTarget = (candidate) => {
    if (typeof candidate !== 'string') {
      return null;
    }

    const trimmed = candidate.trim();
    if (!trimmed) {
      return null;
    }

    return trimmed.replace(/\/+$/, '');
  };

  /**
   * 解析当前代理目标，保证与 health check、直接 fetch 使用同一 base URL
   * （避免 /health 命中外部主机而 /api/* 仍打回环的脑裂）。
   * 优先级：端口已知时 buildOpenCodeUrl 构造的本机地址 → runtime 的
   * 外部 openCodeBaseUrl（OPENCODE_HOST 场景）→ 回环兜底。
   * buildOpenCodeUrl 在端口未知时会抛错，先探测端口避免每个代理请求
   * 都为一次 throw/catch 付出代价。
   */
  // Keep generic proxy requests on the same upstream base URL that health checks
  // and direct fetch helpers use. This avoids split-brain state where /health
  // succeeds against an external host but /api/* still proxies to 127.0.0.1.
  const resolveProxyTarget = () => {
    const runtimeState = getRuntime();

    // `buildOpenCodeUrl` throws while the port is unknown, and the port is
    // nulled on several runtime paths (health-check failure, failed restart),
    // not just cold start. Checking first keeps a degraded OpenCode from
    // making every proxied request pay for a thrown-and-caught exception.
    if (runtimeState.openCodePort) {
      try {
        const resolved = normalizeProxyTarget(buildOpenCodeUrl('/', ''));
        if (resolved) {
          return resolved;
        }
      } catch {
      }
    }

    const externalBase = normalizeProxyTarget(runtimeState.openCodeBaseUrl);
    if (externalBase) {
      return externalBase;
    }

    return FALLBACK_PROXY_TARGET;
  };

  /** 归一化超时值：仅接受正有限数，否则退回 4 分钟默认值。 */
  const normalizeProxyTimeout = (value) => {
    return Number.isFinite(value) && value > 0 ? value : 4 * 60 * 1000;
  };

  /** 普通代理请求的统一截止时间（由 LONG_REQUEST_TIMEOUT_MS 归一化而来）。 */
  const PROXY_REQUEST_TIMEOUT_MS = normalizeProxyTimeout(LONG_REQUEST_TIMEOUT_MS);
  /** 标记请求已被本方截止计时器终结的 Symbol；代理 error 回调据此避免重复写响应。 */
  const PROXY_TIMEOUT_MARKER = Symbol('ompchamberProxyTimedOut');

  /** 交互式 OAuth 回调的截止时间（15 分钟）：等待用户在浏览器完成授权，不共用普通请求 deadline。 */
  // A provider OAuth callback blocks upstream for as long as the user takes to
  // sign in in their browser (device-code polling, or a loopback redirect), so
  // it cannot share the ordinary request deadline. Bounded by the shortest
  // upstream expiry we know of — GitHub device codes last ~15 minutes.
  const INTERACTIVE_OAUTH_TIMEOUT_MS = 15 * 60 * 1000;
  /** 交互式 OAuth 回调的路径模式：/provider/:provider/oauth/callback。 */
  const INTERACTIVE_OAUTH_PATH = /^\/provider\/[^/]+\/oauth\/callback\/?$/;

  /** 判断是否为需要长等待的 provider OAuth 回调请求（POST 且路径匹配）。 */
  const isInteractiveOAuthCallback = (req) =>
    req.method === 'POST' && INTERACTIVE_OAUTH_PATH.test(req.path);

  /** 会话 turn 类请求的截止时间（6 小时）：turn 在引擎侧结算、无固定时长，此值只兜住"接收请求后失联"的引擎，不是 turn 时长上限。 */
  // An engine session turn answers its request when the *turn* settles, not
  // when the engine accepts it. Measured against the managed engine: a 7s turn
  // returned its `prompt_async` POST at 6.97s, and a 5-minute turn (the
  // sharkly/tubeworm session) held the POST past the ordinary deadline. A turn
  // has no bounded length, so the ordinary deadline reported "upstream timed
  // out" for prompts the engine had already accepted and was running, which
  // also told every caller the send failed.
  //
  // Six hours is not a turn bound: it only catches an engine that took the
  // request and then went silent. The routes that run a turn are the ones that
  // block; the rest of the session API stays on the ordinary deadline.
  const TURN_BOUND_TIMEOUT_MS = 6 * 60 * 60 * 1000;
  /** 会触发完整 turn 的 session 动作集合（prompt、command、shell 等），这些动作按 turn 级 deadline 处理。 */
  const TURN_BOUND_SESSION_ACTIONS = new Set(['prompt', 'prompt_async', 'command', 'shell', 'summarize', 'init']);
  /** 从 /session/:id/:action 路径中捕获 action 名的模式。 */
  const SESSION_ACTION_PATH = /^\/session\/[^/]+\/([^/?]+)\/?$/;

  /** 判断请求是否为 turn 类会话动作（POST 且 action 在 TURN_BOUND_SESSION_ACTIONS 中）。 */
  const isTurnBoundSessionRequest = (req) => {
    if (req.method !== 'POST') return false;
    const match = SESSION_ACTION_PATH.exec(req.path ?? '');
    return match ? TURN_BOUND_SESSION_ACTIONS.has(match[1]) : false;
  };

  /** 识别上游超时类错误：错误码 ETIMEDOUT/ESOCKETTIMEDOUT，或消息包含 timeout/timed out。 */
  const isProxyTimeoutError = (error) => {
    const code = typeof error?.code === 'string' ? error.code : '';
    const message = typeof error?.message === 'string' ? error.message.toLowerCase() : '';
    return code === 'ETIMEDOUT'
      || code === 'ESOCKETTIMEDOUT'
      || message.includes('timeout')
      || message.includes('timed out');
  };

  /** 尽力发送代理错误响应（504 超时 / 503 不可用）；已发送 header、已结束或不可写时返回 false 且不再触碰响应。 */
  const sendProxyErrorResponse = (res, statusCode) => {
    if (!res || res.headersSent || res.writableEnded || typeof res.status !== 'function') {
      return false;
    }
    res.status(statusCode).json({ error: statusCode === 504 ? 'OpenCode upstream timed out' : 'OpenCode service unavailable' });
    return true;
  };

  /**
   * 普通代理请求的响应截止中间件：超时则打 PROXY_TIMEOUT_MARKER、回 504，
   * 并在响应结束后销毁请求以释放上游连接；交互式 OAuth 与 turn 类请求
   * 跳过（各有更长的 deadline）。finish/close 时清理计时器。
   */
  const applyProxyResponseDeadline = (req, res, next) => {
    if (isInteractiveOAuthCallback(req) || isTurnBoundSessionRequest(req)) {
      return next();
    }

    const timeout = setTimeout(() => {
      req[PROXY_TIMEOUT_MARKER] = true;
      if (sendProxyErrorResponse(res, 504)) {
        res.once('finish', () => req.destroy?.());
      }
    }, PROXY_REQUEST_TIMEOUT_MS);
    timeout.unref?.();

    const clear = () => clearTimeout(timeout);
    res.once('finish', clear);
    res.once('close', clear);
    next();
  };

  /**
   * 手动转发 OpenCode SSE 事件流（fetch + ReadableStream，绕过 http-proxy）。
   *
   * 流程：剥离 /api 前缀 → 组装转发 header（鉴权 + 目录解码 + SSE 缺省头）
   * → fetch 上游 → 透传状态码与响应头 → 非 event-stream 响应整体透传，
   * 事件流则逐 chunk 经串行写队列转发（背压传播）。
   * 维持三类活性：周期心跳（仅在事件边界注入，避免撕裂上游事件）、上游
   * 停滞计时器（超窗 abort 上游）、客户端断连监听（abort 上游）。
   * AbortError 视为正常收尾（停滞场景补写 res.end）；其它错误在未发送头
   * 时回 503。finally 中清理计时器、解绑监听并取消上游 reader/body。
   */
  const forwardSseRequest = async (req, res) => {
  // 上游 fetch 的中止控制器：客户端断连或上游停滞都会触发 abort。
    const abortController = new AbortController();
  // 客户端断连时中止上游 fetch 的回调。
    const closeUpstream = () => abortController.abort();
  // 上游 fetch 的 Response（finally 中释放 body）。
    let upstream = null;
  // 上游流的 reader（finally 中 cancel 并 releaseLock）。
    let reader = null;
  // 心跳计时器句柄（finally 中清理）。
    let heartbeatTimer = null;
  // 上游停滞计时器句柄（finally 中清理）。
    let upstreamStallTimer = null;
  // 是否因上游停滞而 abort（决定收尾时是否补 res.end）。
    let didUpstreamStall = false;
  // 串行写队列尾（保证 chunk 顺序与背压传播）。
    let writeQueue = Promise.resolve(true);
  // SSE 事件边界跟踪器（判定心跳的安全插入点）。
    const sseBoundary = createSseBoundaryTracker();

    req.on('close', closeUpstream);

    try {
      const requestUrl = typeof req.originalUrl === 'string' && req.originalUrl.length > 0
        ? req.originalUrl
        : (typeof req.url === 'string' ? req.url : '');
      const upstreamPath = requestUrl.startsWith('/api') ? requestUrl.slice(4) || '/' : requestUrl;
      const headers = normalizeForwardedDirectoryHeaders(
        collectForwardProxyHeaders(req.headers, getOpenCodeAuthHeaders())
      );
      headers.accept ??= 'text/event-stream';
      headers['cache-control'] ??= 'no-cache';

      upstream = await fetch(buildOpenCodeUrl(upstreamPath, ''), {
        method: 'GET',
        headers,
        signal: abortController.signal,
      });

      res.status(upstream.status);
      applyForwardProxyResponseHeaders(upstream.headers, res);

      const contentType = upstream.headers.get('content-type') || 'text/event-stream';
      const isEventStream = contentType.toLowerCase().includes('text/event-stream');

      if (!upstream.body) {
        res.end(await upstream.text().catch(() => ''));
        return;
      }

      if (!isEventStream) {
        res.end(await upstream.text());
        return;
      }

      res.setHeader('Content-Type', contentType);
      res.setHeader('Cache-Control', 'no-cache');
      res.setHeader('Connection', 'keep-alive');
      res.setHeader('X-Accel-Buffering', 'no');
      if (typeof res.flushHeaders === 'function') {
        res.flushHeaders();
      }

      // Disable TCP Nagle's algorithm so small SSE chunks are sent immediately
      // instead of being buffered up to ~200ms by the TCP stack.
      if (res.socket && typeof res.socket.setNoDelay === 'function') {
        res.socket.setNoDelay(true);
      }

      /** 调度下一拍心跳：流已终止直接返回；未处于事件边界只重排不注入；否则入队心跳注释并在可继续写时排下一拍。 */
      const scheduleHeartbeat = () => {
        heartbeatTimer = setTimeout(async () => {
          if (abortController.signal.aborted || res.writableEnded || res.destroyed) {
            return;
          }
          if (!sseBoundary.isAtBoundary()) {
            scheduleHeartbeat();
            return;
          }
          const canContinue = await enqueueSseWrite(':heartbeat\n\n');
          if (canContinue) {
            scheduleHeartbeat();
          }
        }, SSE_HEARTBEAT_INTERVAL_MS);
      };

      /** 清除上游停滞计时器。 */
      const clearUpstreamStallTimer = () => {
        clearTimeout(upstreamStallTimer);
        upstreamStallTimer = null;
      };

      /** 重置上游停滞计时器：窗口（getSseUpstreamStallTimeoutMs）内没有新数据则置位 didUpstreamStall 并 abort 上游。 */
      const resetUpstreamStallTimer = () => {
        clearUpstreamStallTimer();
        upstreamStallTimer = setTimeout(() => {
          didUpstreamStall = true;
          abortController.abort();
        }, getSseUpstreamStallTimeoutMs());
        upstreamStallTimer.unref?.();
      };

      /** 把一次 SSE 写入排到串行队列末尾：前序写失败（false）直接短路，保证 chunk 顺序与背压传播。 */
      const enqueueSseWrite = (value) => {
        writeQueue = writeQueue
          .catch(() => false)
          .then((canContinue) => {
            if (!canContinue) {
              return false;
            }
            return writeSseChunkWithBackpressure(res, value, abortController.signal);
          });
        return writeQueue;
      };

      scheduleHeartbeat();
      resetUpstreamStallTimer();

      reader = upstream.body.getReader();
      while (!abortController.signal.aborted) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        if (value && value.length > 0) {
          resetUpstreamStallTimer();
          sseBoundary.observe(value);
          const canContinue = await enqueueSseWrite(value);
          if (!canContinue) {
            break;
          }
        }
      }

      res.end();
    } catch (error) {
      if (isAbortError(error)) {
        if (didUpstreamStall && !res.writableEnded && !res.destroyed) {
          await writeQueue.catch(() => false);
          res.end();
        }
        return;
      }
      console.error('[proxy] OpenCode SSE proxy error:', error?.message ?? error);
      if (!res.headersSent) {
        res.status(503).json({ error: 'OpenCode service unavailable' });
      } else {
        res.end();
      }
    } finally {
      if (heartbeatTimer) {
        clearTimeout(heartbeatTimer);
        heartbeatTimer = null;
      }
      if (upstreamStallTimer) {
        clearTimeout(upstreamStallTimer);
        upstreamStallTimer = null;
      }
      req.off('close', closeUpstream);
      try {
        if (reader) {
          await reader.cancel();
          reader.releaseLock();
        } else if (upstream?.body && !upstream.body.locked) {
          await upstream.body.cancel();
        }
      } catch {
      }
    }
  };

  /**
   * 拉取会话列表类 JSON 载荷。
   *
   * 传入 req 时从其 header 复制转发头（含鉴权与目录解码），否则仅携带
   * 鉴权头；可选用 AbortSignal.timeout 限时。返回原始 Response、
   * content-type、正文文本与解析结果：非 JSON 或解析失败时 payload 为
   * null（parseError 记录原因），由调用方决定透传还是清洗。
   */
  const fetchSessionListPayload = async (upstreamPath, { req = null, timeoutMs = null } = {}) => {
    const headers = req
      ? {
          ...normalizeForwardedDirectoryHeaders(collectForwardProxyHeaders(req.headers, getOpenCodeAuthHeaders())),
          accept: 'application/json',
          'accept-encoding': 'identity',
        }
      : {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
          'accept-encoding': 'identity',
        };
    const upstream = await fetch(buildOpenCodeUrl(upstreamPath, ''), {
      method: 'GET',
      headers,
      ...(typeof timeoutMs === 'number' ? { signal: AbortSignal.timeout(timeoutMs) } : {}),
    });
    const contentType = upstream.headers.get('content-type') || 'application/json; charset=utf-8';
    const bodyText = await upstream.text();
    const isJson = contentType.toLowerCase().includes('application/json');

    if (!isJson) {
      return { upstream, contentType, bodyText, payload: null, isJson: false };
    }

    try {
      const payload = JSON.parse(bodyText);
      return { upstream, contentType, bodyText, payload, isJson: true, parseError: null };
    } catch (parseError) {
      return { upstream, contentType, bodyText, payload: null, isJson: true, parseError };
    }
  };

  /** 取请求对应的上游路径：剥离 /api 前缀，并把 directory= 查询参数规范化为真实路径。 */
  const getRequestUpstreamPath = async (req) => {
    const requestUrl = typeof req.originalUrl === 'string' && req.originalUrl.length > 0
      ? req.originalUrl
      : (typeof req.url === 'string' ? req.url : '');
    const upstreamPathRaw = requestUrl.startsWith('/api') ? requestUrl.slice(4) || '/' : requestUrl;
    return canonicalizeDirectoryQuery(upstreamPathRaw);
  };

  /**
   * 转发并脱敏会话列表请求：透传上游状态码与响应头；载荷非 JSON、解析
   * 失败或非数组时原样透传正文；否则经 sanitizeSessionListPayload 清洗后
   * 以 JSON 返回。AbortError 静默返回；其它错误在未发送头时回 503，
   * 否则交 next(error)。
   */
  const forwardSanitizedSessionListRequest = async (req, res, next, logLabel) => {
    try {
      const upstreamPath = await getRequestUpstreamPath(req);
      const result = await fetchSessionListPayload(upstreamPath, { req });

      res.status(result.upstream.status);
      applyForwardProxyResponseHeaders(result.upstream.headers, res);

      if (!result.isJson) {
        res.setHeader('content-type', result.contentType);
        res.end(result.bodyText);
        return;
      }

      if (result.parseError || !Array.isArray(result.payload)) {
        res.setHeader('content-type', result.contentType);
        res.end(result.bodyText);
        return;
      }

      res.setHeader('content-type', result.contentType);
      res.json(sanitizeSessionListPayload(result.payload));
    } catch (error) {
      if (isAbortError(error)) {
        return;
      }
      console.error(`[proxy] OpenCode ${logLabel} proxy error:`, error?.message ?? error);
      if (!res.headersSent) {
        res.status(503).json({ error: 'OpenCode service unavailable' });
        return;
      }
      next(error);
    }
  };

  // Ensure API prefix is detected before proxying
  app.use('/api', (_req, _res, next) => {
    ensureOpenCodeApiPrefix();
    next();
  });

  // Readiness gate — while OpenCode is starting/restarting, HOLD the request and
  // poll readiness instead of returning 503 immediately. A bare 503 pushes the
  // client into an exponential-backoff retry loop (500ms → 1s → …) that wastes
  // seconds of cold-start time and can fail bootstrap outright. Holding the
  // request until OpenCode is ready (typically well under a second) lets the
  // first call simply succeed. We still 503 if readiness doesn't arrive within a
  // bounded window so genuinely-down servers fail fast.
  /** 就绪门控的轮询间隔（毫秒）。 */
  const READINESS_HOLD_POLL_MS = 75;
  /** 就绪门控的最长挂起时间（毫秒），与 OPEN_CODE_READY_GRACE_MS 取小。 */
  const READINESS_HOLD_MAX_MS = 6000;
  /** Promise 化的 setTimeout。 */
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  /** 判断是否仍应挂起请求：未就绪且仍在宽限窗口内、正在重启、或端口未知，三者居一即等待。 */
  const isStillWaiting = (runtimeState) => {
    const waitElapsed = runtimeState.openCodeNotReadySince === 0 ? 0 : Date.now() - runtimeState.openCodeNotReadySince;
    return (
      (!runtimeState.isOpenCodeReady && (runtimeState.openCodeNotReadySince === 0 || waitElapsed < OPEN_CODE_READY_GRACE_MS)) ||
      runtimeState.isRestartingOpenCode ||
      !runtimeState.openCodePort
    );
  };
  /** 把请求路径归类（session-messages / session / events / other），仅用于启动性能打点。 */
  const classifyReadinessRoute = (requestPath) => {
    if (/^\/session\/[^/]+\/message(?:\/|$)/.test(requestPath)) return 'session-messages';
    if (requestPath === '/session' || requestPath.startsWith('/session/')) return 'session';
    if (requestPath === '/event' || requestPath === '/global/event') return 'events';
    return 'other';
  };

  /**
   * 就绪门控中间件：先豁免由本服务自行处理的主题/推送/配置/健康等路径；
   * 仍在等待就绪时每 75ms 轮询，就绪即放行，客户端放弃（断连/中止）则
   * 停止挂起；超过 min(宽限, 6s) 窗口回 503 并标记 restarting。
   * 每次结束都记录启动性能打点（outcome: ready/aborted/timeout）。
   */
  app.use('/api', async (req, res, next) => {
    if (
      req.path.startsWith('/themes/custom') ||
      req.path.startsWith('/push') ||
      req.path.startsWith('/config/agents') ||
      req.path.startsWith('/config/opencode-resolution') ||
      req.path.startsWith('/config/settings') ||
      req.path.startsWith('/config/skills') ||
      req.path === '/config/reload' ||
      req.path === '/health'
    ) {
      return next();
    }

    if (!isStillWaiting(getRuntime())) {
      return next();
    }

    const holdStartedAt = performance.now();
    const routeClass = classifyReadinessRoute(req.path);
    const deadline = Date.now() + Math.min(OPEN_CODE_READY_GRACE_MS, READINESS_HOLD_MAX_MS);
    while (Date.now() < deadline) {
      // Client gave up (closed/aborted) — stop holding.
      if (res.writableEnded || req.aborted) {
        recordStartupPerformance('proxy.readiness-hold', {
          durationMs: performance.now() - holdStartedAt,
          outcome: 'aborted',
          routeClass,
        });
        return;
      }
      await sleep(READINESS_HOLD_POLL_MS);
      if (!isStillWaiting(getRuntime())) {
        recordStartupPerformance('proxy.readiness-hold', {
          durationMs: performance.now() - holdStartedAt,
          outcome: 'ready',
          routeClass,
        });
        return next();
      }
    }

    recordStartupPerformance('proxy.readiness-hold', {
      durationMs: performance.now() - holdStartedAt,
      outcome: 'timeout',
      routeClass,
    });
    if (!res.headersSent) {
      res.status(503).json({
        error: 'OpenCode is restarting',
        restarting: true,
      });
    }
  });

  // Windows: session merge for cross-directory session listing
  if (process.platform === 'win32') {
    /**
     * Windows 跨目录会话列表合并路由：请求自带 directory= 时交回默认流程；
     * 否则合并全局列表与 settings.projects 中各项目目录（含正/反斜杠两种
     * 写法）的会话，按 id 去重、按 time_updated 降序排序，脱敏后返回。
     * 全局与所有目录读取都失败时回 504，其余异常回 500。
     */
    app.get('/api/session', async (req, res, next) => {
      const rawUrl = req.originalUrl || req.url || '';
      if (rawUrl.includes('directory=')) return next();

      /** 拉取并清洗单个来源的会话列表（10 秒限时）；上游非 200 或载荷非数组时返回 null。 */
      const fetchWindowsSessionList = async (sessionPath) => {
        const result = await fetchSessionListPayload(sessionPath, { req, timeoutMs: 10000 });
        if (!result.upstream.ok || !Array.isArray(result.payload)) return null;
        return sanitizeSessionListPayload(result.payload);
      };

      try {
        const globalSessions = await fetchWindowsSessionList('/session').catch((error) => {
          console.log(`[SessionMerge] Global session list failed: ${error.message}`);
          return null;
        });

        // 从持久化 settings.json 读取项目目录列表；读取/解析失败按空列表处理。
        const settingsPath = path.join(os.homedir(), '.config', 'ompchamber', 'settings.json');
        let projectDirs = [];
        try {
          const settingsRaw = fs.readFileSync(settingsPath, 'utf8');
          const settings = JSON.parse(settingsRaw);
          projectDirs = (settings.projects || [])
            .map((project) => (typeof project?.path === 'string' ? project.path.trim() : ''))
            .filter(Boolean);
        } catch {
        }

        const seen = new Set(
          (globalSessions || [])
            .map((session) => (session && typeof session.id === 'string' ? session.id : null))
            .filter((id) => typeof id === 'string')
        );
        const extraSessions = [];
        let successfulProjectReads = 0;
        for (const dir of projectDirs) {
          const candidates = Array.from(new Set([
            dir,
            dir.replace(/\\/g, '/'),
            dir.replace(/\//g, '\\'),
          ]));
          for (const candidateDir of candidates) {
            const encoded = encodeURIComponent(candidateDir);
            try {
              const dirSessions = await fetchWindowsSessionList(`/session?directory=${encoded}`);
              if (dirSessions) {
                successfulProjectReads += 1;
              }
              for (const session of dirSessions || []) {
                const id = session && typeof session.id === 'string' ? session.id : null;
                if (id && !seen.has(id)) {
                  seen.add(id);
                  extraSessions.push(session);
                }
              }
            } catch {
            }
          }
        }

        if (!globalSessions && successfulProjectReads === 0) {
          return res.status(504).json({ error: 'OpenCode session list timed out' });
        }

        const merged = [...(globalSessions || []), ...extraSessions];
        merged.sort((a, b) => {
          const aTime = a && typeof a.time_updated === 'number' ? a.time_updated : 0;
          const bTime = b && typeof b.time_updated === 'number' ? b.time_updated : 0;
          return bTime - aTime;
        });
        console.log(`[SessionMerge] ${globalSessions?.length || 0} global + ${extraSessions.length} extra = ${merged.length} total`);
        return res.json(sanitizeSessionListPayload(merged));
      } catch (error) {
        console.log(`[SessionMerge] Error: ${error.message}`);
        return res.status(500).json({ error: error.message || 'Failed to merge Windows sessions' });
      }
    });
  }

  // 默认会话列表路由：转发上游并做字段白名单脱敏。
  app.get('/api/session', (req, res, next) => {
    return forwardSanitizedSessionListRequest(req, res, next, 'session.list');
  });

  // OpenCode 事件流的 SSE 手动转发路由（全局与当前实例两个端点）。
  app.get('/api/global/event', forwardSseRequest);
  app.get('/api/event', forwardSseRequest);

  // 实验性会话列表端点：同样走脱敏转发。
  app.get('/api/experimental/session', (req, res, next) => {
    return forwardSanitizedSessionListRequest(req, res, next, 'experimental.session');
  });

  /** 按 scheme 记忆化的共享 agent 解析器（getter 化使用，注册时 scheme 未知也不受影响）。 */
  // Generic proxy for non-SSE OpenCode API routes.
  // The agent is exposed as a getter so its class is resolved per request, not
  // at registration: the proxy is registered before OpenCode bootstraps, so an
  // https target configured via OPENCODE_HOST is not yet visible here. Agents
  // are memoized per scheme, so this is still one shared pool per scheme across
  // `apiProxy` and `interactiveOAuthProxy`.
  const resolveOpenCodeProxyAgent = createOpenCodeProxyAgentResolver(resolveProxyTarget);

  /**
   * 构造一层 http-proxy 代理中间件：目标动态解析（router，支持重启后换
   * 端口）、/api 前缀重写、注入 OpenCode 鉴权头、目录 header 解码、
   * 请求体重放、响应头白名单过滤；超时与错误统一回 504/503。
   * timeoutMs 同时作为该层的 timeout 与 proxyTimeout。
   */
  const createApiProxy = (timeoutMs) => createProxyMiddleware({
    target: resolveProxyTarget(),
    get agent() {
      return resolveOpenCodeProxyAgent();
    },
    changeOrigin: true,
    pathRewrite: { '^/api': '' },
    timeout: timeoutMs,
    proxyTimeout: timeoutMs,
    // Dynamic target — port can change after restart
    router: () => resolveProxyTarget(),
    on: {
        // 组装上游请求头：鉴权、URI 编码的目录 header 解码、identity 编码与请求体重放。
      proxyReq: (proxyReq, req) => {
        // Inject OpenCode auth headers
        const authHeaders = getOpenCodeAuthHeaders();
        if (authHeaders.Authorization) {
          proxyReq.setHeader('Authorization', authHeaders.Authorization);
        }

        if (req.headers?.['x-opencode-directory-encoding'] === 'uri') {
          const rawDirectory = req.headers['x-opencode-directory'];
          if (typeof rawDirectory === 'string') {
            try {
              proxyReq.setHeader('x-opencode-directory', decodeURIComponent(rawDirectory));
            } catch {
              proxyReq.setHeader('x-opencode-directory', rawDirectory);
            }
          }
          proxyReq.removeHeader?.('x-opencode-directory-encoding');
        }

        // Defensive: request identity encoding from upstream OpenCode.
        // This avoids compressed-body/header mismatches in multi-proxy setups.
        proxyReq.setHeader('accept-encoding', 'identity');

        replayParsedBody(proxyReq, req);
      },
        // 响应头白名单过滤：剔除不应转发给客户端的上游响应头。
      proxyRes: (proxyRes) => {
        for (const key of Object.keys(proxyRes.headers || {})) {
          if (!shouldForwardProxyResponseHeader(key)) {
            delete proxyRes.headers[key];
          }
        }
      },
        // 代理错误统一出口：已被本方截止计时器终结的请求不重复写响应；超时类错误回 504，其余回 503。
      error: (err, req, res) => {
        console.error('[proxy] OpenCode proxy error:', err.message);
        if (req?.[PROXY_TIMEOUT_MARKER]) {
          return;
        }
        const statusCode = isProxyTimeoutError(err) ? 504 : 503;
        sendProxyErrorResponse(res, statusCode);
      },
    },
  });

  /** 通用代理层：普通 /api 请求（默认截止时间）。 */
  const apiProxy = createApiProxy(PROXY_REQUEST_TIMEOUT_MS);
  /** 交互式 OAuth 代理层：provider OAuth 与 MCP 认证回调（15 分钟截止）。 */
  const interactiveOAuthProxy = createApiProxy(INTERACTIVE_OAUTH_TIMEOUT_MS);
  /** 会话 turn 代理层：prompt/command 等长动作（6 小时截止）。 */
  const turnBoundProxy = createApiProxy(TURN_BOUND_TIMEOUT_MS);

  // Best-effort fallback for stale clients still sending symlink paths.
  // Settings and project selection normalize at source; this cached async path
  // avoids blocking the proxy hot path on every directory-scoped request.
  app.use('/api', async (req, _res, next) => {
    try {
      const rewrittenUrl = await canonicalizeDirectoryQuery(req.url);
      if (rewrittenUrl !== req.url) {
        req.url = rewrittenUrl;
      }
    } catch {
      // Pass through as-is if URL parsing or realpath resolution fails.
    }
    next();
  });

  app.use('/api', applyProxyResponseDeadline);
  app.post('/api/provider/:providerID/oauth/callback', interactiveOAuthProxy);
  // OpenCode's native MCP OAuth flow: the request blocks until the user
  // finishes authorization in the browser (up to OpenCode's 5-minute callback
  // timeout), so it needs the interactive-OAuth deadline, not the default one.
  app.post('/api/mcp/:name/auth/authenticate', interactiveOAuthProxy);
  // Session turns: prompt, command, shell, and the like run until the agent
  // stops, so they get the turn-bound deadline instead of the request one.
  app.post('/api/session/:sessionID/:action', (req, res, next) => {
    if (!TURN_BOUND_SESSION_ACTIONS.has(String(req.params.action))) {
      return next();
    }
    return turnBoundProxy(req, res, next);
  });
  app.use('/api', apiProxy);
};
