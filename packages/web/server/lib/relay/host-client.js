// Long-lived relay host client: maintains the signed `host-control` socket to
// the relay, and per connected client a signed `host-data` socket that runs the
// responder E2EE handshake and feeds decrypted frames into a tunnel-host
// dispatcher. Spec: .opencode/plans/private-relay/01-protocol-spec.md (Layer 1).
/**
 * 长驻中继 host 客户端：维护到中继的签名 `host-control` socket（注册、
 * 收 connected/disconnected/sync 指令、ping/pong 保活），并为每个接入的
 * 客户端建立签名 `host-data` socket——先跑响应方 E2EE 握手，再把解密后
 * 的帧喂给 tunnel-host 分发器。控制 socket 断开后按指数退避重连并重新
 * 注册；数据 socket 由中继指令与空闲收割器共同管理。
 */

import { WebSocket } from 'ws';

import { RELAY_PROTOCOL_VERSION, RelayCloseCode, createHostHandshake } from './e2ee.js';
import { createOutboundFrameBatcher, decodeFrameBatch } from './tunnel-codec.js';
import { createTunnelHost } from './tunnel-host.js';

/** 重连退避基数：1s 起，按连续失败次数指数增长。 */
const BACKOFF_BASE_MS = 1000;
/** 重连退避上限：30s。 */
const BACKOFF_CAP_MS = 30000;
/** host-data socket 建立超时：超过即拆链重建。 */
const DATA_SOCKET_OPEN_TIMEOUT_MS = 15000;
// 客户端空闲时也至少每 ~30s 发一个隧道 Ping，因此一个连续 3 个 ping
// 周期无入站流量的数据 socket 属于“没发 close 就死掉的客户端”（断网、
// 杀后台）。中继 worker 可能很久都察觉不到这条死腿，host 必须自己收割
// ——既释放资源，也让“N 台设备已连接”的状态真实、不数幽灵连接。
// Clients send a tunnel Ping at least every ~30s when idle, so a data socket
// with no inbound traffic for 3 ping intervals belongs to a client that died
// without a WebSocket close (network loss, battery kill). The relay worker may
// not notice the dead client leg for a long time, so the host must reap these
// itself — both to free resources and to keep the "N devices connected" status
// honest instead of counting ghosts.
const DATA_SOCKET_IDLE_TIMEOUT_MS = 90_000;
/** 空闲收割器的扫描周期（与空闲超时配套）。 */
const DATA_SOCKET_IDLE_SWEEP_INTERVAL_MS = 30_000;
// 控制 socket 的协议级保活。没有它，一条静默死掉的网络路径（NAT 超时、
// 中继边缘节点不发 close 就驱逐）会让 host 以为自己仍注册在中继，而
// 中继早已把它忘掉——所有客户端隧道将永远卡在 connecting。错过 pong
// 窗口即终止 socket，走正常的重连 + 重新注册流程。
// Protocol-level keepalive for the control socket. Without it, a network path
// that dies silently (NAT timeout, relay-edge eviction without close frames)
// leaves the host believing it is registered while the relay has forgotten it —
// every client tunnel then hangs in `connecting` forever. A missed pong window
// terminates the socket, which drives the normal reconnect + re-registration.
const CONTROL_PING_INTERVAL_MS = 30_000;
/** pong 宽限期：超过 interval+grace 无任何回包即判定路径死亡。 */
const CONTROL_PONG_GRACE_MS = 10_000;
/** 出站批量窗口默认值（毫秒），与 tunnel-codec 的默认一致。 */
const DEFAULT_BATCH_WINDOW_MS = 150;

// 解析帧批量冲刷窗口：显式入参优先，其次环境变量，最后 150ms 默认值。
// 仅在协商出批量能力的方向生效。
// Resolve the frame-batching flush window: explicit option wins, then env, then
// the 150 ms default. Only applies on directions where batching was negotiated.
const resolveBatchWindowMs = (option) => {
  if (Number.isFinite(option) && option >= 0) return option;
  const envValue = Number.parseInt(process.env.OMPCHAMBER_RELAY_BATCH_WINDOW_MS ?? '', 10);
  if (Number.isFinite(envValue) && envValue >= 0) return envValue;
  return DEFAULT_BATCH_WINDOW_MS;
};

/**
 * 启动中继 host 客户端：立即发起首个控制连接，随后全自治——重连、
 * 保活、按中继指令开合数据 socket。返回 `{ stop, getStatus }` 句柄；
 * onStatus 在每次状态或连接数变化时回调（回调抛错会被吞掉，不影响传输）。
 */
/**
 * @param {{
 *   relayUrl: string,
 *   identity: { serverId: string, hostEncPrivateKey: CryptoKey, signRelayAuth: (role: string, connectionId?: string | null) => { ts: number, sig: string, pk: string } },
 *   localPort?: number,
 *   getLocalPort?: () => number,
 *   onStatus?: (status: { state: string, lastError: string | null, connectedClients: number }) => void,
 *   logger?: Pick<Console, 'warn'>,
 * }} options
 */
export const startRelayHost = ({ relayUrl, identity, localPort, getLocalPort, onStatus, logger = console, batchWindowMs, batch }) => {
  // 本地端口取值函数：优先 getLocalPort，退化为常量 localPort。
  const resolveLocalPort = typeof getLocalPort === 'function' ? getLocalPort : () => localPort;
  // 本端是否愿意使用批量封装（协商仍需对端同意）。
  const localBatch = batch !== false;
  // 解析后的批量冲刷窗口（毫秒）。
  const resolvedBatchWindowMs = resolveBatchWindowMs(batchWindowMs);

  // 生命周期与状态：stopped 标记、对外状态、最近错误、当前控制 socket、
  // 重连定时器与连续失败计数。
  let stopped = false;
  let state = 'connecting';
  let lastError = null;
  let controlSocket = null;
  let reconnectTimer = null;
  let consecutiveFailures = 0;
  // 数据 socket 表：connectionId -> { socket, tunnel, openTimer, batcher, lastActivityAt }。
  /** @type {Map<string, { socket: WebSocket, tunnel: ReturnType<typeof createTunnelHost> | null, openTimer: NodeJS.Timeout | null }>} */
  const dataSockets = new Map();

  // 向 onStatus 消费者推送当前状态；消费方抛错不影响传输层。
  const emitStatus = () => {
    try {
      onStatus?.({ state, lastError, connectedClients: dataSockets.size });
    } catch {
      // status consumers must not break the transport
    }
  };

  // 更新状态（可选地同时记录错误）并广播。
  const setState = (nextState, error) => {
    state = nextState;
    if (error !== undefined) lastError = error;
    emitStatus();
  };

  // 构造带签名鉴权参数的 socket URL：协议版本、角色、serverId、
  // connectionId（数据 socket 才有）+ signRelayAuth 产出的 ts/sig/pk。
  const buildSocketUrl = (role, connectionId) => {
    const url = new URL(relayUrl);
    url.searchParams.set('v', String(RELAY_PROTOCOL_VERSION));
    url.searchParams.set('role', role);
    url.searchParams.set('serverId', identity.serverId);
    if (connectionId) url.searchParams.set('connectionId', connectionId);
    const auth = identity.signRelayAuth(role, connectionId ?? null);
    url.searchParams.set('ts', String(auth.ts));
    url.searchParams.set('sig', auth.sig);
    url.searchParams.set('pk', auth.pk);
    return url.toString();
  };

  // 拆除一个数据 socket：清定时器、销毁批量器、关闭隧道、按需 close 或
  // terminate 底层 socket，最后广播连接数变化。
  const teardownDataSocket = (connectionId, closeCode, reason) => {
    const entry = dataSockets.get(connectionId);
    if (!entry) return;
    dataSockets.delete(connectionId);
    if (entry.openTimer) clearTimeout(entry.openTimer);
    entry.batcher?.dispose();
    entry.tunnel?.close();
    try {
      if (entry.socket.readyState === WebSocket.OPEN || entry.socket.readyState === WebSocket.CONNECTING) {
        if (closeCode) entry.socket.close(closeCode, reason ?? '');
        else entry.socket.terminate();
      }
    } catch {
      // socket already gone
    }
    emitStatus();
  };

  // 为 connectionId 建立并驱动一个数据 socket：拨号 -> 握手 -> E2EE 信道
  // -> 隧道分发。已存在或已停止时是 no-op。
  const openDataSocket = (connectionId) => {
    if (stopped || dataSockets.has(connectionId)) return;

    let socket;
    try {
      socket = new WebSocket(buildSocketUrl('host-data', connectionId));
    } catch (error) {
      logger.warn(`[Relay] host-data dial failed: ${error?.message ?? error}`);
      return;
    }

  // 数据 socket 记录：socket、隧道实例、建连超时定时器、批量器与最近活跃时刻。
    const entry = { socket, tunnel: null, openTimer: null, batcher: null, lastActivityAt: Date.now() };
    dataSockets.set(connectionId, entry);
    entry.openTimer = setTimeout(() => {
      logger.warn('[Relay] host-data socket open timeout');
      teardownDataSocket(connectionId);
    }, DATA_SOCKET_OPEN_TIMEOUT_MS);

  // 响应方握手状态机；batch 为本端协商意愿。
    const handshake = createHostHandshake(identity.hostEncPrivateKey, { batch: localBatch });
    let channel = null;
    let batchNegotiated = false;
  // 串行化异步消息处理，保证加密帧顺序（与解密器严格递增的计数器）不乱。
    // Serialize async message handling so encrypted frame order (and the
    // strictly-increasing decrypt counter) is preserved.
    let processing = Promise.resolve();
  // 串行化“加密 + 发送”：一个方向的 IV 计数器必须按加密顺序上线。
  // 一次 encrypt() == 一条 WS 消息 == 一次计数器自增，无论其中携带
  // 批量还是单帧。
    // Serialize encrypt+send so the per-direction IV counter reaches the wire in
    // encryption order. One encrypt() == one WS message == one counter tick,
    // whether it carries a batch or a lone frame.
    let sendChain = Promise.resolve();
  // 经 sendChain 串行加密并发送一条明文（socket 已换/未开/无信道时静默丢弃）。
    const sendEncryptedPlaintext = (plaintext) => {
      sendChain = sendChain
        .then(async () => {
          if (dataSockets.get(connectionId) !== entry || socket.readyState !== WebSocket.OPEN || !channel) return;
          const encrypted = await channel.encryptor.encrypt(plaintext);
          socket.send(encrypted, { binary: true });
        })
        .catch((error) => {
          logger.warn(`[Relay] host-data send failed: ${error?.message ?? error}`);
        });
    };

  // 信道失败出口：只记录 connectionId 与原因（绝不记录 payload 内容），
  // 然后拆除数据 socket。
    const failChannel = (closeCode, reason) => {
      // connectionId + reason only — never payload contents.
      logger.warn(`[Relay] data channel failed connectionId=${connectionId} reason=${reason ?? 'unknown'}`);
      teardownDataSocket(connectionId, closeCode, reason);
    };

  // 数据 socket 的统一消息入口：文本帧驱动握手状态机；二进制帧先解密
  // （批量明文先拆批），再按序喂给隧道分发器。任何入站消息都会刷新
  // lastActivityAt，供空闲收割器判定客户端存活。
    const handleMessage = async (data, isBinary) => {
      const current = dataSockets.get(connectionId);
      if (current !== entry) return;
      // Any inbound message (including the client's keepalive Ping) proves the
      // client is alive; the idle sweeper reaps sockets this stops updating.
      entry.lastActivityAt = Date.now();

      if (!isBinary) {
        const action = await handshake.handleText(data.toString('utf8'));
        if (action.type === 'send-text') {
          socket.send(action.text);
        } else if (action.type === 'established') {
          channel = action.channel;
          batchNegotiated = action.batch === true;
          entry.batcher = batchNegotiated
            ? createOutboundFrameBatcher({ windowMs: resolvedBatchWindowMs, sendBatch: sendEncryptedPlaintext })
            : null;
          entry.tunnel = createTunnelHost({
            connectionId,
            getLocalPort: resolveLocalPort,
            getBufferedAmount: () => socket.bufferedAmount,
            sendFrame: (plaintextFrame) => {
              if (dataSockets.get(connectionId) !== entry || socket.readyState !== WebSocket.OPEN) return;
              if (entry.batcher) entry.batcher.enqueue(plaintextFrame);
              else sendEncryptedPlaintext(plaintextFrame);
            },
          });
          if (action.replyText) socket.send(action.replyText);
        } else if (action.type === 'fail') {
          failChannel(action.closeCode, action.reason);
        }
        return;
      }

      if (!channel || !entry.tunnel) {
        // Encrypted traffic before the handshake completed: fail closed.
        failChannel(RelayCloseCode.ChannelFailure, 'binary frame before handshake');
        return;
      }
      let plaintext;
      try {
        plaintext = await channel.decryptor.decrypt(new Uint8Array(data));
      } catch {
        failChannel(RelayCloseCode.ChannelFailure, 'frame decryption failed');
        return;
      }
      try {
        if (batchNegotiated) {
          // One encrypted message may carry several tunnel frames; dispatch each
          // in order through the same per-frame handling as legacy.
          for (const frame of decodeFrameBatch(plaintext)) {
            if (dataSockets.get(connectionId) !== entry) return;
            await entry.tunnel.handleFrame(frame);
          }
        } else {
          await entry.tunnel.handleFrame(plaintext);
        }
      } catch (error) {
        logger.warn(`[Relay] tunnel frame handling failed: ${error?.message ?? error}`);
      }
    };

    socket.on('open', () => {
      if (entry.openTimer) {
        clearTimeout(entry.openTimer);
        entry.openTimer = null;
      }
      emitStatus();
    });
    socket.on('message', (data, isBinary) => {
      processing = processing
        .then(() => handleMessage(data, isBinary))
        .catch((error) => {
          logger.warn(`[Relay] data socket message failed: ${error?.message ?? error}`);
          failChannel(RelayCloseCode.ChannelFailure, 'internal error');
        });
    });
    socket.on('close', () => {
      teardownDataSocket(connectionId);
    });
    socket.on('error', (error) => {
      logger.warn(`[Relay] host-data socket error: ${error?.message ?? error}`);
    });
  };

  // 控制 socket 的消息处理：sync 全量对齐数据 socket 集合（多退少补），
  // connected/disconnected 增删单个连接。
  const handleControlMessage = (raw) => {
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    if (!message || typeof message !== 'object') return;
    if (message.type === 'sync' && Array.isArray(message.connectionIds)) {
      const wanted = new Set(message.connectionIds.filter((id) => typeof id === 'string' && id.length > 0));
      for (const connectionId of [...dataSockets.keys()]) {
        if (!wanted.has(connectionId)) teardownDataSocket(connectionId);
      }
      for (const connectionId of wanted) {
        openDataSocket(connectionId);
      }
      return;
    }
    if (message.type === 'connected' && typeof message.connectionId === 'string') {
      openDataSocket(message.connectionId);
      return;
    }
    if (message.type === 'disconnected' && typeof message.connectionId === 'string') {
      teardownDataSocket(message.connectionId);
    }
  };

  // 指数退避调度重连：连续失败越多等待越久（封顶 30s），期间状态置
  // reconnecting；已有排程则不重复排。
  const scheduleReconnect = () => {
    if (stopped || reconnectTimer) return;
    const delay = Math.min(BACKOFF_BASE_MS * 2 ** consecutiveFailures, BACKOFF_CAP_MS);
    consecutiveFailures += 1;
    setState('reconnecting');
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connectControl();
    }, delay);
  };

  // 建立（或重建）控制 socket：拨号、注册鉴权参数在 URL 中；成功后
  // 复位失败计数。ping/pong 保活与消息解析都在本函数内接线。
  const connectControl = () => {
    if (stopped) return;
    setState(consecutiveFailures === 0 ? 'connecting' : 'reconnecting');

    let socket;
    try {
      socket = new WebSocket(buildSocketUrl('host-control'));
    } catch (error) {
      lastError = error?.message ?? String(error);
      scheduleReconnect();
      return;
    }
    controlSocket = socket;

  // 活性探测：按周期 ping，任何 pong（或消息）都算路径存活。静默超过
  // 周期+宽限即认为连接已死——terminate，让 close 处理器走重连与
  // 重新注册。
    // Liveness: ping on an interval; any pong (or message) proves the path.
    // A quiet window beyond interval+grace means the connection silently died —
    // terminate so the close handler reconnects and re-registers at the relay.
    let lastAliveAt = Date.now();
    const pingTimer = setInterval(() => {
      if (controlSocket !== socket || socket.readyState !== WebSocket.OPEN) return;
      if (Date.now() - lastAliveAt > CONTROL_PING_INTERVAL_MS + CONTROL_PONG_GRACE_MS) {
        logger.warn('[Relay] control socket unresponsive (missed pong) — reconnecting');
        try {
          socket.terminate();
        } catch {
          // terminate is best-effort; the close handler still runs.
        }
        return;
      }
      try {
        socket.ping();
      } catch {
        // Send failure surfaces via the error/close handlers.
      }
    }, CONTROL_PING_INTERVAL_MS);
    if (typeof pingTimer.unref === 'function') pingTimer.unref();

    socket.on('open', () => {
      if (controlSocket !== socket) return;
      consecutiveFailures = 0;
      lastAliveAt = Date.now();
      setState('connected', null);
    });
    socket.on('pong', () => {
      lastAliveAt = Date.now();
    });
    socket.on('message', (data, isBinary) => {
      if (controlSocket !== socket || isBinary) return;
      lastAliveAt = Date.now();
      handleControlMessage(data.toString('utf8'));
    });
    socket.on('error', (error) => {
      if (controlSocket !== socket) return;
      lastError = error?.message ?? String(error);
    });
    socket.on('close', (code, reasonBuffer) => {
      clearInterval(pingTimer);
      if (controlSocket !== socket) return;
      controlSocket = null;
      const reason = reasonBuffer ? reasonBuffer.toString('utf8') : '';
      if (!lastError && code && code !== 1000) {
        lastError = `control socket closed (${code}${reason ? `: ${reason}` : ''})`;
      }
      // Data sockets ride their own relay connections; the relay keeps clients
      // alive through a 30 s control-reconnect grace window, so leave them up.
      scheduleReconnect();
    });
  };

// 空闲收割器：周期扫描数据 socket，踢掉客户端已静默（无帧、无保活
// ping）的连接——即中继 worker 尚未察觉的死客户端腿。
  // Reap data sockets whose client went silent (no frames, no keepalive pings)
  // — a dead phone leg the relay worker hasn't noticed yet.
  const idleSweepTimer = setInterval(() => {
    const now = Date.now();
    for (const [connectionId, entry] of [...dataSockets.entries()]) {
      if (now - entry.lastActivityAt <= DATA_SOCKET_IDLE_TIMEOUT_MS) continue;
      logger.info(`[Relay] reaping idle data socket connectionId=${connectionId}`);
      teardownDataSocket(connectionId, 1001, 'client idle timeout');
    }
  }, DATA_SOCKET_IDLE_SWEEP_INTERVAL_MS);
  if (typeof idleSweepTimer.unref === 'function') idleSweepTimer.unref();

  // 停止整个 host 客户端：停收割器与重连排程、拆除全部数据 socket、
  // 关闭控制 socket，状态置 disabled。幂等。
  const stop = () => {
    if (stopped) return;
    stopped = true;
    clearInterval(idleSweepTimer);
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    for (const connectionId of [...dataSockets.keys()]) {
      teardownDataSocket(connectionId, 1001, 'host stopping');
    }
    const socket = controlSocket;
    controlSocket = null;
    if (socket) {
      try {
        socket.close(1001, 'host stopping');
      } catch {
        socket.terminate();
      }
    }
    setState('disabled');
  };

  connectControl();

  // 对外句柄：stop 与 getStatus（快照读取，无副作用）。
  return {
    stop,
    getStatus: () => ({ state, lastError, connectedClients: dataSockets.size }),
  };
};
