// Host side of the tunnel mux (Layer 3): consumes decrypted tunnel frames for
// ONE relay connection and dispatches them to the local loopback origin.
// HTTP streams -> fetch http://127.0.0.1:<port> with streamed duplex bodies;
// WS streams -> `ws` client to the loopback WebSocket endpoints.
// The dispatcher NEVER injects credentials: tunneled requests authenticate
// exactly like any remote client (bearer oc_client_* header, oc_url_token query).
// Spec: .opencode/plans/private-relay/01-protocol-spec.md (Layer 3).
/**
 * 隧道多路复用的 host 侧（Layer 3）：消费单条中继连接解密后的隧道帧，
 * 并分发到本地回环（loopback）源站。HTTP 流 -> fetch
 * `http://127.0.0.1:<port>`（流式 duplex body）；WS 流 -> 用 `ws` 客户端
 * 拨回环 WebSocket 端点。分发器绝不注入凭据：隧道请求与任何远程客户端
 * 一样自行鉴权（bearer `oc_client_*` 头、`oc_url_token` query）。路径
 * 白名单 + 头部剥离构成纵深防御；v1 用轮询 bufferedAmount 实现背压。
 */

import { WebSocket } from 'ws';

import {
  MAX_TUNNEL_PAYLOAD_BYTES,
  TunnelFrameType,
  chunkPayload,
  createFragmentAssembler,
  decodeJsonPayload,
  decodeTunnelFrame,
  encodeFragmentedMessage,
  encodeJsonPayload,
  encodeTunnelFrame,
} from './tunnel-codec.js';

// HTTP 路径白名单（纵深防御；与 realtime-proxy.js 放行的家族一致）。
// Path allowlists (defense in depth; same families realtime-proxy.js allows).
const isAllowedHttpPath = (pathname) =>
  pathname === '/health'
  || pathname === '/api'
  || pathname.startsWith('/api/')
  || pathname === '/auth'
  || pathname.startsWith('/auth/');

/** WebSocket 路径白名单：只允许事件、终端与听写这几类长连接端点。 */
const ALLOWED_WS_PATHS = new Set([
  '/api/global/event/ws',
  '/api/event/ws',
  '/api/terminal/ws',
  '/api/dictation/ws',
]);

// 逐跳头部：从隧道请求中剥离；`host` 由 fetch 指向回环源站时自行设置。
// content-length 也被丢弃，因为 body 经隧道重新分块、由 undici 自行计帧。
// Hop-by-hop headers stripped from tunneled requests; `host` is set by fetch
// to the loopback origin. content-length is dropped too because the body is
// re-chunked through the tunnel and undici computes framing itself.
const STRIPPED_REQUEST_HEADERS = new Set([
  'connection',
  'keep-alive',
  'transfer-encoding',
  'upgrade',
  'host',
  'content-length',
]);

// 响应侧的成帧头部：body 改以 HttpBody 块穿越隧道后即失效（回环
// fetch 已解码 content-encoding）。
// Response framing headers that no longer apply once the body crosses the
// tunnel as HttpBody chunks (loopback fetch already decoded content-encoding).
const STRIPPED_RESPONSE_HEADERS = new Set([
  'connection',
  'keep-alive',
  'transfer-encoding',
  'content-length',
  'content-encoding',
]);

// v1 背压规则：出站中继 socket 缓冲超过该值时暂停读取回环源。
// v1 backpressure rule: pause reading the loopback source while the outbound
// relay socket has more than this buffered.
const BACKPRESSURE_LIMIT_BYTES = 4 * 1024 * 1024;
// 背压轮询周期（bufferedAmount 无法订阅，只能轮询）。
const BACKPRESSURE_POLL_MS = 20;

// 小于该值的请求体先完整缓冲再发给回环服务，避免“途中丢帧的隧道体”
// （中继重连、HttpBody 帧丢失）以空/截断的 chunked body 形式抵达回环
// 服务——那会被裸 400 拒绝，表现为移动端的“Failed to send message
// (400)”。更大的请求体照旧实时流式转发。
// Bodies smaller than this are fully buffered before the loopback request is
// sent, so a tunneled body that lost frames (relay reconnect, dropped HttpBody
// frames) can never reach the loopback server as an empty/truncated chunked
// body — the server rejects those with a bare 400, surfacing as the mobile
// app's "Failed to send message (400)". Larger bodies stream live as before.
const BODY_BUFFER_MAX_BYTES = 512 * 1024;
// 请求体仍在缓冲阶段时设置投递时限：迟迟不完整就中断该流，把停滞的
// 隧道转换成“结果未知的传输失败”（客户端本来就会重试），而不是挂死的
// 回环请求。
// While the body is still being buffered, abort the stream if it never
// completes, so a stalled tunnel converts into an ambiguous transport failure
// (which the client already retries) instead of a hung loopback request.
const BODY_DELIVERY_TIMEOUT_MS = 15_000;

/** 简单的毫秒级 sleep，背压轮询用。 */
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** 形状校验：入站 HttpRequest 帧的 JSON payload 必须具备的字段。 */
const isHttpRequestPayload = (parsed) =>
  Boolean(parsed && typeof parsed === 'object'
    && typeof parsed.method === 'string'
    && typeof parsed.path === 'string'
    && typeof parsed.query === 'string'
    && parsed.headers && typeof parsed.headers === 'object');

/** 形状校验：入站 WsOpen 帧的 JSON payload（protocols 可选数组）。 */
const isWsOpenPayload = (parsed) =>
  Boolean(parsed && typeof parsed === 'object'
    && typeof parsed.path === 'string'
    && typeof parsed.query === 'string'
    && (parsed.protocols === undefined || Array.isArray(parsed.protocols)));

/** 形状校验：入站 WsClose 帧只要是对象即可（字段宽松处理）。 */
const isWsClosePayload = (parsed) => Boolean(parsed && typeof parsed === 'object');

/**
 * 创建一条中继连接的隧道分发器。sendFrame 用于回发加密前的明文帧，
 * getBufferedAmount 用于背压判定，getLocalPort 决定回环源站端口。
 * 返回 `{ handleFrame, close, streamCount }`；所有解码/处理错误都转换
 * 为对客户端的 StreamAbort 或合成响应，绝不让单条流拖垮整条连接。
 */
/**
 * @param {{
 *   connectionId: string,
 *   getLocalPort: () => number,
 *   sendFrame: (plaintextFrame: Uint8Array) => void | Promise<void>,
 *   getBufferedAmount: () => number,
 *   bodyDeliveryTimeoutMs?: number,
 * }} deps
 */
export const createTunnelHost = ({ connectionId, getLocalPort, sendFrame, getBufferedAmount, bodyDeliveryTimeoutMs = BODY_DELIVERY_TIMEOUT_MS }) => {
  // 活动流表：streamId -> HTTP（含 AbortController、body 槽、noBody 标记）
  // 或 WS（含 socket 与 opened 标记）条目。
  /** @type {Map<number, { kind: 'http', abort: AbortController, body: { enqueue(payload: Uint8Array): void, close(): void, error(error: Error): void } | null, noBody: boolean } | { kind: 'ws', socket: WebSocket, opened: boolean }>} */
  const streams = new Map();
  // WS 分片重组器（按 streamId+帧型聚合分片消息）。
  const assembler = createFragmentAssembler();
  // 整条连接关闭标记；置位后 send 变为 no-op，close 会中止所有流。
  let closed = false;

  // 出站发送包装：连接已关闭时丢弃帧，否则交给 sendFrame。
  const send = async (frame) => {
    if (closed) return;
    await sendFrame(frame);
  };

  // 发送一帧 JSON payload（编码 + 组帧一步完成）。
  const sendJson = (frameType, streamId, payload) =>
    send(encodeTunnelFrame(frameType, streamId, encodeJsonPayload(payload)));

  // 向客户端回报流中止及原因（字符串化，永不抛错）。
  const sendAbort = async (streamId, reason) => {
    await sendJson(TunnelFrameType.StreamAbort, streamId, { reason: String(reason ?? 'stream error') });
  };

  // 移除流表与重组器中该流的全部痕迹。
  const dropStream = (streamId) => {
    streams.delete(streamId);
    assembler.dropStream(streamId);
  };

  // 中止本地流并清理：HTTP 流 error 掉 body 槽并触发 AbortController；
  // WS 流 terminate 底层 socket。已失效的句柄静默吞掉。
  const abortLocalStream = (streamId, reason) => {
    const stream = streams.get(streamId);
    if (!stream) return;
    dropStream(streamId);
    if (stream.kind === 'http') {
      try {
        stream.body?.error(new Error(String(reason ?? 'aborted')));
      } catch {
        // body already closed
      }
      stream.abort.abort();
    } else {
      try {
        stream.socket.terminate();
      } catch {
        // socket already gone
      }
    }
  };

  // 背压等待：出站缓冲高于阈值时循环休眠，直到降下来、连接关闭或信号中止。
  const waitForBackpressure = async (signal) => {
    while (!closed && getBufferedAmount() > BACKPRESSURE_LIMIT_BYTES) {
      if (signal?.aborted) return;
      await sleep(BACKPRESSURE_POLL_MS);
    }
  };

  // -------------------------------------------------------------------------
  // HTTP
  // -------------------------------------------------------------------------

  // 构造发给回环源站的请求头：剥离逐跳/成帧头部、拒绝含 CR/LF 的键值、
  // 补上中继连接标记，并用回环 origin 覆盖客户端自报的 origin。
  const buildRequestHeaders = (rawHeaders, loopbackOrigin) => {
    const headers = {};
    for (const [name, value] of Object.entries(rawHeaders)) {
      if (typeof name !== 'string' || typeof value !== 'string') continue;
      const lower = name.toLowerCase();
      if (STRIPPED_REQUEST_HEADERS.has(lower)) continue;
      if (/[\r\n]/.test(name) || /[\r\n]/.test(value)) continue;
      headers[lower] = value;
    }
    headers['x-ompchamber-relay-connection'] = connectionId;
    // Browser-generated Origin is not visible to the tunnel client. Present the
    // loopback origin being dialed and overwrite any client-supplied value.
    headers.origin = loopbackOrigin;
    return headers;
  };

  // 合成响应绝不带空 body：`reason` 字段明确该响应产自中继 host 而非
  // 上游服务。
  // Synthetic responses never ship an empty body: `reason` states explicitly
  // that the relay host (not the upstream server) produced this response.
  const syntheticResponse = async (streamId, status, message) => {
    await sendJson(TunnelFrameType.HttpResponse, streamId, {
      status,
      headers: { 'content-type': 'application/json' },
    });
    await send(encodeTunnelFrame(TunnelFrameType.HttpBody, streamId, encodeJsonPayload({ error: message, reason: message, source: 'relay-tunnel-host' })));
    await send(encodeTunnelFrame(TunnelFrameType.StreamEnd, streamId, new Uint8Array(0)));
  };

  // 把一条（已就绪的）HTTP 流转发到回环 fetch：发出请求、回传
  // HttpResponse 头帧，再按背压节奏流式回传 HttpBody 块并以 StreamEnd
  // 收尾；任一环节失败即向客户端 StreamAbort（先确认流仍归本流所有，
  // 防止中止已切换归属的流）。
  const forwardRequest = async (streamId, stream, url, method, request, body, loopbackOrigin) => {
    let response;
    try {
      response = await fetch(url, {
        method,
        headers: buildRequestHeaders(request.headers, loopbackOrigin),
        body,
        duplex: body ? 'half' : undefined,
        signal: stream.abort.signal,
      });
    } catch (error) {
      if (streams.get(streamId) === stream) {
        dropStream(streamId);
        await sendAbort(streamId, error?.message ?? 'loopback request failed');
      }
      return;
    }

    const responseHeaders = {};
    for (const [name, value] of response.headers.entries()) {
      if (STRIPPED_RESPONSE_HEADERS.has(name)) continue;
      responseHeaders[name] = value;
    }
    await sendJson(TunnelFrameType.HttpResponse, streamId, { status: response.status, headers: responseHeaders });

    try {
      if (response.body) {
        for await (const chunk of response.body) {
          if (closed || stream.abort.signal.aborted) return;
          const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
          for (const piece of chunkPayload(bytes, MAX_TUNNEL_PAYLOAD_BYTES)) {
            await waitForBackpressure(stream.abort.signal);
            if (closed || stream.abort.signal.aborted) return;
            await send(encodeTunnelFrame(TunnelFrameType.HttpBody, streamId, piece));
          }
        }
      }
      if (streams.get(streamId) === stream) {
        dropStream(streamId);
        await send(encodeTunnelFrame(TunnelFrameType.StreamEnd, streamId, new Uint8Array(0)));
      }
    } catch (error) {
      if (streams.get(streamId) === stream) {
        dropStream(streamId);
        await sendAbort(streamId, error?.message ?? 'loopback response failed');
      }
    }
  };

  // HTTP 流主流程：路径白名单校验 -> 无 body 直接转发；有 body 则先
  // 缓冲（超限转实时流式），完整后一次性以 content-length 形式发出，
  // 避免丢帧的 chunked body 触发回环服务裸 400。
  const runHttpStream = async (streamId, request) => {
    const method = request.method.toUpperCase();
    if (!isAllowedHttpPath(request.path)) {
      dropStream(streamId);
      await syntheticResponse(streamId, 403, 'Path is not allowed through the relay');
      return;
    }

    const stream = streams.get(streamId);
    if (!stream || stream.kind !== 'http') return;

    const hasBody = method !== 'GET' && method !== 'HEAD';
    const loopbackOrigin = `http://127.0.0.1:${getLocalPort()}`;
    const url = `${loopbackOrigin}${request.path}${request.query ? `?${request.query}` : ''}`;

    if (!hasBody) {
      stream.noBody = true;
      await forwardRequest(streamId, stream, url, method, request, null, loopbackOrigin);
      return;
    }

    // Body-carrying request. Buffer the tunneled body frames and forward the
    // COMPLETE body only once StreamEnd arrives. Forwarding a body that lost
    // frames through the tunnel (relay reconnect, dropped HttpBody frames)
    // reaches the loopback server as an empty/truncated chunked body, which it
    // rejects with a bare 400 (empty response body) — the "Failed to send
    // message (400)" seen from the mobile APK. Bodies above BODY_BUFFER_MAX_BYTES
    // stream live so large uploads are not fully buffered.
    const buffered = [];
    let bufferedBytes = 0;
    let bodyFrameCount = 0;
    let liveStream = null;
    let liveController = null;
    let completed = false;
    let bodyFailure = null;
    let resolveBodyEnd;
    const bodyEnded = new Promise((resolve) => { resolveBodyEnd = resolve; });

    // 请求体的唯一收尾出口：记录失败原因、关闭/报错实时流控制器并
    // 唤醒等待方；重复调用幂等。
    const finishBody = (error) => {
      if (completed) return;
      completed = true;
      bodyFailure = error ?? null;
      if (liveController) {
        try {
          if (error) liveController.error(error);
          else liveController.close();
        } catch {
          // stream already errored/closed
        }
      }
      resolveBodyEnd();
    };

    let deliveryDeadline = null;
    // 缓冲体积超限后切换到实时流式：把已缓冲块灌入新建的 ReadableStream，
    // 由 forwardRequest 接管，取消投递时限。
    const switchToLive = () => {
      liveStream = new ReadableStream({
        start(controller) {
          liveController = controller;
          stream.body = controller;
        },
      });
      for (const chunk of buffered) {
        try { liveController.enqueue(chunk); } catch { break; }
      }
      buffered.length = 0;
      // The loopback request is now streaming live; runHttpStream has nothing
      // left to do — clear the deadline and let the async body forwarding own
      // this stream from here (abort/StreamEnd close the controller).
      if (deliveryDeadline) clearTimeout(deliveryDeadline);
      resolveBodyEnd();
      void forwardRequest(streamId, stream, url, method, request, liveStream, loopbackOrigin);
    };

    // 请求体汇聚槽：缓冲阶段累积块；切换实时后改由 liveController 接收。
    // close/error 对应客户端的 StreamEnd 与异常路径。
    stream.body = {
      enqueue(payload) {
        if (completed) return;
        bodyFrameCount += 1;
        if (liveController) {
          try { liveController.enqueue(payload); } catch {
            // stream already errored/closed
          }
          return;
        }
        buffered.push(payload);
        bufferedBytes += payload.length;
        if (bufferedBytes > BODY_BUFFER_MAX_BYTES) {
          switchToLive();
        }
      },
      close() {
        finishBody(null);
      },
      error(error) {
        finishBody(error);
      },
    };

    // 投递时限：body 迟迟不完整就中止流（视为传输失败，客户端可安全重试）。
    deliveryDeadline = setTimeout(() => {
      if (streams.get(streamId) === stream && !completed && !liveStream) {
        dropStream(streamId);
        void sendAbort(streamId, 'tunnel request body was not delivered in time');
        // Settle the buffered-body wait below so the stream's buffered chunks
        // and this call frame are released (the post-wait guard sees the
        // dropped stream and returns without a second abort).
        finishBody(new Error('tunnel request body was not delivered in time'));
      }
    }, bodyDeliveryTimeoutMs);
    deliveryDeadline.unref?.();

    await bodyEnded;
    if (deliveryDeadline) clearTimeout(deliveryDeadline);
    if (streams.get(streamId) !== stream) return; // aborted or dropped meanwhile
    if (bodyFailure) {
      dropStream(streamId);
      await sendAbort(streamId, bodyFailure.message ?? 'tunnel request body failed');
      return;
    }
    if (liveStream) return; // already forwarded via the streaming path

    // The client signaled it had a body but no HttpBody frame arrived before
    // StreamEnd — the body frames were lost through the tunnel. Forwarding an
    // empty body would make the loopback server reject the request with a bare
    // 400. Abort instead so the client treats it as an ambiguous transport
    // failure (dispatched, outcome unknown) and can safely retry.
    if (request.hasBody === true && bodyFrameCount === 0) {
      dropStream(streamId);
      await sendAbort(streamId, 'tunnel request body frames were lost');
      return;
    }

    // Buffered path: forward the complete body as a single buffer so Bun
    // frames it with content-length — never as a chunked body that could be
    // truncated. Reset the buffered handler so late frames cannot enqueue.
    stream.body = null;
    await forwardRequest(streamId, stream, url, method, request, Buffer.concat(buffered), loopbackOrigin);
  };

  // 处理 HttpRequest 帧：拒绝重复 streamId；解码校验 payload 后登记流并
  // 异步跑 runHttpStream。
  const handleHttpRequest = (streamId, payload) => {
    if (streams.has(streamId)) {
      abortLocalStream(streamId, 'duplicate stream id');
      void sendAbort(streamId, 'duplicate stream id');
      return;
    }
    let request;
    try {
      request = decodeJsonPayload(payload, isHttpRequestPayload);
    } catch (error) {
      void sendAbort(streamId, error?.message ?? 'malformed request');
      return;
    }
    const stream = { kind: 'http', abort: new AbortController(), body: null, noBody: false };
    streams.set(streamId, stream);
    void runHttpStream(streamId, request);
  };

  // 处理 HttpBody 帧：交给该流登记的 body 槽（缓冲 handler 或实时流
  // 控制器）；已收尾的流直接丢弃迟到字节。
  const handleHttpBody = (streamId, payload) => {
    const stream = streams.get(streamId);
    if (!stream || stream.kind !== 'http' || stream.noBody) return;
    // runHttpStream installs a body sink (buffering handler, or the live stream
    // controller once the buffer cap is crossed) before any HttpBody frame can
    // arrive; drop stray bytes for request bodies already completed/aborted.
    try {
      stream.body?.enqueue(payload);
    } catch {
      // stream already errored/closed
    }
  };

  // 处理 StreamEnd 帧：半关闭请求体（响应侧继续）；等待中的缓冲 body
  // 就此定稿发出。
  const handleStreamEnd = (streamId) => {
    const stream = streams.get(streamId);
    if (!stream || stream.kind !== 'http') return;
    try {
      stream.body?.close();
    } catch {
      // stream already errored/closed
    }
    // Response side keeps running; only the request body is half-closed.
  };

  // -------------------------------------------------------------------------
  // WebSocket
  // -------------------------------------------------------------------------

  // 处理 WsOpen 帧：路径白名单校验后用 `ws` 客户端拨回环端点，接管
  // open/message/close/error 事件并镜像回客户端；origin 固定为回环
  // origin（WKWebView 里客户端自报 origin 不可靠，`ws` 客户端默认不发
  // origin 会被 403 拒绝；真正鉴权靠隧道里的 oc_url_token）。
  const handleWsOpen = (streamId, payload) => {
    if (streams.has(streamId)) {
      abortLocalStream(streamId, 'duplicate stream id');
      void sendAbort(streamId, 'duplicate stream id');
      return;
    }
    let open;
    try {
      open = decodeJsonPayload(payload, isWsOpenPayload);
    } catch (error) {
      void sendAbort(streamId, error?.message ?? 'malformed ws open');
      return;
    }
    if (!ALLOWED_WS_PATHS.has(open.path)) {
      void sendAbort(streamId, 'Path is not allowed through the relay');
      return;
    }

    const url = `ws://127.0.0.1:${getLocalPort()}${open.path}${open.query ? `?${open.query}` : ''}`;
    // Present the loopback origin we're actually dialing. The server derives this
    // as a trusted same-origin candidate from the Host header (127.0.0.1:<port>),
    // so the WS origin check passes reliably for every client platform. We do NOT
    // use the client's window.location.origin: it's unreliable in WKWebView (empty
    // or "null" for custom schemes), and the `ws` client sends no Origin at all
    // otherwise — a no-origin upgrade is rejected 403. The request itself is still
    // authenticated by the tunneled oc_url_token, not by this origin.
    const dialHeaders = {
      'x-ompchamber-relay-connection': connectionId,
      origin: `http://127.0.0.1:${getLocalPort()}`,
    };
    let socket;
    try {
      socket = new WebSocket(url, open.protocols, {
        headers: dialHeaders,
      });
    } catch (error) {
      void sendAbort(streamId, error?.message ?? 'ws dial failed');
      return;
    }
    const stream = { kind: 'ws', socket, opened: false };
    streams.set(streamId, stream);

    socket.on('open', () => {
      if (streams.get(streamId) !== stream) return;
      stream.opened = true;
      void sendJson(TunnelFrameType.WsOpened, streamId, socket.protocol ? { protocol: socket.protocol } : {});
    });
    socket.on('message', (data, isBinary) => {
      if (streams.get(streamId) !== stream || closed) return;
      const bytes = Buffer.isBuffer(data) ? new Uint8Array(data) : new Uint8Array(Buffer.concat(data));
      const frameType = isBinary ? TunnelFrameType.WsBinary : TunnelFrameType.WsText;
      void (async () => {
        for (const frame of encodeFragmentedMessage(frameType, streamId, bytes)) {
          await waitForBackpressure(null);
          if (streams.get(streamId) !== stream || closed) return;
          await send(frame);
        }
      })();
    });
    socket.on('close', (code, reasonBuffer) => {
      if (streams.get(streamId) !== stream) return;
      dropStream(streamId);
      const reason = reasonBuffer ? reasonBuffer.toString('utf8') : '';
      if (stream.opened) {
        void sendJson(TunnelFrameType.WsClose, streamId, { code: code || 1000, reason });
      } else {
        void sendAbort(streamId, reason || `upstream ws closed (${code || 'no code'})`);
      }
    });
    socket.on('error', (error) => {
      if (streams.get(streamId) !== stream) return;
      if (!stream.opened) {
        dropStream(streamId);
        try {
          socket.terminate();
        } catch {
          // already gone
        }
        void sendAbort(streamId, error?.message ?? 'upstream ws error');
      }
      // Post-open errors are followed by 'close', handled above.
    });
  };

  // 处理隧道 WsText/WsBinary 消息：WS 流已打开时按帧型以文本或二进制
  // 透传给回环 socket。
  const handleWsMessage = (streamId, frameType, message) => {
    const stream = streams.get(streamId);
    if (!stream || stream.kind !== 'ws' || stream.socket.readyState !== WebSocket.OPEN) return;
    if (frameType === TunnelFrameType.WsText) {
      stream.socket.send(Buffer.from(message).toString('utf8'));
    } else {
      stream.socket.send(message, { binary: true });
    }
  };

  // 处理 WsClose 帧：按客户端给的 code/reason 关闭回环 socket（code
  // 越界回退 1000）；关闭失败则 terminate。
  const handleWsClose = (streamId, payload) => {
    const stream = streams.get(streamId);
    if (!stream || stream.kind !== 'ws') return;
    dropStream(streamId);
    let close = { code: 1000, reason: '' };
    try {
      close = decodeJsonPayload(payload, isWsClosePayload);
    } catch {
      // fall through with defaults
    }
    const code = Number.isInteger(close.code) && close.code >= 1000 && close.code <= 4999 ? close.code : 1000;
    try {
      stream.socket.close(code, typeof close.reason === 'string' ? close.reason : '');
    } catch {
      stream.socket.terminate();
    }
  };

  // -------------------------------------------------------------------------
  // Frame entrypoint
  // -------------------------------------------------------------------------

// 隧道帧统一入口：解码后按帧型分发。WS 消息帧先经分片重组；其余帧
// 整帧处理。Ping 回 Pong；host 不会收到的帧型（HttpResponse/WsOpened）
// 直接忽略而不是拆链。
  /** @param {Uint8Array} plaintextFrame one decrypted tunnel frame */
  const handleFrame = async (plaintextFrame) => {
    if (closed) return;
    const frame = decodeTunnelFrame(plaintextFrame);

    // WS message frames can be fragmented; everything else arrives whole.
    if (frame.frameType === TunnelFrameType.WsText || frame.frameType === TunnelFrameType.WsBinary) {
      const message = assembler.push(frame);
      if (message === null) return;
      handleWsMessage(frame.streamId, frame.frameType, message);
      return;
    }

    switch (frame.frameType) {
      case TunnelFrameType.HttpRequest:
        handleHttpRequest(frame.streamId, frame.payload);
        return;
      case TunnelFrameType.HttpBody:
        handleHttpBody(frame.streamId, frame.payload);
        return;
      case TunnelFrameType.StreamEnd:
        handleStreamEnd(frame.streamId);
        return;
      case TunnelFrameType.StreamAbort:
        abortLocalStream(frame.streamId, 'aborted by client');
        return;
      case TunnelFrameType.WsOpen:
        handleWsOpen(frame.streamId, frame.payload);
        return;
      case TunnelFrameType.WsClose:
        handleWsClose(frame.streamId, frame.payload);
        return;
      case TunnelFrameType.Ping:
        await send(encodeTunnelFrame(TunnelFrameType.Pong, frame.streamId, new Uint8Array(0)));
        return;
      case TunnelFrameType.Pong:
        return;
      default:
        // Host never receives HttpResponse/WsOpened; ignore rather than tear down.
        return;
    }
  };

  // 关闭整条连接：中止并清空所有活动流；幂等。
  const close = () => {
    if (closed) return;
    closed = true;
    for (const streamId of [...streams.keys()]) {
      abortLocalStream(streamId, 'connection closed');
    }
    streams.clear();
  };

  // 对外句柄：handleFrame（喂入解密帧）、close 与 streamCount 只读标记。
  return {
    handleFrame,
    close,
    get streamCount() {
      return streams.size;
    },
  };
};
